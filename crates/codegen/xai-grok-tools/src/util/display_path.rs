//! Model-visible path rendering.
//!
//! Paths surfaced to the model (error echoes, suggestions) must use a single
//! separator convention regardless of the host platform so that model-visible
//! strings stay stable. Forward slashes are the convention (POSIX-style
//! test/tooling expectations); on Unix this is a no-op.

use std::path::Path;

/// Render a path as a string with all separators normalized to `/`.
pub fn render(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().replace('\\', "/")
}
