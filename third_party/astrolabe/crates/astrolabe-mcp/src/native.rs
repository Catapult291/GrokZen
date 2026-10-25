//! In-process facade over the tool layer (`feature = "native"`).
//!
//! A host that links this crate (Grok Build does) calls these methods as plain
//! Rust: no MCP transport, no `astrolabe` child process. The facade only maps
//! request types and unwraps `CallToolResult` into text + JSON, so the search
//! ranking, pagination, body rendering and freshness barrier stay in `server`.
//!
//! Model-visible contract is unchanged: `text` is the same string the MCP tool
//! returns, paths are repo-relative, lines are 1-based.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};

use crate::precise_tools::{FindReferencesParams, GetDiagnosticsParams, GotoDefinitionParams};
use crate::server::{AstrolabeServer, BudgetOnly, QueryParams, ResolveContextParams, SearchParams};

/// Same default the MCP tool uses when `budget_tokens` is omitted.
const DEFAULT_BUDGET: usize = 2_500;

/// One tool reply: the rendered text plus the structured payload the MCP path
/// would have sent as `structuredContent` (may be `Null` when off).
#[derive(Debug, Clone)]
pub struct NativeOutput {
    pub text: String,
    pub structured: serde_json::Value,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct NativeIndexStatus {
    /// `building` | `ready` | `failed`
    pub state: &'static str,
    pub message: Option<String>,
    pub stats: Option<serde_json::Value>,
}

impl NativeIndexStatus {
    pub fn is_ready(&self) -> bool {
        self.state == "ready"
    }
}

/// Cooperative-cancel flag for one in-process call.
///
/// The tool bodies poll a thread-local flag that the MCP `call_tool` path
/// installs per request. A native host has no JSON-RPC cancel notification, so
/// it owns this handle and passes it to the `*_cancellable` entry points.
#[derive(Clone, Debug, Default)]
pub struct NativeCancel(Arc<AtomicBool>);

impl NativeCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a flag the host already owns.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Arguments of [`AstrolabeServer::native_search_code`].
///
/// Mirrors the MCP `SearchParams` minus `root` routing, MCP paging switches and
/// session dedup: a native host resolves the root itself and should always get
/// the full result (`dedup` is forced off, the host's loop guard handles
/// repeats).
#[derive(Debug, Clone)]
pub struct NativeSearchCode {
    pub query: String,
    pub regex: bool,
    /// Repo-relative path substring (case-sensitive), not a glob.
    pub path_filter: Option<String>,
    /// `None` = smart case: sensitive iff `query` contains an uppercase letter.
    pub case_sensitive: Option<bool>,
    /// 0-10; `None` = adaptive.
    pub context_lines: Option<usize>,
    /// `top` | `dir` | `file`
    pub aggregate: Option<String>,
    /// Bare names match a path segment; globs are accepted too.
    pub exclude: Option<Vec<String>>,
    /// Include glob (`*.rs`, `*.{ts,tsx}`); `None` searches every admitted file.
    pub glob: Option<String>,
    pub skip_comments: bool,
    /// 0-based match offset; pair with `next_offset` from `structured`.
    pub offset: usize,
    /// Token budget; 0 means [`DEFAULT_BUDGET`].
    pub budget_tokens: usize,
}

impl NativeSearchCode {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            context_lines: None,
            aggregate: None,
            exclude: None,
            glob: None,
            skip_comments: false,
            offset: 0,
            budget_tokens: DEFAULT_BUDGET,
        }
    }
}

/// Arguments of [`AstrolabeServer::native_find_symbol`]. Mirrors the MCP
/// `QueryParams` minus `root`.
#[derive(Debug, Clone)]
pub struct NativeFindSymbol {
    pub query: String,
    pub include_body: bool,
    /// `None` = ranked substring; `false` = exact name; `true` = substring.
    pub substring: Option<bool>,
    /// `function` | `method` | `class` | … (see `VALID_KIND_NAMES`).
    pub kind: Option<String>,
    /// Repo-relative path substring (case-sensitive), not a glob.
    pub path_filter: Option<String>,
    /// Member levels to expand for a file/`*` outline query.
    pub depth: u8,
    pub offset: usize,
    pub budget_tokens: usize,
}

