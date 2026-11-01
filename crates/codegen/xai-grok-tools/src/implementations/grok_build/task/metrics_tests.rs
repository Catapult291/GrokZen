use super::*;

fn result(stop_reason: &str, success: bool, cancelled: bool) -> SubagentResult {
    SubagentResult {
        success,
        cancelled,
        stop_reason: stop_reason.to_owned(),
        ..SubagentResult::default()
    }
}

#[test]
fn spawns_are_counted_per_type() {
    let mut metrics = SubagentMetrics::default();
    metrics.record_spawn("explore");
    metrics.record_spawn("explore");
    metrics.record_spawn("implementer");

    let summary = metrics.summary();
    assert_eq!(summary.spawns_by_type.get("explore"), Some(&2));
    assert_eq!(summary.spawns_by_type.get("implementer"), Some(&1));
    assert_eq!(summary.spawns_by_type.len(), 2);
}

#[test]
fn queue_wait_tracks_count_total_and_max() {
    let mut metrics = SubagentMetrics::default();
    metrics.record_queue_wait(Duration::from_millis(30));
    metrics.record_queue_wait(Duration::from_millis(90));
    metrics.record_queue_wait(Duration::ZERO);

    let summary = metrics.summary();
    assert_eq!(summary.queued_starts, 3);
    assert_eq!(summary.queue_wait_ms_total, 120);
    assert_eq!(summary.queue_wait_ms_max, 90);
    assert_eq!(summary.queue_wait_ms_avg(), 40);
}

#[test]
fn terminal_reason_maps_canonical_stop_reasons() {
    assert_eq!(
        terminal_reason(
            &result(xai_tool_types::stop_reason::END_TURN, true, false),
            true
        ),
        "end_turn"
    );
    assert_eq!(
        terminal_reason(
            &result(xai_tool_types::stop_reason::MAX_TURNS, false, true),
            true
        ),
        "max_turns"
    );
    assert_eq!(
        terminal_reason(
            &result(xai_tool_types::stop_reason::CANCELLED, false, true),
            true
        ),
        "cancelled"
    );
    assert_eq!(
        terminal_reason(
            &result(
                xai_tool_types::stop_reason::NO_STRUCTURED_RESULT,
                false,
                false
            ),
            true
        ),
        "no_structured_result"
    );
    assert_eq!(
        terminal_reason(
            &result(xai_tool_types::stop_reason::ERROR, false, false),
            true
        ),
        "error"
    );
}

#[test]
fn unstamped_stop_reason_falls_back_to_outcome_flags() {
    assert_eq!(terminal_reason(&result("", true, false), true), "end_turn");
    assert_eq!(terminal_reason(&result("", false, true), true), "cancelled");
    assert_eq!(terminal_reason(&result("", false, false), true), "error");
    assert_eq!(
        terminal_reason(&result("unexpected", false, false), true),
        "error"
    );
}

#[test]
fn never_started_records_are_split_by_cancellation() {
    assert_eq!(
        terminal_reason(&result("", false, false), false),
        "admission_rejected"
    );
    assert_eq!(
        terminal_reason(&result("", false, true), false),
        "queued_cancelled"
    );
}

#[test]
fn launched_terminals_total_run_and_token_usage() {
    let mut metrics = SubagentMetrics::default();
    metrics.record_terminal(
        &SubagentResult {
            success: true,
            duration_ms: 1_200,
            tokens_used: 800,
            output_tokens_used: 120,
            stop_reason: xai_tool_types::stop_reason::END_TURN.to_owned(),
            ..SubagentResult::default()
        },
        true,
        Duration::from_millis(5),
    );
    metrics.record_terminal(
        &SubagentResult {
            success: false,
            duration_ms: 0,
            tokens_used: 200,
            output_tokens_used: 30,
            stop_reason: xai_tool_types::stop_reason::ERROR.to_owned(),
            ..SubagentResult::default()
        },
        true,
        Duration::from_millis(400),
    );

    let summary = metrics.summary();
    assert_eq!(summary.run_samples, 2);
    assert_eq!(summary.run_ms_total, 1_600);
    assert_eq!(summary.run_ms_max, 1_200);
    assert_eq!(summary.run_ms_avg(), 800);
    assert_eq!(summary.tokens_used_total, 1_000);
    assert_eq!(summary.output_tokens_total, 150);
    assert_eq!(summary.terminals_by_reason.get("end_turn"), Some(&1));
    assert_eq!(summary.terminals_by_reason.get("error"), Some(&1));
}

#[test]
fn never_started_terminals_do_not_contribute_run_or_tokens() {
    let mut metrics = SubagentMetrics::default();
    metrics.record_terminal(
        &SubagentResult {
            duration_ms: 1_000,
            tokens_used: 50,
            ..SubagentResult::default()
        },
        false,
        Duration::from_millis(100),
    );

    let summary = metrics.summary();
    assert_eq!(summary.run_samples, 0);
    assert_eq!(summary.run_ms_total, 0);
    assert_eq!(summary.tokens_used_total, 0);
    assert_eq!(
        summary.terminals_by_reason.get("admission_rejected"),
        Some(&1)
    );
}
