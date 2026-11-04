//! Same-batch tool-call dedup: identical `(name, arguments)` calls execute once.
//!
//! A dropped call never gets a ToolCall update, so the UI shows only calls that run, but it still
//! gets a paired `tool_result` because the assistant item carries its id.

use super::support::*;
use super::*;
use agent_client_protocol as acp;
use xai_grok_tools::registry::types::ToolConfig;

fn read_file_call(id: &str, target: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(
            "read_file",
            serde_json::json!({ "target_file": target }).to_string(),
        ),
    }
}

pub(super) fn drain_tool_call_statuses(
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) -> Vec<(String, acp::ToolCallStatus)> {
    let mut statuses = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        let SessionEvent::Notification(notification) = event else {
            continue;
        };
        let Some(acp_n) = (match notification {
            SessionNotification::Acp(n) => Some(*n),
            SessionNotification::Xai(_) => None,
        }) else {
            continue;
        };
        match acp_n.update {
            acp::SessionUpdate::ToolCall(tc) => {
                statuses.push((tc.tool_call_id.0.as_ref().to_string(), tc.status));
            }
            acp::SessionUpdate::ToolCallUpdate(upd) => {
                if let Some(status) = upd.fields.status {
                    statuses.push((upd.tool_call_id.0.as_ref().to_string(), status));
                }
            }
            _ => {}
        }
    }
    statuses
}

async fn tool_result_text(actor: &SessionActor, call_id: &str) -> String {
    let conv = actor.chat_state_handle.get_conversation().await;
    conv.iter()
        .rev()
        .find_map(|item| match item {
            xai_grok_sampling_types::ConversationItem::ToolResult(tr)
                if tr.tool_call_id == call_id =>
            {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool_result for {call_id} in {conv:?}"))
}

#[tokio::test(flavor = "current_thread")]
async fn identical_calls_in_one_batch_execute_once() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let (actor, mut event_rx) =
                create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.agent.borrow_mut() =
                test_agent_with_tools(vec![ToolConfig::from_id("GrokBuild:read_file")]).await;

            let batch = vec![
                read_file_call("dup_1", "/tmp/grok-zen-dedup-a.txt"),
                read_file_call("dup_2", "/tmp/grok-zen-dedup-a.txt"),
                read_file_call("dup_3", "/tmp/grok-zen-dedup-a.txt"),
                read_file_call("other", "/tmp/grok-zen-dedup-b.txt"),
            ];

            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                actor.execute_tool_calls(batch, None),
            )
            .await
            .expect("execute_tool_calls must not hang")
            .expect("execute_tool_calls must not error");

            let statuses = drain_tool_call_statuses(&mut event_rx);
            let started: Vec<&str> = statuses
                .iter()
                .filter(|(_, status)| *status == acp::ToolCallStatus::Pending)
                .map(|(id, _)| id.as_str())
                .collect();
            assert_eq!(
                started,
                vec!["dup_1", "other"],
                "only the first duplicate and the distinct call may reach execution: {statuses:?}"
            );

            for id in ["dup_2", "dup_3"] {
                let text = tool_result_text(&actor, id).await;
                assert!(
                    text.contains("Duplicate tool call skipped") && text.contains("dup_1"),
                    "dropped call {id} must still be answered: {text}"
                );
            }
            let first = tool_result_text(&actor, "dup_1").await;
            assert!(
                !first.contains("Duplicate tool call skipped"),
                "first occurrence must execute: {first}"
            );
            let other = tool_result_text(&actor, "other").await;
            assert!(
                !other.contains("Duplicate tool call skipped"),
                "distinct arguments must execute: {other}"
            );
        })
        .await;
}
