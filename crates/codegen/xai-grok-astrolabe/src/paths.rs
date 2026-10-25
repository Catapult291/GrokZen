//! Path mapping between the engine's index root, the session working
//! directory, and the path the model is allowed to see.
//!
//! Three bases (see the plan, §7.2):
//! - `R` (root): the index root, a git root when there is one;
//! - `C` (cwd): the session working directory that `read_file`/`grep` use;
//! - `D` (display): what the model sees when it differs from `C` (forked
//!   sessions and worktrees).
//!
//! The engine speaks repo-relative (`R`-relative) paths. The model speaks `C`-
//! relative paths (or `D`-prefixed absolute ones). Every query goes in through
//! [`PathMap::engine_rel`] and every result comes out through
//! [`PathMap::rewrite`].

use std::path::{Component, Path, PathBuf};

/// Failure to map a model-supplied path onto the engine root.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PathError {
    #[error(
        "`{0}` is outside the indexed workspace root ({1}); use grep for paths outside the \
         workspace"
    )]
    OutsideRoot(String, String),
    #[error("empty path")]
    Empty,
}

/// One `(root, cwd, display)` mapping.
#[derive(Debug, Clone)]
pub struct PathMap {
    root: PathBuf,
    cwd: PathBuf,
    display: Option<PathBuf>,
}

impl PathMap {
    pub fn new(
        root: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        display: Option<PathBuf>,
    ) -> Self {
        let root = canonical_or_normalize(&root.into());
        let cwd = canonical_or_normalize(&cwd.into());
        Self { root, cwd, display }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Map a model-supplied path (cwd-relative, or absolute under `C`/`D`) to
    /// an absolute path on disk.
    pub fn absolute(&self, input: &str) -> Result<PathBuf, PathError> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        let raw = raw.trim_start_matches("./");
        let candidate = Path::new(raw);
        let abs = if candidate.is_absolute() {
            match &self.display {
                Some(display) if candidate.starts_with(display) => {
                    let suffix = candidate.strip_prefix(display).unwrap_or(Path::new(""));
                    self.cwd.join(suffix)
                }
                _ => candidate.to_path_buf(),
            }
        } else {
            self.cwd.join(candidate)
        };
        Ok(normalize(&abs))
    }

    /// Map a model-supplied path to an `R`-relative posix path.
    pub fn engine_rel(&self, input: &str) -> Result<String, PathError> {
        let abs = self.absolute(input)?;
        match relative_posix(&self.root, &abs) {
            Some(rel) if !escapes(&rel) => Ok(rel),
            _ => Err(PathError::OutsideRoot(
                input.to_string(),
                self.root.display().to_string(),
            )),
        }
    }

    /// Map an `R`-relative engine path to the path the model should see.
    ///
    /// Inside `C`: the `C`-relative path (forward slashes). Elsewhere in `R`:
    /// a `../`-relative path from `C`. When `D` is set, both are expressed
    /// under `D` as absolute posix paths, matching the `read_file` contract.
    pub fn display_path(&self, engine_rel: &str) -> String {
        let rel = engine_rel.trim_start_matches("./").trim_start_matches('/');
        if rel.is_empty() {
            return ".".to_string();
        }
        let abs = self.root.join(rel);
        let from_cwd = relative_posix(&self.cwd, &abs).unwrap_or_else(|| rel.to_string());
        match &self.display {
            Some(display) => posix(&display.join(from_cwd)),
            None => from_cwd,
        }
    }

