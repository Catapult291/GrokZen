use super::support::*;
use super::*;
use crate::session::helpers::tool_input_parsing::RAW_ARGUMENTS_META_KEY;
use agent_client_protocol as acp;
use std::time::Duration;
use xai_grok_tools::implementations::grok_build::read_file::ReadFileTool;
use xai_grok_tools::registry::types::ToolConfig;

/// Live mimo/cline signature: emission stops inside the value of a numeric field.
const RAW_ARGUMENTS: &str = r#"{"target_file": "src/main.rs", "limit": {"#;

async fn actor_with_read_file() -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let (gateway_tx, mut gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let (actor, event_rx) = create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    *actor.agent.borrow_mut() =
        test_agent_with_tools(vec![ToolConfig::for_tool::<ReadFileTool>()]).await;
    (actor, event_rx)
}

async fn prepare_outcome(
    actor: &SessionActor,
    name: &str,
    arguments: &str,
) -> Result<PreparedToolCall, ToolLoop> {
    let call = crate::sampling::types::ToolCallResponse {
        id: "call-1".to_owned(),
        kind: "function".to_owned(),
        function: crate::sampling::types::ToolCallFunction::new(name, arguments),
    };
    let mut deferred = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        actor.prepare_tool_call(call, &mut deferred, None),
    )
    .await
    .expect("prepare_tool_call must not hang (a hang means a permission prompt was issued)")
    .expect("prepare_tool_call must not error")
}

async fn tool_result_text(actor: &SessionActor, call_id: &str) -> String {
    let conv = actor.chat_state_handle.get_conversation().await;
    conv.iter()
        .rev()
        .find_map(|item| match item {
            crate::sampling::ConversationItem::ToolResult(result)
                if result.tool_call_id == call_id =>
            {
                Some(result.content.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool_result for {call_id}"))
}

fn collect_start_updates(
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) -> (Option<acp::ToolCall>, Vec<acp::ToolCallUpdate>) {
    let mut early = None;
    let mut updates = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let SessionEvent::Notification(SessionNotification::Acp(notification)) = event {
            match notification.update {
                acp::SessionUpdate::ToolCall(call) => early = Some(call),
                acp::SessionUpdate::ToolCallUpdate(update) if update.fields.raw_input.is_some() => {
                    updates.push(update);
                }
                _ => {}
            }
        }
    }
    (early, updates)
}

fn early_tool_call(
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) -> Option<acp::ToolCall> {
    collect_start_updates(event_rx).0
}

/// A truncated call must be reported as truncation, and the model's original string
/// must travel in ACP `_meta` instead of being replaced by the repaired object.
#[tokio::test(flavor = "current_thread")]
async fn truncated_arguments_report_and_keep_original_string() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut event_rx) = actor_with_read_file().await;
            let outcome = prepare_outcome(&actor, "read_file", RAW_ARGUMENTS).await;
            assert!(
                matches!(outcome, Err(ToolLoop::ToolParsingError)),
                "truncated arguments that still fail the schema must end the loop with a parsing error"
            );

            let early = early_tool_call(&mut event_rx).expect("early ToolCall registered");
            assert_eq!(
                early
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get(RAW_ARGUMENTS_META_KEY))
                    .and_then(|raw| raw.as_str()),
                Some(RAW_ARGUMENTS),
                "the original argument string must be carried in _meta"
            );

            let text = tool_result_text(&actor, "call-1").await;
            assert!(
                text.contains("truncated or invalid upstream"),
                "the error must name truncation; got:\n{text}"
            );
            assert!(
                text.contains("limit"),
                "the error must name the offending field; got:\n{text}"
            );
            assert!(
                text.contains(RAW_ARGUMENTS),
                "the original string must still be echoed; got:\n{text}"
            );
        })
        .await;
}

/// A repair that passes the schema must still keep the model's original string in
/// `_meta`, so the refined update cannot overwrite it in chat history.
#[tokio::test(flavor = "current_thread")]
async fn successful_repair_keeps_original_string_on_the_refined_update() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut event_rx) = actor_with_read_file().await;
            // The truncated number is dropped; the rest passes the schema.
            let arguments = r#"{"target_file": "src/main.rs", "limit": 5"#;
            let outcome = prepare_outcome(&actor, "read_file", arguments).await;
            assert!(
                matches!(outcome, Ok(_)),
                "a repair that satisfies the schema must prepare the call"
            );

            let (early, updates) = collect_start_updates(&mut event_rx);
            let raw_meta = |meta: Option<&acp::Meta>| {
                meta.and_then(|meta| meta.get(RAW_ARGUMENTS_META_KEY))
                    .and_then(|raw| raw.as_str())
                    .map(str::to_owned)
            };

            let early = early.expect("early ToolCall registered");
            assert_eq!(raw_meta(early.meta.as_ref()), Some(arguments.to_owned()));

            let refined = updates
                .iter()
                .find(|update| raw_meta(update.meta.as_ref()).as_deref() == Some(arguments))
                .expect("the refined ToolCallUpdate must carry the original string");
            assert_eq!(
                refined
                    .fields
                    .raw_input
                    .as_ref()
                    .and_then(|raw| raw.get("target_file"))
                    .and_then(|path| path.as_str()),
                Some("src/main.rs"),
                "the display keeps using the repaired object"
            );
        })
        .await;
}

/// Valid arguments that fail the schema keep the plain parse-error text and send no
/// raw-arguments metadata.
#[tokio::test(flavor = "current_thread")]
async fn schema_error_on_valid_json_is_not_reported_as_truncation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, mut event_rx) = actor_with_read_file().await;
            let arguments = r#"{"target_file": "src/main.rs", "limit": {}}"#;
            let outcome = prepare_outcome(&actor, "read_file", arguments).await;
            assert!(matches!(outcome, Err(ToolLoop::ToolParsingError)));

            let early = early_tool_call(&mut event_rx).expect("early ToolCall registered");
            assert!(
                early
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get(RAW_ARGUMENTS_META_KEY))
                    .is_none(),
                "valid JSON needs no raw-arguments metadata"
            );

            let text = tool_result_text(&actor, "call-1").await;
            assert!(
                !text.contains("truncated or invalid upstream"),
                "valid JSON must not be reported as truncated; got:\n{text}"
            );
        })
        .await;
}
