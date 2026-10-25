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
    PathMap, ResolveContextRequest, SearchCodeRequest, engine_for, root_for_cwd,
};

use crate::implementations::lsp::{
    DiagnosticSeverityLevel, LspBackend, PreciseDiagnostic, PreciseLocation,
};
use crate::types::output::{TextOutput, ToolOutput};
use crate::types::resources::{DenyReadGlobs, DisplayCwd, SharedResources};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, resolve_cwd, shared_resources};

pub const SEARCH_CODE_DESC: &str = r#"Exhaustive literal/regex search over the indexed workspace. Each hit comes with its enclosing function or type body, so you often do not need read_file. Wide results are summarized per file, or counted with aggregate; page with offset. Searches respect .gitignore and the workspace read policy. Use grep for paths outside the workspace or for multiline patterns."#;

pub const FIND_SYMBOL_DESC: &str = r#"Locate a symbol by name, prefix, `Class/method` path, or file outline; returns its signature and path:line."#;

pub const RESOLVE_CONTEXT_DESC: &str =
    r#"Task-ranked map of related files and symbols; use it at the start of an unfamiliar task."#;

pub const GET_LANGUAGES_DESC: &str =
    r#"Indexed languages, file and LOC counts, index gaps, and language-server readiness."#;

pub const FIND_REFERENCES_DESC: &str = r#"Precise references for a symbol: callers, usages, and rename impact. Index first; the workspace language server answers when the index cannot decide. `dry_run_rename=NewName` returns a read-only per-file checklist. Lines are 1-based."#;

pub const GOTO_DEFINITION_DESC: &str = r#"Resolve a symbol's definition from a file and line, or by name; index first, language server fallback. Lines are 1-based."#;

pub const GET_DIAGNOSTICS_DESC: &str = r#"Syntax and type errors for one file, from language-server diagnostics or the parser. Prefer build/test for project-wide truth."#;

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

/// `[toolset.astrolabe].replace_grep = false`: `grep` owns the Search slot, so
/// `search_code` moves behind the last `grep` entry. `by_kind` is first-come,
/// and the system prompt follows it.
pub fn demote_search_code(tools: &mut Vec<crate::registry::types::ToolConfig>) {
    let Some(from) = tools
        .iter()
        .position(|tool| tool.id == "GrokBuild:search_code")
    else {
        return;
    };
    let Some(grep) = tools.iter().rposition(|tool| tool.id == "GrokBuild:grep") else {
        return;
    };
    if grep < from {
        return;
    }
    let search = tools.remove(from);
    let at = tools
        .iter()
        .rposition(|tool| tool.id == "GrokBuild:grep")
        .map(|index| index + 1)
        .unwrap_or(0);
    tools.insert(at, search);
}

/// Session-scoped inputs shared by every Astrolabe tool.
struct CallEnv {
    paths: PathMap,
    deny: DenyPolicy,
    hashline_note: bool,
}

