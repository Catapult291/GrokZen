//! Per-root runtime: index, watcher, lock, LSP, and the query-time barrier.
//!
//! The MCP server holds either one [`RootSession`] (Repo / Single) or a
//! [`DispatchState`] of them (MultiProject). Watcher spawn happens *before* the
//! cold build so edits during `IncrementalIndex::build` arrive as channel
//! events instead of being folded into a post-build baseline.

use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use astrolabe_core::cache::{CacheConfig, FileCache};
use astrolabe_core::freshness::{barrier_decision, staleness_note, BarrierDecision, FRESH_WINDOW};
use astrolabe_core::watch::{ChangeSet, FreshnessLog, WatchHandle, Watcher};
use astrolabe_core::RelPath;
use rmcp::model::{CallToolResult, ContentBlock};

use crate::index::{self, RepoIndex};
use crate::knobs::{McpKnobs, WatchMode};
use crate::live_backend::LiveBackend;
use crate::precise_tools::PreciseCapability;
use crate::roots::{DispatcherConfig, RootDispatcher};

/// Lock a mutex, recovering the data if a panicking thread poisoned it.
///
/// Every value guarded in a session (pending changesets, the watch handle,
/// the freshness log slot, the dispatcher maps) stays structurally valid
/// across a panic: the worst case is a partially merged changeset, which the
/// next apply or rebuild converges. Crashing every later tool call with
/// "poisoned" is strictly worse than serving from that state.
pub(crate) trait LockOrRecover<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockOrRecover<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("recovering a poisoned session lock");
            self.clear_poison();
            poisoned.into_inner()
        })
    }
}

/// Changes the index failed to absorb, kept so they are retried instead of
/// silently dropped (F4). Lock order: `pending` before this.
#[derive(Debug, Default)]
pub(crate) struct ApplyHealth {
    /// Paths whose incremental update failed and is not in the index yet.
    pub unapplied: ChangeSet,
    /// Earliest next query-time retry of `unapplied` (one failing file must
    /// not cost a full apply on every call).
    pub next_inline_retry: Option<Instant>,
    /// A full background rebuild failed; do not start another before this.
    pub rebuild_backoff_until: Option<Instant>,
    /// Failed changes handed to the running background rebuild, which reads
    /// the whole tree after taking them. Cleared when it publishes, put back
    /// into `unapplied` when it fails.
    pub in_rebuild: ChangeSet,
}

/// Gap between query-time retries of a failed incremental update.
const INLINE_RETRY_GAP: Duration = Duration::from_secs(2);
/// Gap before another background rebuild after one failed.
const REBUILD_BACKOFF: Duration = Duration::from_secs(60);

/// Serving snapshot for one index root.
#[derive(Debug)]
pub(crate) enum IndexState {
    Building,
    Ready(Arc<RepoIndex>),
    Failed(String),
}

/// `FileCache` is not `Debug`; wrap it so session structs can keep a derive.
#[derive(Clone)]
pub(crate) struct SharedFileCache(pub Arc<FileCache>);

impl Deref for SharedFileCache {
    type Target = FileCache;

    fn deref(&self) -> &FileCache {
        &self.0
    }
}

impl fmt::Debug for SharedFileCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileCache")
            .field("weighted_size", &self.weighted_size())
            .finish()
    }
}

/// Everything that must be created, torn down, and isolated per index root.
pub(crate) struct RootSession {
    pub root: PathBuf,
    pub state: Arc<RwLock<IndexState>>,
    pub watch: Arc<Mutex<Option<WatchHandle>>>,
    pub freshness: Arc<Mutex<Option<Arc<FreshnessLog>>>>,
    pub indexer_lock: Arc<Mutex<Option<astrolabe_core::elect::IndexerLock>>>,
    pub precise: Arc<dyn PreciseCapability>,
    pub pending: Arc<Mutex<ChangeSet>>,
    pub file_cache: SharedFileCache,
    pub cache_hits: Arc<AtomicU64>,
    pub cache_misses: Arc<AtomicU64>,
    pub cache_budget_bytes: u64,
    pub rebuild_inflight: Arc<AtomicBool>,
    /// Changesets applied to the live index while a background rebuild runs;
    /// replayed onto the rebuilt index so the rebuild cannot erase them.
    pub rebuild_journal: Arc<Mutex<ChangeSet>>,
    /// Set by [`RootSession::teardown`]; a bootstrap that races with an
    /// eviction checks it so it never leaves a watcher running. It is also
    /// the cancel token of this session's cold build and background
    /// rebuilds: an evicted root or a shutting-down server stops parsing
    /// instead of finishing a build nobody will serve.
    pub closed: Arc<AtomicBool>,
    /// Full builds (cold or background rebuild) currently running for this
    /// session; [`RootSession::wait_builds_idle`] waits for it to reach 0.
    pub builds_active: Arc<std::sync::atomic::AtomicUsize>,
    /// Last coverage note ("N 个文件有语法错误…") shown in full in this
    /// session. The note is repeated only when it changes, a result is
    /// empty, or a hit lands in an error file.
    pub coverage_note_shown: Arc<Mutex<Option<String>>>,
    /// Failed incremental updates awaiting retry / rebuild.
    pub apply_health: Arc<Mutex<ApplyHealth>>,
    pub knobs: McpKnobs,
    /// Test hook: the next N incremental applies fail as if the engine
    /// panicked.
    #[cfg(test)]
    pub fail_applies: Arc<std::sync::atomic::AtomicUsize>,
}

impl fmt::Debug for RootSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootSession")
            .field("root", &self.root)
            .field("cache_budget_bytes", &self.cache_budget_bytes)
            .finish_non_exhaustive()
    }
}

impl RootSession {
    /// `true` when this process may take the indexer lock and write the
    /// store. Called at every cold build and apply: a host that pinned
    /// `persist_enabled = false` never creates `<root>/.astrolabe/`.
    pub(crate) fn persist_enabled(&self) -> bool {
        self.knobs.persist_enabled && crate::reindex::refresh_leader(&self.indexer_lock, &self.root)
    }

