//! Hybrid file watcher for incremental re-index: OS events by default, polling fallback.
//!
//! The index is a snapshot. After an editor save the graph is stale until
//! something rebuilds it. This module is that something: it detects filesystem
//! changes and diffs `mtime` + size against the previous snapshot so the
//! watched set matches the indexer (gitignore, default excludes, size/binary
//! gates).
//!
//! ## Architecture: Event fast path + conditional safety + freshness log
//!
//! 1. **Native OS events as a path hint**: We register a
//!    [`notify::RecommendedWatcher`] recursively on the workspace root
//!    (FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on
//!    Windows). Events carry paths; they are not themselves the diff.
//! 2. **Prefix filter before any walk**: Paths under excluded directories
//!    (`.git`, `node_modules`, `target`, `.astrolabe`, … plus
//!    [`ScanOptions::extra_excludes`] prefixes) are dropped on arrival. If
//!    every event in a wake is dropped, the loop does not arm debounce and
//!    does not call [`Watcher::check`] / [`Watcher::check_paths`] — this
//!    breaks the self-trigger loop of writing into `.astrolabe`.
//! 3. **Debounce & pending-path merge**: Surviving paths are merged into a
//!    deduplicated pending set (paths, not event objects). Rapid editor
//!    saves collapse into one directed diff after
//!    [`WatchConfig::debounce`] (default 500 ms).
//! 4. **Directed diff, storm degrade**: After the quiet period,
//!    [`Watcher::check_paths`] stats only the suspects (and admits new
//!    paths via [`crate::scan::path_admission`]). Directory events expand
//!    that subtree with a bounded walk. If the pending set exceeds
//!    [`WatchConfig::storm_paths`] (default 5000; e.g. `git checkout` of a
//!    large branch), the loop degrades to a full [`Watcher::check`].
//! 5. **Safety net**: A slow periodic full-tree scan (every
//!    [`WatchConfig::safety_interval`], default 30 minutes) still runs
//!    during OS-event silence. Overflow or a storm sets
//!    [`FreshnessLog::distrusted`]; while distrusted the safety scan is
//!    scheduled within [`DISTRUST_SAFETY_INTERVAL`] (60 s) and a clean
//!    full check clears the flag. The first deadline is phase-shifted by a
//!    random offset in `[0, interval)` so concurrent watchers do not
//!    stampede.
//! 6. **Automatic polling fallback**: If the OS notification system fails
//!    to initialize or errors at runtime, the watcher logs a warning and
//!    polls at [`WatchConfig::poll_interval`] (default 10 s).
//!
//! ## Cost model
//!
//! In event mode, idle CPU is 0.0% because the background thread sleeps on
//! channel `recv_timeout`. A typical editor save is a handful of `stat`
//! calls via `check_paths`, not a tree walk. A file is hashed only when it
//! looks different (`mtime`/size) or is still inside the mtime-precision
//! window. Full `check` (scan + diff) runs on storm/overflow, on the
//! safety interval, and in polling mode. The safety walk is phase-jittered
//! so N processes on one tree do not align their full scans.
//!
//! ## mtime precision
//!
//! Some filesystems report `mtime` at 1 s (APFS/HFS in compatibility
//! mode, some NFS) or 2 s (FAT). Two same-size writes in that window can
//! share one `mtime`. Files whose `mtime` is within
//! [`MTIME_UNTRUSTED_WINDOW`] of "now" (or in the future) are treated as
//! unsettled: we store a content hash on observe and re-hash on the next
//! check even if metadata looks unchanged.
//!
//! ## Usage
//!
//! Embed in an existing loop (you own scheduling and debounce):
//!
//! ```ignore
//! let mut watcher = astrolabe_core::watch::Watcher::new(root);
//! let _ = watcher.check(); // baseline; returns empty
//! loop {
//!     std::thread::sleep(std::time::Duration::from_secs(10));
//!     let changes = watcher.check();
//!     if !changes.is_empty() {
//!         // reindex `changes.added` + `changes.modified`;
//!         // drop `changes.removed` from the graph
//!     }
//! }
//! ```
//!
//! Or spawn a daemon thread that debounces and pushes on a channel:
//!
//! ```ignore
//! let (rx, handle, log) = astrolabe_core::watch::Watcher::new(root)
//!     .config(astrolabe_core::watch::WatchConfig::default())
//!     .spawn_with_log();
//! while let Ok(changes) = rx.recv() {
//!     // one changeset per quiet period
//! }
//! handle.stop();
//! ```

use crate::scan::{self, ScanOptions};
use crate::types::RelPath;
use notify::{RecursiveMode, Watcher as _};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// Files whose mtime is newer than this are not trusted without a hash.
///
/// FAT timestamps are 2 s; most other coarse clocks are 1 s. A 2 s window
/// covers both, plus a little clock skew.
pub const MTIME_UNTRUSTED_WINDOW: Duration = Duration::from_secs(2);

const STOP_SLICE: Duration = Duration::from_millis(50);
const EVENT_SAFETY_INTERVAL: Duration = Duration::from_secs(1800);
/// While [`FreshnessLog`] is distrusted, a full safety scan is scheduled
/// within this cap rather than waiting out [`WatchConfig::safety_interval`].
const DISTRUST_SAFETY_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_STORM_PATHS: usize = 5000;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

/// Polling cadence, emit delay, event-mode safety scan, and storm threshold
/// for [`Watcher::spawn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// Cadence for the polling fallback when native OS events are disabled or unavailable.
    pub poll_interval: Duration,
    /// Quiet period before a pending [`ChangeSet`] is sent. Rapid editor
    /// saves collapse into one event.
    pub debounce: Duration,
    /// Periodic full-tree scan in event mode even during complete OS-event
    /// silence. Guards against dropped or lost notifications. Default 30
    /// minutes. `Duration::ZERO` disables the safety scan (unless
    /// [`FreshnessLog`] is distrusted, in which case a 60 s scan is still
    /// scheduled).
    pub safety_interval: Duration,
    /// Phase-shift the first safety-scan deadline by a random offset in
    /// `[0, safety_interval)` so concurrent watchers on the same tree do not
    /// stampede. Default true; tests that depend on loop timing should set
    /// this false.
    pub safety_jitter: bool,
    /// Pending suspect-path count that forces a full-tree [`Watcher::check`].
    /// Default 5000. Covers `git checkout` of a large branch.
    pub storm_paths: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            poll_interval: Duration::from_secs(10),
            debounce: Duration::from_millis(500),
            safety_interval: EVENT_SAFETY_INTERVAL,
            safety_jitter: true,
            storm_paths: DEFAULT_STORM_PATHS,
        }
    }
}

impl WatchConfig {
    /// Periodic full-tree scan interval in event mode. See [`Self::safety_interval`].
    pub fn safety_interval(mut self, d: Duration) -> Self {
        self.safety_interval = d;
        self
    }

    /// Enable or disable the first-scan phase offset. See [`Self::safety_jitter`].
    pub fn safety_jitter(mut self, enabled: bool) -> Self {
        self.safety_jitter = enabled;
        self
    }

    /// Storm threshold. See [`Self::storm_paths`].
    pub fn storm_paths(mut self, n: usize) -> Self {
        self.storm_paths = n;
        self
    }
}

/// Query-side freshness barrier: watcher thread writes, readers observe.
///
/// Shared via [`Arc`]. A watermark is the last completed verification (a
/// finished full [`Watcher::check`], or a finished directed
/// [`Watcher::check_paths`]). Overflow / storm sets `distrusted` until the
/// next clean full check.
pub struct FreshnessLog {
    inner: Mutex<FreshnessInner>,
    /// Set by the query-time barrier when the watermark is stale but there
    /// are no suspects (`Repair([])`). The watch loop polls this every
    /// [`STOP_SLICE`] and runs a full [`Watcher::check`] (which calls
    /// [`FreshnessLog::note_verified`]).
    verify_requested: AtomicBool,
    /// First completed full [`Watcher::check`] (the spawn baseline). The
    /// cold build must not start until this is set, or a file created in
    /// the gap is absorbed into the baseline and never emitted.
    seeded: AtomicBool,
    /// Polling cadence (ms) while the watch thread runs in polling mode
    /// (forced, or fallen back from events): the configured interval plus
    /// backoff plus the last scan's duration. `0` while events drive the
    /// loop. The query barrier sizes its freshness window from this, because
    /// in polling mode the watermark only advances once per cycle.
    poll_cycle_ms: AtomicU64,
}

struct FreshnessInner {
    watermark: Instant,
    distrusted: bool,
    /// Suspect paths accumulated after the current watermark, in note order.
    /// When `distrusted`, [`FreshnessLog::suspect_paths`] hides these and
    /// returns empty (callers look at [`FreshnessLog::distrusted`]).
    suspects: Vec<(Instant, RelPath)>,
    /// Paths the query barrier re-read from disk on its own
    /// ([`FreshnessLog::note_repaired`]). The index may now hold content the
    /// watcher snapshot never saw (an intermediate save), so the next check
    /// that looks at such a path reports it even when it matches the old
    /// snapshot stamp. Without this, "edit, query, revert within the
    /// debounce" compared the revert against the pre-edit stamp, found no
    /// change, and the deleted symbol stayed in the index until a rebuild.
    repaired: BTreeSet<RelPath>,
}

impl FreshnessLog {
    fn new() -> Self {
        FreshnessLog {
            inner: Mutex::new(FreshnessInner {
                watermark: Instant::now(),
                distrusted: false,
                suspects: Vec::new(),
                repaired: BTreeSet::new(),
            }),
            verify_requested: AtomicBool::new(false),
            seeded: AtomicBool::new(false),
            poll_cycle_ms: AtomicU64::new(0),
        }
    }

