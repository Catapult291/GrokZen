//! Codemode Phase 2: MCP tool exposure policy resolution.
//!
//! Resolves the per-qualified-tool exposure map that routes MCP tools between
//! the direct-call lane and the codemode lane:
//!
//! 1. Protocol meta wins: a tool advertising `_meta.ui.visibility` without
//!    `"model"` is `Hidden` (app-only), regardless of config.
//! 2. Per-server config: `[mcp_servers.<x>] tool_exposure = "direct" |
//!    "deferred" | "codemode_only" | "hidden"` overrides the global mode for
//!    that server's tools. Only consults TOML configs; compat-source servers
//!    (`~/.claude.json`, `~/.cursor/mcp.json`) never resolve here.
//! 3. Global `[toolset.codemode.exposure]` map: `server name = "deferred"` (or
//!    any exposure string) applies to a server whatever its config source —
//!    the route for compat-source servers like `~/.claude.json` imports.
//! 4. Global `[toolset.codemode] mode`:
//!    - `adaptive` (default): no global trimming; tools without per-server
//!      config keep `Direct`, except `direct_servers` whitelisting in `only`
//!      mode.
//!    - `only`: every local MCP tool becomes `CodemodeOnly` — the model's only
//!      path to MCP tools is a codemode script.
//!    - `off`: historical behavior; per-server exposure config is ignored.
//! 5. Global `[toolset.codemode] default_exposure`: the fallback routing for
//!    registered tools no explicit entry above covers — `"deferred"` keeps
//!    every unconfigured server off the model-visible table until a
//!    `search_tool` hit declares it; `"direct"` reverts to the legacy
//!    turn-one full pre-declaration. The compiled-in default is `deferred`
//!    (pi-style structural default, flipped for the 1.0.28 release): with no
//!    config at all, MCP tools stay off the table until discovered. The
//!    `direct_servers` whitelist stays `Direct` under a `deferred` default,
//!    so hot tools keep their turn-one declaration — the compiled-in
//!    whitelist presets the built-in light search servers (`exa`, `tavily`)
//!    when the config omits the key.

use std::collections::HashMap;
use std::path::Path;

use xai_grok_config::{CodemodeMcpMode, McpToolExposure};
use xai_grok_mcp::servers::McpState;

/// `[toolset.codemode]` routing section: `mode`, the `direct_servers` hot-tools
/// whitelist (server names that stay Direct even under `mode = "only"` and
/// under `default_exposure = "deferred"`), the `exposure` global map
/// (`server name → McpToolExposure`, applied to any server regardless of its
/// config source — TOML, `~/.claude.json` compat, …), and `default_exposure`
/// (the fallback routing for tools no explicit entry covers; compiled-in
/// default is `Deferred` — `"direct"` reverts to the legacy pre-declaration).
pub(crate) struct CodemodeMcpSection {
    mode: CodemodeMcpMode,
    direct_servers: Vec<String>,
    exposure: std::collections::HashMap<String, McpToolExposure>,
    default_exposure: McpToolExposure,
}

/// Compiled-in `direct_servers` whitelist: the built-in light search servers
/// every user touches every turn. Preset so the compiled-in `deferred` default
/// only hides heavyweight servers (jadx / ida …) behind `search_tool`, while
/// web search keeps its zero-tax direct lane. A config that declares
/// `direct_servers` (even `[]`) replaces the preset wholesale.
const DEFAULT_DIRECT_SERVERS: [&str; 2] = ["exa", "tavily"];

