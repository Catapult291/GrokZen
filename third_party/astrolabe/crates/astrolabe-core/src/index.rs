//! Index orchestration: the one path that turns a directory into a graph.
//!
//! Wires together scan -> parse -> resolve -> graph. Every stage reports what
//! it could not do into [`IndexReport`] rather than dropping it, so a partial
//! index never looks complete.
//!
//! When [`IndexOptions::persist`] is on (the default), parse results are stored
//! under `<repo>/.astrolabe/` and reused on the next run if the content hash
//! still matches. A missing, read-only, or corrupt store is logged and the
//! index continues in memory — the cache is an optimisation, not a dependency.
//!
//! ## Incremental reindex
//!
//! A full [`index_repo`] always walks, reads, hashes, and (on cache miss)
//! tree-sitter-parses every admitted file. On a 94k-file tree that is
//! minutes of CPU even when almost nothing changed. [`IncrementalIndex`]
//! keeps the last parse products in a byte-capped LRU and applies a watcher
//! [`ChangeSet`]: only `added`/`modified` source files are read and parsed
//! (plus LRU evictions, which fall back to a re-parse of that one file).
//! The graph is then reassembled in memory.
//!
//! Route (b) — retain per-file products and reassemble — rather than true
//! graph-delta (route a): import resolution, FileId assignment, and
//! name-matched call edges are coupled across the whole file set, so
//! rewriting only the dirty files' edges is more complex than the sequential
//! assemble step, which on ~10^5 files is typically sub-second. Resolver
//! `detect` may still read a handful of build-config files (`go.mod`,
//! `tsconfig.json`, …); those are not the 94k-file read+hash+parse path.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use crate::graph::CodeGraph;
use crate::parse::{ParsedFile, ParserPool};
use crate::resolvers::{resolver_owner, ResolverSet};
use crate::scan::{self, ScanOptions, ScanResult};
use crate::store::{self, Store};
use crate::types::{
    CodeEdge, CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, FileIndex, ImportResolution,
    Language, RelPath, SymbolId,
};
use crate::watch::ChangeSet;
use crate::IndexReport;

/// Maximum number of syntactic call targets allowed for a single callee name.
/// Common identifiers exceeding this threshold (e.g. `new`, `get`, `default`, `clone`)
/// are skipped to prevent combinatorial edge explosion and OOM.
pub const MAX_SYNTACTIC_CALL_TARGETS: usize = 50;

/// Default in-memory budget for retained parse products (encoded `ParsedFile`
/// bytes plus a small per-entry overhead). Oversized entries are still used
/// for the current assemble pass, but are not kept for the next one.
pub const DEFAULT_PARSE_CACHE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub scan: ScanOptions,
    /// Emit name-matched call edges. They carry `Confidence::Syntactic` and
    /// recall poorly (measured: 66% on TypeScript, 18% on Python), so callers
    /// that only need file-level impact should leave this off.
    pub call_edges: bool,
    /// Persist parse results under `<repo>/.astrolabe/` and reuse them on
    /// the next index when the content hash still matches.
    ///
    /// On by default so the MCP server — which constructs
    /// [`IndexOptions::default`] and is not part of this change — actually
    /// skips unchanged files after a restart. Persistence is an optimisation:
    /// tests and one-shot runs can set this to `false`.
    pub persist: bool,
    /// With `persist == false`: still *read* an existing store (never create,
    /// write, prune or heal it). For a second process on the same repository,
    /// which must not contend for the writer role but should not pay a cold
    /// reparse either. Ignored when `persist` is on.
    pub persist_read_only: bool,
    /// Byte budget for [`IncrementalIndex`]'s in-memory parse-product LRU.
    /// Unused by [`index_repo`]. Default [`DEFAULT_PARSE_CACHE_BYTES`].
    pub parse_cache_bytes: u64,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            call_edges: false,
            persist: true,
            persist_read_only: false,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
        }
    }
}

/// On-disk parse cache for `root`. Gitignored via the workspace `.gitignore`.
pub fn store_path(root: &Path) -> PathBuf {
    root.join(".astrolabe").join("index.redb")
}

fn open_store(root: &Path, opts: &IndexOptions) -> Option<Store> {
    if !opts.persist {
        if !opts.persist_read_only {
            return None;
        }
        let path = store_path(root);
        return match Store::open_read_only_with_retry(
            &path,
            Store::DEFAULT_RETRY_ATTEMPTS,
            Store::DEFAULT_RETRY_DELAY,
        ) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "read-only parse cache unavailable; parsing from source"
                );
                None
            }
        };
    }
    let dir = root.join(".astrolabe");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            error = %error,
            path = %dir.display(),
            "index persist disabled: cannot create store directory"
        );
        return None;
    }
    // The cache lives inside the repository being indexed, so it must not show
    // up in that repository's `git status`. A self-ignoring `.gitignore` makes
    // the whole directory invisible to git without asking the user to edit
    // their own ignore rules — the same trick pytest and ruff use for their
    // caches. Best effort: failing to write it costs cleanliness, not
    // correctness.
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# Created by Astrolabe. Ignores itself.\n*\n");
    }
    let path = store_path(root);
    // Sibling processes cold-starting the same repository race for this
    // single-writer lock; retry before giving up persistence.
    match Store::open_or_heal_with_retry(
        &path,
        Store::DEFAULT_RETRY_ATTEMPTS,
        Store::DEFAULT_RETRY_DELAY,
    ) {
        Ok(store) => Some(store),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "index persist disabled: store unavailable"
            );
            None
        }
    }
}

/// Content hash and line count, computed once so the store can skip unchanged
/// files on reindex.
fn digest(source: &str) -> (String, u32) {
    // FNV-1a: not cryptographic, but stable across runs and platforms, which
    // is all a change-detection key needs.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in source.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let loc = source.lines().count() as u32;
    (format!("{h:016x}"), loc)
}

struct ParsedRow {
    path: RelPath,
    language: Language,
    sha: String,
    loc: u32,
    /// Shared with the in-memory product cache: an incremental apply reuses
    /// an unchanged file's parse as-is (no decode, no copy of its imports
    /// and calls); only its symbols are cloned into the new graph.
    parsed: Arc<ParsedFile>,
}

#[derive(Clone)]
struct CachedProduct {
    language: Language,
    sha: String,
    loc: u32,
    parsed: Arc<ParsedFile>,
    /// Estimated heap footprint of `parsed`, computed once for the weigher.
    weight: u32,
}

/// Approximate heap bytes of a decoded parse (strings plus vector slots).
fn parsed_weight(parsed: &ParsedFile) -> usize {
    let symbols: usize = parsed
        .symbols
        .iter()
        .map(|s| std::mem::size_of::<CodeSymbol>() + s.name.len() + s.signature.len())
        .sum();
    let imports: usize = parsed
        .imports
        .iter()
        .map(|i| std::mem::size_of::<String>() + i.len())
        .sum();
    let calls: usize = parsed
        .calls
        .iter()
        .map(|(c, _)| std::mem::size_of::<(String, u32)>() + c.len())
        .sum();
    std::mem::size_of::<ParsedFile>() + symbols + imports + calls
}

/// Byte-capped LRU of decoded [`ParsedFile`] products, keyed by repo-relative
/// path. Values are cheap to clone (`Arc`). They used to be the JSON cache
/// encoding, which every apply decoded again for every unchanged file; that
/// decode was a fifth of the cost of a one-file edit on a large repository.
/// A new cache is filled on each incremental apply so a retained previous
/// [`IncrementalIndex`] is not mutated.
struct ParseProductCache {
    budget: u64,
    bytes: u64,
    /// Apply number that produced this cache; entries carry the generation
    /// in which their file was last parsed.
    generation: u64,
    entries: HashMap<RelPath, (CachedProduct, u64), FxBuild>,
}

impl ParseProductCache {
    fn new(budget_bytes: u64, generation: u64) -> Self {
        Self {
            budget: budget_bytes.max(1),
            bytes: 0,
            generation,
            entries: HashMap::default(),
        }
    }

    fn entry_bytes(path: &RelPath, value: &CachedProduct) -> u64 {
        (path.as_str().len()
            + value.sha.len()
            + value.weight as usize
            + std::mem::size_of::<CachedProduct>()) as u64
    }

    fn get(&self, path: &RelPath) -> Option<CachedProduct> {
        self.entries.get(path).map(|(product, _)| product.clone())
    }

    /// A product parsed in this cache's generation.
    fn insert(&mut self, path: RelPath, product: CachedProduct) {
        let generation = self.generation;
        self.insert_at(path, product, generation);
    }

    /// Carry a product over from an earlier cache, keeping its age.
    fn insert_at(&mut self, path: RelPath, product: CachedProduct, generation: u64) {
        self.bytes += Self::entry_bytes(&path, &product);
        if let Some((old, _)) = self.entries.insert(path.clone(), (product, generation)) {
            self.bytes -= Self::entry_bytes(&path, &old);
        }
    }

    fn generation_of(&self, path: &RelPath) -> Option<u64> {
        self.entries.get(path).map(|(_, generation)| *generation)
    }

    /// Enforce the byte budget: drop the products parsed longest ago (files
    /// edited recently are the likeliest to be edited again), path order
    /// breaking ties so eviction is deterministic. An evicted file is simply
    /// re-read and re-parsed by the next apply.
    fn run_pending(&mut self) {
        if self.bytes <= self.budget {
            return;
        }
        let mut order: Vec<(u64, RelPath, u64)> = self
            .entries
            .iter()
            .map(|(path, (product, generation))| {
                (*generation, path.clone(), Self::entry_bytes(path, product))
            })
            .collect();
        order.sort_unstable();
        for (_, path, bytes) in order {
            if self.bytes <= self.budget {
                break;
            }
            self.entries.remove(&path);
            self.bytes -= bytes;
        }
    }

    #[cfg(test)]
    fn weighted_size(&self) -> u64 {
        self.bytes
    }
}

/// FxHash (rustc's hasher): the assembly maps are keyed by repository paths
/// and symbol names, rebuilt on every apply, and SipHash was a visible share
/// of a one-file edit. Not DoS-resistant, which a local index does not need.
#[derive(Default, Clone, Copy)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

type FxBuild = std::hash::BuildHasherDefault<FxHasher>;

fn declared_excludes(resolvers: &ResolverSet) -> Vec<String> {
    [
        Language::Python,
        Language::Go,
        Language::Java,
        Language::Rust,
        Language::TypeScript,
        Language::Dart,
        Language::CSharp,
        Language::VisualBasic,
        Language::Swift,
        Language::ObjC,
        Language::Kotlin,
        Language::Shell,
        Language::Ruby,
        Language::Lua,
    ]
    .iter()
    .filter_map(|lang| resolvers.meta_for(*lang))
    .flat_map(|meta| meta.excludes.iter().cloned())
    .collect()
}

/// Last observed `#!` verdict per extension-less path, keyed by
/// `(mtime, len)` so an untouched `LICENSE` is never reopened.
#[derive(Clone, Copy)]
struct ShebangEntry {
    mtime: std::time::SystemTime,
    len: u64,
    script: bool,
}

const SHEBANG_CACHE_CAP: usize = 100_000;
/// Same-size writes inside this window can share an mtime; never trust a
/// cached verdict that young.
const SHEBANG_UNTRUSTED_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

static SHEBANG_CACHE: std::sync::LazyLock<Mutex<HashMap<PathBuf, ShebangEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Verdict of [`shebang_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShebangState {
    /// The file exists and starts with `#!` (optionally after a UTF-8 BOM).
    pub script: bool,
    /// The last cached verdict for this path said it was a script.
    pub was_script: bool,
    /// The file could not be stat'ed (deleted / racing).
    pub missing: bool,
}

/// Whether an extension-less file is a shebang script. Reads at most 5
/// bytes, and only when `(mtime, len)` changed since the last check.
pub fn has_shebang(path: &Path) -> bool {
    shebang_state(path).script
}

/// [`has_shebang`] plus the previously cached verdict, so the watcher can
/// tell "a script lost its shebang" (drop its symbols) from "a non-script
/// was edited" (nothing to do).
pub fn shebang_state(path: &Path) -> ShebangState {
    let cached = || {
        SHEBANG_CACHE
            .lock()
            .expect("shebang cache poisoned")
            .get(path)
            .copied()
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return ShebangState {
            script: false,
            was_script: cached().is_some_and(|e| e.script),
            missing: true,
        };
    };
    let mtime = metadata.modified().ok();
    let trusted = mtime.is_some_and(|m| {
        std::time::SystemTime::now()
            .duration_since(m)
            .is_ok_and(|age| age >= SHEBANG_UNTRUSTED_WINDOW)
    });
    let previous = cached();
    if let Some(e) = previous {
        if trusted && Some(e.mtime) == mtime && e.len == metadata.len() {
            return ShebangState {
                script: e.script,
                was_script: e.script,
                missing: false,
            };
        }
    }
    let script = metadata.is_file() && read_starts_with_shebang(path);
    if let Some(mtime) = mtime {
        let mut cache = SHEBANG_CACHE.lock().expect("shebang cache poisoned");
        if cache.len() >= SHEBANG_CACHE_CAP {
            cache.clear();
        }
        cache.insert(
            path.to_path_buf(),
            ShebangEntry {
                mtime,
                len: metadata.len(),
                script,
            },
        );
    }
    ShebangState {
        script,
        was_script: previous.is_some_and(|e| e.script),
        missing: false,
    }
}

fn read_starts_with_shebang(path: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::with_capacity(5);
    if file.take(5).read_to_end(&mut head).is_err() {
        return false;
    }
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&head);
    head.starts_with(b"#!")
}

/// Extension-less path (no language by path) that is a shebang script.
fn is_extensionless_script(root: &Path, path: &RelPath) -> bool {
    Language::is_shebang_candidate(path)
        && Language::from_path(path).is_none()
        && has_shebang(&root.join(path.as_str()))
}