impl NativeFindSymbol {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            include_body: false,
            substring: None,
            kind: None,
            path_filter: None,
            depth: 0,
            offset: 0,
            budget_tokens: DEFAULT_BUDGET,
        }
    }
}

/// Arguments of [`AstrolabeServer::native_resolve_context`].
#[derive(Debug, Clone)]
pub struct NativeResolveContext {
    pub task_description: String,
    pub path_filter: Option<String>,
    pub offset: usize,
    pub budget_tokens: usize,
}

impl NativeResolveContext {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task_description: task.into(),
            path_filter: None,
            offset: 0,
            budget_tokens: DEFAULT_BUDGET,
        }
    }
}

/// Arguments of [`AstrolabeServer::native_find_references`]. `line` and
/// `character` are 0-based, exactly as the MCP tool documents them; a host
/// that shows 1-based coordinates subtracts 1 before calling.
#[derive(Debug, Clone)]
pub struct NativeFindReferences {
    pub symbol: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub character: Option<u32>,
    pub dry_run_rename: Option<String>,
    pub offset: Option<usize>,
    pub budget_tokens: usize,
}

impl NativeFindReferences {
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            path: None,
            line: None,
            character: None,
            dry_run_rename: None,
            offset: None,
            budget_tokens: DEFAULT_BUDGET,
        }
    }
}

/// Arguments of [`AstrolabeServer::native_goto_definition`]. `line` and
/// `character` are 0-based.
#[derive(Debug, Clone)]
pub struct NativeGotoDefinition {
    pub path: String,
    pub symbol: Option<String>,
    pub line: Option<u32>,
    pub character: Option<u32>,
    pub offset: Option<usize>,
    pub budget_tokens: usize,
}

/// Arguments of [`AstrolabeServer::native_get_diagnostics`].
#[derive(Debug, Clone)]
pub struct NativeDiagnostics {
    pub path: String,
    /// `error` | `warning` | `information` | `hint`.
    pub severity: Option<String>,
    /// `true` asks the language server (slow); `None` lets the tool decide.
    pub lsp: Option<bool>,
    /// `false` forbids running `cargo check` / `go test -c` / `tsc`: the
    /// answer comes from the parser only. Hosts with their own build/test
    /// tools set this.
    pub toolchain: bool,
    pub budget_tokens: usize,
}

impl NativeDiagnostics {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            severity: None,
            lsp: None,
            toolchain: true,
            budget_tokens: DEFAULT_BUDGET,
        }
    }
}

impl AstrolabeServer {
    /// In-process `search_code`. Blocks the calling thread; async hosts should
    /// run it on a blocking pool.
    pub fn native_search_code(&self, req: NativeSearchCode) -> NativeOutput {
        let params = SearchParams {
            root: None,
            query: req.query,
            regex: req.regex,
            path_filter: req.path_filter,
            case_sensitive: req.case_sensitive,
            budget_tokens: budget(req.budget_tokens),
            offset: req.offset,
            context_lines: req.context_lines,
            page: false,
            group: None,
            dedup: Some(false),
            aggregate: req.aggregate,
            exclude: req.exclude,
            count_only: false,
            skip_comments: req.skip_comments,
            glob: req.glob,
        };
        split(self.search_code(Parameters(params)))
    }

    /// In-process `search_code` with cooperative cancellation.
    ///
    /// The file scan polls `cancel` at each file and stops early, returning the
    /// same partial-result framing the MCP path uses when its request is
    /// cancelled.
    pub fn native_search_code_cancellable(
        &self,
        req: NativeSearchCode,
        cancel: &NativeCancel,
    ) -> NativeOutput {
        crate::server::cancel::scope(cancel.flag(), || self.native_search_code(req))
    }

