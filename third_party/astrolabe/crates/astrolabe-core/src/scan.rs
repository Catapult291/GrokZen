//! File discovery: walk the repo and decide what enters the index.
//!
//! Owns: gitignore semantics (root + nested, negation), a universal exclude
//! list (`.git`, `node_modules`, `target`, `build`, `dist`, `__pycache__`,
//! `.venv`), a max file size, binary detection, and content hashing for
//! incremental reindex.
//!
//! Use the `ignore` crate for the walk — it is ripgrep's, handles nested
//! gitignore correctly, and parallelizes. Do not hand-roll glob matching.
//!
//! Report skipped files by reason. The prior art printed only aggregate
//! counts, which hid a 20% parse-failure rate on a real repo.
//!
//! ## Gitignore audit vs. a single walk
//!
//! `ignore` never yields excluded entries, so listing every gitignore skip
//! requires a second walk (rules off, then on). [`ScanOptions::audit_ignored`]
//! is **off by default**: everyday indexing only needs the admitted set, and
//! the extra walk roughly doubles scan cost. Turn it on when diagnosing
//! "why wasn't this file indexed?".
//!
//! ## Build-declared excludes
//!
//! [`crate::types::ProjectMeta::excludes`] is not known until after `detect`,
//! which itself needs a [`FileIndex`] from this module. Do not fold those
//! directories into the first walk. Call [`apply_excludes`] on the
//! [`ScanResult`] (in-memory, no second disk walk) and then [`to_index`].
//!
//! ## Single-path admission
//!
//! [`path_admission`] replays the same gates as [`scan`] for one relative path
//! without walking the tree: the root→parent `.gitignore` chain (one
//! [`ignore::gitignore::Gitignore`] per directory, last matching rule wins,
//! including negation), the default excluded-directory names /
//! [`ScanOptions::extra_excludes`],
//! [`ScanOptions::max_file_bytes`], and the cached binary sniff. A racing
//! delete is [`PathAdmission::Missing`].
//!
//! ## Quick file count
//!
//! [`count_files`] is a parallel walk that stops once `limit` regular files
//! are seen. It applies the same directory exclusions as [`scan`] (default
//! dirs, gitignore, extra excludes) but not size or binary gates.
//!
//! ## Binary sniff cache
//!
//! Binary sniffing consults a process-wide map keyed by `(path, mtime, len)`.
//! A hit is valid only for that triple: a content rewrite that updates mtime
//! or length is a miss and re-sniffs. If `mtime` cannot be read, or is inside
//! the 2 s untrusted window (same rule as [`crate::watch`]), we do not cache.
//! Capacity is 100_000 entries; overflow drops the whole
//! map (cheaper than LRU bookkeeping on the parallel walk). The lock is a
//! `Mutex` so the walker threads share it.

use crate::types::{FileIndex, Language, RelPath};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::Gitignore;
use ignore::{DirEntry, Match, WalkBuilder, WalkState};
use memchr::memchr;
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, Metadata};
use std::io::{self, ErrorKind, Read, Take};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

const BINARY_SNIFF_BYTES: u64 = 8 * 1024;
/// Upper bound on cached binary-sniff verdicts. Overflow clears the map.
const BINARY_SNIFF_CACHE_CAP: usize = 100_000;
/// Same window as `watch::MTIME_UNTRUSTED_WINDOW`. Duplicated so this module
/// does not depend on `watch`. Verdicts for files inside this window are not
/// cached: two same-size writes can share an mtime, so `(mtime, len)` is not
/// a content identity.
const MTIME_UNTRUSTED_WINDOW: Duration = Duration::from_secs(2);
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".openvisio",
    ".serena",
    // Flutter / Dart toolchain artifacts. Fallback exclude for non-git trees.
    ".dart_tool",
    ".vs",
    // Own store (index.redb). Must not depend on gitignore: `ignore` skips
    // gitignore rules in non-git trees, so a store-written `.gitignore` of `*`
    // is not enough.
    ".astrolabe",
];

/// Build-output directory names. Unlike [`EXCLUDED_DIRS`] these are only
/// pruned at the repo root or directly inside a project root (a directory
/// holding a build manifest, see [`PROJECT_MANIFESTS`]). Elsewhere (e.g. Go's
/// `internal/build/`) they are ordinary source directories and only
/// `.gitignore` can exclude them. `bin` is intentionally not listed: Rust
/// crates keep sources in `src/bin`; C# / VB `bin/` is excluded per project
/// (`ProjectMeta::excludes`).
const BUILD_OUTPUT_DIRS: &[&str] = &["target", "build", "dist", "obj"];

/// File names (or `*.ext` suffixes) that mark a directory as a project root.
const PROJECT_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "go.mod",
    "pubspec.yaml",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "CMakeLists.txt",
    "Makefile",
    "meson.build",
    "tsconfig.json",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "build.sbt",
    "Package.swift",
    "*.csproj",
    "*.fsproj",
    "*.vbproj",
    "*.sln",
];

fn dir_is_project_root(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten().any(|ent| {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        PROJECT_MANIFESTS.iter().any(|m| match m.strip_prefix('*') {
            Some(suffix) => name.len() > suffix.len() && name.ends_with(suffix),
            None => name == *m,
        })
    })
}

/// Whether directory `dir` (absolute, under `root`) is pruned by the built-in
/// exclude rules: always-excluded tool/cache directory names anywhere, and
/// build-output names only at the root or directly inside a project root.
pub fn is_default_excluded_dir(root: &Path, dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if EXCLUDED_DIRS.contains(&name) {
        return true;
    }
    if !BUILD_OUTPUT_DIRS.contains(&name) {
        return false;
    }
    match dir.parent() {
        Some(parent) => parent == root || dir_is_project_root(parent),
        None => false,
    }
}

/// Why a discovered file did not enter the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Ignored,
    TooLarge,
    Binary,
    ReadError,
}

/// Outcome of replaying [`scan`] gates for a single relative path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathAdmission {
    /// Would appear in [`ScanResult::files`].
    Indexed,
    /// Gitignore, the default exclude list, or [`ScanOptions::extra_excludes`].
    Ignored,
    /// Larger than [`ScanOptions::max_file_bytes`].
    TooLarge,
    /// Failed the binary sniff (NUL / invalid UTF-8 density).
    Binary,
    /// Path does not exist, or raced away while we were looking.
    Missing,
}