/// Source-language files the scanner dropped for exceeding the size cap.
fn too_large_sources(root: &Path, scanned: &ScanResult) -> Vec<RelPath> {
    scanned
        .skipped
        .iter()
        .filter(|(path, reason)| {
            *reason == scan::SkipReason::TooLarge
                && (Language::from_path(path).is_some() || is_extensionless_script(root, path))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// Scan + detect + resolver-declared excludes. Shared by the full and
/// incremental-capable first build.
fn discover(
    root: &Path,
    opts: &IndexOptions,
) -> (ScanResult, FileIndex, ResolverSet, Vec<RelPath>) {
    let scanned = scan::scan(root, &opts.scan);
    let files = scan::to_index(root, &scanned);
    let resolvers = ResolverSet::detect(&files);
    let (scanned, files, dropped) = apply_declared_excludes(root, scanned, files, &resolvers);
    (scanned, files, resolvers, dropped)
}

/// Authored source files (TS family, not `.d.ts`) among paths dropped by
/// declared build-output excludes. Build output never contains these, so
/// any hit means sources went missing and must be surfaced as unindexed.
fn dropped_sources(before: &[RelPath], after: &[RelPath]) -> Vec<RelPath> {
    let kept: HashSet<&RelPath> = after.iter().collect();
    before
        .iter()
        .filter(|p| !kept.contains(p))
        .filter(|p| crate::resolvers::typescript::is_authored_ts(p.as_str()))
        .cloned()
        .collect()
}

fn apply_declared_excludes(
    root: &Path,
    scanned: ScanResult,
    files: FileIndex,
    resolvers: &ResolverSet,
) -> (ScanResult, FileIndex, Vec<RelPath>) {
    // Build output can sit outside the default exclude list — Cargo's
    // `crates/*/target`, a tsconfig `outDir` like `packages/app/dist`. Only
    // `detect` knows about those, so the scan is filtered afterwards rather
    // than walked twice. Detection itself stays valid: dropping generated
    // files cannot invalidate the build config we already read.
    let excludes = declared_excludes(resolvers);
    // `files` is the index of `scanned` as passed in; it is rebuilt only
    // when an exclude actually removed something (rebuilding it was a
    // visible share of every incremental apply).
    if excludes.is_empty() {
        return (scanned, files, Vec::new());
    }
    let before_len = scanned.files.len();
    let before = scanned.files.clone();
    let scanned = scan::apply_excludes(scanned, &excludes);
    if scanned.files.len() == before_len {
        return (scanned, files, Vec::new());
    }
    let dropped = dropped_sources(&before, &scanned.files);
    let files = scan::to_index(root, &scanned);
    (scanned, files, dropped)
}

/// `(mtime, len)` of a file as observed before it was read.
#[derive(Clone, Copy)]
struct FileStat {
    mtime_ns: u128,
    len: u64,
    /// Old enough that a same-size rewrite cannot share this mtime.
    trusted: bool,
}

impl FileStat {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        let modified = metadata.modified().ok()?;
        let mtime_ns = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let trusted = std::time::SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age >= SHEBANG_UNTRUSTED_WINDOW);
        Some(Self {
            mtime_ns,
            len: metadata.len(),
            trusted,
        })
    }
}

/// Per-run side channels of [`parse_one`]: stamps to persist and the keys
/// that are still live (for pruning after a full scan).
struct ParseSide {
    /// Whether the store accepts writes.
    writable: bool,
    track_live: bool,
    stamps: Mutex<Vec<(String, store::FileStamp)>>,
    live_keys: Mutex<Vec<String>>,
    live_paths: Mutex<Vec<String>>,
}

impl ParseSide {
    fn new(store: Option<&Store>, track_live: bool) -> Self {
        Self {
            writable: store.is_some_and(|s| !s.is_read_only()),
            track_live,
            stamps: Mutex::new(Vec::new()),
            live_keys: Mutex::new(Vec::new()),
            live_paths: Mutex::new(Vec::new()),
        }
    }

    fn note_live(&self, path: &RelPath, key: &str) {
        if self.track_live {
            self.live_keys
                .lock()
                .expect("live keys poisoned")
                .push(key.to_owned());
            self.live_paths
                .lock()
                .expect("live paths poisoned")
                .push(path.as_str().to_owned());
        }
    }

    fn note_stamp(
        &self,
        path: &RelPath,
        stat: Option<&FileStat>,
        key: &str,
        sha: &str,
        loc: u32,
        language: Language,
    ) {
        let Some(stat) = stat else {
            return;
        };
        if !self.writable || !stat.trusted {
            return;
        }
        self.stamps.lock().expect("stamps poisoned").push((
            path.as_str().to_owned(),
            store::FileStamp {
                mtime_ns: stat.mtime_ns,
                len: stat.len,
                key: key.to_owned(),
                sha: sha.to_owned(),
                loc,
                language: store::language_to_u8(language),
            },
        ));
    }
}

/// Warm start without reading the file: the recorded `(mtime, len)` still
/// matches, so reuse the cached parse stored under the recorded key.
fn stamp_hit(
    store: &Store,
    path: &RelPath,
    stat: &FileStat,
    side: &ParseSide,
    cache_hits: &AtomicUsize,
) -> Option<ParsedRow> {
    if !stat.trusted {
        return None;
    }
    let stamp = store.file_stamp(path.as_str()).ok()??;
    if stamp.mtime_ns != stat.mtime_ns
        || stamp.len != stat.len
        || !stamp.key.starts_with(&store::parse_cache_key_prefix())
    {
        return None;
    }
    let language = stamp.language()?;
    let bytes = store.cached_parse(&stamp.key).ok()??;
    let parsed = store::decode_parsed(&bytes).ok()?;
    cache_hits.fetch_add(1, Ordering::Relaxed);
    side.note_live(path, &stamp.key);
    Some(ParsedRow {
        path: path.clone(),
        language,
        sha: stamp.sha,
        loc: stamp.loc,
        parsed: Arc::new(parsed),
    })
}

/// Reason string carried through the failure list for a file that parsed with
/// syntax errors. Such a file is still in the index (its symbols are kept), so
/// [`split_partial`] moves it out of `parse_failures` into `partial_parses`.
use crate::{PARTIAL_PARSE_REASON, PARTIAL_PARSE_REWRITE_REASON};

fn is_partial_reason(reason: &str) -> bool {
    reason == PARTIAL_PARSE_REASON || reason == PARTIAL_PARSE_REWRITE_REASON
}

fn partial_reason(parsed: &ParsedFile) -> &'static str {
    if parsed.c_family_rewrite {
        PARTIAL_PARSE_REWRITE_REASON
    } else {
        PARTIAL_PARSE_REASON
    }
}

type FailureList = Vec<(RelPath, String)>;

/// Record that `path` is in the index with syntax errors.
fn note_partial(failures: &Mutex<Vec<(RelPath, String)>>, path: &RelPath, parsed: &ParsedFile) {
    failures
        .lock()
        .expect("failure list poisoned")
        .push((path.clone(), partial_reason(parsed).to_owned()));
}

/// Split a raw failure list into real failures (not in the index) and partial
/// parses (in the index, possibly incomplete).
fn split_partial(raw: &[(RelPath, String)]) -> (FailureList, FailureList) {
    raw.iter()
        .cloned()
        .partition(|(_, reason)| !is_partial_reason(reason))
}

#[allow(clippy::too_many_arguments)]
fn parse_one(
    root: &Path,
    path: &RelPath,
    pool: &ParserPool,
    store: Option<&Store>,
    failures: &Mutex<Vec<(RelPath, String)>>,
    cache_writes: &CacheWrites,
    cache_hits: &AtomicUsize,
    side: &ParseSide,
) -> Option<ParsedRow> {
    let by_path = Language::from_path(path);
    // Extension-less files may still be scripts (`#!/usr/bin/env bash`);
    // only those are sniffed. Anything else without a known extension is skipped.
    if by_path.is_none() && !Language::is_shebang_candidate(path) {
        return None;
    }
    // Cheap cached `#!` probe first: a `LICENSE` / `Makefile` is never read
    // in full, and an unchanged one is not even reopened.
    if by_path.is_none() && !has_shebang(&root.join(path.as_str())) {
        return None;
    }
    // Stat before reading: a write that lands after this point changes the
    // mtime/len we recorded, so the next run re-reads instead of trusting it.
    let stat = std::fs::metadata(root.join(path.as_str()))
        .ok()
        .and_then(|m| FileStat::of(&m));
    if let (Some(store), Some(stat)) = (store, stat.as_ref()) {
        if let Some(row) = stamp_hit(store, path, stat, side, cache_hits) {
            if row.parsed.partial {
                note_partial(failures, path, &row.parsed);
            }
            return Some(row);
        }
    }
    let source = match crate::text::read_source(&root.join(path.as_str())) {
        Ok(source) => source,
        // An unreadable / binary extension-less file is not a source failure.
        Err(_) if by_path.is_none() => return None,
        Err(error) => {
            failures
                .lock()
                .expect("failure list poisoned")
                .push((path.clone(), format!("read: {error}")));
            return None;
        }
    };
    let language = Language::from_path_and_source(path, &source).or(by_path)?;
    let (sha, loc) = digest(&source);
    let mut cache_key = store::parse_cache_key(language, &sha);
    // Path-dependent extractors must not share a content-addressed product:
    // two identical `.vue` files at different paths get different component
    // symbols (named after the file stem).
    if let Some(salt) = crate::parse::path_cache_salt(language, path) {
        cache_key.push(':');
        cache_key.push_str(&salt);
    }

    side.note_live(path, &cache_key);
    if let Some(store) = store {
        match store.cached_parse(&cache_key) {
            Ok(Some(bytes)) => match store::decode_parsed(&bytes) {
                Ok(parsed) => {
                    cache_hits.fetch_add(1, Ordering::Relaxed);
                    if parsed.partial {
                        note_partial(failures, path, &parsed);
                    }
                    side.note_stamp(path, stat.as_ref(), &cache_key, &sha, loc, language);
                    return Some(ParsedRow {
                        path: path.clone(),
                        language,
                        sha,
                        loc,
                        parsed: Arc::new(parsed),
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.as_str(),
                        "parse cache entry ignored; re-parsing"
                    );
                }
            },
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.as_str(),
                    "parse cache lookup failed; re-parsing"
                );
            }
        }
    }

    match pool.parse(language, path, &source) {
        Ok(parsed) => {
            if parsed.partial {
                // Syntax errors: symbols/imports/calls may be incomplete.
                // Keep what was extracted and surface it. The product is
                // cached like a clean one (the `partial` flag is part of the
                // cached payload, so a cache hit reports it again): a file
                // that stays broken must not pay the parse, plus the C-family
                // / C# / TS retry pass, on every start. Timeouts and other
                // parse errors are `Err` below and are never cached.
                note_partial(failures, path, &parsed);
            }
            // Only a writable store needs the encoding; the in-memory product
            // keeps the decoded parse.
            if let Some(store) = store.filter(|s| !s.is_read_only()) {
                match store::encode_parsed(&parsed) {
                    Ok(bytes) => {
                        side.note_stamp(path, stat.as_ref(), &cache_key, &sha, loc, language);
                        cache_writes.push(store, cache_key, Arc::from(bytes));
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            path = %path.as_str(),
                            "parse cache encode failed"
                        );
                    }
                }
            }
            Some(ParsedRow {
                path: path.clone(),
                language,
                sha,
                loc,
                parsed: Arc::new(parsed),
            })
        }
        Err(error) => {
            failures
                .lock()
                .expect("failure list poisoned")
                .push((path.clone(), error.to_string()));
            None
        }
    }
}

/// Encoded parses waiting for the next store write. Bounded: once the pending
/// bytes pass [`CACHE_FLUSH_BYTES`] a parse thread flushes them, so a cold
/// start never accumulates the whole repo's encoding just to write it at the
/// end.
/// `(entries, total bytes)` not yet written.
type PendingWrites = (Vec<(String, Arc<[u8]>)>, usize);

struct CacheWrites {
    pending: Mutex<PendingWrites>,
    stored: AtomicUsize,
    flush_bytes: usize,
}

/// Pending-bytes threshold that triggers a flush from a parse thread.
const CACHE_FLUSH_BYTES: usize = 32 * 1024 * 1024;

impl CacheWrites {
    fn new() -> Self {
        Self::with_flush_bytes(CACHE_FLUSH_BYTES)
    }

    fn with_flush_bytes(flush_bytes: usize) -> Self {
        Self {
            pending: Mutex::new((Vec::new(), 0)),
            stored: AtomicUsize::new(0),
            flush_bytes,
        }
    }

    fn push(&self, store: &Store, key: String, bytes: Arc<[u8]>) {
        if store.is_read_only() {
            return;
        }
        let batch = {
            let mut pending = self.pending.lock().expect("cache write list poisoned");
            pending.1 += bytes.len();
            pending.0.push((key, bytes));
            if pending.1 >= self.flush_bytes {
                pending.1 = 0;
                Some(std::mem::take(&mut pending.0))
            } else {
                None
            }
        };
        if let Some(batch) = batch {
            self.write(store, batch);
        }
    }

    fn write(&self, store: &Store, batch: Vec<(String, Arc<[u8]>)>) {
        if batch.is_empty() {
            return;
        }
        self.stored.fetch_add(batch.len(), Ordering::Relaxed);
        if let Err(error) =
            store.put_cached_parses(batch.iter().map(|(key, bytes)| (key.as_str(), &bytes[..])))
        {
            tracing::warn!(
                error = %error,
                "parse cache write failed; index is still valid"
            );
        }
    }

    fn flush(&self, store: &Store) {
        let batch = {
            let mut pending = self.pending.lock().expect("cache write list poisoned");
            pending.1 = 0;
            std::mem::take(&mut pending.0)
        };
        self.write(store, batch);
    }
}

fn persist_cache_writes(
    root: &Path,
    store: Option<&mut Store>,
    cache_writes: CacheWrites,
    cache_hits: AtomicUsize,
    side: ParseSide,
    prune: bool,
) {
    let Some(store) = store else {
        return;
    };
    if store.is_read_only() {
        tracing::info!(
            hits = cache_hits.load(Ordering::Relaxed),
            path = %store_path(root).display(),
            "parse cache (read-only)"
        );
        return;
    }
    cache_writes.flush(store);
    let stored = cache_writes.stored.load(Ordering::Relaxed);
    let stamps = side.stamps.into_inner().expect("stamps poisoned");
    if let Err(error) = store.put_file_stamps(&stamps) {
        tracing::warn!(error = %error, "file stamp write failed; index is still valid");
    }
    if prune {
        let live_keys: HashSet<String> = side
            .live_keys
            .into_inner()
            .expect("live keys poisoned")
            .into_iter()
            .collect();
        let live_paths: HashSet<String> = side
            .live_paths
            .into_inner()
            .expect("live paths poisoned")
            .into_iter()
            .collect();
        let removed = match store.prune_parse_cache(&live_keys, &live_paths) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(error = %error, "parse cache prune failed; index is still valid");
                0
            }
        };
        if removed > 0 {
            tracing::info!(removed, "parse cache pruned");
        }
        // redb reuses freed pages but never returns them. After an upgrade
        // (every old-format entry dead at once) the file would keep the
        // previous generation's size on top of the new one, and a cold build
        // leaves the slack of its batched commits. Compact when a large share
        // of the table was removed or rewritten: a one-off after a format or
        // extractor change or a cold build (tens of ms), never on a warm start.
        let churn = removed.max(stored);
        if churn >= COMPACT_MIN_CHURN && churn * 4 >= removed + live_keys.len() {
            let started = std::time::Instant::now();
            match store.compact() {
                Ok(_) => tracing::info!(
                    removed,
                    stored,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "parse cache compacted"
                ),
                Err(error) => tracing::warn!(
                    error = %error,
                    "parse cache compaction failed; index is still valid"
                ),
            }
        }
    }
    tracing::info!(
        hits = cache_hits.load(Ordering::Relaxed),
        stored,
        path = %store_path(root).display(),
        "parse cache"
    );
}

/// Removed or written entries that make a compaction worthwhile (and at
/// least a quarter of the table).
const COMPACT_MIN_CHURN: usize = 256;

/// Parse every path that has a known language. Failures are collected, never
/// swallowed. A disk-store cache hit skips tree-sitter; FileId assignment
/// still happens later after a path sort, so rayon order does not leak.
///
/// `full_scan` marks a parse over *every* admitted file (cold build): only
/// then may the persistent cache be pruned down to what is still referenced.
fn parse_paths(
    root: &Path,
    paths: &[RelPath],
    opts: &IndexOptions,
    full_scan: bool,
) -> ParseOutcome {
    parse_paths_cancellable(root, paths, opts, full_scan, None)
        .expect("parse without a cancel token always completes")
}

/// [`parse_paths`] that stops early once `cancel` is set (session closed,
/// root evicted, server shutting down). Workers skip every file not yet
/// started, and a cancelled run writes **nothing more** to the store: no
/// pending parse batch, no file stamps and, above all, no prune — a prune
/// keyed on a partial live set would delete valid entries. Batches already
/// flushed mid-run are content-addressed (`sha` key) and committed in their
/// own transactions, so the store stays consistent. Returns `None` when
/// cancelled.
fn parse_paths_cancellable(
    root: &Path,
    paths: &[RelPath],
    opts: &IndexOptions,
    full_scan: bool,
    cancel: Option<&AtomicBool>,
) -> Option<ParseOutcome> {
    let pool = ParserPool::new();
    let mut store = open_store(root, opts);
    let side = ParseSide::new(store.as_ref(), full_scan);
    let failures = Mutex::new(Vec::new());
    let cache_writes = CacheWrites::new();
    let cache_hits = AtomicUsize::new(0);
    let parsed: Vec<ParsedRow> = paths
        .par_iter()
        .filter_map(|path| {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return None;
            }
            parse_one(
                root,
                path,
                &pool,
                store.as_ref(),
                &failures,
                &cache_writes,
                &cache_hits,
                &side,
            )
        })
        .collect();
    if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
        return None;
    }
    persist_cache_writes(
        root,
        store.as_mut(),
        cache_writes,
        cache_hits,
        side,
        full_scan,
    );
    let mut failures = failures.into_inner().expect("failure list poisoned");
    failures.sort();
    Some(ParseOutcome { parsed, failures })
}

struct ParseOutcome {
    parsed: Vec<ParsedRow>,
    failures: Vec<(RelPath, String)>,
}

fn insert_product(cache: &mut ParseProductCache, row: &ParsedRow) {
    cache.insert(
        row.path.clone(),
        CachedProduct {
            language: row.language,
            sha: row.sha.clone(),
            loc: row.loc,
            parsed: Arc::clone(&row.parsed),
            weight: u32::try_from(parsed_weight(&row.parsed)).unwrap_or(u32::MAX),
        },
    );
}

fn fill_products(rows: &[ParsedRow], budget_bytes: u64) -> ParseProductCache {
    let mut products = ParseProductCache::new(budget_bytes, 0);
    for row in rows {
        insert_product(&mut products, row);
    }
    products.run_pending();
    products
}

fn row_from_product(path: RelPath, product: &CachedProduct) -> ParsedRow {
    ParsedRow {
        path,
        language: product.language,
        sha: product.sha.clone(),
        loc: product.loc,
        parsed: Arc::clone(&product.parsed),
    }
}

/// Parallel collection order is not stable; sort before assigning ids so
/// FileId is a deterministic function of the repo contents.
fn assemble_graph(
    parsed: Vec<ParsedRow>,
    files: &FileIndex,
    resolvers: &ResolverSet,
    opts: &IndexOptions,
    report: IndexReport,
) -> (CodeGraph, IndexReport) {
    let (graph, report, _) =
        assemble_graph_with(parsed, files, resolvers, opts, report, None, false);
    (graph, report)
}

