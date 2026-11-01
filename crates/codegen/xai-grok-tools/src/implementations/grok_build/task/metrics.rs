//! Process-local subagent metrics.
//!
//! The host already emits per-child telemetry. This rollup answers the routing
//! questions from the same local data: spawns per type, admission queue wait,
//! run duration, token use, and terminal-reason distribution. Nothing is
//! exported; the summary is written as one `INFO` line when the coordinator
//! actor exits.

use std::collections::BTreeMap;
use std::time::Duration;

use super::types::SubagentResult;

/// Counters accumulated by the coordinator for the process lifetime.
#[derive(Debug, Default)]
pub(crate) struct SubagentMetrics {
    spawns_by_type: BTreeMap<String, u64>,
    terminals_by_reason: BTreeMap<&'static str, u64>,
    queued_starts: u64,
    queue_wait_ms_total: u64,
    queue_wait_ms_max: u64,
    run_samples: u64,
    run_ms_total: u64,
    run_ms_max: u64,
    tokens_used_total: u64,
    output_tokens_total: u64,
}

impl SubagentMetrics {
    /// One admitted spawn (started immediately or parked in the admission queue).
    /// Wakes are continuations of an existing child and are not counted.
    pub(crate) fn record_spawn(&mut self, subagent_type: &str) {
        *self
            .spawns_by_type
            .entry(subagent_type.to_owned())
            .or_default() += 1;
    }

    /// Time a spawn spent parked before its runner started.
    pub(crate) fn record_queue_wait(&mut self, waited: Duration) {
        let ms = millis(waited);
        self.queued_starts += 1;
        self.queue_wait_ms_total = self.queue_wait_ms_total.saturating_add(ms);
        self.queue_wait_ms_max = self.queue_wait_ms_max.max(ms);
    }

    /// One terminal child record, including spawns that never reached a runner.
    pub(crate) fn record_terminal(
        &mut self,
        result: &SubagentResult,
        launched: bool,
        elapsed: Duration,
    ) {
        *self
            .terminals_by_reason
            .entry(terminal_reason(result, launched))
            .or_default() += 1;
        if !launched {
            return;
        }
        let run_ms = if result.duration_ms > 0 {
            result.duration_ms
        } else {
            millis(elapsed)
        };
        self.run_samples += 1;
        self.run_ms_total = self.run_ms_total.saturating_add(run_ms);
        self.run_ms_max = self.run_ms_max.max(run_ms);
        self.tokens_used_total = self.tokens_used_total.saturating_add(result.tokens_used);
        self.output_tokens_total = self
            .output_tokens_total
            .saturating_add(result.output_tokens_used);
    }

    pub(crate) fn summary(&self) -> SubagentMetricsSummary {
        SubagentMetricsSummary {
            spawns_by_type: self.spawns_by_type.clone(),
            terminals_by_reason: self.terminals_by_reason.clone(),
            queued_starts: self.queued_starts,
            queue_wait_ms_total: self.queue_wait_ms_total,
            queue_wait_ms_max: self.queue_wait_ms_max,
            run_samples: self.run_samples,
            run_ms_total: self.run_ms_total,
            run_ms_max: self.run_ms_max,
            tokens_used_total: self.tokens_used_total,
            output_tokens_total: self.output_tokens_total,
        }
    }

    /// Emit the rollup once, at actor shutdown. Silent when nothing ran.
    pub(crate) fn log_summary(&self) {
        if self.spawns_by_type.is_empty() && self.terminals_by_reason.is_empty() {
            return;
        }
        let summary = self.summary();
        tracing::info!(
            spawns_by_type = ?summary.spawns_by_type,
            terminals_by_reason = ?summary.terminals_by_reason,
            queued_starts = summary.queued_starts,
            queue_wait_ms_avg = summary.queue_wait_ms_avg(),
            queue_wait_ms_max = summary.queue_wait_ms_max,
            run_ms_avg = summary.run_ms_avg(),
            run_ms_max = summary.run_ms_max,
            tokens_used_total = summary.tokens_used_total,
            output_tokens_total = summary.output_tokens_total,
            "subagent metrics summary"
        );
    }
}

/// Snapshot of [`SubagentMetrics`], shaped for tests and logging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubagentMetricsSummary {
    pub(crate) spawns_by_type: BTreeMap<String, u64>,
    pub(crate) terminals_by_reason: BTreeMap<&'static str, u64>,
    pub(crate) queued_starts: u64,
    pub(crate) queue_wait_ms_total: u64,
    pub(crate) queue_wait_ms_max: u64,
    pub(crate) run_samples: u64,
    pub(crate) run_ms_total: u64,
    pub(crate) run_ms_max: u64,
    pub(crate) tokens_used_total: u64,
    pub(crate) output_tokens_total: u64,
}

impl SubagentMetricsSummary {
    pub(crate) fn queue_wait_ms_avg(&self) -> u64 {
        if self.queued_starts == 0 {
            0
        } else {
            self.queue_wait_ms_total / self.queued_starts
        }
    }

    pub(crate) fn run_ms_avg(&self) -> u64 {
        if self.run_samples == 0 {
            0
        } else {
            self.run_ms_total / self.run_samples
        }
    }
}

/// Bucket a terminal child by why its turn ended.
///
/// Records that never reached a runner are split by cancellation so an
/// admission rejection stays distinguishable from a queue-time stop. A
/// launched child reports its canonical `stop_reason`; an unstamped result
/// falls back to the outcome flags.
pub(crate) fn terminal_reason(result: &SubagentResult, launched: bool) -> &'static str {
    if !launched {
        return if result.cancelled {
            "queued_cancelled"
        } else {
            "admission_rejected"
        };
    }
    match result.stop_reason.as_str() {
        "" => {
            if result.cancelled {
                "cancelled"
            } else if result.success {
                "end_turn"
            } else {
                "error"
            }
        }
        xai_tool_types::stop_reason::END_TURN => "end_turn",
        xai_tool_types::stop_reason::MAX_TURNS => "max_turns",
        xai_tool_types::stop_reason::CANCELLED => "cancelled",
        xai_tool_types::stop_reason::NO_STRUCTURED_RESULT => "no_structured_result",
        xai_tool_types::stop_reason::ERROR => "error",
        _ => "error",
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
