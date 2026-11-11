//! Types for the `search_tool`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Per-hit verbosity for `search_tool` results — controls discovery token
/// cost on large catalogs. Defaults to [`SearchDetailLevel::Full`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchDetailLevel {
    /// Tool names only — cheapest sweep.
    Name,
    /// Name + short description + parameter names, without the input schema.
    Brief,
    /// Name + description + full input schema (legacy behavior).
    Full,
}

impl Default for SearchDetailLevel {
    fn default() -> Self {
        Self::Full
    }
}

/// Input for the `search_tool` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchToolInput {
    /// Keywords to match against tool names, server names, and descriptions.
    /// Include the server name and action for best results
    /// (e.g. "linear create issue", "slack read thread history").
    pub query: String,
    /// Maximum number of results to return (default 5).
    #[serde(default = "default_limit")]
    pub limit: Option<u8>,
    /// Per-hit verbosity: "name" (names only), "brief" (description +
    /// parameter names, no schema), or "full" (default: full input schema).
    /// Sweep a large catalog cheaply with "name"/"brief", then re-search the
    /// chosen tool with "full" before calling it.
    #[serde(default)]
    pub detail: Option<SearchDetailLevel>,
}

fn default_limit() -> Option<u8> {
    Some(5)
}
