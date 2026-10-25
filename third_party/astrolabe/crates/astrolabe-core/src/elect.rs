//! Per-repo indexer leader election via a cross-process exclusive file lock.
//!
//! Several Astrolabe MCP processes commonly attach to the same workspace
//! (nine on a development machine is not unusual). The redb store at
//! `<repo>/.astrolabe/index.redb` is single-writer: a second process that
//! opens it gets `DatabaseAlreadyOpen`. This module is the election that
//! happens *before* that open: one process becomes the indexer **leader**
//! and is allowed to persist; everyone else is a **follower**.
//!
//! ## Design stance: demotion, not correctness
//!
//! The lock is a *hint used to demote*, not a correctness barrier. A process
//! that loses the election still serves every tool against an in-memory
//! graph; it simply skips writing the on-disk index cache. Taking the lock
//! is not required to answer a query, and dropping it must not take the
//! process down.
//!
//! ## Why flock / LockFileEx, not a PID file
//!
//! A PID (or "I exist") file survives `kill -9`, a panic, and a host crash.
//! The next process then either refuses to become leader forever or has to
//! guess whether the recorded PID is still alive — a classic stale-lock
//! mess. Advisory locks tied to an open file descriptor do not: the kernel
//! releases them when the last fd/handle is closed, including process
//! death. That is the whole reason this module exists.
//!
//! We use [`std::fs::File::try_lock`] (stable since 1.89; this crate's MSRV
//! is 1.90) rather than `fd-lock` or `fs2`:
//!
//! - It is `flock(LOCK_EX|LOCK_NB)` on Unix and `LockFileEx` with
//!   `LOCKFILE_EXCLUSIVE_LOCK|LOCKFILE_FAIL_IMMEDIATELY` on Windows — the
//!   six release targets (macOS arm64/x64, Linux gnu/musl x64, Linux gnu
//!   arm64, Windows msvc x64) are covered with no extra crate.
//! - `fd-lock`'s write guard borrows the `RwLock`, so an owned
//!   [`IndexerLock`] would need a self-referential struct or a leaked fd.
//! - `fs2` is unmaintained and its `FileExt` now collides with `std`.
//!
//! ## Cost model
//!
//! `try_acquire` is one `open` plus one non-blocking lock syscall. There is
//! no wait loop, no thread, no I/O after the election. The lock file is an
//! empty inode at [`lock_path`]; we never unlink it on drop (unlinking a
//! held flock file is a well-known NFS/stale-name footgun). Followers pay
//! the same one-shot cost, get `Ok(None)`, and move on.
//!
//! ## Usage
//!
//! ```ignore
//! match astrolabe_core::elect::IndexerLock::try_acquire(&root)? {
//!     Some(_leader) => { /* persist to index.redb */ }
//!     None => { /* follower: serve tools, skip store writes */ }
//! }
//! ```

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Lock file for the indexer election under `root`.
///
/// The parent directory (`.astrolabe/`) is **not** created here; [`IndexerLock::try_acquire`]
/// is responsible for that, and surfaces a real [`io::Error`] if creation fails.
pub fn lock_path(root: &Path) -> PathBuf {
    root.join(".astrolabe").join("indexer.lock")
}

/// Guard for the exclusive indexer lock.
///
/// While this value is alive the calling process is the per-repo leader.
/// `Drop` closes the fd/handle; the OS then releases the lock, including
/// when the process is killed. The struct is deliberately not `Clone`:
/// cloning the fd is unspecified if it already holds a lock (and can
/// deadlock on some platforms).
#[derive(Debug)]
#[must_use = "dropping IndexerLock releases leadership"]
pub struct IndexerLock {
    /// Held open so the OS lock stays alive. Closed on drop.
    file: File,
}

