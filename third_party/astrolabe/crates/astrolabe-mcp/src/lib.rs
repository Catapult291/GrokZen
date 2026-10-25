//! Astrolabe MCP protocol adapter.
//!
//! Every tool response is constructed in `server` and starts with a non-empty
//! text content block. Structured content is off by default (`default` and
//! `claude-code` contexts): Claude Code substitutes `structuredContent` for
//! `content[0].text` and will hide the real hits if that field is only
//! `{confidence, budget_tokens}`. Named contexts (`--context` /
//! `ASTROLABE_CONTEXT`) and `ASTROLABE_STRUCTURED` can turn it on.

mod alloc;
mod cli;
mod code_mask;
mod context;
mod context_page;
mod coverage;
mod diag_env;
mod diag_toolchain;
mod ensure_ls;
mod explain;
mod knobs;
mod line_scan;
mod session;
mod shared_index;
// hooks / instructions 对 bin（main.rs 的子命令分发）公开：hook 协议入口
// 不经 ServerHandler，直接调用模块函数。
pub mod hooks;
mod index;
pub mod instructions;
mod live_backend;
mod loop_guard;
#[cfg(feature = "native")]
pub mod native;
mod openai_schema;
mod precise_tools;
mod py_refs;
mod refs_graph;
mod refs_graph_ts;
mod refs_render;
mod reindex;
mod root;
pub mod roots;
mod server;
mod word_index;

pub use cli::{parse_launch, Launch, USAGE};
pub use context::{resolve_context_name, ClientContext};
pub use root::{
    child_git_repos, count_root_files, find_project_root, is_cwd_sentinel, resolve_index_root,
    resolve_root, ResolvedRoot, DEFAULT_MAX_ROOT_FILES,
};
pub use server::{AstrolabeServer, HostLspServer, StartupFailedServer};

pub const TOOL_CATALOG_TTL_MS: u64 = 86_400_000;
/// Catalog tools: `resolve_context`, `search_code`, `find_symbol`,
/// `get_languages`, plus the language-server-backed
/// `find_references` / `goto_definition` / `get_diagnostics`.
pub const TOOL_COUNT: usize = 4 + PRECISE_TOOL_COUNT;
/// Language-server-backed tools defined in `precise_tools`, mounted on the server.
pub const PRECISE_TOOL_COUNT: usize = 3;

/// Names of every catalog tool. Length must equal [`TOOL_COUNT`].
pub const CATALOG_TOOL_NAMES: &[&str] = &[
    "search_code",
    "find_symbol",
    "resolve_context",
    "get_diagnostics",
    "find_references",
    "goto_definition",
    "get_languages",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_tool_names_match_tool_count() {
        assert_eq!(CATALOG_TOOL_NAMES.len(), TOOL_COUNT);
        let unique: HashSet<&str> = CATALOG_TOOL_NAMES.iter().copied().collect();
        assert_eq!(
            unique.len(),
            TOOL_COUNT,
            "CATALOG_TOOL_NAMES must not contain duplicates"
        );
        let expected: HashSet<&str> = [
            "search_code",
            "find_symbol",
            "resolve_context",
            "get_diagnostics",
            "find_references",
            "goto_definition",
            "get_languages",
        ]
        .into_iter()
        .collect();
        assert_eq!(unique, expected);
    }
}
