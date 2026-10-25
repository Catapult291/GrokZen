//! Tool-facing request types and engine output metadata.
//!
//! These types are the contract between the Grok Build tool layer and the
//! in-process Astrolabe engine. They carry no `astrolabe_core` or `rmcp` types,
//! so the tool crate never sees the engine's internals.
//!
//! Coordinate convention: every string path is resolved relative to the
//! session working directory (or is absolute inside the workspace), and every
//! line number this crate emits is 1-based. The engine converts to the 0-based
//! coordinates the precise tools expect.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Cooperative cancellation for one engine call.
///
/// The tool layer creates one per call, flips it when the host cancels, and
/// the scan stops at its next file boundary. Dropping the handle does not flip
/// it; callers that want cancel-on-drop wrap it in a guard.
#[derive(Clone, Debug, Default)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}

/// `search_code`: exhaustive literal/regex search over the indexed workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchCodeRequest {
    /// Text to find; a regular expression when `regex` is true.
    pub query: String,
    /// Treat `query` as a regular expression. Single-line patterns only.
    #[serde(default)]
    pub regex: bool,
    /// File or directory to search in, relative to the workspace (default: whole workspace).
    #[serde(default)]
    pub path: Option<String>,
    /// Only search files matching this glob, e.g. `*.rs` or `*.{ts,tsx}`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Paths to skip. A bare name matches a path segment (`vendor`); globs work too.
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    /// Case sensitivity. Omit for smart case: sensitive only when `query` has an uppercase letter.
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// Lines of context around each hit (0-10). Omit for the engine's adaptive default.
    #[serde(default)]
    pub context_lines: Option<usize>,
    /// Instead of matches, count hits per `top`, `dir`, or `file`.
    #[serde(default)]
    pub aggregate: Option<String>,
    /// Ignore hits inside comments and string literals.
    #[serde(default)]
    pub skip_comments: bool,
    /// 0-based hit offset; pass `next_offset` from the previous page.
    #[serde(default)]
    pub offset: usize,
}

/// `find_symbol`: locate a symbol by name, `Class/method` path, or file outline.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FindSymbolRequest {
    /// Symbol name, name substring, `Class/method` path, or a file path for an outline.
    pub query: String,
    /// Restrict to a `SymbolKind` (`function`, `method`, `class`, `struct`, …).
    #[serde(default)]
    pub kind: Option<String>,
    /// File or directory prefix, relative to the workspace.
    #[serde(default)]
    pub path: Option<String>,
    /// Attach the source body of the first few hits.
    #[serde(default)]
    pub include_body: bool,
    /// Expand member levels for a file or `*` outline query.
    #[serde(default)]
    pub depth: Option<u8>,
    /// 0-based match offset; pass `next_offset` from the previous page.
    #[serde(default)]
    pub offset: usize,
}

/// `resolve_context`: task-ranked list of relevant files and symbols.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ResolveContextRequest {
    /// What you are trying to do, in a sentence. Identifiers and paths in it rank highest.
    pub task: String,
    /// File or directory prefix, relative to the workspace.
    #[serde(default)]
    pub path: Option<String>,
    /// 0-based file offset; pass `next_offset` from the previous page.
    #[serde(default)]
    pub offset: usize,
}

/// `get_languages`: language mix, code size, index gaps and language-server readiness.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LanguagesRequest {}

/// `find_references`: precise references, callers and rename impact.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesRequest {
    /// Symbol name to find references for.
    pub symbol: String,
    /// File that declares or uses it, relative to the workspace (inferred when omitted).
    #[serde(default)]
    pub path: Option<String>,
    /// Line of the symbol, 1-based (disambiguates overloads and same names).
    #[serde(default)]
    pub line: Option<u32>,
    /// UTF-16 column on `line`, 1-based; used with `line`.
    #[serde(default)]
    pub character: Option<u32>,
    /// New name: return a read-only per-file rename checklist instead of a list.
    #[serde(default)]
    pub dry_run_rename: Option<String>,
    /// 0-based result offset; pass `next_offset` from the previous page.
    #[serde(default)]
    pub offset: usize,
}

/// `goto_definition`: resolve a symbol's definition from a position or a name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GotoDefinitionRequest {
    /// File containing the use site, relative to the workspace.
    #[serde(default)]
    pub path: Option<String>,
    /// Symbol name; resolves by name when `line`/`character` are omitted.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Line of the use site, 1-based.
    #[serde(default)]
    pub line: Option<u32>,
    /// UTF-16 column on `line`, 1-based; used with `line`.
    #[serde(default)]
    pub character: Option<u32>,
    /// 0-based candidate offset when several definitions are listed.
    #[serde(default)]
    pub offset: usize,
}

/// `get_diagnostics`: syntax/type errors for one file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsRequest {
    /// File to check, relative to the workspace.
    pub path: String,
    /// Minimum severity to report: `error`, `warning`, `information`, or `hint`.
    #[serde(default)]
    pub severity: Option<String>,
}

/// A position a host language server can query (0-based, as LSP uses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// Repo-relative posix path.
    pub engine_path: String,
    /// Absolute path on disk.
    pub absolute: PathBuf,
    pub line: u32,
    pub character: u32,
}

/// One location returned by the host language server. Coordinates are
/// 0-based; rendering converts them to the 1-based model contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LspSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl LspSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
        }
    }
}

/// One diagnostic returned by the host language server. Lines and columns are
/// 1-based, matching the model-visible contract.
#[derive(Debug, Clone)]
pub struct LspDiagnostic {
    pub line: u32,
    pub character: u32,
    pub severity: LspSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// A precise-tool result plus what the host still has to do.
#[derive(Debug)]
pub struct PreciseOutcome {
    pub output: EngineOutput,
    /// The index only answered `unknown`; a host language server may know.
    pub needs_host_lsp: bool,
    /// Where to ask the host server, when a position is known.
    pub anchor: Option<Anchor>,
    /// Several index declarations when a name alone was ambiguous.
    pub candidates: Vec<Anchor>,
    /// Human-readable reason the index could not decide.
    pub unknown_reason: Option<String>,
}

/// Engine reply: rendered text plus the counters the engine reported.
#[derive(Debug, Clone, Default)]
pub struct EngineOutput {
    pub text: String,
    pub meta: EngineMeta,
}

/// Counters extracted from the engine's structured payload.
#[derive(Debug, Clone, Default)]
pub struct EngineMeta {
    pub total: Option<u64>,
    pub shown: Option<u64>,
    pub omitted: Option<u64>,
    pub offset: Option<u64>,
    pub next_offset: Option<u64>,
    pub generation: Option<u64>,
    pub confidence: Option<String>,
    pub truncated: bool,
    pub is_error: bool,
}
