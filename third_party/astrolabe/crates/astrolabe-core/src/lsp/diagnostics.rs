//! File diagnostics for the rewrite loop: "does this file still compile?"
//!
//! Language servers deliver diagnostics in two ways, and this module has to
//! handle both. Prefer a pull request (`textDocument/diagnostic`, LSP 3.17+)
//! when the server supports it. Fall back to waiting for a
//! `textDocument/publishDiagnostics` notification after `didOpen` when it does
//! not. A missing push is not "no problems" — some servers stay silent on a
//! clean file, and treating that silence as [`Confidence::Exact`] would let an
//! agent ship a broken edit.
//!
//! [`LanguageServer::diagnostics`](super::LanguageServer::diagnostics) is the
//! pooled high-level method. This module sits above it: it chooses pull vs
//! push, bounds the wait, filters, sorts, and wraps the answer in
//! [`Precise`] so "clean" and "unchecked" stay distinguishable.
//!
//! `transport.rs` / `pool.rs` are separate workstreams. Talk to them through
//! [`DiagnosticClient`] and [`LanguageServer`](super::LanguageServer); do not
//! import their types.

use std::time::Duration;

use crate::types::{Language, RelPath};

use super::{Diagnostic, LanguageServer, LspError, Position, Precise, Range, Severity};

/// How long to wait for a `publishDiagnostics` notification after `didOpen`.
///
/// Two and a half seconds sits in the 2–3 s band the call site asked for.
/// rust-analyzer and pyright typically publish within a second or two of
/// opening a file; waiting much longer stalls the agent, and waiting much
/// shorter misses a slow first typecheck. Language-specific overrides (Serena
/// waits 8 s for rust-analyzer) belong at the pool/server layer via
/// [`FetchOptions::push_timeout`], not baked in here. The timeout is also the
/// only way to finish when a clean file never generates a push at all.
pub const DEFAULT_PUSH_TIMEOUT: Duration = Duration::from_millis(2500);

/// Which severities to keep.
///
/// The rewrite loop's question is usually "did I break the build?", which is
/// errors, sometimes also warnings. Information and hints stay available for
/// callers that ask for everything.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SeverityFilter {
    /// Only [`Severity::Error`].
    Errors,
    /// Errors and warnings. Default for the rewrite loop.
    #[default]
    ErrorsAndWarnings,
    /// Error, warning, information, and hint.
    All,
}

impl SeverityFilter {
    /// Whether a diagnostic of `severity` survives this filter.
    pub fn allows(self, severity: Severity) -> bool {
        match self {
            Self::Errors => severity == Severity::Error,
            Self::ErrorsAndWarnings => severity <= Severity::Warning,
            Self::All => true,
        }
    }
}

/// Options for a single diagnostics fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchOptions {
    pub filter: SeverityFilter,
    /// Bound on waiting for a push. Ignored when a pull request succeeds.
    pub push_timeout: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            filter: SeverityFilter::default(),
            push_timeout: DEFAULT_PUSH_TIMEOUT,
        }
    }
}

/// Outcome of waiting for `textDocument/publishDiagnostics`.
///
/// Distinguishing "the server said the file is clean" from "the server said
/// nothing" is the whole point of this type. An empty vector inside
/// [`Received`](PushWait::Received) is Exact; [`NeverReceived`] is Unknown.
#[derive(Clone, Debug)]
pub enum PushWait {
    /// At least one notification arrived. An empty vector means the server
    /// published an empty list — the file is clean as far as it knows.
    Received(Vec<Diagnostic>),
    /// The timeout elapsed without any notification for this document.
    NeverReceived,
}

/// Protocol surface needed to fetch diagnostics with pull and push.
///
/// [`LanguageServer`](super::LanguageServer) only exposes `diagnostics()`, which
/// is what a pool hands out. Pull vs push, `didOpen` / `didClose`, and "did a
/// push ever arrive?" need a richer client. Transport implements this; tests
/// inject a fake that also implements [`LanguageServer`](super::LanguageServer).
pub trait DiagnosticClient: Send + Sync {
    /// Whether the server advertised `textDocument/diagnostic` (or is known to
    /// handle it). When false, skip the pull request — some servers crash on
    /// unknown methods.
    fn supports_pull(&self) -> bool;

