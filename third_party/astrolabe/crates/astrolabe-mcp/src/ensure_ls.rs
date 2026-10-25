//! Default language-server installer used by `get_languages`.

use astrolabe_core::lsp::install::StubInstaller;

/// Default process-PATH installer used by the MCP server.
pub(crate) fn default_installer() -> StubInstaller {
    StubInstaller::from_process()
}