    /// Rewrite engine output into model-visible text:
    /// - drops the leading `index_root: …` line (the host knows the root);
    /// - rewrites `R`-relative path tokens to `C`-relative (or `D`-absolute);
    /// - drops any line whose path token is denied by policy.
    ///
    /// Lines are metadata (hit anchors, body headers, file lists); source body
    /// lines are never touched, so quoted code stays byte-identical.
    pub fn rewrite(&self, text: &str, deny: &DenyPolicy) -> RewriteOutcome {
        let mut out = String::with_capacity(text.len());
        let mut hidden = 0usize;
        // A denied body header hides the body that follows it: keeping those
        // lines would leak the content the header points at.
        let mut skip_body = false;
        for line in text.lines() {
            if line.starts_with("index_root:") {
                continue;
            }
            if skip_body {
                if is_body_line(line) || line.trim().is_empty() {
                    continue;
                }
                skip_body = false;
            }
            let (rewritten, blocked) = self.rewrite_line(line, deny);
            if blocked {
                hidden += 1;
                if line.starts_with("----- body") {
                    skip_body = true;
                }
                continue;
            }
            out.push_str(&rewritten);
            out.push('\n');
        }
        if text.ends_with('\n') {
            // `lines()` dropped the trailing newline; the push above restored it.
        }
        RewriteOutcome {
            text: out.trim_end_matches('\n').to_string(),
            hidden,
        }
    }

    fn rewrite_line(&self, line: &str, deny: &DenyPolicy) -> (String, bool) {
        // Fast path: a repo-root cwd needs no rewriting for root-relative
        // output, but deny filtering still applies.
        let mut found: Vec<(usize, usize, String, bool)> = Vec::new();
        for token in path_tokens(line) {
            let (rel, blocked) = match self.token_to_rel(&token.text) {
                Some(rel) => {
                    let blocked = deny.is_denied(&rel);
                    (rel, blocked)
                }
                None => continue,
            };
            let shown = self.display_path(&rel);
            found.push((token.start, token.end, shown, blocked));
        }
        if found.is_empty() {
            return (line.to_string(), false);
        }
        let blocked = found.iter().any(|entry| entry.3);
        if blocked {
            return (line.to_string(), true);
        }
        let mut out = String::with_capacity(line.len() + 16);
        let mut cursor = 0usize;
        for (start, end, shown, _) in found {
            out.push_str(line.get(cursor..start).unwrap_or_default());
            out.push_str(&shown);
            cursor = end;
        }
        out.push_str(line.get(cursor..).unwrap_or_default());
        (out, false)
    }

    /// Interpret a path-looking token: `R`-relative already, or an absolute
    /// path under `D`/`C` that needs mapping back onto `R`.
    fn token_to_rel(&self, token: &str) -> Option<String> {
        let token = token.trim_end_matches([':', ')', ',']);
        if token.is_empty() {
            return None;
        }
        if Path::new(token).is_absolute() {
            let abs = self.absolute(token).ok()?;
            return relative_posix(&self.root, &abs);
        }
        let rel = token.trim_start_matches("./").trim_start_matches('/');
        if rel.is_empty() {
            return None;
        }
        Some(rel.to_string())
    }
}

/// Result of rewriting one engine reply.
#[derive(Debug, Clone)]
pub struct RewriteOutcome {
    pub text: String,
    /// Lines dropped because their path is denied by read policy.
    pub hidden: usize,
}

/// Read-deny policy (`DenyReadGlobs`), evaluated on `R`-relative paths.
#[derive(Debug, Clone, Default)]
pub struct DenyPolicy {
    patterns: Vec<String>,
    set: Option<globset::GlobSet>,
}

impl DenyPolicy {
    /// Build the policy for one index root. Absolute patterns under the root
    /// are rewritten to root-relative ones so they can match engine output;
    /// an absolute pattern outside the root can never match and is dropped.
    pub fn for_root(root: &Path, patterns: &[String]) -> Self {
        let mapped: Vec<String> = patterns
            .iter()
            .filter_map(|raw| {
                let path = Path::new(raw.trim());
                if !path.is_absolute() {
                    return Some(raw.clone());
                }
                match relative_posix(root, &normalize(path)) {
                    Some(rel) if !escapes(&rel) => Some(rel),
                    _ => None,
                }
            })
            .collect();
        Self::new(&mapped)
    }

