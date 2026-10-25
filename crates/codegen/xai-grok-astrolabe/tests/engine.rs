//! End-to-end engine tests against a throwaway workspace.
//!
//! Each test builds its own root under a temp dir (no `.git`, so the root is
//! the temp dir), turns the watcher off, and queries through the same entry
//! points the tool layer uses.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use xai_grok_astrolabe::config::IndexRoot;
use xai_grok_astrolabe::{
    AstrolabeConfig, DenyPolicy, DiagnosticsRequest, EngineError, FindReferencesRequest,
    FindSymbolRequest, GotoDefinitionRequest, IndexPersist, LanguagesRequest, PathMap,
    ResolveContextRequest, SearchCodeRequest, engine_for, root_for_cwd,
};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join("src/util")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            concat!(
                "fn grep_timeout_secs(is_wsl: bool) -> u64 {\n",
                "    if is_wsl { 60 } else { 20 }\n",
                "}\n",
                "\n",
                "fn caller() -> u64 {\n",
                "    grep_timeout_secs(true)\n",
                "}\n",
            ),
        )
        .unwrap();
        fs::write(
            root.join("src/util/helper.rs"),
            "pub fn helper_timeout() -> u64 {\n    7\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("docs/notes.md"),
            "# Notes\n\nhelper_timeout is documented here.\n",
        )
        .unwrap();
        fs::write(root.join(".env"), "SECRET_TOKEN=do-not-leak\n").unwrap();
        Self { _tmp: tmp, root }
    }

    fn cfg(&self) -> AstrolabeConfig {
        AstrolabeConfig {
            index_root: IndexRoot::Cwd,
            watch: false,
            ready_timeout: Duration::from_secs(120),
            ..AstrolabeConfig::default()
        }
    }

    fn engine(&self) -> Arc<xai_grok_astrolabe::AstrolabeEngine> {
        let root = root_for_cwd(&self.root, &self.cfg());
        engine_for(&root, &self.cfg()).expect("engine")
    }

    fn map(&self, cwd: &Path) -> PathMap {
        PathMap::new(
            root_for_cwd(&self.root, &self.cfg()),
            cwd.to_path_buf(),
            None,
        )
    }
}

