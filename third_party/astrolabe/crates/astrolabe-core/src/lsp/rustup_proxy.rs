//! `rust-analyzer` found on PATH is often the rustup proxy (`~/.cargo/bin/
//! rust-analyzer`, a hard link or symlink to `rustup`), not the server. The
//! proxy exists even when no toolchain is selected (`no default is
//! configured`) or the selected toolchain lacks the component (`Unknown
//! binary 'rust-analyzer' in official toolchain …`). Discovery used to report
//! such a proxy as Ready and every precise call then failed to start.
//!
//! [`proxy_problem`] recognises a proxy and asks it once (`--version`, with
//! rustup auto-install off, in the project root so `rust-toolchain.toml`
//! applies) whether it can run. A real `rust-analyzer` binary is never
//! spawned here. Answers are cached for [`TTL`] so `get_languages` stays
//! cheap and a `rustup default stable` takes effect without a restart.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one answer is reused.
const TTL: Duration = Duration::from_secs(30);
/// A proxy that resolves answers in milliseconds; on timeout we do not block
/// discovery and let the normal start report what happens.
const TIMEOUT: Duration = Duration::from_secs(5);

type Key = (PathBuf, Option<PathBuf>);
/// When the answer was taken, and the answer.
type Answer = (Instant, Option<String>);
static CACHE: Mutex<Option<HashMap<Key, Answer>>> = Mutex::new(None);

/// `Some(hint)` when `command` is a rustup proxy that cannot run
/// rust-analyzer in `cwd`; `None` for a real binary or a working proxy.
pub(super) fn proxy_problem(command: &Path, cwd: Option<&Path>) -> Option<String> {
    if !is_rustup_proxy(command) {
        return None;
    }
    let key = (command.to_path_buf(), cwd.map(Path::to_path_buf));
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, answer)) = guard.as_ref().and_then(|m| m.get(&key)) {
            if at.elapsed() < TTL {
                return answer.clone();
            }
        }
    }
    let answer = check(command, cwd)
        .err()
        .map(|stderr| hint(command, &stderr));
    if let Ok(mut guard) = CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(key, (Instant::now(), answer.clone()));
    }
    answer
}

/// Symlink to `rustup`, or the same file as a `rustup` next to it (rustup
/// installs its proxies as hard links, or copies where links fail).
fn is_rustup_proxy(command: &Path) -> bool {
    let is_rustup = |p: &Path| {
        p.file_stem().is_some_and(|s| {
            s.eq_ignore_ascii_case("rustup") || s.eq_ignore_ascii_case("rustup-init")
        })
    };
    if let Ok(target) = std::fs::canonicalize(command) {
        if is_rustup(&target) {
            return true;
        }
    }
    let Some(dir) = command.parent() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(command) else {
        return false;
    };
    ["rustup", "rustup.exe"].iter().any(|name| {
        let Ok(other) = std::fs::metadata(dir.join(name)) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if other.ino() == meta.ino() && other.dev() == meta.dev() {
                return true;
            }
        }
        other.is_file() && other.len() == meta.len() && meta.len() > 0
    })
}

/// Run `command --version`; `Err(stderr)` when it exits non-zero.
fn check(command: &Path, cwd: Option<&Path>) -> Result<(), String> {
    let mut cmd = Command::new(command);
    cmd.arg("--version")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    let Ok(mut child) = cmd.spawn() else {
        return Ok(()); // the normal start reports spawn errors
    };
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Err(stderr)
}

/// Install hint naming the proxy, rustup's own first error line and the fix.
fn hint(command: &Path, stderr: &str) -> String {
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("exited with an error")
        .trim_start_matches("error: ")
        .trim_end_matches('.');
    let fix = if stderr.contains("no default is configured") {
        "run `rustup default stable`, then `rustup component add rust-analyzer`"
    } else if stderr.contains("Unknown binary") || stderr.contains("not installed for") {
        "run `rustup component add rust-analyzer` for that toolchain"
    } else if stderr.contains("is not installed") {
        "install the toolchain (`rustup toolchain install` in the project root), then `rustup component add rust-analyzer`"
    } else {
        "run `rustup default stable` and `rustup component add rust-analyzer`"
    };
    format!(
        "rust-analyzer is not installed: `{}` is a rustup proxy that cannot run it here ({first}). To fix, {fix}; or set ASTROLABE_LSP_RUST to a standalone rust-analyzer binary.",
        command.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-rustup-proxy-{tag}-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Fake `rustup` plus a hard-linked `rust-analyzer` proxy.
    pub(crate) fn fake_proxy(dir: &Path, script: &str) -> PathBuf {
        let rustup = dir.join("rustup");
        fs::write(&rustup, script).unwrap();
        fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
        let ra = dir.join("rust-analyzer");
        fs::hard_link(&rustup, &ra).unwrap();
        ra
    }

    #[test]
    fn broken_proxy_is_reported_with_rustups_reason() {
        let dir = scratch("broken");
        let ra = fake_proxy(
            &dir,
            "#!/bin/sh\necho \"error: rustup could not choose a version of rust-analyzer to run, because one wasn't specified explicitly, and no default is configured.\" >&2\nexit 1\n",
        );
        let got = proxy_problem(&ra, None).expect("broken proxy");
        assert!(got.contains("rustup default stable"), "{got}");
        assert!(got.contains("no default is configured"), "{got}");
        assert!(got.contains("not installed"), "{got}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn working_proxy_symlink_and_real_binary_pass() {
        let dir = scratch("ok");
        let ra = fake_proxy(&dir, "#!/bin/sh\necho rust-analyzer 1.0\n");
        assert_eq!(proxy_problem(&ra, None), None);
        // Symlinked proxy to a broken rustup is still recognised.
        let dir2 = scratch("link");
        let rustup = dir2.join("rustup");
        fs::write(&rustup, "#!/bin/sh\necho \"error: Unknown binary 'rust-analyzer' in official toolchain 'x'.\" >&2\nexit 1\n").unwrap();
        fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir2.join("rust-analyzer");
        std::os::unix::fs::symlink("rustup", &link).unwrap();
        let got = proxy_problem(&link, None).expect("symlinked broken proxy");
        assert!(got.contains("rustup component add rust-analyzer"), "{got}");
        // A standalone binary that would fail is never run.
        let dir3 = scratch("real");
        let real = dir3.join("rust-analyzer");
        fs::write(&real, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(proxy_problem(&real, None), None);
        for d in [dir, dir2, dir3] {
            let _ = fs::remove_dir_all(d);
        }
    }
}
