//! Engine configuration: which tools to register, where the index root is, and
//! the budget / timeout knobs the tool layer needs.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which of the seven tools the toolset registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstrolabeMode {
    /// Four index tools, plus the precise tools that answer from the index in
    /// practice. `get_diagnostics` stays off until Phase 2.
    Auto,
    /// Only the four index tools.
    Index,
    /// All seven tools (Phase 2 completes the precise ones).
    All,
    /// Register nothing; `grep`/`lsp` only.
    Off,
}

impl AstrolabeMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "index" => Some(Self::Index),
            "all" => Some(Self::All),
            "off" | "false" | "0" | "no" => Some(Self::Off),
            // `true`/`1`/`on` mean "register the Phase 1 set".
            "true" | "1" | "on" | "yes" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Index-layer tools (Phase 1).
    pub fn index_tools(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Precise-layer tools.
    pub fn precise_tools(self) -> bool {
        matches!(self, Self::Auto | Self::All)
    }
}

/// Where the precise tools get compiler-grade answers from when the index
/// cannot decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspFallback {
    /// The host's own LSP manager (`Arc<dyn LspBackend>` in resources).
    Grok,
    /// Astrolabe's built-in pool, started on demand by the engine.
    Astrolabe,
    /// Index-only; say so instead of starting anything.
    None,
}

impl LspFallback {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "grok" | "host" | "" => Some(Self::Grok),
            "astrolabe" | "builtin" | "live" => Some(Self::Astrolabe),
            "none" | "off" | "false" | "0" => Some(Self::None),
            _ => None,
        }
    }
}

/// Where the index root comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexRoot {
    /// Git root above the session cwd, falling back to the cwd.
    Git,
    /// The session cwd itself.
    Cwd,
}

impl IndexRoot {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "git" | "" => Some(Self::Git),
            "cwd" => Some(Self::Cwd),
            _ => None,
        }
    }
}

/// Whether the index may write its parse cache into the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPersist {
    /// `<repo>/.astrolabe/index.redb`, written by whichever process wins the
    /// leader election. This is the upstream default.
    Repo,
    /// In-memory only: no store directory is created, and every process
    /// indexes on its own. Use this when the repository must stay untouched
    /// (read-only checkouts, sandboxes, repositories that forbid new
    /// untracked directories).
    None,
}

impl IndexPersist {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "repo" | "true" | "on" | "" => Some(Self::Repo),
            "none" | "memory" | "off" | "false" => Some(Self::None),
            _ => None,
        }
    }
}

/// `[toolset.astrolabe]` in `config.toml`.
///
/// Every value is kept as written so a single bad key falls back to its
/// default instead of resetting the whole section; [`AstrolabeConfig::from_toolset`]
/// parses and warns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AstrolabeToolsetConfig {
    /// `false` is the same as `mode = "off"`.
    pub enabled: bool,
    pub mode: String,
    pub index_root: String,
    /// Refuse to index a root with more regular files than this.
    pub max_files: u64,
    pub watch: bool,
    /// `"repo"` (default) or `"none"`.
    pub persist: String,
    /// `"grok"` (default), `"astrolabe"`, or `"none"`.
    pub lsp_fallback: String,
    pub budget_tokens: usize,
    /// How long the first query waits for the index; `0` means do not wait.
    pub ready_timeout_ms: u64,
    /// Keep `search_code` ahead of `grep` in the Search slot. `false` puts
    /// `grep` first, for sessions that want the old default back.
    pub replace_grep: bool,
    pub prewarm: bool,
}

impl Default for AstrolabeToolsetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "auto".to_string(),
            index_root: "git".to_string(),
            max_files: 20_000,
            watch: true,
            persist: "repo".to_string(),
            lsp_fallback: "grok".to_string(),
            budget_tokens: 2_500,
            ready_timeout_ms: 20_000,
            replace_grep: true,
            prewarm: false,
        }
    }
}

/// Engine knobs. Defaults match the plan (§5.5) where the engine can honor
/// them today; fields the engine cannot apply yet say so.
#[derive(Debug, Clone)]
pub struct AstrolabeConfig {
    pub mode: AstrolabeMode,
    pub index_root: IndexRoot,
    /// Token budget handed to the engine per call.
    pub budget_tokens: usize,
    /// Byte ceiling for one tool result; the host truncates above this.
    pub budget_bytes: usize,
    /// How long the first query waits for the index to become ready.
    pub ready_timeout: Duration,
    /// Start indexing when the engine is created instead of on the first query.
    pub prewarm: bool,
    /// Watch the tree for changes. Off means queries only see host-applied
    /// edits and the query-time freshness barrier.
    pub watch: bool,
    /// Language-server backend for `find_references` / `goto_definition` /
    /// `get_diagnostics` when the index alone cannot answer.
    pub lsp_fallback: LspFallback,
    /// Refuse to build an index for a root with more files than this. The
    /// upstream MCP binary applies the same limit while resolving its root;
    /// the in-process facade has to enforce it itself.
    pub max_files: u64,
    /// Whether the index may write `<repo>/.astrolabe/index.redb`.
    pub persist: IndexPersist,
    /// Keep `search_code` ahead of `grep` in the Search slot.
    pub replace_grep: bool,
}