    /// In-process `find_symbol`. Blocks the calling thread.
    pub fn native_find_symbol(&self, req: NativeFindSymbol) -> NativeOutput {
        let params = QueryParams {
            root: None,
            query: req.query,
            include_body: req.include_body,
            substring: req.substring,
            kind: req.kind,
            path_filter: req.path_filter,
            depth: req.depth,
            budget_tokens: budget(req.budget_tokens),
            offset: req.offset,
        };
        split(self.find_symbol(Parameters(params)))
    }

    /// In-process `resolve_context`. Blocks the calling thread.
    pub fn native_resolve_context(&self, req: NativeResolveContext) -> NativeOutput {
        let params = ResolveContextParams {
            root: None,
            task_description: req.task_description,
            budget_tokens: budget(req.budget_tokens),
            dedup: Some(false),
            offset: req.offset,
            path_filter: req.path_filter,
        };
        split(self.resolve_context(Parameters(params)))
    }

    /// In-process `find_references`. Blocks the calling thread; may start a
    /// language server when the index cannot decide.
    pub fn native_find_references(&self, req: NativeFindReferences) -> NativeOutput {
        let params = FindReferencesParams {
            root: None,
            symbol: req.symbol,
            path: req.path,
            line: req.line,
            character: req.character,
            budget_tokens: budget(req.budget_tokens),
            offset: req.offset,
            dry_run_rename: req.dry_run_rename,
        };
        split(self.find_references(Parameters(params)))
    }

    /// In-process `goto_definition`. Blocks the calling thread.
    pub fn native_goto_definition(&self, req: NativeGotoDefinition) -> NativeOutput {
        let params = GotoDefinitionParams {
            root: None,
            path: req.path,
            symbol: req.symbol,
            line: req.line,
            character: req.character,
            budget_tokens: budget(req.budget_tokens),
            offset: req.offset,
        };
        split(self.goto_definition(Parameters(params)))
    }

    /// In-process `get_diagnostics`. Blocks the calling thread.
    pub fn native_get_diagnostics(&self, req: NativeDiagnostics) -> NativeOutput {
        let params = GetDiagnosticsParams {
            root: None,
            path: req.path,
            severity: req.severity,
            budget_tokens: budget(req.budget_tokens),
            lsp: req.lsp,
            toolchain: req.toolchain,
        };
        split(self.get_diagnostics(Parameters(params)))
    }

    /// In-process `get_languages` with the default budget and no host LSP
    /// override (PATH-probe status only).
    pub fn native_get_languages(&self) -> NativeOutput {
        self.native_get_languages_with_budget(DEFAULT_BUDGET, Vec::new())
    }

    /// In-process `get_languages` with an explicit token budget and the
    /// host's registered language servers (from `lsp.json` / plugins). When
    /// `host_lsp` is non-empty the LSP column reports that pool instead of
    /// Astrolabe's PATH probe.
    pub fn native_get_languages_with_budget(
        &self,
        budget_tokens: usize,
        host_lsp: Vec<crate::server::HostLspServer>,
    ) -> NativeOutput {
        split(self.get_languages(Parameters(BudgetOnly {
            root: None,
            budget_tokens: budget(budget_tokens),
            host_lsp: if host_lsp.is_empty() {
                None
            } else {
                Some(host_lsp)
            },
        })))
    }

    /// Canonical index root this server serves (the path the engine passed
    /// to [`Self::new_native`], canonicalized).
    pub fn native_root(&self) -> &Path {
        self.native_primary_root()
    }

    /// `true` when this process holds the write side of `<repo>/.astrolabe`;
    /// `false` means it is a follower reading the leader's store (no writes).
    pub fn native_is_leader(&self) -> bool {
        self.native_leader_held()
    }