/// Early-stop file count from [`count_files`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickCount {
    pub files: u64,
    pub truncated: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub files: Vec<RelPath>,
    pub skipped: Vec<(RelPath, SkipReason)>,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub max_file_bytes: u64,
    pub extra_excludes: Vec<String>,
    pub respect_gitignore: bool,
    /// Record gitignore exclusions in [`ScanResult::skipped`].
    ///
    /// Off by default. Daily indexing does not need the gitignore skip list,
    /// and producing it costs a second parallel walk (`ignore` does not
    /// callback excluded entries). Enable this when answering "why wasn't
    /// my file indexed?". Size, binary, read-error, and `extra_excludes`
    /// skips are always recorded, regardless of this flag.
    pub audit_ignored: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_file_bytes: 1_500_000,
            extra_excludes: Vec::new(),
            respect_gitignore: true,
            audit_ignored: false,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct BinarySniffKey {
    path: PathBuf,
    mtime: SystemTime,
    len: u64,
}

static BINARY_SNIFF_CACHE: LazyLock<Mutex<HashMap<BinarySniffKey, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn scan(root: &Path, opts: &ScanOptions) -> ScanResult {
    let root = root.to_path_buf();
    let excludes = Arc::new(ExcludeMatcher::new(&opts.extra_excludes));

    // `ignore` does not yield ignored entries. The audit trail therefore
    // needs a rules-off walk plus a rules-on walk; skip the extra walk
    // unless the caller asked for it.
    let audit_gitignore = opts.audit_ignored && opts.respect_gitignore;
    let candidates = collect_paths(
        &root,
        !audit_gitignore && opts.respect_gitignore,
        Arc::clone(&excludes),
    );
    let admitted = if audit_gitignore {
        Some(
            collect_paths(&root, true, Arc::clone(&excludes))
                .into_iter()
                .collect::<BTreeSet<_>>(),
        )
    } else {
        None
    };

    // Stat + binary sniff are independent per file and dominate on cold disks,
    // network mounts and Windows. `par_iter().collect()` on an indexed source
    // keeps candidate order, and the sorts below make the result independent
    // of scheduling regardless.
    let outcomes: Vec<ScanOutcome> = candidates
        .par_iter()
        .map(|path| classify_candidate(&root, path, opts, &excludes, admitted.as_ref()))
        .collect();
    let mut result = ScanResult::default();
    for outcome in outcomes {
        match outcome {
            ScanOutcome::Drop => {}
            ScanOutcome::File(rel) => result.files.push(rel),
            ScanOutcome::Skip(rel, reason) => result.skipped.push((rel, reason)),
        }
    }

    result.files.sort();
    result.skipped.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

enum ScanOutcome {
    Drop,
    File(RelPath),
    Skip(RelPath, SkipReason),
}

fn classify_candidate(
    root: &Path,
    path: &Path,
    opts: &ScanOptions,
    excludes: &ExcludeMatcher,
    admitted: Option<&BTreeSet<PathBuf>>,
) -> ScanOutcome {
    let rel = RelPath::new(path_to_rel(root, path));
    let gitignored = admitted.is_some_and(|admitted| !admitted.contains(path));
    if gitignored || excludes.matches_file(root, path) {
        return ScanOutcome::Skip(rel, SkipReason::Ignored);
    }
    let metadata = match path.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return ScanOutcome::Drop,
        Err(_) => return ScanOutcome::Skip(rel, SkipReason::ReadError),
    };
    if metadata.len() > opts.max_file_bytes {
        return ScanOutcome::Skip(rel, SkipReason::TooLarge);
    }
    match looks_binary_cached(path, &metadata, Language::from_path(&rel).is_some()) {
        Ok(true) => ScanOutcome::Skip(rel, SkipReason::Binary),
        Ok(false) => ScanOutcome::File(rel),
        Err(_) => ScanOutcome::Skip(rel, SkipReason::ReadError),
    }
}

/// Replay the scan gates for one path without walking the whole tree.
///
/// The root→path `.gitignore` chain is rebuilt with
/// [`ignore::gitignore::Gitignore`] (one file per directory, depth = number
/// of path components). Later files override earlier ones, so a nested
/// `!important` can un-ignore a parent `*`. Directory excludes, size, and
/// the cached binary sniff then apply in the same order as [`scan`]. A path
/// that is gone (or not a regular file we can stat) is [`PathAdmission::Missing`].
pub fn path_admission(root: &Path, rel: &RelPath, opts: &ScanOptions) -> PathAdmission {
    let path = rel_to_abs(root, rel);
    let metadata = match path.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return PathAdmission::Ignored,
        Err(err) if err.kind() == ErrorKind::NotFound => return PathAdmission::Missing,
        Err(_) => return PathAdmission::Missing,
    };

    let excludes = ExcludeMatcher::new(&opts.extra_excludes);
    if path_is_excluded(root, rel, &path, &excludes) {
        return PathAdmission::Ignored;
    }
    if opts.respect_gitignore && gitignore_ignores(root, rel) {
        return PathAdmission::Ignored;
    }
    if metadata.len() > opts.max_file_bytes {
        return PathAdmission::TooLarge;
    }
    match looks_binary_cached(&path, &metadata, Language::from_path(rel).is_some()) {
        Ok(true) => PathAdmission::Binary,
        Ok(false) => PathAdmission::Indexed,
        Err(err) if err.kind() == ErrorKind::NotFound => PathAdmission::Missing,
        Err(_) => PathAdmission::Missing,
    }
}