fn search(engine: &xai_grok_astrolabe::AstrolabeEngine, map: &PathMap, query: &str) -> String {
    engine
        .search_code(
            map,
            &DenyPolicy::default(),
            SearchCodeRequest {
                query: query.to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap()
        .text
}

#[test]
fn search_code_returns_body_and_cwd_relative_paths() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let cwd = fixture.root.join("src");
    let map = fixture.map(&cwd);

    let text = search(&engine, &map, "grep_timeout_secs");

    assert!(!text.contains("index_root"), "{text}");
    assert!(!text.contains(r"\\?\"), "{text}");
    assert!(
        text.contains("main.rs:"),
        "path must be cwd-relative: {text}"
    );
    assert!(
        !text.contains("src/main.rs"),
        "root-relative path leaked: {text}"
    );
    // The enclosing body comes with the hit, so no read_file is needed.
    assert!(text.contains("if is_wsl"), "{text}");
}

#[test]
fn gitignored_files_follow_the_session_policy() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(
        fixture.root.join("ignored.rs"),
        "fn ignored_policy_marker() {}\n",
    )
    .unwrap();

    let cfg = fixture.cfg();
    let root = root_for_cwd(&fixture.root, &cfg);
    let map = fixture.map(&fixture.root);

    let respecting =
        xai_grok_astrolabe::engine_for_session(&root, &cfg, true, &DenyPolicy::default())
            .expect("engine");
    let text = search(&respecting, &map, "ignored_policy_marker");
    assert!(!text.contains("ignored.rs"), "{text}");

    let open = xai_grok_astrolabe::engine_for_session(&root, &cfg, false, &DenyPolicy::default())
        .expect("engine");
    let text = search(&open, &map, "ignored_policy_marker");
    assert!(text.contains("ignored.rs"), "{text}");
}

/// Two engines for one root with opposite `.gitignore` policies share a store
/// path. The store is a per-file parse cache (content-hash keyed), not the
/// graph itself: each engine scans under its own policy, the exclusive
/// `indexer.lock` elects one writer, and the follower only reads. Concurrent
/// cold builds must still answer each policy correctly and leave one readable
/// store — never two leaders, never a half-written `index.redb`.
#[test]
fn concurrent_gitignore_policies_share_one_store_without_cross_talk() {
    use std::sync::Barrier;
    use std::thread;

    let fixture = Fixture::new();
    fs::write(fixture.root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(
        fixture.root.join("ignored.rs"),
        "fn concurrent_ignored_policy_marker() {}\n",
    )
    .unwrap();

    let cfg = fixture.cfg();
    let root = root_for_cwd(&fixture.root, &cfg);
    let map = fixture.map(&fixture.root);
    let barrier = Arc::new(Barrier::new(2));

    let spawn = |respect: bool| {
        let root = root.clone();
        let cfg = cfg.clone();
        let map = map.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let engine = xai_grok_astrolabe::engine_for_session(
                &root,
                &cfg,
                respect,
                &DenyPolicy::default(),
            )
            .expect("engine");
            // Align the first query so both cold builds race the store lock.
            barrier.wait();
            let text = search(&engine, &map, "concurrent_ignored_policy_marker");
            let leader = engine.is_leader();
            (text, leader, engine)
        })
    };

    // Both threads must be live before either joins: the barrier is size 2.
    let respecting_handle = spawn(true);
    let open_handle = spawn(false);
    let respecting = respecting_handle.join().expect("respect=true thread");
    let open = open_handle.join().expect("respect=false thread");

    assert!(
        !respecting.0.contains("ignored.rs"),
        "respect=true must stay blind to gitignored files: {}",
        respecting.0
    );
    assert!(
        open.0.contains("ignored.rs"),
        "respect=false must still see gitignored files: {}",
        open.0
    );
    // Both engines remain queryable after the race; the store must not be
    // corrupted into an open failure or empty graph.
    assert!(
        respecting.0.contains("concurrent_ignored_policy_marker")
            || open.0.contains("concurrent_ignored_policy_marker"),
        "at least one engine must answer the marker; got respect={} open={}",
        respecting.0,
        open.0
    );
    assert_eq!(
        respecting.1 as u8 + open.1 as u8,
        1,
        "exactly one of the dual-policy engines may hold the indexer lock \
         (respect leader={}, open leader={})",
        respecting.1,
        open.1
    );

    let store = fixture.root.join(".astrolabe").join("index.redb");
    assert!(
        store.is_file(),
        "persist=repo dual engines must leave a store at {}",
        store.display()
    );
    // A follow-up query after both threads finish must still succeed: the
    // store is openable and the graphs still answer.
    let again = search(&respecting.2, &map, "concurrent_ignored_policy_marker");
    assert!(!again.contains("ignored.rs"), "{again}");
    let again = search(&open.2, &map, "concurrent_ignored_policy_marker");
    assert!(again.contains("ignored.rs"), "{again}");
}

#[test]
fn search_code_glob_limits_candidates() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let mut req = SearchCodeRequest {
        query: "helper_timeout".to_string(),
        ..SearchCodeRequest::default()
    };
    req.glob = Some("*.md".to_string());
    let text = engine
        .search_code(&map, &DenyPolicy::default(), req)
        .unwrap()
        .text;
    assert!(text.contains("notes.md"), "{text}");
    assert!(!text.contains("helper.rs"), "{text}");
}

#[test]
fn search_code_exclude_skips_a_name_segment() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let mut req = SearchCodeRequest {
        query: "helper_timeout".to_string(),
        ..SearchCodeRequest::default()
    };
    req.exclude = Some(vec!["util".to_string()]);
    let text = engine
        .search_code(&map, &DenyPolicy::default(), req)
        .unwrap()
        .text;
    assert!(!text.contains("helper.rs"), "{text}");
    assert!(text.contains("notes.md"), "{text}");
}