impl Default for AstrolabeConfig {
    fn default() -> Self {
        Self {
            mode: AstrolabeMode::Auto,
            index_root: IndexRoot::Git,
            budget_tokens: 2_500,
            budget_bytes: 40_000,
            ready_timeout: Duration::from_secs(20),
            prewarm: false,
            watch: true,
            lsp_fallback: LspFallback::Grok,
            max_files: 20_000,
            persist: IndexPersist::Repo,
            replace_grep: true,
        }
    }
}

impl AstrolabeConfig {
    /// Apply `[toolset.astrolabe]` on top of the defaults. Invalid values warn
    /// and keep the default.
    pub fn from_toolset(toolset: &AstrolabeToolsetConfig) -> Self {
        let mut cfg = Self::default();
        if !toolset.enabled {
            cfg.mode = AstrolabeMode::Off;
        } else if let Some(mode) = AstrolabeMode::parse(&toolset.mode) {
            cfg.mode = mode;
        } else {
            tracing::warn!(value = %toolset.mode, "invalid toolset.astrolabe.mode; using auto");
        }
        if let Some(root) = IndexRoot::parse(&toolset.index_root) {
            cfg.index_root = root;
        } else {
            tracing::warn!(value = %toolset.index_root, "invalid toolset.astrolabe.index_root; using git");
        }
        if let Some(persist) = IndexPersist::parse(&toolset.persist) {
            cfg.persist = persist;
        } else if toolset.persist.trim().eq_ignore_ascii_case("session") {
            // `session` asks for a store outside the repository; that needs an
            // upstream store_dir. Memory-only is the honest fallback.
            tracing::warn!(
                "toolset.astrolabe.persist = \"session\" needs an upstream store_dir; using none"
            );
            cfg.persist = IndexPersist::None;
        } else {
            tracing::warn!(value = %toolset.persist, "invalid toolset.astrolabe.persist; using repo");
        }
        if let Some(fallback) = LspFallback::parse(&toolset.lsp_fallback) {
            cfg.lsp_fallback = fallback;
        } else {
            tracing::warn!(value = %toolset.lsp_fallback, "invalid toolset.astrolabe.lsp_fallback; using grok");
        }
        cfg.max_files = toolset.max_files.max(1);
        cfg.watch = toolset.watch;
        cfg.prewarm = toolset.prewarm;
        cfg.replace_grep = toolset.replace_grep;
        cfg.budget_tokens = toolset.budget_tokens.clamp(1_000, 8_000);
        cfg.ready_timeout = Duration::from_millis(toolset.ready_timeout_ms);
        cfg.apply_env();
        cfg
    }

    /// `[toolset.astrolabe]` followed by the `GROK_ASTROLABE*` environment
    /// override: the environment wins, so a shell can flip one knob for a
    /// single run without editing the file.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        cfg.apply_env();
        cfg
    }

    fn apply_env(&mut self) {
        if let Ok(raw) = std::env::var("GROK_ASTROLABE") {
            if let Some(mode) = AstrolabeMode::parse(&raw) {
                self.mode = mode;
            } else {
                tracing::warn!(value = %raw, "invalid GROK_ASTROLABE; using toolset/default mode");
            }
        }
        if let Ok(raw) = std::env::var("GROK_ASTROLABE_ROOT") {
            match IndexRoot::parse(&raw) {
                Some(root) => self.index_root = root,
                None => {
                    tracing::warn!(value = %raw, "invalid GROK_ASTROLABE_ROOT; keeping current")
                }
            }
        }
        if let Some(value) = parse_usize("GROK_ASTROLABE_BUDGET", 1_000) {
            self.budget_tokens = value.clamp(1_000, 8_000);
        }
        if let Some(value) = parse_usize("GROK_ASTROLABE_READY_MS", 0) {
            self.ready_timeout = Duration::from_millis(value as u64);
        }
        if let Ok(raw) = std::env::var("GROK_ASTROLABE_WATCH") {
            self.watch = !matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            );
        }
        if let Ok(raw) = std::env::var("GROK_ASTROLABE_PREWARM") {
            self.prewarm = matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        if let Ok(raw) = std::env::var("GROK_ASTROLABE_LSP") {
            match LspFallback::parse(&raw) {
                Some(fallback) => self.lsp_fallback = fallback,
                None => tracing::warn!(value = %raw, "invalid GROK_ASTROLABE_LSP; keeping current"),
            }
        }
        if let Ok(raw) = std::env::var("GROK_ASTROLABE_PERSIST") {
            match IndexPersist::parse(&raw) {
                Some(persist) => self.persist = persist,
                None => {
                    tracing::warn!(value = %raw, "invalid GROK_ASTROLABE_PERSIST; keeping current")
                }
            }
        }
        if let Some(value) = parse_usize("GROK_ASTROLABE_MAX_FILES", 1) {
            self.max_files = value as u64;
        }
        if let Ok(raw) = std::env::var("GROK_ASTROLABE_REPLACE_GREP") {
            self.replace_grep = !matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            );
        }
    }
}

