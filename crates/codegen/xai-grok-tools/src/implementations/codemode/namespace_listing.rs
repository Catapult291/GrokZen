//! Codemode Phase 2: namespace listing for the model-visible description.
//!
//! pi-style structural default: the codemode description carries a compact
//! per-server listing of the currently registered MCP tools under a shared
//! token budget (`[toolset.codemode] inline_budget`, ~4 bytes per token).
//! Servers whose full listing does not fit are summarized as one compact
//! line (name + tool count + in-script discovery hint); servers that do not
//! even fit that are aggregated into a single tail line. With no MCP
//! servers registered the description stays byte-identical to the static
//! template.

/// One tool inside a namespace listing: bare tool name + compact summary.
#[derive(Debug, Clone)]
pub struct CatalogTool {
    /// Tool name without the server prefix (e.g. `tavily_search`).
    pub name: String,
    /// `(params): first sentence` summary, or just `()` when the schema has
    /// no properties and the description is empty.
    pub summary: String,
}

/// One MCP server's entry in the namespace listing.
#[derive(Debug, Clone)]
pub struct CatalogServer {
    /// Server name (the client_name prefix before `__`).
    pub name: String,
    /// Registered tools in registration order.
    pub tools: Vec<CatalogTool>,
}

/// Budget header: explains what the listing is and how to discover more
/// in-script. Information stays reachable even when the budget is exhausted.
const HEADER: &str = "\n\nConnected MCP namespaces (in-script discovery: `await tools.searchTool({ query: \"<server>\" })`; full catalog: `await host.servers()`):\n";

/// How many bytes one estimated token counts for (`~4 chars/token`).
pub const BYTES_PER_TOKEN: usize = 4;

/// First sentence of a tool description, hard-capped so one verbose tool
/// cannot eat the shared budget. Cuts on char boundaries.
fn first_sentence(description: Option<&str>) -> String {
    let raw = description.unwrap_or("").trim();
    if raw.is_empty() {
        return String::new();
    }
    let line = raw.split('\n').next().unwrap_or(raw).trim();
    const CAP: usize = 96;
    let mut cut = CAP.min(line.len());
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut sentence = line[..cut].to_string();
    if cut < line.len() {
        sentence.push('…');
    }
    sentence
}

/// Parameter names from a tool's input schema (`properties` keys), optional
/// ones suffixed with `?`, capped at 8 with an ellipsis marker.
fn param_names(parameters: &serde_json::Value) -> Vec<String> {
    const CAP: usize = 8;
    let Some(props) = parameters
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let required: std::collections::HashSet<&str> = parameters
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<std::collections::HashSet<&str>>()
        })
        .unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for (i, name) in props.keys().enumerate() {
        if i == CAP {
            names.push("…".to_string());
            break;
        }
        if required.contains(name.as_str()) {
            names.push(name.clone());
        } else {
            names.push(format!("{name}?"));
        }
    }
    names
}

/// `name(param, opt?): summary` summary for one catalog tool.
pub fn tool_summary_line(tool: &CatalogTool) -> String {
    if tool.summary.is_empty() {
        format!("  - {}", tool.name)
    } else {
        format!("  - {}", tool.summary)
    }
}
/// Compact one-line entry for a server whose full listing did not fit.
fn overflow_line(server: &CatalogServer) -> String {
    format!(
        "  - {} ({} tools) — discover in-script: await tools.searchTool({{ query: \"{}\" }})",
        server.name,
        server.tools.len(),
        server.name
    )
}

/// Full per-server block: header line + one summary line per tool.
fn server_block(server: &CatalogServer) -> String {
    let mut block = format!("{} ({} tools):", server.name, server.tools.len());
    for tool in &server.tools {
        block.push('\n');
        block.push_str(&tool_summary_line(tool));
    }
    block
}

/// Byte allowance reserved for the aggregate tail line ("… and N more
/// servers"), so the safety net is never itself cut by exact budget
/// exhaustion. Chunk fitting runs against `remaining - TAIL_RESERVE`.
const TAIL_RESERVE: usize = 72;

