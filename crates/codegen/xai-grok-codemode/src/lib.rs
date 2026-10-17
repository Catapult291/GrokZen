//! Codemode sandbox: run a model-written JavaScript program that orchestrates
//! harness-side tool calls (`searchTool` / `useTool`). Intermediate tool JSON
//! stays inside this sandbox; only the script's final JSON-safe return value
//! is handed back to the model.
//!
//! The runtime is engine-only (QuickJS via rquickjs): no session, no MCP, no
//! permission types. Callers inject a [`HostTools`] implementation.

mod error;
mod host;
mod runtime;

pub use error::{CodemodeError, CodemodeErrorKind};
pub use host::{HostCallError, HostFuture, HostTools};
pub use runtime::{execute, ExecOutcome, Limits};
