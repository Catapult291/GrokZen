//! `codemode` — run a sandboxed JS program that orchestrates multiple tool
//! calls (search / use) on the harness side. Intermediate tool JSON stays in
//! the sandbox; only the script's final JSON-safe return value re-enters the
//! model context.

mod input;
pub use input::CodemodeInput;

use std::sync::{Arc, Mutex};

use xai_grok_codemode::{HostCallError, HostFuture, HostTools, Limits, execute};
use xai_tool_runtime::{Tool as _, ToolCallContext};

use crate::types::output::{TextOutput, ToolOutput};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_index::ToolIndex;

/// Wire name of the codemode meta-tool.
pub const CODEMODE_NAME: &str = "codemode";

/// User-configurable sandbox limits (`[toolset.codemode]` in config.toml).
/// Every field is optional; `None` keeps the engine default from
/// [`Limits::default`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CodemodeParams {
    /// Wall-clock deadline for one script run (seconds). Default 30.
    pub timeout_secs: Option<u64>,
    /// QuickJS memory cap (MiB). Default 64.
    pub memory_mb: Option<usize>,
    /// Maximum `searchTool` + `useTool` calls per script. Default 64.
    pub max_host_calls: Option<usize>,
    /// Maximum concurrent host calls (Promise.all fan-out). Default 4.
    pub max_concurrency: Option<usize>,
    /// Return-value budget (KiB); larger values are truncated. Default 256.
    pub max_return_kb: Option<usize>,
}

impl CodemodeParams {
    /// Overlay configured values onto the engine defaults.
    pub fn to_limits(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            timeout: std::time::Duration::from_secs(
                self.timeout_secs.unwrap_or(defaults.timeout.as_secs()),
            ),
            memory_bytes: self
                .memory_mb
                .unwrap_or(defaults.memory_bytes / (1024 * 1024))
                * (1024 * 1024),
            max_stack_bytes: defaults.max_stack_bytes,
            max_host_calls: self.max_host_calls.unwrap_or(defaults.max_host_calls),
            max_concurrency: self.max_concurrency.unwrap_or(defaults.max_concurrency),
            max_return_bytes: self
                .max_return_kb
                .unwrap_or(defaults.max_return_bytes / 1024)
                * 1024,
            max_console_lines: defaults.max_console_lines,
        }
    }
}

crate::register_resource!("grok_build", "Codemode", CodemodeParams);

/// Host binding backed by the session's tool index and `use_tool` dispatch
/// (local MCP via `InnerDispatch`, managed gateway via catalog fallback).
struct CodemodeHost {
    ctx: ToolCallContext,
    index: Option<ToolIndex>,
    /// Every `useTool` target the script dispatched (audit trail; surfaced as
    /// `toolsCalled` in the tool result).
    tools_called: Mutex<Vec<String>>,
}

fn host_error_from_tool(err: xai_tool_runtime::ToolError) -> HostCallError {
    // Preserve the original custom code (e.g. `permission_denied`,
    // `not_found`) when present; otherwise fall back to the kind name.
    let code = err
        .details
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| err.variant_name().to_string());
    HostCallError::new(code, err.detail.clone())
}

impl HostTools for CodemodeHost {
    fn servers(&self) -> serde_json::Value {
        let summaries = self
            .index
            .as_ref()
            .map(|idx| idx.0.list_server_summaries())
            .unwrap_or_default();
        let servers: Vec<serde_json::Value> = summaries
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "description": s.description,
                    "toolCount": s.tool_count,
                })
            })
            .collect();
        serde_json::Value::Array(servers)
    }

    fn search(
        &self,
        query: String,
        limit: usize,
    ) -> HostFuture<Result<serde_json::Value, HostCallError>> {
        let ctx = self.ctx.clone();
        Box::pin(async move {
            let input = crate::implementations::search_tool::SearchToolInput {
                query,
                limit: Some(limit.clamp(1, u8::MAX as usize) as u8),
            };
            let output = crate::implementations::search_tool::SearchTool
                .run(ctx, input)
                .await
                .map_err(host_error_from_tool)?;
            match output {
                ToolOutput::SearchTool(o) => serde_json::from_str(&o.content).map_err(|e| {
                    HostCallError::new("codemode_host_error", format!("search output decode: {e}"))
                }),
                other => Ok(serde_json::json!({
                    "text": other.to_prompt_format(),
                    "isError": other.is_error(),
                })),
            }
        })
    }

    fn use_tool(
        &self,
        tool_name: String,
        input: serde_json::Value,
    ) -> HostFuture<Result<serde_json::Value, HostCallError>> {
        if let Ok(mut called) = self.tools_called.lock() {
            called.push(tool_name.clone());
        }
        let ctx = self.ctx.clone();
        Box::pin(async move {
            // Same distribution semantics as `use_tool`: local MCP via
            // InnerDispatch (skips reminder/persistence — the outer codemode
            // call does one post-processing pass), managed-gateway fallback
            // preserved.
            let output = crate::implementations::use_tool::dispatch_mcp_tool(
                &ctx, &tool_name, input, "codemode",
            )
            .await
            .map_err(host_error_from_tool)?;
            Ok(serde_json::json!({
                "text": output.to_prompt_format(),
                "isError": output.is_error(),
            }))
        })
    }
}