/// Resolve the `[toolset.codemode]` routing section for the given working
/// directory. Falls back to `Adaptive` with the preset whitelist and the
/// compiled-in `Deferred` default exposure when no config is loadable (same
/// tolerance as the other free-function config readers).
pub(crate) fn resolve_codemode_mcp_section(cwd: &Path) -> CodemodeMcpSection {
    let empty = CodemodeMcpSection {
        mode: CodemodeMcpMode::default(),
        direct_servers: default_direct_servers(),
        exposure: std::collections::HashMap::new(),
        default_exposure: McpToolExposure::Deferred,
    };
    let root = match xai_grok_config::effective_config::load_effective_config() {
        Ok(root) => root,
        Err(_) => return empty,
    };
    let section = root.get("toolset").and_then(|t| t.get("codemode"));
    let mode = section
        .and_then(|c| c.get("mode"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_value::<CodemodeMcpMode>(serde_json::json!(s)).ok())
        .unwrap_or_default();
    // `direct_servers`: the user's array when the key is present (an explicit
    // `[]` means "defer everything"), the preset whitelist when absent.
    let direct_servers_entry = section.and_then(|c| c.get("direct_servers"));
    let direct_servers = match direct_servers_entry.and_then(|v| v.as_array()) {
        Some(values) => values
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        None => default_direct_servers(),
    };
    // Global exposure map: server name → exposure string. Applies to servers
    // whose per-server `[mcp_servers.<x>]` config is unavailable (e.g. compat
    // sources like `~/.claude.json`, which `get_mcp_server_config_with_project`
    // never consults).
    let exposure = parse_exposure_map(section);
    let default_exposure = parse_default_exposure(section);
    CodemodeMcpSection {
        mode,
        direct_servers,
        exposure,
        default_exposure,
    }
}

/// Parse the `[toolset.codemode.exposure]` table: server name → exposure
/// string. Unknown exposure strings are skipped (the server keeps its default
/// routing) so a typo never hides a whole server.
fn parse_exposure_map(codemode_section: Option<&toml::Value>) -> HashMap<String, McpToolExposure> {
    codemode_section
        .and_then(|c| c.get("exposure"))
        .and_then(|v| v.as_table())
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            let exposure: Option<McpToolExposure> = value
                .as_str()
                .and_then(|s| serde_json::from_value::<McpToolExposure>(serde_json::json!(s)).ok());
            exposure.map(|e| (name.clone(), e))
        })
        .collect()
}

/// Parse the `[toolset.codemode] default_exposure` fallback routing for tools
/// no per-server / global-map entry covers. Only `direct` and `deferred` are
/// accepted; any other value (or absence) falls back to the compiled-in
/// default — `deferred`, so a typo never hides a whole fleet (deferred tools
/// stay registered, searchable and declareable; only the turn-one
/// declaration is held back).
fn parse_default_exposure(codemode_section: Option<&toml::Value>) -> McpToolExposure {
    codemode_section
        .and_then(|c| c.get("default_exposure"))
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "direct" => Some(McpToolExposure::Direct),
            "deferred" => Some(McpToolExposure::Deferred),
            _ => None,
        })
        .unwrap_or(McpToolExposure::Deferred)
}

fn default_direct_servers() -> Vec<String> {
    DEFAULT_DIRECT_SERVERS.iter().map(|s| s.to_string()).collect()
}

