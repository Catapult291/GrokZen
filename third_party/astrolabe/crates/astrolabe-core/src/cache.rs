//! Bounded in-memory caching. Memory is a product guarantee here.
//!
//! Built on moka, which gives TTL, TTI (time-to-idle — the sliding window we
//! actually want), per-entry expiry, a `weigher` for size-aware eviction, and
//! an eviction listener for spilling to `store`.
//!
//! Why this matters: the two systems we measured both failed here. One kept
//! unbounded symbol caches with no eviction of any kind; the other capped by
//! entry count only, so a cap of 10000 line-arrays said nothing about bytes.
//!
//! Rules:
//!   * Every cache declares a byte budget via `weigher`, never just a count.
//!   * Use `time_to_idle`, not only `time_to_live` — a repo untouched for N
//!     minutes should release, while an actively used one should not expire
//!     mid-session.
//!   * The sum of all budgets is the process's steady-state ceiling, and that
//!     number goes in the README as a promise.

use std::{mem::size_of, sync::Arc, time::Duration};

use moka::sync::Cache;

/// Default ceiling for all in-memory caches combined.
pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Release a repo's hot state after this long with no access.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

pub struct CacheConfig {
    pub budget_bytes: u64,
    pub idle_ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
            idle_ttl: DEFAULT_IDLE_TTL,
        }
    }
}

/// File-content cache: the read-side hot layer for slicing and search.
pub struct FileCache {
    cache: Cache<String, CachedFile>,
}

#[derive(Clone)]
struct CachedFile {
    lines: Arc<Vec<String>>,
    mtime_ms: u64,
    /// On-disk byte length when the caller supplied one.
    len: Option<u64>,
}

impl FileCache {
    pub fn new(cfg: CacheConfig) -> Self {
        let cache = Cache::builder()
            .max_capacity(cfg.budget_bytes)
            .weigher(|_path: &String, file: &CachedFile| -> u32 {
                // Approximate the owned line storage as:
                //   Vec<String> header
                // + one String header per line
                // + all UTF-8 string contents.
                //
                // Arc and CachedFile metadata are small fixed costs and are
                // intentionally excluded; saturating to u32::MAX is required
                // by moka's per-entry weight type.
                let bytes = size_of::<Vec<String>>()
                    .saturating_add(file.lines.len().saturating_mul(size_of::<String>()))
                    .saturating_add(
                        file.lines
                            .iter()
                            .map(|line| line.len())
                            .fold(0usize, usize::saturating_add),
                    );
                u32::try_from(bytes).unwrap_or(u32::MAX)
            })
            .time_to_idle(cfg.idle_ttl)
            .build();

        Self { cache }
    }

    /// Returns the cached lines when the file's mtime still matches.
    pub fn get(&self, path: &str, mtime_ms: u64) -> Option<Arc<Vec<String>>> {
        let file = self.cache.get(path)?;
        if file.mtime_ms == mtime_ms {
            Some(file.lines)
        } else {
            self.cache.invalidate(path);
            None
        }
    }

    pub fn insert(&self, path: &str, lines: Vec<String>, mtime_ms: u64) {
        self.insert_arc(path, Arc::new(lines), mtime_ms);
    }

    /// Insert already-shared lines without deep-copying them.
    pub fn insert_arc(&self, path: &str, lines: Arc<Vec<String>>, mtime_ms: u64) {
        self.cache.insert(
            path.to_owned(),
            CachedFile {
                lines,
                mtime_ms,
                len: None,
            },
        );
    }

    /// Like [`Self::get`], but the file's byte length must match too. An
    /// mtime restored by `cp -p`/`git stash`, or two writes inside one
    /// timestamp tick, usually change the length; mtime alone cannot tell.
    /// An entry stored without a length never matches here.
    pub fn get_checked(&self, path: &str, mtime_ms: u64, len: u64) -> Option<Arc<Vec<String>>> {
        let file = self.cache.get(path)?;
        if file.mtime_ms == mtime_ms && file.len == Some(len) {
            Some(file.lines)
        } else {
            self.cache.invalidate(path);
            None
        }
    }

    /// Store shared lines together with the file's byte length, for
    /// [`Self::get_checked`].
    pub fn insert_checked(&self, path: &str, lines: Arc<Vec<String>>, mtime_ms: u64, len: u64) {
        self.cache.insert(
            path.to_owned(),
            CachedFile {
                lines,
                mtime_ms,
                len: Some(len),
            },
        );
    }

    /// Current weighted size in bytes — exposed so tests can assert the
    /// ceiling actually holds under load.
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    fn config(budget_bytes: u64, idle_ttl: Duration) -> CacheConfig {
        CacheConfig {
            budget_bytes,
            idle_ttl,
        }
    }

    #[test]
    fn checked_get_rejects_same_mtime_different_length() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert_checked("a.rs", Arc::new(vec!["old".into()]), 1000, 4);
        assert!(cache.get_checked("a.rs", 1000, 4).is_some());
        // Same mtime, different length: the file changed under a restored timestamp.
        assert!(cache.get_checked("a.rs", 1000, 5).is_none());
        assert!(
            cache.get_checked("a.rs", 1000, 4).is_none(),
            "a mismatch must evict the stale entry"
        );
        // Entries stored without a length never satisfy a checked read.
        cache.insert("b.rs", vec!["x".into()], 7);
        assert!(cache.get_checked("b.rs", 7, 1).is_none());
    }

    #[test]
    fn insert_then_get_returns_shared_lines() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert("src/lib.rs", vec!["first".into(), "second".into()], 42);

        let lines = cache.get("src/lib.rs", 42).expect("entry should exist");
        assert_eq!(&*lines, &["first", "second"]);
    }

    #[test]
    fn changed_mtime_is_a_miss_and_invalidates_entry() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert("src/lib.rs", vec!["old".into()], 10);

        assert!(cache.get("src/lib.rs", 11).is_none());
        assert!(cache.get("src/lib.rs", 10).is_none());
    }

    #[test]
    fn byte_budget_is_enforced() {
        const BUDGET: u64 = 1024;
        let cache = FileCache::new(config(BUDGET, Duration::from_secs(60)));

        for index in 0..8 {
            cache.insert(
                &format!("large-{index}"),
                vec!["x".repeat(BUDGET as usize)],
                index,
            );
        }
        cache.cache.run_pending_tasks();

        assert!(cache.weighted_size() <= BUDGET);
    }

    #[test]
    fn idle_entries_expire() {
        let cache = FileCache::new(config(1024, Duration::from_millis(100)));
        cache.insert("idle", vec!["line".into()], 1);
        assert!(cache.get("idle", 1).is_some());

        thread::sleep(Duration::from_millis(175));

        assert!(cache.get("idle", 1).is_none());
    }

    #[test]
    fn concurrent_reads_and_writes_do_not_panic() {
        let cache = Arc::new(FileCache::new(config(64 * 1024, Duration::from_secs(60))));
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for index in 0..250 {
                        let path = format!("worker-{worker}/file-{}", index % 20);
                        cache.insert(&path, vec![format!("line-{index}")], index);
                        let _ = cache.get(&path, index);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("cache worker panicked");
        }
        cache.cache.run_pending_tasks();
        assert!(cache.weighted_size() <= 64 * 1024);
    }
}