#[test]
fn deny_policy_keeps_secrets_out_of_results() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);
    let deny = DenyPolicy::new(&["**/.env".to_string()]);

    let text = engine
        .search_code(
            &map,
            &deny,
            SearchCodeRequest {
                query: "SECRET_TOKEN".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap()
        .text;

    assert!(!text.contains("do-not-leak"), "{text}");
}

/// Build-time half of the `DenyReadGlobs` contract (plan §7.5.1): with the
/// query-side policy empty, a denied file can only stay out of the answer if
/// the scan never admitted it. The permissive index on the same root proves
/// the file really is indexable.
#[test]
fn deny_globs_are_excluded_at_index_time() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("src/classified.rs"),
        "pub fn classified_only_marker() -> u8 {\n    1\n}\n",
    )
    .unwrap();
    let cfg = fixture.cfg();
    let root = root_for_cwd(&fixture.root, &cfg);
    let map = fixture.map(&fixture.root);

    let plain =
        xai_grok_astrolabe::engine_for_session(&root, &cfg, true, &DenyPolicy::default())
            .expect("plain");
    let deny = DenyPolicy::new(&[".env".to_string(), "src/classified.rs".to_string()]);
    let strict = xai_grok_astrolabe::engine_for_session(&root, &cfg, true, &deny)
        .expect("strict");

    // Warm both indexes, then compare what got admitted.
    let plain_text = search(&plain, &map, "grep_timeout_secs");
    assert!(plain_text.contains("main.rs"), "{plain_text}");
    let strict_text = search(&strict, &map, "grep_timeout_secs");
    assert!(strict_text.contains("main.rs"), "{strict_text}");

    // Query-side policy is default-empty: no rewrite filter can hide these.
    let text = search(&strict, &map, "SECRET_TOKEN");
    assert!(!text.contains("do-not-leak"), "{text}");
    let symbols = strict
        .find_symbol(
            &map,
            &DenyPolicy::default(),
            FindSymbolRequest {
                query: "classified_only_marker".to_string(),
                ..FindSymbolRequest::default()
            },
        )
        .unwrap();
    assert!(
        symbols.meta.total.is_none_or(|t| t == 0),
        "a denied source file must contribute no symbols: {}",
        symbols.text
    );
    assert!(
        !symbols.text.contains("classified_only_marker"),
        "{}",
        symbols.text
    );

    // The permissive engine on the same root still sees both files, so the
    // two deny sets did not pollute each other through the shared store.
    let text = search(&plain, &map, "SECRET_TOKEN");
    assert!(text.contains("do-not-leak"), "{text}");
    let symbols = plain
        .find_symbol(
            &map,
            &DenyPolicy::default(),
            FindSymbolRequest {
                query: "classified_only_marker".to_string(),
                ..FindSymbolRequest::default()
            },
        )
        .unwrap();
    assert!(
        symbols.text.contains("classified_only_marker"),
        "{}",
        symbols.text
    );

    let strict_stats = strict.status().stats.expect("strict index ready");
    let plain_stats = plain.status().stats.expect("plain index ready");
    let strict_files = strict_stats["files"].as_u64().expect("files count");
    let plain_files = plain_stats["files"].as_u64().expect("files count");
    assert_eq!(
        plain_files,
        strict_files + 1,
        "only src/classified.rs may differ: strict={strict_stats}, plain={plain_stats}"
    );
    assert!(
        plain_stats["symbols"].as_u64().unwrap_or(0)
            > strict_stats["symbols"].as_u64().unwrap_or(0),
        "the denied file must not contribute symbols: strict={strict_stats}, plain={plain_stats}"
    );
}

/// Reverse creation order on a fresh root: a permissive index opened *after*
/// a denied one still sees the file the denied index skipped.
#[test]
fn permissive_index_is_not_blinded_by_a_prior_denied_index() {
    let fixture = Fixture::new();
    let cfg = fixture.cfg();
    let root = root_for_cwd(&fixture.root, &cfg);
    let map = fixture.map(&fixture.root);

    let deny = DenyPolicy::new(&[".env".to_string()]);
    let strict = xai_grok_astrolabe::engine_for_session(&root, &cfg, true, &deny)
        .expect("strict");
    let text = search(&strict, &map, "SECRET_TOKEN");
    assert!(!text.contains("do-not-leak"), "{text}");

    let plain =
        xai_grok_astrolabe::engine_for_session(&root, &cfg, true, &DenyPolicy::default())
            .expect("plain");
    let text = search(&plain, &map, "SECRET_TOKEN");
    assert!(text.contains("do-not-leak"), "{text}");
}

