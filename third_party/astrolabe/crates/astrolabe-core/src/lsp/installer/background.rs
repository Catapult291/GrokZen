//! In-process once-semantics for auto-install downloads.
//!
//! A download (plus checksum, extract and the cross-process file lock) can
//! take minutes. Discovery runs inside the pool's per-language start gate and
//! inside `get_languages`, so doing it inline held every other request for
//! that language (and the whole `get_languages` call) for that long. Here the
//! first caller starts the install on a background thread and everyone,
//! including that caller, gets [`BackgroundInstall::Pending`] until it
//! finishes. One install per key runs at a time; a failed one is not retried
//! before [`RETRY_FAILED_AFTER`], so a broken mirror is not re-downloaded on
//! every pool cooldown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Marker carried in the "unavailable" message while an install runs. The
/// pool keys a short retry on it (instead of its full start cooldown) so the
/// server starts soon after the download lands.
pub const INSTALL_PENDING: &str = "auto-install in progress";

/// How long a failed install is remembered before a request may retry it.
pub const RETRY_FAILED_AFTER: Duration = Duration::from_secs(600);

/// State of one install as seen by a caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundInstall {
    /// Installed; path of the runnable binary.
    Ready(PathBuf),
    /// Running on a background thread for `elapsed`.
    Pending { elapsed: Duration },
    /// Last attempt failed (within the retry window).
    Failed(String),
}

enum Job {
    Running(Instant),
    Done(PathBuf),
    Failed { error: String, at: Instant },
}

/// Registry of installs keyed by artifact (cache root, name, version).
pub struct Installs {
    jobs: Mutex<HashMap<String, Job>>,
    cv: Condvar,
    retry_after: Duration,
}

fn lock(m: &Mutex<HashMap<String, Job>>) -> MutexGuard<'_, HashMap<String, Job>> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Installs {
    pub fn new(retry_after: Duration) -> Arc<Self> {
        Arc::new(Installs {
            jobs: Mutex::new(HashMap::new()),
            cv: Condvar::new(),
            retry_after,
        })
    }

    /// Process-wide registry used by discovery.
    pub fn global() -> &'static Arc<Installs> {
        static GLOBAL: OnceLock<Arc<Installs>> = OnceLock::new();
        GLOBAL.get_or_init(|| Installs::new(RETRY_FAILED_AFTER))
    }

    /// Current state of `key`, starting `run` on a background thread when no
    /// install is running, none succeeded, and no recent one failed. Waits at
    /// most `wait` for the outcome; the caller never runs `run` itself.
    pub fn poll_or_start<F>(
        self: &Arc<Self>,
        key: &str,
        wait: Duration,
        run: F,
    ) -> BackgroundInstall
    where
        F: FnOnce() -> Result<PathBuf, String> + Send + 'static,
    {
        let mut jobs = lock(&self.jobs);
        let start = match jobs.get(key) {
            Some(Job::Running(_)) => false,
            // A binary deleted behind our back is installed again.
            Some(Job::Done(path)) if path.exists() => {
                return BackgroundInstall::Ready(path.clone());
            }
            Some(Job::Failed { error, at }) if at.elapsed() < self.retry_after => {
                return BackgroundInstall::Failed(error.clone());
            }
            _ => true,
        };
        if start {
            jobs.insert(key.to_string(), Job::Running(Instant::now()));
            let me = Arc::clone(self);
            let owned = key.to_string();
            let spawned = std::thread::Builder::new()
                .name("astrolabe-lsp-install".into())
                .spawn(move || {
                    let outcome = run();
                    let mut jobs = lock(&me.jobs);
                    let job = match outcome {
                        Ok(path) => Job::Done(path),
                        Err(error) => Job::Failed {
                            error,
                            at: Instant::now(),
                        },
                    };
                    jobs.insert(owned, job);
                    me.cv.notify_all();
                });
            if let Err(err) = spawned {
                let error = format!("could not start install thread: {err}");
                jobs.insert(
                    key.to_string(),
                    Job::Failed {
                        error: error.clone(),
                        at: Instant::now(),
                    },
                );
                return BackgroundInstall::Failed(error);
            }
        }
        let deadline = Instant::now() + wait;
        loop {
            match jobs.get(key) {
                Some(Job::Done(path)) => return BackgroundInstall::Ready(path.clone()),
                Some(Job::Failed { error, .. }) => return BackgroundInstall::Failed(error.clone()),
                Some(Job::Running(since)) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return BackgroundInstall::Pending {
                            elapsed: since.elapsed(),
                        };
                    }
                    jobs = self
                        .cv
                        .wait_timeout(jobs, deadline - now)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
                None => {
                    return BackgroundInstall::Pending {
                        elapsed: Duration::ZERO,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn concurrent_callers_start_one_install_and_none_blocks() {
        let installs = Installs::new(Duration::from_secs(60));
        let runs = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::write(&bin, b"x").unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let installs = Arc::clone(&installs);
            let runs = Arc::clone(&runs);
            let release_rx = Arc::clone(&release_rx);
            let bin = bin.clone();
            handles.push(std::thread::spawn(move || {
                let started = Instant::now();
                let got = installs.poll_or_start("k", Duration::ZERO, move || {
                    runs.fetch_add(1, Ordering::SeqCst);
                    // Blocks until the test releases it: a slow download.
                    let _ = release_rx.lock().unwrap().recv();
                    Ok(bin)
                });
                (got, started.elapsed())
            }));
        }
        for h in handles {
            let (got, took) = h.join().unwrap();
            assert!(matches!(got, BackgroundInstall::Pending { .. }), "{got:?}");
            assert!(took < Duration::from_secs(2), "caller blocked {took:?}");
        }
        release_tx.send(()).unwrap();
        let ready = installs.poll_or_start("k", Duration::from_secs(5), || {
            panic!("must not start a second install while one is done/running")
        });
        assert_eq!(ready, BackgroundInstall::Ready(bin));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failure_is_remembered_until_the_retry_window_passes() {
        let installs = Installs::new(Duration::from_millis(150));
        let runs = Arc::new(AtomicUsize::new(0));
        let attempt = |installs: &Arc<Installs>, runs: &Arc<AtomicUsize>| {
            let runs = Arc::clone(runs);
            installs.poll_or_start("k", Duration::from_secs(5), move || {
                runs.fetch_add(1, Ordering::SeqCst);
                Err("mirror down".into())
            })
        };
        assert_eq!(
            attempt(&installs, &runs),
            BackgroundInstall::Failed("mirror down".into())
        );
        assert_eq!(
            attempt(&installs, &runs),
            BackgroundInstall::Failed("mirror down".into())
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "no re-download inside window"
        );
        std::thread::sleep(Duration::from_millis(200));
        let _ = attempt(&installs, &runs);
        assert_eq!(runs.load(Ordering::SeqCst), 2, "retried after the window");
    }

    #[test]
    fn deleted_binary_is_installed_again() {
        let installs = Installs::new(Duration::from_secs(60));
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let make = |bin: PathBuf| {
            move || {
                std::fs::write(&bin, b"x").map_err(|e| e.to_string())?;
                Ok(bin)
            }
        };
        let first = installs.poll_or_start("k", Duration::from_secs(5), make(bin.clone()));
        assert_eq!(first, BackgroundInstall::Ready(bin.clone()));
        std::fs::remove_file(&bin).unwrap();
        let again = installs.poll_or_start("k", Duration::from_secs(5), make(bin.clone()));
        assert_eq!(again, BackgroundInstall::Ready(bin.clone()));
        assert!(bin.exists());
    }
}
