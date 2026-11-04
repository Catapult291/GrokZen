pub(crate) mod lsp_runtime;

pub(crate) const TEST_MODEL: &str = "test-model";

/// Permission bits (`mode & 0o777`) of `path`, for owner-only assertions.
#[cfg(unix)]
pub(crate) fn unix_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Set `path`'s permission bits, e.g. to simulate umask-default dirs.
#[cfg(unix)]
pub(crate) fn set_unix_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Keep this crate's unit-test binary from writing synthetic events into the real unified log.
/// Runs pre-main so the redirect beats the lazily-opened writer.
/// Integration binaries under `tests/` isolate via `TestSandbox` homes instead.
#[ctor::ctor]
fn redirect_unified_log_for_tests() {
    xai_grok_telemetry::unified_log::redirect_to_temp_for_tests();
}

/// Keep this crate's unit-test binary off the real `~/.grok` unless the caller already chose a
/// home. Tests that dispatch real tools share session, persistence, hook, and telemetry state
/// with any live Grok Build instance on the same home; contention there can block a test
/// indefinitely. Runs pre-main so the value lands before `grok_home()`'s process-wide `OnceLock`
/// is first read.
#[ctor::ctor]
fn isolate_grok_home_for_tests() {
    if std::env::var_os("GROK_HOME").is_some() {
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let home = std::env::temp_dir().join(format!(
        "grok-shell-test-home-{}-{nanos}",
        std::process::id()
    ));
    if std::fs::create_dir_all(&home).is_err() {
        return;
    }
    // SAFETY: constructors run before libtest starts its worker threads.
    unsafe { std::env::set_var("GROK_HOME", &home) };
}

#[cfg(test)]
mod tests {
    /// Guards the ctor above: if it stops running, unit tests silently go back to sharing the
    /// developer's live `~/.grok`, where a tool-dispatching test can block indefinitely.
    #[test]
    fn unit_test_binary_uses_a_private_grok_home() {
        assert_ne!(
            xai_grok_config::grok_home(),
            xai_grok_config::default_grok_home(),
            "unit tests are sharing the real ~/.grok; set GROK_HOME for this test run"
        );
    }
}

/// Prepend the hermetic git binary (via `GIT_BIN_PATH`) to `PATH`.
/// `Command::new("git")` in test helpers then resolves to the Bazel-provided static binary instead of system-installed git.
/// Safe to call multiple times; only the first call mutates `PATH`.
pub(crate) fn ensure_hermetic_git_on_path() {
    use std::path::PathBuf;
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Ok(git_bin) = std::env::var("GIT_BIN_PATH") {
            let p = PathBuf::from(&git_bin);
            let p = if p.is_relative() {
                std::env::current_dir().unwrap().join(&p)
            } else {
                p
            };
            if let Some(dir) = p.parent() {
                let cur = std::env::var("PATH").unwrap_or_default();
                unsafe {
                    std::env::set_var("PATH", format!("{}:{}", dir.display(), cur));
                    // git-minimal spawns subcommands (`git stash` invokes `git update-index`) through its exec path That path is baked to a build-machine prefix Helpers live next to the binary, so point the exec path there
                    // Skip the host-fallback wrapper: host git must keep its own exec path
                    if p.file_name().is_some_and(|name| name == "git") {
                        std::env::set_var("GIT_EXEC_PATH", dir);
                    }
                }
            }
        }
    });
}
