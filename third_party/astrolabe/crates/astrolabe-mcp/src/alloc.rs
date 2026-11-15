//! Hand freed heap back to the OS after big index swaps.
//!
//! On a 4.35M-line repo about 45% of RSS after an index build is free memory
//! kept in allocator arenas (rayon parses on every core, so every core gets an
//! arena full of parse temporaries). Two layers are purged:
//!
//! 1. `mi_collect(true)` collects the calling thread's default mimalloc heap.
//!    We also queue a collection on every Rayon worker, so cross-thread frees
//!    from index swaps are drained by the persistent workers that own them.
//!    Worker collection is asynchronous: busy workers collect when next idle.
//! 2. The platform allocator is trimmed too, for memory the C libraries
//!    (tree-sitter, sqlite, system frameworks) allocated through the system
//!    `malloc` rather than through Rust's global allocator:
//!    - Linux glibc: `malloc_trim(0)` walks every arena and `madvise`s the
//!      free pages away.
//!    - macOS: `malloc_zone_pressure_relief(NULL, 0)` relieves pressure on
//!      all registered malloc zones.
//!    - Windows: `_heapmin()` returns free CRT heap pages to the OS.
//!
//! This reaches the global Rayon pool from the background trim thread (the
//! index uses that pool). When called within a custom Rayon pool, it reaches
//! that pool instead. It cannot collect arbitrary non-Rayon live thread heaps.
//! The live data (graph, NameIndex) is untouched. Linux musl and other targets
//! compile this to a no-op. `ASTROLABE_MALLOC_TRIM=0` turns it off (A/B and
//! escape hatch).

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

const SUPPORTED: bool = cfg!(any(
    all(target_os = "linux", target_env = "gnu"),
    target_os = "macos",
    target_os = "windows",
));

/// Collect the calling heap and request collection on each Rayon worker, then
/// trim the platform allocator. Worker collection may finish after this returns.
/// If another thread is already trimming, this only marks the heap dirty and
/// returns at once; that thread does one more pass before it stops. Returns whether this
/// call did a local walk and requested worker collection itself.
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

#[cfg(any(
    all(target_os = "linux", target_env = "gnu"),
    target_os = "macos",
    target_os = "windows",
))]
fn trim_now() {
    let started = std::time::Instant::now();
    collect_current_thread();
    // Do not wait for workers while holding RUNNING or any caller-owned locks.
    // A synchronous broadcast could deadlock if a busy worker needs such a lock.
    request_worker_collection(collect_current_thread);
    let native_released = trim_platform_allocator();
    tracing::debug!(
        native_released,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "mimalloc local trim; worker collection queued"
    );
}

#[cfg(not(any(
    all(target_os = "linux", target_env = "gnu"),
    target_os = "macos",
    target_os = "windows",
)))]
fn trim_now() {}

// Keep collection on the owner thread: mi_collect does NOT walk all live
// thread heaps. In particular, native malloc trimming cannot reclaim these pages.
#[cfg(any(
    test,
    all(target_os = "linux", target_env = "gnu"),
    target_os = "macos",
    target_os = "windows",
))]
fn collect_current_thread() {
    // SAFETY: this public mimalloc API collects only the current thread's
    // default heap. It takes no application pointers and does not access a
    // foreign thread's heap through an unsound mi_heap_collect call.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

#[cfg(any(
    test,
    all(target_os = "linux", target_env = "gnu"),
    target_os = "macos",
    target_os = "windows",
))]
fn request_worker_collection(collect: impl Fn() + Send + Sync + 'static) {
    // Unlike N ordinary spawned jobs, a broadcast runs once on EVERY worker;
    // work stealing cannot let one worker consume the whole collection pass.
    // Outside Rayon this targets the global pool used by index construction.
    // The callback owns its captures, so no caller stack or heap pointer escapes.
    rayon::spawn_broadcast(move |_| collect());
}

/// Trim the system allocator used by C libraries that do not go through
/// Rust's global allocator. Returns whether the platform reported releasing
/// anything.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_platform_allocator() -> bool {
    // SAFETY: malloc_trim takes no pointers and is thread-safe (it locks
    // each arena while trimming it).
    unsafe { libc::malloc_trim(0) != 0 }
}

#[cfg(target_os = "macos")]
fn trim_platform_allocator() -> bool {
    // libmalloc is part of libSystem; the symbol has shipped since 10.7.
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: a null zone asks libmalloc to relieve every registered zone.
    // The call takes no pointer we own and is safe to run concurrently.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) != 0 }
}

#[cfg(target_os = "windows")]
fn trim_platform_allocator() -> bool {
    // `_heapmin` lives in the CRT that Rust links against.
    unsafe extern "C" {
        fn _heapmin() -> std::ffi::c_int;
    }
    // SAFETY: _heapmin operates on the CRT heap and is thread-safe. It
    // returns 0 on success (whether or not pages were actually released).
    unsafe { _heapmin() == 0 }
}

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
    fn trim_is_supported_and_returns_true_on_supported_targets() {
        if !SUPPORTED {
            // musl and other targets compile the trim to a no-op.
            assert!(!release_free_memory(), "unsupported targets must not trim");
            return;
        }
        if !enabled() {
            // ASTROLABE_MALLOC_TRIM opted out of trims for this process.
            return;
        }
        // A concurrent test may hold RUNNING; retry rather than racing it.
        let mut released = false;
        for _ in 0..50 {
            released = release_free_memory();
            if released {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(released, "release_free_memory should run a trim");
    }

    #[test]
    fn collection_is_requested_on_every_persistent_worker() {
        use std::collections::HashSet;
        use std::sync::mpsc;
        use std::time::Duration;

        // A private pool makes the worker count deterministic without changing
        // process-wide Rayon settings used by other tests.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(3)
            .build()
            .unwrap();
        let caller = std::thread::current().id();
        let mut previous_workers = None;
        for _ in 0..2 {
            let (tx, rx) = mpsc::channel();
            pool.install(|| {
                request_worker_collection(move || {
                    collect_current_thread();
                    tx.send(std::thread::current().id()).unwrap();
                });
            });
            let workers: HashSet<_> = (0..3)
                .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
                .collect();
            assert_eq!(workers.len(), 3, "every owner thread must collect");
            assert!(!workers.contains(&caller));
            if let Some(previous) = previous_workers.replace(workers.clone()) {
                assert_eq!(workers, previous, "persistent workers must be revisited");
            }
        }
        // RSS is deliberately not asserted: live allocations, allocator build
        // options, and OS page accounting make it nondeterministic. This checks
        // the necessary owner-thread execution rather than promising an RSS delta.
    }

    #[test]
    fn background_collection_reaches_the_global_pool() {
        use std::collections::HashSet;
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        let (caller, count) = std::thread::spawn(move || {
            assert!(rayon::current_thread_index().is_none());
            let count = rayon::current_num_threads();
            request_worker_collection(move || {
                collect_current_thread();
                tx.send(std::thread::current().id()).unwrap();
            });
            (std::thread::current().id(), count)
        })
        .join()
        .unwrap();
        let workers: HashSet<_> = (0..count)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert_eq!(workers.len(), count);
        assert!(!workers.contains(&caller));
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