    /// `textDocument/diagnostic` for `path`. The document should already be
    /// open.
    fn pull(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError>;

    /// `textDocument/didOpen`. Push diagnostics usually start here; pull
    /// servers also want the buffer in sync.
    fn did_open(&self, path: &RelPath) -> Result<(), LspError>;

    /// `textDocument/didClose`. Pair with every successful `didOpen` so the
    /// server does not retain the document indefinitely.
    fn did_close(&self, path: &RelPath) -> Result<(), LspError>;

    /// Wait up to `timeout` for a `publishDiagnostics` notification for `path`.
    ///
    /// Return [`PushWait::Received`] as soon as one arrives (including an empty
    /// payload). Return [`PushWait::NeverReceived`] when the timer fires
    /// without a notification — never invent an empty Exact answer.
    fn wait_push(&self, path: &RelPath, timeout: Duration) -> Result<PushWait, LspError>;
}

/// [`DiagnosticClient`] adapter over the pooled [`LanguageServer`] trait.
///
/// Treats `diagnostics()` as a pull. There is no push channel on this path, so
/// a pull timeout or protocol error degrades to [`Confidence::Unknown`] rather
/// than looking like a clean file.
pub struct ServerAdapter<'a> {
    pub server: &'a dyn LanguageServer,
}

impl DiagnosticClient for ServerAdapter<'_> {
    fn supports_pull(&self) -> bool {
        true
    }

    fn pull(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
        self.server.diagnostics(path)
    }

    fn did_open(&self, _path: &RelPath) -> Result<(), LspError> {
        // The LanguageServer implementation owns document lifetime.
        Ok(())
    }

    fn did_close(&self, _path: &RelPath) -> Result<(), LspError> {
        Ok(())
    }

    fn wait_push(&self, _path: &RelPath, _timeout: Duration) -> Result<PushWait, LspError> {
        Ok(PushWait::NeverReceived)
    }
}

/// Fetch diagnostics using pull, with push as the fallback.
///
/// Selection:
/// 1. `didOpen` (always; push needs it, pull usually does too).
/// 2. If [`DiagnosticClient::supports_pull`], send `textDocument/diagnostic`.
///    Success — including an empty list — is [`Confidence::Exact`].
/// 3. If pull is unsupported, or returns a protocol error / timeout, wait for
///    `publishDiagnostics` up to [`FetchOptions::push_timeout`].
/// 4. A received push (even empty) is Exact. A timeout with no push is
///    [`Confidence::Unknown`].
/// 5. `didClose` runs on the way out, including after errors.
pub fn query(
    client: &dyn DiagnosticClient,
    path: &RelPath,
    options: FetchOptions,
) -> Precise<Vec<Diagnostic>> {
    let _guard = match DocumentGuard::open(client, path) {
        Ok(guard) => guard,
        Err(err) => return unknown_from_err(&err, path),
    };

    if client.supports_pull() {
        match client.pull(path) {
            Ok(diagnostics) => return finalize(diagnostics, options.filter),
            Err(err) if pull_should_fall_back(&err) => {
                tracing::debug!(
                    path = %path,
                    error = %err,
                    "textDocument/diagnostic failed; falling back to publishDiagnostics"
                );
            }
            Err(err) => return unknown_from_err(&err, path),
        }
    }

    match client.wait_push(path, options.push_timeout) {
        Ok(PushWait::Received(diagnostics)) => finalize(diagnostics, options.filter),
        Ok(PushWait::NeverReceived) => {
            Precise::unknown(Vec::new(), never_received_note(path, options.push_timeout))
        }
        Err(err) => unknown_from_err(&err, path),
    }
}

