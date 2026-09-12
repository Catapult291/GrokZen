use std::path::Path;
use std::process::Command;

fn git_stdout(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=GROK_VERSION");

    // Watch the git files that change on commit/checkout so the version stamp refreshes
    // Never emit a missing path: cargo treats it as always dirty and rebuilds this crate every build
    let mut watch_paths = Vec::new();
    watch_paths.extend(git_stdout(&["rev-parse", "--git-path", "HEAD"]));
    watch_paths.extend(git_stdout(&["rev-parse", "--git-path", "logs/HEAD"]));
    if let Some(head_ref) = git_stdout(&["symbolic-ref", "-q", "HEAD"]) {
        watch_paths.extend(git_stdout(&["rev-parse", "--git-path", &head_ref]));
    }
    for path in watch_paths.iter().filter(|p| Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={path}");
    }

    let commit = git_stdout(&["rev-parse", "HEAD"])
        .map(|s| s.chars().take(12).collect::<String>())
        .filter(|s| s.len() == 12)
        .unwrap_or_else(|| "unknown".to_string());

    let version = std::env::var("GROK_VERSION")
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| "0.0.0".to_string());

    println!("cargo:rustc-env=VERSION_WITH_COMMIT={version} ({commit})");

    // Windows gives the main thread 1 MB (MSVC) or 2 MB (GNU), less than the unoptimized
    // startup frames of this binary need: a debug build dies with STATUS_STACK_OVERFLOW
    // before printing anything. Reserve the 8 MB the Unix default already provides.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let stack_arg = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            "/STACK:8388608"
        } else {
            "-Wl,--stack,8388608"
        };
        println!("cargo:rustc-link-arg-bins={stack_arg}");
    }
}
