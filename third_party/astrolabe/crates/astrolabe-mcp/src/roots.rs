//! Multi-project root dispatcher: routing, residency, and idle eviction.
//!
//! This module is **decision-only**. It never starts or stops a file watcher,
//! takes or drops an indexer leader lock, or talks to a language server. Wave-2
//! integration injects those callbacks and must run them for every path
//! returned by [`RootDispatcher::activate`] or [`RootDispatcher::evict_idle`].
//!
//! Evicting a root is the bite-point with cross-process election: dropping
//! residency means yielding the leader lock so another Astrolabe process can
//! take over that child repo. Store and lock files live *inside* the child, so
//! the parent directory is left untouched.
//!
//! The five-minute default idle is a trade-off. Because the index store sits
//! in the child repo, a later `activate` reuses a warm on-disk cache and
//! typically returns in seconds rather than a full rebuild. Keeping many roots
//! resident would pin watchers, locks, and LSP processes; [`DEFAULT_RESIDENT_ROOTS`]
//! bounds that.
//!
//! Environment overrides (`ASTROLABE_IDLE_EVICT_SECS`,
//! `ASTROLABE_RESIDENT_ROOTS`) are parsed by the integrator, not here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default idle-eviction timeout in seconds (5 minutes).
///
/// Integration may override this with `ASTROLABE_IDLE_EVICT_SECS`.
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// Default cap on simultaneously resident roots.
///
/// Integration may override this with `ASTROLABE_RESIDENT_ROOTS`.
pub const DEFAULT_RESIDENT_ROOTS: usize = 3;

/// Idle timeout and residency cap for [`RootDispatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatcherConfig {
    /// How long a resident root may sit unused before [`RootDispatcher::evict_idle`]
    /// yields it. Default: [`DEFAULT_IDLE_SECS`].
    pub idle: Duration,
    /// Maximum number of roots kept resident at once. Default:
    /// [`DEFAULT_RESIDENT_ROOTS`].
    pub resident_roots: usize,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(DEFAULT_IDLE_SECS),
            resident_roots: DEFAULT_RESIDENT_ROOTS,
        }
    }
}

/// Multi-project root dispatcher: which sub-repo serves a query, which stay
/// resident, which get evicted for idleness.
///
/// `children` is the discovered set (typically from WP-C's `MultiProject`).
/// The resident set is a parallel LRU: index 0 is the next capacity victim.
#[derive(Debug, Clone)]
pub struct RootDispatcher {
    children: Vec<PathBuf>,
    /// Resident roots, least-recently-used first.
    resident: Vec<PathBuf>,
    /// `last_used` aligned with [`Self::resident`].
    last_used: Vec<Instant>,
    cfg: DispatcherConfig,
}

impl RootDispatcher {
    /// Build a dispatcher over `children`. Duplicate paths are kept as given;
    /// the caller is expected to pass unique sub-repos.
    pub fn new(children: Vec<PathBuf>, cfg: DispatcherConfig) -> Self {
        Self {
            children,
            resident: Vec::new(),
            last_used: Vec::new(),
            cfg,
        }
    }

    pub fn children(&self) -> &[PathBuf] {
        &self.children
    }

    /// Pick a child root from a query hint (task text or a path prefix).
    ///
    /// Exact path-prefix matches win. Nested prefixes collapse to the longest
    /// (most specific) child; incomparable prefixes are ambiguous and return
    /// `None`. If nothing prefixes, the child's last path component is matched
    /// case-insensitively on a word boundary. Multiple name hits return `None`
    /// so the caller can disambiguate.
    pub fn route(&self, hint: &str) -> Option<&Path> {
        let hint = hint.trim();
        if hint.is_empty() || self.children.is_empty() {
            return None;
        }

        let prefix_hits: Vec<&PathBuf> = self
            .children
            .iter()
            .filter(|child| child_is_path_prefix(child, hint))
            .collect();
        if !prefix_hits.is_empty() {
            return pick_unique_or_nested(&prefix_hits);
        }

        let name_hits: Vec<&PathBuf> = self
            .children
            .iter()
            .filter(|child| child_name_matches(child, hint))
            .collect();
        if name_hits.len() == 1 {
            Some(name_hits[0].as_path())
        } else {
            None
        }
    }