/// Fetch diagnostics through a pooled [`LanguageServer`].
///
/// `None` is the common "nothing is installed" case and must surface as
/// [`Confidence::Unknown`], not an empty Exact list. When `Some`, this wraps
/// `diagnostics()` as a pull; a timeout or protocol error still degrades to
/// Unknown rather than looking like a clean file.
pub fn query_server(
    server: Option<&dyn LanguageServer>,
    path: &RelPath,
    options: FetchOptions,
) -> Precise<Vec<Diagnostic>> {
    match server {
        Some(server) => query(&ServerAdapter { server }, path, options),
        None => Precise::unknown(Vec::new(), no_server_note(path)),
    }
}

/// Convert an LSP diagnostic onto `path`.
///
/// A missing severity is treated as [`Severity::Error`] so an agent cannot
/// miss a problem because the server omitted the field.
pub fn from_lsp_diagnostic(path: RelPath, diagnostic: &lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        path,
        range: Range {
            start: from_lsp_position(diagnostic.range.start),
            end: from_lsp_position(diagnostic.range.end),
        },
        severity: from_lsp_severity(diagnostic.severity),
        message: diagnostic.message.clone(),
        code: diagnostic.code.as_ref().map(code_to_string),
    }
}

/// Items from a pull-report, or `None` when the server said "unchanged".
///
/// Transport should keep the previous snapshot on `None`. Related documents
/// are ignored here; the caller asked about one file.
pub fn from_document_diagnostic_report(
    path: RelPath,
    report: &lsp_types::DocumentDiagnosticReport,
) -> Option<Vec<Diagnostic>> {
    match report {
        lsp_types::DocumentDiagnosticReport::Full(full) => Some(
            full.full_document_diagnostic_report
                .items
                .iter()
                .map(|item| from_lsp_diagnostic(path.clone(), item))
                .collect(),
        ),
        lsp_types::DocumentDiagnosticReport::Unchanged(_) => None,
    }
}

struct DocumentGuard<'a> {
    client: &'a dyn DiagnosticClient,
    path: &'a RelPath,
}

impl<'a> DocumentGuard<'a> {
    fn open(client: &'a dyn DiagnosticClient, path: &'a RelPath) -> Result<Self, LspError> {
        client.did_open(path)?;
        Ok(DocumentGuard { client, path })
    }
}

impl Drop for DocumentGuard<'_> {
    fn drop(&mut self) {
        if let Err(err) = self.client.did_close(self.path) {
            tracing::debug!(
                path = %self.path,
                error = %err,
                "textDocument/didClose failed"
            );
        }
    }
}

fn pull_should_fall_back(err: &LspError) -> bool {
    // Method-not-found and hung unknown-method requests are how old servers
    // reject pull. Unavailable / crash / startup failure mean there is no
    // server to wait on.
    matches!(err, LspError::Protocol(_) | LspError::Timeout(_))
}

fn finalize(mut diagnostics: Vec<Diagnostic>, filter: SeverityFilter) -> Precise<Vec<Diagnostic>> {
    diagnostics.retain(|d| filter.allows(d.severity));
    sort_diagnostics(&mut diagnostics);
    Precise::exact(diagnostics)
}

fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.range.end.cmp(&b.range.end))
            .then_with(|| a.severity.cmp(&b.severity))
            .then_with(|| a.message.cmp(&b.message))
            .then_with(|| a.code.cmp(&b.code))
    });
}

fn unknown_from_err(err: &LspError, path: &RelPath) -> Precise<Vec<Diagnostic>> {
    Precise::unknown(
        Vec::new(),
        format!("could not fetch diagnostics for {path}: {err}"),
    )
}

fn never_received_note(path: &RelPath, timeout: Duration) -> String {
    format!(
        "timed out after {timeout:?} waiting for textDocument/publishDiagnostics on {path}; \
         the server never published, so an empty list does not mean the file is clean"
    )
}