    /// Effective polling cycle when the watch thread is polling, `None`
    /// while native events drive it (or before the loop has started).
    ///
    /// In polling mode nothing advances the watermark between scans, so a
    /// fixed 5 s window would turn every query in the second half of a
    /// 10 s cycle into an extra full-tree verification. Callers size the
    /// window from this instead and only verify when the poller is overdue.
    pub fn poll_cycle(&self) -> Option<Duration> {
        match self.poll_cycle_ms.load(Ordering::SeqCst) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    fn set_poll_cycle(&self, cycle: Option<Duration>) {
        let ms = cycle.map_or(0, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        self.poll_cycle_ms.store(ms, Ordering::SeqCst);
    }

    /// Ask the watch loop to run a full verification on the next `STOP_SLICE`.
    ///
    /// Used by the MCP freshness barrier for `Repair([])` (stale watermark,
    /// no suspects). A clean [`Watcher::check`] pushes the watermark.
    pub fn request_verification(&self) {
        self.verify_requested.store(true, Ordering::SeqCst);
    }

    fn take_verification_request(&self) -> bool {
        self.verify_requested.swap(false, Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FreshnessInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True after the first full [`Watcher::check`] has sealed the baseline.
    ///
    /// [`Watcher::spawn_with_log`] returns before that scan finishes. Callers
    /// that must not race the baseline (the MCP cold build) wait on this.
    pub fn is_seeded(&self) -> bool {
        self.seeded.load(Ordering::SeqCst)
    }

    /// Instant of the last completed verification (full check or directed).
    pub fn watermark(&self) -> Instant {
        self.lock().watermark
    }

    /// Suspect paths noted after `since`.
    ///
    /// Includes event paths and the pending set as it stood before a storm
    /// expanded into a full check. When the log is distrusted this returns
    /// empty; callers should consult [`Self::distrusted`].
    pub fn suspect_paths(&self, since: Instant) -> Vec<RelPath> {
        let g = self.lock();
        if g.distrusted {
            return Vec::new();
        }
        let mut out: Vec<RelPath> = g
            .suspects
            .iter()
            .filter(|(t, _)| *t > since)
            .map(|(_, p)| p.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Overflow or storm happened, and no clean full check has run since.
    pub fn distrusted(&self) -> bool {
        self.lock().distrusted
    }

    /// Note event paths as suspects. A path that is already a suspect is
    /// re-stamped instead of duplicated: a second write to a path that is
    /// still waiting for its debounce (edit, then revert) must become visible
    /// to the barrier again even if an earlier suspect for it was already
    /// repaired and cleared.
    pub(crate) fn note_paths(&self, paths: &[RelPath]) {
        if paths.is_empty() {
            return;
        }
        let mut g = self.lock();
        let now = Instant::now();
        let mut fresh: HashSet<&RelPath> = paths.iter().collect();
        for entry in g.suspects.iter_mut() {
            if fresh.remove(&entry.1) {
                entry.0 = now;
            }
        }
        for p in paths {
            if fresh.remove(p) {
                g.suspects.push((now, p.clone()));
            }
        }
    }

    /// The query barrier is about to re-read `paths` from disk into the
    /// index, bypassing the watcher. Call this *before* reading, so a check
    /// that races the repair still reports the path (see
    /// `FreshnessInner::repaired`).
    pub fn note_repaired(&self, paths: &[RelPath]) {
        if paths.is_empty() {
            return;
        }
        let mut g = self.lock();
        g.repaired.extend(paths.iter().cloned());
    }

    /// Take the repaired marks for `paths` (all of them when `None`).
    fn take_repaired(&self, paths: Option<&BTreeSet<RelPath>>) -> BTreeSet<RelPath> {
        let mut g = self.lock();
        if g.repaired.is_empty() {
            return BTreeSet::new();
        }
        match paths {
            None => std::mem::take(&mut g.repaired),
            Some(paths) => {
                let hit: BTreeSet<RelPath> = g.repaired.intersection(paths).cloned().collect();
                for p in &hit {
                    g.repaired.remove(p);
                }
                hit
            }
        }
    }

    pub(crate) fn note_distrust(&self) {
        self.lock().distrusted = true;
    }

    /// A completed full check: push watermark, clear distrust and suspects.
    ///
    /// `started` is when the scan began. Suspects noted after that (events
    /// that arrived while the scan ran off the watch thread) may or may not
    /// be reflected in what the scan saw, so they are kept and re-stamped
    /// after the new watermark instead of being cleared.
    ///
    /// `changes` is the delta this check just produced. It has been sent (or
    /// is about to be) to the reindex consumer but is not applied to the
    /// index yet, so its paths stay suspects *after* the new watermark until
    /// the query barrier repairs them or a rebuild covers them
    /// ([`Self::clear_suspects_through`]). Advancing the watermark alone would
    /// make the barrier report "fresh" for an index that does not yet contain
    /// the change.
    pub(crate) fn note_verified(&self, changes: &ChangeSet, started: Instant) {
        let mut g = self.lock();
        let watermark = Instant::now();
        g.watermark = watermark;
        g.distrusted = false;
        let after = watermark + Duration::from_nanos(1);
        let mut kept: Vec<(Instant, RelPath)> = std::mem::take(&mut g.suspects)
            .into_iter()
            .filter(|(t, _)| *t > started)
            .map(|(_, p)| (after, p))
            .collect();
        g.suspects.append(&mut kept);
        Self::push_unapplied(&mut g, changes);
        self.seeded.store(true, Ordering::SeqCst);
    }

    /// A completed directed check; same contract as [`Self::note_verified`]
    /// but without clearing distrust or earlier suspects.
    fn bump_watermark(&self, changes: &ChangeSet) {
        let mut g = self.lock();
        g.watermark = Instant::now();
        Self::push_unapplied(&mut g, changes);
    }

    fn push_unapplied(g: &mut FreshnessInner, changes: &ChangeSet) {
        if changes.is_empty() {
            return;
        }
        // Strictly after the watermark so `suspect_paths(watermark)` sees them.
        let t = Instant::now().max(g.watermark + Duration::from_nanos(1));
        for (_, p) in changes.iter() {
            g.suspects.push((t, p.clone()));
        }
    }

    /// Drop suspects noted at or before `through`: the caller has repaired
    /// (or rebuilt past) them. Later suspects are kept.
    pub fn clear_suspects_through(&self, through: Instant) {
        self.lock().suspects.retain(|(t, _)| *t > through);
    }
}

impl std::fmt::Debug for FreshnessLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("FreshnessLog")
            .field("distrusted", &g.distrusted)
            .field("suspects", &g.suspects.len())
            .finish()
    }
}

/// What happened to one file since the previous snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// A deterministic set of path changes.
///
/// Each vec is sorted by [`RelPath`]. Field order is added → modified →
/// removed so a debug print matches the names the orchestrator cares about.
/// When applying to a graph, drop `removed` first, then parse `added` and
/// `modified`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub added: Vec<RelPath>,
    pub modified: Vec<RelPath>,
    pub removed: Vec<RelPath>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.removed.len()
    }

    /// Paths in stable order: added, then modified, then removed.
    /// Within each kind, paths are sorted.
    pub fn iter(&self) -> impl Iterator<Item = (ChangeKind, &RelPath)> {
        self.added
            .iter()
            .map(|p| (ChangeKind::Added, p))
            .chain(self.modified.iter().map(|p| (ChangeKind::Modified, p)))
            .chain(self.removed.iter().map(|p| (ChangeKind::Removed, p)))
    }

    /// Fold `other` into this set with editor-style coalescing.
    ///
    /// Used by the background thread to collapse bursts inside the debounce
    /// window. Also usable by an orchestrator that batches its own `check`
    /// results.
    pub fn merge(&mut self, other: ChangeSet) {
        if other.is_empty() {
            return;
        }
        let mut map = self.to_map();
        for (kind, path) in other.iter() {
            apply_coalesce(&mut map, path.clone(), kind);
        }
        *self = ChangeSet::from_map(map);
    }

    fn to_map(&self) -> BTreeMap<RelPath, ChangeKind> {
        let mut map = BTreeMap::new();
        for (kind, path) in self.iter() {
            map.insert(path.clone(), kind);
        }
        map
    }

    fn from_map(map: BTreeMap<RelPath, ChangeKind>) -> Self {
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut removed = Vec::new();
        for (path, kind) in map {
            match kind {
                ChangeKind::Added => added.push(path),
                ChangeKind::Modified => modified.push(path),
                ChangeKind::Removed => removed.push(path),
            }
        }
        ChangeSet {
            added,
            modified,
            removed,
        }
    }
}

fn apply_coalesce(map: &mut BTreeMap<RelPath, ChangeKind>, path: RelPath, next: ChangeKind) {
    match (map.get(&path).copied(), next) {
        (None, kind) => {
            map.insert(path, kind);
        }
        (Some(ChangeKind::Added), ChangeKind::Added) => {}
        (Some(ChangeKind::Added), ChangeKind::Modified) => {}
        // Keep the removal: the add may already have been applied to the
        // index out of band (barrier inline repair), and dropping a path that
        // is not indexed is a cheap idempotent no-op. Cancelling both would
        // leave a ghost file behind.
        (Some(ChangeKind::Added), ChangeKind::Removed) => {
            map.insert(path, ChangeKind::Removed);
        }
        (Some(ChangeKind::Modified), ChangeKind::Added) => {}
        (Some(ChangeKind::Modified), ChangeKind::Modified) => {}
        (Some(ChangeKind::Modified), ChangeKind::Removed) => {
            map.insert(path, ChangeKind::Removed);
        }
        (Some(ChangeKind::Removed), ChangeKind::Added) => {
            map.insert(path, ChangeKind::Modified);
        }
        (Some(ChangeKind::Removed), ChangeKind::Modified) => {
            map.insert(path, ChangeKind::Modified);
        }
        (Some(ChangeKind::Removed), ChangeKind::Removed) => {}
    }
}

/// A directed check that must degrade to a full [`Watcher::check`]
/// (`distrust`: storm / root event, so the log is marked distrusted first).
#[derive(Debug)]
struct NeedFull {
    distrust: bool,
}

#[derive(Clone, Debug)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
    hash: Option<u64>,
    unsettled: bool,
    mtime_ok: bool,
}

/// Hybrid filesystem watcher (native OS events with polling fallback).
///
/// Holds the last snapshot; [`check`] returns the full-tree delta and
/// [`check_paths`] diffs a suspect set. Prefer [`spawn`] / [`spawn_with_log`]
/// for a background thread that debounces and pushes [`ChangeSet`]s.
pub struct Watcher {
    root: PathBuf,
    scan: ScanOptions,
    config: WatchConfig,
    snapshot: BTreeMap<RelPath, FileStamp>,
    seeded: bool,
    events: bool,
    freshness: Arc<FreshnessLog>,
    check_calls: Arc<AtomicU64>,
    check_paths_calls: Arc<AtomicU64>,
    /// Test hook: stretch every full [`Watcher::check`] (after the tree was
    /// observed) to simulate a slow scan on a large repository.
    #[cfg(test)]
    scan_delay: Duration,
}

impl Watcher {
    /// Construct without touching disk. The first [`check`] (or [`spawn`])
    /// takes the baseline and returns an empty changeset.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Watcher {
            root: root.into(),
            scan: ScanOptions::default(),
            config: WatchConfig::default(),
            snapshot: BTreeMap::new(),
            seeded: false,
            events: true,
            freshness: Arc::new(FreshnessLog::new()),
            check_calls: Arc::new(AtomicU64::new(0)),
            check_paths_calls: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            scan_delay: Duration::ZERO,
        }
    }

    /// Enable or disable native OS filesystem events (default: true).
    /// When disabled, [`Watcher::spawn`] uses pure polling at [`WatchConfig::poll_interval`].
    pub fn events(mut self, enabled: bool) -> Self {
        self.events = enabled;
        self
    }

    pub fn scan_options(mut self, scan: ScanOptions) -> Self {
        self.scan = scan;
        self
    }

    pub fn config(mut self, config: WatchConfig) -> Self {
        self.config = config;
        self
    }

    pub fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.config.poll_interval = poll_interval;
        self
    }

    pub fn debounce(mut self, debounce: Duration) -> Self {
        self.config.debounce = debounce;
        self
    }

    pub fn safety_interval(mut self, safety_interval: Duration) -> Self {
        self.config.safety_interval = safety_interval;
        self
    }

    pub fn safety_jitter(mut self, enabled: bool) -> Self {
        self.config.safety_jitter = enabled;
        self
    }

    pub fn storm_paths(mut self, n: usize) -> Self {
        self.config.storm_paths = n;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Files in the current snapshot (0 until the first `check` / spawn).
    pub fn tracked_files(&self) -> usize {
        self.snapshot.len()
    }

    pub fn is_seeded(&self) -> bool {
        self.seeded
    }

    /// Diff the tree against the last snapshot.
    ///
    /// The first call only records a baseline and returns empty. Later
    /// calls return added / modified / removed paths, each vec sorted.
    /// Completing this full scan pushes the freshness watermark and
    /// clears distrust.
    pub fn check(&mut self) -> ChangeSet {
        self.check_calls.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let now = SystemTime::now();
        let observed = observe_tree(&self.root, &self.scan);
        #[cfg(test)]
        if !self.scan_delay.is_zero() {
            thread::sleep(self.scan_delay);
        }

        if !self.seeded {
            self.snapshot = stamps_from_observed(&self.root, &observed, now);
            self.seeded = true;
            self.freshness.note_verified(&ChangeSet::default(), started);
            return ChangeSet::default();
        }

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut next_snapshot = BTreeMap::new();

        for (path, meta) in &observed {
            match self.snapshot.get(path) {
                None => {
                    added.push(path.clone());
                    next_snapshot
                        .insert(path.clone(), make_stamp(&self.root, path, meta, now, None));
                }
                Some(old) => {
                    let (stamp, changed) = confirm(&self.root, path, old, meta, now);
                    if changed {
                        modified.push(path.clone());
                    }
                    next_snapshot.insert(path.clone(), stamp);
                }
            }
        }

        let mut removed = Vec::new();
        for path in self.snapshot.keys() {
            if !observed.contains_key(path) {
                removed.push(path.clone());
            }
        }

        self.snapshot = next_snapshot;

        let mut changes = ChangeSet {
            added,
            modified,
            removed,
        };
        let repaired = self.freshness.take_repaired(None);
        self.report_repaired(&mut changes, repaired);
        if !changes.is_empty() {
            tracing::debug!(
                added = changes.added.len(),
                modified = changes.modified.len(),
                removed = changes.removed.len(),
                "watch changeset"
            );
        }
        self.freshness.note_verified(&changes, started);
        changes
    }

    /// Directed diff of `paths` against the snapshot.
    ///
    /// Deduplicates `paths`. Directories are expanded with a bounded walk
    /// (excluded names skipped; snapshot keys under the prefix included so
    /// deletions are visible). Paths already in the snapshot are `stat`ed
    /// and run through [`confirm`]; new paths go through
    /// [`scan::path_admission`]. If the unique / expanded set exceeds
    /// [`WatchConfig::storm_paths`], this degrades to [`Self::check`].
    pub fn check_paths(&mut self, paths: &[PathBuf]) -> ChangeSet {
        match self.directed_check(paths) {
            Ok(changes) => changes,
            Err(NeedFull { distrust }) => {
                if distrust {
                    self.freshness.note_distrust();
                }
                self.check()
            }
        }
    }

    /// [`Self::check_paths`] without the full-check fallback: returns
    /// `Err(NeedFull)` where `check_paths` would degrade, so the event loop
    /// can run that full scan off the watch thread (L2) instead of inline.
    fn directed_check(&mut self, paths: &[PathBuf]) -> Result<ChangeSet, NeedFull> {
        self.check_paths_calls.fetch_add(1, Ordering::Relaxed);
        if !self.seeded {
            return Err(NeedFull { distrust: false });
        }
        let cap = self.config.storm_paths;
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for p in paths {
            let abs = if p.is_absolute() {
                p.clone()
            } else {
                self.root.join(p)
            };
            if seen.insert(abs.clone()) {
                unique.push(abs);
            }
        }
        if unique.len() > cap {
            return Err(NeedFull { distrust: true });
        }

        let extra = &self.scan.extra_excludes;
        let mut work: BTreeSet<RelPath> = BTreeSet::new();
        for abs in &unique {
            if abs == &self.root
                || (relativize(&self.root, abs).is_none() && abs.starts_with(&self.root))
            {
                // Root-directory event: treat as a bounded walk of the whole
                // tree. Hitting `cap` degrades to a full check.
                if expand_dir_bounded(&self.root, abs, extra, cap, &mut work).is_err() {
                    return Err(NeedFull { distrust: true });
                }
                if work.len() > cap {
                    return Err(NeedFull { distrust: true });
                }
                continue;
            }
            let Some(rel) = relativize(&self.root, abs) else {
                continue;
            };
            if is_excluded_rel(&self.root, &rel, extra) {
                continue;
            }
            // Never follow symlinked directories (or paths below one): a full
            // scan does not, so expanding them would index files from outside
            // the repo that the next full check immediately removes.
            if crosses_symlink_dir(&self.root, abs) {
                continue;
            }
            if abs.is_dir() {
                if expand_dir_bounded(&self.root, abs, extra, cap, &mut work).is_err() {
                    return Err(NeedFull { distrust: true });
                }
                let prefix = rel.as_str();
                for k in self.snapshot.keys() {
                    if rel_is_under(k, prefix) {
                        work.insert(k.clone());
                        if work.len() > cap {
                            return Err(NeedFull { distrust: true });
                        }
                    }
                }
            } else {
                work.insert(rel.clone());
                if !abs.exists() {
                    let prefix = rel.as_str();
                    for k in self.snapshot.keys() {
                        if rel_is_under(k, prefix) {
                            work.insert(k.clone());
                            if work.len() > cap {
                                return Err(NeedFull { distrust: true });
                            }
                        }
                    }
                }
                if work.len() > cap {
                    return Err(NeedFull { distrust: true });
                }
            }
        }

        if work.len() > cap {
            return Err(NeedFull { distrust: true });
        }

        let now = SystemTime::now();
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut removed = Vec::new();

        let repaired = self.freshness.take_repaired(Some(&work));
        for rel in work {
            match self.snapshot.get(&rel).cloned() {
                Some(old) => match (
                    admit_new(&self.root, &rel, &self.scan),
                    observe_one(&self.root, &rel),
                ) {
                    // Still admitted: same stat + confirm as before.
                    (Admit::Indexed, Some(meta)) => {
                        let (stamp, changed) = confirm(&self.root, &rel, &old, &meta, now);
                        if changed {
                            modified.push(rel.clone());
                        }
                        self.snapshot.insert(rel, stamp);
                    }
                    // Gone, or no longer passes the scan gates (gitignored,
                    // grew past the size cap, turned binary): a full scan
                    // would drop it, so the directed path must too.
                    _ => {
                        self.snapshot.remove(&rel);
                        removed.push(rel);
                    }
                },
                None => match admit_new(&self.root, &rel, &self.scan) {
                    Admit::Indexed => {
                        if let Some(meta) = observe_one(&self.root, &rel) {
                            let stamp = make_stamp(&self.root, &rel, &meta, now, None);
                            self.snapshot.insert(rel.clone(), stamp);
                            added.push(rel);
                        }
                    }
                    Admit::Missing | Admit::Skip => {}
                },
            }
        }

        let mut changes = ChangeSet {
            added,
            modified,
            removed,
        };
        self.report_repaired(&mut changes, repaired);
        if !changes.is_empty() {
            tracing::debug!(
                added = changes.added.len(),
                modified = changes.modified.len(),
                removed = changes.removed.len(),
                "watch directed changeset"
            );
        }
        self.freshness.bump_watermark(&changes);
        Ok(changes)
    }

    /// Add barrier-repaired paths the diff did not report: the index may hold
    /// content the snapshot never saw, so re-read them (modified) or drop
    /// them (removed) to converge on what is on disk now. Sorts all vecs.
    fn report_repaired(&self, changes: &mut ChangeSet, repaired: BTreeSet<RelPath>) {
        for rel in repaired {
            if changes.added.contains(&rel)
                || changes.modified.contains(&rel)
                || changes.removed.contains(&rel)
            {
                continue;
            }
            if self.snapshot.contains_key(&rel) {
                changes.modified.push(rel);
            } else {
                changes.removed.push(rel);
            }
        }
        changes.added.sort();
        changes.modified.sort();
        changes.removed.sort();
    }

    /// Run `check` on a background thread and send debounced changesets.
    ///
    /// Delegates to [`Self::spawn_with_log`] and drops the log handle.
    pub fn spawn(self) -> (Receiver<ChangeSet>, WatchHandle) {
        let (rx, handle, _log) = self.spawn_with_log();
        (rx, handle)
    }

    /// Like [`Self::spawn`], also returning the shared [`FreshnessLog`].
    ///
    /// The thread is daemon-like: dropping the [`WatchHandle`] signals stop
    /// **without joining**, so it cannot hold the process open. Call
    /// [`WatchHandle::stop`] for a graceful join.
    pub fn spawn_with_log(self) -> (Receiver<ChangeSet>, WatchHandle, Arc<FreshnessLog>) {
        let log = Arc::clone(&self.freshness);
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        // First full-tree scan runs on the background thread (see event_loop /
        // run_loop), not on the spawn caller. If already seeded, that initial
        // check preserves any non-empty delta since the prior check.
        let notify_ctx = if self.events {
            match init_notify(&self.root, &self.scan, SELECTIVE_WATCH) {
                Some((feed, guard)) => Some((feed, guard)),
                None => {
                    tracing::warn!(
                        "filesystem event watcher could not be initialized; falling back to polling"
                    );
                    None
                }
            }
        } else {
            None
        };

        let thread = thread::Builder::new()
            .name("astrolabe-watch".into())
            .spawn(move || {
                if let Some((feed, guard)) = notify_ctx {
                    let (watcher, ok) =
                        event_loop(self, feed, guard, tx.clone(), Arc::clone(&stop_thread));
                    if let (false, Some(watcher)) = (ok, watcher) {
                        if !stop_thread.load(Ordering::Relaxed) {
                            tracing::warn!(
                                "filesystem event watcher encountered an error; falling back to polling"
                            );
                            run_loop(watcher, tx, stop_thread);
                        }
                    }
                    return;
                }
                run_loop(self, tx, stop_thread);
            })
            .expect("failed to spawn astrolabe-watch thread");
        (
            rx,
            WatchHandle {
                stop,
                thread: Some(thread),
            },
            log,
        )
    }
}

