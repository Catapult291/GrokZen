//! Side-effect tool execution gate: an identical side-effect call shortly after an earlier one
//! is skipped instead of re-executing (TOOL-CALL-DUP-GROK-BUILD.md 方法 3).

use super::support::*;
use super::*;
use agent_client_protocol as acp;
use xai_grok_tools::registry::types::ToolConfig;

const GATE_PROBE_COMMAND: &str = "echo grokzen-side-effect-gate-probe";

fn terminal_call(id: &str, command: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(
            "run_terminal_cmd",
            serde_json::json!({ "command": command }).to_string(),
        ),
    }
}

fn background_disabled_terminal_tool() -> ToolConfig {
    // Disable background bash so finalize does not require the get_task_output / kill_task
    // companion tools.
    ToolConfig {
        id: "GrokBuild:run_terminal_cmd".into(),
        params: Some(
            serde_json::from_value(serde_json::json!({
                "enabled_background": false
            }))
            .unwrap(),
        ),
        name_override: None,
        params_name_overrides: None,
        description_override: None,
        behavior_version: None,
        kind: None,
    }
}

fn pending_call_ids(statuses: &[(String, acp::ToolCallStatus)]) -> Vec<&str> {
    statuses
        .iter()
        .filter(|(_, status)| *status == acp::ToolCallStatus::Pending)
        .map(|(id, _)| id.as_str())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn identical_side_effect_call_in_next_batch_is_gated() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let (mut actor, mut event_rx) =
                create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.agent.borrow_mut() =
                test_agent_with_tools(vec![background_disabled_terminal_tool()]).await;

            let first = vec![terminal_call("run_1", GATE_PROBE_COMMAND)];
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                actor.execute_tool_calls(first, None),
            )
            .await
            .expect("first batch must not hang")
            .expect("first batch must not error");

            let statuses = super::tool_call_dedup_tests::drain_tool_call_statuses(&mut event_rx);
            assert_eq!(
                pending_call_ids(&statuses),
                vec!["run_1"],
                "first identical call must execute: {statuses:?}"
            );

            let second = vec![
                terminal_call("run_2", GATE_PROBE_COMMAND),
                terminal_call("run_3", "echo grokzen-side-effect-gate-other"),
            ];
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                actor.execute_tool_calls(second, None),
            )
            .await
            .expect("second batch must not hang")
            .expect("second batch must not error");

            let statuses = super::tool_call_dedup_tests::drain_tool_call_statuses(&mut event_rx);
            let all_ids: Vec<&str> = statuses.iter().map(|(id, _)| id.as_str()).collect();
            assert_eq!(
                pending_call_ids(&statuses),
                vec!["run_3"],
                "only the distinct call may execute; the identical side-effect call is gated and \
                 never dispatches: {statuses:?}"
            );
            assert!(
                !all_ids.contains(&"run_2"),
                "gated call must not emit any ToolCall update: {statuses:?}"
            );
        })
        .await;
}