    /// Explicit `root` argument: an absolute path at or under a child, a child
    /// directory name (exact, then case-insensitive when unique), or a
    /// relative path whose first component is a child name.
    pub fn route_explicit(&self, root: &str) -> Option<&Path> {
        let root = root.trim();
        if root.is_empty() {
            return None;
        }
        if let Some((child, _)) = self.route_path(root) {
            return Some(child);
        }
        let name = root.trim_matches(|c| c == '/' || c == '\\');
        let exact: Vec<&PathBuf> = self
            .children
            .iter()
            .filter(|c| c.file_name().and_then(|s| s.to_str()) == Some(name))
            .collect();
        if exact.len() == 1 {
            return Some(exact[0].as_path());
        }
        let lower = name.to_lowercase();
        let loose: Vec<&PathBuf> = self
            .children
            .iter()
            .filter(|c| {
                c.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.to_lowercase() == lower)
            })
            .collect();
        if loose.len() == 1 {
            Some(loose[0].as_path())
        } else {
            None
        }
    }

    /// Route a file path: an absolute path under a child, or a relative path
    /// whose first component is a child's directory name. Returns the child
    /// and the path relative to it (empty for the child itself).
    pub fn route_path(&self, path: &str) -> Option<(&Path, String)> {
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        let p = Path::new(path);
        if p.is_absolute() {
            let hits: Vec<&PathBuf> = self.children.iter().filter(|c| p.starts_with(c)).collect();
            let child = pick_unique_or_nested(&hits)?;
            let rel = p.strip_prefix(child).ok()?;
            return Some((child, rel.to_string_lossy().replace('\\', "/")));
        }
        let norm = path.replace('\\', "/");
        let norm = norm.trim_start_matches("./");
        let (first, rest) = match norm.split_once('/') {
            Some((a, b)) => (a, b),
            None => (norm, ""),
        };
        let hits: Vec<&PathBuf> = self
            .children
            .iter()
            .filter(|c| c.file_name().and_then(|s| s.to_str()) == Some(first))
            .collect();
        if hits.len() == 1 {
            Some((hits[0].as_path(), rest.to_string()))
        } else {
            None
        }
    }

    /// The only resident root, when exactly one is resident.
    pub fn sole_resident(&self) -> Option<&Path> {
        match self.resident.as_slice() {
            [only] => Some(only.as_path()),
            _ => None,
        }
    }

    /// Touch `root`: insert it into the resident set and refresh `last_used`.
    ///
    /// If the set would exceed [`DispatcherConfig::resident_roots`], the
    /// least-recently-used root(s) are removed and returned. The caller must
    /// stop their watcher, yield the leader lock, and recycle any LSP for each
    /// returned path. Unknown roots (not in [`Self::children`]) are ignored.
    #[must_use = "evicted roots need watcher/lock/LSP teardown"]
    pub fn activate(&mut self, root: &Path, now: Instant) -> Vec<PathBuf> {
        let Some(path) = self
            .children
            .iter()
            .find(|child| paths_eq(child, root))
            .cloned()
        else {
            return Vec::new();
        };

        if let Some(pos) = self.resident.iter().position(|p| paths_eq(p, &path)) {
            self.resident.remove(pos);
            self.last_used.remove(pos);
        }
        self.resident.push(path);
        self.last_used.push(now);

        let mut evicted = Vec::new();
        while self.resident.len() > self.cfg.resident_roots {
            evicted.push(self.resident.remove(0));
            self.last_used.remove(0);
        }
        evicted
    }

    /// Drop resident roots whose `last_used` is at least `cfg.idle` ago.
    ///
    /// Returned paths need the same teardown as [`Self::activate`] victims.
    /// Roots still inside the idle window stay resident.
    #[must_use = "evicted roots need watcher/lock/LSP teardown"]
    pub fn evict_idle(&mut self, now: Instant) -> Vec<PathBuf> {
        let idle = self.cfg.idle;
        let mut keep_paths = Vec::new();
        let mut keep_times = Vec::new();
        let mut evicted = Vec::new();
        for (path, used) in self.resident.iter().zip(self.last_used.iter()) {
            if now.saturating_duration_since(*used) >= idle {
                evicted.push(path.clone());
            } else {
                keep_paths.push(path.clone());
                keep_times.push(*used);
            }
        }
        self.resident = keep_paths;
        self.last_used = keep_times;
        evicted
    }

    /// Currently resident roots, least-recently-used first.
    pub fn resident(&self) -> &[PathBuf] {
        &self.resident
    }
}

