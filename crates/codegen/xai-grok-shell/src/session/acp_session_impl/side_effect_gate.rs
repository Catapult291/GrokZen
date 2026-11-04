//! Side-effect tool execution gate (TOOL-CALL-DUP-GROK-BUILD.md 方法 3).
//!
//! The batch dedup layer drops identical calls within one assistant batch; this gate covers the
//! short session window across batches: an identical `(name, canonical arguments)` call for a
//! side-effect-classified tool is skipped while the earlier one is still executing or shortly
//! after it finished, instead of running a second time.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use xai_grok_tools::types::tool::ToolKind;

/// Window for calls whose effect ends when the tool returns (`run_terminal_command`).
const SHORT_LIVED_WINDOW: Duration = Duration::from_secs(60);
/// Window for calls that leave long-lived work behind (`monitor`, subagent/task spawns,
/// workflows): the tool returns fast, the effect keeps running.
const LONG_LIVED_WINDOW: Duration = Duration::from_secs(600);

/// Side-effect classes; the class picks the post-completion window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SideEffectClass {
    ShortLived,
    LongLived,
}

/// Classify a tool call by its registered `ToolKind`, falling back to well-known wire names
/// when the toolset has no kind for the name (renamed / client-injected tools).
pub(super) fn side_effect_class(
    kind: Option<ToolKind>,
    tool_name: &str,
) -> Option<SideEffectClass> {
    match kind {
        Some(ToolKind::Execute) => Some(SideEffectClass::ShortLived),
        Some(ToolKind::Monitor | ToolKind::Task | ToolKind::Workflow) => {
            Some(SideEffectClass::LongLived)
        }
        _ => fallback_side_effect_class(tool_name),
    }
}

fn fallback_side_effect_class(tool_name: &str) -> Option<SideEffectClass> {
    match tool_name {
        "run_terminal_command" | "run_terminal_cmd" | "bash" => Some(SideEffectClass::ShortLived),
        "monitor" | "spawn_subagent" | "task" | "workflow" => Some(SideEffectClass::LongLived),
        _ => None,
    }
}

/// Why a call was gated: the earlier identical call is still executing, or finished inside the
/// class window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkipReason {
    InFlight,
    RecentlyFinished,
}

impl SkipReason {
    pub(super) fn description(self) -> &'static str {
        match self {
            SkipReason::InFlight => "is already running",
            SkipReason::RecentlyFinished => "just completed",
        }
    }
}

struct GateEntry {
    call_id: String,
    started_at: Instant,
    finished_at: Option<Instant>,
}

/// Session-scoped registry of recent side-effect tool executions, keyed on
/// `(tool name, canonical arguments)`. Signatures are canonicalized with
/// `canonical_tool_arguments`, shared with the batch dedup layer.
#[derive(Default)]
pub(crate) struct SideEffectGate {
    entries: parking_lot::Mutex<HashMap<(String, String), GateEntry>>,
}

/// Marks the entry finished when dropped, so a cancelled turn cannot leave a stale in-flight
/// entry blocking later identical calls forever.
pub(super) struct SideEffectRunGuard {
    gate: std::sync::Arc<SideEffectGate>,
    tool_name: String,
    arguments: String,
}

impl Drop for SideEffectRunGuard {
    fn drop(&mut self) {
        self.gate.finish(&self.tool_name, &self.arguments);
    }
}

impl SideEffectGate {
    /// `Some((first call id, reason))` when an identical side-effect call is executing or
    /// finished inside its class window.
    pub(super) fn check(
        &self,
        class: SideEffectClass,
        tool_name: &str,
        arguments: &str,
    ) -> Option<(String, SkipReason)> {
        self.check_at(class, tool_name, arguments, Instant::now())
    }

    fn check_at(
        &self,
        class: SideEffectClass,
        tool_name: &str,
        arguments: &str,
        now: Instant,
    ) -> Option<(String, SkipReason)> {
        let entries = self.entries.lock();
        let entry = entries.get(&(tool_name.to_owned(), canonical_arguments(arguments)))?;
        let Some(finished_at) = entry.finished_at else {
            return Some((entry.call_id.clone(), SkipReason::InFlight));
        };
        let window = match class {
            SideEffectClass::ShortLived => SHORT_LIVED_WINDOW,
            SideEffectClass::LongLived => LONG_LIVED_WINDOW,
        };
        (now.duration_since(finished_at) < window)
            .then(|| (entry.call_id.clone(), SkipReason::RecentlyFinished))
    }