/// Handle for the thread started by [`Watcher::spawn`].
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

// Derived `Debug` is unavailable because `JoinHandle` is only `Debug` on some
// versions; report the observable state instead so callers holding this in a
// `#[derive(Debug)]` struct still compile.
impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHandle")
            .field("stopping", &self.stop.load(Ordering::Relaxed))
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl WatchHandle {
    /// True once the thread has exited (or was never started).
    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(true)
    }

    /// Ask the thread to exit. Does not wait; see [`stop`](Self::stop).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Signal stop and join the thread.
    pub fn stop(mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Detach: dropping JoinHandle without join must not block process
        // exit. The thread notices the flag within STOP_SLICE.
    }
}

/// One wake from the OS event backend (or a test double).
#[derive(Debug)]
enum FeedWake {
    /// Native event(s). `overflow` is latched when the bounded channel
    /// dropped at least one event since the last recv.
    Events { paths: Vec<PathBuf>, overflow: bool },
    /// Backend error → fall back to polling.
    Error,
}

trait EventFeed {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError>;
}

struct NotifyFeed {
    rx: Receiver<Result<notify::Event, notify::Error>>,
    overflow: Arc<AtomicBool>,
    /// Present only in selective (per-directory) mode; owns the watcher so
    /// new directories can be registered from the consumer thread.
    selective: Option<Selective>,
}

/// Per-directory, non-recursive inotify registration that skips excluded
/// trees (node_modules, target, ...), so their files cost no kernel watches
/// or events.
struct Selective {
    watcher: notify::RecommendedWatcher,
    root: PathBuf,
    extra: Vec<String>,
    dirs: HashSet<PathBuf>,
}

impl Selective {
    /// Register `start` and every non-excluded, non-symlink directory below
    /// it. Files seen are appended to `found`. Err on registration failure.
    fn register_tree(
        &mut self,
        start: &Path,
        found: &mut Vec<PathBuf>,
    ) -> Result<(), notify::Error> {
        let mut stack = vec![start.to_path_buf()];
        while let Some(d) = stack.pop() {
            if d != self.root {
                let Some(rel) = relativize(&self.root, &d) else {
                    continue;
                };
                if is_excluded_rel(&self.root, &rel, &self.extra) {
                    continue;
                }
            }
            if self.dirs.contains(&d) {
                continue;
            }
            self.watcher.watch(&d, RecursiveMode::NonRecursive)?;
            self.dirs.insert(d.clone());
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for ent in rd.flatten() {
                let Ok(ft) = ent.file_type() else { continue };
                if ft.is_dir() {
                    stack.push(ent.path());
                } else {
                    found.push(ent.path());
                }
            }
        }
        Ok(())
    }

    /// Register directories newly appearing among `paths`; returns the files
    /// found inside them (they may have been created before the watch).
    fn adopt(&mut self, paths: &[PathBuf], overflow: &AtomicBool) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for p in paths {
            let is_new_dir = fs::symlink_metadata(p)
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false);
            if is_new_dir && !self.dirs.contains(p) && p.starts_with(&self.root) {
                if let Err(e) = self.register_tree(p, &mut found) {
                    tracing::warn!(%e, dir = %p.display(), "failed to watch new directory");
                    overflow.store(true, Ordering::SeqCst);
                }
            }
        }
        found
    }
}

impl EventFeed for NotifyFeed {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(ev)) => {
                let overflow = self.overflow.swap(false, Ordering::SeqCst) || ev.need_rescan();
                // Read-only access (open / close-nowrite / read) never changes
                // content. Our own scans, rebuilds and search reads generate a
                // constant stream of these on inotify; treating them as
                // suspects kept untouched repos permanently "stale".
                let mut paths = if is_content_event(&ev.kind) {
                    ev.paths
                } else {
                    Vec::new()
                };
                if let Some(sel) = self.selective.as_mut() {
                    let found = sel.adopt(&paths, &self.overflow);
                    paths.extend(found);
                }
                Ok(FeedWake::Events { paths, overflow })
            }
            Ok(Err(_)) => Ok(FeedWake::Error),
            Err(e) => {
                if self.overflow.swap(false, Ordering::SeqCst) {
                    Ok(FeedWake::Events {
                        paths: Vec::new(),
                        overflow: true,
                    })
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// Whether a notify event can reflect a content / tree change. Pure
/// access events (open, read, close without write) are dropped; a
/// close-after-write is kept because it marks a completed write.
fn is_content_event(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};
    match kind {
        notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        notify::EventKind::Access(_) => false,
        _ => true,
    }
}

/// Cap queued OS events so a burst cannot grow the channel without bound.
/// Overflow latches a flag; the receiver marks the tree dirty and the
/// safety / debounce path recovers with a full check.
const NOTIFY_CHANNEL_CAP: usize = 65536;

/// Selective registration is Linux-only: inotify cost scales with the number
/// of watched directories, whereas FSEvents / ReadDirectoryChangesW watch a
/// whole subtree at no per-directory cost.
const SELECTIVE_WATCH: bool = cfg!(any(target_os = "linux", target_os = "android"));

fn init_notify(root: &Path, scan: &ScanOptions, selective: bool) -> Option<(NotifyFeed, ())> {
    let overflow = Arc::new(AtomicBool::new(false));
    let overflow_cb = Arc::clone(&overflow);
    let (tx, rx) = mpsc::sync_channel(NOTIFY_CHANNEL_CAP);
    let mut watcher = match notify::recommended_watcher(move |res| match tx.try_send(res) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            overflow_cb.store(true, Ordering::SeqCst);
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {}
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(%e, "failed to create notify recommended_watcher");
            return None;
        }
    };
    if !selective {
        if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
            tracing::warn!(%e, root = %root.display(), "failed to register notify watch on root");
            return None;
        }
        return Some((
            NotifyFeed {
                rx,
                overflow,
                selective: None,
            },
            (),
        ));
    }
    let mut sel = Selective {
        watcher,
        root: root.to_path_buf(),
        extra: scan.extra_excludes.clone(),
        dirs: HashSet::new(),
    };
    let mut ignored = Vec::new();
    if let Err(e) = sel.register_tree(root, &mut ignored) {
        tracing::warn!(%e, root = %root.display(), "failed to register notify watches");
        return None;
    }
    // `.git/info/exclude` changes the admitted set; `.git` itself is excluded.
    let info = root.join(".git").join("info");
    if info.is_dir() {
        let _ = sel.watcher.watch(&info, RecursiveMode::NonRecursive);
    }
    Some((
        NotifyFeed {
            rx,
            overflow,
            selective: Some(sel),
        },
        (),
    ))
}

/// Mix pid, a high-res clock, and a per-call counter into a u64.
/// Not cryptographic — only used to desynchronize safety-scan deadlines.
fn entropy64() -> u64 {
    use std::sync::atomic::AtomicU64 as Seq;
    static SEQ: Seq = Seq::new(1);
    let mut h = FNV_OFFSET;
    h ^= u64::from(std::process::id());
    h = h.wrapping_mul(FNV_PRIME);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    h ^= nanos;
    h = h.wrapping_mul(FNV_PRIME);
    h ^= SEQ.fetch_add(1, Ordering::Relaxed);
    h.wrapping_mul(FNV_PRIME)
}

/// Offset in `[0, interval)`. `interval == 0` yields zero.
fn safety_phase_offset(interval: Duration, entropy: u64) -> Duration {
    let total = interval.as_nanos();
    if total == 0 {
        return Duration::ZERO;
    }
    duration_from_nanos_u128((entropy as u128) % total)
}

fn duration_from_nanos_u128(n: u128) -> Duration {
    const NANOS_PER_SEC: u128 = 1_000_000_000;
    Duration::new((n / NANOS_PER_SEC) as u64, (n % NANOS_PER_SEC) as u32)
}

fn initial_last_safety(config: &WatchConfig) -> Instant {
    let now = Instant::now();
    if !config.safety_jitter {
        return now;
    }
    now.checked_sub(safety_phase_offset(config.safety_interval, entropy64()))
        .unwrap_or(now)
}

/// Effective wait until the next full safety scan.
///
/// Distrust caps the wait at [`DISTRUST_SAFETY_INTERVAL`]. A configured
/// `Duration::ZERO` still disables safety when *not* distrusted.
fn safety_wait(configured: Duration, distrusted: bool) -> Duration {
    if distrusted {
        if configured.is_zero() {
            DISTRUST_SAFETY_INTERVAL
        } else {
            configured.min(DISTRUST_SAFETY_INTERVAL)
        }
    } else {
        configured
    }
}

