//! Call-time shims for tool names that are not in the fixed 7-tool catalog.
//!
//! Two kinds live here:
//! * [`TOOL_ALIASES`]: the old long names (`resolve_context`, …) and the
//!   `ast_*` spellings are still accepted on `tools/call` and rewritten to the
//!   canonical short name before dispatch. `tools/list` only advertises the
//!   short names.
//! * [`EXPLAIN_SYMBOL`]: Cursor and other clients still invoke `explain_symbol`
//!   from stale prompts (~257 calls / 14d in one audit). It is accepted on
//!   `tools/call` and forwarded to `find_symbol` with bodies.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::server::{AstrolabeServer, QueryParams};

/// Removed in v0.12 (#70); still accepted as a call-time alias.
pub const EXPLAIN_SYMBOL: &str = "explain_symbol";

/// Old tool names (and `ast_*` spellings) accepted on `tools/call`, mapped to
/// the canonical name in [`crate::CATALOG_TOOL_NAMES`].
///
/// Keep in sync with `npm/packages/astrolabe/lib/setup/clients.js` and
/// `scripts/setup-cursor-permissions.mjs` (catalog only; aliases are server
/// side and never advertised).
pub const TOOL_ALIASES: &[(&str, &str)] = &[
    ("resolve_context", "resolve_contx"),
    ("ast_resolve_contx", "resolve_contx"),
    ("ast_context", "resolve_contx"),
    ("get_diagnostics", "get_diag"),
    ("ast_get_diag", "get_diag"),
    ("ast_diag", "get_diag"),
    ("get_languages", "get_lang"),
    ("ast_get_lang", "get_lang"),
    ("ast_langs", "get_lang"),
    ("ast_lang", "get_lang"),
    ("find_references", "find_refs"),
    ("ast_find_ref", "find_refs"),
    ("ast_find_refs", "find_refs"),
    ("goto_definition", "goto_def"),
    ("ast_goto_def", "goto_def"),
    ("ast_def", "goto_def"),
    ("ast_search_code", "search_code"),
    ("ast_search", "search_code"),
    ("ast_find_symbol", "find_symbol"),
    ("ast_symbol", "find_symbol"),
];

/// Names accepted on `tools/call` but never listed in `tools/list`.
pub const TOOL_ALIAS_NAMES: &[&str] = &[
    "resolve_context",
    "ast_resolve_contx",
    "ast_context",
    "get_diagnostics",
    "ast_get_diag",
    "ast_diag",
    "get_languages",
    "ast_get_lang",
    "ast_langs",
    "ast_lang",
    "find_references",
    "ast_find_ref",
    "ast_find_refs",
    "goto_definition",
    "ast_goto_def",
    "ast_def",
    "ast_search_code",
    "ast_search",
    "ast_find_symbol",
    "ast_symbol",
];

/// Rewrite an accepted alias to its canonical tool name; `None` means the
/// name is already canonical (or unknown — the router rejects it).
pub fn canonical_tool_name(name: &str) -> Option<&'static str> {
    TOOL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, canonical)| *canonical)
}

/// Compat tool names recognized by hooks (counter reset) but not listed in
/// `tools/list`.
pub const COMPAT_TOOL_NAMES: &[&str] = &[EXPLAIN_SYMBOL];

const COMPAT_NOTE: &str = "\
[compat] explain_symbol → find_symbol(include_body=true, substring=false). \
Use find_refs for call sites; switch to these two tools and stop calling explain_symbol.";

#[derive(Debug, Deserialize)]
struct ExplainCompatParams {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    index_root: Option<String>,
    /// Historical `explain_symbol` parameter.
    #[serde(default)]
    name: Option<String>,
    /// Accept `query` if a client already migrated halfway.
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    path_filter: Option<String>,
    #[serde(default)]
    budget_tokens: Option<usize>,
}

impl AstrolabeServer {
    /// Map a stale `explain_symbol` call onto `find_symbol`.
    pub(crate) fn explain_symbol_compat(&self, args: Option<Map<String, Value>>) -> CallToolResult {
        let value = Value::Object(args.unwrap_or_default());
        let params: ExplainCompatParams = match serde_json::from_value(value) {
            Ok(p) => p,
            Err(err) => {
                return crate::server::text_error(format!(
                    "explain_symbol was removed from the catalog; invalid params ({err}).\
                     Use find_symbol (include_body=true) for definitions and find_refs for call sites."
                ));
            }
        };
        let query = first_nonempty([&params.name, &params.query]);
        if query.is_empty() {
            return crate::server::text_error(
                "explain_symbol was removed from the catalog (compat shim still works).\
                 Pass name (or query); prefer find_symbol(include_body=true) and find_refs.",
            );
        }
        let result = self.find_symbol(Parameters(QueryParams {
            root: params.root,
            index_root: params.index_root,
            query,
            include_body: true,
            substring: Some(false),
            kind: None,
            path_filter: params.path_filter,
            depth: 0,
            budget_tokens: params
                .budget_tokens
                .unwrap_or_else(crate::server::default_budget),
            offset: 0,
            dedup: None,
            agent_id: None,
        }));
        annotate_compat(result)
    }
}

fn first_nonempty(opts: [&Option<String>; 2]) -> String {
    for s in opts.into_iter().flatten() {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

fn annotate_compat(mut result: CallToolResult) -> CallToolResult {
    if let Some(Value::Object(obj)) = result.structured_content.as_mut() {
        obj.insert("compat_alias".into(), json!("find_symbol"));
        obj.insert("compat_from".into(), json!(EXPLAIN_SYMBOL));
    } else if result.structured_content.is_none() {
        result.structured_content = Some(json!({
            "compat_alias": "find_symbol",
            "compat_from": EXPLAIN_SYMBOL,
        }));
    }
    match result.content.first_mut() {
        Some(first) if first.as_text().is_some() => {
            let text = first.as_text().map(|t| t.text.clone()).unwrap_or_default();
            let rewritten = if let Some((head, rest)) = text.split_once('\n') {
                if head.starts_with("index_root:") {
                    format!("{head}\n{COMPAT_NOTE}\n{rest}")
                } else {
                    format!("{COMPAT_NOTE}\n{text}")
                }
            } else {
                format!("{COMPAT_NOTE}\n{text}")
            };
            *first = ContentBlock::text(rewritten);
        }
        _ => result
            .content
            .insert(0, ContentBlock::text(COMPAT_NOTE.to_string())),
    }
    result
}
