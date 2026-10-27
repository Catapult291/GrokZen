//! Repository index held by the server.
//!
//! The build itself lives in `astrolabe_core::index` — this module only adds
//! the lookups the tool layer needs. Keeping one orchestration path means the
//! numbers the server reports are the same ones the acceptance suite measures.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::UNIX_EPOCH,
};

use astrolabe_core::cache::FileCache;
use astrolabe_core::graph::CodeGraph;
use astrolabe_core::index::IncrementalIndex;
use astrolabe_core::{CodeFile, FileId, IndexReport, RelPath};

use crate::reindex::mcp_index_options;

/// Serving snapshot. Production holds an [`IncrementalIndex`] so the watch
/// thread can `apply_changeset` without a full rescan. Unit tests that inject a
/// synthetic graph use [`RepoIndex::from_graph`].
pub(crate) struct RepoIndex {
    pub root: PathBuf,
    inner: InnerIndex,
    in_graph_paths: OnceLock<HashSet<String>>,
    names: OnceLock<NameIndex>,
    /// Path → file and file → symbols tables (built on first lookup).
    files_lookup: OnceLock<FileLookup>,
    /// Sub-word index for `find_symbol`'s zero-hit fallback, built on first use.
    words: OnceLock<crate::word_index::WordIndex>,
    /// Markdown coverage report path, written lazily once per snapshot.
    coverage_report: OnceLock<Option<PathBuf>>,
    /// Per partial-parse file: language, first error and error kind.
    partial_diagnoses: OnceLock<Vec<crate::coverage::PartialEntry>>,
    /// Process-unique stamp; every index replacement (edit, rebuild) gets a new one.
    generation: u64,
}

/// Lookup tables over `graph().symbols`, built once per index snapshot (the
/// snapshot is immutable, so they never go stale): lowercase names and a
/// name-sorted inverted index (in `name_path.names()`), id → position and
/// file id → position. Built in parallel, and prewarmed off the query path
/// right after an index is published ([`RepoIndex::prewarm`]).
pub(crate) struct NameIndex {
    pub by_id: std::collections::HashMap<astrolabe_core::SymbolId, usize>,
    pub name_path: astrolabe_core::name_path::NamePathIndex,
    pub file_pos: std::collections::HashMap<FileId, usize>,
}

impl NameIndex {
    fn table(&self) -> &astrolabe_core::name_path::NameTable {
        self.name_path
            .names()
            .expect("NameIndex builds its NamePathIndex with names")
    }

    /// `name.to_lowercase()` of every symbol, by position.
    pub fn lower(&self) -> &[String] {
        self.table().lower()
    }

    /// Positions of the symbols whose lowercase name is `lower_name`.
    pub fn by_lower(&self, lower_name: &str) -> &[u32] {
        self.table().exact(lower_name)
    }

    /// Some symbol is named exactly `name` (case-sensitive).
    pub fn has_exact(&self, symbols: &[astrolabe_core::CodeSymbol], name: &str) -> bool {
        self.by_lower(&name.to_lowercase())
            .iter()
            .any(|&i| symbols[i as usize].name == name)
    }

    /// One group of positions per distinct lowercase name; `group[0]` is
    /// that name's first occurrence in `graph().symbols`.
    pub fn name_groups(&self) -> impl Iterator<Item = &[u32]> {
        self.table().groups()
    }
}

/// Per-snapshot file lookups that replace linear scans on the query path:
/// exact path → file, file name → candidate files (for suffix targets), and
/// file → positions of its symbols in `graph().symbols` (in graph order).
pub(crate) struct FileLookup {
    exact: std::collections::HashMap<Box<str>, FileId>,
    /// Last path component → indices into `graph().files`, in graph order.
    by_basename: std::collections::HashMap<Box<str>, Vec<u32>>,
    symbols_by_file: std::collections::HashMap<FileId, Vec<u32>>,
}

