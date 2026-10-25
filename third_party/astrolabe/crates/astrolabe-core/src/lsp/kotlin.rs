//! JetBrains kotlin-lsp: launcher discovery and startup knobs.
//!
//! Since v262.4739 the standalone distribution is launched through
//! `bin/intellij-server`; the old top-level `kotlin-lsp.sh` only prints a
//! deprecation warning and forwards, and is slated for removal. The archives
//! also differ per platform:
//!
//! | source | layout |
//! |---|---|
//! | Linux `.tar.gz` | `kotlin-server-<ver>/bin/intellij-server` |
//! | macOS `.sit` / Homebrew | `libexec/bin/intellij-server`, `bin/kotlin-lsp` symlink |
//! | Windows `.zip` | `kotlin-server-<ver>\bin\intellij-server.exe` |
//! | VS Code / Cursor extension | `jetbrains.kotlin-server-<ver>-<os>/server/bin/intellij-server` |
//!
//! [`launcher_in`] finds the launcher under any of those roots, and
//! [`well_known_roots`] lists where installers put them, so a user who
//! unpacked the archive (or only has the editor extension) is not told to
//! install something they already have.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Launcher file name inside a distribution's `bin/`.
pub const LAUNCHER: &str = "intellij-server";

/// Env var: build-tool importer for the workspace root, passed through as
/// kotlin-lsp's `initializationOptions.buildTools` (`gradle`, `maven`,
/// `json`, `*`; empty string skips project import).
pub const ENV_BUILD_TOOL: &str = "ASTROLABE_KOTLIN_BUILD_TOOL";

/// Paths, relative to a distribution root, where the launcher lives.
const LAYOUTS: &[&str] = &[
    "bin",
    "libexec/bin",
    "server/bin",
    "extension/server/bin",
    "Contents/bin",
    "Contents/libexec/bin",
];

/// Launcher file names, newest first. The legacy wrappers are accepted only
/// when no `intellij-server` exists (pre-262.4739 archives).
fn launcher_names(windows: bool) -> Vec<String> {
    if windows {
        vec![
            format!("{LAUNCHER}.exe"),
            format!("{LAUNCHER}.cmd"),
            format!("{LAUNCHER}.bat"),
        ]
    } else {
        vec![LAUNCHER.to_string()]
    }
}

fn legacy_names(windows: bool) -> &'static [&'static str] {
    if windows {
        &["kotlin-lsp.cmd", "kotlin-lsp.bat"]
    } else {
        &["kotlin-lsp.sh"]
    }
}

fn is_file(path: &Path) -> bool {
    path.metadata().map(|m| m.is_file()).unwrap_or(false)
}

