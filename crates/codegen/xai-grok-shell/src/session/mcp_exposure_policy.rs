//! Codemode Phase 2: MCP tool exposure policy resolution.
//!
//! Resolves the per-qualified-tool exposure map that routes MCP tools between
//! the direct-call lane and the codemode lane:
//!
//! 1. Protocol meta wins: a tool advertising `_meta.ui.visibility` without
//!    `"model"` is `Hidden` (app-only), regardless of config.
//! 2. Per-server config: `[mcp_servers.<x>] tool_exposure = "direct" |
//!    "deferred" | "codemode_only" | "hidden"` overrides the global mode for
//!    that server's tools.
//! 3. Global `[toolset.codemode] mode`:
//!    - `adaptive` (default): no global trimming; tools without per-server
//!      config keep `Direct`, except `direct_servers` whitelisting in `only`
//!      mode.
//!    - `only`: every local MCP tool becomes `CodemodeOnly` — the model's only
//!      path to MCP tools is a codemode script.
//!    - `off`: historical behavior; per-server exposure config is ignored.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use xai_grok_config::{CodemodeMcpMode, McpToolExposure};
use xai_grok_mcp::servers::McpState;

/// `[toolset.codemode]` routing section: `mode` plus the `direct_servers` hot-tools
/// whitelist (server names that stay Direct even under `mode = "only"`).
struct CodemodeMcpSection {
    mode: CodemodeMcpMode,
    direct_servers: Vec<String>,
}