    /// Record that `call_id` started executing; the returned guard marks completion on drop.
    pub(super) fn begin(
        self: &std::sync::Arc<Self>,
        tool_name: &str,
        arguments: &str,
        call_id: &str,
    ) -> SideEffectRunGuard {
        let mut entries = self.entries.lock();
        prune(&mut entries, Instant::now());
        entries.insert(
            (tool_name.to_owned(), canonical_arguments(arguments)),
            GateEntry {
                call_id: call_id.to_owned(),
                started_at: Instant::now(),
                finished_at: None,
            },
        );
        SideEffectRunGuard {
            gate: std::sync::Arc::clone(self),
            tool_name: tool_name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn finish(&self, tool_name: &str, arguments: &str) {
        if let Some(entry) = self
            .entries
            .lock()
            .get_mut(&(tool_name.to_owned(), canonical_arguments(arguments)))
        {
            entry.finished_at = Some(Instant::now());
        }
    }
}

fn canonical_arguments(arguments: &str) -> String {
    xai_grok_sampling_types::canonical_tool_arguments(arguments)
}

fn prune(entries: &mut HashMap<(String, String), GateEntry>, now: Instant) {
    entries.retain(|_, entry| {
        let reference = entry.finished_at.unwrap_or(entry.started_at);
        now.duration_since(reference) < LONG_LIVED_WINDOW
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SideEffectGate {
        fn begin_at(
            self: &std::sync::Arc<Self>,
            tool_name: &str,
            arguments: &str,
            call_id: &str,
            started_at: Instant,
        ) -> SideEffectRunGuard {
            self.entries.lock().insert(
                (tool_name.to_owned(), canonical_arguments(arguments)),
                GateEntry {
                    call_id: call_id.to_owned(),
                    started_at,
                    finished_at: None,
                },
            );
            SideEffectRunGuard {
                gate: std::sync::Arc::clone(self),
                tool_name: tool_name.to_owned(),
                arguments: arguments.to_owned(),
            }
        }

        fn finish_at(&self, tool_name: &str, arguments: &str, finished_at: Instant) {
            if let Some(entry) = self
                .entries
                .lock()
                .get_mut(&(tool_name.to_owned(), canonical_arguments(arguments)))
            {
                entry.finished_at = Some(finished_at);
            }
        }
    }

    #[test]
    fn in_flight_duplicate_is_skipped_with_first_call_id() {
        let gate = std::sync::Arc::new(SideEffectGate::default());
        let _guard = gate.begin("monitor", r#"{"command":"tail -f x.log"}"#, "call_1");
        assert_eq!(
            gate.check(
                SideEffectClass::LongLived,
                "monitor",
                r#"{ "command": "tail -f x.log" }"#
            ),
            Some(("call_1".to_owned(), SkipReason::InFlight))
        );
    }

    #[test]
    fn finished_call_is_gated_inside_class_window_and_free_after() {
        let gate = std::sync::Arc::new(SideEffectGate::default());
        let finished = Instant::now();
        let _guard = gate.begin_at(
            "run_terminal_command",
            r#"{"command":"git status"}"#,
            "call_1",
            finished,
        );
        gate.finish_at("run_terminal_command", r#"{"command":"git status"}"#, finished);

        let args = r#"{"command":"git status"}"#;
        // Short-lived: gated inside 60s, free after.
        assert_eq!(
            gate.check_at(
                SideEffectClass::ShortLived,
                "run_terminal_command",
                args,
                finished + Duration::from_secs(59)
            ),
            Some(("call_1".to_owned(), SkipReason::RecentlyFinished))
        );
        assert_eq!(
            gate.check_at(
                SideEffectClass::ShortLived,
                "run_terminal_command",
                args,
                finished + Duration::from_secs(61)
            ),
            None
        );
        // Long-lived: the window is longer, so the same age is still gated.
        assert_eq!(
            gate.check_at(
                SideEffectClass::LongLived,
                "run_terminal_command",
                args,
                finished + Duration::from_secs(61)
            ),
            Some(("call_1".to_owned(), SkipReason::RecentlyFinished))
        );
    }

    #[test]
    fn different_arguments_and_read_only_tools_are_never_gated() {
        let gate = std::sync::Arc::new(SideEffectGate::default());
        let _guard = gate.begin("monitor", r#"{"command":"a"}"#, "call_1");
        assert_eq!(
            gate.check(SideEffectClass::LongLived, "monitor", r#"{"command":"b"}"#),
            None
        );
        assert_eq!(
            gate.check(SideEffectClass::LongLived, "read_file", r#"{"command":"a"}"#),
            None
        );
    }

    #[test]
    fn guard_drop_marks_entry_finished() {
        let gate = std::sync::Arc::new(SideEffectGate::default());
        let finished = Instant::now();
        {
            let _guard = gate.begin_at("monitor", r#"{"id":1}"#, "call_1", finished);
        }
        assert_eq!(
            gate.check_at(
                SideEffectClass::LongLived,
                "monitor",
                r#"{"id":1}"#,
                finished + Duration::from_secs(1)
            ),
            Some(("call_1".to_owned(), SkipReason::RecentlyFinished))
        );
    }

    #[test]
    fn class_follows_tool_kind_then_wire_name_fallback() {
        assert_eq!(
            side_effect_class(Some(ToolKind::Execute), "anything"),
            Some(SideEffectClass::ShortLived)
        );
        assert_eq!(
            side_effect_class(Some(ToolKind::Monitor), "anything"),
            Some(SideEffectClass::LongLived)
        );
        assert_eq!(
            side_effect_class(Some(ToolKind::Task), "anything"),
            Some(SideEffectClass::LongLived)
        );
        assert_eq!(
            side_effect_class(Some(ToolKind::Workflow), "anything"),
            Some(SideEffectClass::LongLived)
        );
        assert_eq!(
            side_effect_class(Some(ToolKind::Read), "read_file"),
            None
        );
        assert_eq!(
            side_effect_class(None, "run_terminal_command"),
            Some(SideEffectClass::ShortLived)
        );
        assert_eq!(
            side_effect_class(None, "monitor"),
            Some(SideEffectClass::LongLived)
        );
        assert_eq!(side_effect_class(None, "unknown_tool"), None);
    }
}
