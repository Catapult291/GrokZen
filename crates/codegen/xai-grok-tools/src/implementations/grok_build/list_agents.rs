//! `list_agents` — enumerate this session's subagents without remembering ids.

use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::{SessionIdResource, SubagentAgentEntry};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const LIST_AGENTS_TOOL_NAME: &str = "list_agents";

/// Default page size; also the cap, so one call cannot flood the context.
pub const LIST_AGENTS_LIMIT: usize = 50;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ListAgentsInput {
    /// Include finished subagents. Defaults to true so resumable rows stay visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_finished: Option<bool>,
    /// Restrict the result to descendants of this subagent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Maximum rows to return (1–50). Defaults to 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ListAgentsOutput {
    pub agents: Vec<SubagentAgentEntry>,
}

impl xai_tool_runtime::ToolOutput for ListAgentsOutput {}

#[derive(Debug, Default)]
pub struct ListAgentsTool;

impl crate::types::tool_metadata::ToolMetadata for ListAgentsTool {
    fn kind(&self) -> ToolKind {
        ToolKind::List
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "List the subagents this session spawned, with status, model, tokens, context usage, \
         current tool, queue position, and whether each can be resumed. Use it to find the id \
         to message, interrupt, or resume instead of relying on memory. Finished subagents are \
         included by default."
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool_kind(ToolKind::Task))
    }
}

impl xai_tool_runtime::Tool for ListAgentsTool {
    type Args = ListAgentsInput;
    type Output = ListAgentsOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(LIST_AGENTS_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            LIST_AGENTS_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.list_agents", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ListAgentsInput,
    ) -> Result<ListAgentsOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;
        let (parent_session_id, backend) = {
            let res = resources.lock().await;
            let session_id = res
                .get::<SessionIdResource>()
                .map(|resource| resource.0.clone())
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "missing_resource",
                        "SessionIdResource (subagent support not initialized)",
                    )
                })?;
            let backend = res
                .get::<SubagentBackendResource>()
                .cloned()
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "missing_resource",
                        "SubagentBackendResource (subagent support not initialized)",
                    )
                })?;
            (session_id, backend)
        };

        let limit = input
            .limit
            .filter(|limit| *limit > 0)
            .unwrap_or(LIST_AGENTS_LIMIT)
            .min(LIST_AGENTS_LIMIT);
        let agents = backend
            .backend()
            .list_agents(
                &parent_session_id,
                input.include_finished.unwrap_or(true),
                input.parent_id.as_deref(),
                limit,
            )
            .await;
        Ok(ListAgentsOutput { agents })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::grok_build::task::backend::ChannelBackend;
    use crate::implementations::grok_build::task::types::{
        SubagentAgentStatus, SubagentEvent, SubagentListAgentsRequest,
    };
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx_with_call_id;

    fn entry(id: &str, status: SubagentAgentStatus) -> SubagentAgentEntry {
        SubagentAgentEntry {
            id: id.to_owned(),
            label: Some("label".to_owned()),
            subagent_type: "explore".to_owned(),
            status,
            model: Some("model".to_owned()),
            parent_id: "root".to_owned(),
            depth: 1,
            elapsed_ms: 10,
            tokens_in: 20,
            tokens_out: 5,
            context_pct: 3,
            current_tool: Some("read_file".to_owned()),
            queue_position: None,
            resumable: false,
        }
    }

    #[tokio::test]
    async fn lists_agents_from_the_coordinator_backend() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let mut resources = Resources::new();
        resources.insert(SessionIdResource("root".to_owned()));
        resources.insert(SubagentBackendResource(std::sync::Arc::new(
            ChannelBackend::new(tx),
        )));

        let responder = tokio::spawn(async move {
            let SubagentEvent::ListAgents(SubagentListAgentsRequest {
                parent_session_id,
                include_finished,
                parent_id,
                limit,
                respond_to,
            }) = rx.recv().await.unwrap()
            else {
                panic!("expected ListAgents");
            };
            assert_eq!(parent_session_id, "root");
            assert!(include_finished);
            assert_eq!(parent_id.as_deref(), Some("sub"));
            assert_eq!(limit, 10);
            respond_to
                .send(vec![entry("sub", SubagentAgentStatus::Running)])
                .unwrap();
        });

        let output = xai_tool_runtime::Tool::run(
            &ListAgentsTool,
            test_ctx_with_call_id(resources.into_shared(), "call"),
            ListAgentsInput {
                include_finished: None,
                parent_id: Some("sub".to_owned()),
                limit: Some(10),
            },
        )
        .await
        .unwrap();
        responder.await.unwrap();

        assert_eq!(output.agents.len(), 1);
        assert_eq!(output.agents[0].id, "sub");
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["agents"][0]["type"], "explore");
        assert_eq!(json["agents"][0]["status"], "running");
    }

    #[test]
    fn every_status_serializes_to_its_wire_name() {
        for (status, wire) in [
            (SubagentAgentStatus::Queued, "queued"),
            (SubagentAgentStatus::Running, "running"),
            (SubagentAgentStatus::Idle, "idle"),
            (SubagentAgentStatus::Ready, "ready"),
            (SubagentAgentStatus::Completed, "completed"),
            (SubagentAgentStatus::Failed, "failed"),
            (SubagentAgentStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(wire.to_owned())
            );
            assert_eq!(status.as_str(), wire);
        }
    }

    #[test]
    fn tool_metadata_is_read_only_and_named() {
        let tool = ListAgentsTool;
        assert_eq!(
            xai_tool_runtime::Tool::id(&tool).as_str(),
            LIST_AGENTS_TOOL_NAME
        );
        assert!(xai_tool_runtime::Tool::capabilities(&tool).is_read_only);
        assert_eq!(
            crate::types::tool_metadata::ToolMetadata::kind(&tool),
            ToolKind::List
        );
    }
}