/// Resolve the `[toolset.codemode]` routing section for the given working
/// directory. Falls back to `Adaptive` with an empty whitelist when no config
/// is loadable (same tolerance as the other free-function config readers).
fn resolve_codemode_mcp_section(cwd: &Path) -> CodemodeMcpSection {
    let root = match xai_grok_config::effective_config::load_effective_config() {
        Ok(root) => root,
        Err(_) => return CodemodeMcpSection { mode: CodemodeMcpMode::default(), direct_servers: Vec::new() },
    };
    let section = root.get("toolset").and_then(|t| t.get("codemode"));
    let mode = section
        .and_then(|c| c.get("mode"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_value::<CodemodeMcpMode>(serde_json::json!(s)).ok())
        .unwrap_or_default();
    let direct_servers = section
        .and_then(|c| c.get("direct_servers"))
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    CodemodeMcpSection { mode, direct_servers }
}

/// Resolve the per-qualified-tool exposure map for this session's MCP state.
///
/// Sync on purpose: called while building a [`SnapshotRefresher`] under a short
/// `agent` borrow; config reads go through the process-cached effective config.
/// Returns `(qualified_name, exposure)` pairs already flattened per tool —
/// server-level overrides expand onto the server's registered tool names.
pub(crate) fn resolve_exposure_overrides(
    _tool_bridge: &crate::tools::bridge::ToolBridge,
    cwd: &Path,
    mcp_state: Arc<tokio::sync::Mutex<McpState>>,
) -> HashMap<String, McpToolExposure> {
    use crate::util::config::get_mcp_server_config_with_project;

    let section = resolve_codemode_mcp_section(cwd);
    if section.mode == CodemodeMcpMode::Off {
        return HashMap::new();
    }
    let mode = section.mode;

    let state = match mcp_state.try_lock() {
        Ok(state) => state,
        // A concurrent holder means an init pass is mutating the map; keep the
        // per-tool entries already recorded on the state and skip expansion.
        Err(_) => return HashMap::new(),
    };

    let mut overrides: HashMap<String, McpToolExposure> = HashMap::new();
    for (server, config) in state
        .configs
        .iter()
        .filter_map(|c| {
            let name = xai_grok_mcp::servers::mcp_server_name(c).to_string();
            get_mcp_server_config_with_project(&name, cwd).map(|cfg| (name, cfg))
        })
    {
        let Some(per_server) = config.tool_exposure else {
            continue;
        };
        let prefix = format!(
            "{}{}",
            server,
            xai_grok_mcp::servers::MCP_TOOL_NAME_DELIMITER
        );
        for name in state.tool_exposures.keys() {
            if name.starts_with(&prefix) {
                overrides.insert(name.clone(), per_server);
            }
        }
    }

    if mode == CodemodeMcpMode::Only {
        // Global demotion: every locally registered MCP tool routes through
        // codemode unless the per-server config, protocol meta, or the
        // `direct_servers` hot-tools whitelist said otherwise.
        let server_of = |qualified: &str| {
            qualified.split_once(xai_grok_mcp::servers::MCP_TOOL_NAME_DELIMITER)
                .map(|(server, _)| server.to_string())
        };
        for name in state.tool_exposures.keys() {
            let whitelisted = server_of(name)
                .is_some_and(|server| section.direct_servers.iter().any(|s| *s == server));
            let exposure = if whitelisted {
                McpToolExposure::Direct
            } else {
                McpToolExposure::CodemodeOnly
            };
            overrides.entry(name.clone()).or_insert(exposure);
        }
    }

    overrides
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol as acp;
    use crate::util::config::get_mcp_server_config_with_project;

    fn mcp_state_with(
        configs: Vec<acp::McpServer>,
        tool_exposures: &[(&str, McpToolExposure)],
    ) -> Arc<tokio::sync::Mutex<McpState>> {
        let mut state = McpState::new(configs);
        for (name, exposure) in tool_exposures {
            state.tool_exposures.insert((*name).to_string(), *exposure);
        }
        Arc::new(tokio::sync::Mutex::new(state))
    }

    fn stdio_server(name: &str) -> acp::McpServer {
        acp::McpServer::Stdio(acp::McpServerStdio::new(name.to_string(), "echo"))
    }

    fn resolve(state: Arc<tokio::sync::Mutex<McpState>>) -> HashMap<String, McpToolExposure> {
        let bridge = crate::tools::bridge::ToolBridge::for_test();
        resolve_exposure_overrides(&bridge, Path::new("."), state)
    }

    #[test]
    fn adaptive_mode_without_config_keeps_state_entries_as_authority() {
        let state = mcp_state_with(
            vec![stdio_server("linear")],
            &[("linear__save_issue", McpToolExposure::CodemodeOnly)],
        );
        // This workspace has no `[toolset.codemode] mode` configured, so the
        // default is adaptive — the resolver emits config-driven overrides
        // only, and the per-tool entry recorded on the state stays the
        // authority that the snapshot merge publishes.
        let resolved = resolve(Arc::clone(&state));
        assert!(
            resolved.is_empty(),
            "adaptive without config emits no overrides; got {resolved:?}"
        );
        assert_eq!(
            state.try_lock().expect("state lock").tool_exposures.get("linear__save_issue"),
            Some(&McpToolExposure::CodemodeOnly)
        );
    }

    #[test]
    fn unconfigured_servers_produce_no_overrides() {
        let state = mcp_state_with(
            vec![stdio_server("linear"), stdio_server("grafana")],
            &[
                ("linear__save_issue", McpToolExposure::Direct),
                ("linear__list_issues", McpToolExposure::Direct),
                ("grafana__SearchDashboards", McpToolExposure::Direct),
            ],
        );
        let resolved = resolve(state);
        assert!(
            resolved.is_empty(),
            "no config written for these servers, so no overrides; got {resolved:?}"
        );
    }

    #[test]
    fn exposure_config_field_roundtrips_from_toml() {
        let root: toml::Value = toml::from_str(
            r#"
[mcp_servers.linear]
command = "echo"
tool_exposure = "codemode_only"
"#,
        )
        .unwrap();
        let servers = xai_grok_config::mcp_servers::parse_mcp_servers_from_toml(&root);
        let config = servers.get("linear").expect("linear loads");
        assert_eq!(config.tool_exposure, Some(McpToolExposure::CodemodeOnly));
        assert_eq!(
            config.tool_exposure.map(|e| e.is_registerable()),
            Some(true)
        );
        let _ = get_mcp_server_config_with_project; // used by resolve_exposure_overrides
    }
}
