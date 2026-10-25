//! Incremental reindex helpers used by the MCP watch thread.
//!
//! Extracted from `server` so language filtering, changeset coalescing, leader
//! persist, and throttle-window merge can be unit-tested without spinning a
//! full MCP server.

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use astrolabe_core::elect::IndexerLock;
use astrolabe_core::index::{IncrementalIndex, IndexOptions, DEFAULT_PARSE_CACHE_BYTES};
use astrolabe_core::{ChangeSet, Language, RelPath};

/// Minimum gap between two published rebuilds. Bursts of editor saves collapse
/// into one `apply_changeset` instead of a chain of full (or even incremental)
/// rebuilds. `Duration::ZERO` disables the wait but still drains+merges
/// whatever is already queued.
pub(crate) const DEFAULT_REINDEX_THROTTLE: Duration = Duration::from_secs(2);

const BYTES_PER_MB: u64 = 1024 * 1024;

/// `ASTROLABE_PARSE_CACHE_MB` (whole megabytes). Invalid values fall back to
/// the core default so a typo cannot silently disable the LRU.
pub(crate) fn parse_parse_cache_bytes(raw: Option<&str>) -> u64 {
    match raw {
        None => DEFAULT_PARSE_CACHE_BYTES,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(mb) => mb.saturating_mul(BYTES_PER_MB),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_PARSE_CACHE_MB; using default"
                );
                DEFAULT_PARSE_CACHE_BYTES
            }
        },
    }
}

pub(crate) fn parse_cache_bytes_from_env() -> u64 {
    parse_parse_cache_bytes(std::env::var("ASTROLABE_PARSE_CACHE_MB").ok().as_deref())
}

/// `ASTROLABE_REINDEX_MS`: unset → 2s; `0` → no wait (still drain+merge);
/// invalid → default 2s.
pub(crate) fn parse_reindex_throttle(raw: Option<&str>) -> Duration {
    match raw {
        None => DEFAULT_REINDEX_THROTTLE,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => Duration::ZERO,
            Ok(ms) => Duration::from_millis(ms),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_REINDEX_MS; using default 2000ms"
                );
                DEFAULT_REINDEX_THROTTLE
            }
        },
    }
}

pub(crate) fn reindex_throttle_from_env() -> Duration {
    parse_reindex_throttle(std::env::var("ASTROLABE_REINDEX_MS").ok().as_deref())
}

pub(crate) fn mcp_index_options(
    persist: bool,
    parse_cache_bytes: u64,
    respect_gitignore: bool,
    extra_excludes: &[String],
) -> IndexOptions {
    IndexOptions {
        call_edges: true,
        persist,
        // A follower never writes the store, but reading the leader's cache
        // saves it a cold reparse of every file.
        persist_read_only: !persist,
        parse_cache_bytes,
        scan: astrolabe_core::scan::ScanOptions {
            respect_gitignore,
            extra_excludes: extra_excludes.to_vec(),
            ..astrolabe_core::scan::ScanOptions::default()
        },
        ..IndexOptions::default()
    }
}

/// Max size of a text-only file admitted by the incremental path; mirrors the
/// scanner's default `max_file_bytes` so a cold build and a live edit agree.
const TEXT_ONLY_MAX_BYTES: u64 = 1_500_000;

/// A non-language file (md, yaml, sql, `LICENSE` …) that the scanner would
/// admit: it exists, is within the size cap, and is not binary (no NUL in the
/// first 8 KiB). Such a change must still publish a new index so that
/// `text_only_paths` and the index generation (dedup / loop-guard stamp) see
/// it; a missing or oversized file is dropped as before.
fn text_only_admissible(root: &Path, path: &RelPath) -> bool {
    use std::io::Read;
    let full = root.join(path.as_str());
    let Ok(meta) = std::fs::metadata(&full) else {
        return false;
    };
    if !meta.is_file() || meta.len() > TEXT_ONLY_MAX_BYTES {
        return false;
    }
    let Ok(file) = std::fs::File::open(&full) else {
        return false;
    };
    let mut head = Vec::with_capacity(8192);
    if file.take(8192).read_to_end(&mut head).is_err() {
        return false;
    }
    !head.contains(&0)
}