#[test]
fn find_symbol_returns_signature_and_anchor() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let out = engine
        .find_symbol(
            &map,
            &DenyPolicy::default(),
            FindSymbolRequest {
                query: "grep_timeout_secs".to_string(),
                include_body: true,
                ..FindSymbolRequest::default()
            },
        )
        .unwrap();

    assert!(out.text.contains("fn grep_timeout_secs"), "{}", out.text);
    assert!(out.text.contains("src/main.rs:"), "{}", out.text);
    assert_eq!(out.meta.total, Some(1));
}

#[test]
fn resolve_context_ranks_the_task_target_first() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let out = engine
        .resolve_context(
            &map,
            &DenyPolicy::default(),
            ResolveContextRequest {
                task: "fix grep_timeout_secs on WSL".to_string(),
                ..ResolveContextRequest::default()
            },
        )
        .unwrap();

    assert!(out.text.contains("main.rs"), "{}", out.text);
    assert!(!out.text.contains("index_root"), "{}", out.text);
}

#[test]
fn get_languages_reports_the_indexed_languages() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let out = engine
        .get_languages(
            &map,
            &DenyPolicy::default(),
            LanguagesRequest::default(),
            &[],
        )
        .unwrap();

    assert!(out.text.contains("rust"), "{}", out.text);
    assert!(!out.text.contains("index_root"), "{}", out.text);
}

#[test]
fn host_edits_are_visible_to_the_next_query() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    // Warm the index first so the edit is an incremental apply.
    let _ = search(&engine, &map, "grep_timeout_secs");

    let fresh = fixture.root.join("src/fresh.rs");
    fs::write(&fresh, "fn freshly_added_marker() -> u8 {\n    1\n}\n").unwrap();
    // The edit-tool hook path: only roots with a live engine are notified.
    xai_grok_astrolabe::note_edited_on_disk(&[fresh]);

    let text = search(&engine, &map, "freshly_added_marker");
    assert!(text.contains("fresh.rs"), "{text}");
}