    pub(crate) fn new(root: PathBuf, knobs: McpKnobs, cache_budget_bytes: u64) -> Arc<Self> {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let state = Arc::new(RwLock::new(IndexState::Building));
        let syntax_state = Arc::clone(&state);
        let file_cache = Arc::new(FileCache::new(CacheConfig {
            budget_bytes: cache_budget_bytes,
            ..CacheConfig::default()
        }));
        let precise = LiveBackend::new(root.clone())
            .with_syntax_index(Arc::new(move || match &*syntax_state.read().ok()? {
                IndexState::Ready(index) => Some(Arc::clone(index)),
                _ => None,
            }))
            .with_file_cache(Arc::clone(&file_cache))
            .with_lsp_enabled(knobs.lsp_enabled);
        Arc::new(Self {
            precise: Arc::new(precise),
            root,
            state,
            watch: Arc::new(Mutex::new(None)),
            freshness: Arc::new(Mutex::new(None)),
            indexer_lock: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(ChangeSet::default())),
            file_cache: SharedFileCache(file_cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
            cache_misses: Arc::new(AtomicU64::new(0)),
            cache_budget_bytes,
            rebuild_inflight: Arc::new(AtomicBool::new(false)),
            rebuild_journal: Arc::new(Mutex::new(ChangeSet::default())),
            closed: Arc::new(AtomicBool::new(false)),
            builds_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            coverage_note_shown: Arc::new(Mutex::new(None)),
            apply_health: Arc::new(Mutex::new(ApplyHealth::default())),
            knobs,
            #[cfg(test)]
            fail_applies: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Wait (bounded) until no full build of this session is running.
    /// Returns `true` when idle. Used after [`Self::teardown`] at shutdown so
    /// a cancelled build reaches its safe point before the process exits.
    pub(crate) fn wait_builds_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.builds_active.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stop the watcher and drop the indexer lock. Store files stay on disk.
    pub(crate) fn teardown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.watch.lock_or_recover().take() {
            handle.stop();
        }
        *self.indexer_lock.lock_or_recover() = None;
        *self.freshness.lock_or_recover() = None;
    }
}

/// Multi-project dispatcher plus the live per-child sessions.
#[derive(Debug)]
pub(crate) struct DispatchState {
    pub dispatcher: Mutex<RootDispatcher>,
    pub sessions: Mutex<HashMap<PathBuf, Arc<RootSession>>>,
    pub children: Vec<PathBuf>,
    pub knobs: McpKnobs,
    pub cache_budget_bytes: u64,
}

impl DispatchState {
    pub(crate) fn new(
        children: Vec<PathBuf>,
        knobs: McpKnobs,
        cache_budget_bytes: u64,
    ) -> Arc<Self> {
        let cfg = DispatcherConfig {
            idle: knobs.idle,
            resident_roots: knobs.resident_roots,
        };
        Arc::new(Self {
            dispatcher: Mutex::new(RootDispatcher::new(children.clone(), cfg)),
            sessions: Mutex::new(HashMap::new()),
            children,
            knobs,
            cache_budget_bytes,
        })
    }
}

/// Counts one running full build in [`RootSession::builds_active`].
struct BuildActive(Arc<std::sync::atomic::AtomicUsize>);

impl BuildActive {
    fn enter(session: &RootSession) -> Self {
        session.builds_active.fetch_add(1, Ordering::SeqCst);
        BuildActive(Arc::clone(&session.builds_active))
    }
}

impl Drop for BuildActive {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Full build for `session` that stops early once the session is closed
/// (teardown on eviction / shutdown). `None` = cancelled; nothing to publish.
pub(crate) fn build_session_index(session: &RootSession, persist: bool) -> Option<IndexState> {
    let _active = BuildActive::enter(session);
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        index::build_cancellable(
            &session.root,
            persist,
            session.knobs.parse_cache_bytes,
            session.knobs.respect_gitignore,
            &session.knobs.extra_excludes,
            &session.closed,
        )
    }));
    let state = match outcome {
        Err(_) => IndexState::Failed("底层索引能力尚未实现或发生 panic".to_string()),
        Ok(None) => {
            tracing::info!(
                root = %session.root.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "index build cancelled (session closed)"
            );
            return None;
        }
        Ok(Some(index)) => IndexState::Ready(Arc::new(index)),
    };
    Some(state)
}

/// Index once, converting a panic in the engine into a reportable failure.
/// Production paths use the cancellable [`build_session_index`].
#[cfg(test)]
pub(crate) fn build_index(root: &Path, persist: bool, parse_cache_bytes: u64) -> IndexState {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        index::build_with_options(root, persist, parse_cache_bytes)
    }))
    .map_err(|_| "底层索引能力尚未实现或发生 panic".to_string())
    .and_then(|result| result.map_err(|error| error.to_string()))
    .map(Arc::new)
    .map(IndexState::Ready)
    .unwrap_or_else(IndexState::Failed)
}

/// Spawn the cold-build thread. Watcher is started *inside* that thread
/// before `IncrementalIndex::build`.
pub(crate) fn start_root_indexing(session: Arc<RootSession>) {
    std::thread::Builder::new()
        .name("astrolabe-index".into())
        .spawn(move || bootstrap_root(session))
        .expect("failed to spawn index thread");
}

fn bootstrap_root(session: Arc<RootSession>) {
    if session.closed.load(Ordering::SeqCst) {
        return;
    }
    let persist = session.persist_enabled();

    // Spawn the watcher (and take its baseline) BEFORE the cold build.
    // Edits during IncrementalIndex::build must arrive as channel events, not
    // be absorbed into a post-build baseline; the reindex thread buffers them
    // until Ready and applies once.
    //
    // `start_watching` publishes the handle before the watch thread finishes
    // its first `check`. Waiting here closes that gap: a file created after
    // the handle exists but before the scan returns would otherwise be sealed
    // into the baseline (no event) while `build_index` walks past it.
    if let Some(mode) = session.knobs.watch_mode {
        start_watching(&session, mode);
        wait_for_watcher_baseline(&session);
    }

    if session.closed.load(Ordering::SeqCst) {
        // Evicted before the cold build started: skip the whole build.
        session.teardown();
        return;
    }
    // Cancelled when the session closes mid-build (eviction / shutdown).
    let Some(outcome) = build_session_index(&session, persist) else {
        session.teardown();
        return;
    };
    publish_ready(&session, outcome);
}

