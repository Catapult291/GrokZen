//! Hand freed heap back to the OS after big index swaps.
//!
//! On a 4.35M-line repo about 45% of RSS after an index build is free memory
//! that glibc keeps in its per-thread arenas (rayon parses on every core, so
//! every core gets an arena full of parse temporaries). `malloc_trim(0)` walks
//! every arena and `madvise`s the free pages away; the live data (graph,
//! NameIndex) is untouched.
//!
//! Only Linux glibc has `malloc_trim`. musl, macOS and Windows compile this to
//! a no-op. `ASTROLABE_MALLOC_TRIM=0` turns it off (A/B and escape hatch).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Held while a trim walks the arenas. Only one walk at a time: two
/// concurrent walks just contend on the same arena locks.
static RUNNING: Mutex<()> = Mutex::new(());
/// Someone asked for a trim since the current walk started.
static DIRTY: AtomicBool = AtomicBool::new(false);
/// A trim thread is spawned and still waiting for its moment.
static QUEUED: AtomicBool = AtomicBool::new(false);

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| parse_enabled(std::env::var("ASTROLABE_MALLOC_TRIM").ok().as_deref()))
}

fn parse_enabled(raw: Option<&str>) -> bool {
    !matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "off" | "false" | "no")
    )
}

const SUPPORTED: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

/// Return free heap pages to the OS, on the calling thread. If another
/// thread is already trimming, this only marks the heap dirty and returns at
/// once; that thread does one more pass before it stops. Returns whether this
/// call did a walk itself.
pub(crate) fn release_free_memory() -> bool {
    if !enabled() || !SUPPORTED {
        return false;
    }
    DIRTY.store(true, Ordering::Release);
    let Ok(_guard) = RUNNING.try_lock() else {
        return false;
    };
    // Bounded: a steady stream of requests cannot pin this thread forever.
    for _ in 0..3 {
        if !DIRTY.swap(false, Ordering::AcqRel) {
            break;
        }
        trim_now();
    }
    true
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_now() {
    let started = std::time::Instant::now();
    // SAFETY: malloc_trim takes no pointers and is thread-safe (it locks
    // each arena while trimming it).
    let released = unsafe { libc::malloc_trim(0) };
    tracing::debug!(
        released = released != 0,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "malloc_trim"
    );
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_now() {}

/// How long applies must stay quiet before [`release_free_memory_when_quiet`]
/// trims. Each apply reassembles the whole graph, so trimming between two
/// applies only makes the second one fault the same pages back in; one trim
/// after the burst keeps the RSS win without that cost.
const QUIET: std::time::Duration = std::time::Duration::from_secs(5);

/// Milliseconds since [`epoch`] of the latest `when_quiet` request.
static LAST_REQUEST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn epoch() -> std::time::Instant {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// Trailing-edge [`release_free_memory`]: trim on a background thread once
/// no further request has arrived for [`QUIET`]. Never blocks the caller; a
/// burst of requests costs one thread and one walk.
pub(crate) fn release_free_memory_when_quiet() {
    if !enabled() || !SUPPORTED {
        return;
    }
    let _ = epoch();
    LAST_REQUEST_MS.store(now_ms(), Ordering::Release);
    if QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("astrolabe-trim".into())
        .spawn(|| {
            let quiet = QUIET.as_millis() as u64;
            loop {
                let due = LAST_REQUEST_MS.load(Ordering::Acquire) + quiet;
                let now = now_ms();
                if now >= due {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(due - now));
            }
            // Clear before trimming: a request that lands during the walk
            // queues its own trailing pass.
            QUEUED.store(false, Ordering::Release);
            release_free_memory();
        });
    if spawned.is_err() {
        QUEUED.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values_that_disable_trim() {
        assert!(parse_enabled(None));
        assert!(parse_enabled(Some("1")));
        assert!(parse_enabled(Some("")));
        for off in ["0", "off", "OFF", " false ", "no"] {
            assert!(!parse_enabled(Some(off)), "{off:?} should disable");
        }
    }

    #[test]
    fn trim_is_safe_to_call_repeatedly_and_concurrently() {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..8 {
                        let _ = release_free_memory();
                        release_free_memory_when_quiet();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
