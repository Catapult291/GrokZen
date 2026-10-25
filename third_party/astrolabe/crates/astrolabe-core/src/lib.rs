//! Astrolabe core: a deterministic, file-level code graph for coding agents.
//!
//! Design rules learned from measuring the prior art (see `docs/` for the
//! measurements behind each one):
//!
//! 1. **Never silently drop a parse failure.** OpenVisio's `catch {}` lost 20%
//!    of files on a real repo with no signal to the caller. Errors surface in
//!    [`IndexReport`].
//! 2. **Release parser resources explicitly.** Not doing so cost 5 GB of peak
//!    RSS in the WASM implementation.
//! 3. **Bound every cache.** Memory is a product feature here, not an
//!    afterthought.
//! 4. **Say "unknown" instead of guessing.** See [`types::Confidence`].

pub mod body;
pub mod budget;
pub mod cache;
pub mod elect;
pub mod freshness;
pub mod go_iface;
pub mod graph;
pub mod index;
pub mod lsp;
pub mod name_path;
pub mod parse;
pub mod render;
pub mod resolvers;
pub mod scan;
pub mod store;
pub mod text;
pub mod text_search;
pub mod types;
pub mod watch;

pub use types::{
    CodeEdge, CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, FileIndex, Language,
    ModuleResolver, ModuleUnit, ProjectMeta, RelPath, SymbolId, SymbolKind,
};
pub use watch::{ChangeKind, ChangeSet, WatchConfig, WatchHandle, Watcher};

/// `IndexReport::partial_parses` reason of a file that parsed with syntax
/// errors.
pub const PARTIAL_PARSE_REASON: &str = "partial parse: syntax errors in file";
/// Same, for a C / C++ file whose indexed parse is that of the length-
/// preserving macro/preprocessor rewrite (`ParsedFile::c_family_rewrite`):
/// diagnose it with `parse::diagnose_indexed_syntax(.., true)`.
pub const PARTIAL_PARSE_REWRITE_REASON: &str =
    "partial parse: syntax errors in file (C/C++ rewrite kept)";

/// Outcome of an index run. Every file that failed is named — a partial index
/// that looks complete is worse than one that admits its gaps.
#[derive(Debug, Default)]
pub struct IndexReport {
    pub files_scanned: usize,
    pub files_parsed: usize,
    /// `(path, reason)` for every file that could not be parsed.
    pub parse_failures: Vec<(RelPath, String)>,
    /// Files that parsed with syntax errors. They ARE in the index (symbols
    /// found are kept) but may be incomplete; not a coverage failure.
    pub partial_parses: Vec<(RelPath, String)>,
    /// Source files skipped by the scanner for exceeding the size cap.
    pub too_large: Vec<RelPath>,
    /// Import specifiers that pointed inside the repo but did not resolve.
    /// This is the metric the five resolver workstreams are judged on.
    pub unresolved_imports: Vec<(RelPath, String)>,
    pub import_edges: usize,
    pub symbols: usize,
}

impl IndexReport {
    /// Share of in-repo imports that resolved. The CI gate lives on this.
    pub fn import_resolution_rate(&self) -> f64 {
        let total = self.import_edges + self.unresolved_imports.len();
        if total == 0 {
            return 1.0;
        }
        self.import_edges as f64 / total as f64
    }
}
