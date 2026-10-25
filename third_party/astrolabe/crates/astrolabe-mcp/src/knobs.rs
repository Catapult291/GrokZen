//! MCP-layer environment knobs.
//!
//! Parsing matches `ASTROLABE_WATCH_SECS`: invalid values warn and fall back
//! to the documented default so a typo cannot silently disable a guard.

use std::time::Duration;

use astrolabe_core::freshness::BARRIER_MAX_INLINE;
use astrolabe_core::index::DEFAULT_PARSE_CACHE_BYTES;

use crate::reindex::{
    parse_cache_bytes_from_env, reindex_throttle_from_env, DEFAULT_REINDEX_THROTTLE,
};
use crate::roots::{DEFAULT_IDLE_SECS, DEFAULT_RESIDENT_ROOTS};

/// Default safety-scan interval (`ASTROLABE_SAFETY_SECS`). `0` disables.
pub(crate) const DEFAULT_SAFETY_SECS: u64 = 1800;
/// Default storm threshold (`ASTROLABE_STORM_PATHS`).
pub(crate) const DEFAULT_STORM_PATHS: usize = 5000;

/// Watcher mode selected from `ASTROLABE_WATCH_SECS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchMode {
    Events,
    Poll(Duration),
}

/// Parsed MCP knobs. Production constructors call [`Self::from_env`]; tests
/// pass an explicit value so they do not inherit the process environment.
#[derive(Clone, Debug)]
pub(crate) struct McpKnobs {
    pub watch_mode: Option<WatchMode>,
    pub safety_interval: Duration,
    pub storm_paths: usize,
    pub barrier_max: usize,
    pub idle: Duration,
    pub resident_roots: usize,
    pub parse_cache_bytes: u64,
    pub reindex_throttle: Duration,
    pub loop_guard: crate::loop_guard::LoopGuardConfig,
    /// Whether the precise tools may start Astrolabe's own language-server
    /// pool. In-process hosts with their own LSP manager turn this off so the
    /// index-only answer is returned and the host supplies the exact one.
    pub lsp_enabled: bool,
    /// Whether this process may take the indexer lock and write the redb
    /// store. `false` keeps the whole index in memory and never creates
    /// `<root>/.astrolabe/`; hosts use it for repositories that must stay
    /// untouched.
    pub persist_enabled: bool,
    /// Whether scans and watcher admissions honor `.gitignore`/`.ignore`.
    /// `false` indexes and searches gitignored files, matching a host `grep`
    /// that was told not to respect ignore files.
    pub respect_gitignore: bool,
    /// Host-supplied deny/exclude globs (root-relative or filename globs)
    /// applied as [`astrolabe_core::scan::ScanOptions::extra_excludes`] on
    /// every scan, watcher admission and incremental apply. Set through
    /// [`crate::server::AstrolabeServer::new_native_full`]; never read from
    /// the environment.
    pub extra_excludes: Vec<String>,
}

impl Default for McpKnobs {
    fn default() -> Self {
        Self {
            watch_mode: Some(WatchMode::Events),
            safety_interval: Duration::from_secs(DEFAULT_SAFETY_SECS),
            storm_paths: DEFAULT_STORM_PATHS,
            barrier_max: BARRIER_MAX_INLINE,
            idle: Duration::from_secs(DEFAULT_IDLE_SECS),
            resident_roots: DEFAULT_RESIDENT_ROOTS,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            reindex_throttle: DEFAULT_REINDEX_THROTTLE,
            loop_guard: crate::loop_guard::LoopGuardConfig::default(),
            lsp_enabled: true,
            persist_enabled: true,
            respect_gitignore: true,
            extra_excludes: Vec::new(),
        }
    }
}

