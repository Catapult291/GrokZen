//! Input for the `codemode` meta-tool.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the `codemode` meta-tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CodemodeInput {
    /// A JavaScript program (ES2020) executed in a harness-side sandbox.
    /// Discover tools with `await tools.searchTool({ query, limit })`, call
    /// them with `await tools.useTool(name, input)`, and `return` the final
    /// JSON-safe value. Only that return value re-enters the model context.
    pub code: String,
}