impl IndexerLock {
    /// Non-blocking attempt to become the indexer leader for `root`.
    ///
    /// * `Ok(Some(lock))` — this process holds the exclusive lock.
    /// * `Ok(None)` — another handle (this process or another) already holds it.
    /// * `Err(_)` — a real I/O failure (permissions, `.astrolabe` is a file, …).
    ///
    /// Creates `<root>/.astrolabe/` if it is missing. Creation failure is
    /// returned, never panicked.
    pub fn try_acquire(root: &Path) -> io::Result<Option<IndexerLock>> {
        let path = lock_path(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Windows LockFileEx rejects append-only handles; read+write is the
        // portable open. Do not truncate: a sibling may already hold the inode.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(IndexerLock { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    /// Whether this guard still represents a live exclusive lock.
    ///
    /// Always `true` for a constructed [`IndexerLock`]: the OS lock is tied
    /// to the open file descriptor, so the guard cannot exist without holding
    /// it. Callers typically write `lock.as_ref().is_some_and(IndexerLock::held)`.
    pub fn held(&self) -> bool {
        let _ = &self.file;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
                "astrolabe-elect-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            let path = fs::canonicalize(&path).expect("canonicalize test directory");
            TestDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lock_path_is_under_dot_astrolabe() {
        let root = Path::new("repo");
        assert_eq!(
            lock_path(root),
            Path::new("repo").join(".astrolabe").join("indexer.lock")
        );
    }

    #[test]
    fn try_acquire_is_exclusive_in_process_and_releases_on_drop() {
        let dir = TestDir::new();
        let first = IndexerLock::try_acquire(dir.path())
            .expect("first acquire I/O")
            .expect("first acquire should win");
        assert!(first.held());

        let second = IndexerLock::try_acquire(dir.path()).expect("second acquire I/O");
        assert!(
            second.is_none(),
            "flock is per-open-file-description; a second fd in this process must lose"
        );

        drop(first);
        let third = IndexerLock::try_acquire(dir.path())
            .expect("third acquire I/O")
            .expect("acquire after drop should win");
        assert!(third.held());
    }

    #[test]
    fn try_acquire_creates_missing_astrolabe_dir() {
        let dir = TestDir::new();
        let astrolabe = dir.path().join(".astrolabe");
        assert!(
            !astrolabe.exists(),
            "fixture must not pre-create .astrolabe"
        );

        let lock = IndexerLock::try_acquire(dir.path())
            .expect("acquire I/O")
            .expect("acquire should win on an empty root");
        assert!(astrolabe.is_dir(), ".astrolabe should be created");
        assert!(
            lock_path(dir.path()).is_file(),
            "indexer.lock should exist after acquire"
        );
        assert!(lock.held());
    }

    #[test]
    fn try_acquire_errors_when_astrolabe_is_a_file() {
        let dir = TestDir::new();
        fs::write(dir.path().join(".astrolabe"), b"not a directory")
            .expect("plant a file where the lock dir should be");
        let err = IndexerLock::try_acquire(dir.path())
            .expect_err("creating .astrolabe must fail rather than panic");
        assert_ne!(err.kind(), io::ErrorKind::WouldBlock);
    }

    /// Child-process holder. When `ASTROLABE_ELECT_CHILD_ROOT` is set this
    /// test *is* the holder: acquire, write a ready file, then sleep until
    /// the parent kills us. The OS must release the lock on exit.
    #[test]
    fn lock_is_exclusive_across_processes_and_releases_on_exit() {
        const FLAG: &str = "ASTROLABE_ELECT_CHILD_ROOT";
        if let Ok(root) = std::env::var(FLAG) {
            let root = PathBuf::from(root);
            let lock = IndexerLock::try_acquire(&root)
                .expect("child try_acquire I/O")
                .expect("child should win the lock");
            assert!(lock.held());
            fs::write(root.join("child.ready"), b"1").expect("ready file");
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }

        let dir = TestDir::new();
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(&exe)
            .arg("--exact")
            .arg("elect::tests::lock_is_exclusive_across_processes_and_releases_on_exit")
            .arg("--nocapture")
            .env(FLAG, dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child lock holder");

        let ready = dir.path().join("child.ready");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not acquire the lock in time");
            }
            match child.try_wait() {
                Ok(Some(status)) => panic!("child exited before ready: {status}"),
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(err) => {
                    let _ = child.kill();
                    panic!("try_wait: {err}");
                }
            }
        }

        let contended = IndexerLock::try_acquire(dir.path()).expect("parent try_acquire I/O");
        assert!(
            contended.is_none(),
            "parent must lose while the child holds the lock"
        );

        child.kill().expect("kill child");
        let _ = child.wait();

        // Process death releases the lock; a short retry absorbs scheduler lag.
        let mut won = None;
        for _ in 0..50 {
            won = IndexerLock::try_acquire(dir.path()).expect("acquire after child exit");
            if won.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            won.is_some(),
            "lock should be free after the child process exits"
        );
    }
}
