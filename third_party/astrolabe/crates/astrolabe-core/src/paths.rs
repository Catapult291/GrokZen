//! Path comparison helpers that treat Windows verbatim (`\\?\`) and
//! non-verbatim forms of the same path as equal.
//!
//! On Windows, [`std::fs::canonicalize`] returns paths with a `\\?\` (or
//! `\\?\UNC\`) prefix. OS file-watch backends and agents often emit the
//! ordinary `C:\…` form. Byte-wise [`Path::starts_with`] / [`Path::strip_prefix`]
//! then fail, which used to mark every watch event as "foreign" and degrade
//! the watcher to polling, and made status text show the raw `\\?\` prefix.
//!
//! Comparison keys also unify `\` / `/` and ASCII-lowercase Windows drive/UNC
//! paths so the same helpers work in unit tests on Unix (where `\` is not a
//! separator) and tolerate drive-letter case differences on Windows.

use std::path::{Path, PathBuf};

/// Strip a leading Windows verbatim prefix (`\\?\` / `\\?\UNC\`) when present.
///
/// Non-Windows paths, and Windows paths that are already non-verbatim, are
/// returned unchanged (as a [`PathBuf`] clone of the input). Safe to call on
/// any platform so unit tests can feed `\\?\C:\…` style strings on Unix.
pub fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share\…` → `\\server\share\…`
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Human-facing path string without a Windows verbatim prefix.
pub fn display_path(path: &Path) -> String {
    without_verbatim_prefix(path).display().to_string()
}

fn looks_windows_path(s: &str) -> bool {
    // `C:/…`, `C:\…`, or UNC `//server/share`
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || s.starts_with("//")
}