/// Keep `added`/`modified` when [`Language::from_path`] recognizes the
/// file, or it has no real extension ([`Language::is_shebang_candidate`]) and is (or just stopped being) a shebang
/// script, or when it is an admissible text-only file
/// ([`text_only_admissible`]); the latter produce no symbols but republish the
/// index so text search and dedup stamps stay fresh. The `#!` probe is cached
/// by mtime/size so it costs a stat. A script that
/// lost its shebang is turned into a removal so its symbols are dropped.
/// Every `removed` path is kept (dropping a node is cheap and must not miss a
/// delete).
pub(crate) fn filter_changeset(root: &Path, changes: ChangeSet) -> ChangeSet {
    let mut removed = changes.removed;
    let mut keep = |path: &RelPath| -> bool {
        if Language::from_path(path).is_some() {
            return true;
        }
        if !Language::is_shebang_candidate(path) {
            return text_only_admissible(root, path);
        }
        let state = astrolabe_core::index::shebang_state(&root.join(path.as_str()));
        if state.script || state.missing {
            return true;
        }
        if state.was_script {
            removed.push(path.clone());
        }
        text_only_admissible(root, path)
    };
    let added: Vec<RelPath> = changes.added.into_iter().filter(|p| keep(p)).collect();
    let modified: Vec<RelPath> = changes.modified.into_iter().filter(|p| keep(p)).collect();
    ChangeSet {
        added,
        modified,
        removed,
    }
}

/// If this process does not already hold the indexer lock, try once.
///
/// `true` → leader, `IndexOptions.persist` may be on. `false` → follower:
/// in-memory incremental only, never open the redb store.
///
/// Self-heal: when the previous leader exits, its `IndexerLock` is dropped
/// (or the OS releases the flock on process death). The next cycle's
/// `try_acquire` promotes a follower. An I/O error is logged and treated as
/// follower so the server keeps answering tools.
pub(crate) fn refresh_leader(slot: &Mutex<Option<IndexerLock>>, root: &Path) -> bool {
    let mut guard = crate::session::LockOrRecover::lock_or_recover(slot);
    if guard.as_ref().is_some_and(IndexerLock::held) {
        return true;
    }
    match IndexerLock::try_acquire(root) {
        Ok(Some(lock)) => {
            *guard = Some(lock);
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "indexer lock acquire failed; running as follower (persist=false)"
            );
            false
        }
    }
}

/// Apply `changes` onto `snapshot`, or cold-build when there is no Ready
/// incremental snapshot (e.g. the first index panicked and a later changeset
/// is the self-heal). Panics become `Err` so the caller can keep the old graph.
pub(crate) fn apply_reindex(
    root: &Path,
    snapshot: Option<&IncrementalIndex>,
    changes: &ChangeSet,
    opts: &IndexOptions,
) -> Result<IncrementalIndex, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match snapshot {
        Some(previous) => previous.apply_changeset(root, changes, opts),
        None => IncrementalIndex::build(root, opts),
    }))
    .map_err(|_| "底层索引能力尚未实现或发生 panic".to_string())
}