/// Count regular files with the same directory exclusions as [`scan`],
/// stopping once `limit` is reached.
///
/// Size and binary gates are skipped — this is a cheap upper bound for root
/// probing, not an admitted-set replica. Parallel walkers may overshoot
/// `limit` by a few files; [`QuickCount::truncated`] is set as soon as the
/// cap is hit, and [`QuickCount::files`] is then `>= limit`.
pub fn count_files(root: &Path, opts: &ScanOptions, limit: u64) -> QuickCount {
    let excludes = Arc::new(ExcludeMatcher::new(&opts.extra_excludes));
    let builder = configured_walker(root, opts.respect_gitignore, excludes);

    let count = Arc::new(AtomicU64::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let output = Arc::clone(&count);
    let stop = Arc::clone(&truncated);
    builder.build_parallel().run(|| {
        let output = Arc::clone(&output);
        let stop = Arc::clone(&stop);
        Box::new(move |entry| {
            if output.load(Ordering::Relaxed) >= limit {
                stop.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            if let Ok(entry) = entry {
                if entry.depth() > 0 && entry.file_type().is_some_and(|kind| kind.is_file()) {
                    let prev = output.fetch_add(1, Ordering::Relaxed);
                    if prev + 1 >= limit {
                        stop.store(true, Ordering::Relaxed);
                        return WalkState::Quit;
                    }
                }
            }
            WalkState::Continue
        })
    });

    let files = count.load(Ordering::Relaxed);
    QuickCount {
        files,
        truncated: truncated.load(Ordering::Relaxed) || files >= limit,
    }
}

pub fn to_index(root: &Path, res: &ScanResult) -> FileIndex {
    FileIndex::new(root, res.files.iter().cloned())
}

/// True if `rel` is exactly `dir`, or a file/dir inside it.
///
/// Matching is by path segment, not string prefix: `target` matches
/// `target/foo.rs` but not `targets/foo.rs`. Empty strings and `"."` never
/// match, so a resolver that has nothing to declare cannot wipe the index.
pub fn path_under_excludes(rel: &RelPath, excludes: &[String]) -> bool {
    let rel = rel.as_str();
    excludes
        .iter()
        .any(|raw| normalize_exclude_dir(raw).is_some_and(|dir| is_under_dir(rel, &dir)))
}

/// Drop admitted files that live under repo-relative exclude directories.
///
/// This is the scan-side consumer of [`crate::types::ProjectMeta::excludes`].
/// `scan` must run before `detect`, so those directories are not known at
/// walk time. The orchestrator should:
///
/// ```ignore
/// let scanned = scan(root, opts);
/// let files = to_index(root, &scanned);
/// let resolvers = ResolverSet::detect(&files);
/// let mut excludes = Vec::new();
/// for lang in [
///     Language::Python,
///     Language::Go,
///     Language::Java,
///     Language::Rust,
///     Language::TypeScript,
/// ] {
///     if let Some(meta) = resolvers.meta_for(lang) {
///         excludes.extend(meta.excludes.iter().cloned());
///     }
/// }
/// let scanned = apply_excludes(scanned, &excludes);
/// let files = to_index(root, &scanned);
/// ```
///
/// In-memory only — no second disk walk. Files already in `skipped` are left
/// as they are; newly excluded files are recorded as [`SkipReason::Ignored`].
/// Prefer this over stuffing `ProjectMeta::excludes` into
/// [`ScanOptions::extra_excludes`]: glob matching there is filename-aware
/// and would treat `out` as "any directory named out", which is broader than
/// the repo-relative directories `detect` emits.
pub fn apply_excludes(mut result: ScanResult, excludes: &[String]) -> ScanResult {
    let dirs: Vec<String> = excludes
        .iter()
        .filter_map(|raw| normalize_exclude_dir(raw))
        .collect();
    if dirs.is_empty() {
        result.files.sort();
        result.skipped.sort_by(|a, b| a.0.cmp(&b.0));
        return result;
    }

    let mut kept = Vec::with_capacity(result.files.len());
    for path in std::mem::take(&mut result.files) {
        if dirs.iter().any(|dir| is_under_dir(path.as_str(), dir)) {
            result.skipped.push((path, SkipReason::Ignored));
        } else {
            kept.push(path);
        }
    }
    result.files = kept;
    result.files.sort();
    result.skipped.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn normalize_exclude_dir(raw: &str) -> Option<String> {
    let rel = RelPath::new(raw);
    let dir = rel.as_str().trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        None
    } else {
        Some(dir.to_string())
    }
}

fn is_under_dir(rel: &str, dir: &str) -> bool {
    rel == dir
        || (rel.len() > dir.len() && rel.as_bytes()[dir.len()] == b'/' && rel.starts_with(dir))
}

#[derive(Debug)]
struct ExcludeMatcher {
    globs: GlobSet,
    literals: Vec<String>,
}

impl ExcludeMatcher {
    fn new(patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut literals = Vec::new();
        for pattern in patterns {
            match Glob::new(pattern) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => literals.push(pattern.replace('\\', "/")),
            }
        }
        ExcludeMatcher {
            globs: builder
                .build()
                .expect("validated glob patterns must build successfully"),
            literals,
        }
    }

    fn matches_file(&self, root: &Path, path: &Path) -> bool {
        self.matches(root, path)
    }

    fn matches_dir(&self, root: &Path, path: &Path) -> bool {
        is_default_excluded_dir(root, path) || self.matches(root, path)
    }

    fn matches(&self, root: &Path, path: &Path) -> bool {
        let rel = path_to_rel(root, path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        self.globs.is_match(&rel)
            || self.globs.is_match(name)
            || self
                .literals
                .iter()
                .any(|literal| literal == &rel || literal == name)
    }
}

fn collect_paths(
    root: &Path,
    respect_gitignore: bool,
    excludes: Arc<ExcludeMatcher>,
) -> Vec<PathBuf> {
    let builder = configured_walker(root, respect_gitignore, excludes);
    let paths = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&paths);
    builder.build_parallel().run(|| {
        let output = Arc::clone(&output);
        Box::new(move |entry| {
            if let Ok(entry) = entry {
                if entry.depth() > 0 && !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    output
                        .lock()
                        .expect("scan result mutex poisoned")
                        .push(entry.into_path());
                }
            }
            WalkState::Continue
        })
    });
    drop(output);

    let mut paths = Arc::try_unwrap(paths)
        .expect("all parallel walker references must be dropped")
        .into_inner()
        .expect("scan result mutex poisoned");
    paths.sort();
    paths
}

fn configured_walker(
    root: &Path,
    respect_gitignore: bool,
    excludes: Arc<ExcludeMatcher>,
) -> WalkBuilder {
    let root_for_filter = root.to_path_buf();
    let mut builder = WalkBuilder::new(root);
    builder.hidden(false).require_git(false);
    if !respect_gitignore {
        builder
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
    }
    builder.filter_entry(move |entry| !is_pruned_dir(entry, &root_for_filter, excludes.as_ref()));
    builder
}