/// Import resolutions of one file, in `imports` order.
struct FileResolutions {
    /// Language the specs were resolved as.
    lang: Language,
    items: Vec<(String, ImportResolution)>,
}

/// Per-file import resolutions kept by an [`IncrementalIndex`] so an edit
/// re-resolves only the files whose imports (or resolver metadata) changed.
#[derive(Default)]
struct ResolutionCache {
    by_path: HashMap<RelPath, Arc<FileResolutions>>,
}

/// Cached resolutions from the previous snapshot, usable only when the file
/// set is identical (resolution reads paths, never contents) and for
/// languages whose resolver metadata did not change.
struct ResolveReuse<'a> {
    cache: &'a ResolutionCache,
    changed: &'a [Language],
}

fn assemble_graph_with(
    mut parsed: Vec<ParsedRow>,
    files: &FileIndex,
    resolvers: &ResolverSet,
    opts: &IndexOptions,
    mut report: IndexReport,
    reuse: Option<ResolveReuse<'_>>,
    record: bool,
) -> (CodeGraph, IndexReport, Option<ResolutionCache>) {
    let mut new_cache = record.then(ResolutionCache::default);
    parsed.sort_by(|a, b| a.path.cmp(&b.path));
    report.files_parsed = parsed.len();

    let mut graph = CodeGraph::default();
    let mut index_of: HashMap<RelPath, FileId, FxBuild> =
        HashMap::with_capacity_and_hasher(parsed.len(), FxBuild::default());
    for (i, row) in parsed.iter().enumerate() {
        let id = FileId(i as u32);
        index_of.insert(row.path.clone(), id);
        graph.files.push(CodeFile {
            id,
            path: row.path.clone(),
            language: Some(row.language),
            loc: row.loc,
            sha: row.sha.clone(),
        });
    }

    // Symbols move into the graph (no second copy of every name/signature);
    // nothing below reads `row.parsed.symbols`. The cache encoding is not
    // needed past this point either.
    // Rows are sorted by path and FileId(i) is row i, so symbols stay grouped
    // by file in file order; `file_ranges[i]` is file i's slice. Cloned (not
    // moved) because the parse is shared with the product cache, in parallel
    // because this is a copy of every symbol of the repository per apply.
    let mut file_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(parsed.len());
    let mut next_symbol = 0usize;
    for row in &parsed {
        let n = row.parsed.symbols.len();
        file_ranges.push(next_symbol..next_symbol + n);
        next_symbol += n;
    }
    let per_file: Vec<Vec<CodeSymbol>> = parsed
        .par_iter()
        .enumerate()
        .map(|(i, row)| {
            let base = file_ranges[i].start;
            row.parsed
                .symbols
                .iter()
                .enumerate()
                .map(|(k, sym)| CodeSymbol {
                    id: SymbolId((base + k) as u32),
                    file: FileId(i as u32),
                    ..sym.clone()
                })
                .collect()
        })
        .collect();
    graph.symbols.reserve(next_symbol);
    for chunk in per_file {
        graph.symbols.extend(chunk);
    }
    report.symbols = graph.symbols.len();

    // Import edges. An unresolved specifier is only worth reporting when it
    // looks like it points inside the repo — stdlib and third-party imports
    // resolving to None is the correct answer, not a gap.
    let mut edges = Vec::new();
    // Resolution is per file and read-only, so it runs in parallel; edges,
    // the report and the cache are merged below in file order, keeping the
    // output deterministic.
    let entries: Vec<Arc<FileResolutions>> = parsed
        .par_iter()
        .map(|row| {
            // Path language first (keeps `.h` on the C resolver); content-sniffed
            // language only for extension-less shebang scripts.
            let import_lang = Language::from_path(&row.path).unwrap_or(row.language);
            let cached = reuse.as_ref().and_then(|r| {
                let entry = r.cache.by_path.get(&row.path)?;
                let same = entry.lang == import_lang
                    && !r.changed.contains(&resolver_owner(import_lang))
                    && entry.items.len() == row.parsed.imports.len()
                    && entry
                        .items
                        .iter()
                        .zip(&row.parsed.imports)
                        .all(|((spec, _), now)| spec == now);
                same.then(|| Arc::clone(entry))
            });
            let entry = cached.unwrap_or_else(|| {
                Arc::new(FileResolutions {
                    lang: import_lang,
                    items: row
                        .parsed
                        .imports
                        .iter()
                        .map(|spec| {
                            (
                                spec.clone(),
                                resolvers.resolve_import_as(import_lang, &row.path, spec, files),
                            )
                        })
                        .collect(),
                })
            });
            entry
        })
        .collect();
    for (row, entry) in parsed.iter().zip(entries) {
        let from = index_of[&row.path];
        for (spec, resolution) in &entry.items {
            match resolution {
                ImportResolution::Files(targets) => {
                    for target in targets {
                        if let Some(&to) = index_of.get(target) {
                            if to == from {
                                continue;
                            }
                            edges.push(CodeEdge {
                                from: from.0,
                                to: to.0,
                                kind: EdgeKind::Import,
                                weight: 1,
                                confidence: Confidence::Scoped,
                            });
                        }
                    }
                }
                ImportResolution::File(target) => {
                    if let Some(&to) = index_of.get(target).filter(|&&to| to != from) {
                        edges.push(CodeEdge {
                            from: from.0,
                            to: to.0,
                            kind: EdgeKind::Import,
                            weight: 1,
                            // Backed by build configuration, not by a compiler.
                            confidence: Confidence::Scoped,
                        });
                    }
                }
                ImportResolution::Namespace { files: targets, .. } => {
                    // Every declaring file, partials included. Not an Import:
                    // PageRank stays on real file edges.
                    for target in targets {
                        if let Some(&to) = index_of.get(target).filter(|&&to| to != from) {
                            edges.push(CodeEdge {
                                from: from.0,
                                to: to.0,
                                kind: EdgeKind::Namespace,
                                weight: 1,
                                confidence: Confidence::Scoped,
                            });
                        }
                    }
                }
                ImportResolution::Unresolved => {
                    if looks_internal(spec) {
                        report
                            .unresolved_imports
                            .push((row.path.clone(), spec.clone()));
                    }
                }
            }
        }
        if let Some(cache) = new_cache.as_mut() {
            cache.by_path.insert(row.path.clone(), entry);
        }
    }

    // ProjectReference pairs become an edge only when both manifests were
    // parsed as files. In practice `.csproj` is not a parsed source, so this
    // adds nothing today; the relationship lives on ProjectMeta::project_refs.
    for (_lang, meta) in resolvers.iter_meta() {
        for pref in &meta.project_refs {
            let from_path = RelPath::new(&pref.from);
            let to_path = RelPath::new(&pref.to);
            if let (Some(&from), Some(&to)) = (index_of.get(&from_path), index_of.get(&to_path)) {
                if from != to {
                    edges.push(CodeEdge {
                        from: from.0,
                        to: to.0,
                        kind: EdgeKind::Import,
                        weight: 1,
                        confidence: Confidence::Scoped,
                    });
                }
            }
        }
    }

    // C-family header -> implementation file. Not an include (see
    // `EdgeKind::Pair`): lets queries hop from a declaration to its body.
    edges.extend(crate::resolvers::c_cpp::pair_edges(&graph.files));

    let mut call_edges: Vec<CodeEdge> = Vec::new();
    if opts.call_edges {
        // Import edges join files; call edges join symbols. Both endpoints are
        // plain u32, so the distinction lives in `kind` — callers must read it
        // before interpreting `from`/`to`.
        let mut by_name: HashMap<&str, Vec<SymbolId>, FxBuild> =
            HashMap::with_capacity_and_hasher(graph.symbols.len(), FxBuild::default());
        for s in &graph.symbols {
            by_name.entry(s.name.as_str()).or_default().push(s.id);
        }
        let by_name = &by_name;
        let symbols = &graph.symbols;
        let calls: Vec<Vec<CodeEdge>> = parsed
            .par_iter()
            .enumerate()
            .map(|(i, row)| {
                let mut out = Vec::new();
                let in_file: Vec<&CodeSymbol> = symbols[file_ranges[i].clone()].iter().collect();
                if in_file.is_empty() {
                    return out;
                }
                // A call belongs to the smallest symbol spanning its line;
                // attributing it to the file would collapse every caller in
                // the file into one node.
                let callers = enclosing_symbols(&in_file, &row.parsed.calls);
                // (caller, its candidate targets) per call. Sorting these
                // (one per call) and then each caller's target ids (u32s)
                // emits the edges already ordered by `(from, to)`, far
                // cheaper than sorting the edges themselves.
                let mut by_caller: Vec<(u32, &[SymbolId])> = Vec::new();
                for ((callee, _line), caller) in row.parsed.calls.iter().zip(&callers) {
                    let Some(caller) = caller else {
                        continue;
                    };
                    // Take the trailing segment so `a.b.c()` matches `c`.
                    let short = callee
                        .rsplit(['.', ':'])
                        .find(|p| !p.is_empty())
                        .unwrap_or(callee);
                    let Some(targets) = by_name.get(short) else {
                        continue;
                    };
                    if targets.len() > MAX_SYNTACTIC_CALL_TARGETS {
                        continue;
                    }
                    by_caller.push((caller.id.0, targets.as_slice()));
                }
                by_caller.sort_by_key(|(caller, _)| *caller);
                let mut to: Vec<u32> = Vec::new();
                for group in by_caller.chunk_by(|a, b| a.0 == b.0) {
                    let from = group[0].0;
                    to.clear();
                    for (_, targets) in group {
                        to.extend(targets.iter().map(|t| t.0).filter(|&t| t != from));
                    }
                    to.sort_unstable();
                    for run in to.chunk_by(|a, b| a == b) {
                        out.push(CodeEdge {
                            from,
                            to: run[0],
                            kind: EdgeKind::Call,
                            weight: run.len() as u32,
                            // Name matching only, with no scope or type
                            // information. Measured recall against a
                            // language server: 66% on TypeScript, 18% on
                            // Python. Labelled so callers can decide whether
                            // to verify.
                            confidence: Confidence::Syntactic,
                        });
                    }
                }
                // Every `from` here is a symbol of file i, and symbol ids
                // ascend with the file, so per-file sorted runs concatenate
                // into one sorted run: no global sort over millions of call
                // edges on every apply.
                out
            })
            .collect();
        call_edges.reserve(calls.iter().map(Vec::len).sum());
        for chunk in calls {
            call_edges.extend(chunk);
        }
    }

    if opts.call_edges {
        // Go implicit interface satisfaction (type -> interface, syntactic
        // method-set match). Symbol-level like call edges, so behind the
        // same switch.
        let implements = crate::go_iface::implementation_edges(&graph, &edges);
        edges.extend(implements);
    }

    // Collapse duplicates into weights, ordered by `(from, to, kind)`. Every
    // edge kind has one confidence and weight 1 before merging, so equal keys
    // are identical and the order among them does not matter. The few
    // non-call edges are sorted here; the call edges arrive sorted and are
    // merged in one linear pass.
    sort_and_merge(&mut edges);
    debug_assert!(call_edges
        .windows(2)
        .all(|w| edge_key(&w[0]) < edge_key(&w[1])));
    let merged = merge_sorted_edges(edges, call_edges);
    report.import_edges = merged.iter().filter(|e| e.kind == EdgeKind::Import).count();
    graph.edges = merged;
    report.unresolved_imports.sort();

    (graph, report, new_cache)
}

fn edge_key(e: &CodeEdge) -> (u32, u32, u8) {
    (e.from, e.to, e.kind as u8)
}

/// Sort `edges` by [`edge_key`] and fold duplicates into their weight.
fn sort_and_merge(edges: &mut Vec<CodeEdge>) {
    edges.sort_unstable_by_key(edge_key);
    edges.dedup_by(|e, prev| {
        if edge_key(e) == edge_key(prev) {
            prev.weight += e.weight;
            true
        } else {
            false
        }
    });
}

/// Merge two runs already sorted and deduplicated by [`edge_key`], folding a
/// key present in both into one edge.
fn merge_sorted_edges(a: Vec<CodeEdge>, b: Vec<CodeEdge>) -> Vec<CodeEdge> {
    if b.is_empty() {
        return a;
    }
    if a.is_empty() {
        return b;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut a = a.into_iter().peekable();
    let mut b = b.into_iter().peekable();
    loop {
        let next = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => match edge_key(x).cmp(&edge_key(y)) {
                std::cmp::Ordering::Less => a.next(),
                std::cmp::Ordering::Greater => b.next(),
                std::cmp::Ordering::Equal => {
                    let mut x = a.next().expect("peeked");
                    x.weight += b.next().expect("peeked").weight;
                    Some(x)
                }
            },
            (Some(_), None) => a.next(),
            (None, Some(_)) => b.next(),
            (None, None) => break,
        };
        out.extend(next);
    }
    out
}

/// Linear-scan threshold of [`enclosing_symbols`]: below it the plain scan is
/// cheaper than building the sweep structures.
const ENCLOSING_SWEEP_MIN_SYMBOLS: usize = 48;

/// For each call `(callee, line)` the smallest symbol whose line span contains
/// `line` (ties: the earliest in `syms`), or `None`. Same answer as
/// `syms.filter(span contains line).min_by_key(span)` per call, but a large
/// (generated) file with S symbols and C calls costs O((S + C) log S) instead
/// of O(S·C): lines are swept in order with an ordered set of open symbols.
fn enclosing_symbols<'a>(
    syms: &[&'a CodeSymbol],
    calls: &[(String, u32)],
) -> Vec<Option<&'a CodeSymbol>> {
    let smallest_linear = |line: u32| {
        syms.iter()
            .filter(|s| s.start_line <= line && line <= s.end_line)
            .min_by_key(|s| s.end_line - s.start_line)
            .copied()
    };
    if syms.len() < ENCLOSING_SWEEP_MIN_SYMBOLS || calls.len() < 2 {
        return calls
            .iter()
            .map(|(_, line)| smallest_linear(*line))
            .collect();
    }
    // Symbols by start line, calls by line.
    let mut by_start: Vec<usize> = (0..syms.len()).collect();
    by_start.sort_by_key(|&i| syms[i].start_line);
    let mut call_order: Vec<usize> = (0..calls.len()).collect();
    call_order.sort_by_key(|&i| calls[i].1);

    let mut out = vec![None; calls.len()];
    // Open symbols keyed (span, original index): first element is the answer.
    let mut open: std::collections::BTreeSet<(u32, usize)> = std::collections::BTreeSet::new();
    // Min-heap of (end_line, key) to retire symbols whose span has ended.
    let mut ends: std::collections::BinaryHeap<std::cmp::Reverse<(u32, u32, usize)>> =
        std::collections::BinaryHeap::new();
    let mut next = 0usize;
    for ci in call_order {
        let line = calls[ci].1;
        while next < by_start.len() && syms[by_start[next]].start_line <= line {
            let i = by_start[next];
            let s = syms[i];
            // A reversed span (`end < start`) never contains a line.
            if s.start_line <= s.end_line {
                open.insert((s.end_line - s.start_line, i));
                ends.push(std::cmp::Reverse((
                    s.end_line,
                    s.end_line - s.start_line,
                    i,
                )));
            }
            next += 1;
        }
        while let Some(&std::cmp::Reverse((end, span, i))) = ends.peek() {
            if end >= line {
                break;
            }
            open.remove(&(span, i));
            ends.pop();
        }
        out[ci] = open.iter().next().map(|&(_, i)| syms[i]);
    }
    out
}

/// Build the graph for a repository.
///
/// Parsing runs in parallel; edge assembly is sequential and sorted so the
/// same bytes always produce the same graph.
///
/// One-shot callers that will not apply a later [`ChangeSet`] can use this
/// and drop the parse products. MCP (and anything else that reindexes on
/// watcher events) should keep an [`IncrementalIndex`] instead.
pub fn index_repo(root: &Path, opts: &IndexOptions) -> (CodeGraph, IndexReport) {
    let (scanned, files, resolvers, _) = discover(root, opts);
    let mut report = IndexReport {
        files_scanned: scanned.files.len(),
        too_large: too_large_sources(root, &scanned),
        ..IndexReport::default()
    };
    let outcome = parse_paths(root, &scanned.files, opts, true);
    (report.parse_failures, report.partial_parses) = split_partial(&outcome.failures);
    assemble_graph(outcome.parsed, &files, &resolvers, opts, report)
}