/// Render `base` (the static, sanitized codemode description) plus the
/// namespace listing for `servers`, keeping the total within `budget_bytes`
/// (token budget × [`BYTES_PER_TOKEN`]). Servers are listed in the given
/// order; the first that does not fit is demoted to a compact overflow
/// line; servers that do not even fit that are aggregated into one tail
/// line pointing at `host.servers()`.
pub fn render_namespace_listing(
    base: &str,
    servers: &[CatalogServer],
    budget_bytes: usize,
) -> String {
    if servers.is_empty() {
        return base.to_owned();
    }
    let mut out = format!("{base}{HEADER}");
    let chunk_budget = budget_bytes
        .saturating_sub(out.len())
        .saturating_sub(TAIL_RESERVE);
    let mut remaining = chunk_budget;

    let mut demoted: Vec<&CatalogServer> = Vec::new();
    for server in servers {
        let block = server_block(server);
        if block.len() + 1 <= remaining {
            remaining -= block.len() + 1;
            out.push('\n');
            out.push_str(&block);
        } else {
            demoted.push(server);
        }
    }

    if !demoted.is_empty() {
        // The overflow section starts with a blank separator line; fold it
        // into the first chunk's cost.
        let mut unlisted = 0usize;
        let mut first = true;
        for server in &demoted {
            let line = overflow_line(server);
            let sep = usize::from(first);
            if line.len() + sep <= remaining {
                remaining -= line.len() + sep;
                if first {
                    out.push('\n');
                    first = false;
                }
                out.push_str(&line);
                out.push('\n');
                remaining = remaining.saturating_sub(1); // trailing newline
            } else {
                unlisted += 1;
            }
        }
        if unlisted > 0 {
            // Zero overflow lines may have fit, leaving `out` mid-block
            // (no trailing newline) — keep the tail on its own line.
            if !out.ends_with('\n') {
                out.push('\n');
            }
            let tail = format!("  - … and {unlisted} more servers — await host.servers()");
            out.push_str(&tail);
            out.push('\n');
        }
    }

    out
}