fn is_pruned_dir(entry: &DirEntry, root: &Path, excludes: &ExcludeMatcher) -> bool {
    entry.depth() > 0
        && entry.file_type().is_some_and(|kind| kind.is_dir())
        && excludes.matches_dir(root, entry.path())
}

fn path_to_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn rel_to_abs(root: &Path, rel: &RelPath) -> PathBuf {
    let mut path = root.to_path_buf();
    for segment in rel.as_str().split('/') {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path
}

fn path_is_excluded(root: &Path, rel: &RelPath, abs: &Path, excludes: &ExcludeMatcher) -> bool {
    if excludes.matches_file(root, abs) {
        return true;
    }
    let mut dir = root.to_path_buf();
    let parts: Vec<&str> = rel
        .as_str()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if parts.len() < 2 {
        return false;
    }
    for segment in &parts[..parts.len() - 1] {
        dir.push(segment);
        if excludes.matches_dir(root, &dir) {
            return true;
        }
    }
    false
}

/// Last matching rule across the root→parent `.gitignore` chain wins.
///
/// Parent directories are tested as directories before we load a nested
/// `.gitignore` inside them, matching git's "do not look inside an ignored
/// directory" rule. A whitelist on the directory (`!src/`) lets us descend
/// and apply the child's rules, which is how `*` / `!important` nesting works.
fn gitignore_ignores(root: &Path, rel: &RelPath) -> bool {
    let mut dir = root.to_path_buf();
    let mut chain: Vec<Gitignore> = Vec::new();
    // Same sources as the `ignore` walker in `scan` (lowest precedence
    // first, last match wins): global ignore file, `.gitignore`/`.ignore`
    // in directories above the root, `.git/info/exclude`, then the root.
    let global = cached_global_gitignore();
    if !global.is_empty() {
        chain.push(global);
    }
    let ancestors: Vec<&Path> = root.ancestors().skip(1).collect();
    for ancestor in ancestors.into_iter().rev() {
        push_gitignore(&mut chain, ancestor);
    }
    let exclude = root.join(".git").join("info").join("exclude");
    if let Some(gi) = cached_matcher(&exclude, MatcherKind::InfoExclude(root)) {
        chain.push(gi);
    }
    push_gitignore(&mut chain, &dir);

    let parts: Vec<&str> = rel
        .as_str()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if parts.is_empty() {
        return false;
    }

    for (i, segment) in parts.iter().enumerate() {
        dir.push(segment);
        let is_last = i + 1 == parts.len();
        if chain_ignores(&chain, &dir, !is_last) {
            return true;
        }
        if !is_last {
            push_gitignore(&mut chain, &dir);
        }
    }
    false
}

fn push_gitignore(chain: &mut Vec<Gitignore>, dir: &Path) {
    // `.ignore` outranks `.gitignore` in the same directory.
    for name in [".gitignore", ".ignore"] {
        if let Some(gi) = cached_matcher(&dir.join(name), MatcherKind::Plain) {
            chain.push(gi);
        }
    }
}

enum MatcherKind<'a> {
    /// `.gitignore` / `.ignore`: patterns are relative to the file's directory.
    Plain,
    /// `.git/info/exclude`: patterns are relative to the repo root.
    InfoExclude(&'a Path),
}

type MatcherStamp = Option<(SystemTime, u64)>;

/// Parsed ignore files keyed by path. An entry is reused only while the
/// file's (mtime, len) is unchanged and the mtime is older than
/// [`MTIME_UNTRUSTED_WINDOW`], so a rewrite inside one mtime tick is re-read.
/// `None` stamp = the file did not exist.
type MatcherEntry = (MatcherStamp, Option<Gitignore>);
static MATCHER_CACHE: LazyLock<Mutex<HashMap<PathBuf, MatcherEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MATCHER_CACHE_CAP: usize = 4096;

fn matcher_stamp(path: &Path) -> MatcherStamp {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    Some((meta.modified().ok()?, meta.len()))
}

fn stamp_trusted(stamp: &MatcherStamp) -> bool {
    match stamp {
        None => true,
        Some((mtime, _)) => SystemTime::now()
            .duration_since(*mtime)
            .is_ok_and(|age| age >= MTIME_UNTRUSTED_WINDOW),
    }
}

fn cached_matcher(path: &Path, kind: MatcherKind<'_>) -> Option<Gitignore> {
    let stamp = matcher_stamp(path);
    // A file that cannot report an mtime is never cached.
    let cacheable = stamp.is_none() && !path.exists() || (stamp.is_some() && stamp_trusted(&stamp));
    if cacheable {
        let cache = MATCHER_CACHE.lock().expect("matcher cache poisoned");
        if let Some((cached_stamp, gi)) = cache.get(path) {
            if *cached_stamp == stamp {
                return gi.clone();
            }
        }
    }
    let built = stamp.as_ref().and_then(|_| match kind {
        MatcherKind::Plain => Some(Gitignore::new(path).0),
        MatcherKind::InfoExclude(root) => {
            let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
            let _ = builder.add(path);
            builder.build().ok()
        }
    });
    if cacheable {
        let mut cache = MATCHER_CACHE.lock().expect("matcher cache poisoned");
        if cache.len() >= MATCHER_CACHE_CAP {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), (stamp, built.clone()));
    }
    built
}

/// Global gitignore (core.excludesFile / XDG), re-read at most every 5 s.
fn cached_global_gitignore() -> Gitignore {
    static CACHE: LazyLock<Mutex<Option<(std::time::Instant, Gitignore)>>> =
        LazyLock::new(|| Mutex::new(None));
    let mut g = CACHE.lock().expect("global gitignore cache poisoned");
    if let Some((at, gi)) = g.as_ref() {
        if at.elapsed() < Duration::from_secs(5) {
            return gi.clone();
        }
    }
    let (gi, _) = Gitignore::global();
    *g = Some((std::time::Instant::now(), gi.clone()));
    gi
}

fn chain_ignores(chain: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;
    for gi in chain {
        match gi.matched(path, is_dir) {
            Match::None => {}
            Match::Ignore(_) => ignored = true,
            Match::Whitelist(_) => ignored = false,
        }
    }
    ignored
}