    pub fn new(patterns: &[String]) -> Self {
        let patterns: Vec<String> = patterns
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if patterns.is_empty() {
            return Self::default();
        }
        let mut builder = globset::GlobSetBuilder::new();
        let mut usable = Vec::new();
        for raw in &patterns {
            let base = raw.trim_start_matches("./").trim_end_matches('/');
            if base.is_empty() {
                continue;
            }
            for variant in deny_variants(base) {
                if let Ok(glob) = globset::Glob::new(&variant) {
                    builder.add(glob);
                    usable.push(raw.clone());
                }
            }
        }
        match builder.build() {
            Ok(set) if !usable.is_empty() => Self {
                patterns,
                set: Some(set),
            },
            _ => Self {
                patterns,
                set: None,
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_none()
    }

    /// Deny globs in the shape the engine's scan `extra_excludes` accepts, so
    /// a build never reads a denied file at all. The list is exactly the
    /// variants [`DenyPolicy`] matches with (see `deny_variants`), sorted and
    /// deduplicated: build-time exclusion and query-time filtering agree, and
    /// two sessions with equal policies hash to the same engine key.
    pub fn engine_excludes(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .patterns
            .iter()
            .flat_map(|raw| {
                let base = raw.trim_start_matches("./").trim_end_matches('/');
                if base.is_empty() {
                    Vec::new()
                } else {
                    deny_variants(base)
                }
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn is_denied(&self, engine_rel: &str) -> bool {
        self.set
            .as_ref()
            .is_some_and(|set| set.is_match(engine_rel.trim_start_matches('/')))
    }

    /// Public form of [`Self::is_denied`] for result rows the engine did not
    /// render itself (host language-server locations).
    pub fn denies(&self, engine_rel: &str) -> bool {
        self.is_denied(engine_rel)
    }
}

/// `true` for a relative path that climbs out of its base.
fn escapes(rel: &str) -> bool {
    rel == ".." || rel.starts_with("../") || Path::new(rel).is_absolute()
}

fn deny_variants(base: &str) -> Vec<String> {
    let mut variants = vec![base.to_string()];
    if !base.starts_with("**/") && !base.starts_with('/') {
        variants.push(format!("**/{base}"));
    }
    if !base.contains(['/', '*', '?', '[']) {
        variants.push(format!("**/{base}/**"));
    }
    variants
}

/// A path-like token inside one output line.
struct Token {
    start: usize,
    end: usize,
    text: String,
}

/// Path tokens in the shapes the engine actually renders:
/// - a leading `R`-relative path (`path:12 …`, `path (3)`, `path: has …`);
/// - `@path:12` anchors everywhere in a line;
/// - `· path:12-18` inside body headers.
fn path_tokens(line: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let mut push = |start: usize| {
        let mut end = start;
        while end < bytes.len() {
            match bytes[end] {
                b' ' | b'\t' | b':' | b')' | b',' | b'"' | b'\'' | b'`' | b'|' | b'>' => break,
                _ => end += 1,
            }
        }
        if end > start && line.is_char_boundary(start) && line.is_char_boundary(end) {
            let text = &line[start..end];
            if text.contains('/') || text.contains('.') {
                tokens.push(Token {
                    start,
                    end,
                    text: text.to_string(),
                });
            }
        }
    };
    if looks_like_path_start(line) {
        push(0);
    }
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'@' => {
                let start = i + 1;
                if start < bytes.len() && !bytes[start].is_ascii_whitespace() {
                    push(start);
                }
            }
            // '·' is two bytes (U+00B7); trigger on its first byte.
            0xC2 if bytes.get(i + 1) == Some(&0xB7) => {
                let mut start = i + 2;
                while start < bytes.len() && bytes[start] == b' ' {
                    start += 1;
                }
                if start < bytes.len() {
                    push(start);
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    tokens
}

/// One rendered source line inside a body: `  181 | fn f() {`.
fn is_body_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.len() == line.len() {
        return false;
    }
    let Some((number, _)) = trimmed.split_once('|') else {
        return false;
    };
    !number.trim().is_empty() && number.trim().parse::<u32>().is_ok()
}

fn looks_like_path_start(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.len() == line.len() {
        return false;
    }
    !trimmed.is_empty()
        && !trimmed.starts_with("--")
        && !trimmed.starts_with("L")
        && !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
        && (trimmed.contains('/') || trimmed.contains(".rs:") || trimmed.contains(".py:"))
}

/// Lexical normalisation: drop `.` components and resolve `..` where possible.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if rooted && out.parent().is_none() {
                    // Already at the filesystem root: `..` cannot go higher.
                    continue;
                }
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::RootDir => {
                rooted = true;
                out.push(component.as_os_str());
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `target` expressed relative to `base`, both absolute. Uses `..` segments
/// when `target` is outside `base`. `None` when they share no root.
pub fn relative_posix(base: &Path, target: &Path) -> Option<String> {
    let base_parts = components(&normalize(base));
    let target_parts = components(&normalize(target));
    // A relative base cannot be compared against an absolute target.
    let base_absolute = base.is_absolute();
    if base_absolute != target.is_absolute() {
        return None;
    }
    let mut common = 0usize;
    while common < base_parts.len()
        && common < target_parts.len()
        && same_component(&base_parts[common], &target_parts[common])
    {
        common += 1;
    }
    if common == 0 && base_absolute {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    parts.extend(std::iter::repeat_n(
        "..".to_string(),
        base_parts.len() - common,
    ));
    parts.extend(target_parts[common..].iter().cloned());
    if parts.is_empty() {
        Some(".".to_string())
    } else {
        Some(parts.join("/"))
    }
}

/// Canonicalize when the path exists; otherwise normalise lexically. Both the
/// engine root and the session cwd go through this, so `\\?\` verbatim
/// prefixes cannot make two spellings of one directory look unrelated.
pub fn canonical_or_normalize(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

fn components(path: &Path) -> Vec<String> {
    path.components().map(posix_component).collect()
}

fn same_component(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn posix_component(component: Component<'_>) -> String {
    match component {
        Component::RootDir => "/".to_string(),
        Component::Prefix(prefix) => {
            let raw = prefix.as_os_str().to_string_lossy();
            raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
        }
        Component::CurDir => ".".to_string(),
        Component::ParentDir => "..".to_string(),
        Component::Normal(part) => part.to_string_lossy().to_string(),
    }
}

/// Absolute path → forward-slash string, without the Windows `\\?\` prefix.
pub fn posix(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    stripped.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> PathMap {
        PathMap::new(
            PathBuf::from("/ws"),
            PathBuf::from("/ws/crates/codegen"),
            None,
        )
    }

    #[test]
    fn engine_rel_from_cwd_relative_input() {
        assert_eq!(
            map().engine_rel("xai-grok-tools/src/lib.rs").unwrap(),
            "crates/codegen/xai-grok-tools/src/lib.rs"
        );
    }

    #[test]
    fn engine_rel_rejects_paths_outside_the_root() {
        let err = map().engine_rel("../../../etc/passwd").unwrap_err();
        assert!(matches!(err, PathError::OutsideRoot(..)));
    }

    #[test]
    fn display_path_strips_the_cwd_prefix() {
        assert_eq!(
            map().display_path("crates/codegen/xai-grok-tools/src/lib.rs"),
            "xai-grok-tools/src/lib.rs"
        );
    }

    #[test]
    fn display_path_uses_parent_segments_for_siblings() {
        assert_eq!(
            map().display_path("crates/codegen/xai-grok-shell/src/lib.rs"),
            "xai-grok-shell/src/lib.rs"
        );
        assert_eq!(map().display_path("Cargo.toml"), "../../Cargo.toml");
    }

    #[test]
    fn display_path_prefers_the_display_cwd() {
        let map = PathMap::new(
            PathBuf::from("/real/ws"),
            PathBuf::from("/real/ws/src"),
            Some(PathBuf::from("/shown/ws")),
        );
        assert_eq!(
            map.display_path("src/lib.rs"),
            "/shown/ws/lib.rs".to_string()
        );
    }

    #[test]
    fn rewrite_drops_index_root_and_rewrites_anchors() {
        let outcome = map().rewrite(
            "index_root: \\\\?\\/ws\n1 hit / 1 file\n@crates/codegen/xai-grok-tools/src/lib.rs:12 fn f()\n",
            &DenyPolicy::default(),
        );
        assert!(!outcome.text.contains("index_root"));
        assert!(outcome.text.contains("@xai-grok-tools/src/lib.rs:12"));
        assert_eq!(outcome.hidden, 0);
    }

    #[test]
    fn rewrite_leaves_source_body_lines_alone() {
        let outcome = map().rewrite(
            "  181 | fn f() { let p = \"crates/codegen/x.rs\"; }\n",
            &DenyPolicy::default(),
        );
        assert!(outcome.text.contains("\"crates/codegen/x.rs\""));
    }

    #[test]
    fn rewrite_drops_denied_lines_and_counts_them() {
        let deny = DenyPolicy::new(&["**/.env".to_string()]);
        let outcome = map().rewrite(
            "@crates/codegen/.env:1 SECRET=1\n@crates/codegen/x.rs:2 ok\n",
            &deny,
        );
        assert!(!outcome.text.contains("SECRET"));
        assert!(outcome.text.contains("x.rs:2"));
        assert_eq!(outcome.hidden, 1);
    }

    #[test]
    fn rewrite_hides_a_denied_body_block() {
        let deny = DenyPolicy::new(&["**/.env".to_string()]);
        let text = "----- body @ fn secret()  @crates/app/.env:3 -----\n  3 | SECRET=1\n  4 | MORE=2\n\n@src/lib.rs:1 ok\n";
        let outcome = map().rewrite(text, &deny);
        assert!(!outcome.text.contains("SECRET"), "{}", outcome.text);
        assert!(!outcome.text.contains("MORE"), "{}", outcome.text);
        assert!(outcome.text.contains("src/lib.rs:1"), "{}", outcome.text);
        assert_eq!(outcome.hidden, 1);
    }

    #[test]
    fn deny_policy_maps_absolute_patterns_onto_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let denied = root.join("crates").join("app").join(".env");
        let deny = DenyPolicy::for_root(root, &[denied.display().to_string()]);
        assert!(!deny.is_empty());
        assert!(deny.is_denied("crates/app/.env"));
        // An absolute pattern outside the root cannot match anything.
        let outside = tempfile::tempdir().unwrap();
        let outside_path = outside.path().join("shadow");
        let policy = DenyPolicy::for_root(root, &[outside_path.display().to_string()]);
        assert!(policy.is_empty());
    }

    #[test]
    fn deny_policy_matches_nested_paths() {
        let deny = DenyPolicy::new(&["**/*.pem".to_string()]);
        assert!(!deny.is_empty());
        assert!(deny.is_denied("a/b/key.pem"));
        assert!(!deny.is_denied("a/b/key.pem.md"));
    }

    #[test]
    fn engine_excludes_are_the_query_variants_sorted_and_deduped() {
        let deny = DenyPolicy::new(&[".env".to_string(), "./secrets/".to_string()]);
        let excludes = deny.engine_excludes();
        // Same variants the query-time GlobSet matches with, so build-time
        // exclusion and query-time filtering admit/keep the same set.
        assert_eq!(excludes, vec!["**/.env", "**/.env/**", "**/secrets", "**/secrets/**", ".env", "secrets"]);
        // Equal policies in a different input order share one engine key.
        let other = DenyPolicy::new(&["./secrets/".to_string(), ".env".to_string()]);
        assert_eq!(excludes, other.engine_excludes());
    }

    #[test]
    fn engine_excludes_drop_patterns_that_cannot_match_the_root() {
        // Absolute patterns outside the root never enter `patterns`, so the
        // exclude list — and the engine key — stays empty.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let policy =
            DenyPolicy::for_root(tmp.path(), &[outside.path().join("shadow").display().to_string()]);
        assert!(policy.engine_excludes().is_empty());
    }

    #[test]
    fn windows_verbatim_prefix_is_stripped_from_display_paths() {
        let map = PathMap::new(
            PathBuf::from(r"\\?\C:\ws"),
            PathBuf::from(r"\\?\C:\ws"),
            None,
        );
        assert_eq!(map.display_path("src/lib.rs"), "src/lib.rs");
    }
}