/// Build one [`CatalogTool`] from a registered tool's description and input
/// schema. `summary` carries `name(params): sentence` so the listing line is
/// a single format away; empty when both inputs are empty.
pub fn catalog_tool(
    name: String,
    description: Option<&str>,
    parameters: &serde_json::Value,
) -> CatalogTool {
    let sentence = first_sentence(description);
    let params = param_names(parameters);
    // The tool name always leads the summary; params and/or the first
    // sentence follow. Both empty → empty summary (the listing line falls
    // back to the bare name).
    let summary = if sentence.is_empty() && params.is_empty() {
        String::new()
    } else {
        format!("{}({}): {}", name, params.join(", "), sentence)
    };
    CatalogTool { name, summary }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, tools: &[(&str, &str)]) -> CatalogServer {
        CatalogServer {
            name: name.to_string(),
            tools: tools
                .iter()
                .map(|(t, d)| CatalogTool {
                    name: t.to_string(),
                    summary: format!("{t}(): {d}"),
                })
                .collect(),
        }
    }

    fn big_server(name: &str, tool_count: usize) -> CatalogServer {
        CatalogServer {
            name: name.to_string(),
            tools: (0..tool_count)
                .map(|i| CatalogTool {
                    name: format!("tool_{i}"),
                    summary: format!("tool_{i}(): x"),
                })
                .collect(),
        }
    }

    const BASE: &str = "static base prose";

    #[test]
    fn no_servers_keeps_base_byte_identical() {
        assert_eq!(render_namespace_listing(BASE, &[], 0), BASE);
    }

    #[test]
    fn listing_fits_within_budget_and_total_is_bounded() {
        let servers = vec![
            server("tavily", &[("tavily_search", "Search the web")]),
            server("exa", &[("web_search_exa", "Search the web")]),
        ];
        let budget = 4096;
        let out = render_namespace_listing(BASE, &servers, budget);
        assert!(out.contains("tavily (1 tools):"));
        assert!(out.contains("  - tavily_search(): Search the web"));
        assert!(out.contains("exa (1 tools):"));
        assert!(out.len() <= budget, "{} > {budget}", out.len());
    }

    #[test]
    fn oversized_server_is_demoted_to_compact_line() {
        let servers = vec![
            big_server("ida", 200),
            server("exa", &[("web_search_exa", "Search")]),
        ];
        // Budget too small for ida's 200 tool lines but roomy for the rest.
        let budget = BASE.len() + HEADER.len() + 512;
        let out = render_namespace_listing(BASE, &servers, budget);
        assert!(
            !out.contains("tool_0"),
            "big server must not be listed in full"
        );
        assert!(out.contains("ida (200 tools) — discover in-script"));
        assert!(out.contains("exa (1 tools):"));
        assert!(out.len() <= budget, "{} > {budget}", out.len());
    }

    #[test]
    fn tiny_budget_leaves_header_only() {
        let servers = vec![server("tavily", &[("tavily_search", "Search")])];
        let out = render_namespace_listing(BASE, &servers, 8);
        assert!(out.starts_with(BASE));
        assert!(out.contains(HEADER));
        assert!(!out.contains("tavily"));
    }

    #[test]
    fn unlistable_overflow_servers_aggregate_into_tail_line() {
        let servers: Vec<CatalogServer> = (0..12)
            .map(|i| big_server(&format!("srv_{i}"), 30))
            .collect();
        // Each server's 30-tool block overruns what remains, so every server
        // is demoted to an overflow line; the budget fits only a handful of
        // those, and the rest aggregate into the tail line.
        let budget = BASE.len() + HEADER.len() + 900;
        let out = render_namespace_listing(BASE, &servers, budget);
        assert!(
            out.contains("more servers"),
            "aggregate tail expected: {out}"
        );
        assert!(out.len() <= budget, "{} > {budget}", out.len());
    }

    #[test]
    fn tail_starts_on_its_own_line_when_no_overflow_line_fits() {
        // First server's block fits the chunk budget with <18 bytes to
        // spare — less than any overflow line — so nothing else is listed
        // and the aggregate tail must not glue onto the first block's last
        // tool line.
        let mut first = big_server("aaa", 30);
        for tool in &mut first.tools {
            tool.summary = "x".to_string(); // "  - x" lines: block ≈ 195 B
        }
        let servers = vec![first, big_server("zzz", 30)];
        let budget = BASE.len() + HEADER.len() + 286;
        let out = render_namespace_listing(BASE, &servers, budget);
        assert!(out.contains("aaa (30 tools):"));
        assert!(
            !out.contains("zzz (30 tools):"),
            "second server must not be listed in full: {out}"
        );
        assert!(
            out.ends_with("  - … and 1 more servers — await host.servers()\n"),
            "tail must be a standalone line: {out}"
        );
        assert!(out.len() <= budget, "{} > {budget}", out.len());
    }

    #[test]
    fn catalog_tool_summary_includes_params_and_first_sentence() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "query": {}, "limit": {} },
            "required": ["query"]
        });
        let tool = catalog_tool(
            "search".to_string(),
            Some("Search the web for anything.\n\nLong tail."),
            &schema,
        );
        assert_eq!(
            tool.summary,
            "search(query, limit?): Search the web for anything."
        );
    }

    #[test]
    fn catalog_tool_without_params_keeps_the_name() {
        let tool = catalog_tool(
            "list_instances".to_string(),
            Some("List all registered IDA Pro instances."),
            &serde_json::json!({"type": "object"}),
        );
        assert_eq!(
            tool.summary,
            "list_instances(): List all registered IDA Pro instances."
        );
    }

    #[test]
    fn catalog_tool_with_empty_inputs_has_empty_summary() {
        let tool = catalog_tool(
            "noop".to_string(),
            None,
            &serde_json::json!({"type": "object"}),
        );
        assert!(tool.summary.is_empty());
    }
}