/// Snapshot of a previous index that can absorb a watcher [`ChangeSet`]
/// without re-reading unchanged source files.
///
/// Route (b): encoded parse products live in a byte-capped LRU; each apply
/// reuses hits, re-parses `added`/`modified` (and LRU misses), then
/// reassembles the graph. True graph-delta (route a) is deferred because
/// import resolution, FileId assignment, and name-matched call edges are
/// coupled across the whole file set.
///
/// MCP integration:
///
/// ```ignore
/// let mut index = IncrementalIndex::build(root, &opts);
/// // serve index.graph / index.report
/// // on ChangeSet from the watcher:
/// index = index_repo_incremental(root, &index, &changes, &opts);
/// ```
pub struct IncrementalIndex {
    pub graph: CodeGraph,
    pub report: IndexReport,
    /// Admitted paths after resolver excludes, including non-source files
    /// (`go.mod`, `Cargo.toml`, …) that resolvers need in [`FileIndex`].
    files: Vec<RelPath>,
    /// Authored sources dropped by declared build-output excludes; reported
    /// as unindexed so an empty result is never mistaken for full coverage.
    excluded_sources: Vec<RelPath>,
    products: ParseProductCache,
    /// Parse failures for files not re-read on the next apply.
    failures: Vec<(RelPath, String)>,
    /// How many times a transient failure (read error, parse budget) has
    /// been retried on an apply, to bound the retries.
    failure_retries: HashMap<RelPath, u8>,
    /// Resolver metadata of this snapshot. An apply that leaves the file set
    /// unchanged re-runs `detect` only for resolvers that read a modified file.
    resolvers: Arc<ResolverSet>,
    /// Per-file import resolutions, reused while the file set is unchanged.
    resolutions: Arc<ResolutionCache>,
}

impl std::fmt::Debug for IncrementalIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalIndex")
            .field("files", &self.files.len())
            .field("graph_files", &self.graph.files.len())
            .field("symbols", &self.graph.symbols.len())
            .finish_non_exhaustive()
    }
}

impl IncrementalIndex {
    /// Full index, retaining parse products for later [`Self::apply_changeset`].
    pub fn build(root: &Path, opts: &IndexOptions) -> Self {
        Self::build_inner(root, opts, None).expect("build without a cancel token always completes")
    }

    /// [`Self::build`] that gives up once `cancel` is set, returning `None`.
    ///
    /// Checked between phases (scan / resolver detect, parse, graph
    /// assembly) and before every file inside the parallel parse, so a
    /// cancelled cold build stops within roughly one file parse per worker.
    /// A cancelled build writes no file stamps and never prunes the parse
    /// cache (see [`parse_paths_cancellable`]); a later build reuses
    /// whatever complete batches it had already committed.
    pub fn build_cancellable(
        root: &Path,
        opts: &IndexOptions,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        Self::build_inner(root, opts, Some(cancel))
    }

    fn build_inner(root: &Path, opts: &IndexOptions, cancel: Option<&AtomicBool>) -> Option<Self> {
        let cancelled = || cancel.is_some_and(|c| c.load(Ordering::SeqCst));
        if cancelled() {
            return None;
        }
        let started = std::time::Instant::now();
        let (scanned, files, resolvers, excluded_sources) = discover(root, opts);
        let discovered = started.elapsed();
        if cancelled() {
            return None;
        }
        let mut report = IndexReport {
            files_scanned: scanned.files.len(),
            too_large: too_large_sources(root, &scanned),
            ..IndexReport::default()
        };
        let outcome = parse_paths_cancellable(root, &scanned.files, opts, true, cancel)?;
        let failures = outcome.failures.clone();
        (report.parse_failures, report.partial_parses) = split_partial(&outcome.failures);
        let parsed_at = started.elapsed();
        let products = fill_products(&outcome.parsed, opts.parse_cache_bytes);
        if cancelled() {
            return None;
        }
        let (graph, report, resolutions) =
            assemble_graph_with(outcome.parsed, &files, &resolvers, opts, report, None, true);
        tracing::info!(
            files = graph.files.len(),
            discover_ms = discovered.as_millis() as u64,
            parse_ms = (parsed_at - discovered).as_millis() as u64,
            assemble_ms = (started.elapsed() - parsed_at).as_millis() as u64,
            "index build"
        );
        if cancelled() {
            return None;
        }
        Some(Self {
            graph,
            report,
            files: scanned.files,
            excluded_sources,
            products,
            failures,
            failure_retries: HashMap::new(),
            resolvers: Arc::new(resolvers),
            resolutions: Arc::new(resolutions.unwrap_or_default()),
        })
    }

    /// Apply a watcher changeset. Only `added`/`modified` source files are
    /// read and tree-sitter parsed, plus LRU evictions (explicit fallback).
    ///
    /// Resolver `detect` may still read a handful of build-config files;
    /// those are not the 94k-file source parse path. Newly excluded paths
    /// are dropped in memory; newly un-excluded paths require an `added`
    /// event (or a full [`Self::build`]).
    pub fn apply_changeset(&self, root: &Path, changes: &ChangeSet, opts: &IndexOptions) -> Self {
        index_repo_incremental(root, self, changes, opts)
    }

    /// Admitted paths after resolver excludes, including non-source files
    /// (`go.mod`, `Cargo.toml`, …) that resolvers need in [`FileIndex`].
    pub fn admitted_paths(&self) -> &[RelPath] {
        &self.files
    }

    /// Authored source files hidden by a declared build-output exclude.
    pub fn excluded_source_paths(&self) -> &[RelPath] {
        &self.excluded_sources
    }

    #[cfg(test)]
    fn product_bytes(&self) -> u64 {
        self.products.weighted_size()
    }
}

/// Incremental rebuild from a previous [`IncrementalIndex`] and a watcher
/// [`ChangeSet`]. The MCP server will pass `(root, changes)` and keep the
/// returned snapshot; `previous` is not mutated.
pub fn index_repo_incremental(
    root: &Path,
    previous: &IncrementalIndex,
    changes: &ChangeSet,
    opts: &IndexOptions,
) -> IncrementalIndex {
    let mut files: BTreeSet<RelPath> = previous.files.iter().cloned().collect();
    for path in &changes.removed {
        files.remove(path);
    }
    for path in &changes.added {
        files.insert(path.clone());
    }
    // A modified path missing from the previous set is treated as added.
    for path in &changes.modified {
        files.insert(path.clone());
    }

    let mut dirty: HashSet<RelPath> = HashSet::new();
    for path in changes.added.iter().chain(&changes.modified) {
        if files.contains(path) {
            dirty.insert(path.clone());
        }
    }

    // The cold scan drops files over `max_file_bytes`; a live edit must agree,
    // otherwise a file that grew past the cap enters the graph with every
    // symbol and a restart makes it vanish.
    let mut newly_too_large: Vec<RelPath> = Vec::new();
    for path in dirty.clone() {
        let too_large = std::fs::metadata(root.join(path.as_str()))
            .is_ok_and(|m| m.is_file() && m.len() > opts.scan.max_file_bytes);
        if too_large {
            files.remove(&path);
            dirty.remove(&path);
            if Language::from_path(&path).is_some() || is_extensionless_script(root, &path) {
                newly_too_large.push(path);
            }
        }
    }

    let scanned = ScanResult {
        files: files.into_iter().collect(),
        skipped: Vec::new(),
    };
    let file_index = scan::to_index(root, &scanned);
    // Resolver metadata and import resolution read paths and (during
    // `detect`) the content of build/declaration files. With an unchanged
    // path set only `modified` contents can differ, so unaffected resolvers
    // and per-file resolutions carry over; any added/removed path (or a file
    // crossing the size cap) redoes everything, as a cold start would.
    let same_paths = scanned.files == previous.files;
    let (resolvers, changed_langs) = if same_paths {
        let modified: BTreeSet<RelPath> = changes.modified.iter().cloned().collect();
        let (set, changed) = previous.resolvers.redetect(&file_index, &modified);
        (set, Some(changed))
    } else {
        (ResolverSet::detect(&file_index), None)
    };
    let (scanned, file_index, newly_dropped) =
        apply_declared_excludes(root, scanned, file_index, &resolvers);
    let mut excluded_sources: BTreeSet<RelPath> = previous
        .excluded_sources
        .iter()
        .filter(|p| !changes.removed.contains(p))
        .cloned()
        .collect();
    excluded_sources.extend(newly_dropped);
    for p in &scanned.files {
        excluded_sources.remove(p);
    }

    let admitted: HashSet<&RelPath> = scanned.files.iter().collect();
    let mut too_large: Vec<RelPath> = previous
        .report
        .too_large
        .iter()
        .filter(|path| !changes.removed.contains(path) && !admitted.contains(path))
        .cloned()
        .collect();
    for path in newly_too_large {
        if !too_large.contains(&path) {
            too_large.push(path);
        }
    }
    let mut report = IndexReport {
        files_scanned: scanned.files.len(),
        too_large,
        ..IndexReport::default()
    };

    let mut to_parse = Vec::new();
    let mut reused = Vec::new();
    let mut reused_products = Vec::new();
    let mut failures = Vec::new();
    let mut lru_misses = 0usize;
    let previous_failures: HashMap<&RelPath, &String> =
        previous.failures.iter().map(|(p, r)| (p, r)).collect();
    let mut retried: HashMap<RelPath, u8> = HashMap::new();
    let mut failure_retries: HashMap<RelPath, u8> = HashMap::new();

    for path in &scanned.files {
        if dirty.contains(path) {
            to_parse.push(path.clone());
            continue;
        }
        // Keep in step with `parse_one`: extension-less files may be shebang
        // scripts, so they are not dropped here (no previous product → re-parse).
        if Language::from_path(path).is_none() && !Language::is_shebang_candidate(path) {
            continue;
        }
        // Extension-less non-scripts never have a product; skip them without
        // re-reading (the `#!` probe is cached by mtime/size).
        if Language::from_path(path).is_none()
            && previous.products.get(path).is_none()
            && !has_shebang(&root.join(path.as_str()))
        {
            continue;
        }
        if let Some(product) = previous.products.get(path) {
            let row = row_from_product(path.clone(), &product);
            // A partial parse keeps its product (the flag travels in the
            // product); carry it so the report stays stable.
            let previous_reason = previous_failures
                .get(path)
                .map(|r| r.as_str())
                .filter(|r| is_partial_reason(r));
            if row.parsed.partial {
                failures.push((path.clone(), partial_reason(&row.parsed).to_owned()));
            } else if let Some(reason) = previous_reason {
                failures.push((path.clone(), reason.to_owned()));
            }
            reused_products.push((path.clone(), product));
            reused.push(row);
        } else if let Some(reason) = previous_failures.get(path) {
            let tries = previous.failure_retries.get(path).copied().unwrap_or(0);
            if is_transient_failure(reason) && tries < MAX_FAILURE_RETRIES {
                // A timeout or a read that raced a writer says nothing about
                // the file itself: try again, a bounded number of times.
                retried.insert(path.clone(), tries + 1);
                to_parse.push(path.clone());
            } else {
                // Unchanged permanent failure: do not re-read.
                failures.push((path.clone(), (*reason).clone()));
                failure_retries.insert(path.clone(), tries);
            }
        } else {
            lru_misses += 1;
            to_parse.push(path.clone());
        }
    }

    let reused_count = reused.len();
    let outcome = parse_paths(root, &to_parse, opts, false);
    let parsed_count = outcome.parsed.len();
    for (path, _) in &outcome.failures {
        if let Some(tries) = retried.get(path) {
            failure_retries.insert(path.clone(), *tries);
        }
    }
    failures.extend(outcome.failures);
    failures.sort();
    (report.parse_failures, report.partial_parses) = split_partial(&failures);

    tracing::info!(
        added = changes.added.len(),
        modified = changes.modified.len(),
        removed = changes.removed.len(),
        reused = reused_count,
        parsed = parsed_count,
        lru_misses,
        "incremental index"
    );

    let mut products =
        ParseProductCache::new(opts.parse_cache_bytes, previous.products.generation + 1);
    for (path, product) in reused_products {
        let generation = previous.products.generation_of(&path).unwrap_or(0);
        products.insert_at(path, product, generation);
    }
    for row in &outcome.parsed {
        insert_product(&mut products, row);
    }
    products.run_pending();

    reused.extend(outcome.parsed);
    let reuse = changed_langs.as_deref().map(|changed| ResolveReuse {
        cache: &previous.resolutions,
        changed,
    });
    let (graph, report, resolutions) =
        assemble_graph_with(reused, &file_index, &resolvers, opts, report, reuse, true);
    IncrementalIndex {
        graph,
        report,
        files: scanned.files,
        excluded_sources: excluded_sources.into_iter().collect(),
        products,
        failures,
        failure_retries,
        resolvers: Arc::new(resolvers),
        resolutions: Arc::new(resolutions.unwrap_or_default()),
    }
}

/// Retries of a transient parse failure before it is treated as permanent.
const MAX_FAILURE_RETRIES: u8 = 3;

/// Failures that describe the moment, not the file.
fn is_transient_failure(reason: &str) -> bool {
    reason.starts_with("read:")
        || reason == crate::parse::ParseError::Timeout.to_string()
        || reason == crate::parse::ParseError::NoTree.to_string()
}

/// Code file extensions across known programming languages (46 pure programming language extensions).
///
/// 收窄说明：这是"未支持编程语言"的统计口径，纯编程语言源码扩展名；
/// 数据/配置文件（json, jsonc, yaml, yml, toml, html, css, graphql, gql, sql, sh, bash, ps1, tf, tfvars, hcl 等）
/// 不在此列，避免把数据/配置文件提示为"尚未支持的语言"从而稀释信号；
/// Serena 58 项全集的 read-deny 用途与统计用途分离。
pub const KNOWN_CODE_EXTENSIONS: &[&str] = &[
    "al", "c", "clj", "cljs", "cpp", "cs", "cshtml", "csx", "dart", "elm", "ex", "exs", "fs",
    "fsx", "go", "groovy", "h", "hpp", "hs", "java", "jl", "js", "jsx", "kt", "kts", "lean", "lua",
    "m", "matlab", "nf", "php", "proto", "py", "r", "razor", "rb", "rs", "scala", "sol", "svelte",
    "swift", "ts", "tsx", "vb", "vue", "zig",
];

/// Count occurrences of unindexed code file extensions among admitted paths.
///
/// - `filter`: when Some, paths not containing this substring are skipped (same contains semantics as `path_filter`).
/// - Skips paths where `in_graph(path.as_str())` is true.
/// - Skips paths whose language is already supported (`Language::from_path(path).is_some()`).
/// - Skips paths without an extension.
/// - Gated by vocabulary: only extensions in `KNOWN_CODE_EXTENSIONS` are counted.
/// - Extensions are normalized to lowercase.
pub fn unindexed_code_exts(
    admitted: &[RelPath],
    in_graph: &dyn Fn(&str) -> bool,
    filter: Option<&str>,
) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for p in admitted {
        let s = p.as_str();
        if let Some(f) = filter {
            if !s.contains(f) {
                continue;
            }
        }
        if in_graph(s) {
            continue;
        }
        if Language::from_path(p).is_some() {
            continue;
        }
        let Some(ext) = p.extension() else {
            continue;
        };
        let ext_lower = ext.to_ascii_lowercase();
        if KNOWN_CODE_EXTENSIONS.contains(&ext_lower.as_str()) {
            *counts.entry(ext_lower).or_insert(0) += 1;
        }
    }
    counts
}

