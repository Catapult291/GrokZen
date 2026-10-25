//! Language-server access for the questions the graph cannot answer precisely.
//!
//! The split is measured, not stylistic. File-level import edges recall 100%
//! of what a language server reports, so those questions never reach this
//! module. Name-matched call edges recall 66% on TypeScript and 18% on Python
//! — for "where is this symbol actually referenced" that is not good enough,
//! and a wrong answer here becomes a wrong edit later.
//!
//! Design constraints, each from a measured failure in the systems this
//! project replaces:
//!
//! 1. **Never keep a language server resident.** Measured on Serena: four
//!    whole-repo reference searches grew the language-server subprocess from
//!    186 MB to 719 MB, and nothing ever reclaimed it — 94% of the growth was
//!    outside the host process, where the host's own memory limits cannot see
//!    it. Servers here start on demand and are reclaimed when idle or over
//!    budget.
//! 2. **A missing language server is a reportable state, not an error and not
//!    silence.** If `gopls` is absent we say so and return
//!    [`Confidence::Unknown`]; we do not fall back to name matching and
//!    present it as precise.
//! 3. **Never block the graph on a language server.** Startup and indexing of
//!    a real server takes seconds; graph-level tools must stay responsive
//!    throughout.

use std::path::PathBuf;
use std::time::Duration;

use crate::types::{Confidence, Language, RelPath};

pub mod anchor;
pub mod apple;
pub mod csharp;
pub mod diagnostics;
pub mod discovery;
mod fallback;
pub mod install;
pub mod installer;
pub mod kotlin;
pub mod pool;
pub mod queries;
pub mod router;
mod rustup_proxy;
pub mod transport;
pub mod vue_bridge;

pub use install::{
    parse_language_name, InstallError as SessionInstallError, InstallPlan, InstallRecord,
    LanguageServerInstaller, LspStatus, StubInstaller, VersionPolicy,
};
pub use installer::{
    ensure_installed, ensure_installed_with, ArchiveFormat, AutoInstallMode, InstallContext,
    InstallError as ArtifactInstallError, InstallHint, InstallSpec, NeedsInstall,
};

/// Zero-based line and UTF-16 column, matching the LSP position encoding.
///
/// UTF-16 is what the protocol mandates by default. Tree-sitter hands us
/// UTF-16 code-unit offsets too, so the two agree as long as nobody converts
/// to byte offsets in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    pub path: RelPath,
    pub range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub path: RelPath,
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    /// Rule or error code, when the server supplies one.
    pub code: Option<String>,
}

/// A rename or edit expressed as replacements, grouped by file.
///
/// Edits within a file must not overlap and are applied last-to-first so
/// earlier offsets stay valid.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceEdit {
    pub edits: Vec<(RelPath, Vec<TextEdit>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// No server binary for this language is installed. This is the common
    /// case on a fresh machine and must be reported to the agent verbatim, so
    /// it can tell the user what to install.
    #[error("no language server available for {0:?}: {1}")]
    Unavailable(Language, String),
    #[error("language server for {0:?} failed to start: {1}")]
    Startup(Language, String),
    #[error("language server request timed out after {0:?}")]
    Timeout(Duration),
    /// LSP `ContentModified` (-32801): the server's view of the document
    /// changed while the request was in flight.
    ///
    /// The spec calls this retryable and expects the client to re-issue the
    /// request. It is distinct from [`LspError::Protocol`] precisely so
    /// callers do not report a transient race as a failed query.
    #[error("language server content modified; the request should be retried")]
    ContentModified,
    #[error("language server protocol error: {0}")]
    Protocol(String),
    #[error("language server exited unexpectedly")]
    Crashed,
}

/// Result of a precise query, carrying how much the answer can be trusted.
///
/// A caller must be able to distinguish "no references exist" from "I could
/// not check". Returning an empty vector for both is the failure mode this
/// type exists to prevent.
#[derive(Clone, Debug)]
pub struct Precise<T> {
    pub value: T,
    pub confidence: Confidence,
    /// Populated when confidence is below `Exact`, explaining why.
    pub note: Option<String>,
}