/// Upper bound on how long a continuously-busy burst may defer delivery.
fn max_debounce_for(debounce: Duration) -> Duration {
    debounce
        .saturating_mul(6)
        .max(Duration::from_secs(3))
        .max(debounce)
}

/// `.gitignore` / `.ignore` edits re-define which other paths are admitted.
fn is_ignore_rule_file(rel: &RelPath) -> bool {
    matches!(
        rel.as_str().rsplit('/').next(),
        Some(".gitignore") | Some(".ignore")
    ) || rel.as_str() == ".git/info/exclude"
}

const FOREIGN_EVENT_LIMIT: usize = 64;

/// How often the event loop checks an off-thread full scan for completion
/// while it keeps draining the event feed.
const SCAN_POLL: Duration = Duration::from_millis(10);

/// Event-side state of [`event_loop`]: the debounced pending set and the
/// counters fed by every wake. Holds its own copies of root / scan options /
/// log so events can still be absorbed while a full scan owns the
/// [`Watcher`] on another thread.
struct Intake {
    root: PathBuf,
    scan: ScanOptions,
    freshness: Arc<FreshnessLog>,
    storm_paths: usize,
    pending: HashSet<PathBuf>,
    all_dirty: bool,
    foreign_events: usize,
    rooted_events: usize,
    last_event: Option<Instant>,
    /// First event of the current debounce burst; bounds the total wait so a
    /// path that keeps changing (a log file) cannot starve delivery.
    first_event: Option<Instant>,
}

impl Intake {
    fn new(watcher: &Watcher) -> Self {
        Intake {
            root: watcher.root.clone(),
            scan: watcher.scan.clone(),
            freshness: Arc::clone(&watcher.freshness),
            storm_paths: watcher.config.storm_paths,
            pending: HashSet::new(),
            all_dirty: false,
            foreign_events: 0,
            rooted_events: 0,
            last_event: None,
            first_event: None,
        }
    }

    /// Merge one wake's paths into the pending set and note new suspects.
    fn absorb(&mut self, paths: Vec<PathBuf>, overflow: bool) {
        let mut kept = 0usize;
        let mut noted: Vec<RelPath> = Vec::new();
        let mut seen: HashSet<RelPath> = HashSet::new();
        if overflow {
            self.all_dirty = true;
            self.freshness.note_distrust();
        }
        for p in paths {
            if p.is_absolute() && !p.starts_with(&self.root) {
                self.foreign_events += 1;
            } else {
                self.rooted_events += 1;
            }
            if let Some((abs, rel)) = keep_event_path(&self.root, &self.scan, &p) {
                // Re-note a path that is already pending too: the barrier may
                // have repaired (and cleared) its first suspect while this
                // later write still waits for the debounce.
                self.pending.insert(abs);
                if seen.insert(rel.clone()) {
                    noted.push(rel);
                }
                kept += 1;
            }
        }
        if self.pending.len() > self.storm_paths {
            self.all_dirty = true;
            self.freshness.note_distrust();
        }
        if !noted.is_empty() {
            // Ignore-rule files change the admitted set of other paths; only
            // a full scan can re-evaluate those.
            if noted.iter().any(is_ignore_rule_file) {
                self.all_dirty = true;
            }
            self.freshness.note_paths(&noted);
        }
        if kept > 0 || overflow || self.all_dirty {
            let now = Instant::now();
            self.last_event = Some(now);
            self.first_event.get_or_insert(now);
        }
    }

    /// Backend reports paths we can never map under the root (e.g. `\\?\`
    /// prefix mismatch on Windows): events are useless.
    fn only_foreign(&self) -> bool {
        self.rooted_events == 0 && self.foreign_events >= FOREIGN_EVENT_LIMIT
    }

    /// A full scan is about to start: everything pending so far is covered by
    /// it. Events that arrive while it runs land in a fresh pending set.
    fn reset_for_full(&mut self) {
        self.all_dirty = false;
        self.pending.clear();
        self.last_event = None;
        self.first_event = None;
    }
}

/// Wait up to `timeout` for one wake, then drain whatever else is queued so a
/// bounded channel does not stay full. `Err` = backend error / disconnect /
/// events that never map under the root → fall back to polling.
fn drain_feed<F: EventFeed>(
    feed: &mut F,
    timeout: Duration,
    intake: &mut Intake,
) -> Result<(), ()> {
    match feed.recv_timeout(timeout) {
        Ok(FeedWake::Events { paths, overflow }) => {
            intake.absorb(paths, overflow);
            loop {
                match feed.recv_timeout(Duration::ZERO) {
                    Ok(FeedWake::Events { paths, overflow }) => intake.absorb(paths, overflow),
                    Ok(FeedWake::Error) => return Err(()),
                    Err(_) => break,
                }
            }
            if intake.only_foreign() {
                tracing::warn!("watch events never map under the root; degrading to polling");
                return Err(());
            }
            Ok(())
        }
        Ok(FeedWake::Error) | Err(RecvTimeoutError::Disconnected) => Err(()),
        Err(RecvTimeoutError::Timeout) => Ok(()),
    }
}

/// Outcome of [`full_check_draining`].
enum FullCheck {
    Done(Box<Watcher>, ChangeSet),
    /// Stop was requested while the scan ran; the scan thread finishes on its
    /// own and its result is dropped.
    Stopped,
    /// The scan thread died (panic); the snapshot is gone.
    Lost,
}

/// Run a full [`Watcher::check`] on a helper thread while this thread keeps
/// draining the event feed into `intake` (L2). Events that arrive during the
/// scan stay pending and suspect, and get a directed check after the usual
/// debounce. Stop is honoured within [`SCAN_POLL`] instead of after the scan.
/// A feed failure during the scan is recorded in `feed_failed`; the scan is
/// still awaited so the caller can fall back to polling with its snapshot.
fn full_check_draining<F: EventFeed>(
    watcher: Watcher,
    feed: &mut F,
    intake: &mut Intake,
    stop: &AtomicBool,
    feed_failed: &mut bool,
) -> FullCheck {
    let (in_tx, in_rx) = mpsc::channel::<Watcher>();
    let (out_tx, out_rx) = mpsc::channel::<(Watcher, ChangeSet)>();
    let spawned = thread::Builder::new()
        .name("astrolabe-watch-scan".into())
        .spawn(move || {
            if let Ok(mut w) = in_rx.recv() {
                let batch = w.check();
                let _ = out_tx.send((w, batch));
            }
        });
    let handed_over = match spawned {
        Ok(_) => in_tx.send(watcher).map_err(|e| e.0),
        Err(_) => Err(watcher),
    };
    if let Err(mut watcher) = handed_over {
        // Could not start a helper thread: scan inline as before.
        let batch = watcher.check();
        return FullCheck::Done(Box::new(watcher), batch);
    }
    loop {
        match out_rx.recv_timeout(SCAN_POLL) {
            Ok((watcher, batch)) => return FullCheck::Done(Box::new(watcher), batch),
            Err(RecvTimeoutError::Disconnected) => return FullCheck::Lost,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if stop.load(Ordering::Relaxed) {
            return FullCheck::Stopped;
        }
        if !*feed_failed && drain_feed(feed, Duration::ZERO, intake).is_err() {
            *feed_failed = true;
        }
    }
}

/// Native-event loop. Returns the watcher (when it still owns it) and `false`
/// when the caller should fall back to polling.
fn event_loop<F: EventFeed, G>(
    mut watcher: Watcher,
    mut feed: F,
    guard: G,
    tx: mpsc::Sender<ChangeSet>,
    stop: Arc<AtomicBool>,
) -> (Option<Watcher>, bool) {
    watcher.freshness.set_poll_cycle(None);
    let debounce = watcher.config.debounce;
    let storm_paths = watcher.config.storm_paths;
    let safety_interval = watcher.config.safety_interval;
    let max_debounce = max_debounce_for(debounce);
    let mut last_safety = initial_last_safety(&watcher.config);
    let mut intake = Intake::new(&watcher);
    let mut feed_failed = false;

    // Full scan off the watch thread; `continue`-style control via macro so
    // every call site handles stop / panic / feed failure the same way.
    macro_rules! full_check {
        () => {{
            match full_check_draining(watcher, &mut feed, &mut intake, &stop, &mut feed_failed) {
                FullCheck::Done(w, batch) => {
                    watcher = *w;
                    if !batch.is_empty() && tx.send(batch).is_err() {
                        drop(guard);
                        return (Some(watcher), true);
                    }
                    if feed_failed {
                        drop(guard);
                        return (Some(watcher), false);
                    }
                }
                FullCheck::Stopped => {
                    drop(guard);
                    return (None, true);
                }
                FullCheck::Lost => {
                    tracing::error!("watch scan thread panicked; file watching stopped");
                    drop(guard);
                    return (None, true);
                }
            }
        }};
    }

    // Seed baseline (!seeded → empty) or emit catch-up delta if already seeded.
    full_check!();

    while !stop.load(Ordering::Relaxed) {
        if drain_feed(&mut feed, STOP_SLICE, &mut intake).is_err() {
            drop(guard);
            return (Some(watcher), false);
        }

        // Query-time Repair([]) asks for a full check so a stale watermark
        // can advance without waiting for the safety interval.
        if watcher.freshness.take_verification_request() {
            intake.reset_for_full();
            last_safety = Instant::now();
            full_check!();
        }

        let quiet = intake
            .last_event
            .map(|t| t.elapsed() >= debounce)
            .unwrap_or(false)
            || intake
                .first_event
                .map(|t| t.elapsed() >= max_debounce)
                .unwrap_or(false);
        let wait = safety_wait(safety_interval, watcher.freshness.distrusted());
        let safety_due = !wait.is_zero() && last_safety.elapsed() >= wait;

        if quiet || safety_due {
            intake.last_event = None;
            intake.first_event = None;
            // Safety scans and storm/overflow always take the full-tree path.
            // Directed `check_paths` does not reset the safety clock, so a
            // distrusted log still gets a full scan within the 60 s cap.
            let storm = intake.all_dirty || intake.pending.len() > storm_paths;
            if safety_due || storm {
                intake.reset_for_full();
                last_safety = Instant::now();
                full_check!();
            } else {
                let paths: Vec<PathBuf> = intake.pending.drain().collect();
                match watcher.directed_check(&paths) {
                    Ok(batch) => {
                        if !batch.is_empty() && tx.send(batch).is_err() {
                            break;
                        }
                    }
                    Err(NeedFull { distrust }) => {
                        if distrust {
                            watcher.freshness.note_distrust();
                        }
                        intake.reset_for_full();
                        full_check!();
                    }
                }
            }
        }
    }

    // Flush pending debounce on stop, matching run_loop's pending flush.
    // Inline on purpose: the thread is exiting and the caller may join it.
    if intake.last_event.is_some() || !intake.pending.is_empty() || intake.all_dirty {
        let batch = if intake.all_dirty || intake.pending.len() > storm_paths {
            watcher.check()
        } else if intake.pending.is_empty() {
            ChangeSet::default()
        } else {
            let paths: Vec<PathBuf> = intake.pending.drain().collect();
            watcher.check_paths(&paths)
        };
        if !batch.is_empty() {
            let _ = tx.send(batch);
        }
    }

    drop(guard);
    (Some(watcher), true)
}

fn run_loop(mut watcher: Watcher, tx: mpsc::Sender<ChangeSet>, stop: Arc<AtomicBool>) {
    let poll_interval = clamp_interval(watcher.config.poll_interval);
    let debounce = watcher.config.debounce;
    let mut pending = ChangeSet::default();
    let mut last_activity: Option<Instant> = None;
    watcher.freshness.set_poll_cycle(Some(poll_interval));

    while !stop.load(Ordering::Relaxed) {
        let scan_started = Instant::now();
        let batch = watcher.check();
        // Keep full scans below ~10% duty cycle on huge trees.
        let scan_took = scan_started.elapsed();
        let poll_interval = if scan_took >= Duration::from_millis(200) {
            poll_interval.max(scan_took * 10)
        } else {
            poll_interval
        };
        // Watermark-to-watermark distance of this loop: sleep, then scan.
        watcher
            .freshness
            .set_poll_cycle(Some(poll_interval + scan_took));
        if !batch.is_empty() {
            pending.merge(batch);
            last_activity = Some(Instant::now());
        }

        let quiet = last_activity
            .map(|t| t.elapsed() >= debounce)
            .unwrap_or(false);
        if !pending.is_empty() && (debounce.is_zero() || quiet) {
            let emit = std::mem::take(&mut pending);
            last_activity = None;
            if tx.send(emit).is_err() {
                break;
            }
        }

        if wait_or_stop_checking(&stop, poll_interval, &mut watcher, &tx) {
            if !pending.is_empty() {
                let _ = tx.send(std::mem::take(&mut pending));
            }
            break;
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(pending);
    }
}

fn clamp_interval(d: Duration) -> Duration {
    if d.is_zero() {
        Duration::from_millis(1)
    } else {
        d
    }
}

/// Sleep up to `total`, waking every [`STOP_SLICE`] to honor stop and
/// on-demand [`FreshnessLog::request_verification`].
///
/// Returns true if stop was requested (or the channel closed during a
/// verification check).
fn wait_or_stop_checking(
    stop: &AtomicBool,
    total: Duration,
    watcher: &mut Watcher,
    tx: &mpsc::Sender<ChangeSet>,
) -> bool {
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        if watcher.freshness.take_verification_request() {
            let batch = watcher.check();
            if !batch.is_empty() && tx.send(batch).is_err() {
                return true;
            }
        }
        let elapsed = start.elapsed();
        if elapsed >= total {
            return stop.load(Ordering::Relaxed);
        }
        thread::sleep((total - elapsed).min(STOP_SLICE));
    }
}

struct Observed {
    mtime: SystemTime,
    len: u64,
    mtime_ok: bool,
}

fn observe_tree(root: &Path, opts: &ScanOptions) -> BTreeMap<RelPath, Observed> {
    let scanned = scan::scan(root, opts);
    let mut out = BTreeMap::new();
    for rel in scanned.files {
        let abs = root.join(rel.as_str());
        let Ok(meta) = abs.metadata() else {
            continue;
        };
        let (mtime, mtime_ok) = match meta.modified() {
            Ok(t) => (t, true),
            Err(_) => (SystemTime::UNIX_EPOCH, false),
        };
        out.insert(
            rel,
            Observed {
                mtime,
                len: meta.len(),
                mtime_ok,
            },
        );
    }
    out
}

fn observe_one(root: &Path, rel: &RelPath) -> Option<Observed> {
    let abs = root.join(rel.as_str());
    let meta = abs.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let (mtime, mtime_ok) = match meta.modified() {
        Ok(t) => (t, true),
        Err(_) => (SystemTime::UNIX_EPOCH, false),
    };
    Some(Observed {
        mtime,
        len: meta.len(),
        mtime_ok,
    })
}

fn stamps_from_observed(
    root: &Path,
    observed: &BTreeMap<RelPath, Observed>,
    now: SystemTime,
) -> BTreeMap<RelPath, FileStamp> {
    observed
        .iter()
        .map(|(path, meta)| (path.clone(), make_stamp(root, path, meta, now, None)))
        .collect()
}

fn make_stamp(
    root: &Path,
    path: &RelPath,
    meta: &Observed,
    now: SystemTime,
    prev_hash: Option<u64>,
) -> FileStamp {
    let unsettled = is_unsettled(meta.mtime, meta.mtime_ok, now);
    let want_hash = unsettled
        || (prev_hash.is_none()
            && (meta.len <= BASELINE_HASH_MAX_BYTES
                || is_far_future(meta.mtime, meta.mtime_ok, now)));
    let hash = if want_hash {
        hash_file(&root.join(path.as_str())).or(prev_hash)
    } else {
        prev_hash
    };
    FileStamp {
        mtime: meta.mtime,
        len: meta.len,
        hash,
        unsettled,
        mtime_ok: meta.mtime_ok,
    }
}

fn confirm(
    root: &Path,
    path: &RelPath,
    old: &FileStamp,
    meta: &Observed,
    now: SystemTime,
) -> (FileStamp, bool) {
    let abs = root.join(path.as_str());
    let mut unsettled = is_unsettled(meta.mtime, meta.mtime_ok, now);
    let meta_changed =
        meta.mtime != old.mtime || meta.len != old.len || meta.mtime_ok != old.mtime_ok;

    if !meta_changed && !old.unsettled && !unsettled {
        return (
            FileStamp {
                mtime: meta.mtime,
                len: meta.len,
                hash: old.hash,
                unsettled: false,
                mtime_ok: meta.mtime_ok,
            },
            false,
        );
    }

    // Suspected: hash only this file. Skip a full-tree content walk.
    let new_hash = hash_file(&abs);
    let content_changed = match (old.hash, new_hash) {
        (Some(prev), Some(next)) => prev != next,
        // First successful hash with no prior: only treat as modified when
        // metadata also changed. Settling unreadable→readable (or other
        // unsettled→hashed) with unchanged mtime/size must not spuriously
        // report modified — the hash becomes the new baseline.
        // Without a baseline hash (large file) a metadata change is the only
        // evidence we have.
        (None, Some(_)) => meta_changed,
        (None, None) => {
            if old.unsettled {
                unsettled = true;
            }
            meta_changed
        }
        (Some(_), None) => {
            unsettled = true;
            false
        }
    };

    let stamp = FileStamp {
        mtime: meta.mtime,
        len: meta.len,
        hash: new_hash.or(old.hash),
        unsettled,
        mtime_ok: meta.mtime_ok,
    };
    (stamp, content_changed)
}

fn is_unsettled(mtime: SystemTime, mtime_ok: bool, now: SystemTime) -> bool {
    if !mtime_ok {
        return true;
    }
    match now.duration_since(mtime) {
        Ok(age) => age < MTIME_UNTRUSTED_WINDOW,
        // Small clock skew stays untrusted; a far-future mtime (tar unpack,
        // container / NFS) would otherwise force a full re-read every scan.
        Err(e) => e.duration() <= FAR_FUTURE,
    }
}

/// Mtimes further ahead than this are hashed once and then trusted via
/// `len` + `mtime` like any settled file.
const FAR_FUTURE: Duration = Duration::from_secs(24 * 3600);

/// Files up to this size get a baseline hash when first seen so a later
/// touch / checkout that leaves content intact is not reported as modified.
const BASELINE_HASH_MAX_BYTES: u64 = 64 * 1024;

fn is_far_future(mtime: SystemTime, mtime_ok: bool, now: SystemTime) -> bool {
    mtime_ok && mtime.duration_since(now).is_ok_and(|d| d > FAR_FUTURE)
}

fn hash_file(path: &Path) -> Option<u64> {
    // Word-at-a-time multiply-xorshift mix: several times faster than the
    // byte-wise FNV loop. Only compared against earlier values of itself.
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut file = File::open(path).ok()?;
    let mut buf = [0u8; 65536];
    let mut h: u64 = FNV_OFFSET;
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        total += n as u64;
        let mut chunks = buf[..n].chunks_exact(8);
        for c in &mut chunks {
            let w = u64::from_le_bytes(c.try_into().expect("8 bytes"));
            h = (h ^ w).wrapping_mul(K);
            h ^= h >> 29;
        }
        for b in chunks.remainder() {
            h = (h ^ u64::from(*b)).wrapping_mul(K);
            h ^= h >> 29;
        }
    }
    h ^= total;
    Some(h.wrapping_mul(K))
}