/// Expand a resolved `[toolset.codemode]` section onto a locked [`McpState`]'s
/// registered tools. Split out so callers can expand at refresh time — the
/// refresher is built before the init pass registers tools, so a
/// construction-time expansion would always be empty.
pub(crate) fn expand_exposure_overrides(
    section: &CodemodeMcpSection,
    state: &McpState,
    cwd: &Path,
) -> HashMap<String, McpToolExposure> {
    use crate::util::config::get_mcp_server_config_with_project;

    if section.mode == CodemodeMcpMode::Off {
        return HashMap::new();
    }
    let mode = section.mode;
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

    // Global `[toolset.codemode.exposure]` map: applies by server name to any
    // registered tool, including servers whose per-server config is
    // unavailable (compat sources like `~/.claude.json`). Per-server config
    // (step 2) takes precedence, so these entries only fill the gaps.
    if !section.exposure.is_empty() {
        for name in state.tool_exposures.keys() {
            let server = name
                .split_once(xai_grok_mcp::servers::MCP_TOOL_NAME_DELIMITER)
                .map(|(server, _)| server);
            if let Some(exposure) = server.and_then(|server| section.exposure.get(server)) {
                overrides.entry(name.clone()).or_insert(*exposure);
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

    // Default exposure fill: the routing for registered tools no explicit
    // entry above covers (per-server `tool_exposure` > global exposure map >
    // `default_exposure` > the registered base). The compiled-in default is
    // `deferred` (pi-style structural default); `default_exposure = "direct"`
    // in config reverts to the legacy turn-one full pre-declaration.
    // `direct_servers` — preset to `exa`/`tavily` when unconfigured — keeps
    // hot tools `Direct` under a `deferred` default. `mode = "off"` already
    // returned above: no trimming.
    if section.default_exposure != McpToolExposure::Direct {
        for name in state.tool_exposures.keys() {
            if overrides.contains_key(name) {
                continue;
            }
            let whitelisted = name
                .split_once(xai_grok_mcp::servers::MCP_TOOL_NAME_DELIMITER)
                .map(|(server, _)| server)
                .is_some_and(|server| section.direct_servers.iter().any(|s| s == server));
            overrides.insert(
                name.clone(),
                if whitelisted {
                    McpToolExposure::Direct
                } else {
                    section.default_exposure
                },
            );
        }
    }

    overrides
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol as acp;
    use crate::util::config::get_mcp_server_config_with_project;
    use std::sync::Arc;

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
        // Mirrors the production path: resolve the section from config, then
        // expand against the (already populated) registration state.
        let section = resolve_codemode_mcp_section(Path::new("."));
        expand_exposure_overrides(&section, &state.try_lock().expect("state lock"), Path::new("."))
    }

    #[test]
    fn adaptive_mode_without_config_falls_to_compiled_deferred_default() {
        let state = mcp_state_with(
            vec![stdio_server("linear")],
            &[("linear__save_issue", McpToolExposure::Direct)],
        );
        // This workspace has no `[toolset.codemode]` configured at all, so the
        // compiled-in default applies: `deferred` — the tool stays registered
        // and declareable, only its turn-one declaration is held back.
        let resolved = resolve(Arc::clone(&state));
        assert_eq!(
            resolved.get("linear__save_issue"),
            Some(&McpToolExposure::Deferred),
            "compiled-in default defers unconfigured tools; got {resolved:?}"
        );
    }

    #[test]
    fn unconfigured_servers_fall_to_compiled_deferred_default() {
        let state = mcp_state_with(
            vec![stdio_server("linear"), stdio_server("grafana")],
            &[
                ("linear__save_issue", McpToolExposure::Direct),
                ("linear__list_issues", McpToolExposure::Direct),
                ("grafana__SearchDashboards", McpToolExposure::Direct),
            ],
        );
        let resolved = resolve(state);
        // No config written for these servers: the compiled-in default
        // (`deferred` + preset `exa`/`tavily` whitelist, which neither server
        // belongs to) defers them all.
        assert_eq!(resolved.get("linear__save_issue"), Some(&McpToolExposure::Deferred));
        assert_eq!(resolved.get("linear__list_issues"), Some(&McpToolExposure::Deferred));
        assert_eq!(
            resolved.get("grafana__SearchDashboards"),
            Some(&McpToolExposure::Deferred)
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

    fn test_section(exposure: &[(&str, McpToolExposure)]) -> CodemodeMcpSection {
        CodemodeMcpSection {
            mode: CodemodeMcpMode::Adaptive,
            direct_servers: Vec::new(),
            exposure: exposure
                .iter()
                .map(|(name, e)| ((*name).to_string(), *e))
                .collect(),
            default_exposure: McpToolExposure::Direct,
        }
    }

    #[test]
    fn global_exposure_map_applies_to_compat_source_servers() {
        // `ida-multi-mcp` has no `[mcp_servers.<x>]` TOML config (its definition
        // lives in `~/.claude.json`), so step 2 cannot reach it; the global
        // `[toolset.codemode.exposure]` map fills the gap.
        let state = mcp_state_with(
            vec![stdio_server("ida-multi-mcp")],
            &[("ida-multi-mcp__status", McpToolExposure::Direct)],
        );
        let section = test_section(&[("ida-multi-mcp", McpToolExposure::Deferred)]);
        let resolved = expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert_eq!(
            resolved.get("ida-multi-mcp__status"),
            Some(&McpToolExposure::Deferred)
        );
    }

    #[test]
    fn global_exposure_map_applies_when_per_server_toml_config_absent() {
        let mut state = McpState::new(vec![stdio_server("jadx-headless-mcp")]);
        state
            .tool_exposures
            .insert("jadx-headless-mcp__status".to_string(), McpToolExposure::Direct);
        let section = test_section(&[("jadx-headless-mcp", McpToolExposure::CodemodeOnly)]);
        // No `[mcp_servers.jadx-headless-mcp]` in TOML here, so the global map
        // (step 3) is what routes the server.
        let resolved = expand_exposure_overrides(&section, &state, Path::new("."));
        assert_eq!(
            resolved.get("jadx-headless-mcp__status"),
            Some(&McpToolExposure::CodemodeOnly)
        );
    }

    #[test]
    fn global_exposure_map_ignores_unknown_servers_and_invalid_values() {
        let state = mcp_state_with(
            vec![stdio_server("linear")],
            &[("linear__save_issue", McpToolExposure::Direct)],
        );
        let section = test_section(&[("grafana", McpToolExposure::Deferred)]);
        let resolved = expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert!(
            resolved.is_empty(),
            "unrelated server entries must not leak: {resolved:?}"
        );
    }

    #[test]
    fn exposure_section_parses_from_toml_table() {
        let root: toml::Value = toml::from_str(
            r#"
[toolset.codemode.exposure]
ida-multi-mcp = "deferred"
tavily = "bogus"
"#,
        )
        .unwrap();
        let codemode_section = root.get("toolset").and_then(|t| t.get("codemode"));
        let exposure = parse_exposure_map(codemode_section);
        assert_eq!(
            exposure.get("ida-multi-mcp"),
            Some(&McpToolExposure::Deferred)
        );
        assert!(!exposure.contains_key("tavily"), "unknown value must be skipped");
    }

    // ---- Phase 1: `[toolset.codemode] default_exposure` ----

    #[test]
    fn default_exposure_parses_direct_deferred_and_rejects_other_values() {
        let root: toml::Value = toml::from_str(
            r#"
[toolset.codemode]
default_exposure = "deferred"
"#,
        )
        .unwrap();
        let codemode_section = root.get("toolset").and_then(|t| t.get("codemode"));
        assert_eq!(parse_default_exposure(codemode_section), McpToolExposure::Deferred);

        // `codemode_only` / `hidden` are not legal fallbacks — a typo must
        // never hide a whole fleet, so anything but direct|deferred falls
        // back to the compiled-in default (`deferred`; deferred tools stay
        // registered, searchable and declareable).
        let bogus: toml::Value = toml::from_str(
            r#"
[toolset.codemode]
default_exposure = "codemode_only"
"#,
        )
        .unwrap();
        let codemode_section = bogus.get("toolset").and_then(|t| t.get("codemode"));
        assert_eq!(parse_default_exposure(codemode_section), McpToolExposure::Deferred);

        // Absent key → compiled-in default (`deferred`, pi-style).
        assert_eq!(parse_default_exposure(None), McpToolExposure::Deferred);
    }

    #[test]
    fn compiled_in_default_defers_uncovered_tools_and_presets_whitelist() {
        // The compiled-in default is `deferred` (pi-style structural default),
        // and the compiled-in `direct_servers` whitelist presets the built-in
        // light search servers so web search keeps its zero-tax direct lane.
        let state = mcp_state_with(
            vec![stdio_server("tavily"), stdio_server("exa"), stdio_server("ida-multi-mcp")],
            &[
                ("tavily__search", McpToolExposure::Direct),
                ("exa__web_search_exa", McpToolExposure::Direct),
                ("ida-multi-mcp__status", McpToolExposure::Direct),
            ],
        );
        let mut section = test_section(&[]);
        section.default_exposure = McpToolExposure::Deferred;
        section.direct_servers = default_direct_servers();
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert_eq!(
            resolved.get("tavily__search"),
            Some(&McpToolExposure::Direct),
            "preset-whitelisted hot server must stay direct"
        );
        assert_eq!(
            resolved.get("exa__web_search_exa"),
            Some(&McpToolExposure::Direct),
            "preset-whitelisted hot server must stay direct"
        );
        assert_eq!(
            resolved.get("ida-multi-mcp__status"),
            Some(&McpToolExposure::Deferred),
            "heavyweight server falls to the deferred default"
        );
    }

    #[test]
    fn explicit_direct_servers_override_replaces_preset_wholesale() {
        // An explicit `direct_servers` (even `[]`) replaces the preset
        // whitelist entirely — the user owns the lane assignment.
        let state = mcp_state_with(
            vec![stdio_server("tavily"), stdio_server("ida-multi-mcp")],
            &[
                ("tavily__search", McpToolExposure::Direct),
                ("ida-multi-mcp__status", McpToolExposure::Direct),
            ],
        );
        let mut section = test_section(&[]);
        section.default_exposure = McpToolExposure::Deferred;
        section.direct_servers = Vec::new();
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert_eq!(
            resolved.get("tavily__search"),
            Some(&McpToolExposure::Deferred),
            "explicit empty whitelist defers even hot servers"
        );
    }

    #[test]
    fn default_exposure_deferred_fills_uncovered_registered_tools() {
        let state = mcp_state_with(
            vec![stdio_server("ida-multi-mcp"), stdio_server("tavily")],
            &[
                ("ida-multi-mcp__status", McpToolExposure::Direct),
                ("tavily__search", McpToolExposure::Direct),
            ],
        );
        let mut section = test_section(&[]);
        section.default_exposure = McpToolExposure::Deferred;
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert_eq!(
            resolved.get("ida-multi-mcp__status"),
            Some(&McpToolExposure::Deferred)
        );
        assert_eq!(
            resolved.get("tavily__search"),
            Some(&McpToolExposure::Deferred)
        );
    }

    #[test]
    fn direct_servers_whitelist_stays_direct_under_default_deferred() {
        // Same lesson as the Phase 2 adaptive deviation: hot tools whitelisted
        // in `direct_servers` keep their turn-one declaration when the default
        // flips to `deferred`.
        let state = mcp_state_with(
            vec![stdio_server("tavily"), stdio_server("ida-multi-mcp")],
            &[
                ("tavily__search", McpToolExposure::Direct),
                ("ida-multi-mcp__status", McpToolExposure::Direct),
            ],
        );
        let mut section = test_section(&[]);
        section.default_exposure = McpToolExposure::Deferred;
        section.direct_servers = vec!["tavily".to_string()];
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert_eq!(
            resolved.get("tavily__search"),
            Some(&McpToolExposure::Direct),
            "whitelisted hot server must stay direct"
        );
        assert_eq!(
            resolved.get("ida-multi-mcp__status"),
            Some(&McpToolExposure::Deferred)
        );
    }

    #[test]
    fn global_exposure_map_beats_default_exposure() {
        let state = mcp_state_with(
            vec![stdio_server("linear")],
            &[("linear__save_issue", McpToolExposure::Direct)],
        );
        let mut section = test_section(&[("linear", McpToolExposure::CodemodeOnly)]);
        section.default_exposure = McpToolExposure::Deferred;
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert_eq!(
            resolved.get("linear__save_issue"),
            Some(&McpToolExposure::CodemodeOnly),
            "explicit global-map entry outranks the default fill"
        );
    }

    #[test]
    fn per_server_tool_exposure_beats_default_exposure() {
        // Per-server `[mcp_servers.<x>] tool_exposure` is the top of the
        // priority chain: it outranks `[toolset.codemode] default_exposure`.
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".grok")).unwrap();
        std::fs::write(
            project.path().join(".grok/config.toml"),
            "[mcp_servers.linear]\ncommand = \"echo\"\ntool_exposure = \"codemode_only\"\n",
        )
        .unwrap();
        let state = mcp_state_with(
            vec![stdio_server("linear")],
            &[("linear__save_issue", McpToolExposure::Direct)],
        );
        let mut section = test_section(&[]);
        section.default_exposure = McpToolExposure::Deferred;
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), project.path());
        assert_eq!(
            resolved.get("linear__save_issue"),
            Some(&McpToolExposure::CodemodeOnly),
            "per-server tool_exposure outranks the default fill"
        );
    }

    #[test]
    fn default_exposure_ignored_under_off_mode() {
        // `mode = "off"` disables all trimming, including the default fill.
        let state = mcp_state_with(
            vec![stdio_server("linear")],
            &[("linear__save_issue", McpToolExposure::Direct)],
        );
        let mut section = test_section(&[]);
        section.mode = CodemodeMcpMode::Off;
        section.default_exposure = McpToolExposure::Deferred;
        let resolved =
            expand_exposure_overrides(&section, &state.try_lock().unwrap(), Path::new("."));
        assert!(
            resolved.is_empty(),
            "`off` must ignore default_exposure too; got {resolved:?}"
        );
    }
}