impl McpKnobs {
    pub(crate) fn from_env() -> Self {
        Self {
            watch_mode: parse_watch_mode(std::env::var("ASTROLABE_WATCH_SECS").ok().as_deref()),
            safety_interval: parse_safety_secs(
                std::env::var("ASTROLABE_SAFETY_SECS").ok().as_deref(),
            ),
            storm_paths: parse_usize_knob(
                std::env::var("ASTROLABE_STORM_PATHS").ok().as_deref(),
                DEFAULT_STORM_PATHS,
                "ASTROLABE_STORM_PATHS",
            ),
            barrier_max: parse_usize_knob(
                std::env::var("ASTROLABE_BARRIER_MAX").ok().as_deref(),
                BARRIER_MAX_INLINE,
                "ASTROLABE_BARRIER_MAX",
            ),
            idle: parse_secs_knob(
                std::env::var("ASTROLABE_IDLE_EVICT_SECS").ok().as_deref(),
                DEFAULT_IDLE_SECS,
                "ASTROLABE_IDLE_EVICT_SECS",
            ),
            resident_roots: parse_usize_knob(
                std::env::var("ASTROLABE_RESIDENT_ROOTS").ok().as_deref(),
                DEFAULT_RESIDENT_ROOTS,
                "ASTROLABE_RESIDENT_ROOTS",
            ),
            parse_cache_bytes: parse_cache_bytes_from_env(),
            reindex_throttle: reindex_throttle_from_env(),
            loop_guard: crate::loop_guard::LoopGuardConfig::from_env(),
            lsp_enabled: true,
            persist_enabled: persist_enabled_from_env(),
            respect_gitignore: respect_gitignore_from_env(),
            // Host-provided only: the native constructor overwrites this.
            extra_excludes: Vec::new(),
        }
    }
}

/// `ASTROLABE_RESPECT_GITIGNORE=0` indexes and searches gitignored files.
fn respect_gitignore_from_env() -> bool {
    match std::env::var("ASTROLABE_RESPECT_GITIGNORE") {
        Err(_) => true,
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// `ASTROLABE_PERSIST=0` keeps the index in memory (no store, no lock).
fn persist_enabled_from_env() -> bool {
    match std::env::var("ASTROLABE_PERSIST") {
        Err(_) => true,
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | "none"
        ),
    }
}

/// Parse watcher mode from the environment variable value:
/// - Unset/None: `Some(WatchMode::Events)` (default: native OS events)
/// - "0": `None` (watching disabled)
/// - N > 0: `Some(WatchMode::Poll(Duration::from_secs(N)))`
/// - Invalid/non-integer: logs warning and defaults to `Some(WatchMode::Events)`
pub(crate) fn parse_watch_mode(raw: Option<&str>) -> Option<WatchMode> {
    match raw {
        None => Some(WatchMode::Events),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(WatchMode::Poll(Duration::from_secs(secs))),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_WATCH_SECS; using default events watcher"
                );
                Some(WatchMode::Events)
            }
        },
    }
}

/// `ASTROLABE_SAFETY_SECS`: unset → 1800s; `0` → disabled (`Duration::ZERO`);
/// invalid → default 1800s.
pub(crate) fn parse_safety_secs(raw: Option<&str>) -> Duration {
    match raw {
        None => Duration::from_secs(DEFAULT_SAFETY_SECS),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => Duration::ZERO,
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_SAFETY_SECS; using default 1800s"
                );
                Duration::from_secs(DEFAULT_SAFETY_SECS)
            }
        },
    }
}

pub(crate) fn parse_secs_knob(raw: Option<&str>, default_secs: u64, name: &str) -> Duration {
    match raw {
        None => Duration::from_secs(default_secs),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    knob = name,
                    "invalid env value; using default"
                );
                Duration::from_secs(default_secs)
            }
        },
    }
}