/// Whether `abs` is, or lies below, a symlink that points at a directory
/// (checked for every component between `root` and `abs`, using `lstat`).
fn crosses_symlink_dir(root: &Path, abs: &Path) -> bool {
    let mut cur = abs;
    loop {
        if cur == root {
            return false;
        }
        if let Ok(md) = fs::symlink_metadata(cur) {
            if md.file_type().is_symlink() && cur.is_dir() {
                return true;
            }
        }
        match cur.parent() {
            Some(p) if p.starts_with(root) => cur = p,
            _ => return false,
        }
    }
}

fn relativize(root: &Path, path: &Path) -> Option<RelPath> {
    let stripped = path.strip_prefix(root).ok()?;
    if stripped.as_os_str().is_empty() {
        return None;
    }
    Some(RelPath::new(stripped.to_string_lossy().as_ref()))
}

/// [`scan::is_default_excluded_dir`] with a short-lived per-thread cache:
/// for `target`/`build`/`dist`/`obj` names it lists the parent directory to
/// look for a project manifest, which is wasteful per event in a build storm.
fn default_excluded_cached(root: &Path, dir: &Path) -> bool {
    use std::cell::RefCell;
    use std::collections::HashMap;
    const TTL: Duration = Duration::from_secs(2);
    thread_local! {
        static CACHE: RefCell<HashMap<PathBuf, (Instant, bool)>> = RefCell::new(HashMap::new());
    }
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !matches!(name, "target" | "build" | "dist" | "obj") {
        return crate::scan::is_default_excluded_dir(root, dir);
    }
    let now = Instant::now();
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((t, v)) = c.get(dir) {
            if now.duration_since(*t) < TTL {
                return *v;
            }
        }
        if c.len() > 4096 {
            c.clear();
        }
        let v = crate::scan::is_default_excluded_dir(root, dir);
        c.insert(dir.to_path_buf(), (now, v));
        v
    })
}

fn is_excluded_rel(root: &Path, rel: &RelPath, extra: &[String]) -> bool {
    let s = rel.as_str();
    let mut dir = root.to_path_buf();
    for comp in s.split('/') {
        if comp.is_empty() {
            continue;
        }
        dir.push(comp);
        if default_excluded_cached(root, &dir) {
            return true;
        }
    }
    for raw in extra {
        let ex = RelPath::new(raw.as_str());
        let e = ex.as_str().trim_end_matches('/');
        if e.is_empty() {
            continue;
        }
        if s == e || (s.len() > e.len() && s.starts_with(e) && s.as_bytes()[e.len()] == b'/') {
            return true;
        }
    }
    false
}

fn rel_is_under(path: &RelPath, dir: &str) -> bool {
    let p = path.as_str();
    if dir.is_empty() {
        return true;
    }
    p == dir || (p.len() > dir.len() && p.starts_with(dir) && p.as_bytes()[dir.len()] == b'/')
}

fn keep_event_path(root: &Path, scan: &ScanOptions, path: &Path) -> Option<(PathBuf, RelPath)> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let rel = relativize(root, &abs)?;
    if rel.as_str() == ".git/info/exclude" {
        // Not content, but it changes the admitted set like a `.gitignore`.
        return Some((abs, rel));
    }
    if is_excluded_rel(root, &rel, &scan.extra_excludes) {
        return None;
    }
    Some((abs, rel))
}

struct Storm;

fn expand_dir_bounded(
    root: &Path,
    dir: &Path,
    extra: &[String],
    cap: usize,
    out: &mut BTreeSet<RelPath>,
) -> Result<(), Storm> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let Some(rel) = relativize(root, &p) else {
                continue;
            };
            if is_excluded_rel(root, &rel, extra) {
                continue;
            }
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                out.insert(rel);
                if out.len() > cap {
                    return Err(Storm);
                }
            }
        }
    }
    Ok(())
}

enum Admit {
    Indexed,
    Missing,
    Skip,
}

