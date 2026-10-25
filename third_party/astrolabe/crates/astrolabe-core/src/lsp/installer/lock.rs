//! Exclusive install lock via an atomic lock-directory create.
//!
//! Multiple Astrolabe processes (or AI agents) may race to install the same
//! server. A directory created with `create_dir` is atomic on local filesystems
//! and needs no extra crate. Stale locks older than `timeout` are broken.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn acquire(lock_path: &Path, timeout: Duration) -> io::Result<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let start = Instant::now();
    loop {
        match fs::create_dir(lock_path) {
            Ok(()) => {
                // Write a stamp so other processes can judge staleness.
                let _ = fs::write(lock_path.join("owner"), format!("{}", std::process::id()));
                return Ok(LockGuard {
                    path: lock_path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if start.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out waiting for install lock {}", lock_path.display()),
                    ));
                }
                if is_stale(lock_path, timeout) {
                    let _ = fs::remove_dir_all(lock_path);
                    continue;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_stale(lock_path: &Path, max_age: Duration) -> bool {
    let Ok(meta) = fs::metadata(lock_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age > max_age
}