impl<T> Precise<T> {
    pub fn exact(value: T) -> Self {
        Precise {
            value,
            confidence: Confidence::Exact,
            note: None,
        }
    }

    pub fn unknown(value: T, note: impl Into<String>) -> Self {
        Precise {
            value,
            confidence: Confidence::Unknown,
            note: Some(note.into()),
        }
    }
}

/// What happened to a file, as reported by the workspace watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChangeKind {
    Created,
    Changed,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: RelPath,
    pub kind: FileChangeKind,
}

/// One running language server, scoped to a workspace root.
///
/// Implementations must be usable from multiple threads; the pool hands out
/// shared references.
pub trait LanguageServer: Send + Sync {
    fn language(&self) -> Language;

    /// All references to the symbol at `position`, including its declaration.
    fn references(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError>;

    fn definition(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError>;

    /// Hover info (docstring/type/signature) for the symbol at `position`.
    /// Raw JSON result of `textDocument/hover`（解析见 `queries::parse_hover`）。
    fn hover(&self, path: &RelPath, position: Position) -> Result<serde_json::Value, LspError>;

    /// Diagnostics for one file. Implementations should prefer a pull request
    /// (`textDocument/diagnostic`) and fall back to whatever the server last
    /// published, rather than blocking indefinitely for a push that may never
    /// arrive.
    fn diagnostics(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError>;

    /// Compute (but do not apply) a rename.
    fn prepare_rename(
        &self,
        path: &RelPath,
        position: Position,
        new_name: &str,
    ) -> Result<WorkspaceEdit, LspError>;

    /// Block until the server finishes its background work, or `deadline`
    /// elapses. Returns whether the server is ready.
    ///
    /// Returning "still indexing" to an agent just buys a wasted round trip;
    /// it cannot do anything but retry. Waiting once, bounded, is the
    /// cheaper trade. Servers that report no progress are ready already.
    fn wait_until_ready(&self, _deadline: std::time::Duration) -> bool {
        true
    }

    /// Whether the server still has background work (typically initial
    /// indexing) outstanding.
    ///
    /// A server that is still indexing answers with an empty result rather
    /// than an error, so without this a cold server's "no references" is
    /// indistinguishable from a real one. Defaults to `false` for servers
    /// that report no progress at all; those answer from a complete index.
    fn is_busy(&self) -> bool {
        false
    }

    /// Files changed on disk (from the workspace watcher).
    ///
    /// Implementations forward these as `workspace/didChangeWatchedFiles`
    /// and resynchronise or close any documents they hold open, so the next
    /// answer is computed from what is on disk now. Default: nothing to do.
    fn files_changed(&self, _changes: &[FileChange]) {}

    /// Resident-set size of the server process tree, when observable. Used by
    /// the pool to reclaim servers that have grown past their budget.
    fn memory_bytes(&self) -> Option<u64>;

    fn shutdown(&self) -> Result<(), LspError>;
}

/// How a language server is launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSpec {
    pub language: Language,
    /// Executable name or absolute path.
    pub command: PathBuf,
    pub args: Vec<String>,
    /// Human-readable install hint, surfaced when the binary is missing.
    /// Without this the agent can only report "not found", which the user
    /// cannot act on.
    pub install_hint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precise_distinguishes_empty_from_unchecked() {
        let found: Precise<Vec<Location>> = Precise::exact(Vec::new());
        let unchecked: Precise<Vec<Location>> =
            Precise::unknown(Vec::new(), "gopls is not installed");

        // Both carry no locations; only the confidence tells them apart, which
        // is exactly the distinction an agent needs before acting.
        assert!(found.value.is_empty() && unchecked.value.is_empty());
        assert_eq!(found.confidence, Confidence::Exact);
        assert_eq!(unchecked.confidence, Confidence::Unknown);
        assert!(unchecked.note.is_some());
    }
}