/// Admission of a newly observed path into the watched set.
///
/// Delegates to [`scan::path_admission`] (WP-A2) so gitignore, size, and
/// binary gates match the indexer.
fn admit_new(root: &Path, rel: &RelPath, opts: &ScanOptions) -> Admit {
    match scan::path_admission(root, rel, opts) {
        scan::PathAdmission::Indexed => Admit::Indexed,
        scan::PathAdmission::Missing => Admit::Missing,
        scan::PathAdmission::Ignored
        | scan::PathAdmission::TooLarge
        | scan::PathAdmission::Binary => Admit::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_build_dirs_are_watched_but_project_outputs_are_not() {
        let root =
            std::env::temp_dir().join(format!("astrolabe-watch-excl-{}", std::process::id()));
        std::fs::create_dir_all(root.join("internal/build")).unwrap();
        std::fs::create_dir_all(root.join("web/dist")).unwrap();
        std::fs::write(root.join("web/package.json"), "{}").unwrap();
        assert!(!is_excluded_rel(
            &root,
            &RelPath::new("internal/build/b.go"),
            &[]
        ));
        assert!(is_excluded_rel(&root, &RelPath::new("web/dist/x.js"), &[]));
        assert!(is_excluded_rel(
            &root,
            &RelPath::new("target/debug/x.rs"),
            &[]
        ));
        assert!(is_excluded_rel(
            &root,
            &RelPath::new("a/node_modules/x.js"),
            &[]
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::UNIX_EPOCH;

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
                "astrolabe-watch-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            let path = fs::canonicalize(&path).expect("canonicalize test directory");
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
            fs::write(&path, contents).expect("write test file");
        }

        fn abs(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_scan() -> ScanOptions {
        ScanOptions {
            respect_gitignore: false,
            audit_ignored: false,
            ..ScanOptions::default()
        }
    }

    fn watcher(dir: &TestDir) -> Watcher {
        // Jitter would make the first safety scan fire immediately with
        // probability STOP_SLICE/interval; keep tests deterministic.
        Watcher::new(dir.path())
            .scan_options(test_scan())
            .safety_jitter(false)
    }

    /// Bump mtime so tests do not depend on filesystem timestamp resolution.
    fn bump_mtime(path: &Path) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime bump");
        let old = file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or_else(SystemTime::now);
        file.set_modified(old + Duration::from_secs(2))
            .expect("set_modified");
    }

    fn rewrite_and_bump(dir: &TestDir, rel: &str, contents: impl AsRef<[u8]>) {
        dir.write(rel, contents);
        bump_mtime(&dir.abs(rel));
    }

    #[test]
    fn repaired_path_is_reported_even_when_content_matches_snapshot() {
        // The barrier re-read an intermediate edit into the index, then the
        // file was restored to its snapshot bytes before the debounce fired.
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        let _ = w.check();
        let log = Arc::clone(&w.freshness);
        log.note_repaired(&[RelPath::new("a.rs"), RelPath::new("gone.rs")]);
        rewrite_and_bump(&dir, "a.rs", "fn a() {}");
        let cs = w.check_paths(&[dir.abs("a.rs"), dir.abs("gone.rs")]);
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert_eq!(cs.removed, vec![RelPath::new("gone.rs")]);
        // Reported changes stay suspects until the index absorbs them.
        let suspects = log.suspect_paths(log.watermark());
        assert!(suspects.contains(&RelPath::new("a.rs")), "{suspects:?}");
        // The mark is consumed: a plain re-check is quiet again.
        assert!(w.check_paths(&[dir.abs("a.rs")]).is_empty());
    }

    #[test]
    fn full_check_reports_repaired_paths_and_consumes_them() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        let _ = w.check();
        w.freshness.note_repaired(&[RelPath::new("a.rs")]);
        let cs = w.check();
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert!(w.check().is_empty());
    }

    #[test]
    fn renoting_a_suspect_restamps_it_without_duplicates() {
        let log = FreshnessLog::new();
        log.note_paths(&[RelPath::new("a.rs")]);
        let first = Instant::now();
        log.clear_suspects_through(first);
        assert!(log.suspect_paths(log.watermark()).is_empty());
        log.note_paths(&[RelPath::new("a.rs"), RelPath::new("a.rs")]);
        log.note_paths(&[RelPath::new("a.rs")]);
        assert_eq!(log.suspect_paths(first), vec![RelPath::new("a.rs")]);
        assert_eq!(log.lock().suspects.len(), 1);
    }

    #[test]
    fn detects_added_file() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        assert_eq!(w.tracked_files(), 1);

        dir.write("nested/b.rs", "fn b() {}");
        let cs = w.check();
        assert_eq!(cs.added, vec![RelPath::new("nested/b.rs")]);
        assert!(cs.modified.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn detects_modified_file() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        rewrite_and_bump(&dir, "a.rs", "fn a() { 1 }");
        let cs = w.check();
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn detects_removed_file() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        dir.write("b.rs", "fn b() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        fs::remove_file(dir.abs("a.rs")).expect("remove");
        let cs = w.check();
        assert_eq!(cs.removed, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.modified.is_empty());
    }

    #[test]
    fn no_changes_returns_empty() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        assert!(w.check().is_empty());
        assert!(w.check().is_empty());
        assert_eq!(w.tracked_files(), 1);
    }

    #[test]
    fn changeset_order_is_deterministic() {
        let dir = TestDir::new();
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        dir.write("z.rs", "z");
        dir.write("a.rs", "a");
        dir.write("m.rs", "m");
        dir.write("nested/b.rs", "b");
        let cs = w.check();
        assert_eq!(
            cs.added,
            vec![
                RelPath::new("a.rs"),
                RelPath::new("m.rs"),
                RelPath::new("nested/b.rs"),
                RelPath::new("z.rs"),
            ]
        );
        let kinds: Vec<_> = cs
            .iter()
            .map(|(k, p)| (k, p.as_str().to_string()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (ChangeKind::Added, "a.rs".into()),
                (ChangeKind::Added, "m.rs".into()),
                (ChangeKind::Added, "nested/b.rs".into()),
                (ChangeKind::Added, "z.rs".into()),
            ]
        );
    }

    #[test]
    fn merge_coalesces_editor_bursts() {
        let mut pending = ChangeSet {
            added: vec![RelPath::new("new.rs")],
            modified: vec![RelPath::new("old.rs")],
            removed: Vec::new(),
        };
        pending.merge(ChangeSet {
            added: Vec::new(),
            modified: vec![RelPath::new("new.rs")],
            removed: vec![RelPath::new("old.rs"), RelPath::new("gone.rs")],
        });
        pending.merge(ChangeSet {
            added: vec![RelPath::new("gone.rs")],
            modified: Vec::new(),
            removed: vec![RelPath::new("new.rs")],
        });
        assert!(pending.added.is_empty(), "{pending:?}");
        assert_eq!(pending.modified, vec![RelPath::new("gone.rs")]);
        // `new.rs` was added then removed: the removal must survive (it may
        // have been applied to the index out of band), so no ghost remains.
        assert_eq!(
            pending.removed,
            vec![RelPath::new("new.rs"), RelPath::new("old.rs")]
        );
    }

    #[test]
    fn added_then_removed_keeps_removal() {
        let mut cs = ChangeSet {
            added: vec![RelPath::new("x.rs")],
            ..ChangeSet::default()
        };
        cs.merge(ChangeSet {
            removed: vec![RelPath::new("x.rs")],
            ..ChangeSet::default()
        });
        assert_eq!(cs.removed, vec![RelPath::new("x.rs")]);
        assert!(cs.added.is_empty() && cs.modified.is_empty());
    }

    #[test]
    fn directed_check_keeps_changed_paths_as_suspects_after_watermark() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        let log = Arc::clone(&w.freshness);
        w.check();
        rewrite_and_bump(&dir, "a.rs", "fn a() { 1 }");
        let ch = w.check_paths(&[dir.abs("a.rs")]);
        assert_eq!(ch.modified, vec![RelPath::new("a.rs")]);
        // Delta is produced but not applied: it must still be a suspect.
        assert_eq!(
            log.suspect_paths(log.watermark()),
            vec![RelPath::new("a.rs")]
        );
        log.clear_suspects_through(Instant::now());
        assert!(log.suspect_paths(log.watermark()).is_empty());
        // A no-change directed check leaves no suspects.
        w.check_paths(&[dir.abs("a.rs")]);
        assert!(log.suspect_paths(log.watermark()).is_empty());
    }

    #[test]
    fn full_check_keeps_changes_as_suspects() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        let log = Arc::clone(&w.freshness);
        w.check();
        dir.write("b.rs", "fn b() {}");
        let ch = w.check();
        assert_eq!(ch.added, vec![RelPath::new("b.rs")]);
        assert_eq!(
            log.suspect_paths(log.watermark()),
            vec![RelPath::new("b.rs")]
        );
    }

    fn default_scan_watcher(dir: &TestDir) -> Watcher {
        Watcher::new(dir.path())
            .scan_options(ScanOptions::default())
            .safety_jitter(false)
    }

    #[test]
    fn directed_check_drops_newly_gitignored_tracked_file() {
        let dir = TestDir::new();
        dir.write("keep.rs", "fn k() {}");
        dir.write("gen.rs", "fn g() {}");
        let mut w = default_scan_watcher(&dir);
        w.check();
        dir.write(".gitignore", "gen.rs\n");
        let ch = w.check_paths(&[dir.abs("gen.rs")]);
        assert_eq!(ch.removed, vec![RelPath::new("gen.rs")], "{ch:?}");
    }

    #[test]
    fn directed_check_drops_file_grown_past_size_cap() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = default_scan_watcher(&dir);
        w.check();
        dir.write("a.rs", vec![b'x'; 1_600_000]);
        let ch = w.check_paths(&[dir.abs("a.rs")]);
        assert_eq!(ch.removed, vec![RelPath::new("a.rs")], "{ch:?}");
        assert!(w.check().is_empty(), "full scan agrees");
    }

    #[test]
    fn directed_admission_honors_dot_ignore_and_git_exclude() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        dir.write(".git/info/.keep", "");
        dir.write(".ignore", "skipme.rs\n");
        dir.write(".git/info/exclude", "hidden.rs\n");
        let mut w = default_scan_watcher(&dir);
        w.check();
        dir.write("skipme.rs", "fn s() {}");
        dir.write("hidden.rs", "fn h() {}");
        dir.write("shown.rs", "fn v() {}");
        let ch = w.check_paths(&[
            dir.abs("skipme.rs"),
            dir.abs("hidden.rs"),
            dir.abs("shown.rs"),
        ]);
        assert_eq!(ch.added, vec![RelPath::new("shown.rs")], "{ch:?}");
        assert!(w.check().is_empty(), "directed and full agree: no flapping");
    }

    #[test]
    fn ignore_rule_file_events_are_recognised() {
        assert!(is_ignore_rule_file(&RelPath::new(".gitignore")));
        assert!(is_ignore_rule_file(&RelPath::new("sub/.ignore")));
        assert!(is_ignore_rule_file(&RelPath::new(".git/info/exclude")));
        assert!(!is_ignore_rule_file(&RelPath::new("a.rs")));
        assert!(!is_ignore_rule_file(&RelPath::new("x.gitignore")));
    }

    struct BusyFeed {
        paths: Vec<PathBuf>,
    }

    impl EventFeed for BusyFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            // The paths change on every wake, so the debounce never goes quiet.
            if timeout.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            thread::sleep(Duration::from_millis(20));
            Ok(FeedWake::Events {
                paths: self.paths.clone(),
                overflow: false,
            })
        }
    }

    #[test]
    fn continuous_events_cannot_starve_delivery() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        dir.write("busy.log", "0");
        let mut w = watcher(&dir).debounce(Duration::from_millis(300));
        w.check();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let feed = BusyFeed {
            paths: vec![dir.abs("busy.log"), dir.abs("a.rs")],
        };
        let stop2 = Arc::clone(&stop);
        let h = thread::spawn(move || {
            event_loop(w, feed, (), tx, stop2);
        });
        // After the loop's catch-up check, edit a file while events never stop.
        thread::sleep(Duration::from_millis(150));
        rewrite_and_bump(&dir, "a.rs", "fn a() { 2 }");
        let started = Instant::now();
        let got = rx.recv_timeout(Duration::from_secs(8));
        let waited = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        let got = got.expect("batch delivered despite constant events");
        assert!(got.modified.contains(&RelPath::new("a.rs")), "{got:?}");
        assert!(waited < Duration::from_secs(6), "waited {waited:?}");
    }
    #[test]
    fn debounce_collapses_rapid_events() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(40))
            .debounce(Duration::from_millis(180))
            .spawn();

        thread::sleep(Duration::from_millis(80));
        dir.write("a.rs", "a");
        thread::sleep(Duration::from_millis(50));
        dir.write("b.rs", "b");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("debounced changeset");
        assert_eq!(
            cs.added,
            vec![RelPath::new("a.rs"), RelPath::new("b.rs")],
            "{cs:?}"
        );
        assert!(cs.modified.is_empty());
        assert!(cs.removed.is_empty());

        match rx.recv_timeout(Duration::from_millis(250)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("debounce leaked a second event: {other:?}"),
        }

        handle.stop();
    }

    #[test]
    fn background_thread_stops() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(10))
            .spawn();

        thread::sleep(Duration::from_millis(40));
        assert!(!handle.is_finished());
        handle.stop();

        let start = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    if start.elapsed() > Duration::from_secs(2) {
                        panic!("watch thread did not drop the sender after stop");
                    }
                }
                Ok(_) => {}
            }
        }
    }

    #[test]
    fn pending_changes_flushed_on_stop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(20))
            .debounce(Duration::from_secs(10)) // Long debounce so changes stay pending
            .spawn();

        thread::sleep(Duration::from_millis(50));
        dir.write("added.rs", "content");
        // Give time for poll to detect the file and put it in pending
        thread::sleep(Duration::from_millis(60));

        handle.stop();

        // The pending change should be flushed when stopping despite long debounce
        let cs = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("flushed changeset on stop");
        assert_eq!(cs.added, vec![RelPath::new("added.rs")]);
    }

    #[test]
    fn temporary_unreadable_file_handling() {
        let dir = TestDir::new();
        dir.write("test.rs", "initial content");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        let path = dir.abs("test.rs");

        // Simulate file becoming temporarily unreadable (000 permissions on unix).
        // `scan` drops ReadError paths from the watched set, so the file is
        // removed while unreadable and re-added when readable again — not a
        // confirm()-level "modified" cycle.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

            let cs1 = w.check();
            assert!(cs1.modified.is_empty());
            assert_eq!(cs1.removed, vec![RelPath::new("test.rs")]);

            let cs2 = w.check();
            assert!(
                cs2.is_empty(),
                "must not keep emitting while unreadable: {cs2:?}"
            );

            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

            let cs3 = w.check();
            assert!(cs3.modified.is_empty());
            assert_eq!(cs3.added, vec![RelPath::new("test.rs")]);
            assert!(cs3.removed.is_empty());
        }
    }

    #[test]
    fn confirm_settle_unreadable_to_readable_no_spurious_modified() {
        let dir = TestDir::new();
        dir.write("test.rs", "unchanged bytes");
        let rel = RelPath::new("test.rs");
        let abs = dir.abs("test.rs");
        let meta_fs = abs.metadata().expect("metadata");
        let mtime = meta_fs.modified().expect("mtime");
        let observed = Observed {
            mtime,
            len: meta_fs.len(),
            mtime_ok: true,
        };
        // Prior observe could not hash (unreadable); metadata unchanged.
        let old = FileStamp {
            mtime,
            len: meta_fs.len(),
            hash: None,
            unsettled: true,
            mtime_ok: true,
        };
        // Far enough in the future that mtime is outside the untrusted window.
        let now = mtime + MTIME_UNTRUSTED_WINDOW + Duration::from_secs(1);
        let (stamp, changed) = confirm(dir.path(), &rel, &old, &observed, now);
        assert!(
            !changed,
            "settling to a first hash with unchanged mtime/size must not report modified"
        );
        assert!(stamp.hash.is_some());
        assert!(!stamp.unsettled);
    }

    #[test]
    fn first_check_is_baseline_not_adds() {
        let dir = TestDir::new();
        dir.write("a.rs", "a");
        dir.write("b.rs", "b");
        let mut w = watcher(&dir);
        let cs = w.check();
        assert!(cs.is_empty());
        assert_eq!(w.tracked_files(), 2);
        assert!(w.is_seeded());
    }

    #[test]
    fn same_size_rewrite_inside_untrusted_window_is_detected() {
        let dir = TestDir::new();
        dir.write("a.rs", "AAAA");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        // Same length, rewrite through the existing inode so a coarse
        // clock might keep the same mtime. The unsettled-window hash
        // must still see the new bytes.
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(dir.abs("a.rs"))
                .expect("reopen");
            f.write_all(b"BBBB").expect("rewrite");
            f.flush().expect("flush");
        }

        let cs = w.check();
        assert_eq!(
            cs.modified,
            vec![RelPath::new("a.rs")],
            "same-size rewrite must be visible even when mtime is coarse"
        );
    }

    #[test]
    fn event_mode_detects_added() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir).debounce(Duration::from_millis(50)).spawn();

        thread::sleep(Duration::from_millis(100));
        dir.write("added.rs", "added content");

        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event mode detected added file");
        assert_eq!(cs.added, vec![RelPath::new("added.rs")]);
        handle.stop();
    }

    #[test]
    fn event_mode_detects_modified() {
        let dir = TestDir::new();
        dir.write("a.rs", "initial");
        let (rx, handle) = watcher(&dir).debounce(Duration::from_millis(50)).spawn();

        thread::sleep(Duration::from_millis(100));
        rewrite_and_bump(&dir, "a.rs", "modified content");

        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event mode detected modified file");
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        handle.stop();
    }

    #[test]
    fn event_mode_detects_removed() {
        let dir = TestDir::new();
        dir.write("a.rs", "initial");
        let (rx, handle) = watcher(&dir).debounce(Duration::from_millis(50)).spawn();

        thread::sleep(Duration::from_millis(100));
        fs::remove_file(dir.abs("a.rs")).expect("remove");

        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event mode detected removed file");
        assert_eq!(cs.removed, vec![RelPath::new("a.rs")]);
        handle.stop();
    }

    #[test]
    fn forced_poll_mode_detects_change() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20))
            .spawn();

        thread::sleep(Duration::from_millis(50));
        dir.write("poll_new.rs", "content");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("poll mode detected added file");
        assert_eq!(cs.added, vec![RelPath::new("poll_new.rs")]);
        handle.stop();
    }

    struct ErrorFeed {
        yielded: bool,
    }

    impl EventFeed for ErrorFeed {
        fn recv_timeout(&mut self, _timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            if !self.yielded {
                self.yielded = true;
                Ok(FeedWake::Error)
            } else {
                Err(RecvTimeoutError::Timeout)
            }
        }
    }

    #[test]
    fn spawn_preserves_seeded_pre_spawn_changeset() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20));
        assert!(w.check().is_empty());

        // Change after seed, before spawn — must not be discarded.
        dir.write("between.rs", "between");
        let (rx, handle) = w.spawn();

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("seeded pre-spawn changeset");
        assert_eq!(cs.added, vec![RelPath::new("between.rs")], "{cs:?}");
        handle.stop();
    }

    struct OneShotFeed {
        fired: bool,
        path: PathBuf,
    }

    impl EventFeed for OneShotFeed {
        fn recv_timeout(&mut self, _timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            if !self.fired {
                self.fired = true;
                Ok(FeedWake::Events {
                    paths: vec![self.path.clone()],
                    overflow: false,
                })
            } else {
                Err(RecvTimeoutError::Timeout)
            }
        }
    }

    #[test]
    fn event_loop_flushes_pending_debounce_on_stop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let flushed = dir.abs("flushed.rs");
        let mut w = watcher(&dir).debounce(Duration::from_secs(60));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = OneShotFeed {
            fired: false,
            path: flushed,
        };

        let thread = thread::spawn(move || {
            let _ = event_loop(w, feed, (), tx, stop_thread);
        });

        // Wait until the one-shot event arms last_event inside the loop.
        thread::sleep(Duration::from_millis(80));
        dir.write("flushed.rs", "flush me");
        thread::sleep(Duration::from_millis(40));

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");

        let cs = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pending debounce flushed on stop");
        assert_eq!(cs.added, vec![RelPath::new("flushed.rs")], "{cs:?}");
    }

    struct TimeoutFeed;

    impl EventFeed for TimeoutFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            // Honor the timeout so the loop does not busy-spin; event_loop
            // passes STOP_SLICE (50 ms) here.
            if !timeout.is_zero() {
                thread::sleep(timeout);
            }
            Err(RecvTimeoutError::Timeout)
        }
    }

    #[test]
    fn watch_config_default_safety_interval_is_thirty_minutes() {
        let cfg = WatchConfig::default();
        assert_eq!(cfg.safety_interval, Duration::from_secs(1800));
        assert_eq!(cfg.storm_paths, 5000);
        assert!(cfg.safety_jitter);
        assert_eq!(
            WatchConfig::default()
                .safety_interval(Duration::from_secs(30))
                .safety_jitter(false)
                .safety_interval,
            Duration::from_secs(30)
        );
        assert!(!WatchConfig::default().safety_jitter(false).safety_jitter);
        assert_eq!(WatchConfig::default().storm_paths(7).storm_paths, 7);
    }

    #[test]
    fn safety_phase_offset_stays_inside_interval() {
        let interval = Duration::from_secs(1800);
        for seed in [0u64, 1, 42, u64::MAX] {
            let offset = safety_phase_offset(interval, seed);
            assert!(offset < interval, "seed={seed} offset={offset:?}");
        }
        assert_eq!(safety_phase_offset(Duration::ZERO, 1), Duration::ZERO);
        assert_eq!(
            safety_phase_offset(Duration::from_nanos(1), 0),
            Duration::ZERO
        );
    }

    #[test]
    fn read_only_access_events_are_not_content_events() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind};
        use notify::EventKind;
        assert!(!is_content_event(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_content_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_content_event(&EventKind::Access(AccessKind::Read)));
        assert!(is_content_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_content_event(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_content_event(&EventKind::Create(CreateKind::File)));
    }

    /// Regression: reading files in an untouched tree (our own scans,
    /// rebuilds and search reads) must not leave suspects behind, or the
    /// query barrier reports a permanently stale index.
    #[test]
    fn reading_untouched_tree_leaves_no_suspects() {
        let dir = TestDir::new();
        for i in 0..20 {
            dir.write(&format!("f{i}.rs"), "fn a() {}");
        }
        // Long debounce: directed checks never run, so any noted suspect
        // would stay visible.
        let w = watcher(&dir).debounce(Duration::from_secs(60));
        let (_rx, handle, log) = w.spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            thread::sleep(Duration::from_millis(5));
        }
        let since = log.watermark();
        thread::sleep(Duration::from_millis(100));
        for _ in 0..3 {
            for i in 0..20 {
                let _ = fs::read_to_string(dir.path().join(format!("f{i}.rs")));
            }
        }
        thread::sleep(Duration::from_millis(300));
        assert!(
            log.suspect_paths(since).is_empty(),
            "reads produced suspects: {:?}",
            log.suspect_paths(since)
        );
        // Sanity: a real write is still noted.
        dir.write("f0.rs", "fn b() {}");
        let start = Instant::now();
        while log.suspect_paths(since).is_empty() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "write was not noted as a suspect"
            );
            thread::sleep(Duration::from_millis(10));
        }
        handle.stop();
    }

    #[test]
    fn distrust_caps_safety_wait_at_sixty_seconds() {
        assert_eq!(
            safety_wait(Duration::from_secs(1800), true),
            Duration::from_secs(60)
        );
        assert_eq!(
            safety_wait(Duration::from_millis(80), true),
            Duration::from_millis(80)
        );
        assert_eq!(safety_wait(Duration::ZERO, false), Duration::ZERO);
        assert_eq!(safety_wait(Duration::ZERO, true), Duration::from_secs(60));
        assert_eq!(
            safety_wait(Duration::from_secs(1800), false),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn event_loop_safety_interval_scans_without_os_events() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_millis(80))
            .debounce(Duration::from_secs(60));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let _ = event_loop(w, TimeoutFeed, (), tx, stop_thread);
        });

        // Let the loop seed, then write so only the safety scan (not the
        // startup check) can observe the new file.
        thread::sleep(Duration::from_millis(30));
        dir.write("late.rs", "late");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("configured safety interval should pick up a silent write");
        assert_eq!(cs.added, vec![RelPath::new("late.rs")], "{cs:?}");

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");
    }

    #[test]
    fn event_feed_error_degrades_to_polling() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let feed = ErrorFeed { yielded: false };
        let dummy_guard = ();

        let thread = thread::spawn(move || {
            let (watcher, ok) =
                event_loop(w, feed, dummy_guard, tx.clone(), Arc::clone(&stop_thread));
            assert!(!ok, "event_loop should report failure on backend error");
            run_loop(
                watcher.expect("watcher kept on feed error"),
                tx,
                stop_thread,
            );
        });

        thread::sleep(Duration::from_millis(50));
        dir.write("fallback.rs", "fallback content");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("polling fallback detected change");
        assert_eq!(cs.added, vec![RelPath::new("fallback.rs")]);

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join");
    }

    struct BurstFeed {
        paths: Vec<PathBuf>,
        overflow: bool,
        fired: bool,
    }

    impl EventFeed for BurstFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            if !self.fired {
                self.fired = true;
                return Ok(FeedWake::Events {
                    paths: std::mem::take(&mut self.paths),
                    overflow: self.overflow,
                });
            }
            if timeout.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            thread::sleep(timeout);
            Err(RecvTimeoutError::Timeout)
        }
    }

    #[test]
    fn directed_check_does_not_follow_symlinked_dirs() {
        let outer = TestDir::new();
        outer.write("d/inner.rs", "fn i() {}");
        outer.write("ext.rs", "fn e() {}");
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        let _ = w.check();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outer.path(), dir.abs("linkdir")).unwrap();
            let cs = w.check_paths(&[dir.abs("linkdir"), dir.abs("linkdir/ext.rs")]);
            assert!(cs.is_empty(), "{cs:?}");
            let full = w.check();
            assert!(full.is_empty(), "{full:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn selective_watch_skips_excluded_dirs_and_adopts_new_ones() {
        let dir = TestDir::new();
        dir.write("src/a.rs", "fn a() {}");
        dir.write("node_modules/p/i.js", "x");
        let scan = test_scan();
        let (mut feed, ()) = init_notify(dir.path(), &scan, true).expect("init");
        {
            let sel = feed.selective.as_ref().unwrap();
            assert!(sel.dirs.contains(&dir.abs("src")));
            assert!(!sel
                .dirs
                .iter()
                .any(|d| d.to_string_lossy().contains("node_modules")));
        }
        dir.write("fresh/sub/x.rs", "fn x() {}");
        let mut seen = Vec::new();
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if let Ok(FeedWake::Events { paths, .. }) =
                feed.recv_timeout(Duration::from_millis(100))
            {
                seen.extend(paths);
            }
            if feed
                .selective
                .as_ref()
                .unwrap()
                .dirs
                .contains(&dir.abs("fresh/sub"))
            {
                break;
            }
        }
        assert!(feed
            .selective
            .as_ref()
            .unwrap()
            .dirs
            .contains(&dir.abs("fresh/sub")));
        dir.write("fresh/sub/y.rs", "fn y() {}");
        let end = Instant::now() + Duration::from_secs(3);
        let mut got = false;
        while Instant::now() < end && !got {
            if let Ok(FeedWake::Events { paths, .. }) =
                feed.recv_timeout(Duration::from_millis(100))
            {
                got = paths.iter().any(|p| p.ends_with("fresh/sub/y.rs"));
            }
        }
        assert!(got, "event from a newly adopted directory");
        dir.write("node_modules/p/j.js", "y");
        let mut leaked = false;
        for _ in 0..5 {
            if let Ok(FeedWake::Events { paths, .. }) =
                feed.recv_timeout(Duration::from_millis(100))
            {
                leaked |= paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("node_modules"));
            }
        }
        assert!(!leaked, "excluded tree must not produce events");
    }

    #[test]
    fn touch_without_content_change_is_not_modified() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        let _ = w.check();
        // Move mtime well outside the untrusted window, content untouched.
        let f = fs::File::options()
            .write(true)
            .open(dir.abs("a.rs"))
            .unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        drop(f);
        let _ = w.check(); // observes meta change; hashes now settled
        let f = fs::File::options()
            .write(true)
            .open(dir.abs("a.rs"))
            .unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        drop(f);
        let cs = w.check();
        assert!(cs.is_empty(), "touch must not report modified: {cs:?}");
        let cs = w.check_paths(&[dir.abs("a.rs")]);
        assert!(cs.is_empty(), "{cs:?}");
        // A real content change with a new mtime is still reported.
        rewrite_and_bump(&dir, "a.rs", "fn a() { 2 }");
        let f = fs::File::options()
            .write(true)
            .open(dir.abs("a.rs"))
            .unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(9000))
            .unwrap();
        drop(f);
        assert_eq!(w.check().modified, vec![RelPath::new("a.rs")]);
    }

    #[test]
    fn far_future_mtime_is_not_permanently_unsettled() {
        let now = SystemTime::now();
        let skew = now + Duration::from_secs(30);
        let far = now + Duration::from_secs(10 * 24 * 3600);
        assert!(is_unsettled(skew, true, now));
        assert!(!is_unsettled(far, true, now));
        assert!(is_far_future(far, true, now));
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let f = fs::File::options()
            .write(true)
            .open(dir.abs("a.rs"))
            .unwrap();
        f.set_modified(far).unwrap();
        drop(f);
        let mut w = watcher(&dir);
        let _ = w.check();
        let st = w.snapshot.get(&RelPath::new("a.rs")).unwrap();
        assert!(!st.unsettled && st.hash.is_some());
        assert!(w.check().is_empty());
    }

    #[test]
    fn hash_file_detects_changes_incl_tail_bytes() {
        let dir = TestDir::new();
        dir.write("h", "0123456789abcdefX");
        let a = hash_file(&dir.abs("h")).unwrap();
        dir.write("h", "0123456789abcdefY");
        let b = hash_file(&dir.abs("h")).unwrap();
        dir.write("h", "0123456789abcdef");
        let c = hash_file(&dir.abs("h")).unwrap();
        assert!(a != b && a != c && b != c);
        assert_eq!(b, {
            dir.write("h", "0123456789abcdefY");
            hash_file(&dir.abs("h")).unwrap()
        });
    }

    #[test]
    fn excluded_events_do_not_trigger_check() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        dir.write(".git/HEAD", "ref: refs/heads/main");
        dir.write("target/out.rs", "x");
        dir.write("node_modules/pkg/index.js", "x");
        dir.write(".astrolabe/store", "x");
        let mut scan = test_scan();
        scan.extra_excludes = vec!["scratch".into()];
        dir.write("scratch/tmp.rs", "x");

        let mut w = watcher(&dir)
            .scan_options(scan)
            .debounce(Duration::from_millis(30));
        assert!(w.check().is_empty());
        let seed_mtime = w
            .snapshot
            .get(&RelPath::new("seed.rs"))
            .expect("seed stamp")
            .mtime;
        let checks = Arc::clone(&w.check_calls);
        let path_checks = Arc::clone(&w.check_paths_calls);
        let before_full = checks.load(Ordering::Relaxed);
        let before_paths = path_checks.load(Ordering::Relaxed);

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = BurstFeed {
            paths: vec![
                dir.abs(".git/HEAD"),
                dir.abs("target/out.rs"),
                dir.abs("node_modules/pkg/index.js"),
                dir.abs(".astrolabe/store"),
                dir.abs("scratch/tmp.rs"),
            ],
            overflow: false,
            fired: false,
        };

        let thread = thread::spawn(move || event_loop(w, feed, (), tx, stop_thread));

        // Seed check inside event_loop, then excluded burst, then debounce.
        thread::sleep(Duration::from_millis(200));
        match rx.recv_timeout(Duration::from_millis(50)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("excluded events leaked a changeset: {other:?}"),
        }

        stop.store(true, Ordering::SeqCst);
        let (w, _) = thread.join().expect("join event_loop");
        let w = w.expect("watcher returned on clean stop");

        // event_loop always does one full check at start; excluded events
        // must not add another full check or a directed check.
        assert_eq!(
            w.check_calls.load(Ordering::Relaxed),
            before_full + 1,
            "excluded events must not trigger another full check"
        );
        assert_eq!(
            w.check_paths_calls.load(Ordering::Relaxed),
            before_paths,
            "excluded events must not trigger check_paths"
        );
        assert_eq!(
            w.snapshot
                .get(&RelPath::new("seed.rs"))
                .expect("seed stamp")
                .mtime,
            seed_mtime
        );
    }

    #[test]
    fn check_paths_detects_added() {
        let dir = TestDir::new();
        dir.write("a.rs", "a");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        dir.write("nested/b.rs", "b");
        let cs = w.check_paths(&[dir.abs("nested/b.rs")]);
        assert_eq!(cs.added, vec![RelPath::new("nested/b.rs")]);
        assert!(cs.modified.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn check_paths_detects_modified() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        rewrite_and_bump(&dir, "a.rs", "fn a() { 1 }");
        let cs = w.check_paths(&[dir.abs("a.rs")]);
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn check_paths_detects_removed() {
        let dir = TestDir::new();
        dir.write("a.rs", "a");
        dir.write("b.rs", "b");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        fs::remove_file(dir.abs("a.rs")).expect("remove");
        let cs = w.check_paths(&[dir.abs("a.rs")]);
        assert_eq!(cs.removed, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.modified.is_empty());
    }

    #[test]
    fn check_paths_expands_directory_event() {
        let dir = TestDir::new();
        dir.write("sub/a.rs", "a");
        dir.write("sub/b.rs", "b");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        rewrite_and_bump(&dir, "sub/a.rs", "a2");
        fs::remove_file(dir.abs("sub/b.rs")).expect("remove");
        dir.write("sub/deep/c.rs", "c");

        let cs = w.check_paths(&[dir.abs("sub")]);
        assert_eq!(cs.added, vec![RelPath::new("sub/deep/c.rs")], "{cs:?}");
        assert_eq!(cs.modified, vec![RelPath::new("sub/a.rs")], "{cs:?}");
        assert_eq!(cs.removed, vec![RelPath::new("sub/b.rs")], "{cs:?}");
    }

    #[test]
    fn check_paths_storm_degrades_to_full_check() {
        let dir = TestDir::new();
        dir.write("keep.rs", "k");
        dir.write("seen.rs", "s");
        let mut w = watcher(&dir).storm_paths(3);
        assert!(w.check().is_empty());
        rewrite_and_bump(&dir, "keep.rs", "k2");
        dir.write("outside.rs", "o");

        let dummies: Vec<PathBuf> = (0..4).map(|i| dir.abs(&format!("nope{i}.rs"))).collect();
        let before_full = w.check_calls.load(Ordering::Relaxed);
        let cs = w.check_paths(&dummies);
        assert!(
            w.check_calls.load(Ordering::Relaxed) > before_full,
            "storm must run a full check"
        );
        assert!(
            cs.modified.contains(&RelPath::new("keep.rs")),
            "full check must see keep.rs, not in the dummy set: {cs:?}"
        );
        assert!(
            cs.added.contains(&RelPath::new("outside.rs")),
            "full check must see outside.rs: {cs:?}"
        );
    }

    #[test]
    fn overflow_distrust_fast_safety_then_clears() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_millis(80))
            .debounce(Duration::from_secs(60));
        let _ = w.check();
        let log = Arc::clone(&w.freshness);
        assert!(!log.distrusted());

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = BurstFeed {
            paths: Vec::new(),
            overflow: true,
            fired: false,
        };

        let thread = thread::spawn(move || {
            let _ = event_loop(w, feed, (), tx, stop_thread);
        });

        let start = Instant::now();
        while !log.distrusted() {
            if start.elapsed() > Duration::from_secs(2) {
                panic!("overflow did not set distrust");
            }
            thread::sleep(Duration::from_millis(5));
        }

        dir.write("late.rs", "late");
        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("fast safety after distrust should pick up a silent write");
        assert_eq!(cs.added, vec![RelPath::new("late.rs")], "{cs:?}");
        assert!(!log.distrusted(), "clean full check must clear distrust");

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");
    }

    #[test]
    fn freshness_log_watermark_and_suspect_paths() {
        let log = FreshnessLog::new();
        let t0 = log.watermark();
        assert!(!log.is_seeded());
        assert!(!log.distrusted());
        assert!(log.suspect_paths(t0).is_empty());

        thread::sleep(Duration::from_millis(2));
        log.note_paths(&[RelPath::new("b.rs"), RelPath::new("a.rs")]);
        assert_eq!(
            log.suspect_paths(t0),
            vec![RelPath::new("a.rs"), RelPath::new("b.rs")]
        );
        thread::sleep(Duration::from_millis(2));
        assert!(log.suspect_paths(Instant::now()).is_empty());

        log.note_distrust();
        assert!(log.distrusted());
        assert!(
            log.suspect_paths(t0).is_empty(),
            "distrusted log hides suspects"
        );

        thread::sleep(Duration::from_millis(2));
        log.note_verified(&ChangeSet::default(), Instant::now());
        assert!(log.is_seeded());
        assert!(!log.distrusted());
        assert!(log.watermark() > t0);
        assert!(log.suspect_paths(t0).is_empty());
    }

    #[test]
    fn spawn_with_log_shares_freshness() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle, log) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20))
            .spawn_with_log();
        assert!(!log.distrusted());
        // Drop any baseline/empty noise, then stop.
        let _ = rx.recv_timeout(Duration::from_millis(50));
        handle.stop();
    }

    #[test]
    fn request_verification_advances_watermark_poll_loop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle, log) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_secs(30))
            .debounce(Duration::ZERO)
            .safety_interval(Duration::ZERO)
            .safety_jitter(false)
            .spawn_with_log();

        // First check seeds baseline immediately; give it a moment.
        thread::sleep(Duration::from_millis(40));
        let wm = log.watermark();

        dir.write("late.rs", "fn late() {}");
        bump_mtime(&dir.abs("late.rs"));
        log.request_verification();

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("on-demand check should emit late.rs without waiting poll_interval");
        assert!(
            cs.added.iter().any(|p| p.as_str() == "late.rs"),
            "expected late.rs added, got {cs:?}"
        );
        assert!(
            log.watermark() > wm,
            "request_verification must push the watermark"
        );
        handle.stop();
    }

    struct HangFeed;

    impl EventFeed for HangFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            thread::sleep(timeout);
            Err(RecvTimeoutError::Timeout)
        }
    }

    #[test]
    fn request_verification_event_loop_runs_check() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_secs(3600))
            .debounce(Duration::from_secs(60))
            .safety_jitter(false);
        let _ = w.check();
        let log = Arc::clone(&w.freshness);
        let wm = log.watermark();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let _ = event_loop(w, HangFeed, (), tx, stop_thread);
        });

        thread::sleep(Duration::from_millis(20));
        dir.write("late.rs", "late");
        bump_mtime(&dir.abs("late.rs"));
        log.request_verification();
        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("event_loop should honor request_verification within STOP_SLICE");
        assert!(
            cs.added.iter().any(|p| p.as_str() == "late.rs"),
            "expected late.rs added, got {cs:?}"
        );
        assert!(log.watermark() > wm);

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");
    }

    #[test]
    #[ignore]
    fn bench_check_on_serena() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../serena");
        assert!(
            root.is_dir(),
            "missing corpus {}: {}",
            root.display(),
            "expected ../serena next to the Astrolabe workspace"
        );

        let mut w = Watcher::new(&root);
        let t0 = Instant::now();
        let baseline = w.check();
        let baseline_dt = t0.elapsed();
        assert!(baseline.is_empty());
        let files = w.tracked_files();

        let mut times = Vec::with_capacity(8);
        for _ in 0..8 {
            let t = Instant::now();
            let cs = w.check();
            times.push(t.elapsed());
            assert!(cs.is_empty(), "serena mutated during the bench: {cs:?}");
        }
        times.sort();
        println!(
            "serena files={files} baseline={baseline_dt:?} \
             check min/median/max {:?}/{:?}/{:?}",
            times[0],
            times[times.len() / 2],
            times[times.len() - 1],
        );
    }

    /// Feed backed by a channel so a test injects events at chosen moments.
    struct ChanFeed(mpsc::Receiver<Vec<PathBuf>>);

    impl EventFeed for ChanFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            self.0.recv_timeout(timeout).map(|paths| FeedWake::Events {
                paths,
                overflow: false,
            })
        }
    }

    #[test]
    fn note_verified_keeps_suspects_noted_during_the_scan() {
        let log = FreshnessLog::new();
        log.note_paths(&[RelPath::new("before.rs")]);
        thread::sleep(Duration::from_millis(2));
        let started = Instant::now();
        thread::sleep(Duration::from_millis(2));
        // Arrived while the (off-thread) scan was running.
        log.note_paths(&[RelPath::new("during.rs")]);
        log.note_verified(&ChangeSet::default(), started);
        assert_eq!(
            log.suspect_paths(log.watermark()),
            vec![RelPath::new("during.rs")],
            "a suspect noted mid-scan must stay visible after the new watermark; \
             one noted before the scan began is covered by it"
        );
    }

    #[test]
    fn full_scan_runs_off_thread_and_events_keep_flowing() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_secs(3600))
            .safety_jitter(false)
            .debounce(Duration::from_millis(100));
        let _ = w.check();
        w.scan_delay = Duration::from_millis(1200);
        let log = Arc::clone(&w.freshness);
        let wm0 = log.watermark();
        let (ev_tx, ev_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let started = Instant::now();
        // The loop opens with a full catch-up check, stretched to 1.2 s.
        let thread = thread::spawn(move || event_loop(w, ChanFeed(ev_rx), (), tx, stop_thread));

        thread::sleep(Duration::from_millis(200));
        rewrite_and_bump(&dir, "a.rs", "fn a() { 2 }");
        ev_tx.send(vec![dir.abs("a.rs")]).unwrap();
        // Before L2 the event sat in the feed until the scan returned.
        let deadline = Instant::now() + Duration::from_millis(600);
        while !log.suspect_paths(wm0).contains(&RelPath::new("a.rs")) {
            assert!(
                Instant::now() < deadline,
                "event not absorbed during the scan"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            started.elapsed() < Duration::from_millis(1100),
            "suspect must be noted while the full scan is still running"
        );
        assert_eq!(log.watermark(), wm0, "scan still in flight");

        // The scan observed the tree before the edit, so the change arrives
        // through the directed check that follows the debounce.
        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("edit made during the full scan is delivered");
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert!(log.watermark() > wm0);

        stop.store(true, Ordering::SeqCst);
        let (w, ok) = thread.join().expect("join event_loop");
        assert!(ok && w.is_some());
        drop(ev_tx);
    }

    #[test]
    fn stop_is_honoured_during_a_full_scan() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir).safety_jitter(false);
        let _ = w.check();
        w.scan_delay = Duration::from_secs(5);
        let (tx, _rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || event_loop(w, HangFeed, (), tx, stop_thread));
        thread::sleep(Duration::from_millis(100));
        let asked = Instant::now();
        stop.store(true, Ordering::SeqCst);
        let (w, ok) = thread.join().expect("join event_loop");
        let took = asked.elapsed();
        assert!(ok && w.is_none(), "stop mid-scan abandons the scan");
        assert!(
            took < Duration::from_secs(2),
            "stop took {took:?}; it must not wait out the 5 s scan"
        );
    }

    #[test]
    fn poll_cycle_is_published_only_while_polling() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let (_rx, handle, log) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(300))
            .spawn_with_log();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !log.is_seeded() {
            assert!(Instant::now() < deadline, "never seeded");
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(50));
        let cycle = log.poll_cycle().expect("poll mode publishes its cycle");
        assert!(cycle >= Duration::from_millis(300), "{cycle:?}");
        handle.stop();

        let mut w = watcher(&dir).safety_jitter(false);
        let _ = w.check();
        let log = Arc::clone(&w.freshness);
        log.set_poll_cycle(Some(Duration::from_secs(10)));
        let (tx, _rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || event_loop(w, HangFeed, (), tx, stop_thread));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(log.poll_cycle(), None, "event mode clears the poll cycle");
        stop.store(true, Ordering::SeqCst);
        let _ = thread.join();
    }
}