/// Meta tool that executes a model-written JavaScript program in an
/// in-process QuickJS sandbox, orchestrating `searchTool`/`useTool` calls
/// harness-side.
#[derive(Debug, Default)]
pub struct Codemode;

impl crate::types::tool_metadata::ToolMetadata for Codemode {
    fn kind(&self) -> ToolKind {
        ToolKind::Codemode
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Run a sandboxed JavaScript program that orchestrates multiple tool calls on the \
         harness side; only the script's final returned JSON comes back into context.\n\n\
         Use this for multi-step work that would otherwise take many rounds of \
         `use_tool` calls (discover + N MCP calls + post-process). \
         Do NOT use codemode for a single simple call — a direct `use_tool` call is cheaper.\n\n\
         Sandbox globals:\n\
         - `await host.servers()` — connected MCP servers: `[{ name, description, toolCount }]`\n\
         - `await tools.searchTool({ query, limit })` — discover tools + input schemas \
         (same shape as `search_tool`)\n\
         - `await tools.useTool(name, input)` — call a discovered tool (`{ text, isError }`)\n\
         Also available: `Promise.all`, `console.log` (debug only), JSON/Math/Array. \
         No network, no filesystem, 30s timeout, 64 host-call budget.\n\n\
         Example:\n\
         ```js\n\
         const found = await tools.searchTool({ query: \"sentry issues\", limit: 5 });\n\
         const orgs = await tools.useTool(\"sentry__find_organizations\", {});\n\
         return { found, orgs };\n\
         ```"
    }
}

impl xai_tool_runtime::Tool for Codemode {
    type Args = CodemodeInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(CODEMODE_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            CODEMODE_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            // Scripts can perform writes through `useTool` — treat like `use_tool`.
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: CodemodeInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;

        let index = match shared_resources(&ctx) {
            Ok(resources) => resources.lock().await.get::<ToolIndex>().cloned(),
            Err(_) => None,
        };
        let limits = match shared_resources(&ctx) {
            Ok(resources) => resources
                .lock()
                .await
                .get::<crate::types::resources::Params<CodemodeParams>>()
                .map(|p| p.0.to_limits())
                .unwrap_or_default(),
            Err(_) => Limits::default(),
        };

        let host = Arc::new(CodemodeHost {
            ctx,
            index,
            tools_called: Mutex::new(Vec::new()),
        });
        let outcome = execute(&input.code, host.clone(), &limits)
            .await
            .map_err(|e| xai_tool_runtime::ToolError::custom(e.code.clone(), e.message.clone()))?;

        let tools_called = host
            .tools_called
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        tracing::info!(
            host_call_count = outcome.host_calls,
            wall_ms = outcome.wall_ms,
            return_bytes = serde_json::to_string(&outcome.value).map(|s| s.len()).unwrap_or(0),
            tools_called = %tools_called.join(","),
            truncated = outcome.truncated,
            "codemode.execute"
        );
        if !outcome.console.is_empty() {
            tracing::debug!(
                lines = outcome.console.len(),
                console = %outcome.console.join("\n"),
                "codemode.console"
            );
        }

        let envelope = serde_json::json!({
            "result": outcome.value,
            "toolsCalled": tools_called,
            "truncated": outcome.truncated,
        });
        Ok(ToolOutput::Text(TextOutput {
            text: serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string()),
            consumed_completion_task_id: None,
        }))
    }
}

#[cfg(test)]
#[path = "codemode_tests.rs"]
mod tests;