impl FileLookup {
    fn build(graph: &CodeGraph) -> Self {
        let mut exact = std::collections::HashMap::with_capacity(graph.files.len());
        let mut by_basename: std::collections::HashMap<Box<str>, Vec<u32>> =
            std::collections::HashMap::with_capacity(graph.files.len());
        for (i, file) in graph.files.iter().enumerate() {
            let path = file.path.as_str();
            // First file with a path wins, as the old linear `find` did.
            exact.entry(path.into()).or_insert(file.id);
            let base = path.rsplit('/').next().unwrap_or(path);
            by_basename.entry(base.into()).or_default().push(i as u32);
        }
        let mut symbols_by_file: std::collections::HashMap<FileId, Vec<u32>> =
            std::collections::HashMap::new();
        for (i, symbol) in graph.symbols.iter().enumerate() {
            symbols_by_file
                .entry(symbol.file)
                .or_default()
                .push(i as u32);
        }
        Self {
            exact,
            by_basename,
            symbols_by_file,
        }
    }
}

enum InnerIndex {
    Incremental(IncrementalIndex),
    #[cfg(test)]
    Static {
        graph: CodeGraph,
        report: IndexReport,
    },
}

impl std::fmt::Debug for RepoIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoIndex")
            .field("root", &self.root)
            .field("files", &self.graph().files.len())
            .field("symbols", &self.graph().symbols.len())
            .finish_non_exhaustive()
    }
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl RepoIndex {
    /// Index generation stamp (changes whenever the index is replaced).
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn from_incremental(root: PathBuf, incremental: IncrementalIndex) -> Self {
        Self {
            root,
            inner: InnerIndex::Incremental(incremental),
            in_graph_paths: OnceLock::new(),
            names: OnceLock::new(),
            files_lookup: OnceLock::new(),
            words: OnceLock::new(),
            coverage_report: OnceLock::new(),
            partial_diagnoses: OnceLock::new(),
            generation: next_generation(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_graph(root: PathBuf, graph: CodeGraph, report: IndexReport) -> Self {
        Self {
            root,
            inner: InnerIndex::Static { graph, report },
            in_graph_paths: OnceLock::new(),
            names: OnceLock::new(),
            files_lookup: OnceLock::new(),
            words: OnceLock::new(),
            coverage_report: OnceLock::new(),
            partial_diagnoses: OnceLock::new(),
            generation: next_generation(),
        }
    }

    pub(crate) fn names(&self) -> &NameIndex {
        self.names.get_or_init(|| {
            let graph = self.graph();
            let symbols = &graph.symbols;
            let (name_path, (by_id, file_pos)) = rayon::join(
                || astrolabe_core::name_path::NamePathIndex::with_names(symbols),
                || {
                    let mut by_id = std::collections::HashMap::with_capacity(symbols.len());
                    for (i, symbol) in symbols.iter().enumerate() {
                        by_id.entry(symbol.id).or_insert(i);
                    }
                    let file_pos = graph
                        .files
                        .iter()
                        .enumerate()
                        .rev()
                        .map(|(i, f)| (f.id, i))
                        .collect();
                    (by_id, file_pos)
                },
            );
            NameIndex {
                by_id,
                name_path,
                file_pos,
            }
        })
    }

    /// The sub-word index over this snapshot's symbols (built once, lazily:
    /// only zero-hit multi-word `find_symbol` queries need it).
    pub(crate) fn words(&self) -> &crate::word_index::WordIndex {
        self.words
            .get_or_init(|| crate::word_index::WordIndex::build(self.graph()))
    }

    /// Build the per-snapshot lookup tables on a background thread, so the
    /// first `find_symbol` after a publish does not pay for them. A query
    /// that arrives first simply builds (or waits for) the same `OnceLock`.
    ///
    /// With `coverage`, the syntax-error diagnoses `get_languages` needs (one
    /// re-parse per partial-parse file) and the coverage report are prepared
    /// too. Only for a cold build or full rebuild: a watcher apply after
    /// every save would re-parse all of them each time.
    pub(crate) fn prewarm(self: &Arc<Self>, coverage: bool) {
        let index = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("astrolabe-prewarm".into())
            .spawn(move || {
                index.file_lookup();
                index.names();
                if coverage {
                    index.coverage_detail();
                    // A cold build / full rebuild has freed its parse
                    // temporaries by now; hand them back to the OS (glibc
                    // only). Watcher applies trim later, once edits go quiet
                    // (`apply_pending`).
                    crate::alloc::release_free_memory();
                }
            });
    }

    pub(crate) fn graph(&self) -> &CodeGraph {
        match &self.inner {
            InnerIndex::Incremental(inc) => &inc.graph,
            #[cfg(test)]
            InnerIndex::Static { graph, .. } => graph,
        }
    }

    pub(crate) fn report(&self) -> &IndexReport {
        match &self.inner {
            InnerIndex::Incremental(inc) => &inc.report,
            #[cfg(test)]
            InnerIndex::Static { report, .. } => report,
        }
    }

    pub(crate) fn coverage_report_cell(&self) -> &OnceLock<Option<PathBuf>> {
        &self.coverage_report
    }

    pub(crate) fn partial_diagnoses_cell(&self) -> &OnceLock<Vec<crate::coverage::PartialEntry>> {
        &self.partial_diagnoses
    }

    pub(crate) fn incremental(&self) -> Option<&IncrementalIndex> {
        match &self.inner {
            InnerIndex::Incremental(inc) => Some(inc),
            #[cfg(test)]
            InnerIndex::Static { .. } => None,
        }
    }

    pub fn file(&self, id: FileId) -> Option<&CodeFile> {
        // `.rev()` above makes the first file with this id win, as `find` did.
        let pos = *self.names().file_pos.get(&id)?;
        self.graph().files.get(pos)
    }

    /// Count occurrences of unindexed code file extensions among admitted paths.
    ///
    /// Delegates to [`astrolabe_core::index::unindexed_code_exts`] on the retained
    /// [`IncrementalIndex`]. Only returns entries for unindexed, unsupported code languages.
    pub(crate) fn unindexed_code_exts(
        &self,
        filter: Option<&str>,
    ) -> std::collections::BTreeMap<String, u32> {
        match &self.inner {
            InnerIndex::Incremental(inc) => {
                let in_graph_set = self.in_graph_paths.get_or_init(|| {
                    self.graph()
                        .files
                        .iter()
                        .map(|file| file.path.as_str().to_owned())
                        .collect()
                });
                astrolabe_core::index::unindexed_code_exts(
                    inc.admitted_paths(),
                    &|path| in_graph_set.contains(path),
                    filter,
                )
            }
            #[cfg(test)]
            InnerIndex::Static { .. } => std::collections::BTreeMap::new(),
        }
    }

    /// Scanned text files that are not in the code graph (docs, config,
    /// SQL, templates, unsupported languages …), filtered by the
    /// `path_filter` substring. These already passed the scanner's
    /// gitignore / size / binary gates. Lock files are skipped: they are
    /// large, generated, and drown real hits.
    pub(crate) fn text_only_paths(&self, filter: Option<&str>) -> Vec<&RelPath> {
        match &self.inner {
            InnerIndex::Incremental(inc) => {
                let in_graph_set = self.in_graph_paths.get_or_init(|| {
                    self.graph()
                        .files
                        .iter()
                        .map(|file| file.path.as_str().to_owned())
                        .collect()
                });
                inc.admitted_paths()
                    .iter()
                    .filter(|path| {
                        let s = path.as_str();
                        filter.is_none_or(|f| s.contains(f))
                            && !in_graph_set.contains(s)
                            && !is_lock_file(s)
                            && !is_vcs_ignore_file(s)
                    })
                    .collect()
            }
            #[cfg(test)]
            InnerIndex::Static { .. } => Vec::new(),
        }
    }

    /// Authored sources hidden by a declared build-output exclude (e.g. a
    /// tsconfig `outDir` that still contains sources). Empty for static test
    /// indexes.
    pub(crate) fn excluded_source_paths(&self) -> &[astrolabe_core::types::RelPath] {
        match &self.inner {
            InnerIndex::Incremental(inc) => inc.excluded_source_paths(),
            #[cfg(test)]
            InnerIndex::Static { .. } => &[],
        }
    }

    /// Load source lines through the process-wide [`FileCache`].
    ///
    /// The current mtime is always passed to [`FileCache::get`], so an edited
    /// file is a miss and the stale entry is invalidated. The `bool` is
    /// `true` on a cache hit.
    pub fn cached_lines(
        &self,
        cache: &FileCache,
        rel_path: &str,
    ) -> Option<(Arc<Vec<String>>, bool)> {
        load_source_lines(cache, &self.root, rel_path)
    }

    /// Resolve a user-supplied target to a file. Accepts a full repo-relative
    /// path or any path suffix, so an agent can pass `server.ts` without
    /// knowing the directory. Exact matches win; among suffix matches the
    /// shortest path wins, which keeps the choice stable and predictable.
    pub fn file_id(&self, target: &str) -> Option<FileId> {
        let target = target.trim_start_matches("./");
        let lookup = self.file_lookup();
        if let Some(&id) = lookup.exact.get(target) {
            return Some(id);
        }
        // A path ending in `/{target}` has `target`'s last component as its
        // file name, so only files with that name can match.
        let base = target.rsplit('/').next().unwrap_or(target);
        let files = &self.graph().files;
        lookup
            .by_basename
            .get(base)?
            .iter()
            .map(|&i| &files[i as usize])
            .filter(|file| {
                let path = file.path.as_str();
                path.len() > target.len()
                    && path.ends_with(target)
                    && path.as_bytes()[path.len() - target.len() - 1] == b'/'
            })
            .min_by_key(|file| file.path.as_str().len())
            .map(|file| file.id)
    }

    fn file_lookup(&self) -> &FileLookup {
        self.files_lookup
            .get_or_init(|| FileLookup::build(self.graph()))
    }

    /// Symbols of `file`, in `graph().symbols` order.
    pub fn file_symbols(&self, file: FileId) -> impl Iterator<Item = &astrolabe_core::CodeSymbol> {
        let symbols = &self.graph().symbols;
        self.file_lookup()
            .symbols_by_file
            .get(&file)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(move |&i| &symbols[i as usize])
    }
}

/// Generated dependency lock files excluded from text-only search.
fn is_lock_file(path: &str) -> bool {
    const LOCKS: &[&str] = &[
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "Cargo.lock",
        "go.sum",
        "poetry.lock",
        "Pipfile.lock",
        "uv.lock",
        "composer.lock",
        "Gemfile.lock",
        "pubspec.lock",
        "Podfile.lock",
        "packages.lock.json",
    ];
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCKS.contains(&name)
}

/// VCS ignore files hold path patterns, not source. Ripgrep skips them as
/// hidden files, so returning them from `search_code` would make the two
/// search tools disagree on the same literal.
fn is_vcs_ignore_file(path: &str) -> bool {
    matches!(
        path.rsplit('/').next().unwrap_or(path),
        ".gitignore" | ".ignore"
    )
}

/// Read a source file through [`FileCache`], keyed by repo-relative path.
///
/// Returns `(lines, hit)`. A changed mtime is a miss; the cache already
/// invalidates the stale entry in that case. On a miss the file is read from
/// disk, inserted, and returned even if it is immediately evicted for being
/// larger than the byte budget.
pub(crate) fn load_source_lines(
    cache: &FileCache,
    root: &Path,
    rel_path: &str,
) -> Option<(Arc<Vec<String>>, bool)> {
    let disk = root.join(rel_path);
    // Stat before reading: a write after this point changes mtime/len, so
    // the entry inserted below is never served for the newer content.
    let metadata = fs::metadata(&disk).ok();
    let mtime_ms = metadata.as_ref().and_then(mtime_ms_of).unwrap_or(0);
    let len = metadata.as_ref().map(fs::Metadata::len);
    if let Some(len) = len {
        if let Some(lines) = cache.get_checked(rel_path, mtime_ms, len) {
            return Some((lines, true));
        }
    }
    let source = astrolabe_core::text::read_source(&disk).ok()?;
    let lines = Arc::new(source.lines().map(str::to_owned).collect::<Vec<String>>());
    if let Some(len) = len {
        cache.insert_checked(rel_path, Arc::clone(&lines), mtime_ms, len);
    }
    Some((lines, false))
}

fn mtime_ms_of(metadata: &fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
pub(crate) fn build(root: &Path) -> anyhow::Result<RepoIndex> {
    build_with_options(root, true, astrolabe_core::index::DEFAULT_PARSE_CACHE_BYTES)
}

/// Uncancellable build; the server itself uses [`build_cancellable`].
#[cfg(test)]
pub(crate) fn build_with_options(
    root: &Path,
    persist: bool,
    parse_cache_bytes: u64,
) -> anyhow::Result<RepoIndex> {
    build_with_options_full(root, persist, parse_cache_bytes, true)
}

#[cfg(test)]
pub(crate) fn build_with_options_full(
    root: &Path,
    persist: bool,
    parse_cache_bytes: u64,
    respect_gitignore: bool,
) -> anyhow::Result<RepoIndex> {
    let opts = mcp_index_options(persist, parse_cache_bytes, respect_gitignore, &[]);
    let incremental = IncrementalIndex::build(root, &opts);
    Ok(RepoIndex::from_incremental(root.to_path_buf(), incremental))
}

/// [`build_with_options`] that returns `None` once `cancel` is set (see
/// [`IncrementalIndex::build_cancellable`]). `extra_excludes` are the
/// session's deny globs: the cold scan must never read those files.
pub(crate) fn build_cancellable(
    root: &Path,
    persist: bool,
    parse_cache_bytes: u64,
    respect_gitignore: bool,
    extra_excludes: &[String],
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<RepoIndex> {
    let opts = mcp_index_options(
        persist,
        parse_cache_bytes,
        respect_gitignore,
        extra_excludes,
    );
    let incremental = IncrementalIndex::build_cancellable(root, &opts, cancel)?;
    Some(RepoIndex::from_incremental(root.to_path_buf(), incremental))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(paths: &[&str], symbols: &[(u32, u32, u32)]) -> RepoIndex {
        use astrolabe_core::{CodeSymbol, Language, SymbolId, SymbolKind};
        let graph = CodeGraph {
            files: paths
                .iter()
                .enumerate()
                .map(|(i, p)| CodeFile {
                    id: FileId(i as u32),
                    path: RelPath::new(p),
                    language: Some(Language::Rust),
                    loc: 1,
                    sha: String::new(),
                })
                .collect(),
            symbols: symbols
                .iter()
                .enumerate()
                .map(|(i, &(file, start, end))| CodeSymbol {
                    id: SymbolId(i as u32),
                    file: FileId(file),
                    name: format!("s{i}"),
                    kind: SymbolKind::Function,
                    signature: String::new(),
                    start_line: start,
                    end_line: end,
                    exported: true,
                })
                .collect(),
            edges: vec![],
        };
        RepoIndex::from_graph(PathBuf::from("."), graph, IndexReport::default())
    }

    /// The pre-index linear `file_id`, kept as the reference.
    fn file_id_linear(index: &RepoIndex, target: &str) -> Option<FileId> {
        let target = target.trim_start_matches("./");
        let files = &index.graph().files;
        if let Some(file) = files.iter().find(|f| f.path.as_str() == target) {
            return Some(file.id);
        }
        let suffix = format!("/{target}");
        files
            .iter()
            .filter(|f| f.path.as_str().ends_with(&suffix))
            .min_by_key(|f| f.path.as_str().len())
            .map(|f| f.id)
    }

    #[test]
    fn file_id_lookup_matches_linear_scan() {
        let paths = [
            "a.rs",
            "src/a.rs",
            "src/lib/a.rs",
            "x/src/a.rs",
            "y/src/a.rs",
            "b.rs",
            "src/b.rs",
            "srcb.rs",
            "deep/one/two/c.go",
            "one/two/c.go",
            "dir/",
            "weird//d.rs",
            "日本/語.rs",
        ];
        let index = synthetic(&paths, &[]);
        let targets = [
            "a.rs",
            "./a.rs",
            "src/a.rs",
            "./src/a.rs",
            "lib/a.rs",
            "rc/a.rs",
            "b.rs",
            "src/b.rs",
            "rcb.rs",
            "c.go",
            "two/c.go",
            "one/two/c.go",
            "/c.go",
            "",
            "/",
            "dir",
            "dir/",
            "d.rs",
            "/d.rs",
            "weird//d.rs",
            "語.rs",
            "日本/語.rs",
            "missing.rs",
            "a",
            ".rs",
            "./",
            "././a.rs",
        ];
        for t in targets {
            assert_eq!(index.file_id(t), file_id_linear(&index, t), "target {t:?}");
        }
    }

    #[test]
    fn file_symbols_lists_a_files_symbols_in_graph_order() {
        // Symbols of one file need not be contiguous.
        let index = synthetic(
            &["a.rs", "b.rs"],
            &[(0, 1, 9), (1, 1, 3), (0, 2, 4), (1, 5, 5)],
        );
        let names = |f: u32| -> Vec<String> {
            index
                .file_symbols(FileId(f))
                .map(|s| s.name.clone())
                .collect()
        };
        assert_eq!(names(0), ["s0", "s2"]);
        assert_eq!(names(1), ["s1", "s3"]);
        assert!(names(7).is_empty());
    }

    /// `prewarm` builds the lookup tables off the query path; a query that
    /// arrives meanwhile gets the same tables from the `OnceLock`.
    #[test]
    fn prewarm_builds_name_tables_in_the_background() {
        use astrolabe_core::{CodeSymbol, Language, SymbolId, SymbolKind};
        let graph = CodeGraph {
            files: vec![CodeFile {
                id: FileId(0),
                path: RelPath::new("a.rs"),
                language: Some(Language::Rust),
                loc: 1,
                sha: String::new(),
            }],
            symbols: vec![CodeSymbol {
                id: SymbolId(7),
                file: FileId(0),
                name: "Alpha".into(),
                kind: SymbolKind::Function,
                signature: "fn Alpha()".into(),
                start_line: 1,
                end_line: 1,
                exported: true,
            }],
            edges: vec![],
        };
        let index = Arc::new(RepoIndex::from_graph(
            PathBuf::from("."),
            graph,
            IndexReport::default(),
        ));
        assert!(index.names.get().is_none());
        index.prewarm(true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while index.names.get().is_none() {
            assert!(std::time::Instant::now() < deadline, "prewarm never ran");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let names = index.names();
        assert_eq!(names.by_lower("alpha"), &[0]);
        assert!(names.has_exact(&index.graph().symbols, "Alpha"));
        assert!(!names.has_exact(&index.graph().symbols, "alpha"));
        assert_eq!(names.by_id[&SymbolId(7)], 0);
        assert_eq!(index.file(FileId(0)).map(|f| f.path.as_str()), Some("a.rs"));
    }

    fn unique_temp_dir() -> PathBuf {
        // A timestamp alone is not unique: macOS resolves `SystemTime::now`
        // to roughly a microsecond, so two tests starting in the same instant
        // get the same path, and whichever finishes first deletes the other's
        // files mid-run. The counter makes collisions impossible.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-mcp-{}-{}-{}-{}",
            "index",
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

    #[test]
    fn build_returns_real_index_not_unimplemented() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("hello.py"), "def greet():\n    return 'hi'\n").unwrap();
        let index = build(&dir).expect("index_repo should return a graph, not fail");
        assert!(
            index
                .graph()
                .files
                .iter()
                .any(|file| file.path.as_str().ends_with("hello.py")),
            "expected hello.py in the graph.\n  dir={}\n  dir_entries={:?}\n  files={:?}\n  scanned={} parsed={}\n  parse_failures={:?}\n  skipped_or_unresolved={:?}",
            dir.display(),
            std::fs::read_dir(&dir).map(|rd| rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()),
            index
                .graph()
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            index.report().files_scanned,
            index.report().files_parsed,
            index.report().parse_failures,
            index.report().unresolved_imports,
        );
        assert!(
            index.incremental().is_some(),
            "production build must retain an IncrementalIndex snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_read_hits_then_misses_after_mtime_change() {
        use astrolabe_core::cache::{CacheConfig, FileCache};
        use std::time::Duration;

        let dir = unique_temp_dir();
        let file = dir.join("hello.py");
        std::fs::write(&file, "line-one\n").unwrap();
        let cache = FileCache::new(CacheConfig {
            budget_bytes: 64 * 1024,
            idle_ttl: Duration::from_secs(60),
        });

        let (lines, hit) = load_source_lines(&cache, &dir, "hello.py").unwrap();
        assert!(!hit, "first read must be a miss");
        assert_eq!(&*lines, &["line-one".to_string()]);

        let (lines, hit) = load_source_lines(&cache, &dir, "hello.py").unwrap();
        assert!(hit, "unchanged mtime must be a hit");
        assert_eq!(&*lines, &["line-one".to_string()]);

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&file, "line-two\n").unwrap();
        let (lines, hit) = load_source_lines(&cache, &dir, "hello.py").unwrap();
        assert!(!hit, "changed mtime must be a miss");
        assert_eq!(&*lines, &["line-two".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn respect_gitignore_false_admits_ignored_files() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.join("ignored.rs"), "fn hidden() {}\n").unwrap();
        std::fs::write(dir.join("visible.rs"), "fn shown() {}\n").unwrap();

        let paths = |index: &RepoIndex| -> Vec<String> {
            index
                .graph()
                .files
                .iter()
                .map(|file| file.path.as_str().to_owned())
                .collect()
        };

        let respecting = build_with_options_full(&dir, false, 1 << 20, true).expect("index build");
        let ignored = paths(&respecting);
        assert!(!ignored.iter().any(|p| p == "ignored.rs"), "{ignored:?}");
        assert!(ignored.iter().any(|p| p == "visible.rs"), "{ignored:?}");

        let open = build_with_options_full(&dir, false, 1 << 20, false).expect("index build");
        let ignored = paths(&open);
        assert!(ignored.iter().any(|p| p == "ignored.rs"), "{ignored:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_only_paths_skip_vcs_ignore_files() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "target\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();
        let index = build(&dir).expect("index build");
        let paths: Vec<&str> = index
            .text_only_paths(None)
            .iter()
            .map(|path| path.as_str())
            .collect();
        assert!(!paths.contains(&".gitignore"), "{paths:?}");
        assert!(paths.contains(&"notes.txt"), "{paths:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_code_exts_caches_in_graph_set() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("script.ex"), "puts 'hello'\n").unwrap();
        let index = build(&dir).expect("index build");
        assert!(index.in_graph_paths.get().is_none());

        let counts1 = index.unindexed_code_exts(None);
        assert_eq!(counts1.get("ex"), Some(&1));
        assert!(index.in_graph_paths.get().is_some());

        let cached_ptr = index.in_graph_paths.get().unwrap() as *const _;
        let counts2 = index.unindexed_code_exts(None);
        assert_eq!(counts2.get("ex"), Some(&1));
        let cached_ptr2 = index.in_graph_paths.get().unwrap() as *const _;
        assert_eq!(cached_ptr, cached_ptr2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