/// Stable comparison key: no verbatim prefix, `/` separators, Windows paths
/// lowercased.
pub fn path_norm_key(path: &Path) -> String {
    let s = slashes_preserving_root(&path.to_string_lossy());
    if looks_windows_path(&s) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

/// Component-wise equality after stripping Windows verbatim prefixes.
pub fn paths_eq_normalized(a: &Path, b: &Path) -> bool {
    path_norm_key(a) == path_norm_key(b)
}

/// True when `path` equals `root` or is a component-bounded descendant, after
/// stripping Windows verbatim prefixes on both sides.
pub fn is_under_normalized(root: &Path, path: &Path) -> bool {
    strip_prefix_normalized(root, path).is_some()
}

/// `path` relative to `root` after stripping Windows verbatim prefixes.
///
/// Returns `Some("")` (empty relative) when the paths are equal, `Some(rel)`
/// when `path` is under `root` (relative uses `/` separators), and `None`
/// when it is not a component-bounded prefix match.
pub fn strip_prefix_normalized(root: &Path, path: &Path) -> Option<PathBuf> {
    let root_s = path_norm_key(root);
    let path_s = path_norm_key(path);
    if path_s == root_s {
        return Some(PathBuf::new());
    }
    if root_s.is_empty() {
        return None;
    }
    let prefix = if root_s.ends_with('/') {
        root_s
    } else {
        format!("{root_s}/")
    };
    let rest = path_s.strip_prefix(&prefix)?;
    // Comparison is case-insensitive for Windows roots, but returned relative
    // paths must retain the caller's case for case-sensitive index matching.
    let original = slashes_preserving_root(&path.to_string_lossy());
    Some(PathBuf::from(&original[original.len() - rest.len()..]))
}

/// `/`-separated form of a caller-supplied path: backslashes become `/` and a
/// Windows verbatim prefix (`\\?\` / `\\?\UNC\`) is dropped.
///
/// Leading `./` and `/` are preserved: an absolute path must stay absolute for
/// [`normalize_path_filter_under`] to be able to trim it.
fn slashes_preserving_root(path: &str) -> String {
    let raw = path.replace('\\', "/");
    // `\\?\C:\…` / `\\?\UNC\server\share\…` arrive as `//?/C:/…` /
    // `//?/UNC/server/share/…` once the separators are unified.
    if let Some(rest) = raw.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = raw.strip_prefix("//?/") {
        rest.to_string()
    } else {
        raw
    }
}

/// Drop every leading `/` and `./` (the [`RelPath`] contract).
///
/// [`RelPath`]: crate::types::RelPath
fn strip_leading_dot_slash(s: &str) -> &str {
    let mut rest = s;
    loop {
        let trimmed = rest.trim_start_matches('/');
        match trimmed.strip_prefix("./") {
            Some(next) => rest = next,
            None => return trimmed,
        }
    }
}

/// Repo-relative, `/`-separated form of a caller `path_filter`.
///
/// Index paths are always `/`-separated and repo-relative, but callers (and
/// Windows MCP clients) often spell them natively:
///
/// - `crates\astrolabe-mcp` → `crates/astrolabe-mcp`
/// - `.\crates\astrolabe-mcp` → `crates/astrolabe-mcp`
/// - `C:\proj\crates\astrolabe-mcp` with `root = C:\proj` → `crates/astrolabe-mcp`
///
/// An absolute path outside `root` is only slash-normalised (it cannot be
/// read as repo-relative). Case is preserved: matching stays case-sensitive.
pub fn normalize_path_filter_under(root: &Path, filter: &str) -> String {
    let slashed = slashes_preserving_root(filter.trim());
    // Platform-independent absolute test: unit tests feed `C:\…` on Unix too.
    if slashed.starts_with('/') || looks_windows_path(&slashed) {
        if let Some(rel) = strip_prefix_normalized(root, Path::new(&slashed)) {
            let rel = rel.to_string_lossy();
            return strip_leading_dot_slash(&rel).to_string();
        }
    }
    strip_leading_dot_slash(&slashed).to_string()
}

/// [`normalize_path_filter_under`] without an index root: `\` → `/` plus the
/// leading `./` / `/` strip; absolute paths cannot be made repo-relative.
pub fn normalize_path_filter(filter: &str) -> String {
    strip_leading_dot_slash(&slashes_preserving_root(filter.trim())).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn strips_drive_verbatim_prefix() {
        let verbatim = PathBuf::from(r"\\?\C:\Users\z3437\project\AI-con");
        let plain = PathBuf::from(r"C:\Users\z3437\project\AI-con");
        assert_eq!(
            without_verbatim_prefix(&verbatim).to_string_lossy(),
            plain.to_string_lossy()
        );
        assert!(paths_eq_normalized(&verbatim, &plain));
        assert!(!display_path(&verbatim).contains(r"\\?\"));
    }

    #[test]
    fn strips_unc_verbatim_prefix() {
        let verbatim = PathBuf::from(r"\\?\UNC\server\share\repo");
        let plain = PathBuf::from(r"\\server\share\repo");
        assert!(paths_eq_normalized(&verbatim, &plain));
        assert_eq!(path_norm_key(&verbatim), "//server/share/repo");
    }

    #[test]
    fn under_and_strip_across_prefix_forms() {
        let root = PathBuf::from(r"\\?\C:\proj");
        let child = PathBuf::from(r"C:\proj\src\daemon");
        let file = PathBuf::from(r"C:\proj\src\daemon\main.go");
        let outside = PathBuf::from(r"C:\other\x");
        let sibling = PathBuf::from(r"C:\projectile\x");

        assert!(is_under_normalized(&root, &child));
        assert!(is_under_normalized(&root, &root));
        assert!(!is_under_normalized(&root, &outside));
        assert!(
            !is_under_normalized(&root, &sibling),
            "must be component-bounded"
        );

        let rel = strip_prefix_normalized(&root, &child).expect("under");
        assert_eq!(rel.to_string_lossy(), "src/daemon");

        let empty = strip_prefix_normalized(&root, &root).expect("eq");
        assert!(empty.as_os_str().is_empty());

        assert_eq!(
            strip_prefix_normalized(&root, &file)
                .unwrap()
                .to_string_lossy(),
            "src/daemon/main.go"
        );
        assert!(strip_prefix_normalized(&root, &outside).is_none());
    }

    #[test]
    fn windows_relative_tail_preserves_index_path_case() {
        for (root, child) in [
            (r"C:\repo", r"\\?\C:\repo\Src\Daemon"),
            (r"\\?\C:\repo", "c:/REPO/Src/Daemon"),
            ("//?/C:/repo", r"C:\repo\Src\Daemon"),
            (
                r"\\?\UNC\server\share\repo",
                r"\\SERVER\SHARE\repo\Src\Daemon",
            ),
            (
                "//?/UNC/server/share/repo",
                "//server/share/repo/Src/Daemon",
            ),
        ] {
            assert_eq!(
                strip_prefix_normalized(Path::new(root), Path::new(child)).unwrap(),
                PathBuf::from("Src/Daemon")
            );
        }
        assert!(strip_prefix_normalized(Path::new("/repo"), Path::new("/Repo/Src")).is_none());
    }

    #[test]
    fn unix_paths_unchanged() {
        let p = PathBuf::from("/Users/zhoulei/tmp/Astrolabe");
        assert_eq!(without_verbatim_prefix(&p), p);
        assert!(paths_eq_normalized(&p, &p));
        assert!(is_under_normalized(&p, &p.join("crates")));
        assert_eq!(
            strip_prefix_normalized(&p, &p.join("crates/x"))
                .unwrap()
                .to_string_lossy(),
            "crates/x"
        );
    }

    #[test]
    fn normalize_path_filter_unifies_windows_separators() {
        // The defect repro: an index path is `crates/astrolabe-mcp/src/…`, so
        // a backslash filter must match after normalization.
        assert_eq!(
            normalize_path_filter(r"crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        assert_eq!(
            normalize_path_filter(r".\crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        assert_eq!(
            normalize_path_filter(r"crates\astrolabe-mcp\src\server.rs"),
            "crates/astrolabe-mcp/src/server.rs"
        );
        // Forward-slash filters keep working unchanged.
        assert_eq!(
            normalize_path_filter("crates/astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        // Leading `/` and `./` are stripped like `RelPath::new` does; case is
        // preserved (matching stays case-sensitive).
        assert_eq!(normalize_path_filter("/src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("./src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("././src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("Src/Lib.RS"), "Src/Lib.RS");
        // Surrounding whitespace is not part of the path.
        assert_eq!(normalize_path_filter("  src\\lib.rs  "), "src/lib.rs");
        assert_eq!(normalize_path_filter(""), "");
    }

    #[test]
    fn normalize_path_filter_trims_absolute_paths_under_root() {
        let root = PathBuf::from(r"C:\proj");
        assert_eq!(
            normalize_path_filter_under(&root, r"C:\proj\crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        // Verbatim root / caller form, and a differing separators mix.
        assert_eq!(
            normalize_path_filter_under(&root, r"\\?\C:\proj\crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        assert_eq!(
            normalize_path_filter_under(&PathBuf::from(r"\\?\C:\proj"), r"C:\proj\src\main.rs"),
            "src/main.rs"
        );
        // The caller's relative spelling (case) survives the trimming.
        assert_eq!(
            normalize_path_filter_under(&root, r"C:\Proj\Crates\Astrolabe-Mcp"),
            "Crates/Astrolabe-Mcp"
        );
        // Relative filters are only slash-normalized.
        assert_eq!(
            normalize_path_filter_under(&root, r"crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        // Absolute but outside the root: cannot be repo-relative, so only the
        // separators change (the caller gets the usual "matches no path" hint).
        assert_eq!(
            normalize_path_filter_under(&root, r"D:\other\repo"),
            "D:/other/repo"
        );
        assert_eq!(
            normalize_path_filter_under(&root, r"C:\projectile\x"),
            "C:/projectile/x"
        );
    }

    #[test]
    fn normalize_path_filter_under_unix_root() {
        let root = PathBuf::from("/Users/zhoulei/tmp/Astrolabe");
        assert_eq!(
            normalize_path_filter_under(&root, "/Users/zhoulei/tmp/Astrolabe/crates/x"),
            "crates/x"
        );
        assert_eq!(normalize_path_filter_under(&root, "crates/x"), "crates/x");
        // Same path as the root: empty filter (whole repo).
        assert_eq!(
            normalize_path_filter_under(&root, "/Users/zhoulei/tmp/Astrolabe"),
            ""
        );
    }
}
