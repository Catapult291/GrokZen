//! Server-side loop guard for the search tools (`search_code` / `find_symbol`).
//!
//! Hooks deliberately do not count Astrolabe's own tools, so a model stuck in a
//! loop (observed: 9,055 `search_code` calls in 48 minutes cycling through 109
//! distinct queries) is never stopped client-side. This guard lives in the MCP
//! server process (one per client connection) and:
//!
//! * short-circuits a query whose signature (tool + every argument, including
//!   `budget_tokens`, + index generation) already ran successfully
//!   `repeat_max` times inside `repeat_window`, returning the cached previous
//!   result with a `blocked: true` note instead of re-searching;
//! * appends a warning once the overall successful search count inside
//!   `rate_window` exceeds `rate_max`.
//!
//! Thresholds come from env vars (see [`LoopGuardConfig::from_env`]); `0`
//! disables the corresponding check.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_REPEAT_MAX: usize = 3;
pub(crate) const DEFAULT_REPEAT_WINDOW_SECS: u64 = 600;
pub(crate) const DEFAULT_RATE_MAX: usize = 300;
pub(crate) const DEFAULT_RATE_WINDOW_SECS: u64 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoopGuardConfig {
    /// Allowed runs of one signature per window; `0` disables (as does a
    /// `0` repeat window).
    pub repeat_max: usize,
    pub repeat_window: Duration,
    /// Search calls per window before warning; `0` disables (as does a `0`
    /// rate window).
    pub rate_max: usize,
    pub rate_window: Duration,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            repeat_max: DEFAULT_REPEAT_MAX,
            repeat_window: Duration::from_secs(DEFAULT_REPEAT_WINDOW_SECS),
            rate_max: DEFAULT_RATE_MAX,
            rate_window: Duration::from_secs(DEFAULT_RATE_WINDOW_SECS),
        }
    }
}

fn parse_num<T: std::str::FromStr + Copy>(raw: Option<&str>, default: T, name: &str) -> T {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => default,
        Some(s) => s.parse().unwrap_or_else(|_| {
            tracing::warn!(value = s, knob = name, "invalid value; using default");
            default
        }),
    }
}

impl LoopGuardConfig {
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let n = |k: &str, d: usize| parse_num(get(k).as_deref(), d, k);
        // A window of 0 means "disable the corresponding check" (the same as
        // `*_MAX=0`), not "silently use the default".
        let secs = |k: &str, d: u64| parse_num(get(k).as_deref(), d, k);
        let repeat_secs = secs(
            "ASTROLABE_SEARCH_REPEAT_WINDOW_SECS",
            DEFAULT_REPEAT_WINDOW_SECS,
        );
        let rate_secs = secs(
            "ASTROLABE_SEARCH_RATE_WINDOW_SECS",
            DEFAULT_RATE_WINDOW_SECS,
        );
        let repeat_max = n("ASTROLABE_SEARCH_REPEAT_MAX", DEFAULT_REPEAT_MAX);
        let rate_max = n("ASTROLABE_SEARCH_RATE_MAX", DEFAULT_RATE_MAX);
        Self {
            repeat_max: if repeat_secs == 0 { 0 } else { repeat_max },
            repeat_window: Duration::from_secs(repeat_secs),
            rate_max: if rate_secs == 0 { 0 } else { rate_max },
            rate_window: Duration::from_secs(rate_secs),
        }
    }
}

/// Outcome of [`LoopGuard::check`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict<R> {
    /// Run the search normally.
    Run,
    /// Run, but append this warning to the result.
    RunWithWarning(String),
    /// Do not run; return the cached result of the identical query (same
    /// arguments, same index generation) with this note prepended.
    Block { note: String, cached: R },
}

#[derive(Debug)]
struct Entry<R> {
    runs: VecDeque<Instant>,
    last: R,
}

/// Per-connection guard. `R` is the cached result type (a `CallToolResult`
/// in the server; a plain string in unit tests).
///
/// Only *successful, executed* queries are recorded (via [`LoopGuard::record`]);
/// errors and "index building, retry" responses never count. The signature
/// key includes the full argument set and the index generation stamp, so any
/// index change (an edit picked up by the watcher, a rebuild) starts a fresh
/// count and re-verification after edits always re-executes.
#[derive(Debug)]
pub(crate) struct LoopGuard<R = String> {
    config: LoopGuardConfig,
    per_sig: HashMap<String, Entry<R>>,
    all: VecDeque<Instant>,
}

impl<R> Default for LoopGuard<R> {
    fn default() -> Self {
        Self {
            config: LoopGuardConfig::default(),
            per_sig: HashMap::new(),
            all: VecDeque::new(),
        }
    }
}

