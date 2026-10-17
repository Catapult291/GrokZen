//! Host services injected into the sandbox.

use std::future::Future;
use std::pin::Pin;

/// Future returned by async host calls.
pub type HostFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Failure of a single host call. `code` is the original tool error code when
/// one exists (e.g. `permission_denied`, `not_found`), so the model can tell
/// an MCP rejection from a sandbox failure. `message` is human-readable.
#[derive(Debug, Clone)]
pub struct HostCallError {
    pub code: String,
    pub message: String,
}

impl HostCallError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Harness-side services exposed to sandbox scripts as the `host` and `tools`
/// globals. Implementations own their audit state (e.g. `toolsCalled`).
///
/// All methods are `&self`; per-run state belongs in the implementation
/// (typically behind a mutex), and one instance is used for one script run.
pub trait HostTools: Send + Sync + 'static {
    /// Connected MCP server summaries as a JSON array
    /// `[{ name, description, toolCount }]`.
    fn servers(&self) -> serde_json::Value;

    /// Discovery, same shape as `search_tool`'s model-visible output:
    /// `{ results: [{ server, tools }], total_hidden_tools, status, note }`.
    fn search(&self, query: String, limit: usize) -> HostFuture<Result<serde_json::Value, HostCallError>>;

    /// Dispatch any discovered tool (local MCP or managed gateway), same
    /// distribution semantics as `use_tool` (single post-processing pass;
    /// no reminder/persistence double-run).
    fn use_tool(
        &self,
        tool_name: String,
        input: serde_json::Value,
    ) -> HostFuture<Result<serde_json::Value, HostCallError>>;
}
