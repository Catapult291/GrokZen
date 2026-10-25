//! In-process Astrolabe engine for Grok Build.
//!
//! The seven Astrolabe tools are served from one shared index per workspace
//! root. Ranking, pagination, body rendering and the freshness barrier come
//! from the vendored `astrolabe-mcp` tool layer; this crate adds the parts a
//! Grok host needs: path mapping between the index root and the session cwd,
//! read-policy filtering, host-level truncation, an engine registry, and the
//! bridge to the host language server for the precise tools.
//!
//! The crate deliberately exposes no `astrolabe_*` types: [`types`] holds the
//! request/response contract the tool crate uses, and [`paths`] holds the
//! `PathMap`/`DenyPolicy` pair the tool layer builds from session resources.

pub mod config;
pub mod engine;
pub mod paths;
pub mod precise;
pub mod render;
pub mod types;

pub use config::{
    AstrolabeConfig, AstrolabeMode, AstrolabeToolsetConfig, IndexPersist, IndexRoot, LspFallback,
};
pub use engine::{
    AstrolabeEngine, EngineError, engine_for, engine_for_session, install_config,
    note_edited_on_disk, process_config, root_for_cwd, shutdown_all,
};
pub use paths::{DenyPolicy, PathError, PathMap};
pub use types::{
    Anchor, CancelHandle, DiagnosticsRequest, EngineMeta, EngineOutput, FindReferencesRequest,
    FindSymbolRequest, GotoDefinitionRequest, LanguagesRequest, LspDiagnostic, LspLocation,
    LspSeverity, PreciseOutcome, ResolveContextRequest, SearchCodeRequest,
};
