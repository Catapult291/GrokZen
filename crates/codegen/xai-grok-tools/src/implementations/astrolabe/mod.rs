//! Native Astrolabe tools: `search_code`, `find_symbol`, `resolve_context`,
//! `get_languages`, `find_references`, `goto_definition`, `get_diagnostics`.
//!
//! Each tool is a thin adapter over `xai-grok-astrolabe`: it resolves the
//! session cwd, the display cwd and the read-deny policy from `Resources`,
//! hands them to the shared engine on a blocking thread, and returns the
//! engine's rendered text. All ranking, pagination and body rendering live in
//! the engine; this layer owns the tool metadata, permissions and the mapping
//! onto `ToolInput`/`ToolOutput`.
//!
//! The precise tools are index-first. When the engine reports that only a
//! language server can answer, this layer asks the session's LSP backend
//! (the same `Arc<dyn LspBackend>` the `lsp` tool uses) and renders those
//! locations under the same path/line contract.

use std::sync::Arc;

use xai_grok_astrolabe::{
    CancelHandle, DenyPolicy, DiagnosticsRequest, FindReferencesRequest, FindSymbolRequest,
    GotoDefinitionRequest, LanguagesRequest, LspDiagnostic, LspFallback, LspLocation, LspSeverity,
    PathMap, ResolveContextRequest, SearchCodeRequest, engine_for_session, root_for_cwd,
};

use crate::implementations::lsp::{
    DiagnosticSeverityLevel, LspBackend, PreciseDiagnostic, PreciseLocation,
};
use crate::types::output::{TextOutput, ToolOutput};
use crate::types::resources::{
    DenyReadGlobs, DisplayCwd, FileSystem, RespectGitignore, SharedResources,
};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, resolve_cwd, shared_resources};

pub const SEARCH_CODE_DESC: &str = r#"Exhaustive literal/regex search over the indexed workspace. Hits include the enclosing function/type body — often no read_file needed. Limit paths/file types with glob. Wide results summarize per file; page with offset; aggregate for counts. Respects .gitignore and the workspace read policy. Newlines unsupported — read the file or search each line. Outside root: use read_file. Code-map: resolve_context; call sites: find_references."#;

pub const FIND_SYMBOL_DESC: &str = r#"Locate a symbol by name, prefix, `Class/method` path, or file outline; returns its signature and path:line. For full type information use lsp."#;

// Task-start routing: match the eval phrasings ("which files and symbols",
// "which parts implement", "map of the code") and force the first call.
pub const RESOLVE_CONTEXT_DESC: &str = r#"First when asked which files/symbols are involved, which parts implement X, or a map of the code: task-ranked related files and symbols."#;

pub const GET_LANGUAGES_DESC: &str = r#"Indexed languages, file and LOC counts, index gaps, language-server readiness, and which languages answer precise queries from the index."#;

pub const FIND_REFERENCES_DESC: &str = r#"Precise references for a known symbol: callers, usages, rename impact. Call sites → this first, not search_code. Index-first for Go and unique top-level TypeScript/TSX/Python names; otherwise the workspace language server answers. `dry_run_rename=NewName` returns a read-only per-file checklist. Lines are 1-based."#;

pub const GOTO_DEFINITION_DESC: &str = r#"Resolve a symbol's definition from a file and line, or by name; index-first for Go, language server for other languages. Lines are 1-based."#;

pub const GET_DIAGNOSTICS_DESC: &str = r#"Parser/language-server diagnostics for one file; the toolchain never runs. Not a build: run cargo check / go build / tsc for project-level truth."#;

/// `search_code` params. The toolset flips `hashline_note` on when the
/// hashline file toolset is active: bodies returned here are for reading and
/// carry no edit anchors, and the model needs to be told where to get them.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCodeParams {
    #[serde(default)]
    pub hashline_note: bool,
}

crate::register_resource!("grok_build", "SearchCode", SearchCodeParams);

const HASHLINE_NOTE: &str =
    "note: these bodies carry no edit anchors; use hashline_read to get anchors for hashline_edit.";

/// Whether the index tools should appear in the model manifest.
///
/// `GROK_ASTROLABE=off` drops them at toolset finalization, before `by_kind`
/// is resolved: leaving them out later would make `${{ tools.by_kind.search }}`
/// name a tool that is not listed.
pub fn listing_enabled(registry_id: &str) -> bool {
    listing_enabled_for(xai_grok_astrolabe::process_config().mode, registry_id)
}

/// Registry ids of the precise-layer tools.
pub const PRECISE_TOOL_IDS: [&str; 3] = [
    "GrokBuild:find_references",
    "GrokBuild:goto_definition",
    "GrokBuild:get_diagnostics",
];

/// Registry ids of every tool this module owns.
pub const TOOL_IDS: [&str; 7] = [
    "GrokBuild:search_code",
    "GrokBuild:find_symbol",
    "GrokBuild:resolve_context",
    "GrokBuild:get_languages",
    "GrokBuild:find_references",
    "GrokBuild:goto_definition",
    "GrokBuild:get_diagnostics",
];

/// `true` for a tool served by the in-process index. The toolset drops these
/// for a remote or sandboxed file system, where local indexing would read a
/// different tree than `read_file`.
pub fn is_astrolabe_tool(registry_id: &str) -> bool {
    TOOL_IDS.contains(&registry_id)
}

/// [`listing_enabled`] plus the file-system requirement: a remote or
/// sandboxed backend hides the index tools and leaves everything else alone.
pub fn listed(registry_id: &str, local_fs: bool) -> bool {
    listed_for(
        xai_grok_astrolabe::process_config().mode,
        registry_id,
        local_fs,
    )
}

/// [`listed`] with an explicit mode (pure, testable).
pub fn listed_for(
    mode: xai_grok_astrolabe::AstrolabeMode,
    registry_id: &str,
    local_fs: bool,
) -> bool {
    listing_enabled_for(mode, registry_id) && (local_fs || !is_astrolabe_tool(registry_id))
}

/// `true` for one of the precise tools. Toolset builders use it to place
/// `lsp` ahead of them: `by_kind.lsp` is first-come, so the generic tool must
/// keep that slot.
pub fn is_precise_tool(registry_id: &str) -> bool {
    PRECISE_TOOL_IDS.contains(&registry_id)
}

