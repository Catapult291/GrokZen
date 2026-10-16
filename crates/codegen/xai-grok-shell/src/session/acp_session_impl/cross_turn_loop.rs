//! Cross-turn loop breaker: same visible preamble + same tool-name set across consecutive steps.
//!
//! The in-stream [`super::turn`] `IdenticalToolCallRun` only compares tool-call signatures
//! (name + canonicalized args) and needs many identical steps before it hard-stops. The
//! stream-level text-repetition guard only looks inside one sampling attempt. Neither catches
//! the production incident where a short model re-emits the same Chinese classification line
//! and the same pair of poll tools after every tool result while a background job is still
//! `running` — each step is a clean short stream, so both existing guards stay silent.
//!
//! This breaker fires when `max_identical_steps` consecutive steps share:
//! * the same normalized assistant preamble (whitespace-collapsed), and
//! * the same multiset of tool names (order-insensitive; arguments are *not* compared —
//!   a polling loop can shuffle JSON keys and still be the same stuck behavior).
//!
//! Empty preambles and tool-free steps reset the run: pure text turns end the turn, and
//! argument-only tool thrash is already owned by `IdenticalToolCallRun`.
//!
//! State is one run counter plus the last fingerprint. No message retention.

use std::collections::HashSet;

/// Consecutive identical (preamble, tool-set) steps that trip the breaker.
/// Low on purpose: the UI spam is the product, and text identity is already a strong signal.
pub(crate) const CROSS_TURN_LOOP_THRESHOLD: u32 = 3;

#[derive(Debug)]
pub(crate) struct CrossTurnLoopGuard {
    last_key: Option<String>,
    run_len: u32,
    tripped: bool,
}

impl Default for CrossTurnLoopGuard {
    fn default() -> Self {
        Self {
            last_key: None,
            run_len: 0,
            tripped: false,
        }
    }
}

impl CrossTurnLoopGuard {
    /// Feed one sampling step. Returns `Some(run_len)` the first time the run reaches
    /// [`CROSS_TURN_LOOP_THRESHOLD`]; later calls stay silent (latched).
    pub(crate) fn observe(&mut self, assistant_text: &str, tool_names: &[String]) -> Option<u32> {
        if self.tripped || tool_names.is_empty() {
            return None;
        }
        let preamble = normalize_preamble(assistant_text);
        if preamble.is_empty() {
            self.reset();
            return None;
        }
        let key = fingerprint(&preamble, tool_names);
        if self.last_key.as_deref() == Some(key.as_str()) {
            self.run_len += 1;
        } else {
            self.last_key = Some(key);
            self.run_len = 1;
        }
        if self.run_len >= CROSS_TURN_LOOP_THRESHOLD && !self.tripped {
            self.tripped = true;
            return Some(self.run_len);
        }
        None
    }

    pub(crate) fn run_len(&self) -> u32 {
        self.run_len
    }

    pub(crate) fn is_tripped(&self) -> bool {
        self.tripped
    }

    fn reset(&mut self) {
        self.last_key = None;
        self.run_len = 0;
    }
}

/// Collapse all whitespace runs (including full-width spaces the CJK models emit) and trim.
/// Case is left alone: tool names are already case-sensitive and Chinese has no case.
fn normalize_preamble(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fingerprint(preamble: &str, tool_names: &[String]) -> String {
    let mut names: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    names.sort_unstable();
    let mut unique: Vec<&str> = Vec::with_capacity(names.len());
    let mut seen = HashSet::with_capacity(names.len());
    for name in names {
        if seen.insert(name) {
            unique.push(name);
        }
    }
    format!("{preamble}\u{1f}{}", unique.join("\u{1e}"))
}

#[cfg(test)]
mod cross_turn_loop_guard_tests {
    use super::{CROSS_TURN_LOOP_THRESHOLD, CrossTurnLoopGuard};

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn trips_after_threshold_identical_preamble_and_tool_set() {
        let mut guard = CrossTurnLoopGuard::default();
        let text = "任务分类：**build**（发版，直接生产）。Windows 两腿失败是 Chocolatey 503 抖动，等 run 结束后重跑失败腿。";
        let tools = names(&["run_terminal_command", "get_command_or_subagent_output"]);
        assert_eq!(guard.observe(text, &tools), None);
        assert_eq!(guard.observe(text, &tools), None);
        assert_eq!(
            guard.observe(text, &tools),
            Some(CROSS_TURN_LOOP_THRESHOLD),
            "third identical step must trip"
        );
        assert!(guard.is_tripped());
        // Latched: further identical steps stay silent.
        assert_eq!(guard.observe(text, &tools), None);
    }

    #[test]
    fn whitespace_and_tool_order_do_not_matter() {
        let mut guard = CrossTurnLoopGuard::default();
        let a = "任务分类：build 等 run 结束后重跑。";
        let b = "  任务分类：build\n等 run  结束后重跑。  ";
        assert_eq!(guard.observe(a, &names(&["b", "a"])), None);
        assert_eq!(guard.observe(b, &names(&["a", "b"])), None);
        assert_eq!(
            guard.observe(a, &names(&["b", "a"])),
            Some(CROSS_TURN_LOOP_THRESHOLD)
        );
    }

    #[test]
    fn different_preamble_or_tool_set_resets_the_run() {
        let mut guard = CrossTurnLoopGuard::default();
        let t1 = names(&["run_terminal_command"]);
        let t2 = names(&["get_command_or_subagent_output"]);
        assert_eq!(guard.observe("同一句", &t1), None);
        assert_eq!(guard.observe("换了句", &t1), None, "text change resets");
        assert_eq!(guard.observe("换了句", &t2), None, "tool set change resets");
        assert_eq!(guard.observe("换了句", &t2), None);
        assert_eq!(
            guard.observe("换了句", &t2),
            Some(CROSS_TURN_LOOP_THRESHOLD)
        );
    }

    #[test]
    fn empty_preamble_or_tool_free_step_resets_and_never_trips() {
        let mut guard = CrossTurnLoopGuard::default();
        let tools = names(&["run_terminal_command"]);
        assert_eq!(guard.observe("有句子", &tools), None);
        assert_eq!(guard.observe("   ", &tools), None, "blank preamble resets");
        assert_eq!(guard.observe("", &tools), None);
        // Tool-free steps return early without counting; a later run starts fresh at 1.
        assert_eq!(guard.observe("有句子", &[]), None);
        assert_eq!(guard.observe("有句子", &tools), None, "run restarts at 1");
        assert_eq!(guard.observe("有句子", &tools), None, "run_len=2");
        assert_eq!(
            guard.observe("有句子", &tools),
            Some(CROSS_TURN_LOOP_THRESHOLD)
        );
    }

    #[test]
    fn duplicate_tool_names_collapse_to_one_slot() {
        let mut guard = CrossTurnLoopGuard::default();
        assert_eq!(guard.observe("等 CI", &names(&["gh", "gh"])), None);
        assert_eq!(guard.observe("等 CI", &names(&["gh"])), None);
        assert_eq!(
            guard.observe("等 CI", &names(&["gh", "gh"])),
            Some(CROSS_TURN_LOOP_THRESHOLD)
        );
    }
}