fn no_server_note(path: &RelPath) -> String {
    match Language::from_path(path) {
        Some(language) => format!(
            "no language server available for {language:?}; cannot check diagnostics for {path}"
        ),
        None => format!("no language server available; cannot check diagnostics for {path}"),
    }
}

fn from_lsp_position(position: lsp_types::Position) -> Position {
    Position {
        line: position.line,
        character: position.character,
    }
}

fn from_lsp_severity(severity: Option<lsp_types::DiagnosticSeverity>) -> Severity {
    match severity {
        Some(lsp_types::DiagnosticSeverity::ERROR) => Severity::Error,
        Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Information,
        Some(lsp_types::DiagnosticSeverity::HINT) => Severity::Hint,
        _ => Severity::Error,
    }
}

fn code_to_string(code: &lsp_types::NumberOrString) -> String {
    match code {
        lsp_types::NumberOrString::Number(n) => n.to_string(),
        lsp_types::NumberOrString::String(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::types::{Confidence, Language};

    use super::super::{Location, WorkspaceEdit};
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn diag(path: &str, line: u32, col: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            path: RelPath::new(path),
            range: Range {
                start: pos(line, col),
                end: pos(line, col + 1),
            },
            severity,
            message: message.into(),
            code: None,
        }
    }

    fn keys(diagnostics: &[Diagnostic]) -> Vec<(String, u32, u32, &'static str)> {
        diagnostics
            .iter()
            .map(|d| {
                let sev = match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                    Severity::Information => "info",
                    Severity::Hint => "hint",
                };
                (
                    d.path.as_str().to_string(),
                    d.range.start.line,
                    d.range.start.character,
                    sev,
                )
            })
            .collect()
    }

    #[derive(Clone)]
    enum PullBehavior {
        Ok(Vec<Diagnostic>),
        Fail(FakeError),
    }

    #[derive(Clone)]
    enum PushBehavior {
        Received(Vec<Diagnostic>),
        Never,
        Fail(FakeError),
    }

    #[derive(Clone, Copy)]
    enum FakeError {
        Unavailable,
        Timeout,
        Protocol,
        Crashed,
        Startup,
    }

    impl FakeError {
        fn to_lsp(self) -> LspError {
            match self {
                Self::Unavailable => LspError::Unavailable(
                    Language::Rust,
                    "rust-analyzer is not installed; rustup component add rust-analyzer".into(),
                ),
                Self::Timeout => LspError::Timeout(Duration::from_secs(2)),
                Self::Protocol => {
                    LspError::Protocol("method not found: textDocument/diagnostic".into())
                }
                Self::Crashed => LspError::Crashed,
                Self::Startup => LspError::Startup(Language::Rust, "spawn failed".into()),
            }
        }
    }

    struct FakeServer {
        supports_pull: bool,
        pull: PullBehavior,
        push: PushBehavior,
        open_error: Option<FakeError>,
        events: Mutex<Vec<String>>,
        waited: Mutex<Vec<Duration>>,
    }

    impl FakeServer {
        fn pull_ok(diagnostics: Vec<Diagnostic>) -> Self {
            FakeServer {
                supports_pull: true,
                pull: PullBehavior::Ok(diagnostics),
                push: PushBehavior::Never,
                open_error: None,
                events: Mutex::new(Vec::new()),
                waited: Mutex::new(Vec::new()),
            }
        }

        fn push_only(diagnostics: Vec<Diagnostic>) -> Self {
            FakeServer {
                supports_pull: false,
                pull: PullBehavior::Fail(FakeError::Protocol),
                push: PushBehavior::Received(diagnostics),
                open_error: None,
                events: Mutex::new(Vec::new()),
                waited: Mutex::new(Vec::new()),
            }
        }

        fn push_silent() -> Self {
            FakeServer {
                supports_pull: false,
                pull: PullBehavior::Fail(FakeError::Protocol),
                push: PushBehavior::Never,
                open_error: None,
                events: Mutex::new(Vec::new()),
                waited: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().expect("events lock").clone()
        }

        fn record(&self, event: impl Into<String>) {
            self.events.lock().expect("events lock").push(event.into());
        }
    }

    impl LanguageServer for FakeServer {
        fn language(&self) -> Language {
            Language::Rust
        }

        fn hover(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<serde_json::Value, LspError> {
            Ok(serde_json::Value::Null)
        }

        fn references(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<Vec<Location>, LspError> {
            Ok(Vec::new())
        }

        fn definition(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<Vec<Location>, LspError> {
            Ok(Vec::new())
        }

        fn diagnostics(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
            self.pull(path)
        }

        fn prepare_rename(
            &self,
            _path: &RelPath,
            _position: Position,
            _new_name: &str,
        ) -> Result<WorkspaceEdit, LspError> {
            Ok(WorkspaceEdit::default())
        }

        fn memory_bytes(&self) -> Option<u64> {
            None
        }

        fn shutdown(&self) -> Result<(), LspError> {
            Ok(())
        }
    }

    impl DiagnosticClient for FakeServer {
        fn supports_pull(&self) -> bool {
            self.supports_pull
        }

        fn pull(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
            self.record(format!("pull:{path}"));
            match &self.pull {
                PullBehavior::Ok(items) => Ok(items.clone()),
                PullBehavior::Fail(err) => Err(err.to_lsp()),
            }
        }

        fn did_open(&self, path: &RelPath) -> Result<(), LspError> {
            self.record(format!("open:{path}"));
            match self.open_error {
                Some(err) => Err(err.to_lsp()),
                None => Ok(()),
            }
        }

        fn did_close(&self, path: &RelPath) -> Result<(), LspError> {
            self.record(format!("close:{path}"));
            Ok(())
        }

        fn wait_push(&self, path: &RelPath, timeout: Duration) -> Result<PushWait, LspError> {
            self.record(format!("wait:{path}"));
            self.waited.lock().expect("waited lock").push(timeout);
            match &self.push {
                PushBehavior::Received(items) => Ok(PushWait::Received(items.clone())),
                PushBehavior::Never => Ok(PushWait::NeverReceived),
                PushBehavior::Fail(err) => Err(err.to_lsp()),
            }
        }
    }

    #[test]
    fn pull_mode_returns_exact_diagnostics() {
        let path = RelPath::new("src/lib.rs");
        let server = FakeServer::pull_ok(vec![diag(
            "src/lib.rs",
            3,
            1,
            Severity::Error,
            "missing type",
        )]);
        let got = query(&server, &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Exact);
        assert!(got.note.is_none());
        assert_eq!(got.value.len(), 1);
        assert_eq!(got.value[0].message, "missing type");
        assert_eq!(
            server.events(),
            vec![
                "open:src/lib.rs".to_string(),
                "pull:src/lib.rs".to_string(),
                "close:src/lib.rs".to_string(),
            ]
        );
    }

    #[test]
    fn pull_empty_list_is_exact_not_unknown() {
        // A successful pull that returns nothing is the server asserting the
        // file is clean. That must not be confused with a missing push.
        let path = RelPath::new("src/clean.rs");
        let server = FakeServer::pull_ok(Vec::new());
        let got = query(&server, &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Exact);
        assert!(got.value.is_empty());
        assert!(!server.events().iter().any(|e| e.starts_with("wait:")));
    }

    #[test]
    fn pull_via_language_server_trait() {
        let path = RelPath::new("src/lib.rs");
        let server = FakeServer::pull_ok(vec![diag("src/lib.rs", 0, 0, Severity::Error, "E0001")]);
        let as_ls: &dyn LanguageServer = &server;
        let got = query_server(Some(as_ls), &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Exact);
        assert_eq!(got.value[0].message, "E0001");
    }

    #[test]
    fn missing_language_server_is_unknown() {
        let path = RelPath::new("src/lib.rs");
        let got = query_server(None, &path, FetchOptions::default());
        assert_eq!(got.confidence, Confidence::Unknown);
        assert!(got.value.is_empty());
        let note = got.note.as_deref().expect("must explain the gap");
        assert!(note.contains("Rust"), "note: {note}");
        assert!(note.contains("cannot check diagnostics"), "note: {note}");
    }

    #[test]
    fn push_mode_returns_after_notification() {
        let path = RelPath::new("src/main.rs");
        let server = FakeServer::push_only(vec![diag(
            "src/main.rs",
            8,
            4,
            Severity::Error,
            "undefined name",
        )]);
        let got = query(&server, &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Exact);
        assert_eq!(got.value[0].message, "undefined name");
        assert_eq!(
            server.events(),
            vec![
                "open:src/main.rs".to_string(),
                "wait:src/main.rs".to_string(),
                "close:src/main.rs".to_string(),
            ]
        );
    }

    #[test]
    fn push_published_empty_list_is_exact() {
        let path = RelPath::new("src/ok.rs");
        let server = FakeServer::push_only(Vec::new());
        let got = query(&server, &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Exact);
        assert!(got.value.is_empty());
        assert!(got.note.is_none());
    }

    #[test]
    fn push_timeout_without_notification_is_unknown_not_empty_exact() {
        let path = RelPath::new("src/main.rs");
        let server = FakeServer::push_silent();
        let options = FetchOptions {
            filter: SeverityFilter::All,
            push_timeout: Duration::from_millis(40),
        };
        let got = query(&server, &path, options);

        assert_eq!(
            got.confidence,
            Confidence::Unknown,
            "silence after didOpen is not a clean bill of health"
        );
        assert!(
            got.value.is_empty(),
            "there are no diagnostics to report, but that is not Exact"
        );
        let note = got.note.as_deref().expect("Unknown must explain itself");
        assert!(
            note.contains("publishDiagnostics"),
            "note should name the missing notification: {note}"
        );
        assert!(
            note.contains("never"),
            "note should say the server never published: {note}"
        );
        assert!(
            note.contains("does not mean the file is clean"),
            "note must stop an agent treating this as success: {note}"
        );
        assert_eq!(
            server.waited.lock().expect("waited").as_slice(),
            &[Duration::from_millis(40)]
        );
        assert!(server.events().contains(&"close:src/main.rs".to_string()));
    }

    #[test]
    fn pull_method_not_found_falls_back_to_push() {
        let path = RelPath::new("src/app.rs");
        let server = FakeServer {
            supports_pull: true,
            pull: PullBehavior::Fail(FakeError::Protocol),
            push: PushBehavior::Received(vec![diag(
                "src/app.rs",
                1,
                0,
                Severity::Warning,
                "unused",
            )]),
            open_error: None,
            events: Mutex::new(Vec::new()),
            waited: Mutex::new(Vec::new()),
        };
        let got = query(
            &server,
            &path,
            FetchOptions {
                filter: SeverityFilter::All,
                push_timeout: DEFAULT_PUSH_TIMEOUT,
            },
        );

        assert_eq!(got.confidence, Confidence::Exact);
        assert_eq!(got.value[0].message, "unused");
        assert_eq!(
            server.events(),
            vec![
                "open:src/app.rs".to_string(),
                "pull:src/app.rs".to_string(),
                "wait:src/app.rs".to_string(),
                "close:src/app.rs".to_string(),
            ]
        );
    }

    #[test]
    fn severity_filter_keeps_errors_and_optionally_warnings() {
        let path = RelPath::new("src/mixed.rs");
        let mixed = vec![
            diag("src/mixed.rs", 1, 0, Severity::Hint, "hint"),
            diag("src/mixed.rs", 2, 0, Severity::Error, "error"),
            diag("src/mixed.rs", 3, 0, Severity::Warning, "warning"),
            diag("src/mixed.rs", 4, 0, Severity::Information, "info"),
        ];

        let errors = query(
            &FakeServer::pull_ok(mixed.clone()),
            &path,
            FetchOptions {
                filter: SeverityFilter::Errors,
                push_timeout: DEFAULT_PUSH_TIMEOUT,
            },
        );
        assert_eq!(
            keys(&errors.value),
            vec![("src/mixed.rs".into(), 2, 0, "error")]
        );

        let errors_and_warnings = query(
            &FakeServer::pull_ok(mixed.clone()),
            &path,
            FetchOptions {
                filter: SeverityFilter::ErrorsAndWarnings,
                push_timeout: DEFAULT_PUSH_TIMEOUT,
            },
        );
        assert_eq!(
            keys(&errors_and_warnings.value),
            vec![
                ("src/mixed.rs".into(), 2, 0, "error"),
                ("src/mixed.rs".into(), 3, 0, "warning"),
            ]
        );

        let all = query(
            &FakeServer::pull_ok(mixed),
            &path,
            FetchOptions {
                filter: SeverityFilter::All,
                push_timeout: DEFAULT_PUSH_TIMEOUT,
            },
        );
        assert_eq!(all.value.len(), 4);
    }

    #[test]
    fn results_sort_by_path_then_line_then_column() {
        let path = RelPath::new("src/a.rs");
        let unsorted = vec![
            diag("src/b.rs", 10, 2, Severity::Error, "b"),
            diag("src/a.rs", 4, 8, Severity::Error, "a-late"),
            diag("src/a.rs", 1, 3, Severity::Error, "a-mid"),
            diag("src/a.rs", 1, 0, Severity::Error, "a-early"),
        ];
        let got = query(
            &FakeServer::pull_ok(unsorted),
            &path,
            FetchOptions {
                filter: SeverityFilter::All,
                push_timeout: DEFAULT_PUSH_TIMEOUT,
            },
        );

        assert_eq!(
            keys(&got.value),
            vec![
                ("src/a.rs".into(), 1, 0, "error"),
                ("src/a.rs".into(), 1, 3, "error"),
                ("src/a.rs".into(), 4, 8, "error"),
                ("src/b.rs".into(), 10, 2, "error"),
            ]
        );
    }

    #[test]
    fn unavailable_server_is_unknown_with_install_hint() {
        let path = RelPath::new("src/lib.rs");
        let server = FakeServer {
            supports_pull: true,
            pull: PullBehavior::Fail(FakeError::Unavailable),
            push: PushBehavior::Never,
            open_error: None,
            events: Mutex::new(Vec::new()),
            waited: Mutex::new(Vec::new()),
        };
        let as_ls: &dyn LanguageServer = &server;
        let got = query_server(Some(as_ls), &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Unknown);
        assert!(got.value.is_empty());
        let note = got.note.as_deref().expect("Unavailable must be explained");
        assert!(
            note.contains("rust-analyzer"),
            "agent needs an install hint: {note}"
        );
        assert!(
            !server.events().iter().any(|e| e.starts_with("wait:")),
            "do not wait for a push from a server that is not there"
        );

        // Same failure through DiagnosticClient still pairs didClose with didOpen.
        let client = FakeServer {
            supports_pull: true,
            pull: PullBehavior::Fail(FakeError::Unavailable),
            push: PushBehavior::Never,
            open_error: None,
            events: Mutex::new(Vec::new()),
            waited: Mutex::new(Vec::new()),
        };
        let via_client = query(&client, &path, FetchOptions::default());
        assert_eq!(via_client.confidence, Confidence::Unknown);
        assert_eq!(
            client.events(),
            vec![
                "open:src/lib.rs".to_string(),
                "pull:src/lib.rs".to_string(),
                "close:src/lib.rs".to_string(),
            ]
        );
    }

    #[test]
    fn pull_timeout_falls_back_to_push_then_unknown_if_silent() {
        let path = RelPath::new("src/lib.rs");
        let server = FakeServer {
            supports_pull: true,
            pull: PullBehavior::Fail(FakeError::Timeout),
            push: PushBehavior::Never,
            open_error: None,
            events: Mutex::new(Vec::new()),
            waited: Mutex::new(Vec::new()),
        };
        let got = query(
            &server,
            &path,
            FetchOptions {
                filter: SeverityFilter::All,
                push_timeout: Duration::from_millis(40),
            },
        );
        assert_eq!(got.confidence, Confidence::Unknown);
        assert!(got
            .note
            .as_deref()
            .unwrap_or("")
            .contains("publishDiagnostics"));
        assert_eq!(
            server.events(),
            vec![
                "open:src/lib.rs".to_string(),
                "pull:src/lib.rs".to_string(),
                "wait:src/lib.rs".to_string(),
                "close:src/lib.rs".to_string(),
            ]
        );
    }

    #[test]
    fn push_wait_error_is_unknown() {
        let path = RelPath::new("src/lib.rs");
        let server = FakeServer {
            supports_pull: false,
            pull: PullBehavior::Fail(FakeError::Protocol),
            push: PushBehavior::Fail(FakeError::Crashed),
            open_error: None,
            events: Mutex::new(Vec::new()),
            waited: Mutex::new(Vec::new()),
        };
        let got = query(&server, &path, FetchOptions::default());
        assert_eq!(got.confidence, Confidence::Unknown);
        assert!(got
            .note
            .as_deref()
            .unwrap_or("")
            .contains("exited unexpectedly"));
        assert!(server.events().contains(&"close:src/lib.rs".to_string()));
    }

    #[test]
    fn crashed_and_startup_errors_are_unknown() {
        let path = RelPath::new("src/lib.rs");
        for err in [FakeError::Crashed, FakeError::Startup] {
            let server = FakeServer {
                supports_pull: true,
                pull: PullBehavior::Fail(err),
                push: PushBehavior::Never,
                open_error: None,
                events: Mutex::new(Vec::new()),
                waited: Mutex::new(Vec::new()),
            };
            let got = query(&server, &path, FetchOptions::default());
            assert_eq!(got.confidence, Confidence::Unknown);
            assert!(got.note.is_some());
        }
    }

    #[test]
    fn did_open_failure_is_unknown_and_skips_close() {
        let path = RelPath::new("src/lib.rs");
        let server = FakeServer {
            supports_pull: true,
            pull: PullBehavior::Ok(Vec::new()),
            push: PushBehavior::Never,
            open_error: Some(FakeError::Unavailable),
            events: Mutex::new(Vec::new()),
            waited: Mutex::new(Vec::new()),
        };
        let got = query(&server, &path, FetchOptions::default());

        assert_eq!(got.confidence, Confidence::Unknown);
        assert_eq!(server.events(), vec!["open:src/lib.rs".to_string()]);
    }

    #[test]
    fn default_push_timeout_is_two_and_a_half_seconds() {
        assert_eq!(DEFAULT_PUSH_TIMEOUT, Duration::from_millis(2500));
        assert_eq!(FetchOptions::default().push_timeout, DEFAULT_PUSH_TIMEOUT);
    }

    #[test]
    fn lsp_conversion_maps_range_severity_and_code() {
        let lsp = lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(2, 4),
                lsp_types::Position::new(2, 9),
            ),
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            code: Some(lsp_types::NumberOrString::Number(2304)),
            message: "not found".into(),
            ..lsp_types::Diagnostic::default()
        };
        let converted = from_lsp_diagnostic(RelPath::new("a.ts"), &lsp);
        assert_eq!(converted.path.as_str(), "a.ts");
        assert_eq!(converted.range.start, pos(2, 4));
        assert_eq!(converted.range.end, pos(2, 9));
        assert_eq!(converted.severity, Severity::Warning);
        assert_eq!(converted.message, "not found");
        assert_eq!(converted.code.as_deref(), Some("2304"));
    }

    #[test]
    fn omitted_lsp_severity_is_treated_as_error() {
        let lsp = lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 1),
            ),
            severity: None,
            message: "something".into(),
            ..lsp_types::Diagnostic::default()
        };
        let converted = from_lsp_diagnostic(RelPath::new("a.rs"), &lsp);
        assert_eq!(converted.severity, Severity::Error);
    }
}