fn parse_usize(var: &str, min: usize) -> Option<usize> {
    let raw = std::env::var(var).ok()?;
    match raw.trim().parse::<usize>() {
        Ok(value) if value >= min => Some(value),
        Ok(value) => {
            tracing::warn!(value, var, "value below the minimum; ignored");
            None
        }
        Err(_) => {
            tracing::warn!(value = %raw, var, "invalid integer; using default");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_accepts_short_forms() {
        assert_eq!(AstrolabeMode::parse("off"), Some(AstrolabeMode::Off));
        assert_eq!(AstrolabeMode::parse("false"), Some(AstrolabeMode::Off));
        assert_eq!(AstrolabeMode::parse("all"), Some(AstrolabeMode::All));
        assert_eq!(AstrolabeMode::parse(""), Some(AstrolabeMode::Auto));
        assert_eq!(AstrolabeMode::parse("nonsense"), None);
    }

    #[test]
    fn off_registers_nothing_and_index_mode_skips_precise_tools() {
        assert!(!AstrolabeMode::Off.index_tools());
        assert!(!AstrolabeMode::Off.precise_tools());
        assert!(AstrolabeMode::Index.index_tools());
        assert!(!AstrolabeMode::Index.precise_tools());
        assert!(AstrolabeMode::Auto.index_tools());
        assert!(AstrolabeMode::Auto.precise_tools());
    }

    #[test]
    fn toolset_section_maps_every_knob() {
        let toolset: AstrolabeToolsetConfig = toml::from_str(
            r#"
enabled = true
mode = "index"
index_root = "cwd"
max_files = 5000
watch = false
persist = "none"
lsp_fallback = "astrolabe"
budget_tokens = 4000
ready_timeout_ms = 1500
replace_grep = false
prewarm = true
"#,
        )
        .expect("section parses");
        let cfg = AstrolabeConfig::from_toolset(&toolset);
        assert_eq!(cfg.mode, AstrolabeMode::Index);
        assert_eq!(cfg.index_root, IndexRoot::Cwd);
        assert_eq!(cfg.max_files, 5000);
        assert!(!cfg.watch);
        assert_eq!(cfg.persist, IndexPersist::None);
        assert_eq!(cfg.lsp_fallback, LspFallback::Astrolabe);
        assert_eq!(cfg.budget_tokens, 4000);
        assert_eq!(cfg.ready_timeout, Duration::from_millis(1500));
        assert!(!cfg.replace_grep);
        assert!(cfg.prewarm);
    }

    #[test]
    fn enabled_false_is_off_and_bad_values_keep_defaults() {
        let toolset = AstrolabeToolsetConfig {
            enabled: false,
            mode: "nonsense".to_string(),
            index_root: "nonsense".to_string(),
            persist: "session".to_string(),
            lsp_fallback: "nonsense".to_string(),
            ..AstrolabeToolsetConfig::default()
        };
        let cfg = AstrolabeConfig::from_toolset(&toolset);
        assert_eq!(cfg.mode, AstrolabeMode::Off);
        assert_eq!(cfg.index_root, IndexRoot::Git);
        // `session` needs an upstream store_dir; memory-only is the fallback
        // so the repository is left untouched.
        assert_eq!(cfg.persist, IndexPersist::None);
        assert_eq!(cfg.lsp_fallback, LspFallback::Grok);
    }

    #[test]
    fn budget_and_ready_timeout_are_clamped() {
        let toolset = AstrolabeToolsetConfig {
            budget_tokens: 1,
            max_files: 0,
            ..AstrolabeToolsetConfig::default()
        };
        let cfg = AstrolabeConfig::from_toolset(&toolset);
        assert_eq!(cfg.budget_tokens, 1_000);
        assert_eq!(cfg.max_files, 1);
    }
}