/// [`listing_enabled`] with an explicit mode (pure, testable).
pub fn listing_enabled_for(mode: xai_grok_astrolabe::AstrolabeMode, registry_id: &str) -> bool {
    let index_tool = matches!(
        registry_id,
        "GrokBuild:search_code"
            | "GrokBuild:find_symbol"
            | "GrokBuild:resolve_context"
            | "GrokBuild:get_languages"
    );
    (!index_tool || mode.index_tools()) && (!is_precise_tool(registry_id) || mode.precise_tools())
}

/// Session-scoped inputs shared by every Astrolabe tool.
struct CallEnv {
    paths: PathMap,
    deny: DenyPolicy,
    hashline_note: bool,
    respect_gitignore: bool,
}

/// Why the index tools refuse a session whose files do not live on this
/// machine: the engine reads bytes from local disk, so indexing a remote or
/// sandboxed workspace would answer from the wrong tree.
const REMOTE_WORKSPACE_NOTE: &str = "astrolabe index tools need a local workspace. This session's file system is remote or \
     sandboxed, so an in-process index would read different bytes than read_file. Use read_file \
     instead, or set [toolset.astrolabe] mode = \"off\" to hide these tools.";

async fn call_env(
    ctx: &xai_tool_runtime::ToolCallContext,
    resources: &SharedResources,
) -> Result<CallEnv, xai_tool_runtime::ToolError> {
    let cwd = resolve_cwd(ctx, resources).await?;
    let (display, deny, hashline_note, respect_gitignore, is_local) = {
        let res = resources.lock().await;
        (
            res.get::<DisplayCwd>().map(|d| d.0.clone()),
            res.get::<DenyReadGlobs>()
                .map(|d| d.0.clone())
                .unwrap_or_default(),
            res.get::<crate::types::resources::Params<SearchCodeParams>>()
                .is_some_and(|p| p.0.hashline_note),
            res.get::<RespectGitignore>().is_none_or(|r| r.0),
            // Only a backend that explicitly marks itself remote disables the
            // index; a missing resource keeps the historical local behavior.
            res.get::<FileSystem>().is_none_or(|fs| fs.0.is_local()),
        )
    };
    if !is_local {
        return Err(xai_tool_runtime::ToolError::custom(
            "astrolabe",
            REMOTE_WORKSPACE_NOTE,
        ));
    }
    let cfg = xai_grok_astrolabe::process_config();
    let root = root_for_cwd(&cwd, cfg);
    let paths = PathMap::new(root, cwd, display);
    Ok(CallEnv {
        deny: DenyPolicy::for_root(paths.root(), &deny),
        paths,
        hashline_note,
        respect_gitignore,
    })
}

/// The shared engine for this session's root, ignore policy and deny set.
async fn open_engine(
    env: &CallEnv,
) -> Result<Arc<xai_grok_astrolabe::AstrolabeEngine>, xai_tool_runtime::ToolError> {
    let root = env.paths.root().to_path_buf();
    let cfg = xai_grok_astrolabe::process_config().clone();
    let respect_gitignore = env.respect_gitignore;
    let deny = env.deny.clone();
    blocking(move || engine_for_session(&root, &cfg, respect_gitignore, &deny))
        .await?
        .map_err(engine_error)
}

fn engine_error(err: xai_grok_astrolabe::EngineError) -> xai_tool_runtime::ToolError {
    xai_tool_runtime::ToolError::custom("astrolabe", err.to_string())
}

async fn blocking<T, F>(f: F) -> Result<T, xai_tool_runtime::ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| xai_tool_runtime::ToolError::custom("astrolabe", e.to_string()))
}

/// A short index lookup that honors turn cancellation: `None` when the turn is
/// already cancelled, and no further wait once it is cancelled mid-flight.
///
/// Unlike `search_code`, the engine lookup has no per-file cancel checkpoint,
/// so dropping the join only ends the wait — the blocking task finishes its
/// query in the background. That still hands the turn back promptly, which is
/// what matters for a lookup this short.
async fn blocking_lookup<T, F>(
    ctx: &xai_tool_runtime::ToolCallContext,
    f: F,
) -> Result<Option<T>, xai_tool_runtime::ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let cancellation = ctx
        .get::<xai_tool_runtime::Cancellation>()
        .map(|c| c.0.clone());
    if cancellation
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return Ok(None);
    }
    let mut handle = tokio::task::spawn_blocking(f);
    let joined = |res: Result<T, tokio::task::JoinError>| {
        res.map_err(|e| xai_tool_runtime::ToolError::custom("astrolabe", e.to_string()))
    };
    match cancellation {
        Some(token) => tokio::select! {
            res = &mut handle => Ok(Some(joined(res)?)),
            _ = token.cancelled() => Ok(None),
        },
        None => Ok(Some(joined((&mut handle).await)?)),
    }
}

/// Flips an engine cancel flag whenever it drops. The dispatcher cancels a
/// tool by dropping its future, so the guard (not just the token arm) is what
/// stops the scan when the turn goes away.
struct CancelOnDrop(CancelHandle);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

const SEARCH_CANCELLED: &str =
    "(search cancelled before it finished; narrow the query with path/glob and retry.)";

const LOOKUP_CANCELLED: &str =
    "(lookup cancelled before it finished; retry or narrow the query with path.)";

fn text_output(text: String) -> ToolOutput {
    ToolOutput::Text(TextOutput {
        text,
        consumed_completion_task_id: None,
    })
}

/// The session's language-server backend, when one is installed.
async fn host_lsp(resources: &SharedResources) -> Option<Arc<dyn LspBackend>> {
    let res = resources.lock().await;
    res.get::<Arc<dyn LspBackend>>().cloned()
}

fn host_lsp_allowed() -> bool {
    xai_grok_astrolabe::process_config().lsp_fallback == LspFallback::Grok
}

fn to_lsp_locations(found: Vec<PreciseLocation>) -> Vec<LspLocation> {
    found
        .into_iter()
        .map(|location| LspLocation {
            path: location.path,
            line: location.line,
            character: location.character,
            end_line: location.end_line,
            end_character: location.end_character,
        })
        .collect()
}

fn to_lsp_diagnostics(found: Vec<PreciseDiagnostic>) -> Vec<LspDiagnostic> {
    found
        .into_iter()
        .map(|diagnostic| LspDiagnostic {
            line: diagnostic.line,
            character: diagnostic.character,
            severity: match diagnostic.severity {
                DiagnosticSeverityLevel::Error => LspSeverity::Error,
                DiagnosticSeverityLevel::Warning => LspSeverity::Warning,
            },
            message: diagnostic.message,
            source: diagnostic.source,
            code: diagnostic.code,
        })
        .collect()
}