    /// Current state of the primary index. Does not block.
    pub fn native_index_status(&self) -> NativeIndexStatus {
        let (code, message) = self.native_primary_state();
        let (state, stats) = match code {
            1 => ("ready", self.native_primary_stats()),
            2 => ("failed", None),
            _ => ("building", None),
        };
        NativeIndexStatus {
            state,
            message,
            stats,
        }
    }

    /// Poll the primary index until it is ready, failed, or `timeout` elapses.
    /// Returns the last observed status, so callers can distinguish a timeout
    /// (`building`) from a failed build.
    pub fn native_wait_ready(&self, timeout: Duration) -> NativeIndexStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.native_index_status();
            if status.state != "building" || Instant::now() >= deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Declarations of `symbol` from the syntax index, as 0-based anchors a
    /// host can hand to its own language server. `path` (repo-relative) narrows
    /// the search; `limit` caps the list (0 means "no cap").
    ///
    /// The precise tools need a position, not just a name: this is how a
    /// symbol-only `find_references` / `goto_definition` finds one without
    /// starting a language server.
    pub fn native_symbol_anchors(
        &self,
        symbol: &str,
        path: Option<&str>,
        limit: usize,
    ) -> Vec<NativeSymbolAnchor> {
        let name = symbol.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let Some(index) = self.native_index_snapshot() else {
            return Vec::new();
        };
        let lower = name.to_lowercase();
        let root = self.native_root();
        let mut rows: Vec<NativeSymbolAnchor> = Vec::new();
        for sym in &index.graph().symbols {
            if sym.name != name && sym.name.to_lowercase() != lower {
                continue;
            }
            let Some(file) = index.file(sym.file) else {
                continue;
            };
            let rel = file.path.as_str();
            if let Some(filter) = path.map(str::trim).filter(|p| !p.is_empty()) {
                if !rel.contains(filter) {
                    continue;
                }
            }
            let line = sym.start_line.saturating_sub(1);
            rows.push(NativeSymbolAnchor {
                name: sym.name.clone(),
                path: rel.to_string(),
                line,
                character: identifier_column(root, rel, line, &sym.name),
                kind: format!("{:?}", sym.kind).to_lowercase(),
                signature: sym.signature.clone(),
            });
        }
        // Exact case first, then source order; identical anchors collapse.
        rows.sort_by(|a, b| {
            (a.name != name)
                .cmp(&(b.name != name))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        rows.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        if limit > 0 {
            rows.truncate(limit);
        }
        rows
    }
}

/// One declaration anchor: repo-relative path and 0-based line/character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSymbolAnchor {
    pub name: String,
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub kind: String,
    pub signature: String,
}

/// UTF-16 column of `name` on `line`, or 0 when it is not on that line (a
/// multi-line signature, a generated file).
fn identifier_column(root: &Path, rel: &str, line: u32, name: &str) -> u32 {
    let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
        return 0;
    };
    let Some(text) = text.lines().nth(line as usize) else {
        return 0;
    };
    match text.find(name) {
        Some(byte) => text[..byte].encode_utf16().count() as u32,
        None => 0,
    }
}

fn budget(raw: usize) -> usize {
    if raw == 0 {
        DEFAULT_BUDGET
    } else {
        raw
    }
}

fn split(result: CallToolResult) -> NativeOutput {
    let text = result
        .content
        .first()
        .and_then(ContentBlock::as_text)
        .map(|t| t.text.clone())
        .unwrap_or_default();
    NativeOutput {
        text,
        structured: result.structured_content.unwrap_or(serde_json::Value::Null),
        is_error: result.is_error.unwrap_or(false),
    }
}

#[cfg(test)]
mod native_cancel_tests {
    use super::*;

    #[test]
    fn cancel_flag_is_visible_inside_the_scope() {
        let cancel = NativeCancel::new();
        let trip = cancel.clone();
        crate::server::cancel::scope(cancel.flag(), || {
            assert!(!crate::server::cancel::is_cancelled());
            trip.cancel();
            assert!(crate::server::cancel::is_cancelled());
        });
        assert!(cancel.is_cancelled());
    }
}