async fn call_env(
    ctx: &xai_tool_runtime::ToolCallContext,
    resources: &SharedResources,
) -> Result<CallEnv, xai_tool_runtime::ToolError> {
    let cwd = resolve_cwd(ctx, resources).await?;
    let (display, deny, hashline_note) = {
        let res = resources.lock().await;
        (
            res.get::<DisplayCwd>().map(|d| d.0.clone()),
            res.get::<DenyReadGlobs>()
                .map(|d| d.0.clone())
                .unwrap_or_default(),
            res.get::<crate::types::resources::Params<SearchCodeParams>>()
                .is_some_and(|p| p.0.hashline_note),
        )
    };
    let cfg = xai_grok_astrolabe::process_config();
    let root = root_for_cwd(&cwd, cfg);
    let paths = PathMap::new(root, cwd, display);
    Ok(CallEnv {
        deny: DenyPolicy::for_root(paths.root(), &deny),
        paths,
        hashline_note,
    })
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

const NO_LSP_NOTE: &str = "note: `lsp_fallback = grok` but no workspace language server is configured. \
Add one to ~/.grok/lsp.json, or set GROK_ASTROLABE_LSP=astrolabe to use Astrolabe's own pool.";

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
        let root = env.paths.root().to_path_buf();
        let engine = blocking(move || engine_for(&root, &cfg))
            .await?
            .map_err(engine_error)?;
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
        let cfg = xai_grok_astrolabe::process_config().clone();
        let root = env.paths.root().to_path_buf();
        let engine = blocking(move || engine_for(&root, &cfg))
            .await?
            .map_err(engine_error)?;
        let paths = env.paths;
        let deny = env.deny;
        let out = blocking(move || engine.find_symbol(&paths, &deny, input)).await?;
        Ok(text_output(out.map_err(engine_error)?.text))
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
        let cfg = xai_grok_astrolabe::process_config().clone();
        let root = env.paths.root().to_path_buf();
        let engine = blocking(move || engine_for(&root, &cfg))
            .await?
            .map_err(engine_error)?;
        let paths = env.paths;
        let deny = env.deny;
        let out = blocking(move || engine.resolve_context(&paths, &deny, input)).await?;
        Ok(text_output(out.map_err(engine_error)?.text))
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
        let cfg = xai_grok_astrolabe::process_config().clone();
        let root = env.paths.root().to_path_buf();
        let engine = blocking(move || engine_for(&root, &cfg))
            .await?
            .map_err(engine_error)?;
        let paths = env.paths;
        let deny = env.deny;
        let out = blocking(move || engine.get_languages(&paths, &deny, input)).await?;
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
        let root = env.paths.root().to_path_buf();
        let engine_cfg = cfg.clone();
        let engine = blocking(move || engine_for(&root, &engine_cfg))
            .await?
            .map_err(engine_error)?;
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
                    let note = format!("note: language server: {err}");
                    return Ok(text_output(with_note(output.text, &note, cfg.budget_bytes)));
                }
            }
        }

        let mut text = output.text;
        if unknown_reason.is_some() && lsp.is_none() && host_lsp_allowed() {
            text = with_note(text, NO_LSP_NOTE, cfg.budget_bytes);
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
        let root = env.paths.root().to_path_buf();
        let engine_cfg = cfg.clone();
        let engine = blocking(move || engine_for(&root, &engine_cfg))
            .await?
            .map_err(engine_error)?;
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
                    let note = format!("note: language server: {err}");
                    return Ok(text_output(with_note(output.text, &note, cfg.budget_bytes)));
                }
            }
        }

        let mut text = output.text;
        if unknown_reason.is_some() && lsp.is_none() && host_lsp_allowed() {
            text = with_note(text, NO_LSP_NOTE, cfg.budget_bytes);
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
                    return Ok(text_output(rendered.text));
                }
                Ok(_) => {}
                Err(err) => host_error = Some(err),
            }
        }

        let root = env.paths.root().to_path_buf();
        let engine_cfg = cfg.clone();
        let engine = blocking(move || engine_for(&root, &engine_cfg))
            .await?
            .map_err(engine_error)?;
        let paths = env.paths;
        let deny = env.deny;
        let out = blocking(move || engine.get_diagnostics(&paths, &deny, input))
            .await?
            .map_err(engine_error)?;
        let mut text = out.text;
        if let Some(err) = host_error {
            let note = format!(
                "note: host language server unavailable ({err}); parser/toolchain result below."
            );
            text = with_note(text, &note, cfg.budget_bytes);
        }
        Ok(text_output(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use crate::implementations::lsp::{
        DiagnosticsSummary, DiskChangeKind, FileDiagnosticEntry, LspToolInput, LspToolResult,
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

    #[test]
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
        assert!(listing_enabled_for(
            AstrolabeMode::All,
            "GrokBuild:get_diagnostics"
        ));
        assert!(!listing_enabled_for(
            AstrolabeMode::Off,
            "GrokBuild:goto_definition"
        ));
        // Tools outside this pack are never filtered by this gate.
        assert!(listing_enabled_for(AstrolabeMode::Off, "GrokBuild:grep"));
    }

    #[test]
    fn replace_grep_false_moves_search_code_behind_grep() {
        fn configs(ids: &[&str]) -> Vec<crate::registry::types::ToolConfig> {
            ids.iter()
                .map(|id| crate::registry::types::ToolConfig {
                    id: (*id).to_string(),
                    params: None,
                    name_override: None,
                    params_name_overrides: None,
                    description_override: None,
                    behavior_version: None,
                    kind: None,
                })
                .collect()
        }
        let mut tools = configs(&[
            "GrokBuild:search_code",
            "GrokBuild:find_symbol",
            "GrokBuild:grep",
            "GrokBuild:read_file",
        ]);
        demote_search_code(&mut tools);
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "GrokBuild:find_symbol",
                "GrokBuild:grep",
                "GrokBuild:search_code",
                "GrokBuild:read_file"
            ]
        );

        // Already behind grep: no change.
        let mut tools = configs(&["GrokBuild:grep", "GrokBuild:search_code"]);
        demote_search_code(&mut tools);
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["GrokBuild:grep", "GrokBuild:search_code"]);

        // No grep in the list: keep the original position.
        let mut tools = configs(&["GrokBuild:search_code", "GrokBuild:read_file"]);
        demote_search_code(&mut tools);
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["GrokBuild:search_code", "GrokBuild:read_file"]);
    }
}