/// Append a note and re-apply the host byte ceiling.
fn with_note(text: String, note: &str, budget_bytes: usize) -> String {
    let mut out = text.trim_end().to_string();
    if !note.is_empty() {
        out.push_str("\n\n");
        out.push_str(note);
    }
    xai_grok_astrolabe::render::truncate(out, budget_bytes)
}

/// One failure sentence for the `lsp_fallback = grok` dead end, used when no
/// host server is registered and when the registered pool cannot answer. The
/// `lsp` tool's unavailable error names the same config files, so the model has
/// one fix to apply.
fn no_host_lsp_note(detail: Option<&str>) -> String {
    let mut note = String::from("note: `lsp_fallback = grok` and ");
    match detail {
        Some(detail) => {
            note.push_str("the workspace language server did not answer (");
            note.push_str(detail.trim());
            note.push(')');
        }
        None => note.push_str("no workspace language server is registered"),
    }
    note.push_str(". ");
    note.push_str(crate::implementations::lsp::LSP_SERVER_SETUP);
    note.push_str(
        " Or set lsp_fallback = \"astrolabe\" to use Astrolabe's built-in pool; get_languages shows configured/Ready.",
    );
    note
}

/// Diagnostics answer "what does the parser or LSP say about this file", not
/// "does the project build": cross-file type checking, features and cfg belong
/// to the build. Kept short on purpose — it rides on every diagnostics reply.
const BUILD_TRUTH_NOTE: &str = "note: single-file diagnostics, not a build. Run the project's build/test (cargo check, go build, tsc) for project-level truth.";

// ---------------------------------------------------------------------------
// search_code
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SearchCodeTool;

impl ToolMetadata for SearchCodeTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        SEARCH_CODE_DESC
    }
}

impl xai_tool_runtime::Tool for SearchCodeTool {
    type Args = SearchCodeRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("search_code").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "search_code",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.search_code", skip_all, fields(query = %input.query))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: SearchCodeRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let cancellation = ctx
            .get::<xai_tool_runtime::Cancellation>()
            .map(|c| c.0.clone());
        if cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Ok(text_output(SEARCH_CANCELLED.to_string()));
        }
        let cfg = xai_grok_astrolabe::process_config().clone();
        let budget_bytes = cfg.budget_bytes;
        let engine = open_engine(&env).await?;
        let paths = env.paths;
        let deny = env.deny;
        let hashline_note = env.hashline_note;
        let cancel = CancelHandle::new();
        let guard = CancelOnDrop(cancel.clone());
        let worker_cancel = cancel.clone();
        let mut handle = tokio::task::spawn_blocking(move || {
            engine.search_code_cancellable(&paths, &deny, input, &worker_cancel)
        });
        let out = match cancellation {
            Some(token) => tokio::select! {
                res = &mut handle => res,
                _ = token.cancelled() => {
                    return Ok(text_output(SEARCH_CANCELLED.to_string()));
                }
            },
            None => (&mut handle).await,
        };
        drop(guard);
        let mut text = out
            .map_err(|e| xai_tool_runtime::ToolError::custom("astrolabe", e.to_string()))?
            .map_err(engine_error)?
            .text;
        if hashline_note {
            text = with_note(text, HASHLINE_NOTE, budget_bytes);
        }
        Ok(text_output(text))
    }
}

// ---------------------------------------------------------------------------
// find_symbol
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct FindSymbolTool;

impl ToolMetadata for FindSymbolTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        FIND_SYMBOL_DESC
    }
}

impl xai_tool_runtime::Tool for FindSymbolTool {
    type Args = FindSymbolRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("find_symbol").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "find_symbol",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.find_symbol", skip_all, fields(query = %input.query))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: FindSymbolRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let engine = open_engine(&env).await?;
        let paths = env.paths;
        let deny = env.deny;
        let out = blocking_lookup(&ctx, move || engine.find_symbol(&paths, &deny, input)).await?;
        Ok(match out {
            Some(out) => text_output(out.map_err(engine_error)?.text),
            None => text_output(LOOKUP_CANCELLED.to_string()),
        })
    }
}

// ---------------------------------------------------------------------------
// resolve_context
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ResolveContextTool;

impl ToolMetadata for ResolveContextTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        RESOLVE_CONTEXT_DESC
    }
}

impl xai_tool_runtime::Tool for ResolveContextTool {
    type Args = ResolveContextRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("resolve_context").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "resolve_context",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.resolve_context", skip_all, fields(task = %input.task))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ResolveContextRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let engine = open_engine(&env).await?;
        let paths = env.paths;
        let deny = env.deny;
        let out =
            blocking_lookup(&ctx, move || engine.resolve_context(&paths, &deny, input)).await?;
        Ok(match out {
            Some(out) => text_output(out.map_err(engine_error)?.text),
            None => text_output(LOOKUP_CANCELLED.to_string()),
        })
    }
}

// ---------------------------------------------------------------------------
// get_languages
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct GetLanguagesTool;

impl ToolMetadata for GetLanguagesTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        GET_LANGUAGES_DESC
    }
}

impl xai_tool_runtime::Tool for GetLanguagesTool {
    type Args = LanguagesRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("get_languages").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "get_languages",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: LanguagesRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let engine = open_engine(&env).await?;
        let lsp = host_lsp(&resources).await;
        // Host pool wins for the LSP column when `lsp_fallback=grok` and a
        // backend is installed; empty/`astrolabe` keeps the PATH probe.
        let host_servers: Vec<xai_grok_astrolabe::HostLspServer> =
            if host_lsp_allowed() && lsp.is_some() {
                lsp.as_ref()
                    .expect("checked above")
                    .list_server_statuses()
                    .await
                    .into_iter()
                    .map(|s| xai_grok_astrolabe::HostLspServer {
                        name: s.name,
                        command: s.command,
                        languages: s.languages,
                        running: s.running,
                    })
                    .collect()
            } else {
                Vec::new()
            };
        let paths = env.paths;
        let deny = env.deny;
        let out =
            blocking(move || engine.get_languages(&paths, &deny, input, &host_servers)).await?;
        Ok(text_output(out.map_err(engine_error)?.text))
    }
}

// ---------------------------------------------------------------------------
// find_references
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct FindReferencesTool;

impl ToolMetadata for FindReferencesTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Lsp
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        FIND_REFERENCES_DESC
    }
}

