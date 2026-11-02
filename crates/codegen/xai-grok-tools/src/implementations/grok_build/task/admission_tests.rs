use pretty_assertions::assert_eq;

use super::{
    Admission, AdmissionDecision, AdmissionError, LimitBehavior, SubagentLimits,
    DEFAULT_MAX_TOTAL_AGENTS,
};
use crate::implementations::grok_build::task::types::{SubagentOwner, SubagentRequest};

fn request(parent_session_id: &str) -> SubagentRequest {
    SubagentRequest {
        label: None,
        id: uuid::Uuid::now_v7().to_string(),
        prompt: "work".to_owned(),
        description: "work".to_owned(),
        subagent_type: "general-purpose".to_owned(),
        parent_session_id: parent_session_id.to_owned(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: false,
        await_to_completion: true,
        fork_context: false,
        owner: SubagentOwner::Task,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        spawn_root: Default::default(),
        tool_call_id: None,
    }
}

#[test]
fn limits_accept_plain_positive_digits_and_ignore_everything_else() {
    let from = |concurrent: Option<&str>, behavior: Option<&str>| {
        SubagentLimits::from_lookup(|var| {
            match var {
                "GROK_MAX_CONCURRENT_SUBAGENTS" => concurrent,
                "GROK_SUBAGENTS_MAX_TOTAL_AGENTS" => None,
                "GROK_SUBAGENT_LIMIT_BEHAVIOR" => behavior,
                other => panic!("unexpected lookup: {other}"),
            }
            .map(str::to_owned)
        })
    };

    assert_eq!(from(None, None), SubagentLimits::default());
    assert_eq!(
        from(Some("5"), Some("FAIL")),
        SubagentLimits {
            max_concurrent: 5,
            max_total_agents: DEFAULT_MAX_TOTAL_AGENTS,
            behavior: LimitBehavior::Fail,
        }
    );
    // Adjustable but never disabled: zero, negatives, non-digits, scientific
    // notation, digit separators, and out-of-range values all keep defaults.
    for ignored in ["0", "-1", "abc", "1e3", "20_000", "18446744073709551616"] {
        assert_eq!(
            from(Some(ignored), Some("nonsense")),
            SubagentLimits::default(),
            "value {ignored:?} should fall back to the defaults"
        );
    }
}

#[test]
fn a_zero_limit_is_clamped_to_one_not_disabled() {
    let admission = Admission::new(SubagentLimits {
        max_concurrent: 0,
        max_total_agents: 8,
        behavior: LimitBehavior::Queue,
    });
    assert_eq!(admission.max_concurrent(), 1);
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 0, /*total*/ 0),
        AdmissionDecision::Start,
        "a zero limit must still admit one child, or queued spawns starve"
    );
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 1, /*total*/ 0),
        AdmissionDecision::Enqueue
    );
}

#[test]
fn fail_mode_rejects_at_the_concurrent_limit() {
    let admission = Admission::new(SubagentLimits {
        max_concurrent: 1,
        max_total_agents: 8,
        behavior: LimitBehavior::Fail,
    });

    assert_eq!(
        admission.admit(&request("a"), /*running*/ 0, /*total*/ 0),
        AdmissionDecision::Start
    );
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 1, /*total*/ 0),
        AdmissionDecision::Reject(AdmissionError::ConcurrentLimitReached { limit: 1 })
    );
    // The rejection is stateless: spawning succeeds once the count drops.
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 0, /*total*/ 0),
        AdmissionDecision::Start
    );
}

#[test]
fn total_budget_rejects_regardless_of_concurrency_behavior() {
    // Queue mode still refuses past the cumulative budget: waiting for a
    // slot never resolves an exhausted total.
    let admission = Admission::new(SubagentLimits {
        max_concurrent: 8,
        max_total_agents: 2,
        behavior: LimitBehavior::Queue,
    });
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 0, /*total*/ 1),
        AdmissionDecision::Start
    );
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 0, /*total*/ 2),
        AdmissionDecision::Reject(AdmissionError::TotalAgentsLimitReached { limit: 2 })
    );
    // Workflow children draw from their own pool and bypass the budget.
    let mut workflow = request("a");
    workflow.owner = SubagentOwner::workflow("run-1");
    assert_eq!(
        admission.admit(&workflow, /*running*/ 0, /*total*/ 99),
        AdmissionDecision::Start
    );
    // A zero budget is clamped to one, never disabled.
    let clamped = Admission::new(SubagentLimits {
        max_concurrent: 8,
        max_total_agents: 0,
        behavior: LimitBehavior::Queue,
    });
    assert_eq!(
        clamped.admit(&request("a"), /*running*/ 0, /*total*/ 1),
        AdmissionDecision::Reject(AdmissionError::TotalAgentsLimitReached { limit: 1 })
    );
}

#[test]
fn loop_fires_hold_concurrency_slots() {
    let admission = Admission::new(SubagentLimits {
        max_concurrent: 1,
        max_total_agents: 8,
        behavior: LimitBehavior::Fail,
    });
    assert_eq!(
        admission.admit(&request("a"), /*running*/ 0, /*total*/ 0),
        AdmissionDecision::Start
    );

    let mut loop_fire = request("a");
    loop_fire.runtime_overrides.loop_task_id = Some("task-1".to_owned());
    assert_eq!(
        admission.admit(&loop_fire, /*running*/ 0, /*total*/ 0),
        AdmissionDecision::Start
    );
    assert_eq!(
        admission.admit(&loop_fire, /*running*/ 1, /*total*/ 0),
        AdmissionDecision::Reject(AdmissionError::ConcurrentLimitReached { limit: 1 })
    );
}