fn prune(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while q
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= window)
    {
        q.pop_front();
    }
}

fn key(tool: &str, signature: &str, stamp: u64) -> String {
    format!("{tool}\u{1f}{stamp}\u{1f}{signature}")
}

impl<R: Clone> LoopGuard<R> {
    pub(crate) fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(crate) fn check(&mut self, tool: &str, signature: &str, stamp: u64) -> Verdict<R> {
        self.check_at(tool, signature, stamp, Instant::now())
    }

    pub(crate) fn record(&mut self, tool: &str, signature: &str, stamp: u64, result: R) {
        self.record_at(tool, signature, stamp, result, Instant::now());
    }

    /// Decide what to do with a query. Does not count anything by itself.
    pub(crate) fn check_at(
        &mut self,
        tool: &str,
        signature: &str,
        stamp: u64,
        now: Instant,
    ) -> Verdict<R> {
        let cfg = self.config;
        let mut rate_note = None;
        if cfg.rate_max > 0 {
            prune(&mut self.all, now, cfg.rate_window);
            if self.all.len() >= cfg.rate_max {
                rate_note = Some(format!(
                    "⚠ loop guard: 最近 {} 秒内已成功执行搜索 {} 次（上限 {}）。\
                     很可能陷入了循环：请停止批量轮换查询，基于已有结果作答，或换用 \
                     find_references / find_symbol / Read 等方式推进。",
                    cfg.rate_window.as_secs(),
                    self.all.len() + 1,
                    cfg.rate_max
                ));
            }
        }
        if cfg.repeat_max > 0 {
            if let Some(entry) = self.per_sig.get_mut(&key(tool, signature, stamp)) {
                prune(&mut entry.runs, now, cfg.repeat_window);
                let prior = entry.runs.len();
                if prior >= cfg.repeat_max {
                    let mut note = format!(
                        "blocked: true\nloop guard: 这条 {tool} 查询（参数完全相同、索引自上次执行后未变化）\
                         在最近 {} 秒内已执行 {prior} 次（上限 {}），本次未重新执行，下面附上上一次的执行结果。\
                         下面的结果就是该查询的完整结果（即使这条查询最初来自其他子任务或你已压缩的上下文，也请直接使用）。\
                         若需要不同信息，请修改查询/path_filter/budget_tokens，或换一种方法\
                         （find_references、goto_definition、Read 指定行段）。",
                        cfg.repeat_window.as_secs(),
                        cfg.repeat_max
                    );
                    if let Some(rate) = rate_note {
                        note.push('\n');
                        note.push_str(&rate);
                    }
                    return Verdict::Block {
                        note,
                        cached: entry.last.clone(),
                    };
                }
            }
        }
        match rate_note {
            Some(note) => Verdict::RunWithWarning(note),
            None => Verdict::Run,
        }
    }