fn looks_binary_cached(path: &Path, metadata: &Metadata, known_source: bool) -> io::Result<bool> {
    let key = binary_sniff_key(path, metadata);
    if let Some(ref key) = key {
        let cache = BINARY_SNIFF_CACHE
            .lock()
            .expect("binary sniff cache mutex poisoned");
        if let Some(&verdict) = cache.get(key) {
            return Ok(verdict);
        }
    }

    let verdict = sniff_binary(path, known_source)?;

    if let Some(key) = key {
        let mut cache = BINARY_SNIFF_CACHE
            .lock()
            .expect("binary sniff cache mutex poisoned");
        if cache.len() >= BINARY_SNIFF_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, verdict);
    }
    Ok(verdict)
}

fn binary_sniff_key(path: &Path, metadata: &Metadata) -> Option<BinarySniffKey> {
    let mtime = metadata.modified().ok()?;
    let now = SystemTime::now();
    match now.duration_since(mtime) {
        Ok(age) if age < MTIME_UNTRUSTED_WINDOW => return None,
        Err(_) => return None,
        Ok(_) => {}
    }
    Some(BinarySniffKey {
        path: path.to_path_buf(),
        mtime,
        len: metadata.len(),
    })
}

/// A stray NUL inside a file with a known source extension should not cost us
/// the whole file. Measured on a real repository: `mcp/src/adapter.ts` carries
/// two NUL bytes in 8 KB of otherwise valid TypeScript, and treating it as
/// binary silently dropped its symbols and left three imports unresolvable
/// elsewhere. For source files we therefore require either invalid UTF-8 or a
/// meaningful density of NULs; everything else keeps the strict rule, which is
/// what actually protects us from images and object files.
fn sniff_binary(path: &Path, known_source: bool) -> io::Result<bool> {
    let file = File::open(path)?;
    let mut reader: Take<File> = file.take(BINARY_SNIFF_BYTES);
    let mut header = Vec::with_capacity(BINARY_SNIFF_BYTES as usize);
    reader.read_to_end(&mut header)?;

    // UTF-16 with a BOM (Windows .rc/.resx/.sql/.csproj) is full of NULs but
    // is text; the readers transcode it.
    let utf32_le = header.starts_with(&[0xFF, 0xFE, 0x00, 0x00]);
    if !utf32_le && (header.starts_with(&[0xFF, 0xFE]) || header.starts_with(&[0xFE, 0xFF])) {
        return Ok(false);
    }
    if !known_source {
        return Ok(memchr(0, &header).is_some());
    }
    if header.is_empty() {
        return Ok(false);
    }

    let mut nuls = 0usize;
    let mut rest = header.as_slice();
    while let Some(i) = memchr(0, rest) {
        nuls += 1;
        rest = &rest[i + 1..];
    }
    if nuls == 0 {
        return Ok(false);
    }
    // Truncating the sniff window can split a multi-byte character, so a
    // trailing invalid sequence is not evidence of a binary file.
    let text_like = match std::str::from_utf8(&header) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    Ok(!text_like || nuls * 1000 > header.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn gitignore_matcher_cache_reuses_and_invalidates() {
        let dir = std::env::temp_dir().join(format!("astro-gi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gi = dir.join(".gitignore");
        std::fs::write(&gi, "a.rs\n").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&gi)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let rel = |s: &str| RelPath::new(s);
        assert!(gitignore_ignores(&dir, &rel("a.rs")));
        assert!(!gitignore_ignores(&dir, &rel("b.rs")));
        // Cached now; rewrite with different length -> must be re-read.
        std::fs::write(&gi, "b.rs\n# longer\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&gi)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(!gitignore_ignores(&dir, &rel("a.rs")));
        assert!(gitignore_ignores(&dir, &rel("b.rs")));
        // Deleted -> nothing ignored.
        std::fs::remove_file(&gi).unwrap();
        assert!(!gitignore_ignores(&dir, &rel("b.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_output_dirs_only_pruned_at_project_roots() {
        let dir = TestDir::new();
        dir.write("go.mod", "module x\n");
        dir.write("build/out.go", "package out");
        dir.write("internal/build/build.go", "package build");
        dir.write("cmd/target/t.go", "package target");
        dir.write("web/package.json", "{}");
        dir.write("web/dist/bundle.js", "x");
        dir.write("web/src/dist/keep.js", "x");
        dir.write("app/App.csproj", "<Project/>");
        dir.write("app/obj/gen.cs", "class G {}");
        dir.write("crate/Cargo.toml", "[package]");
        dir.write("crate/src/bin/tool.rs", "fn main() {}");
        dir.write("crate/target/debug/x.rs", "fn x() {}");

        let result = scan(dir.path(), &ScanOptions::default());
        let files: Vec<&str> = result.files.iter().map(RelPath::as_str).collect();
        for keep in [
            "internal/build/build.go",
            "cmd/target/t.go",
            "web/src/dist/keep.js",
            "crate/src/bin/tool.rs",
        ] {
            assert!(files.contains(&keep), "{keep} must be indexed: {files:?}");
        }
        for drop in [
            "build/out.go",
            "web/dist/bundle.js",
            "app/obj/gen.cs",
            "crate/target/debug/x.rs",
        ] {
            assert!(!files.contains(&drop), "{drop} must be pruned: {files:?}");
        }
        let opts = ScanOptions::default();
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("internal/build/build.go"), &opts),
            PathAdmission::Indexed
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("web/dist/bundle.js"), &opts),
            PathAdmission::Ignored
        );
    }

    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "astrolabe-scan-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            TestDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, contents).expect("write test file");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn has_skip(result: &ScanResult, path: &str, reason: SkipReason) -> bool {
        result.skipped.contains(&(RelPath::new(path), reason))
    }

    fn list_all_regular_files(root: &Path) -> Vec<RelPath> {
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(false)
            .require_git(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
        let mut files = Vec::new();
        for entry in builder.build().flatten() {
            if entry.depth() > 0 && entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(RelPath::new(path_to_rel(root, entry.path())));
            }
        }
        files
    }

    fn admission_fixture() -> TestDir {
        let dir = TestDir::new();
        dir.write(".gitignore", "*.txt\n!keep.txt\nsecret/\n");
        dir.write("drop.txt", "ignored");
        dir.write("keep.txt", "kept");
        dir.write("ok.rs", "fn ok() {}");
        dir.write("skip_me.rs", "fn skip() {}");
        dir.write("src/.gitignore", "*.tmp\n!keep.tmp\n");
        dir.write("src/drop.tmp", "ignored");
        dir.write("src/keep.tmp", "kept");
        dir.write("src/main.rs", "fn main() {}");
        dir.write("nested/.gitignore", "*\n!important\n");
        dir.write("nested/foo.rs", "fn foo() {}");
        dir.write("nested/important", "keep me");
        dir.write("secret/hidden.rs", "fn hidden() {}");
        dir.write("target/generated.rs", "generated");
        dir.write("node_modules/pkg/index.js", "generated");
        dir.write(".astrolabe/index.redb", "not-a-real-index");
        dir.write("large.bin", "x".repeat(64));
        dir.write("binary.dat", b"text\0more");
        dir
    }

    #[test]
    fn scan_honors_gitignore_nested_rules_and_negation() {
        let dir = TestDir::new();
        dir.write(".gitignore", "*.txt\n!keep.txt\n");
        dir.write("drop.txt", "ignored");
        dir.write("keep.txt", "kept");
        dir.write("src/.gitignore", "*.tmp\n!keep.tmp\n");
        dir.write("src/drop.tmp", "ignored");
        dir.write("src/keep.tmp", "kept");
        dir.write("src/main.rs", "fn main() {}");

        let result = scan(
            dir.path(),
            &ScanOptions {
                audit_ignored: true,
                ..ScanOptions::default()
            },
        );
        assert!(result.files.contains(&RelPath::new("keep.txt")));
        assert!(result.files.contains(&RelPath::new("src/keep.tmp")));
        assert!(result.files.contains(&RelPath::new("src/main.rs")));
        assert!(has_skip(&result, "drop.txt", SkipReason::Ignored));
        assert!(has_skip(&result, "src/drop.tmp", SkipReason::Ignored));
    }

    #[test]
    fn scan_prunes_default_excluded_directories() {
        // see also build_output_dirs_only_pruned_at_project_roots

        let dir = TestDir::new();
        dir.write("src/main.rs", "fn main() {}");
        dir.write("target/generated.rs", "generated");
        dir.write("node_modules/pkg/index.js", "generated");
        dir.write(".serena/cache.txt", "cache");

        let result = scan(dir.path(), &ScanOptions::default());
        assert_eq!(result.files, vec![RelPath::new("src/main.rs")]);
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn scan_prunes_astrolabe_index_dir_with_and_without_gitignore() {
        let dir = TestDir::new();
        dir.write("src/main.rs", "fn main() {}");
        dir.write(".astrolabe/index.redb", "not-a-real-index");

        for respect_gitignore in [true, false] {
            let result = scan(
                dir.path(),
                &ScanOptions {
                    respect_gitignore,
                    ..ScanOptions::default()
                },
            );
            assert_eq!(
                result.files,
                vec![RelPath::new("src/main.rs")],
                "respect_gitignore={respect_gitignore}"
            );
            assert!(
                result
                    .files
                    .iter()
                    .all(|p| !p.as_str().contains(".astrolabe")),
                "respect_gitignore={respect_gitignore}: {:?}",
                result.files
            );
            assert!(
                result
                    .skipped
                    .iter()
                    .all(|(p, _)| !p.as_str().contains(".astrolabe")),
                "respect_gitignore={respect_gitignore}: {:?}",
                result.skipped
            );
        }
    }

    #[test]
    fn scan_marks_large_files() {
        let dir = TestDir::new();
        dir.write("large.txt", b"12345");
        let opts = ScanOptions {
            max_file_bytes: 4,
            ..ScanOptions::default()
        };

        let result = scan(dir.path(), &opts);
        assert!(has_skip(&result, "large.txt", SkipReason::TooLarge));
    }

    #[test]
    fn scan_marks_binary_files() {
        let dir = TestDir::new();
        dir.write("binary.dat", b"text\0more");

        let result = scan(dir.path(), &ScanOptions::default());
        assert!(has_skip(&result, "binary.dat", SkipReason::Binary));
    }

    #[test]
    fn source_files_survive_a_stray_nul_but_not_a_dense_one() {
        let dir = TestDir::new();
        // Mirrors a real case: two NULs inside 8 KB of valid TypeScript.
        let mut sparse = b"export function f() { return 1 }\n".repeat(80);
        sparse[10] = 0;
        sparse[20] = 0;
        dir.write("sparse.ts", &sparse);
        // Dense NULs are still binary, whatever the extension claims.
        dir.write("dense.ts", b"\0\0\0\0".repeat(64));
        // Unknown extensions keep the strict rule.
        dir.write("blob.dat", b"text\0more");

        let result = scan(dir.path(), &ScanOptions::default());
        assert!(
            result.files.iter().any(|p| p.as_str() == "sparse.ts"),
            "a source file must not be lost to two stray bytes"
        );
        assert!(has_skip(&result, "dense.ts", SkipReason::Binary));
        assert!(has_skip(&result, "blob.dat", SkipReason::Binary));
    }

    #[test]
    fn scan_is_deterministic() {
        let dir = TestDir::new();
        for path in ["z.rs", "a.rs", "nested/y.rs", "nested/b.rs"] {
            dir.write(path, path);
        }

        let first = scan(dir.path(), &ScanOptions::default());
        let second = scan(dir.path(), &ScanOptions::default());
        assert_eq!(first.files, second.files);
        assert_eq!(first.skipped, second.skipped);
        assert!(first.files.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn audit_ignored_false_omits_gitignore_skips_but_keeps_file_set() {
        let dir = TestDir::new();
        dir.write(".gitignore", "*.txt\n!keep.txt\n");
        dir.write("drop.txt", "ignored");
        dir.write("keep.txt", "kept");
        dir.write("src/main.rs", "fn main() {}");
        dir.write("huge.rs", "x".repeat(64));
        dir.write("binary.dat", b"x\0y");

        let audit = ScanOptions {
            audit_ignored: true,
            max_file_bytes: 32,
            ..ScanOptions::default()
        };
        let fast = ScanOptions {
            audit_ignored: false,
            max_file_bytes: 32,
            ..ScanOptions::default()
        };
        assert!(
            !ScanOptions::default().audit_ignored,
            "daily indexing must not pay for a second walk"
        );

        let audited = scan(dir.path(), &audit);
        let fast = scan(dir.path(), &fast);

        assert_eq!(audited.files, fast.files);
        assert!(audited.files.contains(&RelPath::new("keep.txt")));
        assert!(audited.files.contains(&RelPath::new("src/main.rs")));
        assert!(!audited.files.contains(&RelPath::new("drop.txt")));
        assert!(!fast.files.contains(&RelPath::new("drop.txt")));

        assert!(has_skip(&audited, "drop.txt", SkipReason::Ignored));
        assert!(!fast.skipped.iter().any(|(p, _)| p.as_str() == "drop.txt"));
        assert!(!fast.skipped.iter().any(|(_, r)| *r == SkipReason::Ignored));

        assert!(has_skip(&fast, "huge.rs", SkipReason::TooLarge));
        assert!(has_skip(&audited, "huge.rs", SkipReason::TooLarge));
        assert!(has_skip(&fast, "binary.dat", SkipReason::Binary));
        assert!(has_skip(&audited, "binary.dat", SkipReason::Binary));
    }

    #[test]
    fn apply_excludes_filters_dirs_without_prefix_false_positive() {
        // `target/` is pruned during the walk, so the prefix rule is tested
        // on a synthetic result that still contains both `target` and `targets`.
        let synthetic = ScanResult {
            files: vec![
                RelPath::new("src/main.rs"),
                RelPath::new("target/generated.rs"),
                RelPath::new("targets/keep.rs"),
                RelPath::new("pkg/out/A.java"),
                RelPath::new("pkg/outgoing/B.java"),
            ],
            skipped: vec![(RelPath::new("skip.dat"), SkipReason::Binary)],
        };
        let filtered = apply_excludes(
            synthetic,
            &["target".to_string(), "pkg/out".to_string(), "".to_string()],
        );

        assert_eq!(
            filtered.files,
            vec![
                RelPath::new("pkg/outgoing/B.java"),
                RelPath::new("src/main.rs"),
                RelPath::new("targets/keep.rs"),
            ]
        );
        assert!(has_skip(
            &filtered,
            "target/generated.rs",
            SkipReason::Ignored
        ));
        assert!(has_skip(&filtered, "pkg/out/A.java", SkipReason::Ignored));
        assert!(!filtered
            .skipped
            .iter()
            .any(|(p, _)| p.as_str() == "targets/keep.rs"));
        assert!(!filtered
            .skipped
            .iter()
            .any(|(p, _)| p.as_str() == "pkg/outgoing/B.java"));
        assert!(has_skip(&filtered, "skip.dat", SkipReason::Binary));
        assert!(!path_under_excludes(
            &RelPath::new("targets/keep.rs"),
            &["target".to_string()]
        ));
        assert!(path_under_excludes(
            &RelPath::new("target/generated.rs"),
            &["target".to_string()]
        ));

        let dir = TestDir::new();
        dir.write("src/main.rs", "fn main() {}");
        dir.write("targets/keep.rs", "keep");
        dir.write("pkg/out/A.java", "class A {}");
        dir.write("pkg/outgoing/B.java", "class B {}");
        dir.write("target/generated.rs", "generated");

        let scanned = scan(dir.path(), &ScanOptions::default());
        assert!(scanned.files.contains(&RelPath::new("targets/keep.rs")));
        assert!(!scanned
            .files
            .iter()
            .any(|p| p.as_str() == "target/generated.rs"));

        let filtered = apply_excludes(scanned, &["target".to_string(), "pkg/out".to_string()]);
        assert!(filtered.files.contains(&RelPath::new("src/main.rs")));
        assert!(filtered.files.contains(&RelPath::new("targets/keep.rs")));
        assert!(filtered
            .files
            .contains(&RelPath::new("pkg/outgoing/B.java")));
        assert!(!filtered.files.contains(&RelPath::new("pkg/out/A.java")));
        assert!(has_skip(&filtered, "pkg/out/A.java", SkipReason::Ignored));
    }

    #[test]
    fn apply_excludes_and_fast_scan_are_deterministic() {
        let dir = TestDir::new();
        for path in ["z.rs", "a.rs", "out/y.rs", "out/b.rs", "targets/c.rs"] {
            dir.write(path, path);
        }

        let opts = ScanOptions::default();
        assert!(!opts.audit_ignored);
        let first = scan(dir.path(), &opts);
        let second = scan(dir.path(), &opts);
        assert_eq!(first.files, second.files);
        assert_eq!(first.skipped, second.skipped);
        assert!(first.files.windows(2).all(|pair| pair[0] <= pair[1]));

        let excludes = vec!["out".to_string()];
        let first = apply_excludes(first, &excludes);
        let second = apply_excludes(second, &excludes);
        assert_eq!(first.files, second.files);
        assert_eq!(first.skipped, second.skipped);
        assert!(first.files.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(first.skipped.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(
            first.files,
            vec![
                RelPath::new("a.rs"),
                RelPath::new("targets/c.rs"),
                RelPath::new("z.rs"),
            ]
        );
        assert!(has_skip(&first, "out/b.rs", SkipReason::Ignored));
        assert!(has_skip(&first, "out/y.rs", SkipReason::Ignored));
    }

    #[test]
    fn path_admission_agrees_with_scan_on_fixture_tree() {
        let opts = ScanOptions {
            extra_excludes: vec!["skip_me.rs".to_string()],
            max_file_bytes: 32,
            ..ScanOptions::default()
        };
        let dir = admission_fixture();
        let scanned = scan(dir.path(), &opts);
        let admitted: BTreeSet<_> = scanned.files.iter().cloned().collect();

        for rel in list_all_regular_files(dir.path()) {
            let verdict = path_admission(dir.path(), &rel, &opts);
            assert_eq!(
                verdict == PathAdmission::Indexed,
                admitted.contains(&rel),
                "{}: admission={verdict:?} scan_files={:?}",
                rel.as_str(),
                scanned.files
            );
        }

        assert_eq!(
            path_admission(dir.path(), &RelPath::new("keep.txt"), &opts),
            PathAdmission::Indexed
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("drop.txt"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("src/keep.tmp"), &opts),
            PathAdmission::Indexed
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("src/drop.tmp"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("nested/important"), &opts),
            PathAdmission::Indexed
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("nested/foo.rs"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("secret/hidden.rs"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("target/generated.rs"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("skip_me.rs"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("large.bin"), &opts),
            PathAdmission::TooLarge
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("binary.dat"), &opts),
            PathAdmission::Binary
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("no/such/file.rs"), &opts),
            PathAdmission::Missing
        );
    }

    #[test]
    fn path_admission_honors_dot_ignore_and_git_info_exclude_like_scan() {
        let dir = TestDir::new();
        dir.write(".ignore", "dot_ignored.rs\n");
        dir.write(".git/info/exclude", "excluded.rs\n");
        dir.write("dot_ignored.rs", "fn a() {}");
        dir.write("excluded.rs", "fn b() {}");
        dir.write("kept.rs", "fn c() {}");
        let opts = ScanOptions::default();
        let scanned = scan(dir.path(), &opts);
        for name in ["dot_ignored.rs", "excluded.rs", "kept.rs"] {
            let rel = RelPath::new(name);
            let admitted = path_admission(dir.path(), &rel, &opts) == PathAdmission::Indexed;
            assert_eq!(
                admitted,
                scanned.files.contains(&rel),
                "{name}: path_admission and scan must agree"
            );
        }
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("kept.rs"), &opts),
            PathAdmission::Indexed
        );
    }

    #[test]
    fn path_admission_replays_nested_gitignore_negation() {
        let dir = TestDir::new();
        dir.write(".gitignore", "*\n!src/\n");
        dir.write("src/.gitignore", "*\n!important\n");
        dir.write("src/important", "keep");
        dir.write("src/other.rs", "fn other() {}");
        dir.write("root.rs", "fn root() {}");

        let opts = ScanOptions::default();
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("src/important"), &opts),
            PathAdmission::Indexed
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("src/other.rs"), &opts),
            PathAdmission::Ignored
        );
        assert_eq!(
            path_admission(dir.path(), &RelPath::new("root.rs"), &opts),
            PathAdmission::Ignored
        );

        let scanned = scan(dir.path(), &opts);
        assert!(scanned.files.contains(&RelPath::new("src/important")));
        assert!(!scanned.files.iter().any(|p| p.as_str() == "src/other.rs"));
        assert!(!scanned.files.iter().any(|p| p.as_str() == "root.rs"));
    }

    #[test]
    fn count_files_truncates_at_limit() {
        let dir = TestDir::new();
        for i in 0..20 {
            dir.write(&format!("f{i}.rs"), "fn f() {}");
        }
        dir.write("target/hidden.rs", "fn hidden() {}");
        dir.write(".gitignore", "ignored.rs\n");
        dir.write("ignored.rs", "fn ignored() {}");

        let opts = ScanOptions::default();
        let limited = count_files(dir.path(), &opts, 5);
        assert!(limited.truncated, "limit below the real count must stop");
        assert!(
            limited.files >= 5,
            "truncated count must be at least the limit, got {}",
            limited.files
        );

        let full = count_files(dir.path(), &opts, 1_000);
        assert!(!full.truncated);
        // 20 admitted sources + `.gitignore` itself; excluded dir and gitignored file stay out.
        assert_eq!(full.files, 21);

        let empty_limit = count_files(dir.path(), &opts, 0);
        assert!(empty_limit.truncated);
        assert_eq!(empty_limit.files, 0);
    }

    #[test]
    fn binary_sniff_cache_rechecks_after_mtime_change() {
        let dir = TestDir::new();
        dir.write("blob.dat", b"text\0more");
        let path = dir.path().join("blob.dat");
        let aged = SystemTime::now() - Duration::from_secs(10);
        File::open(&path)
            .expect("open blob")
            .set_modified(aged)
            .expect("age mtime past the untrusted window");

        let rel = RelPath::new("blob.dat");
        let opts = ScanOptions::default();
        assert_eq!(
            path_admission(dir.path(), &rel, &opts),
            PathAdmission::Binary
        );
        assert_eq!(
            path_admission(dir.path(), &rel, &opts),
            PathAdmission::Binary,
            "same (path, mtime, len) must hit the cache"
        );

        fs::write(&path, b"just text").expect("overwrite blob with text");
        File::open(&path)
            .expect("reopen blob")
            .set_modified(aged + Duration::from_secs(5))
            .expect("bump mtime");

        assert_eq!(
            path_admission(dir.path(), &rel, &opts),
            PathAdmission::Indexed,
            "a new (mtime, len) must miss the cache and re-sniff"
        );
    }

    #[test]
    #[ignore]
    fn bench_audit_ignored_modes_on_corpus() {
        use std::time::{Duration, Instant};

        fn time_scan(
            root: &Path,
            opts: &ScanOptions,
            warmup: usize,
            iters: usize,
        ) -> (Duration, Duration, Duration, usize, usize) {
            for _ in 0..warmup {
                let _ = scan(root, opts);
            }
            let mut times = Vec::with_capacity(iters);
            let mut files = 0usize;
            let mut skipped = 0usize;
            for _ in 0..iters {
                let t = Instant::now();
                let r = scan(root, opts);
                times.push(t.elapsed());
                files = r.files.len();
                skipped = r.skipped.len();
            }
            times.sort();
            (
                times[0],
                times[times.len() / 2],
                times[times.len() - 1],
                files,
                skipped,
            )
        }

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let corpora = [
            ("corpus/java", manifest.join("../../corpus/java")),
            ("../serena", manifest.join("../../../serena")),
        ];

        let fast = ScanOptions::default();
        let audit = ScanOptions {
            audit_ignored: true,
            ..ScanOptions::default()
        };

        for (label, root) in corpora {
            assert!(root.is_dir(), "missing corpus {label}: {}", root.display());
            let (fast_min, fast_t, fast_max, fast_files, fast_skipped) =
                time_scan(&root, &fast, 2, 15);
            let (audit_min, audit_t, audit_max, audit_files, audit_skipped) =
                time_scan(&root, &audit, 2, 15);
            println!(
                "{label}: fast(audit_ignored=false) min/median/max {:?}/{:?}/{:?} files={fast_files} skipped={fast_skipped}; \
                 audit(audit_ignored=true) min/median/max {:?}/{:?}/{:?} files={audit_files} skipped={audit_skipped}; \
                 median ratio {:.2}x",
                fast_min,
                fast_t,
                fast_max,
                audit_min,
                audit_t,
                audit_max,
                audit_t.as_secs_f64() / fast_t.as_secs_f64().max(1e-9),
            );
        }
    }
}
