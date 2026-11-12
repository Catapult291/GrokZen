//! Codemode Phase 2: per-server MCP tool exposure policy.
//!
//! Controls how an MCP server's tools reach the model:
//! - [`McpToolExposure`] — the per-server (and per-tool, via `_meta.ui.visibility`) policy.
//! - [`CodemodeMcpMode`] — the global `[toolset.codemode] mode` switch that decides how much
//!   the exposure policy trims the model-visible tool list.

use serde::{Deserialize, Serialize};

/// How an MCP server's tools are exposed to the model.
///
/// Resolution precedence (see `resolve_tool_exposure` in the shell):
/// protocol meta (`_meta.ui.visibility` without `"model"`) wins as `Hidden`,
/// then the per-server `tool_exposure` config, then the global
/// `[toolset.codemode.exposure]` map, then the `[toolset.codemode]
/// default_exposure` fallback (`direct` | `deferred`), then the global
/// `[toolset.codemode] mode` (`only` demotes everything to codemode-only,
/// `off` ignores all trimming).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolExposure {
    /// Advertised to the model from turn one (default; historical behavior).
    #[default]
    Direct,
    /// Not advertised; a `search_tool` hit activates it for direct calls
    /// (session-scoped `DeclaredMcpTools`).
    Deferred,
    /// Reachable only inside a codemode script (`tools.useTool`); direct
    /// model calls are rejected. Never declared, so `search_tool` hits do
    /// not advertise it either.
    CodemodeOnly,
    /// Fully hidden: not registered, not indexed, not dispatchable.
    Hidden,
}

impl McpToolExposure {
    /// Whether a `search_tool` hit may declare this tool for direct calls.
    /// `Direct` tools are already model-visible; `Deferred` tools are the
    /// activation target: a search hit declares them (session-scoped
    /// `DeclaredMcpTools`). `CodemodeOnly` stays on the codemode lane.
    pub fn is_declareable(self) -> bool {
        matches!(self, Self::Direct | Self::Deferred)
    }

    /// Whether the tool must be registered on the tool bridge at all.
    /// `Hidden` tools stay out of the registry, so they are neither
    /// searchable nor dispatchable.
    pub fn is_registerable(self) -> bool {
        !matches!(self, Self::Hidden)
    }
}

/// Global `[toolset.codemode] mode` switch for MCP tool routing.
///
/// - `adaptive` (default): conservative — per-server `tool_exposure` config
///   applies, whitelisted `direct_servers` stay direct, everything else keeps
///   the historical direct default. No global trimming.
/// - `only`: every local MCP tool routes through codemode (whitelist exempts);
///   the model's only path to MCP tools is a codemode script.
/// - `off`: historical behavior — all trimming disabled, per-server exposure
///   config ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeMcpMode {
    #[default]
    Adaptive,
    Only,
    Off,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_roundtrips_snake_case() {
        assert_eq!(
            serde_json::from_value::<McpToolExposure>(serde_json::json!("codemode_only")).unwrap(),
            McpToolExposure::CodemodeOnly
        );
        assert_eq!(
            serde_json::from_value::<McpToolExposure>(serde_json::json!("direct")).unwrap(),
            McpToolExposure::Direct
        );
        assert_eq!(
            serde_json::to_value(McpToolExposure::Deferred).unwrap(),
            serde_json::json!("deferred")
        );
        assert_eq!(
            serde_json::to_value(McpToolExposure::Hidden).unwrap(),
            serde_json::json!("hidden")
        );
    }

    #[test]
    fn mode_roundtrips_lowercase() {
        assert_eq!(
            serde_json::from_value::<CodemodeMcpMode>(serde_json::json!("only")).unwrap(),
            CodemodeMcpMode::Only
        );
        assert_eq!(
            serde_json::to_value(CodemodeMcpMode::Off).unwrap(),
            serde_json::json!("off")
        );
        assert_eq!(CodemodeMcpMode::default(), CodemodeMcpMode::Adaptive);
    }

    #[test]
    fn hidden_and_codemode_only_are_not_declareable() {
        assert!(McpToolExposure::Direct.is_declareable());
        assert!(McpToolExposure::Deferred.is_declareable());
        assert!(!McpToolExposure::CodemodeOnly.is_declareable());
        assert!(McpToolExposure::Deferred.is_registerable());
        assert!(McpToolExposure::CodemodeOnly.is_registerable());
        assert!(!McpToolExposure::Hidden.is_registerable());
    }
}