    /// Count a successful execution and cache its result.
    pub(crate) fn record_at(
        &mut self,
        tool: &str,
        signature: &str,
        stamp: u64,
        result: R,
        now: Instant,
    ) {
        let cfg = self.config;
        if cfg.rate_max > 0 {
            prune(&mut self.all, now, cfg.rate_window);
            self.all.push_back(now);
        }
        if cfg.repeat_max > 0 {
            let window = cfg.repeat_window;
            // Bound memory: drop signatures whose entries all expired.
            if self.per_sig.len() > 4096 {
                self.per_sig.retain(|_, e| {
                    prune(&mut e.runs, now, window);
                    !e.runs.is_empty()
                });
            }
            let k = key(tool, signature, stamp);
            match self.per_sig.get_mut(&k) {
                Some(entry) => {
                    prune(&mut entry.runs, now, window);
                    entry.runs.push_back(now);
                    entry.last = result;
                }
                None => {
                    self.per_sig.insert(
                        k,
                        Entry {
                            runs: VecDeque::from([now]),
                            last: result,
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(repeat_max: usize, rate_max: usize) -> LoopGuardConfig {
        LoopGuardConfig {
            repeat_max,
            repeat_window: Duration::from_secs(600),
            rate_max,
            rate_window: Duration::from_secs(600),
        }
    }

    /// Simulates the server: check, and on Run execute + record.
    fn call(g: &mut LoopGuard, sig: &str, stamp: u64, now: Instant) -> bool {
        match g.check_at("search_code", sig, stamp, now) {
            Verdict::Block { .. } => true,
            _ => {
                g.record_at("search_code", sig, stamp, format!("res:{sig}"), now);
                false
            }
        }
    }

    #[test]
    fn repeats_within_window_are_blocked_after_max_with_cached_result() {
        let mut g: LoopGuard = LoopGuard::new(cfg(3, 0));
        let t = Instant::now();
        for i in 0..3 {
            assert!(!call(&mut g, "q", 1, t + Duration::from_secs(i)));
        }
        match g.check_at("search_code", "q", 1, t + Duration::from_secs(4)) {
            Verdict::Block { note, cached } => {
                assert_eq!(cached, "res:q");
                assert!(note.contains("blocked: true"), "{note}");
                assert!(!note.contains("结果与之前相同"), "{note}");
            }
            v => panic!("expected block, got {v:?}"),
        }
        // other signature / other tool unaffected
        assert_eq!(g.check_at("search_code", "other", 1, t), Verdict::Run);
        assert_eq!(g.check_at("find_symbol", "q", 1, t), Verdict::Run);
    }

    #[test]
    fn index_change_resets_repeat_count() {
        let mut g: LoopGuard = LoopGuard::new(cfg(3, 0));
        let t = Instant::now();
        for _ in 0..3 {
            assert!(!call(&mut g, "q", 1, t));
        }
        assert!(call(&mut g, "q", 1, t));
        // an edit bumped the index generation: re-verification executes
        assert!(!call(&mut g, "q", 2, t));
    }

    #[test]
    fn unrecorded_failures_never_count() {
        let mut g: LoopGuard = LoopGuard::new(cfg(3, 5));
        let t = Instant::now();
        // "index building" / error responses: checked but never recorded
        for _ in 0..50 {
            assert_eq!(g.check_at("search_code", "q", 1, t), Verdict::Run);
        }
    }

    #[test]
    fn rotating_queries_are_caught_and_window_expires() {
        let mut g: LoopGuard = LoopGuard::new(cfg(3, 0));
        let t = Instant::now();
        let mut blocked = 0;
        for round in 0..20u64 {
            for q in 0..5u64 {
                let now = t + Duration::from_secs(round * 5 + q);
                if call(&mut g, &q.to_string(), 7, now) {
                    blocked += 1;
                }
            }
        }
        assert_eq!(blocked, 100 - 15);
        let later = t + Duration::from_secs(100 + 600);
        assert_eq!(g.check_at("search_code", "0", 7, later), Verdict::Run);
    }

    #[test]
    fn rate_cap_warns_but_runs() {
        let mut g: LoopGuard = LoopGuard::new(cfg(0, 5));
        let t = Instant::now();
        for i in 0..5 {
            assert!(!call(&mut g, &i.to_string(), 1, t));
        }
        assert!(matches!(
            g.check_at("search_code", "x", 1, t),
            Verdict::RunWithWarning(_)
        ));
        assert_eq!(
            g.check_at("search_code", "y", 1, t + Duration::from_secs(601)),
            Verdict::Run
        );
    }

    #[test]
    fn zero_disables_everything() {
        let mut g: LoopGuard = LoopGuard::new(cfg(0, 0));
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(!call(&mut g, "q", 1, t));
        }
    }

    #[test]
    fn config_from_env_parses_and_falls_back() {
        let c = LoopGuardConfig::from_lookup(|_| None);
        assert_eq!(c, LoopGuardConfig::default());
        let c = LoopGuardConfig::from_lookup(|k| match k {
            "ASTROLABE_SEARCH_REPEAT_MAX" => Some("5".into()),
            "ASTROLABE_SEARCH_RATE_MAX" => Some("bogus".into()),
            _ => None,
        });
        assert_eq!(c.repeat_max, 5);
        assert_eq!(c.rate_max, DEFAULT_RATE_MAX);
        // A zero window disables the check; it is not the default window.
        let c = LoopGuardConfig::from_lookup(|k| match k {
            "ASTROLABE_SEARCH_REPEAT_WINDOW_SECS" => Some("0".into()),
            _ => None,
        });
        assert_eq!(c.repeat_window, Duration::ZERO);
        assert_eq!(c.repeat_max, 0);
        let c = LoopGuardConfig::from_lookup(|k| match k {
            "ASTROLABE_SEARCH_RATE_WINDOW_SECS" => Some("0".into()),
            _ => None,
        });
        assert_eq!(c.rate_max, 0);
        assert_eq!(c.repeat_max, DEFAULT_REPEAT_MAX);
    }

    #[test]
    fn zero_window_env_never_blocks() {
        let c = LoopGuardConfig::from_lookup(|k| match k {
            "ASTROLABE_SEARCH_REPEAT_WINDOW_SECS" => Some("0".into()),
            _ => None,
        });
        let mut g: LoopGuard = LoopGuard::new(c);
        let t = Instant::now();
        for _ in 0..20 {
            assert!(!call(&mut g, "q", 1, t));
        }
    }
}