/// Launcher directly under `root` in one of the known [`LAYOUTS`].
fn launcher_at(root: &Path, windows: bool) -> Option<PathBuf> {
    for layout in LAYOUTS {
        for name in launcher_names(windows) {
            let candidate = root.join(layout).join(&name);
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    for name in legacy_names(windows) {
        for dir in [
            root.to_path_buf(),
            root.join("server"),
            root.join("extension/server"),
        ] {
            let candidate = dir.join(name);
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether a child directory of a generic location (`/opt`, an editor's
/// `extensions/`, `/Applications`) looks like a Kotlin server install.
fn looks_like_kotlin_install(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("kotlin-lsp")
        || lower.contains("kotlin-server")
        || lower.contains("kotlin_lsp")
        || lower.starts_with("jetbrains.kotlin")
        || (lower.contains("kotlin") && lower.ends_with(".app"))
}

/// Find the kotlin-lsp launcher under a distribution root or a directory
/// holding versioned distributions (newest name first).
pub fn launcher_in(root: &Path, windows: bool) -> Option<PathBuf> {
    if let Some(found) = launcher_at(root, windows) {
        return Some(found);
    }
    let mut children: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| looks_like_kotlin_install(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    // Version-suffixed names sort lexically close enough to pick the newest
    // (`kotlin-server-263.6379.0` > `kotlin-server-262.9593.0`).
    children.sort();
    children.reverse();
    children
        .iter()
        .find_map(|child| launcher_at(child, windows))
}

/// Prefer `bin/intellij-server` over a deprecated wrapper that sits next to
/// it (`kotlin-lsp.sh`, or a PATH symlink resolving to it). The wrapper
/// prints a deprecation warning on every start and is going away.
pub fn prefer_launcher(command: &Path, windows: bool) -> PathBuf {
    let resolved = std::fs::canonicalize(command).unwrap_or_else(|_| command.to_path_buf());
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.starts_with("kotlin-lsp") {
        if let Some(dir) = resolved.parent() {
            for n in launcher_names(windows) {
                let candidate = dir.join("bin").join(&n);
                if is_file(&candidate) {
                    return candidate;
                }
            }
        }
    }
    command.to_path_buf()
}

/// Whether `command` is JetBrains' server (as opposed to fwcd's
/// kotlin-language-server, which takes neither `--stdio` nor these options).
pub fn is_jetbrains_server(command: &Path) -> bool {
    let resolved = std::fs::canonicalize(command).unwrap_or_else(|_| command.to_path_buf());
    [command, resolved.as_path()].iter().any(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        name.starts_with(LAUNCHER) || name.starts_with("kotlin-lsp")
    })
}

/// Install locations to search when nothing is on `PATH`.
///
/// `home` is `$HOME` / `%USERPROFILE%`; `env` looks up other variables.
pub fn well_known_roots(
    home: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
    windows: bool,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if windows {
        if let Some(local) = env("LOCALAPPDATA") {
            let local = PathBuf::from(local);
            roots.push(local.join("Programs").join("kotlin-lsp"));
            roots.push(local.join("Programs").join("kotlin-server"));
            roots.push(local.join("Programs"));
            roots.push(local.join("JetBrains"));
        }
        if let Some(pf) = env("ProgramFiles") {
            roots.push(PathBuf::from(pf).join("JetBrains"));
        }
    } else {
        if let Some(prefix) = env("HOMEBREW_PREFIX") {
            roots.push(PathBuf::from(prefix).join("opt/kotlin-lsp/libexec"));
        }
        roots.push(PathBuf::from("/opt/homebrew/opt/kotlin-lsp/libexec"));
        roots.push(PathBuf::from("/usr/local/opt/kotlin-lsp/libexec"));
        roots.push(PathBuf::from("/opt/kotlin-lsp"));
        roots.push(PathBuf::from("/opt/kotlin-server"));
        roots.push(PathBuf::from("/opt"));
        roots.push(PathBuf::from("/Applications"));
    }
    if let Some(home) = home {
        for rel in [
            ".local/share/kotlin-lsp",
            ".local/share/kotlin-server",
            ".local/opt",
            "Applications",
            ".vscode/extensions",
            ".vscode-server/extensions",
            ".cursor/extensions",
            ".cursor-server/extensions",
            ".vscode-oss/extensions",
            ".windsurf/extensions",
        ] {
            roots.push(home.join(rel));
        }
    }
    roots
}

/// Env var: override the per-workspace kotlin-lsp cache directory
/// (`--system-path`).
pub const ENV_SYSTEM_PATH: &str = "ASTROLABE_KOTLIN_SYSTEM_PATH";

/// Build files whose change makes an exported workspace model stale.
const BUILD_FILES: &[&str] = &[
    "settings.gradle",
    "settings.gradle.kts",
    "build.gradle",
    "build.gradle.kts",
    "gradle.properties",
    "libs.versions.toml",
    "pom.xml",
];

/// Per-workspace cache directory handed to kotlin-lsp as `--system-path`.
///
/// kotlin-lsp keeps its indexes and the imported workspace model there. A
/// stable per-repo directory (instead of one shared default) keeps two repos
/// from evicting each other and gives Astrolabe a place for the exported
/// workspace model ([`exported_workspace`]).
pub fn system_path(root: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os(ENV_SYSTEM_PATH).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    use sha2::{Digest, Sha256};
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    let name = canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let base = super::installer::default_servers_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("kotlin-lsp")
        .join(format!("{name}-{}", &hex::encode(digest)[..12]))
}

/// Directory kotlin-lsp's `exportWorkspace` command writes `workspace.json` into.
pub fn export_dir(system_path: &Path) -> PathBuf {
    system_path.join("astrolabe-export")
}

/// The exported workspace model, when it exists and is newer than every
/// build file in the repo (depth ≤ 4, skipping hidden and build output).
///
/// Re-importing it with kotlin-lsp's `json` importer takes seconds, where a
/// Gradle sync takes minutes on every start (kotlin-lsp re-runs the sync
/// even when its own cache is warm).
pub fn exported_workspace(root: &Path, system_path: &Path) -> Option<PathBuf> {
    let json = export_dir(system_path).join("workspace.json");
    let exported = json.metadata().ok()?.modified().ok()?;
    let newest = newest_build_file(root, 0)?;
    (newest < exported).then_some(json)
}

fn newest_build_file(dir: &Path, depth: usize) -> Option<std::time::SystemTime> {
    let mut newest = std::time::SystemTime::UNIX_EPOCH;
    let mut any = false;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth >= 4
                || name.starts_with('.')
                || matches!(name.as_ref(), "build" | "out" | "target" | "node_modules")
            {
                continue;
            }
            if let Some(t) = newest_build_file(&entry.path(), depth + 1) {
                newest = newest.max(t);
                any = true;
            }
        } else if BUILD_FILES.contains(&name.as_ref()) {
            if let Ok(t) = entry.metadata().and_then(|m| m.modified()) {
                newest = newest.max(t);
                any = true;
            }
        }
    }
    if any || depth == 0 {
        Some(newest)
    } else {
        None
    }
}

/// `initializationOptions` for JetBrains kotlin-lsp, or `None` for defaults.
///
/// [`ENV_BUILD_TOOL`] wins when set. Otherwise a fresh exported workspace
/// model (see [`exported_workspace`]) is re-imported through the `json`
/// importer with build-tool detection switched off for the root; with no
/// usable export the server auto-detects Gradle / Maven as before.
pub fn initialization_options(
    root: &Path,
    build_tool: Option<String>,
    exported: Option<&Path>,
) -> Option<Value> {
    let uri = super::transport::path_to_file_uri(root);
    if let Some(tool) = build_tool {
        return Some(json!({ "buildTools": { uri: tool.trim() } }));
    }
    let exported = exported?;
    Some(json!({
        "buildTools": { uri: "" },
        "projects": [{
            "type": "json",
            "path": super::transport::path_to_file_uri(exported),
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn scratch() -> Scratch {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-kotlin-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/sh\n").unwrap();
    }

    #[test]
    fn finds_launcher_in_each_platform_layout() {
        for (layout, windows) in [
            ("kotlin-server-263.6379.0/bin/intellij-server", false),
            ("libexec/bin/intellij-server", false),
            (
                "jetbrains.kotlin-server-0.0.13-linux-x64/server/bin/intellij-server",
                false,
            ),
            ("Kotlin Server.app/Contents/bin/intellij-server", false),
            ("kotlin-server-263.6379.0/bin/intellij-server.exe", true),
            ("kotlin-server-263.6379.0/bin/intellij-server.cmd", true),
        ] {
            let s = scratch();
            let expected = s.0.join(layout);
            touch(&expected);
            assert_eq!(launcher_in(&s.0, windows), Some(expected), "{layout}");
        }
    }

    #[test]
    fn newest_versioned_distribution_wins() {
        let s = scratch();
        touch(&s.0.join("kotlin-server-262.9593.0/bin/intellij-server"));
        let newer = s.0.join("kotlin-server-263.6379.0/bin/intellij-server");
        touch(&newer);
        assert_eq!(launcher_in(&s.0, false), Some(newer));
    }

    #[test]
    fn unrelated_children_are_ignored() {
        let s = scratch();
        touch(&s.0.join("idea-IU/bin/intellij-server"));
        assert_eq!(launcher_in(&s.0, false), None);
    }

    #[test]
    fn legacy_wrapper_only_when_no_launcher() {
        let s = scratch();
        let legacy = s.0.join("kotlin-lsp.sh");
        touch(&legacy);
        assert_eq!(launcher_in(&s.0, false), Some(legacy.clone()));
        let launcher = s.0.join("bin/intellij-server");
        touch(&launcher);
        assert_eq!(launcher_in(&s.0, false), Some(launcher.clone()));
        // A PATH entry pointing at the deprecated wrapper is upgraded.
        assert_eq!(
            prefer_launcher(&legacy, false),
            fs::canonicalize(&launcher).unwrap()
        );
    }

    #[test]
    fn jetbrains_server_is_told_apart_from_fwcd() {
        assert!(is_jetbrains_server(Path::new("/x/bin/intellij-server")));
        assert!(is_jetbrains_server(Path::new("/usr/local/bin/kotlin-lsp")));
        assert!(is_jetbrains_server(Path::new(
            "C:/k/bin/intellij-server.exe"
        )));
        assert!(!is_jetbrains_server(Path::new(
            "/x/bin/kotlin-language-server"
        )));
    }

    #[test]
    fn build_tool_knob_maps_to_root_uri() {
        assert_eq!(initialization_options(Path::new("/repo"), None, None), None);
        assert_eq!(
            initialization_options(Path::new("/repo"), Some("gradle".into()), None),
            Some(json!({ "buildTools": { "file:///repo": "gradle" } }))
        );
        assert_eq!(
            initialization_options(
                Path::new("/repo"),
                None,
                Some(Path::new("/c/workspace.json"))
            ),
            Some(json!({
                "buildTools": { "file:///repo": "" },
                "projects": [{ "type": "json", "path": "file:///c/workspace.json" }],
            }))
        );
    }

    #[test]
    fn exported_workspace_goes_stale_when_a_build_file_changes() {
        let repo = scratch();
        let sys = scratch();
        fs::write(repo.0.join("settings.gradle.kts"), "x").unwrap();
        assert_eq!(exported_workspace(&repo.0, &sys.0), None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let json = export_dir(&sys.0).join("workspace.json");
        fs::create_dir_all(json.parent().unwrap()).unwrap();
        fs::write(&json, "{}").unwrap();
        assert_eq!(exported_workspace(&repo.0, &sys.0), Some(json.clone()));
        std::thread::sleep(std::time::Duration::from_millis(20));
        touch(&repo.0.join("lib/build.gradle.kts"));
        assert_eq!(exported_workspace(&repo.0, &sys.0), None);
    }

    #[test]
    fn system_path_is_stable_and_per_root() {
        let a = system_path(Path::new("/nonexistent/a/okhttp"));
        assert_eq!(a, system_path(Path::new("/nonexistent/a/okhttp")));
        assert_ne!(a, system_path(Path::new("/nonexistent/b/okhttp")));
        assert!(a.to_string_lossy().contains("okhttp-"));
    }
}