impl xai_tool_runtime::Tool for FindReferencesTool {
    type Args = FindReferencesRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("find_references").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "find_references",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.find_references", skip_all, fields(symbol = %input.symbol))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: FindReferencesRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let lsp = host_lsp(&resources).await;
        let cfg = xai_grok_astrolabe::process_config().clone();
        let use_host = host_lsp_allowed() && lsp.is_some();
        let symbol = input.symbol.clone();
        let offset = input.offset;
        let dry_run = input.dry_run_rename.clone();
        let engine = open_engine(&env).await?;
        let paths_for_call = env.paths.clone();
        let deny_for_call = env.deny.clone();
        let outcome = blocking(move || {
            engine.find_references(&paths_for_call, &deny_for_call, input, use_host)
        })
        .await?
        .map_err(engine_error)?;

        let xai_grok_astrolabe::PreciseOutcome {
            output,
            needs_host_lsp,
            anchor,
            unknown_reason,
            ..
        } = outcome;

        if needs_host_lsp && let (Some(handle), Some(anchor)) = (lsp.as_ref(), anchor) {
            if env.deny.denies(&anchor.engine_path) {
                return Ok(text_output(with_note(
                    output.text,
                    "note: this file is excluded by the workspace read policy (DenyReadGlobs).",
                    cfg.budget_bytes,
                )));
            }
            match handle
                .precise_references(&anchor.absolute, anchor.line, anchor.character)
                .await
            {
                Ok(found) if !found.is_empty() => {
                    let locations = to_lsp_locations(found);
                    let rendered = xai_grok_astrolabe::precise::render_references(
                        &env.paths,
                        &env.deny,
                        &symbol,
                        Some(&anchor),
                        &locations,
                        offset,
                        dry_run.as_deref(),
                        cfg.budget_bytes,
                    );
                    return Ok(text_output(rendered.text));
                }
                Ok(_) => {
                    let note = "note: the language server reports no references for this symbol at that position.";
                    return Ok(text_output(with_note(output.text, note, cfg.budget_bytes)));
                }
                Err(err) => {
                    let note = no_host_lsp_note(Some(&err));
                    return Ok(text_output(with_note(output.text, &note, cfg.budget_bytes)));
                }
            }
        }

        let mut text = output.text;
        if unknown_reason.is_some() && lsp.is_none() && host_lsp_allowed() {
            text = with_note(text, &no_host_lsp_note(None), cfg.budget_bytes);
        }
        Ok(text_output(text))
    }
}

// ---------------------------------------------------------------------------
// goto_definition
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct GotoDefinitionTool;

impl ToolMetadata for GotoDefinitionTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Lsp
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        GOTO_DEFINITION_DESC
    }
}

impl xai_tool_runtime::Tool for GotoDefinitionTool {
    type Args = GotoDefinitionRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("goto_definition").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "goto_definition",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.goto_definition", skip_all, fields(symbol = input.symbol.as_deref().unwrap_or("")))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: GotoDefinitionRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let lsp = host_lsp(&resources).await;
        let cfg = xai_grok_astrolabe::process_config().clone();
        let use_host = host_lsp_allowed() && lsp.is_some();
        let symbol = input.symbol.clone();
        let engine = open_engine(&env).await?;
        let paths_for_call = env.paths.clone();
        let deny_for_call = env.deny.clone();
        let outcome = blocking(move || {
            engine.goto_definition(&paths_for_call, &deny_for_call, input, use_host)
        })
        .await?
        .map_err(engine_error)?;

        let xai_grok_astrolabe::PreciseOutcome {
            output,
            needs_host_lsp,
            anchor,
            unknown_reason,
            ..
        } = outcome;

        if needs_host_lsp && let (Some(handle), Some(anchor)) = (lsp.as_ref(), anchor) {
            if env.deny.denies(&anchor.engine_path) {
                return Ok(text_output(with_note(
                    output.text,
                    "note: this file is excluded by the workspace read policy (DenyReadGlobs).",
                    cfg.budget_bytes,
                )));
            }
            match handle
                .precise_definition(&anchor.absolute, anchor.line, anchor.character)
                .await
            {
                Ok(found) if !found.is_empty() => {
                    let locations = to_lsp_locations(found);
                    let rendered = xai_grok_astrolabe::precise::render_definitions(
                        &env.paths,
                        &env.deny,
                        symbol.as_deref(),
                        &locations,
                        cfg.budget_bytes,
                    );
                    return Ok(text_output(rendered.text));
                }
                Ok(_) => {
                    let note = "note: the language server reports no definition at that position.";
                    return Ok(text_output(with_note(output.text, note, cfg.budget_bytes)));
                }
                Err(err) => {
                    let note = no_host_lsp_note(Some(&err));
                    return Ok(text_output(with_note(output.text, &note, cfg.budget_bytes)));
                }
            }
        }

        let mut text = output.text;
        if unknown_reason.is_some() && lsp.is_none() && host_lsp_allowed() {
            text = with_note(text, &no_host_lsp_note(None), cfg.budget_bytes);
        }
        Ok(text_output(text))
    }
}

// ---------------------------------------------------------------------------
// get_diagnostics
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct GetDiagnosticsTool;

impl ToolMetadata for GetDiagnosticsTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Lsp
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        GET_DIAGNOSTICS_DESC
    }
}

impl xai_tool_runtime::Tool for GetDiagnosticsTool {
    type Args = DiagnosticsRequest;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("get_diagnostics").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "get_diagnostics",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.get_diagnostics", skip_all, fields(path = %input.path))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: DiagnosticsRequest,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let env = call_env(&ctx, &resources).await?;
        let lsp = host_lsp(&resources).await;
        let cfg = xai_grok_astrolabe::process_config().clone();
        let path_input = input.path.clone();
        let severity = input.severity.clone();
        let engine_rel = env
            .paths
            .engine_rel(&input.path)
            .map_err(|err| xai_tool_runtime::ToolError::custom("astrolabe", err.to_string()))?;
        if env.deny.denies(&engine_rel) {
            return Ok(text_output(
                "excluded by the workspace read policy (DenyReadGlobs)".to_string(),
            ));
        }
        let absolute = env
            .paths
            .absolute(&input.path)
            .map_err(|err| xai_tool_runtime::ToolError::custom("astrolabe", err.to_string()))?;