/// Heuristic for "this specifier was meant to resolve inside the repo".
/// Deliberately conservative: a false negative just omits a diagnostic, while
/// a false positive would make the resolution-rate metric meaningless.
fn looks_internal(spec: &str) -> bool {
    spec.starts_with('.') || spec.starts_with("crate::") || spec.starts_with("self::")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, Store};

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astrolabe-index-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn non_utf8_and_cpp_header_files_are_indexed() {
        let root = scratch("enc");
        let (gbk, _, _) =
            encoding_rs::GBK.encode("# 中文注释：计算总和\ndef gbk_func():\n    return '你好'\n");
        let p = root.join("legacy.py");
        std::fs::write(&p, gbk.as_ref()).unwrap();
        std::fs::write(
            root.join("latin.py"),
            b"# caf\xe9\ndef latin_func():\n    pass\n",
        )
        .unwrap();
        write(
            &root,
            "inc/widget.h",
            "#pragma once\nnamespace ui {\nclass Widget {\npublic:\n  void draw();\n};\n}\n",
        );
        write(&root, "inc/plain.h", "int plain_add(int a, int b);\n");

        let opts = IndexOptions::default();
        let inc = IncrementalIndex::build(&root, &opts);
        let names: Vec<&str> = inc.graph.symbols.iter().map(|s| s.name.as_str()).collect();
        for need in ["gbk_func", "latin_func", "Widget", "plain_add"] {
            assert!(names.contains(&need), "missing {need}: {names:?}");
        }
        assert!(
            inc.report.parse_failures.is_empty(),
            "{:?}",
            inc.report.parse_failures
        );
        let lang_of = |path: &str| {
            inc.graph
                .files
                .iter()
                .find(|f| f.path.as_str() == path)
                .and_then(|f| f.language)
        };
        assert_eq!(lang_of("inc/widget.h"), Some(Language::Cpp));
        assert_eq!(lang_of("inc/plain.h"), Some(Language::C));

        // Edit the GBK file, then query again: the new symbol must appear.
        let (gbk2, _, _) = encoding_rs::GBK.encode(
            "# 中文注释：计算总和\ndef gbk_func():\n    return 1\n\ndef gbk_added():\n    pass\n",
        );
        std::fs::write(&p, gbk2.as_ref()).unwrap();
        let changes = ChangeSet {
            modified: vec![RelPath::new("legacy.py")],
            ..ChangeSet::default()
        };
        let next = inc.apply_changeset(&root, &changes, &opts);
        assert!(
            next.graph.symbols.iter().any(|s| s.name == "gbk_added"),
            "edited non-UTF-8 file must be re-indexed"
        );
    }

    #[test]
    fn oversized_extensionless_scripts_are_reported_non_scripts_are_not() {
        let root = scratch("bigscript");
        let mut body = "#!/usr/bin/env bash\n".to_string();
        body.push_str(&"echo hi\n".repeat(400_000));
        write(&root, "bin/huge-tool", &body);
        write(&root, "BIGDATA", &"x\n".repeat(1_000_000));
        write(&root, "small.py", "def small():\n    pass\n");
        let inc = IncrementalIndex::build(&root, &IndexOptions::default());
        assert_eq!(inc.report.too_large, vec![RelPath::new("bin/huge-tool")]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn shebang_probe_is_cheap_and_tracks_changes() {
        let root = scratch("probe");
        write(&root, "LICENSE", "MIT License\n");
        write(&root, "tool", "#!/bin/sh\n");
        write(&root, "bom", "\u{feff}#!/usr/bin/env ruby\n");
        assert!(!has_shebang(&root.join("LICENSE")));
        assert!(has_shebang(&root.join("tool")));
        assert!(has_shebang(&root.join("bom")));
        write(&root, "tool", "plain text now\n");
        let state = shebang_state(&root.join("tool"));
        assert!(!state.script && state.was_script && !state.missing);
        let gone = shebang_state(&root.join("nope"));
        assert!(gone.missing && !gone.script);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn oversized_sources_are_reported_and_survive_incremental_apply() {
        let root = scratch("big");
        write(&root, "small.py", "def small():\n    pass\n");
        write(&root, "huge.py", &"x = 1\n".repeat(400_000));
        write(&root, "huge.txt", &"x\n".repeat(1_000_000));
        let opts = IndexOptions::default();
        let inc = IncrementalIndex::build(&root, &opts);
        assert_eq!(inc.report.too_large, vec![RelPath::new("huge.py")]);

        write(&root, "small.py", "def small():\n    return 2\n");
        let changes = ChangeSet {
            modified: vec![RelPath::new("small.py")],
            ..ChangeSet::default()
        };
        let next = inc.apply_changeset(&root, &changes, &opts);
        assert_eq!(next.report.too_large, vec![RelPath::new("huge.py")]);
        let gone = ChangeSet {
            removed: vec![RelPath::new("huge.py")],
            ..ChangeSet::default()
        };
        assert!(next
            .apply_changeset(&root, &gone, &opts)
            .report
            .too_large
            .is_empty());
    }

    #[test]
    fn indexes_a_repo_end_to_end() {
        // (see also non_utf8_and_cpp_header_files_are_indexed)

        let root = scratch("e2e");
        write(&root, "go.mod", "module example.com/app\n");
        write(
            &root,
            "main.go",
            "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/util\"\n)\n\nfunc main() { fmt.Println(util.Greet()) }\n",
        );
        write(
            &root,
            "util/util.go",
            "package util\n\nfunc Greet() string { return \"hi\" }\n",
        );

        let (graph, report) = index_repo(&root, &IndexOptions::default());

        assert_eq!(report.files_parsed, 2, "both .go files parse");
        assert!(
            report.parse_failures.is_empty(),
            "{:?}",
            report.parse_failures
        );
        assert!(
            graph.symbols.iter().any(|s| s.name == "Greet"),
            "symbols extracted: {:?}",
            graph.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(report.import_edges, 1, "the in-repo import became an edge");
        assert!(
            report.unresolved_imports.is_empty(),
            "stdlib fmt must not count as a gap: {:?}",
            report.unresolved_imports
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn index_is_deterministic() {
        let root = scratch("det");
        write(&root, "go.mod", "module example.com/d\n");
        for n in 0..12 {
            write(
                &root,
                &format!("pkg{n}/a.go"),
                &format!("package pkg{n}\n\nfunc F{n}() {{}}\n"),
            );
        }
        write(
            &root,
            "main.go",
            "package main\n\nimport (\n\t\"example.com/d/pkg1\"\n\t\"example.com/d/pkg2\"\n)\n\nfunc main() { pkg1.F1(); pkg2.F2() }\n",
        );

        let (g1, _) = index_repo(&root, &IndexOptions::default());
        let (g2, _) = index_repo(&root, &IndexOptions::default());

        let paths = |g: &CodeGraph| {
            g.files
                .iter()
                .map(|f| f.path.to_string())
                .collect::<Vec<_>>()
        };
        let edges = |g: &CodeGraph| {
            g.edges
                .iter()
                .map(|e| (e.from, e.to, e.kind as u8, e.weight))
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&g1), paths(&g2), "file ids must be reproducible");
        assert_eq!(edges(&g1), edges(&g2), "edges must be reproducible");

        std::fs::remove_dir_all(&root).ok();
    }

    /// End-to-end over the five real corpora. This is the number that matters:
    /// not "the resolver works in isolation" but "the whole pipeline produces
    /// a graph with no silent gaps".
    #[test]
    #[ignore = "needs the real corpora; run with --ignored"]
    fn real_corpora_produce_complete_graphs() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let cases = [
            ("Go", ws.join("corpus/go")),
            ("Java", ws.join("corpus/java")),
            ("Rust", ws.join("corpus/rust")),
            ("Python", ws.join("../serena")),
            ("TypeScript", ws.join("../openvisio-oss")),
        ];

        println!(
            "\n{:<11} {:>7} {:>7} {:>8} {:>8} {:>9} {:>8}",
            "语言", "扫描", "解析", "符号", "导入边", "解析失败", "未解析"
        );
        println!("{}", "-".repeat(64));
        let mut worst_fail = 0usize;
        for (lang, root) in &cases {
            if !root.exists() {
                println!("{lang:<11} (语料缺失，跳过)");
                continue;
            }
            let t = std::time::Instant::now();
            let (g, r) = index_repo(root, &IndexOptions::default());
            println!(
                "{:<11} {:>7} {:>7} {:>8} {:>8} {:>9} {:>8}  {:.1}s",
                lang,
                r.files_scanned,
                r.files_parsed,
                g.symbols.len(),
                r.import_edges,
                r.parse_failures.len(),
                r.unresolved_imports.len(),
                t.elapsed().as_secs_f64()
            );
            for (p, why) in r.parse_failures.iter().take(3) {
                println!("      解析失败: {p} — {why}");
            }
            for (p, s) in r.unresolved_imports.iter().take(3) {
                println!("      未解析:   {p} — {s}");
            }
            worst_fail = worst_fail.max(r.parse_failures.len());
        }
        assert_eq!(
            worst_fail, 0,
            "任何语料上出现解析失败都要先查清楚，不能当作正常"
        );
    }

    #[test]
    fn parse_failures_are_reported_not_swallowed() {
        let root = scratch("fail");
        // Valid UTF-8 but not valid Go; tree-sitter recovers, so this should
        // still parse. The point is that whatever happens, nothing is silent.
        write(&root, "broken.go", "package main\nfunc ( { { {\n");
        let (_, report) = index_repo(&root, &IndexOptions::default());
        assert_eq!(
            report.files_scanned, 1,
            "the file was discovered regardless of its contents"
        );
        // A syntax-error file is still parsed (partial symbols kept) AND
        // reported, so incompleteness is never silent.
        assert_eq!(report.files_parsed, 1);
        // Partial parses are in the index, so they are NOT parse failures.
        assert!(
            report.parse_failures.is_empty(),
            "{:?}",
            report.parse_failures
        );
        assert_eq!(
            report.partial_parses.len(),
            1,
            "{:?}",
            report.partial_parses
        );
        assert_eq!(report.partial_parses[0].0.as_str(), "broken.go");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn partial_parse_keeps_symbols_and_survives_incremental_apply() {
        let root = scratch("partial_inc");
        write(&root, "broken.py", "def kept():\n    pass\n\ndef oops(:\n");
        write(&root, "ok.py", "def fine():\n    pass\n");
        let opts = IndexOptions::default();
        let inc = IncrementalIndex::build(&root, &opts);
        assert!(inc.report.parse_failures.is_empty());
        assert_eq!(inc.report.partial_parses.len(), 1);
        assert!(
            inc.graph.symbols.iter().any(|s| s.name == "kept"),
            "symbols of a syntax-error file stay in the index"
        );
        // Touch only the clean file: the partial flag of the other file must
        // not disappear from the report.
        write(&root, "ok.py", "def fine():\n    return 1\n");
        let changes = ChangeSet {
            modified: vec![RelPath::new("ok.py")],
            ..ChangeSet::default()
        };
        let next = index_repo_incremental(&root, &inc, &changes, &opts);
        assert_eq!(next.report.partial_parses.len(), 1, "{:?}", next.report);
        assert_eq!(next.report.partial_parses[0].0.as_str(), "broken.py");
        assert!(next.report.parse_failures.is_empty());
        assert!(next.graph.symbols.iter().any(|s| s.name == "kept"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn persist_can_be_disabled() {
        let root = scratch("no-persist");
        write(&root, "main.go", "package main\nfunc main() {}\n");
        let opts = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        let (_, report) = index_repo(&root, &opts);
        assert_eq!(report.files_parsed, 1);
        assert!(
            !root.join(".astrolabe").exists(),
            "disabled persist must not create a store"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unwritable_store_does_not_fail_the_index() {
        let root = scratch("readonly-store");
        write(&root, "main.go", "package main\nfunc Hello() {}\n");
        // Occupy the store directory name so create_dir_all / Database::create
        // cannot succeed. The index must still return a graph.
        std::fs::write(root.join(".astrolabe"), b"not a directory").unwrap();
        let (graph, report) = index_repo(&root, &IndexOptions::default());
        assert_eq!(report.files_parsed, 1, "parse must not depend on the store");
        assert!(
            report.parse_failures.is_empty(),
            "{:?}",
            report.parse_failures
        );
        assert_eq!(graph.files.len(), 1);
        assert!(
            graph.symbols.iter().any(|s| s.name == "Hello"),
            "symbols extracted without a store: {:?}",
            graph.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_store_directory_does_not_fail_the_index() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("unix-ro-store");
        write(&root, "main.go", "package main\nfunc main() {}\n");
        let dir = root.join(".astrolabe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let (graph, report) = index_repo(&root, &IndexOptions::default());
        assert_eq!(report.files_parsed, 1);
        assert!(report.parse_failures.is_empty());
        assert_eq!(graph.files.len(), 1);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn identical_vue_files_keep_their_own_component_names_across_cache() {
        let root = scratch("vuedup");
        let body = "<template>\n  <el-affix :offset=\"120\" />\n</template>\n";
        write(&root, "docs/examples/affix/basic.vue", body);
        write(&root, "ssr-testing/cases/affix.vue", body);
        let opts = IndexOptions {
            persist: true,
            ..IndexOptions::default()
        };
        for run in 0..2 {
            let (graph, _) = index_repo(&root, &opts);
            for (file, name) in [
                ("docs/examples/affix/basic.vue", "basic"),
                ("ssr-testing/cases/affix.vue", "affix"),
            ] {
                let fid = graph
                    .files
                    .iter()
                    .find(|f| f.path.as_str() == file)
                    .unwrap()
                    .id;
                assert!(
                    graph
                        .symbols
                        .iter()
                        .any(|s| s.file == fid && s.name == name),
                    "run {run}: {file} must expose component `{name}`"
                );
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }

    fn sym(name: &str, start: u32, end: u32) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(0),
            file: FileId(0),
            name: name.to_owned(),
            kind: crate::types::SymbolKind::Function,
            start_line: start,
            end_line: end,
            signature: String::new(),
            exported: false,
        }
    }

    #[test]
    fn enclosing_symbols_sweep_matches_linear_scan() {
        // Deterministic pseudo-random nested/overlapping spans, including
        // equal spans (tie: earliest wins) and a reversed span.
        let mut x = 12345u64;
        let mut next = |m: u32| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((x >> 33) as u32) % m
        };
        let owned: Vec<CodeSymbol> = (0..300)
            .map(|i| {
                let a = next(400);
                let len = next(60);
                sym(&format!("s{i}"), a, a + len)
            })
            .chain(std::iter::once(sym("rev", 50, 10)))
            .collect();
        let syms: Vec<&CodeSymbol> = owned.iter().collect();
        let calls: Vec<(String, u32)> = (0..2000).map(|_| (String::new(), next(480))).collect();
        let fast = enclosing_symbols(&syms, &calls);
        for ((_, line), got) in calls.iter().zip(&fast) {
            let want = syms
                .iter()
                .filter(|s| s.start_line <= *line && *line <= s.end_line)
                .min_by_key(|s| s.end_line - s.start_line)
                .copied();
            assert_eq!(got.map(|s| &s.name), want.map(|s| &s.name), "line {line}");
        }
    }

    #[test]
    fn product_cache_evicts_oldest_generation_first_and_tracks_bytes() {
        let product = |n: usize| CachedProduct {
            language: Language::Go,
            sha: "s".into(),
            loc: 1,
            parsed: Arc::new(ParsedFile::default()),
            weight: n as u32,
        };
        let one = ParseProductCache::entry_bytes(&RelPath::new("a"), &product(100));
        let mut cache = ParseProductCache::new(2 * one, 5);
        cache.insert_at(RelPath::new("a"), product(100), 1);
        cache.insert_at(RelPath::new("b"), product(100), 3);
        cache.insert(RelPath::new("c"), product(100));
        // Replacing an entry does not double count it.
        cache.insert(RelPath::new("c"), product(100));
        assert_eq!(cache.weighted_size(), 3 * one);
        cache.run_pending();
        assert_eq!(cache.weighted_size(), 2 * one);
        assert!(cache.get(&RelPath::new("a")).is_none(), "oldest evicted");
        assert!(cache.get(&RelPath::new("b")).is_some());
        assert_eq!(cache.generation_of(&RelPath::new("c")), Some(5));
    }

    #[test]
    fn split_edge_merge_matches_one_global_sort() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |m: u64| {
            seed = xorshift(seed);
            (seed % m) as u32
        };
        let kinds = [
            EdgeKind::Import,
            EdgeKind::Namespace,
            EdgeKind::Pair,
            EdgeKind::Inherit,
        ];
        let edge = |from, to, kind| CodeEdge {
            from,
            to,
            kind,
            weight: 1,
            confidence: Confidence::Scoped,
        };
        for _ in 0..50 {
            let small: Vec<CodeEdge> = (0..next(40))
                .map(|_| edge(next(30), next(30), kinds[next(4) as usize]))
                .collect();
            // Call edges per "file": `from` ranges ascend across files.
            let mut calls_by_file = Vec::new();
            for file in 0..5u32 {
                let run: Vec<CodeEdge> = (0..next(30))
                    .map(|_| edge(file * 6 + next(6), next(30), EdgeKind::Call))
                    .collect();
                calls_by_file.push(run);
            }
            let mut all: Vec<CodeEdge> = small.clone();
            all.extend(calls_by_file.iter().flatten().cloned());
            all.sort_by_key(|e| (e.from, e.to, e.kind as u8));
            let mut expected: Vec<CodeEdge> = Vec::new();
            for e in all {
                match expected.last_mut() {
                    Some(p) if p.from == e.from && p.to == e.to && p.kind == e.kind => {
                        p.weight += 1
                    }
                    _ => expected.push(e),
                }
            }
            let mut small = small;
            sort_and_merge(&mut small);
            let mut calls = Vec::new();
            for mut run in calls_by_file {
                sort_and_merge(&mut run);
                calls.extend(run);
            }
            let got = merge_sorted_edges(small, calls);
            let key = |e: &CodeEdge| (e.from, e.to, e.kind as u8, e.weight);
            assert_eq!(
                got.iter().map(key).collect::<Vec<_>>(),
                expected.iter().map(key).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn cache_writes_flush_in_bounded_batches() {
        let root = scratch("flush");
        std::fs::create_dir_all(store_path(&root).parent().unwrap()).unwrap();
        let store = Store::open(&store_path(&root)).unwrap();
        let writes = CacheWrites::with_flush_bytes(100);
        for i in 0..50 {
            let key = format!("k{i}");
            writes.push(&store, key, Arc::from(vec![7u8; 30]));
            let pending = writes.pending.lock().unwrap();
            assert!(pending.1 < 100 + 30, "pending bytes stay bounded");
        }
        // Most entries were already written before the final flush.
        assert!(store.parse_cache_len().unwrap() >= 40);
        writes.flush(&store);
        assert_eq!(store.parse_cache_len().unwrap(), 50);
        assert_eq!(writes.stored.load(Ordering::Relaxed), 50);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cold_build_products_match_the_store() {
        let root = scratch("reuse-enc");
        write(&root, "a.py", "def a():\n    return 1\n");
        write(&root, "b.go", "package b\nfunc B() {}\n");
        let inc = IncrementalIndex::build(&root, &IndexOptions::default());
        // The decoded product held in memory is what the store holds.
        let store = Store::open(&store_path(&root)).unwrap();
        for f in &inc.graph.files {
            let key = store::parse_cache_key(f.language.unwrap(), &f.sha);
            let stored = store.cached_parse(&key).unwrap().expect("stored");
            let held = inc.products.get(&f.path).expect("product");
            assert_eq!(
                &stored[..],
                &store::encode_parsed(&held.parsed).unwrap()[..]
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn partial_parse_is_cached_and_still_reported_on_cache_hit() {
        let root = scratch("partial-cache");
        let src = "def kept():\n    pass\n\ndef oops(:\n";
        write(&root, "broken.py", src);
        let (g1, r1) = index_repo(&root, &IndexOptions::default());
        assert_eq!(r1.partial_parses.len(), 1, "{r1:?}");
        let file = &g1.files[0];
        let key = store::parse_cache_key(file.language.unwrap(), &file.sha);
        {
            let store = Store::open(&store_path(&root)).unwrap();
            let bytes = store
                .cached_parse(&key)
                .unwrap()
                .expect("a partial parse is cached like a clean one");
            let cached = store::decode_parsed(&bytes).unwrap();
            assert!(cached.partial, "the partial flag is part of the payload");
            // Mark the entry so a hit is distinguishable from a re-parse.
            let mut marked = cached;
            marked.symbols[0].name = "from_cache".into();
            store
                .put_cached_parse(&key, &store::encode_parsed(&marked).unwrap())
                .unwrap();
        }
        // Second start: served from the cache (stamp or content hash), still partial.
        let (g2, r2) = index_repo(&root, &IndexOptions::default());
        assert!(
            g2.symbols.iter().any(|s| s.name == "from_cache"),
            "not re-parsed"
        );
        assert_eq!(r2.partial_parses.len(), 1, "{r2:?}");
        assert!(r2.parse_failures.is_empty(), "{r2:?}");
        // Same bytes, new mtime (content-hash lookup): same.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&root, "broken.py", src);
        let (g3, r3) = index_repo(&root, &IndexOptions::default());
        assert!(g3.symbols.iter().any(|s| s.name == "from_cache"));
        assert_eq!(r3.partial_parses.len(), 1, "{r3:?}");
        // Incremental build from the cache keeps the flag too.
        let inc = IncrementalIndex::build(&root, &IndexOptions::default());
        assert_eq!(inc.report.partial_parses.len(), 1, "{:?}", inc.report);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn kept_c_family_rewrite_is_reported_and_cached_as_such() {
        let root = scratch("cfam-rewrite");
        let src = "#define B { \\\n  /* c */ \\\n  0 \\\n}\nint ok(void);\nint bad( { ;\n";
        write(&root, "a.c", src);
        for run in 0..2 {
            let (_, report) = index_repo(&root, &IndexOptions::default());
            assert_eq!(
                report.partial_parses,
                vec![(
                    RelPath::new("a.c"),
                    crate::PARTIAL_PARSE_REWRITE_REASON.to_owned()
                )],
                "run {run}"
            );
        }
        let inc = IncrementalIndex::build(&root, &IndexOptions::default());
        assert_eq!(
            inc.report.partial_parses[0].1,
            crate::PARTIAL_PARSE_REWRITE_REASON
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn previous_extractor_version_cache_is_never_served() {
        let root = scratch("extractor-bump");
        write(&root, "a.py", "def real():\n    pass\n");
        let (g1, _) = index_repo(&root, &IndexOptions::default());
        let file = &g1.files[0];
        let key = store::parse_cache_key(file.language.unwrap(), &file.sha);
        let old_key = key.replacen(
            &format!("x{}-", store::EXTRACTOR_VERSION),
            &format!("x{}-", store::EXTRACTOR_VERSION - 1),
            1,
        );
        assert_ne!(old_key, key);
        {
            // Simulate a cache written by the previous extractor: a stale
            // payload under the old key and no entry under the current one.
            // (A stamp naming an old-prefix key is rejected by `stamp_hit`.)
            let store = Store::open(&store_path(&root)).unwrap();
            let mut stale =
                store::decode_parsed(&store.cached_parse(&key).unwrap().unwrap()).unwrap();
            stale.symbols[0].name = "stale".into();
            store
                .put_cached_parse(&old_key, &store::encode_parsed(&stale).unwrap())
                .unwrap();
            store
                .prune_parse_cache(
                    &HashSet::from([old_key.clone()]),
                    &HashSet::from(["a.py".to_owned()]),
                )
                .unwrap();
            assert!(store.cached_parse(&key).unwrap().is_none());
        }
        let (g2, _) = index_repo(&root, &IndexOptions::default());
        assert!(g2.symbols.iter().any(|s| s.name == "real"), "re-parsed");
        assert!(!g2.symbols.iter().any(|s| s.name == "stale"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn persist_writes_parse_cache_keyed_by_sha() {
        let root = scratch("cache-write");
        write(&root, "lib.go", "package lib\nfunc F() {}\n");
        let (graph, _) = index_repo(&root, &IndexOptions::default());
        let file = &graph.files[0];
        let store = Store::open(&store_path(&root)).expect("store created on first index");
        let key = store::parse_cache_key(file.language.unwrap(), &file.sha);
        assert!(
            store.cached_parse(&key).unwrap().is_some(),
            "freshly parsed file must land in the parse cache"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cached_second_index_matches_cold_index() {
        let root = scratch("cache-hit");
        write(&root, "go.mod", "module example.com/c\n");
        write(&root, "a.go", "package c\nfunc A() {}\n");
        write(&root, "b.go", "package c\nfunc B() { A() }\n");

        let (g1, r1) = index_repo(&root, &IndexOptions::default());
        let (g2, r2) = index_repo(&root, &IndexOptions::default());

        assert_eq!(r1.files_parsed, r2.files_parsed);
        assert_eq!(
            g1.files
                .iter()
                .map(|f| (f.path.to_string(), f.sha.clone(), f.id.0))
                .collect::<Vec<_>>(),
            g2.files
                .iter()
                .map(|f| (f.path.to_string(), f.sha.clone(), f.id.0))
                .collect::<Vec<_>>(),
            "cache hits must not shuffle FileId assignment"
        );
        let symbols = |g: &CodeGraph| {
            g.symbols
                .iter()
                .map(|s| (s.id.0, s.file.0, s.name.clone(), s.start_line, s.end_line))
                .collect::<Vec<_>>()
        };
        assert_eq!(symbols(&g1), symbols(&g2));
        let edges = |g: &CodeGraph| {
            g.edges
                .iter()
                .map(|e| (e.from, e.to, e.kind as u8, e.weight))
                .collect::<Vec<_>>()
        };
        assert_eq!(edges(&g1), edges(&g2));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn call_edges_skips_ubiquitous_names_exceeding_threshold() {
        let root = scratch("ubiquitous");
        write(&root, "go.mod", "module example.com/ubiq\n");
        // Create files defining more than MAX_SYNTACTIC_CALL_TARGETS functions named "New"
        let mut defs = String::from("package ubiq\n");
        for n in 0..=(MAX_SYNTACTIC_CALL_TARGETS + 5) {
            defs.push_str(&format!("func New{n}() {{}}\n"));
        }
        write(&root, "defs.go", &defs);

        // Many functions with the exact same name "Init"
        let mut inits = String::from("package ubiq\n");
        for n in 0..=(MAX_SYNTACTIC_CALL_TARGETS + 5) {
            inits.push_str(&format!(
                "type S{n} struct {{}}\nfunc (s S{n}) Init() {{}}\n"
            ));
        }
        write(&root, "inits.go", &inits);

        // Caller calling Init() and Rare()
        write(
            &root,
            "caller.go",
            "package ubiq\nfunc Rare() {}\nfunc Call() { Init(); Rare(); }\n",
        );

        let opts = IndexOptions {
            call_edges: true,
            persist: false,
            ..IndexOptions::default()
        };
        let (g, _) = index_repo(&root, &opts);

        let call_edges: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Call)
            .collect();

        // Target for "Init" has > MAX_SYNTACTIC_CALL_TARGETS symbols, so it must be skipped.
        // But "Rare" has only 1 target, so its call edge should be generated.
        let rare_sym = g.symbols.iter().find(|s| s.name == "Rare").unwrap();
        assert!(
            call_edges.iter().any(|e| e.to == rare_sym.id.0),
            "Rare call edge must exist"
        );

        let init_sym_ids: std::collections::HashSet<u32> = g
            .symbols
            .iter()
            .filter(|s| s.name == "Init")
            .map(|s| s.id.0)
            .collect();
        assert_eq!(init_sym_ids.len(), MAX_SYNTACTIC_CALL_TARGETS + 6);
        assert!(
            !call_edges.iter().any(|e| init_sym_ids.contains(&e.to)),
            "Ubiquitous Init calls must be skipped"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    fn no_persist() -> IndexOptions {
        IndexOptions {
            persist: false,
            ..IndexOptions::default()
        }
    }

    fn with_calls() -> IndexOptions {
        IndexOptions {
            persist: false,
            call_edges: true,
            ..IndexOptions::default()
        }
    }

    type FileSnap = (u32, String, Option<Language>, u32, String);
    type SymbolSnap = (u32, u32, String, u8, String, u32, u32, bool);
    type EdgeSnap = (u32, u32, u8, u32, u8);

    fn graph_snapshot(g: &CodeGraph) -> (Vec<FileSnap>, Vec<SymbolSnap>, Vec<EdgeSnap>) {
        let files = g
            .files
            .iter()
            .map(|f| (f.id.0, f.path.to_string(), f.language, f.loc, f.sha.clone()))
            .collect();
        let symbols = g
            .symbols
            .iter()
            .map(|s| {
                (
                    s.id.0,
                    s.file.0,
                    s.name.clone(),
                    s.kind as u8,
                    s.signature.clone(),
                    s.start_line,
                    s.end_line,
                    s.exported,
                )
            })
            .collect();
        let edges = g
            .edges
            .iter()
            .map(|e| (e.from, e.to, e.kind as u8, e.weight, e.confidence as u8))
            .collect();
        (files, symbols, edges)
    }

    fn assert_graphs_equivalent(a: &CodeGraph, b: &CodeGraph) {
        assert_eq!(graph_snapshot(a), graph_snapshot(b));
    }

    fn seed_go_repo(root: &Path) {
        write(root, "go.mod", "module example.com/inc\n");
        write(
            root,
            "a.go",
            "package inc\n\nfunc A() {}\nfunc Shared() {}\n",
        );
        write(root, "b.go", "package inc\n\nfunc B() { A(); Shared() }\n");
        write(root, "c.go", "package inc\n\nfunc C() { B() }\n");
        write(
            root,
            "util/u.go",
            "package util\n\nfunc U() string { return \"u\" }\n",
        );
    }

    #[test]
    fn incremental_build_matches_index_repo() {
        let root = scratch("inc-build");
        seed_go_repo(&root);
        let opts = with_calls();
        let (g1, r1) = index_repo(&root, &opts);
        let inc = IncrementalIndex::build(&root, &opts);
        assert_graphs_equivalent(&g1, &inc.graph);
        assert_eq!(r1.files_parsed, inc.report.files_parsed);
        assert_eq!(r1.import_edges, inc.report.import_edges);
        assert_eq!(r1.symbols, inc.report.symbols);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_changeset_reuses_products_and_matches_full() {
        let root = scratch("inc-empty");
        seed_go_repo(&root);
        let opts = with_calls();
        let inc = IncrementalIndex::build(&root, &opts);
        let next = inc.apply_changeset(&root, &ChangeSet::default(), &opts);
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&inc.graph, &next.graph);
        assert_graphs_equivalent(&full, &next.graph);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ts_out_dir_dot_keeps_sources_indexed_after_edit() {
        // zod: packages/zod/tsconfig.build.json has `"outDir": "."`.
        let root = scratch("ts-outdir-dot");
        write(
            &root,
            "packages/zod/tsconfig.build.json",
            r#"{"compilerOptions": {"outDir": "."}}"#,
        );
        write(
            &root,
            "packages/zod/src/v4/classic/errors.ts",
            "export class ZodError extends Error {}\n",
        );
        let opts = with_calls();
        let inc = IncrementalIndex::build(&root, &opts);
        assert!(
            inc.graph.symbols.iter().any(|s| s.name == "ZodError"),
            "outDir \".\" must not drop the package's sources"
        );
        assert!(inc.excluded_source_paths().is_empty());

        // Edit then query: the new symbol is visible after the changeset.
        write(
            &root,
            "packages/zod/src/v4/classic/errors.ts",
            "export class ZodError extends Error {}\nexport class ZodRealError extends ZodError {}\n",
        );
        let inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("packages/zod/src/v4/classic/errors.ts")],
                ..ChangeSet::default()
            },
            &opts,
        );
        assert!(inc.graph.symbols.iter().any(|s| s.name == "ZodRealError"));
        assert!(inc.excluded_source_paths().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dropped_sources_reports_only_authored_ts() {
        let before = vec![
            RelPath::new("app/dist/main.js"),
            RelPath::new("app/dist/main.d.ts"),
            RelPath::new("app/dist/leak.ts"),
            RelPath::new("app/src/main.ts"),
        ];
        let after = vec![RelPath::new("app/src/main.ts")];
        assert_eq!(
            dropped_sources(&before, &after),
            vec![RelPath::new("app/dist/leak.ts")]
        );
    }

    #[test]
    fn incremental_add_modify_remove_match_full_index() {
        let root = scratch("inc-amr");
        seed_go_repo(&root);
        let opts = with_calls();
        let mut inc = IncrementalIndex::build(&root, &opts);

        write(&root, "d.go", "package inc\n\nfunc D() { A() }\n");
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                added: vec![RelPath::new("d.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);

        write(
            &root,
            "a.go",
            "package inc\n\nimport \"example.com/inc/util\"\n\nfunc A() { util.U() }\nfunc Shared() {}\n",
        );
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("a.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);
        assert!(
            inc.graph.edges.iter().any(|e| e.kind == EdgeKind::Import),
            "import from a.go to util should resolve"
        );

        std::fs::remove_file(root.join("c.go")).unwrap();
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                removed: vec![RelPath::new("c.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);
        assert!(
            inc.graph.files.iter().all(|f| f.path.as_str() != "c.go"),
            "removed file must leave the graph"
        );

        // Adding a path that sorts between existing FileIds must remap ids
        // the same way a full index does.
        write(&root, "ab.go", "package inc\n\nfunc AB() {}\n");
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                added: vec![RelPath::new("ab.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extensionless_shebang_scripts_are_indexed_full_and_incremental() {
        let root = scratch("shebang");
        write(
            &root,
            "bin/tool",
            "#!/usr/bin/env bash\nsource \"$(dirname \"$0\")/../lib/util.sh\"\nf() { :; }\n",
        );
        write(&root, "lib/util.sh", "u() { :; }\n");
        write(&root, "LICENSE", "MIT License\n");
        write(&root, "notes.txt", "#!/bin/bash\nnot_a_script() { :; }\n");
        let opts = IndexOptions::default();
        let has = |graph: &CodeGraph, file: &str, name: &str| {
            graph.symbols.iter().any(|s| {
                s.name == name
                    && graph
                        .files
                        .iter()
                        .any(|f| f.id == s.file && f.path.as_str() == file)
            })
        };

        let (full, report) = index_repo(&root, &opts);
        assert!(has(&full, "bin/tool", "f"), "shebang script must be parsed");
        assert!(
            full.edges.iter().any(|e| e.kind == EdgeKind::Import),
            "`source` in an extension-less script must resolve"
        );
        assert!(
            full.symbols.iter().all(|s| s.name != "not_a_script"),
            "files with an unknown extension are never sniffed"
        );
        assert!(
            report.parse_failures.is_empty(),
            "non-script files are skipped, not failures: {:?}",
            report.parse_failures
        );

        let mut inc = IncrementalIndex::build(&root, &opts);
        assert!(has(&inc.graph, "bin/tool", "f"));
        write(&root, "a.go", "package a\n\nfunc A() {}\n");
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                added: vec![RelPath::new("a.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        assert!(
            has(&inc.graph, "bin/tool", "f"),
            "an unchanged shebang script must survive an incremental run"
        );
        write(&root, "bin/tool", "#!/bin/sh\ng() { :; }\n");
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("bin/tool")],
                ..ChangeSet::default()
            },
            &opts,
        );
        assert!(has(&inc.graph, "bin/tool", "g"));
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn incremental_does_not_reread_unchanged_source_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("inc-noread");
        seed_go_repo(&root);
        let opts = no_persist();
        let inc = IncrementalIndex::build(&root, &opts);

        let a = root.join("a.go");
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();

        write(
            &root,
            "b.go",
            "package inc\n\nfunc B() { Shared() }\nfunc B2() {}\n",
        );
        let next = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("b.go")],
                ..ChangeSet::default()
            },
            &opts,
        );

        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            next.graph.files.iter().any(|f| f.path.as_str() == "a.go"),
            "unchanged a.go must be reused from the parse-product cache, not re-read"
        );
        assert!(
            next.graph.symbols.iter().any(|s| s.name == "A"),
            "symbols from the unreadable-on-disk file must survive"
        );
        assert!(
            next.graph.symbols.iter().any(|s| s.name == "B2"),
            "modified b.go must be re-parsed"
        );
        assert!(
            next.report.parse_failures.is_empty(),
            "re-reading a.go would have failed: {:?}",
            next.report.parse_failures
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn incremental_keeps_stale_product_when_changeset_omits_a_write() {
        let root = scratch("inc-stale");
        seed_go_repo(&root);
        let opts = no_persist();
        let inc = IncrementalIndex::build(&root, &opts);
        let old_sha = inc
            .graph
            .files
            .iter()
            .find(|f| f.path.as_str() == "a.go")
            .unwrap()
            .sha
            .clone();

        write(&root, "a.go", "package inc\n\nfunc ARenamed() {}\n");
        write(
            &root,
            "b.go",
            "package inc\n\nfunc B() {}\nfunc Extra() {}\n",
        );
        let next = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("b.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let a = next
            .graph
            .files
            .iter()
            .find(|f| f.path.as_str() == "a.go")
            .unwrap();
        assert_eq!(
            a.sha, old_sha,
            "a.go was not in the changeset, so the cached product must win"
        );
        assert!(next.graph.symbols.iter().any(|s| s.name == "A"));
        assert!(!next.graph.symbols.iter().any(|s| s.name == "ARenamed"));
        assert!(next.graph.symbols.iter().any(|s| s.name == "Extra"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lru_eviction_falls_back_to_reparse_and_stays_equivalent() {
        let root = scratch("inc-lru");
        write(&root, "go.mod", "module example.com/lru\n");
        for n in 0..12 {
            write(
                &root,
                &format!("f{n}.go"),
                &format!("package lru\n\nfunc F{n}() {{}}\n"),
            );
        }
        let tiny = IndexOptions {
            persist: false,
            call_edges: true,
            parse_cache_bytes: 64,
            ..IndexOptions::default()
        };
        let inc = IncrementalIndex::build(&root, &tiny);
        write(
            &root,
            "f0.go",
            "package lru\n\nfunc F0() {}\nfunc Extra() {}\n",
        );
        let next = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("f0.go")],
                ..ChangeSet::default()
            },
            &tiny,
        );
        let (full, _) = index_repo(&root, &tiny);
        assert_graphs_equivalent(&full, &next.graph);
        // moka admits an entry even when its weight exceeds max_capacity, then
        // evicts asynchronously. After run_pending the cache must not grow with
        // the file count — 12 files at an unbounded cache would be far larger.
        assert!(
            next.product_bytes() < 8 * 1024,
            "LRU must stay bounded, got {}",
            next.product_bytes()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Property: fixture → full incremental build → random add/mod/del →
    /// apply_changeset ≡ a fresh [`index_repo`] of the same tree.
    /// FileIds, symbols, and edges must match (determinism of assemble).
    #[test]
    fn incremental_matches_full_index_after_random_edits() {
        for seed in 1u64..=20 {
            incremental_random_edits_once(seed, false);
            incremental_random_edits_once(seed.wrapping_mul(0x9e37), true);
        }
    }

    fn incremental_random_edits_once(seed: u64, call_edges: bool) {
        let root = scratch(&format!("inc-prop-{seed}-{call_edges}"));
        seed_go_repo(&root);
        let opts = IndexOptions {
            persist: false,
            call_edges,
            ..IndexOptions::default()
        };
        let mut inc = IncrementalIndex::build(&root, &opts);
        let mut rng = seed;
        let mut extra: Vec<String> = Vec::new();
        let mut next_id = 0u32;

        for step in 0..12 {
            rng = xorshift(rng);
            let op = (rng % 3) as u8;
            let changes = match op {
                0 => {
                    let name = format!("gen{next_id}.go");
                    next_id += 1;
                    write(
                        &root,
                        &name,
                        &format!("package inc\n\nfunc Gen{next_id}() {{ A(); Shared() }}\n"),
                    );
                    extra.push(name.clone());
                    ChangeSet {
                        added: vec![RelPath::new(name)],
                        ..ChangeSet::default()
                    }
                }
                1 => {
                    let target = if extra.is_empty() || rng.is_multiple_of(2) {
                        "b.go".to_string()
                    } else {
                        extra[(rng as usize) % extra.len()].clone()
                    };
                    write(
                        &root,
                        &target,
                        &format!(
                            "package inc\n\nfunc Step{step}() {{}}\nfunc SharedStep{step}() {{ Step{step}() }}\n"
                        ),
                    );
                    ChangeSet {
                        modified: vec![RelPath::new(target)],
                        ..ChangeSet::default()
                    }
                }
                _ => {
                    if extra.len() < 2 {
                        write(
                            &root,
                            "c.go",
                            &format!("package inc\n\nfunc C{step}() {{}}\n"),
                        );
                        ChangeSet {
                            modified: vec![RelPath::new("c.go")],
                            ..ChangeSet::default()
                        }
                    } else {
                        let idx = (rng as usize) % extra.len();
                        let name = extra.remove(idx);
                        std::fs::remove_file(root.join(&name)).unwrap();
                        ChangeSet {
                            removed: vec![RelPath::new(name)],
                            ..ChangeSet::default()
                        }
                    }
                }
            };

            inc = inc.apply_changeset(&root, &changes, &opts);
            let (full, full_report) = index_repo(&root, &opts);
            assert_graphs_equivalent(&full, &inc.graph);
            assert_eq!(
                full_report.files_parsed, inc.report.files_parsed,
                "seed={seed} step={step} call_edges={call_edges}"
            );
            assert_eq!(full_report.import_edges, inc.report.import_edges);
            assert_eq!(full_report.symbols, inc.report.symbols);
        }

        std::fs::remove_dir_all(&root).ok();
    }

    fn xorshift(mut x: u64) -> u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }

    #[test]
    fn test_unindexed_code_exts_basic() {
        let admitted = vec![
            RelPath::new("src/main.rs"),     // supported (Rust) -> skipped
            RelPath::new("scripts/run.ex"),  // not supported, in KNOWN -> counted
            RelPath::new("scripts/test.ex"), // not supported, in KNOWN -> counted
            RelPath::new("src/App.hs"),      // not supported, in KNOWN -> counted
            RelPath::new("data/info.json"),  // data file, removed from KNOWN -> skipped
            RelPath::new("docs/readme.txt"), // txt not in KNOWN -> skipped
            RelPath::new("LICENSE"),         // no ext -> skipped
            RelPath::new("src/lib.rs"),      // in_graph -> skipped
        ];

        let in_graph = |p: &str| p == "src/lib.rs";
        let counts = unindexed_code_exts(&admitted, &in_graph, None);
        assert_eq!(counts.get("ex"), Some(&2));
        assert_eq!(counts.get("hs"), Some(&1)); // .hs 仍被统计
        assert_eq!(counts.get("json"), None); // .json 数据文件不再被统计
        assert_eq!(counts.get("rs"), None);
        assert_eq!(counts.get("txt"), None);
    }

    #[test]
    fn test_unindexed_code_exts_with_filter() {
        let admitted = vec![
            RelPath::new("backend/script.zig"),
            RelPath::new("frontend/script.zig"),
            RelPath::new("backend/service.scala"),
        ];
        let in_graph = |_p: &str| false;
        let counts = unindexed_code_exts(&admitted, &in_graph, Some("backend"));
        assert_eq!(counts.get("zig"), Some(&1));
        assert_eq!(counts.get("scala"), Some(&1));

        let counts_fe = unindexed_code_exts(&admitted, &in_graph, Some("frontend"));
        assert_eq!(counts_fe.get("zig"), Some(&1));
        assert_eq!(counts_fe.get("scala"), None);
    }

    #[test]
    fn test_unindexed_code_exts_case_insensitive_and_vocabulary() {
        let admitted = vec![
            RelPath::new("build/deploy.HS"),
            RelPath::new("model.SCALA"),
            RelPath::new("unknown.xyz123"),
        ];
        let in_graph = |_p: &str| false;
        let counts = unindexed_code_exts(&admitted, &in_graph, None);
        assert_eq!(counts.get("hs"), Some(&1));
        assert_eq!(counts.get("scala"), Some(&1));
        assert_eq!(counts.get("xyz123"), None);
    }

    #[test]
    fn csx_is_classified_not_unindexed() {
        let admitted = vec![
            RelPath::new("scripts/main.csx"),
            RelPath::new("lib/bin/Hidden.csx"),
        ];
        let in_graph = |_p: &str| false;
        let counts = unindexed_code_exts(&admitted, &in_graph, None);
        assert!(
            !counts.contains_key("csx"),
            ".csx is C#, not an unsupported extension: {counts:?}"
        );
        assert_eq!(
            Language::from_path(&RelPath::new("scripts/main.csx")),
            Some(Language::CSharp)
        );
    }
}

#[cfg(test)]
mod persist_audit_tests {
    //! Regression tests for the persistent-cache / incremental-size /
    //! transient-failure audit findings (scan-index-store #3 #4 #5 #7 #9 #11).
    use super::*;
    use crate::store::{self, Store};

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astrolabe-persist-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_old(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        age(&p);
    }

    /// Push mtime far enough back that the stamp logic trusts it.
    fn age(path: &Path) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
    }

    fn names(inc: &IncrementalIndex) -> Vec<String> {
        inc.graph.symbols.iter().map(|s| s.name.clone()).collect()
    }

    fn stored_entries(root: &Path) -> u64 {
        Store::open_read_only(&store_path(root))
            .unwrap()
            .expect("store exists")
            .parse_cache_len()
            .unwrap()
    }

    #[test]
    fn repeated_edits_do_not_grow_the_parse_cache() {
        let root = scratch("prune");
        write_old(&root, "keep.py", "def keep():\n    pass\n");
        write_old(&root, "edit.py", "def v0():\n    pass\n");
        let opts = IndexOptions::default();
        IncrementalIndex::build(&root, &opts);
        assert_eq!(stored_entries(&root), 2);
        for i in 1..=8 {
            write_old(&root, "edit.py", &format!("def v{i}():\n    pass\n"));
            let inc = IncrementalIndex::build(&root, &opts);
            assert!(names(&inc).contains(&format!("v{i}")));
        }
        // Before the fix every edit left one more dead entry (2 + 8).
        assert_eq!(stored_entries(&root), 2, "stale entries must be pruned");
    }

    #[test]
    fn entries_from_other_versions_are_pruned() {
        let root = scratch("prune-ver");
        write_old(&root, "a.py", "def a():\n    pass\n");
        let opts = IndexOptions::default();
        IncrementalIndex::build(&root, &opts);
        {
            let s = Store::open(&store_path(&root)).unwrap();
            s.put_cached_parse("x0-v0.0.1:python:deadbeef", b"{}")
                .unwrap();
            assert_eq!(s.parse_cache_len().unwrap(), 2);
        }
        IncrementalIndex::build(&root, &opts);
        assert_eq!(stored_entries(&root), 1);
    }

    #[test]
    fn upgrade_prune_compacts_the_store_file() {
        let root = scratch("prune-compact");
        write_old(&root, "a.py", "def a():\n    pass\n");
        let opts = IndexOptions::default();
        IncrementalIndex::build(&root, &opts);
        {
            // A previous format's generation: many dead entries at once.
            let s = Store::open(&store_path(&root)).unwrap();
            let junk = vec![b'j'; 8 * 1024];
            let keys: Vec<String> = (0..COMPACT_MIN_CHURN + 50)
                .map(|i| format!("x6-v0.15.0:python:{i:016x}"))
                .collect();
            s.put_cached_parses(keys.iter().map(|k| (k.as_str(), junk.as_slice())))
                .unwrap();
        }
        let bloated = std::fs::metadata(store_path(&root)).unwrap().len();
        IncrementalIndex::build(&root, &opts);
        assert_eq!(stored_entries(&root), 1);
        let compacted = std::fs::metadata(store_path(&root)).unwrap().len();
        assert!(
            compacted * 2 < bloated,
            "file must shrink after the upgrade prune: {bloated} -> {compacted}"
        );
        // Still a working cache.
        let again = IncrementalIndex::build(&root, &opts);
        assert!(again.graph.symbols.iter().any(|s| s.name == "a"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn incremental_apply_never_prunes_unrelated_entries() {
        let root = scratch("prune-inc");
        write_old(&root, "a.py", "def a():\n    pass\n");
        write_old(&root, "b.py", "def b():\n    pass\n");
        let opts = IndexOptions::default();
        let inc = IncrementalIndex::build(&root, &opts);
        write_old(&root, "a.py", "def a2():\n    pass\n");
        let changes = ChangeSet {
            added: vec![],
            modified: vec![RelPath::new("a.py")],
            removed: vec![],
        };
        let inc = inc.apply_changeset(&root, &changes, &opts);
        assert!(names(&inc).contains(&"a2".to_string()));
        // b.py's entry survives (a's old one stays until the next cold build).
        let s = Store::open_read_only(&store_path(&root)).unwrap().unwrap();
        let (_, sha) = {
            let src = std::fs::read_to_string(root.join("b.py")).unwrap();
            let (sha, loc) = digest(&src);
            (loc, sha)
        };
        assert!(s
            .cached_parse(&store::parse_cache_key(Language::Python, &sha))
            .unwrap()
            .is_some());
    }

    /// Replace the cached payload for `rel` with one that names `decoy`, so a
    /// test can see whether the cache (not the source) produced the symbols.
    fn plant_decoy(root: &Path, rel: &str, decoy: &str) {
        let src = std::fs::read_to_string(root.join(rel)).unwrap();
        let (sha, _) = digest(&src);
        let key = store::parse_cache_key(Language::Python, &sha);
        let parsed = ParsedFile {
            symbols: vec![CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: decoy.into(),
                kind: crate::types::SymbolKind::Function,
                signature: String::new(),
                start_line: 1,
                end_line: 1,
                exported: true,
            }],
            ..ParsedFile::default()
        };
        let bytes = store::encode_parsed(&parsed).unwrap();
        Store::open(&store_path(root))
            .unwrap()
            .put_cached_parse(&key, &bytes)
            .unwrap();
    }

    #[test]
    fn follower_reads_the_cache_without_writing_it() {
        let root = scratch("follower");
        write_old(&root, "a.py", "def real():\n    pass\n");
        IncrementalIndex::build(&root, &IndexOptions::default());
        plant_decoy(&root, "a.py", "from_cache");
        let before = std::fs::metadata(store_path(&root))
            .unwrap()
            .modified()
            .unwrap();

        let follower = IndexOptions {
            persist: false,
            persist_read_only: true,
            ..IndexOptions::default()
        };
        let inc = IncrementalIndex::build(&root, &follower);
        assert_eq!(names(&inc), vec!["from_cache".to_string()]);

        let off = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        assert_eq!(
            names(&IncrementalIndex::build(&root, &off)),
            vec!["real".to_string()],
            "persist=false without read-only must not touch the cache"
        );
        let after = std::fs::metadata(store_path(&root))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "read-only use must not modify the store");
    }

    #[test]
    fn follower_does_not_create_a_missing_store() {
        let root = scratch("follower-none");
        write_old(&root, "a.py", "def real():\n    pass\n");
        let follower = IndexOptions {
            persist: false,
            persist_read_only: true,
            ..IndexOptions::default()
        };
        let inc = IncrementalIndex::build(&root, &follower);
        assert_eq!(names(&inc), vec!["real".to_string()]);
        assert!(!root.join(".astrolabe").exists());
    }

    #[test]
    fn follower_ignores_a_writer_holding_the_lock() {
        let root = scratch("follower-locked");
        write_old(&root, "a.py", "def real():\n    pass\n");
        IncrementalIndex::build(&root, &IndexOptions::default());
        let _writer = Store::open(&store_path(&root)).unwrap();
        let follower = IndexOptions {
            persist: false,
            persist_read_only: true,
            ..IndexOptions::default()
        };
        let inc = IncrementalIndex::build(&root, &follower);
        assert_eq!(names(&inc), vec!["real".to_string()]);
    }

    #[test]
    fn unchanged_stamp_skips_reading_but_any_change_reparses() {
        let root = scratch("stamp");
        write_old(&root, "a.py", "def real():\n    pass\n");
        let opts = IndexOptions::default();
        IncrementalIndex::build(&root, &opts);
        // Point the stamp at a decoy entry that the file's content hash does
        // NOT lead to: only a run that skips reading can return the decoy.
        let decoy_key = format!("{}python:decoy", store::parse_cache_key_prefix());
        {
            let parsed = ParsedFile {
                symbols: vec![CodeSymbol {
                    id: SymbolId(0),
                    file: FileId(0),
                    name: "from_stamp".into(),
                    kind: crate::types::SymbolKind::Function,
                    signature: String::new(),
                    start_line: 1,
                    end_line: 1,
                    exported: true,
                }],
                ..ParsedFile::default()
            };
            let s = Store::open(&store_path(&root)).unwrap();
            s.put_cached_parse(&decoy_key, &store::encode_parsed(&parsed).unwrap())
                .unwrap();
            let mut stamp = s.file_stamp("a.py").unwrap().expect("stamp recorded");
            stamp.key = decoy_key.clone();
            s.put_file_stamps(&[("a.py".to_string(), stamp)]).unwrap();
        }
        let inc = IncrementalIndex::build(&root, &opts);
        assert_eq!(names(&inc), vec!["from_stamp".to_string()]);
        // The stamp entry was already live, so nothing was re-stamped to real.
        // New length -> full read + reparse.
        write_old(&root, "a.py", "def real_longer():\n    pass\n");
        let inc = IncrementalIndex::build(&root, &opts);
        assert_eq!(names(&inc), vec!["real_longer".to_string()]);
        // Same length, different mtime -> reparse. Re-plant the decoy stamp
        // for the new state first so a wrong skip would be visible.
        {
            let s = Store::open(&store_path(&root)).unwrap();
            let mut stamp = s.file_stamp("a.py").unwrap().expect("stamp recorded");
            stamp.key = decoy_key.clone();
            s.put_cached_parse(
                &decoy_key,
                &store::encode_parsed(&ParsedFile::default()).unwrap(),
            )
            .unwrap();
            s.put_file_stamps(&[("a.py".to_string(), stamp)]).unwrap();
        }
        write_old(&root, "a.py", "def real_longeR():\n    pass\n");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("a.py"))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(10))
            .unwrap();
        let inc = IncrementalIndex::build(&root, &opts);
        assert_eq!(names(&inc), vec!["real_longeR".to_string()]);
    }

    #[test]
    fn young_files_are_never_stamped() {
        let root = scratch("stamp-young");
        // Not aged: mtime is "now", so a same-size rewrite could share it.
        std::fs::write(root.join("a.py"), "def real():\n    pass\n").unwrap();
        IncrementalIndex::build(&root, &IndexOptions::default());
        let s = Store::open(&store_path(&root)).unwrap();
        assert!(s.file_stamp("a.py").unwrap().is_none());
    }

    #[test]
    fn incremental_edit_past_the_size_cap_matches_a_cold_scan() {
        let root = scratch("toolarge");
        write_old(&root, "a.py", "def small():\n    pass\n");
        write_old(&root, "b.py", "def other():\n    pass\n");
        let opts = IndexOptions {
            persist: false,
            scan: ScanOptions {
                max_file_bytes: 200,
                ..ScanOptions::default()
            },
            ..IndexOptions::default()
        };
        let inc = IncrementalIndex::build(&root, &opts);
        let big: String = (0..40)
            .map(|i| format!("def f{i}():\n    pass\n"))
            .collect();
        assert!(big.len() > 200);
        write_old(&root, "a.py", &big);
        let changes = ChangeSet {
            added: vec![],
            modified: vec![RelPath::new("a.py")],
            removed: vec![],
        };
        let inc2 = inc.apply_changeset(&root, &changes, &opts);
        assert!(
            !names(&inc2).iter().any(|n| n.starts_with('f')),
            "oversized file must not enter the graph: {:?}",
            names(&inc2)
        );
        assert_eq!(inc2.report.too_large, vec![RelPath::new("a.py")]);
        let cold = IncrementalIndex::build(&root, &opts);
        assert_eq!(cold.report.too_large, inc2.report.too_large);
        assert_eq!(names(&cold), names(&inc2));

        // Shrinking it again brings it back and clears the report.
        write_old(&root, "a.py", "def small2():\n    pass\n");
        let inc3 = inc2.apply_changeset(&root, &changes, &opts);
        assert!(names(&inc3).contains(&"small2".to_string()));
        assert!(inc3.report.too_large.is_empty());
    }

    fn with_failure(root: &Path, opts: &IndexOptions, reason: &str, tries: u8) -> IncrementalIndex {
        let mut prev = IncrementalIndex::build(root, opts);
        // Pretend the last apply failed on a.py and kept no product for it.
        prev.products = ParseProductCache::new(1 << 20, 0);
        prev.failures = vec![(RelPath::new("a.py"), reason.to_owned())];
        prev.failure_retries.insert(RelPath::new("a.py"), tries);
        prev
    }

    fn noop() -> ChangeSet {
        ChangeSet {
            added: vec![],
            modified: vec![],
            removed: vec![],
        }
    }

    #[test]
    fn transient_failure_is_retried_on_the_next_apply() {
        let root = scratch("retry");
        write_old(&root, "a.py", "def real():\n    pass\n");
        let opts = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        for reason in ["read: interrupted", "parse exceeded its budget"] {
            let prev = with_failure(&root, &opts, reason, 0);
            let next = prev.apply_changeset(&root, &noop(), &opts);
            assert!(next.failures.is_empty(), "{reason}: {:?}", next.failures);
            assert!(names(&next).contains(&"real".to_string()));
        }
    }

    #[test]
    fn permanent_or_exhausted_failures_stay_sticky() {
        let root = scratch("retry-sticky");
        write_old(&root, "a.py", "def real():\n    pass\n");
        let opts = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        let prev = with_failure(&root, &opts, "query error: boom", 0);
        let next = prev.apply_changeset(&root, &noop(), &opts);
        assert_eq!(next.failures.len(), 1, "non-transient failure not retried");
        let prev = with_failure(&root, &opts, "read: busy", MAX_FAILURE_RETRIES);
        let next = prev.apply_changeset(&root, &noop(), &opts);
        assert_eq!(next.failures.len(), 1, "retries are capped");
    }
}

#[cfg(test)]
mod incremental_resolve_tests {
    //! Incremental apply must equal a cold build while reusing resolver
    //! metadata and per-file import resolutions (audit scan-index-store #1).
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astrolabe-incres-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
    }

    fn opts() -> IndexOptions {
        IndexOptions {
            persist: false,
            ..IndexOptions::default()
        }
    }

    type Fingerprint = (
        Vec<String>,
        Vec<String>,
        Vec<(u32, u32, u8, u32)>,
        Vec<(String, String)>,
    );

    fn fingerprint(inc: &IncrementalIndex) -> Fingerprint {
        (
            inc.graph
                .files
                .iter()
                .map(|f| f.path.as_str().to_string())
                .collect(),
            inc.graph
                .symbols
                .iter()
                .map(|s| format!("{}@{}", s.name, s.file.0))
                .collect(),
            inc.graph
                .edges
                .iter()
                .map(|e| (e.from, e.to, e.kind as u8, e.weight))
                .collect(),
            inc.report
                .unresolved_imports
                .iter()
                .map(|(p, s)| (p.as_str().to_string(), s.clone()))
                .collect(),
        )
    }

    fn changes(added: &[&str], modified: &[&str], removed: &[&str]) -> ChangeSet {
        let to = |v: &[&str]| v.iter().map(RelPath::new).collect();
        ChangeSet {
            added: to(added),
            modified: to(modified),
            removed: to(removed),
        }
    }

    fn step(root: &Path, prev: &IncrementalIndex, c: ChangeSet) -> IncrementalIndex {
        let next = prev.apply_changeset(root, &c, &opts());
        let cold = IncrementalIndex::build(root, &opts());
        assert_eq!(fingerprint(&next), fingerprint(&cold), "changes: {c:?}");
        next
    }

    fn fixture(root: &Path) {
        write(root, "pkg/__init__.py", "");
        write(
            root,
            "pkg/a.py",
            "from pkg import b\n\ndef fa():\n    pass\n",
        );
        write(root, "pkg/b.py", "def fb():\n    pass\n");
        write(root, "pkg/c.py", "def fc():\n    pass\n");
        write(root, "go.mod", "module example.com/app\n\ngo 1.21\n");
        write(
            root,
            "main.go",
            "package main\n\nimport \"example.com/app/util\"\n\nfunc main() { util.U() }\n",
        );
        write(root, "util/util.go", "package util\n\nfunc U() {}\n");
        write(root, "util2/util.go", "package util2\n\nfunc V() {}\n");
        write(
            root,
            "App/App.csproj",
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>",
        );
        write(
            root,
            "App/Program.cs",
            "using Lib.Core;\nnamespace App { class Program { static void Main() { } } }\n",
        );
        write(
            root,
            "App/Lib1.cs",
            "namespace Lib.Core { public class One { } }\n",
        );
        write(
            root,
            "App/Lib2.cs",
            "namespace Lib.Core { public class Two { } }\n",
        );
        write(
            root,
            "App/Other.cs",
            "namespace Other.Ns { public class Three { } }\n",
        );
    }

    #[test]
    fn incremental_chain_matches_cold_build() {
        let root = scratch("chain");
        fixture(&root);
        let mut inc = IncrementalIndex::build(&root, &opts());

        // Python edit adds an import; nothing else may change.
        write(
            &root,
            "pkg/a.py",
            "from pkg import b, c\n\ndef fa():\n    pass\n",
        );
        inc = step(&root, &inc, changes(&[], &["pkg/a.py"], &[]));

        // A C# declaration file moves namespaces: namespace edges follow.
        write(
            &root,
            "App/Lib2.cs",
            "namespace Other.Ns { public class Two { } }\n",
        );
        inc = step(&root, &inc, changes(&[], &["App/Lib2.cs"], &[]));

        // The importer edits its `using`.
        write(
            &root,
            "App/Program.cs",
            "using Other.Ns;\nnamespace App { class Program { static void Main() { } } }\n",
        );
        inc = step(&root, &inc, changes(&[], &["App/Program.cs"], &[]));

        // go.mod module rename changes how main.go's import resolves.
        write(&root, "go.mod", "module example.com/renamed\n\ngo 1.21\n");
        inc = step(&root, &inc, changes(&[], &["go.mod"], &[]));
        write(&root, "go.mod", "module example.com/app\n\ngo 1.21\n");
        inc = step(&root, &inc, changes(&[], &["go.mod"], &[]));

        // csproj edit.
        write(
            &root,
            "App/App.csproj",
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net8.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings></PropertyGroup></Project>",
        );
        inc = step(&root, &inc, changes(&[], &["App/App.csproj"], &[]));

        // Add and remove files (path set changes: full redo).
        write(&root, "pkg/d.py", "from pkg import a\n");
        inc = step(&root, &inc, changes(&["pkg/d.py"], &[], &[]));
        std::fs::remove_file(root.join("pkg/b.py")).unwrap();
        inc = step(&root, &inc, changes(&[], &[], &["pkg/b.py"]));

        // A no-op apply stays equal too.
        step(&root, &inc, changes(&[], &[], &[]));
    }

    #[test]
    fn unrelated_edit_reuses_resolver_metadata_and_resolutions() {
        let root = scratch("reuse");
        fixture(&root);
        let inc = IncrementalIndex::build(&root, &opts());

        write(
            &root,
            "pkg/c.py",
            "from pkg import b\n\ndef fc2():\n    pass\n",
        );
        let next = inc.apply_changeset(&root, &changes(&[], &["pkg/c.py"], &[]), &opts());

        // Untouched files keep the very same resolution entry.
        for path in ["pkg/a.py", "main.go", "App/Program.cs"] {
            let p = RelPath::new(path);
            assert!(
                Arc::ptr_eq(&inc.resolutions.by_path[&p], &next.resolutions.by_path[&p]),
                "{path} should reuse its resolutions"
            );
        }
        // The edited file was re-resolved.
        let c = RelPath::new("pkg/c.py");
        assert!(!Arc::ptr_eq(
            &inc.resolutions.by_path[&c],
            &next.resolutions.by_path[&c]
        ));

        // Only resolvers that read a modified file re-run `detect`.
        let mods: BTreeSet<RelPath> = [RelPath::new("pkg/c.py")].into();
        let files = scan::to_index(
            &root,
            &ScanResult {
                files: inc.files.clone(),
                skipped: Vec::new(),
            },
        );
        let (_, changed) = inc.resolvers.redetect(&files, &mods);
        assert!(changed.is_empty(), "{changed:?}");
    }

    #[test]
    fn declaration_edit_redetects_only_its_language() {
        let root = scratch("lang");
        fixture(&root);
        let inc = IncrementalIndex::build(&root, &opts());
        write(
            &root,
            "App/Lib2.cs",
            "namespace Other.Ns { public class Two { } }\n",
        );
        let files = scan::to_index(
            &root,
            &ScanResult {
                files: inc.files.clone(),
                skipped: Vec::new(),
            },
        );
        let mods: BTreeSet<RelPath> = [RelPath::new("App/Lib2.cs")].into();
        let (_, changed) = inc.resolvers.redetect(&files, &mods);
        assert_eq!(changed, vec![Language::CSharp]);
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "astrolabe-cancel-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write with an mtime an hour back, so the stamp is trusted (a fresh
    /// mtime is inside the precision window and never stamped).
    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::write(&path, body).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }

    fn store_len(root: &Path) -> u64 {
        Store::open_read_only(&store_path(root))
            .unwrap()
            .expect("store exists")
            .parse_cache_len()
            .unwrap()
    }

    fn has_stamp(root: &Path, rel: &str) -> bool {
        Store::open_read_only(&store_path(root))
            .unwrap()
            .expect("store exists")
            .file_stamp(rel)
            .unwrap()
            .is_some()
    }

    #[test]
    fn cancelled_parse_writes_nothing_and_never_prunes() {
        let root = scratch("parse");
        write(&root, "a.go", "package p\nfunc A() {}\n");
        write(&root, "b.go", "package p\nfunc B() {}\n");
        let opts = IndexOptions::default();
        let _ = IncrementalIndex::build(&root, &opts);
        let before = store_len(&root);
        assert_eq!(before, 2);

        // b.go is gone and c.go is new: a completed full-scan parse would
        // prune b's entry and store c's. A cancelled one must do neither —
        // a prune keyed on a partial live set deletes valid entries.
        std::fs::remove_file(root.join("b.go")).unwrap();
        write(&root, "c.go", "package p\nfunc C() {}\n");
        let paths = vec![RelPath::new("a.go"), RelPath::new("c.go")];
        let cancel = AtomicBool::new(true);
        assert!(parse_paths_cancellable(&root, &paths, &opts, true, Some(&cancel)).is_none());
        assert_eq!(
            store_len(&root),
            before,
            "cancelled parse touched the store"
        );
        assert!(!has_stamp(&root, "c.go"), "cancelled parse wrote a stamp");

        // Control: the same parse, not cancelled, prunes b and stores c.
        let done = parse_paths(&root, &paths, &opts, true);
        assert_eq!(done.parsed.len(), 2);
        assert_eq!(store_len(&root), 2);
        assert!(has_stamp(&root, "c.go"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_cancellable_matches_build_and_honours_cancel() {
        let root = scratch("build");
        write(&root, "go.mod", "module example.com/p\n");
        write(&root, "a.go", "package p\nfunc A() { B() }\n");
        write(&root, "b.go", "package p\nfunc B() {}\n");
        let opts = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        let cancel = AtomicBool::new(true);
        assert!(IncrementalIndex::build_cancellable(&root, &opts, &cancel).is_none());
        assert!(
            !store_path(&root).exists(),
            "a build cancelled up front must not create a store"
        );

        cancel.store(false, Ordering::SeqCst);
        let a = IncrementalIndex::build_cancellable(&root, &opts, &cancel).expect("not cancelled");
        let b = IncrementalIndex::build(&root, &opts);
        let names = |i: &IncrementalIndex| {
            i.graph
                .symbols
                .iter()
                .map(|s| (s.name.clone(), s.start_line))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&a), names(&b));
        assert_eq!(a.graph.files.len(), b.graph.files.len());
        std::fs::remove_dir_all(&root).ok();
    }
}