fn paths_eq(a: &Path, b: &Path) -> bool {
    a == b || a.components().eq(b.components())
}

/// True when `child` is a component-bounded path prefix of `hint`.
///
/// `/proj/foo` matches `/proj/foo/src/lib.rs` and a mention of that path
/// inside task text; it does not match `/proj/foobar`.
fn child_is_path_prefix(child: &Path, hint: &str) -> bool {
    if child.as_os_str().is_empty() {
        return false;
    }
    if Path::new(hint).starts_with(child) {
        return true;
    }
    let child_s = child.to_string_lossy();
    for (pos, _) in hint.match_indices(child_s.as_ref()) {
        let before_ok = pos == 0 || {
            hint[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| !is_path_token_char(c))
        };
        let after_pos = pos + child_s.len();
        let after_ok = after_pos == hint.len() || {
            hint[after_pos..]
                .chars()
                .next()
                .is_some_and(|c| is_path_separator(c) || !is_path_token_char(c))
        };
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn is_path_separator(c: char) -> bool {
    c == '/' || c == '\\'
}

fn is_path_token_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '~')
}

fn child_name_matches(child: &Path, hint: &str) -> bool {
    let Some(name) = child.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if is_generic_dir_name(name) {
        // Common words ("api", "web", "docs") appear in ordinary prose; only
        // trust them when written like a path or code token.
        return mentions_as_token(hint, name);
    }
    contains_word(hint, name)
}

/// Directory names that are ordinary English/engineering words.
fn is_generic_dir_name(name: &str) -> bool {
    const GENERIC: &[&str] = &[
        "api", "web", "app", "apps", "core", "docs", "doc", "server", "client", "lib", "libs",
        "src", "test", "tests", "common", "utils", "util", "shared", "ui", "cli", "tools", "tool",
        "build", "data", "main", "base", "service", "services", "backend", "frontend", "site",
        "admin", "sdk", "examples", "example", "scripts", "infra", "config", "model", "models",
    ];
    GENERIC.contains(&name.to_lowercase().as_str())
}

/// `name/`, `` `name` ``, or `./name` in the hint (case-sensitive).
fn mentions_as_token(hint: &str, name: &str) -> bool {
    hint.contains(&format!("{name}/"))
        || hint.contains(&format!("`{name}`"))
        || hint.contains(&format!("./{name}"))
}