        // Cached/registered language-server diagnostics first; the parser and
        // toolchain answer when no server covers this file.
        let mut host_error = None;
        if host_lsp_allowed()
            && let Some(handle) = lsp.as_ref()
        {
            match handle.precise_diagnostics(&absolute).await {
                Ok(found) if !found.is_empty() => {
                    let diagnostics = to_lsp_diagnostics(found);
                    let rendered = xai_grok_astrolabe::precise::render_diagnostics(
                        &env.paths,
                        &env.deny,
                        &path_input,
                        &diagnostics,
                        severity.as_deref(),
                        cfg.budget_bytes,
                    );
                    let text = with_note(rendered.text, BUILD_TRUTH_NOTE, cfg.budget_bytes);
                    return Ok(text_output(text));
                }
                Ok(_) => {}
                Err(err) => host_error = Some(err),
            }
        }

        let engine = open_engine(&env).await?;
        let paths = env.paths;
        let deny = env.deny;
        let out = blocking(move || engine.get_diagnostics(&paths, &deny, input))
            .await?
            .map_err(engine_error)?;
        let mut text = out.text;
        if let Some(err) = host_error {
            let note = format!(
                "note: host language server unavailable ({err}). {} Parser result below.",
                crate::implementations::lsp::LSP_SERVER_SETUP
            );
            text = with_note(text, &note, cfg.budget_bytes);
        }
        let text = with_note(text, BUILD_TRUTH_NOTE, cfg.budget_bytes);
        Ok(text_output(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use crate::implementations::lsp::{
        DiagnosticsSummary, DiskChangeKind, FileDiagnosticEntry, HostLspServerStatus, LspToolInput,
        LspToolResult,
    };
    use crate::types::resources::{Cwd, Resources};

    #[test]
    fn descriptions_stay_within_the_plan_budget() {
        for (name, desc, budget) in [
            ("search_code", SEARCH_CODE_DESC, 60),
            ("find_symbol", FIND_SYMBOL_DESC, 35),
            ("resolve_context", RESOLVE_CONTEXT_DESC, 35),
            ("get_languages", GET_LANGUAGES_DESC, 35),
            ("find_references", FIND_REFERENCES_DESC, 60),
            ("goto_definition", GOTO_DEFINITION_DESC, 35),
            ("get_diagnostics", GET_DIAGNOSTICS_DESC, 35),
        ] {
            let words = desc.split_whitespace().count();
            assert!(words <= budget, "{name}: {words} words > {budget}");
        }
    }

    /// The tool table budget from the plan: seven descriptions at or below
    /// 400 tokens in total. Per-description caps keep one tool from eating the
    /// budget even while the sum happens to fit.
    #[test]
    fn descriptions_fit_the_tool_table_token_budget() {
        let mut total = 0;
        for (name, desc, budget) in [
            ("search_code", SEARCH_CODE_DESC, 110),
            ("find_symbol", FIND_SYMBOL_DESC, 40),
            ("resolve_context", RESOLVE_CONTEXT_DESC, 40),
            ("get_languages", GET_LANGUAGES_DESC, 40),
            ("find_references", FIND_REFERENCES_DESC, 80),
            ("goto_definition", GOTO_DEFINITION_DESC, 40),
            ("get_diagnostics", GET_DIAGNOSTICS_DESC, 40),
        ] {
            let tokens = crate::util::estimate_tokens(desc);
            eprintln!("{name}: {tokens} tokens");
            assert!(tokens <= budget, "{name}: {tokens} tokens > {budget}");
            total += tokens;
        }
        eprintln!("seven descriptions: {total} tokens");
        assert!(total <= 400, "seven descriptions use {total} tokens > 400");
    }

    #[test]
    fn search_code_params_default_to_no_hashline_note() {
        assert!(!SearchCodeParams::default().hashline_note);
        let parsed: SearchCodeParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!parsed.hashline_note);
        assert!(
            serde_json::from_value::<SearchCodeParams>(serde_json::json!({"nope": 1})).is_err()
        );
    }

    #[test]
    fn tool_ids_are_stable() {
        // Registry ids double as the telemetry `Namespace:id` key.
        assert_eq!(
            crate::registry::types::ToolConfig::from(&SearchCodeTool).id,
            "GrokBuild:search_code"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&SearchCodeTool).as_str(),
            "search_code"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&FindSymbolTool).as_str(),
            "find_symbol"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&ResolveContextTool).as_str(),
            "resolve_context"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&GetLanguagesTool).as_str(),
            "get_languages"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&FindReferencesTool).as_str(),
            "find_references"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&GotoDefinitionTool).as_str(),
            "goto_definition"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&GetDiagnosticsTool).as_str(),
            "get_diagnostics"
        );
    }

    /// Minimal backend: one reference at the queried position.
    #[derive(Debug)]
    struct FakeLsp;

    /// Backend that reports a running rust-analyzer registration.
    #[derive(Debug)]
    struct FakeLspWithServers;

    #[async_trait::async_trait]
    impl LspBackend for FakeLspWithServers {
        fn ensure_started_background(&self) {}

        async fn ensure_ready(&self) -> Result<(), String> {
            Ok(())
        }

        fn is_ready(&self) -> bool {
            true
        }

        async fn dispatch(&self, _input: &LspToolInput) -> LspToolResult {
            LspToolResult {
                text: String::new(),
                is_error: false,
            }
        }

        async fn drain_diagnostics(&self, _timeout: Duration) -> Option<DiagnosticsSummary> {
            None
        }

        async fn notify_file_changed(&self, _path: &Path, _content: &str) {}

        async fn notify_file_event(
            &self,
            _path: &Path,
            _content: Option<&str>,
            _kind: DiskChangeKind,
        ) {
        }

        async fn read_diagnostics(&self, _paths: &[PathBuf]) -> Vec<FileDiagnosticEntry> {
            Vec::new()
        }

        async fn list_server_statuses(&self) -> Vec<HostLspServerStatus> {
            vec![HostLspServerStatus {
                name: "rust-analyzer".into(),
                command: "rust-analyzer".into(),
                languages: vec!["rust".into()],
                running: true,
            }]
        }
    }

    #[async_trait::async_trait]
    impl LspBackend for FakeLsp {
        fn ensure_started_background(&self) {}

        async fn ensure_ready(&self) -> Result<(), String> {
            Ok(())
        }

        fn is_ready(&self) -> bool {
            true
        }

        async fn dispatch(&self, _input: &LspToolInput) -> LspToolResult {
            LspToolResult {
                text: String::new(),
                is_error: false,
            }
        }

        async fn precise_references(
            &self,
            path: &Path,
            _line: u32,
            _character: u32,
        ) -> Result<Vec<PreciseLocation>, String> {
            Ok(vec![PreciseLocation {
                path: path.to_path_buf(),
                line: 0,
                character: 3,
                end_line: 0,
                end_character: 13,
            }])
        }

        async fn precise_definition(
            &self,
            path: &Path,
            _line: u32,
            _character: u32,
        ) -> Result<Vec<PreciseLocation>, String> {
            Ok(vec![PreciseLocation {
                path: path.to_path_buf(),
                line: 2,
                character: 3,
                end_line: 2,
                end_character: 13,
            }])
        }

        async fn precise_diagnostics(
            &self,
            _path: &Path,
        ) -> Result<Vec<PreciseDiagnostic>, String> {
            Ok(Vec::new())
        }

        async fn drain_diagnostics(&self, _timeout: Duration) -> Option<DiagnosticsSummary> {
            None
        }

        async fn notify_file_changed(&self, _path: &Path, _content: &str) {}

        async fn notify_file_event(
            &self,
            _path: &Path,
            _content: Option<&str>,
            _kind: DiskChangeKind,
        ) {
        }

        async fn read_diagnostics(&self, _paths: &[PathBuf]) -> Vec<FileDiagnosticEntry> {
            Vec::new()
        }
    }

    /// `rust-analyzer` from the default rustup toolchain. This repo pins
    /// 1.94.0 (no rust-analyzer component) and the LSP child inherits this
    /// process's working directory, so a bare `rust-analyzer` would resolve
    /// through the proxy to that toolchain and exit.
    fn stable_rust_analyzer() -> Option<std::path::PathBuf> {
        let out = std::process::Command::new("rustup")
            .args(["which", "rust-analyzer", "--toolchain", "stable"])
            .current_dir(std::env::temp_dir())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!path.is_empty()).then(|| std::path::PathBuf::from(path))
    }

    fn rust_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn precise_marker() -> u8 {\n    1\n}\n",
        )
        .unwrap();
        tmp
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn find_references_falls_back_to_the_host_language_server() {
        let tmp = rust_workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let backend: Arc<dyn LspBackend> = Arc::new(FakeLsp);
        resources.insert(backend);
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(
            &FindReferencesTool,
            ctx,
            FindReferencesRequest {
                symbol: "precise_marker".to_string(),
                path: Some("src/lib.rs".to_string()),
                ..FindReferencesRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("find_references must return text");
        };
        assert!(
            text.text.contains("confidence: exact (lsp)"),
            "{}",
            text.text
        );
        assert!(text.text.contains("[declaration]"), "{}", text.text);
        assert!(text.text.contains("src/lib.rs:1"), "{}", text.text);
    }

    /// No host backend at all: a Rust definition is an explicit dead end with
    /// the config pointers, never an empty list that reads as "no definition".
    #[tokio::test(flavor = "multi_thread")]
    async fn goto_definition_without_a_language_server_explains_the_dead_end() {
        let tmp = rust_workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(
            &GotoDefinitionTool,
            ctx,
            GotoDefinitionRequest {
                path: Some("src/lib.rs".to_string()),
                line: Some(1),
                symbol: Some("precise_marker".to_string()),
                ..GotoDefinitionRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("goto_definition must return text");
        };
        assert!(text.text.contains("lsp_fallback = grok"), "{}", text.text);
        assert!(text.text.contains("~/.grok/lsp.json"), "{}", text.text);
        assert!(
            text.text.contains("lsp_fallback = \"astrolabe\""),
            "{}",
            text.text
        );
    }

    /// End-to-end with the real rust-analyzer (Phase C6): Rust defers from the
    /// index to the host server, references and definition carry the same
    /// 1-based lines as `search_code`, and hover answers through the same
    /// backend the `lsp` tool calls.
    ///
    /// Ignored by default — needs a rust-analyzer binary and 20–60 s:
    /// `cargo test -p xai-grok-tools --lib real_rust_analyzer -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a real rust-analyzer (stable rustup toolchain)"]
    async fn real_rust_analyzer_answers_references_definition_and_hover() {
        use std::collections::BTreeMap;

        use crate::implementations::lsp::config::LspServerConfig;
        use crate::implementations::lsp::{LspBackendAdapter, LspManager, LspOperation};

        let Some(binary) = stable_rust_analyzer() else {
            eprintln!("no stable-toolchain rust-analyzer; skipping");
            return;
        };
        // The same file a user puts at ~/.grok/lsp.json for Rust.
        let fixture = include_str!("testdata/lsp.rust-analyzer.json");
        let mut servers: BTreeMap<String, LspServerConfig> =
            serde_json::from_str(fixture).expect("lsp.json fixture parses");
        // This repo pins 1.94.0, which has no rust-analyzer component; a bare
        // `rust-analyzer` would hit the rustup proxy and exit. Use the binary
        // the default toolchain provides.
        for config in servers.values_mut() {
            config.command = binary.to_string_lossy().into_owned();
        }

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"lsp_e2e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn marker() -> u8 {\n    1\n}\n\npub fn caller() -> u8 {\n    marker()\n}\n",
        )
        .unwrap();

        let manager = LspManager::new(
            servers,
            tmp.path().to_path_buf(),
            true,
            crate::notification::ToolNotificationHandle::noop(),
        );
        let adapter = LspBackendAdapter::new(Arc::new(tokio::sync::Mutex::new(manager)));
        let backend: Arc<dyn LspBackend> = Arc::new(adapter);

        // rust-analyzer answers once its own indexing settles; poll the tool
        // rather than racing a fixed sleep.
        let mut references = String::new();
        for _ in 0..24 {
            let mut resources = Resources::new();
            resources.insert(Cwd(tmp.path().to_path_buf()));
            resources.insert(backend.clone());
            let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
            let output = xai_tool_runtime::Tool::run(
                &FindReferencesTool,
                ctx,
                FindReferencesRequest {
                    symbol: "marker".to_string(),
                    path: Some("src/lib.rs".to_string()),
                    ..FindReferencesRequest::default()
                },
            )
            .await
            .unwrap();
            let ToolOutput::Text(text) = output else {
                panic!("find_references must return text");
            };
            references = text.text;
            if references.contains("confidence: exact (lsp)")
                && references.contains("src/lib.rs:1 [declaration]")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        }
        assert!(
            references.contains("confidence: exact (lsp)"),
            "references: {references}"
        );
        assert!(
            references.contains("src/lib.rs:1 [declaration]"),
            "references: {references}"
        );
        assert!(
            references.contains("src/lib.rs:6"),
            "references: {references}"
        );

        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        resources.insert(backend.clone());
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(
            &GotoDefinitionTool,
            ctx,
            GotoDefinitionRequest {
                path: Some("src/lib.rs".to_string()),
                line: Some(6),
                symbol: Some("marker".to_string()),
                ..GotoDefinitionRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(definition) = output else {
            panic!("goto_definition must return text");
        };
        assert!(
            definition.text.contains("confidence: exact (lsp)"),
            "definition: {}",
            definition.text
        );
        assert!(
            definition.text.contains("src/lib.rs:1"),
            "definition: {}",
            definition.text
        );

        let hover = backend
            .dispatch(&LspToolInput {
                operation: LspOperation::Hover,
                file_path: Some(tmp.path().join("src/lib.rs").to_string_lossy().into_owned()),
                line: Some(5),
                character: Some(4),
                query: None,
            })
            .await;
        assert!(!hover.is_error, "hover: {}", hover.text);
        assert!(hover.text.contains("marker"), "hover: {}", hover.text);

        xai_grok_astrolabe::shutdown_all();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_diagnostics_falls_back_to_the_parser() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "fn broken( {\n").unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(
            &GetDiagnosticsTool,
            ctx,
            DiagnosticsRequest {
                path: "src/lib.rs".to_string(),
                severity: None,
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("get_diagnostics must return text");
        };
        assert!(text.text.contains("lib.rs"), "{}", text.text);
        assert!(!text.text.contains("index_root"), "{}", text.text);
        assert!(
            text.text.contains(BUILD_TRUTH_NOTE),
            "diagnostics must say they are not a build: {}",
            text.text
        );
    }

    fn workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "fn native_marker() -> u8 {\n    7\n}\n",
        )
        .unwrap();
        tmp
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_code_runs_through_the_tool_contract() {
        let tmp = workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(
            &SearchCodeTool,
            ctx,
            SearchCodeRequest {
                query: "native_marker".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("search_code must return text");
        };
        assert!(text.text.contains("lib.rs:"), "{}", text.text);
        assert!(!text.text.contains("index_root"), "{}", text.text);
        assert!(text.text.contains("native_marker"), "{}", text.text);
        assert!(!text.text.contains("hashline_read"), "{}", text.text);
    }

    /// The hashline toolset swaps `read_file`/`grep` for anchor-aware tools;
    /// `search_code` cannot produce anchors, so its output must say where to
    /// get them. Without the param the note stays out of the result.
    /// A backend that proxies reads elsewhere: the in-process index must not
    /// answer from this host's disk.
    struct RemoteFs;

    #[async_trait::async_trait]
    impl crate::computer::types::AsyncFileSystem for RemoteFs {
        fn is_local(&self) -> bool {
            false
        }

        async fn read_file(
            &self,
            _path: &Path,
        ) -> Result<Vec<u8>, crate::computer::types::ComputerError> {
            Err(crate::computer::types::ComputerError::io_with_kind(
                "remote reads are not available in this test",
                std::io::ErrorKind::Unsupported,
            ))
        }

        async fn write_file(
            &self,
            _path: &Path,
            _data: &[u8],
        ) -> Result<(), crate::computer::types::ComputerError> {
            Err(crate::computer::types::ComputerError::io_with_kind(
                "remote writes are not available in this test",
                std::io::ErrorKind::Unsupported,
            ))
        }

        async fn delete_file(
            &self,
            _path: &Path,
        ) -> Result<(), crate::computer::types::ComputerError> {
            Err(crate::computer::types::ComputerError::io_with_kind(
                "remote deletes are not available in this test",
                std::io::ErrorKind::Unsupported,
            ))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_code_refuses_a_remote_file_system() {
        let tmp = workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        resources.insert(FileSystem(std::sync::Arc::new(RemoteFs)));
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let err = xai_tool_runtime::Tool::run(
            &SearchCodeTool,
            ctx,
            SearchCodeRequest {
                query: "native_marker".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .await
        .expect_err("remote sessions must not get an in-process index");
        let message = format!("{err}");
        assert!(message.contains("local workspace"), "{message}");
    }

    /// The session's `RespectGitignore` selects the index policy: with it off,
    /// `search_code` sees the same ignored files `grep` would.
    #[tokio::test(flavor = "multi_thread")]
    async fn search_code_follows_the_session_gitignore_policy() {
        let tmp = workspace();
        std::fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(
            tmp.path().join("ignored.rs"),
            "fn ignored_policy_marker() {}\n",
        )
        .unwrap();

        let run = |respect: bool| {
            let root = tmp.path().to_path_buf();
            async move {
                let mut resources = Resources::new();
                resources.insert(Cwd(root));
                resources.insert(RespectGitignore(respect));
                let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
                let output = xai_tool_runtime::Tool::run(
                    &SearchCodeTool,
                    ctx,
                    SearchCodeRequest {
                        query: "ignored_policy_marker".to_string(),
                        ..SearchCodeRequest::default()
                    },
                )
                .await
                .unwrap();
                let ToolOutput::Text(text) = output else {
                    panic!("search_code must return text");
                };
                text.text
            }
        };

        let hidden = run(true).await;
        assert!(!hidden.contains("ignored.rs"), "{hidden}");
        let visible = run(false).await;
        assert!(visible.contains("ignored.rs"), "{visible}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_code_appends_the_hashline_note_only_when_configured() {
        use crate::types::resources::Params;

        let tmp = workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        resources.insert(Params(SearchCodeParams {
            hashline_note: true,
        }));
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(
            &SearchCodeTool,
            ctx,
            SearchCodeRequest {
                query: "native_marker".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("search_code must return text");
        };
        assert!(text.text.contains(HASHLINE_NOTE), "{}", text.text);
    }

    #[test]
    fn cancel_guard_flips_the_engine_flag() {
        let handle = CancelHandle::new();
        assert!(!handle.is_cancelled());
        {
            let _guard = CancelOnDrop(handle.clone());
            assert!(!handle.is_cancelled());
        }
        assert!(
            handle.is_cancelled(),
            "dropping the tool future must stop the scan"
        );
    }

    /// A turn that is already cancelled must not start a scan.
    #[tokio::test(flavor = "multi_thread")]
    async fn search_code_short_circuits_a_pre_cancelled_call() {
        let tmp = workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let mut ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        ctx.extensions.insert(xai_tool_runtime::Cancellation(token));
        let output = xai_tool_runtime::Tool::run(
            &SearchCodeTool,
            ctx,
            SearchCodeRequest {
                query: "native_marker".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("search_code must return text");
        };
        assert!(text.text.contains("cancelled"), "{}", text.text);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_languages_runs_through_the_tool_contract() {
        let tmp = workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(&GetLanguagesTool, ctx, LanguagesRequest {})
            .await
            .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("get_languages must return text");
        };
        assert!(text.text.contains("rust"), "{}", text.text);
    }

    /// Host LSP registrations surface in `get_languages` when the session
    /// has a backend (`lsp_fallback=grok` is the process default).
    #[tokio::test(flavor = "multi_thread")]
    async fn get_languages_reports_the_host_lsp_pool() {
        let tmp = workspace();
        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let backend: Arc<dyn LspBackend> = Arc::new(FakeLspWithServers);
        resources.insert(backend);
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let output = xai_tool_runtime::Tool::run(&GetLanguagesTool, ctx, LanguagesRequest {})
            .await
            .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("get_languages must return text");
        };
        assert!(text.text.contains("Host LSP servers"), "{}", text.text);
        assert!(
            text.text.contains("Ready (host: rust-analyzer)"),
            "{}",
            text.text
        );
        assert!(
            text.text.contains("index-first precise answers"),
            "get_languages must name the index-first languages: {}",
            text.text
        );
        assert!(text.text.contains("goto_definition = go"), "{}", text.text);
    }

    /// Both dead ends of `lsp_fallback = grok` — nothing registered, and a
    /// registered pool that cannot answer — carry the same config pointers.
    #[test]
    fn no_host_lsp_note_names_the_config_and_the_astrolabe_fallback() {
        for note in [
            no_host_lsp_note(None),
            no_host_lsp_note(Some("No LSP server configured")),
        ] {
            assert!(note.contains("lsp_fallback = grok"), "{note}");
            assert!(note.contains("~/.grok/lsp.json"), "{note}");
            assert!(note.contains("<cwd>/.grok/lsp.json"), "{note}");
            assert!(note.contains("lsp_fallback = \"astrolabe\""), "{note}");
            assert!(note.contains("get_languages"), "{note}");
        }
        assert!(no_host_lsp_note(Some("No LSP server configured")).contains("did not answer"));
        assert!(no_host_lsp_note(None).contains("no workspace language server is registered"));
    }

    /// The `lsp` tool's unavailable error and the astrolabe note share one
    /// sentence, so the model gets the same fix from either tool.
    #[test]
    fn the_lsp_setup_sentence_is_shared() {
        let setup = crate::implementations::lsp::LSP_SERVER_SETUP;
        assert!(setup.contains("~/.grok/lsp.json"), "{setup}");
        assert!(setup.contains("<cwd>/.grok/lsp.json"), "{setup}");
        assert!(
            no_host_lsp_note(None).contains(setup),
            "{}",
            no_host_lsp_note(None)
        );
    }

    /// `find_symbol` and `resolve_context` hand the turn back when it is
    /// already cancelled instead of starting an index lookup.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookups_short_circuit_a_pre_cancelled_turn() {
        let tmp = workspace();

        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let mut ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        ctx.extensions.insert(xai_tool_runtime::Cancellation(token));
        let output = xai_tool_runtime::Tool::run(
            &FindSymbolTool,
            ctx,
            FindSymbolRequest {
                query: "native_marker".to_string(),
                ..FindSymbolRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("find_symbol must return text");
        };
        assert!(text.text.contains("cancelled"), "{}", text.text);

        let mut resources = Resources::new();
        resources.insert(Cwd(tmp.path().to_path_buf()));
        let mut ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        ctx.extensions.insert(xai_tool_runtime::Cancellation(token));
        let output = xai_tool_runtime::Tool::run(
            &ResolveContextTool,
            ctx,
            ResolveContextRequest {
                task: "where is native_marker defined".to_string(),
                ..ResolveContextRequest::default()
            },
        )
        .await
        .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("resolve_context must return text");
        };
        assert!(text.text.contains("cancelled"), "{}", text.text);
    }

    #[test]
    fn remote_file_systems_hide_only_the_index_tools() {
        // Pure filter; the finalize-level path is
        // `registry::types::remote_fs_finalize_hides_only_the_seven_index_tools`.
        use xai_grok_astrolabe::AstrolabeMode;
        for id in TOOL_IDS {
            assert!(
                !listed_for(AstrolabeMode::Auto, id, false),
                "{id} must not be listed for a remote workspace"
            );
            assert!(
                listed_for(AstrolabeMode::Auto, id, true),
                "{id} must be listed for a local workspace"
            );
        }
        // Everything else keeps its slot.
        assert!(listed_for(AstrolabeMode::Auto, "GrokBuild:grep", false));
        assert!(listed_for(
            AstrolabeMode::Auto,
            "GrokBuild:read_file",
            false
        ));
    }

    fn off_hides_only_the_index_tools() {
        use xai_grok_astrolabe::AstrolabeMode;
        assert!(listing_enabled_for(
            AstrolabeMode::Auto,
            "GrokBuild:search_code"
        ));
        assert!(listing_enabled_for(
            AstrolabeMode::Index,
            "GrokBuild:get_languages"
        ));
        assert!(!listing_enabled_for(
            AstrolabeMode::Off,
            "GrokBuild:search_code"
        ));
        assert!(!listing_enabled_for(
            AstrolabeMode::Off,
            "GrokBuild:find_symbol"
        ));
        // `index` keeps the four index tools and drops the precise ones.
        assert!(listing_enabled_for(
            AstrolabeMode::Index,
            "GrokBuild:search_code"
        ));
        assert!(!listing_enabled_for(
            AstrolabeMode::Index,
            "GrokBuild:find_references"
        ));
        assert!(listing_enabled_for(
            AstrolabeMode::Auto,
            "GrokBuild:find_references"
        ));
        // Phase D: `all` is a synonym of `auto` — both register all seven.
        assert!(listing_enabled_for(
            AstrolabeMode::All,
            "GrokBuild:get_diagnostics"
        ));
        assert!(listing_enabled_for(
            AstrolabeMode::Auto,
            "GrokBuild:get_diagnostics"
        ));
        assert!(!listing_enabled_for(
            AstrolabeMode::Off,
            "GrokBuild:goto_definition"
        ));
        // Tools outside this pack are never filtered by this gate.
        assert!(listing_enabled_for(AstrolabeMode::Off, "GrokBuild:grep"));
    }
}