/// Merge `first` with everything that arrives before `throttle` elapses.
/// Already-queued messages are drained immediately; later ones are waited for
/// with `recv_timeout`. A disconnected sender returns whatever was merged.
pub(crate) fn coalesce_window(
    rx: &Receiver<ChangeSet>,
    mut acc: ChangeSet,
    throttle: Duration,
) -> ChangeSet {
    while let Ok(next) = rx.try_recv() {
        acc.merge(next);
    }
    if throttle.is_zero() {
        return acc;
    }
    let deadline = Instant::now() + throttle;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(next) => {
                acc.merge(next);
                while let Ok(more) = rx.try_recv() {
                    acc.merge(more);
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    acc
}

/// Watch-thread body: recv → throttle-merge → language-filter → callback.
///
/// `on_batch` runs only for a non-empty filtered changeset. The callback is
/// the rebuild; changes that arrive while it runs stay in `rx` and merge on
/// the next iteration.
pub(crate) fn for_each_reindex_batch(
    root: &Path,
    rx: Receiver<ChangeSet>,
    throttle: Duration,
    mut on_batch: impl FnMut(&ChangeSet),
) {
    let mut last_apply: Option<Instant> = None;
    while let Ok(first) = rx.recv() {
        let wait = last_apply
            .map(|at| throttle.saturating_sub(at.elapsed()))
            .unwrap_or(Duration::ZERO);
        let merged = coalesce_window(&rx, first, wait);
        let filtered = filter_changeset(root, merged);
        if filtered.is_empty() {
            continue;
        }
        on_batch(&filtered);
        last_apply = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::index::store_path;
    use astrolabe_core::RelPath;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    fn unique_temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-mcp-{}-{}-{}-{}",
            "reindex",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn cs_added(paths: &[&str]) -> ChangeSet {
        ChangeSet {
            added: paths.iter().map(|p| RelPath::new(*p)).collect(),
            ..ChangeSet::default()
        }
    }

    fn cs_modified(paths: &[&str]) -> ChangeSet {
        ChangeSet {
            modified: paths.iter().map(|p| RelPath::new(*p)).collect(),
            ..ChangeSet::default()
        }
    }

    fn cs_removed(paths: &[&str]) -> ChangeSet {
        ChangeSet {
            removed: paths.iter().map(|p| RelPath::new(*p)).collect(),
            ..ChangeSet::default()
        }
    }

    #[test]
    fn parse_cache_and_throttle_env_parsing() {
        assert_eq!(parse_parse_cache_bytes(None), DEFAULT_PARSE_CACHE_BYTES);
        assert_eq!(parse_parse_cache_bytes(Some("8")), 8 * BYTES_PER_MB);
        assert_eq!(
            parse_parse_cache_bytes(Some("nope")),
            DEFAULT_PARSE_CACHE_BYTES
        );
        assert_eq!(parse_reindex_throttle(None), DEFAULT_REINDEX_THROTTLE);
        assert_eq!(parse_reindex_throttle(Some("0")), Duration::ZERO);
        assert_eq!(
            parse_reindex_throttle(Some("250")),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_reindex_throttle(Some("bogus")),
            DEFAULT_REINDEX_THROTTLE
        );
    }

    #[test]
    fn filter_changeset_drops_non_language_keeps_removed() {
        let mixed = ChangeSet {
            added: vec![RelPath::new("README.md"), RelPath::new("src/lib.rs")],
            modified: vec![RelPath::new("notes.log"), RelPath::new("app.py")],
            removed: vec![RelPath::new("gone.sql"), RelPath::new("old.rs")],
        };
        let filtered = filter_changeset(Path::new("/nonexistent-astrolabe-root"), mixed);
        assert_eq!(filtered.added, vec![RelPath::new("src/lib.rs")]);
        assert_eq!(filtered.modified, vec![RelPath::new("app.py")]);
        assert_eq!(
            filtered.removed,
            vec![RelPath::new("gone.sql"), RelPath::new("old.rs")]
        );
        assert!(filter_changeset(
            Path::new("/nonexistent-astrolabe-root"),
            cs_modified(&["docs/guide.md"])
        )
        .is_empty());
        assert!(!filter_changeset(
            Path::new("/nonexistent-astrolabe-root"),
            cs_removed(&["docs/guide.md"])
        )
        .is_empty());
        assert!(!filter_changeset(
            Path::new("/nonexistent-astrolabe-root"),
            cs_modified(&["bin/tool"])
        )
        .is_empty());
    }

    #[test]
    fn language_add_modify_remove_updates_graph() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let opts = mcp_index_options(false, DEFAULT_PARSE_CACHE_BYTES, true, &[]);
        let inc = IncrementalIndex::build(&dir, &opts);
        assert!(
            inc.graph.symbols.iter().any(|s| s.name == "alpha"),
            "cold build must see alpha"
        );
        assert!(!inc.graph.files.iter().any(|f| f.path.as_str() == "b.py"));

        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let inc = apply_reindex(&dir, Some(&inc), &cs_added(&["b.py"]), &opts).expect("add");
        assert!(inc.graph.files.iter().any(|f| f.path.as_str() == "b.py"));
        assert!(inc.graph.symbols.iter().any(|s| s.name == "beta"));

        std::fs::write(dir.join("a.py"), "def gamma():\n    return 3\n").unwrap();
        let inc = apply_reindex(&dir, Some(&inc), &cs_modified(&["a.py"]), &opts).expect("mod");
        assert!(inc.graph.symbols.iter().any(|s| s.name == "gamma"));
        assert!(!inc.graph.symbols.iter().any(|s| s.name == "alpha"));

        std::fs::remove_file(dir.join("b.py")).unwrap();
        let inc = apply_reindex(&dir, Some(&inc), &cs_removed(&["b.py"]), &opts).expect("rm");
        assert!(!inc.graph.files.iter().any(|f| f.path.as_str() == "b.py"));
        assert!(!inc.graph.symbols.iter().any(|s| s.name == "beta"));
        assert!(inc.graph.symbols.iter().any(|s| s.name == "gamma"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extensionless_filter_keeps_scripts_drops_license() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("LICENSE"), "MIT\n").unwrap();
        std::fs::write(dir.join("Makefile"), "all:\n\ttrue\n").unwrap();
        std::fs::write(dir.join("bin/tool"), "#!/usr/bin/env bash\nf() { :; }\n").unwrap();
        let cs = ChangeSet {
            added: vec![RelPath::new("Makefile")],
            modified: vec![RelPath::new("LICENSE"), RelPath::new("bin/tool")],
            removed: Vec::new(),
        };
        let filtered = filter_changeset(&dir, cs);
        assert_eq!(filtered.added, vec![RelPath::new("Makefile")]);
        assert_eq!(
            filtered.modified,
            vec![RelPath::new("LICENSE"), RelPath::new("bin/tool")]
        );
        // Existing text-only files now pass (fresh index / generation).
        assert!(!filter_changeset(&dir, cs_modified(&["LICENSE"])).is_empty());
        // A script that loses its shebang becomes a removal.
        std::fs::write(dir.join("bin/tool"), "just text\n").unwrap();
        let lost = filter_changeset(&dir, cs_modified(&["bin/tool"]));
        assert_eq!(lost.removed, vec![RelPath::new("bin/tool")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn md_touch_of_missing_file_does_not_call_apply() {
        let (tx, rx) = mpsc::channel();
        let applied = Arc::new(Mutex::new(0usize));
        let applied_thread = Arc::clone(&applied);
        let join = std::thread::spawn(move || {
            for_each_reindex_batch(
                Path::new("/nonexistent-astrolabe-root"),
                rx,
                Duration::ZERO,
                |_| {
                    *applied_thread.lock().unwrap() += 1;
                },
            );
        });
        tx.send(cs_modified(&["README.md"])).unwrap();
        tx.send(cs_added(&["notes.sql"])).unwrap();
        drop(tx);
        join.join().unwrap();
        assert_eq!(
            *applied.lock().unwrap(),
            0,
            "changes to missing non-language files must not trigger a rebuild"
        );
    }

    #[test]
    fn text_only_filter_gates_size_and_binary() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("notes.md"), "needle\n").unwrap();
        std::fs::write(dir.join("blob.dat"), b"ab\0cd").unwrap();
        std::fs::write(dir.join("huge.txt"), vec![b'a'; 1_600_000]).unwrap();
        let f = filter_changeset(
            &dir,
            cs_added(&["notes.md", "blob.dat", "huge.txt", "none.md"]),
        );
        assert_eq!(f.added, vec![RelPath::new("notes.md")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn text_only_add_and_edit_visible_in_index_and_generation() {
        use crate::index::RepoIndex;
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        std::fs::write(dir.join("notes.md"), "needle\n").unwrap();
        let opts = mcp_index_options(false, DEFAULT_PARSE_CACHE_BYTES, true, &[]);
        let inc = IncrementalIndex::build(&dir, &opts);
        let before = RepoIndex::from_incremental(dir.clone(), inc);
        let names = |i: &RepoIndex| -> Vec<String> {
            i.text_only_paths(None)
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect()
        };
        assert_eq!(names(&before), vec!["notes.md".to_string()]);

        // (b) a brand-new text-only file shows up after the watcher batch.
        std::fs::write(dir.join("new.md"), "needle again\n").unwrap();
        let cs = filter_changeset(&dir, cs_added(&["new.md"]));
        assert!(!cs.is_empty(), "text-only add must publish a new index");
        let inc = apply_reindex(&dir, before.incremental(), &cs, &opts).expect("add");
        let after_add = RepoIndex::from_incremental(dir.clone(), inc);
        assert!(names(&after_add).contains(&"new.md".to_string()));

        // (a) an edit publishes a new index, hence a new generation stamp.
        std::fs::write(dir.join("notes.md"), "needle\nsecond needle\n").unwrap();
        let cs = filter_changeset(&dir, cs_modified(&["notes.md"]));
        assert!(!cs.is_empty(), "text-only edit must publish a new index");
        let inc = apply_reindex(&dir, after_add.incremental(), &cs, &opts).expect("mod");
        let after_edit = RepoIndex::from_incremental(dir.clone(), inc);
        assert_ne!(after_add.generation(), after_edit.generation());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn throttle_merges_burst_into_one_apply() {
        let (tx, rx) = mpsc::channel();
        tx.send(cs_added(&["a.py"])).unwrap();
        tx.send(cs_modified(&["b.rs"])).unwrap();
        tx.send(cs_added(&["c.go"])).unwrap();
        drop(tx);
        let applied = Arc::new(Mutex::new(Vec::<ChangeSet>::new()));
        let applied_thread = Arc::clone(&applied);
        let join = std::thread::spawn(move || {
            for_each_reindex_batch(
                Path::new("/nonexistent-astrolabe-root"),
                rx,
                Duration::from_millis(80),
                |cs| {
                    applied_thread.lock().unwrap().push(cs.clone());
                },
            );
        });
        join.join().unwrap();
        let batches = applied.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "dense changesets queued before apply must merge into one batch, got {batches:?}"
        );
        let batch = &batches[0];
        assert!(batch.added.iter().any(|p| p.as_str() == "a.py"));
        assert!(batch.added.iter().any(|p| p.as_str() == "c.go"));
        assert!(batch.modified.iter().any(|p| p.as_str() == "b.rs"));
    }

    #[test]
    fn follower_does_not_write_store() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("hello.py"), "def greet():\n    return 1\n").unwrap();

        let leader_slot = Mutex::new(None);
        assert!(
            refresh_leader(&leader_slot, &dir),
            "first acquire must become leader"
        );
        let follower_slot = Mutex::new(None);
        assert!(
            !refresh_leader(&follower_slot, &dir),
            "second acquire must be follower"
        );

        let persist = refresh_leader(&follower_slot, &dir);
        assert!(!persist);
        let opts = mcp_index_options(persist, DEFAULT_PARSE_CACHE_BYTES, true, &[]);
        let _ = IncrementalIndex::build(&dir, &opts);
        assert!(
            !store_path(&dir).exists(),
            "follower persist=false must not create index.redb"
        );

        // Leader still holds the lock; dropping it lets a follower promote.
        drop(leader_slot);
        assert!(
            refresh_leader(&follower_slot, &dir),
            "after leader drop the next try_acquire must promote"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leader_persist_creates_store_follower_leaves_mtime() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("hello.py"), "def greet():\n    return 1\n").unwrap();

        let leader_slot = Mutex::new(None);
        assert!(refresh_leader(&leader_slot, &dir));
        let opts = mcp_index_options(true, DEFAULT_PARSE_CACHE_BYTES, true, &[]);
        let previous = IncrementalIndex::build(&dir, &opts);
        let redb = store_path(&dir);
        assert!(redb.exists(), "leader persist=true must write index.redb");
        let before = std::fs::metadata(&redb).unwrap();
        let before_mtime = before.modified().unwrap();
        let before_len = before.len();

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("hello.py"), "def greet():\n    return 2\n").unwrap();

        let follower_slot = Mutex::new(None);
        assert!(!refresh_leader(&follower_slot, &dir));
        let follower_opts = mcp_index_options(false, DEFAULT_PARSE_CACHE_BYTES, true, &[]);
        let _ = apply_reindex(
            &dir,
            Some(&previous),
            &cs_modified(&["hello.py"]),
            &follower_opts,
        )
        .expect("follower incremental");

        let after = std::fs::metadata(&redb).unwrap();
        assert_eq!(after.len(), before_len);
        assert_eq!(after.modified().unwrap(), before_mtime);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