/// Case-insensitive word-boundary search.
///
/// Word characters are alphanumeric, `_`, and `-`, so a repo named `newapi`
/// does not match `newapi2`, and `foo` does not match `foo-bar`.
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let needle: Vec<char> = word.to_lowercase().chars().collect();
    if needle.len() > hay.len() {
        return false;
    }
    for i in 0..=hay.len() - needle.len() {
        if hay[i..i + needle.len()] != needle[..] {
            continue;
        }
        let before_ok = i == 0 || !is_word_char(hay[i - 1]);
        let after = i + needle.len();
        let after_ok = after == hay.len() || !is_word_char(hay[after]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// One hit, or a nesting chain (longest / most specific wins). Incomparable
/// hits are ambiguous.
fn pick_unique_or_nested<'a>(hits: &[&'a PathBuf]) -> Option<&'a Path> {
    match hits.len() {
        0 => None,
        1 => Some(hits[0].as_path()),
        _ => {
            let mut sorted: Vec<&'a PathBuf> = hits.to_vec();
            sorted.sort_by_key(|p| p.components().count());
            let nested = sorted.windows(2).all(|w| w[1].starts_with(w[0].as_path()));
            if nested {
                sorted.last().map(|p| p.as_path())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(paths: &[&str], idle_secs: u64, cap: usize) -> RootDispatcher {
        RootDispatcher::new(
            paths.iter().map(PathBuf::from).collect(),
            DispatcherConfig {
                idle: Duration::from_secs(idle_secs),
                resident_roots: cap,
            },
        )
    }

    fn sample() -> RootDispatcher {
        disp(
            &[
                "/Users/me/Project/newapi",
                "/Users/me/Project/newapi2",
                "/Users/me/Project/astrolabe",
            ],
            300,
            3,
        )
    }

    #[test]
    fn default_constants() {
        let cfg = DispatcherConfig::default();
        assert_eq!(cfg.idle, Duration::from_secs(300));
        assert_eq!(cfg.resident_roots, 3);
        assert_eq!(DEFAULT_IDLE_SECS, 300);
        assert_eq!(DEFAULT_RESIDENT_ROOTS, 3);
    }

    #[test]
    fn route_path_prefix_hit() {
        let d = sample();
        assert_eq!(
            d.route("/Users/me/Project/newapi/src/lib.rs"),
            Some(Path::new("/Users/me/Project/newapi"))
        );
        assert_eq!(
            d.route("please look at /Users/me/Project/newapi2/crates/foo.rs thanks"),
            Some(Path::new("/Users/me/Project/newapi2"))
        );
        // `/newapi` is not a prefix of `/newapi2`.
        assert_eq!(
            d.route("/Users/me/Project/newapi2/src/main.rs"),
            Some(Path::new("/Users/me/Project/newapi2"))
        );
    }

    #[test]
    fn route_nested_prefix_picks_longest() {
        let d = disp(&["/proj/mono", "/proj/mono/nested"], 300, 3);
        assert_eq!(
            d.route("/proj/mono/nested/src/lib.rs"),
            Some(Path::new("/proj/mono/nested"))
        );
        assert_eq!(
            d.route("/proj/mono/src/lib.rs"),
            Some(Path::new("/proj/mono"))
        );
    }

    #[test]
    fn route_child_name_hit() {
        let d = sample();
        assert_eq!(
            d.route("fix the bug in newapi"),
            Some(Path::new("/Users/me/Project/newapi"))
        );
        assert_eq!(
            d.route("newapi2"),
            Some(Path::new("/Users/me/Project/newapi2"))
        );
    }

    #[test]
    fn route_name_case_insensitive() {
        let d = sample();
        assert_eq!(
            d.route("please review NEWAPI"),
            Some(Path::new("/Users/me/Project/newapi"))
        );
        assert_eq!(
            d.route("Astrolabe crate"),
            Some(Path::new("/Users/me/Project/astrolabe"))
        );
    }

    #[test]
    fn route_word_boundary_newapi_does_not_match_newapi2() {
        let d = sample();
        // Hint "newapi" must not select the child named newapi2.
        assert_eq!(
            d.route("newapi"),
            Some(Path::new("/Users/me/Project/newapi"))
        );
        assert_ne!(
            d.route("newapi"),
            Some(Path::new("/Users/me/Project/newapi2"))
        );
        // Hint "newapi2" must not select the child named newapi.
        assert_eq!(
            d.route("touch newapi2 please"),
            Some(Path::new("/Users/me/Project/newapi2"))
        );
        // Hyphenated names stay atomic: "foo" does not match "foo-bar".
        let hyphen = disp(&["/proj/foo", "/proj/foo-bar"], 300, 3);
        assert_eq!(hyphen.route("foo-bar"), Some(Path::new("/proj/foo-bar")));
        assert_eq!(hyphen.route("foo"), Some(Path::new("/proj/foo")));
    }

    #[test]
    fn route_ambiguity_returns_none() {
        let d = sample();
        assert_eq!(d.route("compare newapi and astrolabe"), None);
        assert_eq!(
            d.route("/Users/me/Project/newapi/a.rs and /Users/me/Project/astrolabe/b.rs"),
            None
        );
        assert_eq!(d.route(""), None);
        assert_eq!(d.route("   "), None);
        assert_eq!(d.route("unrelated task text"), None);
    }

    #[test]
    fn activate_lru_evicts_least_recently_used() {
        let mut d = disp(&["/a", "/b", "/c", "/d"], 300, 2);
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t0 + Duration::from_secs(2);
        let t3 = t0 + Duration::from_secs(3);
        let t4 = t0 + Duration::from_secs(4);

        assert!(d.activate(Path::new("/a"), t0).is_empty());
        assert!(d.activate(Path::new("/b"), t1).is_empty());
        assert_eq!(d.resident(), &[PathBuf::from("/a"), PathBuf::from("/b")]);

        let evicted = d.activate(Path::new("/c"), t2);
        assert_eq!(evicted, vec![PathBuf::from("/a")]);
        assert_eq!(d.resident(), &[PathBuf::from("/b"), PathBuf::from("/c")]);

        // Touch /b so it becomes MRU; /c is now LRU.
        assert!(d.activate(Path::new("/b"), t3).is_empty());
        assert_eq!(d.resident(), &[PathBuf::from("/c"), PathBuf::from("/b")]);

        let evicted = d.activate(Path::new("/d"), t4);
        assert_eq!(evicted, vec![PathBuf::from("/c")]);
        assert_eq!(d.resident(), &[PathBuf::from("/b"), PathBuf::from("/d")]);
    }

    #[test]
    fn resident_cap_enforced() {
        let mut d = disp(&["/a", "/b", "/c", "/d"], 300, 3);
        let t0 = Instant::now();
        let _ = d.activate(Path::new("/a"), t0);
        let _ = d.activate(Path::new("/b"), t0 + Duration::from_secs(1));
        let _ = d.activate(Path::new("/c"), t0 + Duration::from_secs(2));
        assert_eq!(d.resident().len(), 3);
        let evicted = d.activate(Path::new("/d"), t0 + Duration::from_secs(3));
        assert_eq!(evicted, vec![PathBuf::from("/a")]);
        assert_eq!(d.resident().len(), 3);
        assert!(!d.resident().iter().any(|p| p == Path::new("/a")));
        assert!(d.resident().iter().any(|p| p == Path::new("/d")));
    }

    #[test]
    fn evict_idle_only_expired() {
        let mut d = disp(&["/a", "/b", "/c"], 60, 3);
        let t0 = Instant::now();
        let _ = d.activate(Path::new("/a"), t0);
        let _ = d.activate(Path::new("/b"), t0 + Duration::from_secs(10));
        let _ = d.activate(Path::new("/c"), t0 + Duration::from_secs(20));

        // At t0+60, /a has been idle 60s (>= idle), /b 50s, /c 40s.
        let evicted = d.evict_idle(t0 + Duration::from_secs(60));
        assert_eq!(evicted, vec![PathBuf::from("/a")]);
        assert_eq!(d.resident(), &[PathBuf::from("/b"), PathBuf::from("/c")]);

        // Still inside the window for /b (idle 55s) and /c (45s).
        let evicted = d.evict_idle(t0 + Duration::from_secs(65));
        assert!(evicted.is_empty());
        assert_eq!(d.resident().len(), 2);

        // /b reaches 60s idle at t0+70; /c is at 50s.
        let evicted = d.evict_idle(t0 + Duration::from_secs(70));
        assert_eq!(evicted, vec![PathBuf::from("/b")]);
        assert_eq!(d.resident(), &[PathBuf::from("/c")]);
    }

    #[test]
    fn empty_children_is_safe() {
        let mut d = RootDispatcher::new(Vec::new(), DispatcherConfig::default());
        let now = Instant::now();
        assert!(d.children().is_empty());
        assert!(d.resident().is_empty());
        assert_eq!(d.route("anything"), None);
        assert_eq!(d.route("/abs/path/src.rs"), None);
        assert!(d.activate(Path::new("/x"), now).is_empty());
        assert!(d.evict_idle(now + Duration::from_secs(10_000)).is_empty());
        assert!(d.resident().is_empty());
    }

    #[test]
    fn activate_unknown_root_is_noop() {
        let mut d = sample();
        let now = Instant::now();
        assert!(d
            .activate(Path::new("/Users/me/Project/missing"), now)
            .is_empty());
        assert!(d.resident().is_empty());
    }

    #[test]
    fn activate_zero_cap_evicts_immediately() {
        let mut d = disp(&["/a", "/b"], 300, 0);
        let now = Instant::now();
        let evicted = d.activate(Path::new("/a"), now);
        assert_eq!(evicted, vec![PathBuf::from("/a")]);
        assert!(d.resident().is_empty());
    }

    #[test]
    fn explicit_root_matches_name_path_and_case() {
        let d = disp(&["/ws/api", "/ws/Web", "/ws/billing"], 300, 3);
        assert_eq!(d.route_explicit("api"), Some(Path::new("/ws/api")));
        assert_eq!(d.route_explicit("web"), Some(Path::new("/ws/Web")));
        assert_eq!(
            d.route_explicit("/ws/billing/src/x.rs"),
            Some(Path::new("/ws/billing"))
        );
        assert_eq!(d.route_explicit("billing/"), Some(Path::new("/ws/billing")));
        assert_eq!(d.route_explicit("nope"), None);
        assert_eq!(d.route_explicit(""), None);
    }

    #[test]
    fn route_path_rewrites_to_child_relative() {
        let d = disp(&["/ws/api", "/ws/web"], 300, 3);
        assert_eq!(
            d.route_path("/ws/web/src/lib.rs"),
            Some((Path::new("/ws/web"), "src/lib.rs".to_string()))
        );
        assert_eq!(
            d.route_path("api/src"),
            Some((Path::new("/ws/api"), "src".to_string()))
        );
        assert_eq!(
            d.route_path("api"),
            Some((Path::new("/ws/api"), String::new()))
        );
        assert_eq!(d.route_path("src/lib.rs"), None);
        assert_eq!(d.route_path("/elsewhere/x"), None);
    }

    #[test]
    fn sole_resident_only_when_exactly_one() {
        let mut d = disp(&["/ws/a", "/ws/b"], 300, 3);
        assert_eq!(d.sole_resident(), None);
        let _ = d.activate(Path::new("/ws/a"), Instant::now());
        assert_eq!(d.sole_resident(), Some(Path::new("/ws/a")));
        let _ = d.activate(Path::new("/ws/b"), Instant::now());
        assert_eq!(d.sole_resident(), None);
    }

    #[test]
    fn generic_directory_names_need_a_path_like_mention() {
        let d = disp(&["/ws/api", "/ws/billing"], 300, 3);
        assert_eq!(d.route("add an api endpoint"), None, "prose word");
        assert_eq!(d.route("look in api/ for it"), Some(Path::new("/ws/api")));
        assert_eq!(d.route("the `api` crate"), Some(Path::new("/ws/api")));
        assert_eq!(
            d.route("fix billing totals"),
            Some(Path::new("/ws/billing")),
            "distinctive names still match by word"
        );
    }
}