pub(crate) fn parse_usize_knob(raw: Option<&str>, default: usize, name: &str) -> usize {
    match raw {
        None => default,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    knob = name,
                    "invalid env value; using default"
                );
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_watch_mode_matrix() {
        assert_eq!(parse_watch_mode(None), Some(WatchMode::Events));
        assert_eq!(parse_watch_mode(Some("")), Some(WatchMode::Events));
        assert_eq!(parse_watch_mode(Some("0")), None);
        assert_eq!(parse_watch_mode(Some(" 0 ")), None);
        assert_eq!(
            parse_watch_mode(Some("5")),
            Some(WatchMode::Poll(Duration::from_secs(5)))
        );
        assert_eq!(
            parse_watch_mode(Some(" 10 ")),
            Some(WatchMode::Poll(Duration::from_secs(10)))
        );
        assert_eq!(parse_watch_mode(Some("invalid")), Some(WatchMode::Events));
    }

    #[test]
    fn env_parse_defaults_overrides_and_invalid_fallback() {
        assert_eq!(
            parse_safety_secs(None),
            Duration::from_secs(DEFAULT_SAFETY_SECS)
        );
        assert_eq!(parse_safety_secs(Some("0")), Duration::ZERO);
        assert_eq!(parse_safety_secs(Some("60")), Duration::from_secs(60));
        assert_eq!(
            parse_safety_secs(Some("nope")),
            Duration::from_secs(DEFAULT_SAFETY_SECS)
        );

        assert_eq!(parse_usize_knob(None, 5000, "ASTROLABE_STORM_PATHS"), 5000);
        assert_eq!(
            parse_usize_knob(Some("100"), 5000, "ASTROLABE_STORM_PATHS"),
            100
        );
        assert_eq!(
            parse_usize_knob(Some("x"), 5000, "ASTROLABE_STORM_PATHS"),
            5000
        );

        assert_eq!(
            parse_usize_knob(None, BARRIER_MAX_INLINE, "ASTROLABE_BARRIER_MAX"),
            64
        );
        assert_eq!(
            parse_usize_knob(Some("8"), BARRIER_MAX_INLINE, "ASTROLABE_BARRIER_MAX"),
            8
        );
        assert_eq!(
            parse_usize_knob(Some("bogus"), BARRIER_MAX_INLINE, "ASTROLABE_BARRIER_MAX"),
            64
        );

        assert_eq!(
            parse_secs_knob(None, DEFAULT_IDLE_SECS, "ASTROLABE_IDLE_EVICT_SECS"),
            Duration::from_secs(300)
        );
        assert_eq!(
            parse_secs_knob(Some("10"), DEFAULT_IDLE_SECS, "ASTROLABE_IDLE_EVICT_SECS"),
            Duration::from_secs(10)
        );
        assert_eq!(
            parse_secs_knob(Some("no"), DEFAULT_IDLE_SECS, "ASTROLABE_IDLE_EVICT_SECS"),
            Duration::from_secs(300)
        );

        assert_eq!(
            parse_usize_knob(None, DEFAULT_RESIDENT_ROOTS, "ASTROLABE_RESIDENT_ROOTS"),
            3
        );
        assert_eq!(
            parse_usize_knob(
                Some("1"),
                DEFAULT_RESIDENT_ROOTS,
                "ASTROLABE_RESIDENT_ROOTS"
            ),
            1
        );
        assert_eq!(
            parse_usize_knob(
                Some("-1"),
                DEFAULT_RESIDENT_ROOTS,
                "ASTROLABE_RESIDENT_ROOTS"
            ),
            3
        );

        let defaults = McpKnobs::default();
        assert_eq!(defaults.watch_mode, Some(WatchMode::Events));
        assert_eq!(
            defaults.safety_interval,
            Duration::from_secs(DEFAULT_SAFETY_SECS)
        );
        assert_eq!(defaults.storm_paths, DEFAULT_STORM_PATHS);
        assert_eq!(defaults.barrier_max, BARRIER_MAX_INLINE);
        assert_eq!(defaults.idle, Duration::from_secs(DEFAULT_IDLE_SECS));
        assert_eq!(defaults.resident_roots, DEFAULT_RESIDENT_ROOTS);
    }
}
