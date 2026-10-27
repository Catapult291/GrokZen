//! Failure classification for a codemode script run.

use thiserror::Error;

/// Failure classification. [`CodemodeErrorKind::code`] is both the
/// model-visible error prefix and the default `ToolError` code surfaced by
/// the meta-tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodemodeErrorKind {
    /// Wall-clock deadline hit (interrupt handler or outer timeout).
    Timeout,
    /// QuickJS out-of-memory or stack overflow under the sandbox cap.
    Oom,
    /// A host (`searchTool`/`useTool`) call failed. The original tool error
    /// code is preserved in [`CodemodeError::code`] when one exists.
    Host,
    /// Script failed to parse.
    Syntax,
    /// Host call budget exhausted.
    CallLimit,
    /// `JSON.stringify` of a host-call argument failed (circular, BigInt, function).
    InvalidArgument,
    /// Any other in-script failure (e.g. a bare `throw`).
    Runtime,
}

impl CodemodeErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "codemode_timeout",
            Self::Oom => "codemode_oom",
            Self::Host => "codemode_host_error",
            Self::Syntax => "codemode_syntax",
            Self::CallLimit => "codemode_call_limit",
            Self::InvalidArgument => "codemode_invalid_argument",
            Self::Runtime => "codemode_error",
        }
    }
}

/// Sandbox execution failure.
#[derive(Debug, Error)]
#[error("{code}: {message}")]
pub struct CodemodeError {
    pub kind: CodemodeErrorKind,
    /// Error code: the original tool error code for host failures, otherwise
    /// the `codemode_*` prefix for the kind.
    pub code: String,
    pub message: String,
}

impl CodemodeError {
    pub fn new(kind: CodemodeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: kind.code().to_string(),
            message: message.into(),
        }
    }

    /// Host-call failure that preserves the original tool error code.
    pub fn host(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: CodemodeErrorKind::Host,
            code: code.into(),
            message: message.into(),
        }
    }

    /// Classification prefix for telemetry (always `codemode_*`, even when
    /// `code` carries a passthrough tool error code).
    pub fn kind_code(&self) -> &'static str {
        self.kind.code()
    }
}