#[test]
fn external_edits_are_visible_within_the_watch_window() {
    let fixture = Fixture::new();
    let cfg = AstrolabeConfig {
        index_root: IndexRoot::Cwd,
        watch: true,
        ready_timeout: Duration::from_secs(120),
        ..AstrolabeConfig::default()
    };
    let root = root_for_cwd(&fixture.root, &cfg);
    let engine = engine_for(&root, &cfg).expect("engine");
    let map = PathMap::new(root.clone(), root.clone(), None);
    // Warm the index so the watcher has a baseline.
    let _ = search(&engine, &map, "grep_timeout_secs");

    // No `note_edited_on_disk`: this exercises the watcher and the query-time
    // freshness barrier, which is what an edit from another process looks like.
    fs::write(
        root.join("src/external.rs"),
        "fn external_marker() -> u8 {\n    3\n}\n",
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        let text = search(&engine, &map, "external_marker");
        if text.contains("external.rs") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "external file not visible within 12 s: {text}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    engine.shutdown();
}

#[test]
fn roots_above_max_files_are_refused_before_indexing() {
    let fixture = Fixture::new();
    let cfg = AstrolabeConfig {
        index_root: IndexRoot::Cwd,
        watch: false,
        max_files: 2,
        ..AstrolabeConfig::default()
    };
    // The fixture has four regular files (two sources, one doc, one `.env`).
    let err = match engine_for(&fixture.root, &cfg) {
        Ok(_) => panic!("a root above max_files must not open"),
        Err(err) => err,
    };
    assert!(
        matches!(err, EngineError::TooManyFiles { limit: 2, .. }),
        "{err}"
    );
}

#[test]
fn persist_none_never_creates_the_store_directory() {
    let fixture = Fixture::new();
    let cfg = AstrolabeConfig {
        index_root: IndexRoot::Cwd,
        watch: false,
        persist: IndexPersist::None,
        ready_timeout: Duration::from_secs(120),
        ..AstrolabeConfig::default()
    };
    let engine = engine_for(&fixture.root, &cfg).expect("engine");
    let map = PathMap::new(fixture.root.clone(), fixture.root.clone(), None);
    let text = search(&engine, &map, "grep_timeout_secs");
    assert!(text.contains("main.rs"), "{text}");
    assert!(
        !fixture.root.join(".astrolabe").exists(),
        "persist = none must not write into the repository"
    );
}

#[test]
fn paths_outside_the_root_are_rejected() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let err = engine
        .search_code(
            &map,
            &DenyPolicy::default(),
            SearchCodeRequest {
                query: "anything".to_string(),
                path: Some("../../".to_string()),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(err, xai_grok_astrolabe::EngineError::Path(_)),
        "{err}"
    );
}

#[test]
fn zero_hits_are_reported_explicitly() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let out = engine
        .search_code(
            &map,
            &DenyPolicy::default(),
            SearchCodeRequest {
                query: "zzzz_no_such_symbol_zzzz".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap();
    assert!(!out.text.trim().is_empty(), "empty reply");
    assert!(
        out.text.to_lowercase().contains("未找到") || out.text.contains("no match"),
        "{}",
        out.text
    );
}

#[test]
fn line_numbers_are_one_based_and_match_the_file() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let text = search(&engine, &map, "grep_timeout_secs");

    // `fn grep_timeout_secs` is line 1; the call inside `caller` is line 6.
    assert!(text.contains("@src/main.rs:1 "), "{text}");
    assert!(text.contains("@src/main.rs:6 "), "{text}");
}

#[test]
fn paged_queries_resume_from_next_offset() {
    let fixture = Fixture::new();
    let mut cfg = fixture.cfg();
    cfg.budget_tokens = 200;
    let root = root_for_cwd(&fixture.root, &cfg);
    let engine = engine_for(&root, &cfg).expect("engine");
    let map = fixture.map(&fixture.root);

    let mut body = String::new();
    for i in 0..60 {
        body.push_str(&format!("fn page_marker_{i:02}() -> u8 {{ {i} }}\n"));
    }
    fs::write(fixture.root.join("src/paged.rs"), body).unwrap();
    assert!(engine.note_edited(&[fixture.root.join("src/paged.rs")]));

    let request = |offset: usize| FindSymbolRequest {
        query: "page_marker".to_string(),
        offset,
        ..FindSymbolRequest::default()
    };
    let first = engine
        .find_symbol(&map, &DenyPolicy::default(), request(0))
        .unwrap();
    let next = first
        .meta
        .next_offset
        .unwrap_or_else(|| panic!("60 symbols must page: {}", first.text));
    assert!(next > 0, "{}", first.text);
    assert!(first.text.contains("page_marker_"), "{}", first.text);

    let second = engine
        .find_symbol(&map, &DenyPolicy::default(), request(next as usize))
        .unwrap();
    assert!(!second.text.trim().is_empty());
    assert_ne!(first.text, second.text);
}

#[test]
fn aggregate_returns_counts_instead_of_bodies() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let map = fixture.map(&fixture.root);

    let out = engine
        .search_code(
            &map,
            &DenyPolicy::default(),
            SearchCodeRequest {
                query: "helper_timeout".to_string(),
                aggregate: Some("dir".to_string()),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap();
    assert!(
        out.text.contains("src/util") || out.text.contains("dir"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("pub fn helper_timeout"), "{}", out.text);
}

/// Manual check of the cwd-relative path contract on the real repository:
/// every rendered anchor must be usable by `read_file` from that cwd.
#[test]
#[ignore]
fn grokzen_subdir_paths_are_cwd_relative() {
    let root =
        dunce::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../")).unwrap();
    let cfg = AstrolabeConfig {
        watch: false,
        ready_timeout: Duration::from_secs(300),
        ..AstrolabeConfig::default()
    };
    let engine = engine_for(&root, &cfg).unwrap();
    let cwd = root.join("crates/codegen");
    let map = PathMap::new(root, cwd.clone(), None);

    let text = engine
        .search_code(
            &map,
            &DenyPolicy::default(),
            SearchCodeRequest {
                query: "grep_timeout_secs".to_string(),
                // Anchor lines (`@path:line`) render in context mode; the
                // default grouped view uses `L<line>` under a file header.
                context_lines: Some(6),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap()
        .text;
    assert!(!text.contains("@crates/codegen/"), "{text}");
    // The hit anchor must resolve to a real file from the session cwd.
    let anchor = text
        .lines()
        .find_map(|line| line.strip_prefix('@'))
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_else(|| panic!("no hit anchor in:\n{text}"));
    assert!(
        cwd.join(anchor).exists(),
        "anchor `{anchor}` does not resolve from {}",
        cwd.display()
    );

    let symbols = engine
        .find_symbol(
            &map,
            &DenyPolicy::default(),
            FindSymbolRequest {
                query: "grep_timeout_secs".to_string(),
                ..FindSymbolRequest::default()
            },
        )
        .unwrap()
        .text;
    assert!(!symbols.contains("@crates/codegen/"), "{symbols}");
}

/// Manual check against the real repository (slow, opt in):
/// `cargo test -p xai-grok-astrolabe --test engine -- --ignored`.
#[test]
#[ignore]
fn grokzen_repository_smoke() {
    let root =
        dunce::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../")).unwrap();
    let cfg = AstrolabeConfig {
        watch: false,
        ready_timeout: Duration::from_secs(300),
        ..AstrolabeConfig::default()
    };
    let root = root_for_cwd(&root, &cfg);
    let engine = engine_for(&root, &cfg).unwrap();
    let map = PathMap::new(root.clone(), root, None);

    let out = engine
        .search_code(
            &map,
            &DenyPolicy::default(),
            SearchCodeRequest {
                query: "grep_timeout_secs".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .unwrap();
    assert!(out.text.contains("grep/mod.rs"), "{}", out.text);
    eprintln!(
        "search: total={:?} next_offset={:?} bytes={}",
        out.meta.total,
        out.meta.next_offset,
        out.text.len()
    );
}

// ---------------------------------------------------------------------------
// Precise tools: index-first, host LSP only when the index cannot decide
// ---------------------------------------------------------------------------

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn precise_engine(
    files: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    PathBuf,
    Arc<xai_grok_astrolabe::AstrolabeEngine>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    for (rel, text) in files {
        write(&root, rel, text);
    }
    let cfg = AstrolabeConfig {
        index_root: IndexRoot::Cwd,
        watch: false,
        ready_timeout: Duration::from_secs(120),
        ..AstrolabeConfig::default()
    };
    let engine = engine_for(&root, &cfg).expect("engine");
    (tmp, root, engine)
}

fn references(
    engine: &xai_grok_astrolabe::AstrolabeEngine,
    map: &PathMap,
    symbol: &str,
    path: Option<&str>,
    host_lsp: bool,
) -> xai_grok_astrolabe::PreciseOutcome {
    engine
        .find_references(
            map,
            &DenyPolicy::default(),
            FindReferencesRequest {
                symbol: symbol.to_string(),
                path: path.map(str::to_string),
                ..FindReferencesRequest::default()
            },
            host_lsp,
        )
        .unwrap()
}

#[test]
fn go_references_are_answered_by_the_index() {
    let (_tmp, root, engine) = precise_engine(&[(
        "main.go",
        "package main\n\nfunc helperGo() int { return 1 }\n\nfunc main() { _ = helperGo() }\n",
    )]);
    let map = PathMap::new(root.clone(), root, None);
    let out = references(&engine, &map, "helperGo", Some("main.go"), true);
    assert!(
        !out.needs_host_lsp,
        "Go should be answered by the index: {}",
        out.output.text
    );
    assert!(out.output.text.contains("main.go:3"), "{}", out.output.text);
    assert!(!out.output.text.contains("index_root"));
    assert!(
        out.output.meta.confidence.as_deref() != Some("unknown"),
        "{}",
        out.output.text
    );
}

#[test]
fn typescript_top_level_references_are_answered_by_the_index() {
    let (_tmp, root, engine) = precise_engine(&[
        (
            "src/a.ts",
            "export function helperTs(): number { return 1; }\n",
        ),
        (
            "src/b.ts",
            "import { helperTs } from './a';\nexport const value = helperTs();\n",
        ),
    ]);
    let map = PathMap::new(root.clone(), root, None);
    let out = references(&engine, &map, "helperTs", Some("src/a.ts"), true);
    assert!(
        !out.needs_host_lsp,
        "unique TS declaration should be answered by the index: {}",
        out.output.text
    );
    assert!(out.output.text.contains("1 处"), "{}", out.output.text);
}

#[test]
fn python_module_level_references_are_answered_by_the_index() {
    let (_tmp, root, engine) = precise_engine(&[
        ("a.py", "def helper_py():\n    return 1\n"),
        ("b.py", "from a import helper_py\n\nhelper_py()\n"),
    ]);
    let map = PathMap::new(root.clone(), root, None);
    let out = references(&engine, &map, "helper_py", Some("a.py"), true);
    assert!(
        !out.needs_host_lsp,
        "module-level Python name should be answered by the index: {}",
        out.output.text
    );
    assert!(out.output.text.contains("1 处"), "{}", out.output.text);
}

#[test]
fn rust_references_defer_to_the_host_language_server() {
    let (_tmp, root, engine) = precise_engine(&[(
        "src/lib.rs",
        "pub fn helper_rs() -> u8 {\n    1\n}\n\npub fn caller() -> u8 {\n    helper_rs()\n}\n",
    )]);
    let map = PathMap::new(root.clone(), root, None);
    let out = references(&engine, &map, "helper_rs", Some("src/lib.rs"), true);
    assert!(
        out.needs_host_lsp,
        "Rust references need a server: {}",
        out.output.text
    );
    let anchor = out.anchor.expect("anchor for the host server");
    assert_eq!(anchor.engine_path, "src/lib.rs");
    assert_eq!(anchor.line, 0);
    assert!(anchor.absolute.exists());
    // Without a host server the tool still says why instead of pretending.
    let no_host = references(&engine, &map, "helper_rs", Some("src/lib.rs"), false);
    assert!(!no_host.needs_host_lsp);
    assert!(
        no_host.output.text.contains("lsp")
            || no_host.output.text.contains("语言服务器")
            || no_host.output.text.contains("unknown"),
        "{}",
        no_host.output.text
    );
}

#[test]
fn goto_definition_from_a_name_lists_go_candidates() {
    let (_tmp, root, engine) = precise_engine(&[(
        "main.go",
        "package main\n\nfunc helperGo() int { return 1 }\n\nfunc main() { _ = helperGo() }\n",
    )]);
    let map = PathMap::new(root.clone(), root, None);
    let out = engine
        .goto_definition(
            &map,
            &DenyPolicy::default(),
            GotoDefinitionRequest {
                symbol: Some("main".to_string()),
                ..GotoDefinitionRequest::default()
            },
            true,
        )
        .unwrap();
    assert!(out.output.text.contains("main.go:5"), "{}", out.output.text);
    assert!(!out.needs_host_lsp, "{}", out.output.text);
}

#[test]
fn goto_definition_rust_position_defers_with_an_anchor() {
    let (_tmp, root, engine) = precise_engine(&[("src/lib.rs", "fn helper_rs() {}\n")]);
    let map = PathMap::new(root.clone(), root, None);
    let out = engine
        .goto_definition(
            &map,
            &DenyPolicy::default(),
            GotoDefinitionRequest {
                path: Some("src/lib.rs".to_string()),
                symbol: Some("helper_rs".to_string()),
                line: Some(1),
                ..GotoDefinitionRequest::default()
            },
            true,
        )
        .unwrap();
    assert!(out.needs_host_lsp, "{}", out.output.text);
    let anchor = out.anchor.expect("anchor");
    assert_eq!(anchor.line, 0);
    assert!(anchor.character > 0, "column should snap onto the name");
}

#[test]
fn diagnostics_fall_back_to_the_parser_without_an_lsp() {
    let (_tmp, root, engine) = precise_engine(&[
        ("src/lib.rs", "fn broken( {\n"),
        (
            "Cargo.toml",
            "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
    ]);
    let map = PathMap::new(root.clone(), root.clone(), None);
    let out = engine
        .get_diagnostics(
            &map,
            &DenyPolicy::default(),
            DiagnosticsRequest {
                path: "src/lib.rs".to_string(),
                severity: None,
            },
        )
        .unwrap();
    assert!(out.text.contains("lib.rs"), "{}", out.text);
    assert!(!out.text.contains("index_root"), "{}", out.text);
}