/// Block the cold build until the watcher thread has sealed its baseline.
///
/// No-op when watching is disabled. Gives up after 60s so a stuck watch
/// thread cannot pin the server in `Building` forever.
fn wait_for_watcher_baseline(session: &RootSession) {
    let log = {
        let guard = session.freshness.lock_or_recover();
        guard.as_ref().map(Arc::clone)
    };
    let Some(log) = log else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !log.is_seeded() {
        if Instant::now() >= deadline {
            tracing::warn!(
                root = %session.root.display(),
                "watcher baseline was not ready before the cold build; edits during startup may be missed"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Watch the repository and incrementally rebuild the graph when files change.
///
/// The previous index keeps serving for the whole rebuild: dropping back to
/// `Building` would make every tool call fail for a second because someone
/// saved a file. A stale answer beats no answer here, and the window is short.
/// A failed rebuild is also discarded rather than published, so one bad edit
/// cannot replace a working index with an error.
///
/// Reindex loop:
///   recv → coalesce (throttle window, merge every queued ChangeSet) →
///   language-filter (non-source added/modified dropped; removed kept) →
///   skip if empty → if not Ready, merge into pending; if Ready, apply.
fn start_watching(session: &Arc<RootSession>, mode: WatchMode) {
    let watcher = Watcher::new(session.root.clone())
        .safety_interval(session.knobs.safety_interval)
        .storm_paths(session.knobs.storm_paths)
        .scan_options(scan_options(&session.knobs));
    let (rx, handle, log) = match mode {
        WatchMode::Events => {
            tracing::info!(root = %session.root.display(), "watching for changes (native OS events)");
            watcher.spawn_with_log()
        }
        WatchMode::Poll(interval) => {
            tracing::info!(
                root = %session.root.display(),
                interval_secs = interval.as_secs(),
                "watching for changes (forced polling)"
            );
            watcher
                .poll_interval(interval)
                .events(false)
                .spawn_with_log()
        }
    };
    *session.watch.lock_or_recover() = Some(handle);
    *session.freshness.lock_or_recover() = Some(log);
    if session.closed.load(Ordering::SeqCst) {
        // Evicted while the watcher was starting: teardown already ran and
        // missed the handle stored above, so stop it here instead of leaking.
        session.teardown();
        return;
    }

    let session = Arc::clone(session);
    let throttle = session.knobs.reindex_throttle;
    std::thread::Builder::new()
        .name("astrolabe-reindex".into())
        .spawn(move || {
            crate::reindex::for_each_reindex_batch(&session.root, rx, throttle, |changes| {
                tracing::info!(
                    added = changes.added.len(),
                    modified = changes.modified.len(),
                    removed = changes.removed.len(),
                    "reindexing after file changes"
                );
                // Before reindexing: the LSP sync is cheap and a query racing
                // the rebuild should already see the new text.
                session.precise.files_changed(changes);
                apply_or_buffer(&session, changes);
            });
        })
        .expect("failed to spawn reindex thread");
}

/// Lock order: pending, then state. Matches [`publish_ready`] and [`inline_repair`].
fn apply_or_buffer(session: &RootSession, changes: &ChangeSet) {
    let mut pending = session.pending.lock_or_recover();
    pending.merge(changes.clone());
    apply_pending(session, &mut pending);
}

/// Native facade: apply a host-made edit now instead of waiting for the
/// watcher. Buffers into `pending` and applies when the index is Ready, so the
/// call is safe while a cold build runs (the edit lands when it publishes).
pub(crate) fn note_edited(session: &RootSession, changes: &ChangeSet) -> bool {
    let mut pending = session.pending.lock_or_recover();
    pending.merge(changes.clone());
    apply_pending(session, &mut pending)
}

fn publish_ready(session: &RootSession, mut outcome: IndexState) {
    let persist = session.persist_enabled();
    let opts = crate::reindex::mcp_index_options(
        persist,
        session.knobs.parse_cache_bytes,
        session.knobs.respect_gitignore,
        &session.knobs.extra_excludes,
    );

    let mut pending = session.pending.lock_or_recover();

    if let IndexState::Ready(index) = &outcome {
        let drained = std::mem::take(&mut *pending);
        let older = std::mem::take(&mut session.apply_health.lock_or_recover().unapplied);
        let (inc, left) = apply_with_fallback(session, index.incremental(), older, drained, &opts);
        if let Some(inc) = inc {
            outcome = IndexState::Ready(Arc::new(index::RepoIndex::from_incremental(
                session.root.clone(),
                inc,
            )));
        }
        note_unapplied(session, left);
    }
    if session.closed.load(Ordering::SeqCst) {
        // Evicted during the cold build: do not publish into a dead session.
        session.teardown();
        return;
    }
    if let IndexState::Ready(index) = &outcome {
        index.prewarm(true);
    }
    *session.state.write().unwrap_or_else(|e| e.into_inner()) = outcome;
}

/// Apply buffered changes when the index is Ready. The caller holds the
/// `pending` lock, which serialises every writer of `state` (apply, publish),
/// so the heavy `apply_reindex` runs on a snapshot *outside* the state lock and
/// the write lock is held only for the `Arc` swap. Queries keep reading the old
/// index meanwhile instead of queueing behind the rebuild.
///
/// Earlier failed changes ([`ApplyHealth::unapplied`]) are retried together
/// with `pending`. Whatever still fails is put back into `unapplied` instead
/// of being dropped, so the barrier can retry, rebuild, and say the answer
/// may be stale. Returns `true` when nothing is left unapplied.
fn apply_pending(session: &RootSession, pending: &mut ChangeSet) -> bool {
    let previous = match &*session.state.read().unwrap_or_else(|e| e.into_inner()) {
        IndexState::Ready(index) => Arc::clone(index),
        _ => return true,
    };
    let changes = std::mem::take(pending);
    let older = std::mem::take(&mut session.apply_health.lock_or_recover().unapplied);
    if changes.is_empty() && older.is_empty() {
        return true;
    }
    let persist = session.persist_enabled();
    let opts = crate::reindex::mcp_index_options(
        persist,
        session.knobs.parse_cache_bytes,
        session.knobs.respect_gitignore,
        &session.knobs.extra_excludes,
    );
    let mut applied = older.clone();
    applied.merge(changes.clone());
    let (inc, left) = apply_with_fallback(session, previous.incremental(), older, changes, &opts);
    if let Some(inc) = inc {
        let next = Arc::new(index::RepoIndex::from_incremental(
            session.root.clone(),
            inc,
        ));
        next.prewarm(false);
        *session.state.write().unwrap_or_else(|e| e.into_inner()) = IndexState::Ready(next);
        // Free the superseded graph (unless a reader still holds it) off this
        // thread: this may be a query waiting on its inline repair, and
        // tearing down a large repository's graph and parse products cost it
        // tens of ms. If the spawn fails the drop runs here.
        let _ = std::thread::Builder::new()
            .name("astrolabe-drop".into())
            .spawn(move || drop(previous));
        // Return the freed pages to the OS once edits go quiet (glibc only;
        // off-thread, long after the drop above has finished).
        crate::alloc::release_free_memory_when_quiet();
        if session.rebuild_inflight.load(Ordering::SeqCst) {
            session.rebuild_journal.lock_or_recover().merge(applied);
        }
    }
    let clean = left.is_empty();
    note_unapplied(session, left);
    clean
}

/// Apply `older` (earlier failures) plus `newer` onto `base`. If the
/// combined set fails and both parts are non-empty, apply `newer` alone so
/// one persistently failing file cannot block every later edit. Returns the
/// new incremental index (if any apply succeeded) and the changes that are
/// still not in it.
fn apply_with_fallback(
    session: &RootSession,
    base: Option<&astrolabe_core::index::IncrementalIndex>,
    older: ChangeSet,
    newer: ChangeSet,
    opts: &astrolabe_core::index::IndexOptions,
) -> (Option<astrolabe_core::index::IncrementalIndex>, ChangeSet) {
    let mut all = older.clone();
    all.merge(newer.clone());
    if all.is_empty() {
        return (None, all);
    }
    match try_apply(session, base, &all, opts) {
        Ok(inc) => return (Some(inc), ChangeSet::default()),
        Err(error) => {
            tracing::warn!(%error, files = all.len(), "incremental reindex failed; will retry");
        }
    }
    if !older.is_empty() && !newer.is_empty() {
        if let Ok(inc) = try_apply(session, base, &newer, opts) {
            return (Some(inc), older);
        }
    }
    (None, all)
}

fn try_apply(
    session: &RootSession,
    base: Option<&astrolabe_core::index::IncrementalIndex>,
    changes: &ChangeSet,
    opts: &astrolabe_core::index::IndexOptions,
) -> Result<astrolabe_core::index::IncrementalIndex, String> {
    #[cfg(test)]
    if session
        .fail_applies
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
    {
        return Err("injected apply failure".to_string());
    }
    crate::reindex::apply_reindex(&session.root, base, changes, opts)
}

/// Remember changes that are not in the index yet (merged with any already
/// recorded). An empty set clears the retry timer.
fn note_unapplied(session: &RootSession, left: ChangeSet) {
    let mut health = session.apply_health.lock_or_recover();
    if left.is_empty() && health.unapplied.is_empty() {
        health.next_inline_retry = None;
        return;
    }
    health.unapplied.merge(left);
    health.next_inline_retry = Some(Instant::now() + INLINE_RETRY_GAP);
}

/// Query-time handling of failed incremental updates: retry them inline
/// (rate-limited), then fall back to a background rebuild, and return a
/// short note while the index is still missing them.
fn settle_unapplied(session: &Arc<RootSession>) -> Option<String> {
    let retry_due = {
        let health = session.apply_health.lock_or_recover();
        if health.unapplied.is_empty() && health.in_rebuild.is_empty() {
            return None;
        }
        !health.unapplied.is_empty()
            && health
                .next_inline_retry
                .is_none_or(|at| Instant::now() >= at)
    };
    if retry_due {
        let mut pending = session.pending.lock_or_recover();
        apply_pending(session, &mut pending);
    }
    let (count, sample, needs_rebuild) = {
        let health = session.apply_health.lock_or_recover();
        let mut missing = health.in_rebuild.clone();
        missing.merge(health.unapplied.clone());
        if missing.is_empty() {
            return None;
        }
        let sample: Vec<String> = missing
            .iter()
            .take(3)
            .map(|(_, p)| p.as_str().to_owned())
            .collect();
        (
            missing.len(),
            sample,
            !health.unapplied.is_empty()
                && health
                    .rebuild_backoff_until
                    .is_none_or(|until| Instant::now() >= until),
        )
    };
    if needs_rebuild {
        trigger_background_rebuild_arc(session);
    }
    Some(incremental_failure_note(count, &sample))
}

pub(crate) fn incremental_failure_note(count: usize, sample: &[String]) -> String {
    let more = if count > sample.len() { " 等" } else { "" };
    format!(
        "索引可能过期：{count} 个文件的增量更新失败（{}{more}），已重试并触发后台重建；涉及这些文件的结果可能是旧的。confidence: unknown",
        sample.join("、")
    )
}

pub(crate) enum BarrierEffect {
    None,
    StaleNote(String),
}

/// Query-time freshness barrier. No-ops when watching is disabled (no log).
pub(crate) fn run_barrier(session: &Arc<RootSession>) -> BarrierEffect {
    let log = {
        let g = session.freshness.lock_or_recover();
        match g.as_ref() {
            Some(log) => Arc::clone(log),
            None => return BarrierEffect::None,
        }
    };

    let started = Instant::now();
    // Suspects noted up to here are covered by whatever repair this call does.
    let observed_at = started;
    let watermark = log.watermark();
    let watermark_age = watermark.elapsed();
    let suspects = log.suspect_paths(watermark);
    let distrusted = log.distrusted();
    let max_inline = session.knobs.barrier_max;
    let decision = barrier_decision(
        watermark_age,
        fresh_window_for(&session.knobs, log.poll_cycle()),
        distrusted,
        &suspects,
        max_inline,
    );
    tracing::debug!(
        ?decision,
        elapsed_ms = started.elapsed().as_millis() as u64,
        suspects = suspects.len(),
        "freshness barrier"
    );

    let effect = match decision {
        BarrierDecision::Fresh => BarrierEffect::None,
        BarrierDecision::Repair(paths) if paths.is_empty() => {
            let before = log.watermark();
            log.request_verification();
            if wait_watermark_advance(&log, before, BARRIER_VERIFY_WAIT) {
                // The verification may have found changes that are not in
                // the index yet; they are suspects after the new watermark.
                let observed = Instant::now();
                let found = log.suspect_paths(log.watermark());
                if log.distrusted() || found.len() > max_inline {
                    trigger_background_rebuild_arc(session);
                    BarrierEffect::StaleNote(staleness_note(found.len()))
                } else {
                    if !found.is_empty() {
                        inline_repair(session, found);
                        log.clear_suspects_through(observed);
                    }
                    BarrierEffect::None
                }
            } else {
                // The verification scan did not finish in time: we answer
                // from an unverified index, so say so instead of staying silent.
                BarrierEffect::StaleNote(verification_timeout_note())
            }
        }
        BarrierDecision::Repair(paths) => {
            inline_repair(session, paths);
            log.clear_suspects_through(observed_at);
            BarrierEffect::None
        }
        BarrierDecision::Degrade { suspects } => {
            trigger_background_rebuild_arc(session);
            BarrierEffect::StaleNote(staleness_note(suspects))
        }
    };
    // A repair (or an earlier watcher batch) whose apply failed is tracked
    // in `apply_health`, not lost: retry it, and say so while it is missing.
    match (effect, settle_unapplied(session)) {
        (effect, None) => effect,
        (BarrierEffect::None, Some(note)) => BarrierEffect::StaleNote(note),
        (BarrierEffect::StaleNote(first), Some(note)) => {
            BarrierEffect::StaleNote(format!("{first}\n{note}"))
        }
    }
}

/// Freshness window for the barrier. With native events and a safety scan,
/// the watermark only advances on that scan while idle, but every change
/// arrives as a suspect, so an idle watermark older than `FRESH_WINDOW` does
/// not need a full-tree verification per query (M1). In that mode the window
/// is the safety interval plus slack; otherwise it is `FRESH_WINDOW`.
///
/// `poll_cycle` is the watcher's published polling cycle (forced polling, or
/// events that fell back to polling at runtime). A poller advances the
/// watermark once per cycle and records every change it finds as a suspect,
/// so the window is that cycle plus slack: a fixed 5 s window made every
/// query in the second half of a 10 s cycle force an extra full scan (and
/// a 2 s wait / stale note on big trees), while a 30 min events window after
/// a fallback would never notice a stalled poller. The barrier still forces a
/// verification once the poller is overdue.
pub(crate) fn fresh_window_for(
    knobs: &crate::knobs::McpKnobs,
    poll_cycle: Option<Duration>,
) -> Duration {
    if let Some(cycle) = poll_cycle {
        return (cycle + (cycle / 4).max(Duration::from_secs(1))).max(FRESH_WINDOW);
    }
    match knobs.watch_mode {
        Some(crate::knobs::WatchMode::Events) if !knobs.safety_interval.is_zero() => {
            (knobs.safety_interval + Duration::from_secs(60)).max(FRESH_WINDOW)
        }
        _ => FRESH_WINDOW,
    }
}

/// How long a `Repair([])` barrier waits for the requested full check.
const BARRIER_VERIFY_WAIT: Duration = Duration::from_secs(2);

pub(crate) fn verification_timeout_note() -> String {
    "索引可能过期：变更校验未在 2 秒内完成，本结果基于未确认的索引。confidence: unknown".to_string()
}

/// Returns `true` when the watermark advanced before `timeout`.
fn wait_watermark_advance(log: &FreshnessLog, before: Instant, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if log.watermark() != before {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Scan gates for this session: the same `.gitignore` policy must apply to
/// the cold build, inline repairs, and watcher admissions, or a file can be
/// indexed by one path and rejected by another.
pub(crate) fn scan_options(knobs: &McpKnobs) -> astrolabe_core::scan::ScanOptions {
    astrolabe_core::scan::ScanOptions {
        respect_gitignore: knobs.respect_gitignore,
        extra_excludes: knobs.extra_excludes.clone(),
        ..astrolabe_core::scan::ScanOptions::default()
    }
}

fn inline_repair(session: &RootSession, paths: Vec<RelPath>) {
    // Suspects are raw event paths: classify them with the scan gates so a
    // gitignored / deleted / oversized path is removed instead of being
    // blindly parsed into the index.
    let scan_opts = scan_options(&session.knobs);
    let mut repair = ChangeSet::default();
    for path in paths {
        match astrolabe_core::scan::path_admission(&session.root, &path, &scan_opts) {
            astrolabe_core::scan::PathAdmission::Indexed => repair.modified.push(path),
            _ => repair.removed.push(path),
        }
    }
    let repair = crate::reindex::filter_changeset(&session.root, repair);
    if repair.is_empty() {
        return;
    }
    // Tell the watcher before reading: the index is about to hold content
    // its snapshot may never see (edit, query, revert within the debounce).
    let log = session.freshness.lock_or_recover().clone();
    if let Some(log) = log {
        let touched: Vec<RelPath> = repair.iter().map(|(_, p)| p.clone()).collect();
        log.note_repaired(&touched);
    }
    let mut pending = session.pending.lock_or_recover();
    pending.merge(repair);
    apply_pending(session, &mut pending);
}

/// Kick a full `IncrementalIndex::build` without dropping to `Building`.
pub(crate) fn trigger_background_rebuild_arc(session: &Arc<RootSession>) {
    if session
        .rebuild_inflight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    session
        .rebuild_journal
        .lock_or_recover()
        .clone_from(&ChangeSet::default());
    {
        // The full build reads every file after this point, so it covers
        // the failed changes recorded so far.
        let mut health = session.apply_health.lock_or_recover();
        let taken = std::mem::take(&mut health.unapplied);
        health.in_rebuild.merge(taken);
    }
    let session = Arc::clone(session);
    std::thread::Builder::new()
        .name("astrolabe-rebuild".into())
        .spawn(move || {
            let started = Instant::now();
            let persist = session.persist_enabled();
            let outcome = build_session_index(&session, persist);
            let rebuilt = matches!(outcome, Some(IndexState::Ready(_)));
            {
                let mut health = session.apply_health.lock_or_recover();
                let covered = std::mem::take(&mut health.in_rebuild);
                if rebuilt {
                    health.rebuild_backoff_until = None;
                } else {
                    if let Some(IndexState::Failed(error)) = &outcome {
                        tracing::warn!(%error, "background rebuild failed; backing off");
                    }
                    health.unapplied.merge(covered);
                    health.rebuild_backoff_until = Some(Instant::now() + REBUILD_BACKOFF);
                }
            }
            if let Some(outcome @ IndexState::Ready(_)) = outcome {
                publish_rebuilt(&session, outcome);
                // The rebuilt index covers every suspect noted before it began.
                let log = session.freshness.lock_or_recover().clone();
                if let Some(log) = log {
                    log.clear_suspects_through(started);
                }
            }
            session.rebuild_inflight.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Publish a background rebuild without losing changes that were applied to
/// the old index (journal) or buffered (pending) while it was building.
fn publish_rebuilt(session: &RootSession, mut outcome: IndexState) {
    let persist = session.persist_enabled();
    let opts = crate::reindex::mcp_index_options(
        persist,
        session.knobs.parse_cache_bytes,
        session.knobs.respect_gitignore,
        &session.knobs.extra_excludes,
    );
    let mut pending = session.pending.lock_or_recover();
    let mut replay = std::mem::take(&mut *session.rebuild_journal.lock_or_recover());
    replay.merge(std::mem::take(&mut *pending));
    if let IndexState::Ready(index) = &outcome {
        // Changes that failed while the rebuild ran are retried too; a replay
        // that still fails is recorded instead of silently published away.
        let older = std::mem::take(&mut session.apply_health.lock_or_recover().unapplied);
        let (inc, left) = apply_with_fallback(session, index.incremental(), older, replay, &opts);
        if let Some(inc) = inc {
            outcome = IndexState::Ready(Arc::new(index::RepoIndex::from_incremental(
                session.root.clone(),
                inc,
            )));
        }
        note_unapplied(session, left);
    }
    if let IndexState::Ready(index) = &outcome {
        index.prewarm(true);
    }
    *session.state.write().unwrap_or_else(|e| e.into_inner()) = outcome;
}

/// Append a staleness note to the first text block of an already-finished result.
pub(crate) fn append_stale_note(result: CallToolResult, note: &str) -> CallToolResult {
    let mut result = result;
    if let Some(first) = result.content.first_mut() {
        if let Some(existing) = first.as_text() {
            let new_text = format!("{}\n{note}", existing.text.trim_end());
            result.content[0] = ContentBlock::text(new_text);
        }
    }
    result
}

pub(crate) fn unroutable_message(children: &[PathBuf]) -> String {
    let mut out = String::from(
        "多项目工作区：无法路由到唯一子仓。请加 root 参数（子仓名或绝对路径），或让 path 以子仓名/绝对路径开头。候选：\n",
    );
    for child in children {
        out.push_str(&format!("  {}\n", child.display()));
    }
    if let Some(name) = children
        .first()
        .and_then(|c| c.file_name())
        .and_then(|n| n.to_str())
    {
        out.push_str(&format!("例如 root=\"{name}\"\n"));
    }
    out.push_str("confidence: unknown");
    out
}

pub(crate) fn route_session(
    dispatch: &DispatchState,
    hint: &str,
) -> Result<Arc<RootSession>, String> {
    let (path, evicted) = {
        let mut disp = dispatch.dispatcher.lock_or_recover();
        match disp.route(hint) {
            Some(path) => {
                let path = path.to_path_buf();
                let evicted = disp.activate(&path, Instant::now());
                (path, evicted)
            }
            None => return Err(unroutable_message(&dispatch.children)),
        }
    };
    for evicted in evicted {
        teardown_child(dispatch, &evicted);
    }
    Ok(ensure_session(dispatch, &path))
}

fn ensure_session(dispatch: &DispatchState, root: &Path) -> Arc<RootSession> {
    let mut map = dispatch.sessions.lock_or_recover();
    if let Some(existing) = map.get(root) {
        return Arc::clone(existing);
    }
    let session = RootSession::new(
        root.to_path_buf(),
        dispatch.knobs.clone(),
        dispatch.cache_budget_bytes,
    );
    map.insert(root.to_path_buf(), Arc::clone(&session));
    drop(map);
    start_root_indexing(Arc::clone(&session));
    session
}

pub(crate) fn teardown_child(dispatch: &DispatchState, root: &Path) {
    let session = dispatch.sessions.lock_or_recover().remove(root);
    if let Some(session) = session {
        session.teardown();
    }
}

pub(crate) fn start_idle_ticker(dispatch: Arc<DispatchState>, stop: Arc<AtomicBool>) {
    let idle = dispatch.knobs.idle;
    let interval = {
        let quarter = idle / 4;
        let capped = quarter.min(Duration::from_secs(30));
        if capped.is_zero() {
            Duration::from_millis(50)
        } else {
            capped
        }
    };
    std::thread::Builder::new()
        .name("astrolabe-idle-evict".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let evicted = dispatch
                    .dispatcher
                    .lock_or_recover()
                    .evict_idle(Instant::now());
                for path in evicted {
                    teardown_child(&dispatch, &path);
                }
            }
        })
        .expect("failed to spawn idle-evict thread");
}

pub(crate) fn teardown_all(dispatch: &DispatchState) {
    let sessions: Vec<Arc<RootSession>> = dispatch
        .sessions
        .lock_or_recover()
        .drain()
        .map(|(_, s)| s)
        .collect();
    for session in sessions {
        session.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::index::DEFAULT_PARSE_CACHE_BYTES;
    use std::sync::atomic::AtomicU64;

    fn unique_temp_dir() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-session-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn fresh_window_wide_only_for_events_with_safety_scan() {
        let mut k = McpKnobs::default();
        assert_eq!(fresh_window_for(&k, None), Duration::from_secs(1860));
        k.safety_interval = Duration::ZERO;
        assert_eq!(fresh_window_for(&k, None), FRESH_WINDOW);
        k.safety_interval = Duration::from_secs(1800);
        k.watch_mode = Some(crate::knobs::WatchMode::Poll(Duration::from_secs(10)));
        assert_eq!(fresh_window_for(&k, None), FRESH_WINDOW);
    }

    #[test]
    fn fresh_window_follows_the_published_poll_cycle() {
        let mut k = McpKnobs {
            watch_mode: Some(crate::knobs::WatchMode::Poll(Duration::from_secs(10))),
            ..McpKnobs::default()
        };
        // 10 s cycle + 2.5 s slack: no forced verification inside a cycle.
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_millis(10_100))),
            Duration::from_millis(12_625)
        );
        // Backed-off poller on a huge tree: the window grows with it.
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_secs(40))),
            Duration::from_secs(50)
        );
        // Short cycles never drop below the default window.
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_millis(200))),
            FRESH_WINDOW
        );
        // Events mode that fell back to polling: the poller's cycle wins over
        // the 30 min events window.
        k.watch_mode = Some(crate::knobs::WatchMode::Events);
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_secs(10))),
            Duration::from_millis(12_500)
        );
    }

    #[test]
    fn poll_mode_barrier_does_not_force_scans_inside_a_cycle() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: Some(crate::knobs::WatchMode::Poll(Duration::from_secs(8))),
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        start_root_indexing(Arc::clone(&session));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !matches!(&*session.state.read().unwrap(), IndexState::Ready(_)) {
            assert!(Instant::now() < deadline, "index never ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        let log = session.freshness.lock().unwrap().clone().expect("log");
        // Age the watermark past the old fixed 5 s window, still inside the
        // 8 s cycle (the poller rescans at ~8 s).
        let wm = log.watermark();
        while wm.elapsed() < FRESH_WINDOW + Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if log.watermark() == wm && wm.elapsed() < Duration::from_secs(7) {
            assert!(matches!(run_barrier(&session), BarrierEffect::None));
            assert_eq!(
                log.watermark(),
                wm,
                "a query inside the poll cycle must not force a full scan"
            );
        }
        // A real edit is still caught by the next poll and repaired inline.
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let _ = run_barrier(&session);
            let found = match &*session.state.read().unwrap() {
                IndexState::Ready(index) => index.graph().symbols.iter().any(|s| s.name == "beta"),
                _ => false,
            };
            if found {
                break;
            }
            assert!(Instant::now() < deadline, "poller never surfaced b.py");
            std::thread::sleep(Duration::from_millis(100));
        }
        session.teardown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verification_wait_reports_timeout() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let (_rx, handle, log) = Watcher::new(dir.clone()).events(false).spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Live watcher: a requested verification advances the watermark.
        let before = log.watermark();
        log.request_verification();
        assert!(wait_watermark_advance(&log, before, Duration::from_secs(5)));
        // Stopped watcher: nobody advances it, the wait must report timeout.
        handle.stop();
        let before = log.watermark();
        log.request_verification();
        assert!(!wait_watermark_advance(
            &log,
            before,
            Duration::from_millis(50)
        ));
        assert!(verification_timeout_note().contains("索引可能过期"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_ready_applies_pending_changeset() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);

        let persist = session.persist_enabled();
        let cold = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        match &cold {
            IndexState::Ready(index) => {
                assert!(index.graph().symbols.iter().any(|s| s.name == "alpha"));
                assert!(!index
                    .graph()
                    .symbols
                    .iter()
                    .any(|s| s.name == "during_build"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        std::fs::write(dir.join("b.py"), "def during_build():\n    return 2\n").unwrap();
        *session.pending.lock().unwrap() = ChangeSet {
            added: vec![RelPath::new("b.py")],
            ..ChangeSet::default()
        };
        publish_ready(&session, cold);

        match &*session.state.read().unwrap() {
            IndexState::Ready(index) => {
                assert!(
                    index
                        .graph()
                        .symbols
                        .iter()
                        .any(|s| s.name == "during_build"),
                    "pending changeset must be applied before Ready is published"
                );
                assert!(index.graph().symbols.iter().any(|s| s.name == "alpha"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ready_session(dir: &Path) -> Arc<RootSession> {
        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.to_path_buf(), knobs, 8 * 1024 * 1024);
        let persist = session.persist_enabled();
        let cold = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        assert!(matches!(cold, IndexState::Ready(_)));
        *session.state.write().unwrap() = cold;
        session
    }

    fn has_symbol(session: &RootSession, name: &str) -> bool {
        match &*session.state.read().unwrap() {
            IndexState::Ready(index) => index.graph().symbols.iter().any(|s| s.name == name),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn inline_repair_classifies_ignored_and_deleted_suspects() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        std::fs::write(dir.join("gone.py"), "def gone_fn():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        assert!(has_symbol(&session, "gone_fn"));

        std::fs::write(dir.join(".gitignore"), "gen/\n").unwrap();
        std::fs::create_dir_all(dir.join("gen")).unwrap();
        std::fs::write(dir.join("gen/g.py"), "def ignored_fn():\n    return 1\n").unwrap();
        std::fs::write(dir.join("fresh.py"), "def fresh_fn():\n    return 1\n").unwrap();
        std::fs::remove_file(dir.join("gone.py")).unwrap();

        inline_repair(
            &session,
            vec![
                RelPath::new("gen/g.py"),
                RelPath::new("gone.py"),
                RelPath::new("fresh.py"),
            ],
        );
        assert!(has_symbol(&session, "fresh_fn"));
        assert!(
            !has_symbol(&session, "ignored_fn"),
            "gitignored must not enter"
        );
        assert!(!has_symbol(&session, "gone_fn"), "deleted must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuild_publish_replays_changes_applied_during_rebuild() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        // Rebuild "reads" the tree now (before the edit below).
        let persist = session.persist_enabled();
        let rebuilt = build_index(&session.root, persist, session.knobs.parse_cache_bytes);

        // An incremental batch lands on the live index while the rebuild runs.
        session.rebuild_inflight.store(true, Ordering::SeqCst);
        std::fs::write(dir.join("b.py"), "def during_rebuild():\n    return 2\n").unwrap();
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        assert!(has_symbol(&session, "during_rebuild"));

        publish_rebuilt(&session, rebuilt);
        session.rebuild_inflight.store(false, Ordering::SeqCst);
        assert!(
            has_symbol(&session, "during_rebuild"),
            "rebuild must not erase a change applied while it ran"
        );
        assert!(has_symbol(&session, "alpha"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_repairs_batch_that_is_sent_but_not_applied() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        let (_rx, handle, log) = Watcher::new(dir.clone()).events(false).spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            std::thread::sleep(Duration::from_millis(5));
        }
        *session.freshness.lock().unwrap() = Some(Arc::clone(&log));
        // The receiver is never drained, so the batch is "sent, not applied".
        std::fs::write(dir.join("b.py"), "def late_fn():\n    return 2\n").unwrap();
        let before = log.watermark();
        log.request_verification();
        assert!(wait_watermark_advance(&log, before, Duration::from_secs(5)));
        // Watermark is fresh, yet the index lacks b.py.
        assert!(!has_symbol(&session, "late_fn"));
        let _ = run_barrier(&session);
        assert!(has_symbol(&session, "late_fn"), "barrier must repair it");
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// perf/stale.py: append a function, let a query see it, then restore the
    /// original bytes within the watcher debounce. The deleted function must
    /// disappear on the next queries instead of lingering until a rebuild.
    #[test]
    fn edit_then_quick_revert_does_not_leave_stale_symbol() {
        let dir = unique_temp_dir();
        let orig = "def alpha():\n    return 1\n";
        std::fs::write(dir.join("a.py"), orig).unwrap();
        let knobs = McpKnobs {
            watch_mode: Some(WatchMode::Events),
            reindex_throttle: Duration::ZERO,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        start_root_indexing(Arc::clone(&session));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !matches!(&*session.state.read().unwrap(), IndexState::Ready(_)) {
            assert!(Instant::now() < deadline, "index never ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        for (round, gap_ms) in [0u64, 50, 300].into_iter().enumerate() {
            let name = format!("astro_stale_{round}");
            std::fs::write(
                dir.join("a.py"),
                format!("{orig}\n\ndef {name}():\n    return 2\n"),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let _ = run_barrier(&session);
                if has_symbol(&session, &name) {
                    break;
                }
                assert!(Instant::now() < deadline, "{name} never appeared");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(gap_ms));
            std::fs::write(dir.join("a.py"), orig).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let _ = run_barrier(&session);
                if !has_symbol(&session, &name) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{name} still served 5 s after the revert (gap {gap_ms} ms)"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(700));
        }
        session.teardown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Poison `m` by panicking while holding its guard.
    fn poison<T: Send + 'static>(m: Arc<Mutex<T>>) {
        let _ = std::thread::spawn(move || {
            let _guard = m.lock().unwrap();
            panic!("poison for test");
        })
        .join();
    }

    #[test]
    fn poisoned_session_locks_are_recovered_not_fatal() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        let (_rx, handle, log) = Watcher::new(dir.clone()).events(false).spawn_with_log();
        *session.freshness.lock().unwrap() = Some(log);
        poison(Arc::clone(&session.pending));
        poison(Arc::clone(&session.freshness));
        poison(Arc::clone(&session.rebuild_journal));
        poison(Arc::clone(&session.indexer_lock));
        poison(Arc::clone(&session.watch));
        assert!(session.pending.is_poisoned());

        // The reindex path and the query barrier keep working.
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        assert!(has_symbol(&session, "beta"));
        let _ = run_barrier(&session);
        assert!(!session.pending.is_poisoned(), "recovery clears the flag");
        session.teardown();
        handle.stop();

        let dispatch = DispatchState::new(vec![dir.clone()], McpKnobs::default(), 1 << 20);
        let _ = std::thread::scope(|s| {
            s.spawn(|| {
                let _g = dispatch.sessions.lock().unwrap();
                panic!("poison sessions");
            })
            .join()
        });
        assert!(dispatch.sessions.is_poisoned());
        teardown_all(&dispatch);
        teardown_child(&dispatch, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn barrier_note(session: &Arc<RootSession>) -> Option<String> {
        match run_barrier(session) {
            BarrierEffect::None => None,
            BarrierEffect::StaleNote(note) => Some(note),
        }
    }

    fn session_with_log(dir: &Path) -> (Arc<RootSession>, WatchHandle) {
        let session = ready_session(dir);
        let (_rx, handle, log) = Watcher::new(dir.to_path_buf())
            .events(false)
            .poll_interval(Duration::from_secs(3600))
            .spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            std::thread::sleep(Duration::from_millis(5));
        }
        *session.freshness.lock().unwrap() = Some(log);
        (session, handle)
    }

    #[test]
    fn failed_incremental_update_is_retried_not_dropped() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, handle) = session_with_log(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        assert!(!has_symbol(&session, "beta"));
        assert_eq!(session.apply_health.lock().unwrap().unapplied.len(), 1);
        // Inside the retry gap the query says the answer may be stale
        // (and kicks a rebuild); it does not pretend to be fresh.
        session.apply_health.lock().unwrap().rebuild_backoff_until =
            Some(Instant::now() + Duration::from_secs(600));
        let note = barrier_note(&session).expect("stale note while unapplied");
        assert!(
            note.contains("增量更新失败") && note.contains("b.py"),
            "{note}"
        );
        // Once the retry is due, the query retries inline and succeeds.
        session.apply_health.lock().unwrap().next_inline_retry = None;
        assert_eq!(barrier_note(&session), None);
        assert!(has_symbol(&session, "beta"));
        assert!(session.apply_health.lock().unwrap().unapplied.is_empty());
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_apply_failure_falls_back_to_rebuild_and_keeps_new_edits() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, handle) = session_with_log(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        // Older failure + new edit: combined apply fails, the new edit alone
        // still lands, the old one stays recorded.
        std::fs::write(dir.join("c.py"), "def gamma():\n    return 3\n").unwrap();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("c.py")],
                ..ChangeSet::default()
            },
        );
        assert!(
            has_symbol(&session, "gamma"),
            "new edit must not be blocked"
        );
        assert!(!has_symbol(&session, "beta"));
        // Retry fails again: the barrier notes it and rebuilds in background.
        session.fail_applies.store(1, Ordering::SeqCst);
        session.apply_health.lock().unwrap().next_inline_retry = None;
        let note = barrier_note(&session).expect("note while still failing");
        assert!(note.contains("增量更新失败"), "{note}");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !has_symbol(&session, "beta") || session.rebuild_inflight.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "rebuild never absorbed b.py");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(has_symbol(&session, "gamma"));
        assert_eq!(barrier_note(&session), None, "note clears once rebuilt");
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unroutable_message_lists_children() {
        let msg = unroutable_message(&[PathBuf::from("/proj/alpha"), PathBuf::from("/proj/beta")]);
        assert!(msg.contains("/proj/alpha"), "{msg}");
        assert!(msg.contains("/proj/beta"), "{msg}");
        assert!(msg.contains("confidence: unknown"), "{msg}");
    }

    #[test]
    fn watcher_started_after_teardown_does_not_leak() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: Some(WatchMode::Events),
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        // Eviction wins the race: teardown runs before the watcher starts.
        session.teardown();
        start_watching(&session, WatchMode::Events);
        assert!(
            session.watch.lock().unwrap().is_none(),
            "a watcher spawned for a torn-down session must be stopped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicted_session_skips_cold_build_and_never_publishes() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        session.teardown();
        bootstrap_root(Arc::clone(&session));
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Building
        ));
        // A build that finishes after eviction is dropped too.
        let persist = false;
        let built = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        publish_ready(&session, built);
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Building
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repo big enough that a debug cold build takes a while.
    fn write_many_py(dir: &Path, n: usize) {
        for i in 0..n {
            let sub = dir.join(format!("pkg{}", i % 40));
            std::fs::create_dir_all(&sub).unwrap();
            let mut body = String::new();
            for j in 0..12 {
                body.push_str(&format!(
                    "def f{i}_{j}(x):\n    return g{i}_{j}(x) + {j}\n\n\nclass C{i}_{j}:\n    def m(self):\n        return f{i}_{j}(1)\n\n\n"
                ));
            }
            std::fs::write(sub.join(format!("m{i}.py")), body).unwrap();
        }
    }

    #[test]
    fn teardown_cancels_an_in_flight_cold_build() {
        let dir = unique_temp_dir();
        write_many_py(&dir, 1500);
        // Control: how long the full build takes on this machine.
        let t = Instant::now();
        let _ = build_index(&dir, false, DEFAULT_PARSE_CACHE_BYTES);
        let full = t.elapsed();

        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        start_root_indexing(Arc::clone(&session));
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.builds_active.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "cold build never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(full / 10);
        let asked = Instant::now();
        session.teardown();
        assert!(
            session.wait_builds_idle(Duration::from_secs(30)),
            "cancelled build never stopped"
        );
        let latency = asked.elapsed();
        assert!(
            latency < full / 2,
            "cancel took {latency:?}, a full build takes {full:?}"
        );
        assert!(
            matches!(*session.state.read().unwrap(), IndexState::Building),
            "a cancelled build must not be published"
        );
        // Cancelled before the parse finished: no stamps were committed (the
        // repo is far below the 32 MB mid-run flush).
        if let Ok(Some(store)) = astrolabe_core::store::Store::open_read_only(
            &astrolabe_core::index::store_path(&session.root),
        ) {
            assert!(store.file_stamp("pkg0/m0.py").unwrap().is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_background_rebuild_does_not_publish() {
        let dir = unique_temp_dir();
        write_many_py(&dir, 1500);
        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let cold = build_index(&session.root, false, session.knobs.parse_cache_bytes);
        let IndexState::Ready(before) = cold else {
            panic!("cold build failed");
        };
        *session.state.write().unwrap() = IndexState::Ready(Arc::clone(&before));
        trigger_background_rebuild_arc(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.builds_active.load(Ordering::SeqCst) == 0
            && session.rebuild_inflight.load(Ordering::SeqCst)
        {
            assert!(Instant::now() < deadline, "rebuild never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        session.teardown();
        assert!(session.wait_builds_idle(Duration::from_secs(30)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.rebuild_inflight.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "rebuild flag never cleared");
            std::thread::sleep(Duration::from_millis(1));
        }
        match &*session.state.read().unwrap() {
            IndexState::Ready(now) => assert!(
                Arc::ptr_eq(now, &before),
                "a rebuild cancelled by teardown must not replace the index"
            ),
            other => panic!("unexpected state {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_pending_swaps_index_without_holding_state_lock_during_reindex() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let cold = build_index(&session.root, false, session.knobs.parse_cache_bytes);
        *session.state.write().unwrap() = cold;
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let mut pending = ChangeSet {
            added: vec![RelPath::new("b.py")],
            ..ChangeSet::default()
        };
        // A reader holding the read guard must not deadlock the apply (it
        // only takes the write lock for the swap, after dropping its read).
        let old = match &*session.state.read().unwrap() {
            IndexState::Ready(i) => Arc::clone(i),
            _ => panic!("ready"),
        };
        apply_pending(&session, &mut pending);
        assert!(pending.is_empty());
        match &*session.state.read().unwrap() {
            IndexState::Ready(i) => {
                assert!(i.graph().symbols.iter().any(|s| s.name == "beta"));
                assert!(!old.graph().symbols.iter().any(|s| s.name == "beta"));
            }
            _ => panic!("ready"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
