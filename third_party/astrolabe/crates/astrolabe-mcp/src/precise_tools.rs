//! Precise (LSP-backed) query tools: references, definitions, diagnostics.
//!
//! Rendering lives here; `server.rs` mounts `#[tool]` methods that call
//! [`run_find_references`], [`run_goto_definition`], and
//! [`run_get_diagnostics`] through `LiveBackend`.
//! [`crate::TOOL_COUNT`] includes [`crate::PRECISE_TOOL_COUNT`] (3).
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;

use astrolabe_core::{
    budget::{estimate_tokens, truncate_ranked},
    lsp::{queries::AMBIGUOUS_DEFINITIONS_MARK, Diagnostic, Location, Position, Precise, Severity},
    render::{anchor, confidence_note},
    Confidence, Language, RelPath,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::refs_graph::{HitInfo, HitKind};
use crate::refs_render;

const DEFAULT_BUDGET: usize = 2_500;

const SEARCH_CODE_FALLBACK: &str =
    "You can use search_code for a literal search, but that is syntactic: same-name identifiers, strings, and comments all count as hits — both false positives and misses via aliases/re-exports. It cannot replace precise references.";

const DEFINITION_FALLBACK: &str =
    "You can use find_symbol (syntactic/graph name match) or search_code (literal search), but neither replaces language-server binding resolution.";

const NOT_WIRED: &str =
    "This capability is not wired yet: the language-server query layer and rewrite engine are still being connected; no precise result is available.";

fn default_budget() -> usize {
    DEFAULT_BUDGET
}

/// Same ceiling `ASTROLABE_MAX_BUDGET_TOKENS` puts on the other tools
/// (default 32000); `budget_tokens` above it is lowered, never honoured.
const MAX_BUDGET_TOKENS: usize = 32_000;

fn effective_budget(raw: usize) -> usize {
    let max = std::env::var("ASTROLABE_MAX_BUDGET_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(MAX_BUDGET_TOKENS);
    raw.min(max)
}

/// Stated once per result that lists positions.
pub(crate) const COORD_NOTE: &str =
    "Coordinates: line and column are 1-based; columns are UTF-16 units. For line/character parameters use 0-based values (subtract 1 each).\n";

/// One page of `items` starting at `offset`, cut by `budget`. Always keeps at
/// least one item when any remain, so `next_offset` advances. Returns the kept
/// items and how many remain after them.
fn page(items: &[String], offset: usize, budget: usize) -> (Vec<&String>, usize) {
    let rest = items.get(offset..).unwrap_or(&[]);
    let (mut kept, mut omitted) = truncate_ranked(rest, budget, |line| format!("{line}\n"));
    if kept.is_empty() && !rest.is_empty() {
        kept.push(&rest[0]);
        omitted -= 1;
    }
    (kept, omitted)
}

fn omitted_line(omitted: usize, next_offset: usize) -> String {
    format!("{omitted} omitted due to budget_tokens; offset={next_offset} for more.\n")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct FindReferencesParams {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[schemars(description = "Symbol name.")]
    pub symbol: String,
    // Optional on purpose: models often omit it. Prefer an explicit path for
    // ambiguous names. When omitted, the server first reuses this session's
    // last find_symbol / goto_definition hit for the same symbol, else the
    // unique index definition, else lists ranked candidates (no silent guess
    // among many).
    #[serde(default)]
    #[schemars(
        description = "Repo-relative file of the definition or a use site. Strongly preferred for ambiguous names (Init, Run, …). If omitted: this session's last find_symbol/goto_definition hit for the same symbol, else unique index definition, else ranked candidates."
    )]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(description = "0-based line (disambiguates).")]
    pub line: Option<u32>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based UTF-16 column; used with line.")]
    pub character: Option<u32>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(description = "Skip first N results (paging).")]
    pub offset: Option<usize>,
    #[serde(default)]
    #[schemars(description = "New name: preview per-file edits (no write).")]
    pub dry_run_rename: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GotoDefinitionParams {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[schemars(description = "File containing the use site.")]
    pub path: String,
    #[serde(default)]
    #[schemars(description = "Symbol name.")]
    pub symbol: Option<String>,
    #[serde(default)]
    #[schemars(description = "0-based line.")]
    pub line: Option<u32>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based UTF-16 column.")]
    pub character: Option<u32>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(description = "Skip first N results (paging).")]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetDiagnosticsParams {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[schemars(description = "Repo-relative file.")]
    pub path: String,
    #[serde(default)]
    #[schemars(description = "Minimum severity: error, warning, information, or hint.")]
    pub severity: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    /// Not advertised (`server.rs` UNADVERTISED_PARAMS); results name it.
    #[serde(default)]
    #[schemars(description = "true: ask the language server (slow, memory-heavy).")]
    pub lsp: Option<bool>,
    /// `false`: parser diagnostics only, never run the toolchain. A host with
    /// its own build/test tools sets this; MCP callers get the default.
    #[serde(default = "default_toolchain")]
    #[schemars(skip)]
    pub toolchain: bool,
}

fn default_toolchain() -> bool {
    true
}

/// Tool descriptions shared by `PreciseTools` and `AstrolabeServer`, so the
/// two catalogs cannot drift apart.
pub(crate) const FIND_REFERENCES_DESC: &str = "Precise references (index first, language-server fallback): call sites and usages — use for rename and caller analysis. Pass path (definition or use site) to disambiguate; when omitted, reuse this session's latest find_symbol/goto_definition hit, else the unique index definition or a candidate list. Hits show source line, enclosing func, [declaration]; interface → implementors. dry_run_rename=NewName: read-only per-file rename checklist.";
pub(crate) const GOTO_DEFINITION_DESC: &str =
    "Jump to definition: return the definition anchor from the use-site file + symbol name.";
pub(crate) const GET_DIAGNOSTICS_DESC: &str =
    "File diagnostics (syntax and type errors) for checking after edits; skip if build/tests already pass.";

/// Backend for the precise MCP query tools.
///
/// Production uses `LiveBackend` (on-demand language-server pool). Tests
/// inject fakes for the missing-server path.
pub(crate) trait PreciseCapability: std::fmt::Debug + Send + Sync {
    /// References of `symbol`. `exact`: a compiler-grade answer is required
    /// (rename preview), so the index-first shortcut is skipped.
    fn references(
        &self,
        symbol: &str,
        path: Option<&str>,
        position: Option<Position>,
        exact: bool,
    ) -> Precise<Vec<Location>>;
    /// Per-hit source line, enclosing function, declaration / comment /
    /// string / test flags. Empty when the backend cannot tell; the list
    /// then renders as bare coordinates.
    fn annotate(&self, _symbol: &str, _locations: &[Location]) -> Vec<HitInfo> {
        Vec::new()
    }
    /// Source lines of a repo-relative file (cached), for inline caller
    /// context. `None`: no context is shown.
    fn source_lines(&self, _path: &str) -> Option<Arc<Vec<String>>> {
        None
    }
    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>>;
    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>>;
    /// Diagnostics with an explicit choice of source. `lsp`: the caller asked
    /// for the language server; otherwise the backend may answer from the
    /// toolchain / parser without starting one.
    fn diagnostics_with(&self, path: &str, _lsp: bool) -> Precise<Vec<Diagnostic>> {
        self.diagnostics(path)
    }
    /// Diagnostics with a toolchain policy. `toolchain = false` forbids
    /// running `cargo check` / `go test -c` / `tsc`: a host that already owns
    /// build and test tools keeps that cost out of a syntax check.
    fn diagnostics_with_options(
        &self,
        path: &str,
        lsp: bool,
        _toolchain: bool,
    ) -> Precise<Vec<Diagnostic>> {
        self.diagnostics_with(path, lsp)
    }
    /// Files changed on disk; keeps running language servers in sync.
    fn files_changed(&self, _changes: &astrolabe_core::watch::ChangeSet) {}
}

/// Backend with no language server behind it.
///
/// Production uses `LiveBackend`; this remains as the test double for the
/// "capability absent" path, which still has to answer rather than vanish —
/// an agent needs to learn what to install.
#[cfg(test)]
#[derive(Debug)]
struct Unwired;

#[cfg(test)]
impl PreciseCapability for Unwired {
    fn references(
        &self,
        symbol: &str,
        path: Option<&str>,
        _position: Option<Position>,
        _exact: bool,
    ) -> Precise<Vec<Location>> {
        Precise::unknown(
            Vec::new(),
            format!(
                "{NOT_WIRED} target symbol `{symbol}`{}.",
                path.map(|p| format!(" (scoped to {p})"))
                    .unwrap_or_default()
            ),
        )
    }

    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let target = match (symbol, line, character) {
            (_, Some(l), Some(c)) => format!("@{path}:{l}:{c}"),
            (Some(s), _, _) => format!("`{s}` (scoped to {path})"),
            _ => format!(" (scoped to {path})"),
        };
        Precise::unknown(Vec::new(), format!("{NOT_WIRED} target {target}."))
    }

    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>> {
        Precise::unknown(Vec::new(), format!("{NOT_WIRED} target file `{path}`."))
    }
}

#[derive(Clone)]
pub(crate) struct PreciseTools {
    backend: Arc<dyn PreciseCapability>,
    tool_router: ToolRouter<Self>,
}

impl PreciseTools {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_backend(Arc::new(Unwired))
    }

    pub(crate) fn with_backend(backend: Arc<dyn PreciseCapability>) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl PreciseTools {
    #[tool(
        description = FIND_REFERENCES_DESC,
        annotations(title = "Precise references", read_only_hint = true)
    )]
    pub(crate) fn find_references(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        run_find_references(params, self.backend.as_ref())
    }

    #[tool(
        description = GOTO_DEFINITION_DESC,
        annotations(title = "Precise definition", read_only_hint = true)
    )]
    pub(crate) fn goto_definition(
        &self,
        Parameters(params): Parameters<GotoDefinitionParams>,
    ) -> CallToolResult {
        run_goto_definition(params, self.backend.as_ref())
    }

    #[tool(
        description = GET_DIAGNOSTICS_DESC,
        annotations(title = "File diagnostics", read_only_hint = true)
    )]
    pub(crate) fn get_diagnostics(
        &self,
        Parameters(params): Parameters<GetDiagnosticsParams>,
    ) -> CallToolResult {
        run_get_diagnostics(params, self.backend.as_ref())
    }
}

pub(crate) fn run_find_references(
    mut params: FindReferencesParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    params.budget_tokens = effective_budget(params.budget_tokens);
    if params.symbol.trim().is_empty() {
        return text_result(
            unknown_preamble(
                Some("symbol cannot be empty."),
                params.path.as_deref(),
                true,
            ),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let position = match (params.line, params.character) {
        (Some(line), Some(character)) => Some(Position { line, character }),
        _ => None,
    };
    let result = backend.references(
        &params.symbol,
        params.path.as_deref(),
        position,
        params.dry_run_rename.is_some(),
    );
    // Order is the backend's: queried file first, then path and position;
    // rendering moves test hits after non-test ones (stable).
    let locations = result.value;

    // Keep the preamble out of ranked truncation so a zero budget still
    // yields non-empty text (the Cursor `content[0]` contract).
    let preamble = if result.confidence == Confidence::Unknown {
        unknown_preamble(result.note.as_deref(), params.path.as_deref(), true)
    } else {
        let mut header = confidence_header(result.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        if locations.is_empty() {
            header.push_str(&format!("References not found for `{}`.\n", params.symbol));
        }
        header
    };

    let mut infos = if locations.is_empty() {
        Vec::new()
    } else {
        backend.annotate(&params.symbol, &locations)
    };
    if infos.len() != locations.len() {
        infos = vec![HitInfo::default(); locations.len()];
    }
    let mut hits = RefHits::new(&locations, &infos);
    let wide = params.offset.is_none()
        && params.dry_run_rename.is_none()
        && locations.len() > WIDE_REFERENCES;
    if wide {
        let mut body = preamble;
        body.push_str(&hits.summary_line());
        body.push_str(&hits.wide_summary());
        return text_result(
            body,
            json!({
                "confidence": confidence_json(result.confidence),
                "budget_tokens": params.budget_tokens,
                "matches": locations.len(),
                "offset": 0,
                "returned": 0,
                "omitted": locations.len(),
                "truncated": true,
                "next_offset": 0,
                "wide": true,
                "dry_run_rename": false,
                "rename_files": 0,
            }),
        );
    }

    let offset = params.offset.unwrap_or(0);
    let mut summary = if locations.is_empty() {
        String::new()
    } else {
        hits.summary_line()
    };
    if offset == 0 {
        if let Some(line) = refs_render::caller_summary(&locations, &infos) {
            summary.push_str(&line);
        }
    }
    // Complete by construction (language server, or the index answer), so
    // the model can stop instead of grepping to re-check.
    let complete = refs_render::complete_source(result.confidence, result.note.as_deref())
        .filter(|_| !locations.is_empty());
    let files = locations
        .iter()
        .map(|l| l.path.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let completion = complete
        .map(|src| refs_render::completion_line(locations.len(), files, src))
        .unwrap_or_default();
    // Inline caller context: off by default since batch 8 (the model read
    // around those hits anyway); `ASTROLABE_REFS_CONTEXT_HITS=on|N` turns it
    // on, and then only when it fits the budget with the whole list.
    let mut items = hits.items();
    if params.dry_run_rename.is_none() && offset == 0 {
        hits.ctx = refs_render::context_blocks(&locations, &infos, &hits.order, &|p| {
            backend.source_lines(p)
        });
        if !hits.ctx.blocks.is_empty() {
            let with_ctx = hits.items();
            let fixed = estimate_tokens(&preamble)
                + estimate_tokens(&summary)
                + estimate_tokens(REF_COORD_NOTE)
                + estimate_tokens(&completion);
            let need: usize = with_ctx.iter().map(|l| estimate_tokens(l) + 1).sum();
            if fixed + need <= params.budget_tokens {
                items = with_ctx;
            } else {
                hits.ctx = refs_render::Context::default();
            }
        }
    }
    // The header (confidence, note, summary, coordinate line, truncation
    // marker) is part of the output, so it is charged to the budget first.
    let overhead = estimate_tokens(&preamble)
        + estimate_tokens(&summary)
        + estimate_tokens(REF_COORD_NOTE)
        + estimate_tokens(&omitted_line(items.len(), items.len()));
    let (kept, omitted) = page(
        &items,
        offset,
        params.budget_tokens.saturating_sub(overhead),
    );

    let mut body = preamble;
    body.push_str(&summary);
    if !kept.is_empty() {
        body.push_str(REF_COORD_NOTE);
    }
    if offset > 0 && offset >= items.len() && !items.is_empty() {
        body.push_str(&format!(
            "offset={offset} beyond total result count {}.\n",
            items.len()
        ));
    }
    if !kept.is_empty() {
        body.push_str(&hits.continuation_headers(offset));
    }
    for line in &kept {
        body.push_str(line);
        body.push('\n');
    }
    let next_offset = offset + kept.len();
    if omitted > 0 {
        body.push_str(&omitted_line(omitted, next_offset));
    } else if offset == 0 && !kept.is_empty() {
        body.push_str(&completion);
    }
    let mut preview_files = 0usize;
    if let Some(new_name) = params.dry_run_rename.as_deref() {
        // One shared budget: the preview only gets what the list left over.
        let remaining = params.budget_tokens.saturating_sub(estimate_tokens(&body));
        let ambiguous = result
            .note
            .as_deref()
            .is_some_and(|n| n.contains(AMBIGUOUS_DEFINITIONS_MARK));
        let (text, files) = render_rename_preview(
            &params.symbol,
            new_name,
            &locations,
            result.confidence,
            ambiguous,
            remaining,
        );
        body.push_str(&text);
        preview_files = files;
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": locations.len(),
            "offset": offset,
            "returned": kept.len(),
            "omitted": omitted,
            "truncated": omitted > 0,
            "next_offset": (omitted > 0).then_some(next_offset),
            "dry_run_rename": params.dry_run_rename.is_some(),
            "rename_files": preview_files,
            "complete": complete.is_some() && omitted == 0 && offset == 0,
            "context_blocks": hits.ctx.blocks.len(),
        }),
    )
}

/// Above this many hits (and no explicit `offset`), `find_references`
/// answers with per-file counts first, like search_code's wide mode.
const WIDE_REFERENCES: usize = 150;

/// Coordinate legend for the reference list.
pub(crate) const REF_COORD_NOTE: &str =
    "Coords Lline:col 1-based (UTF-16); -1 for line/character params.\n";

/// A reference list in display order: non-test hits first (backend order
/// kept within each group), then test hits.
struct RefHits<'a> {
    locations: &'a [Location],
    infos: &'a [HitInfo],
    order: Vec<usize>,
    /// Inline caller context for short lists (empty otherwise).
    ctx: refs_render::Context,
}

impl<'a> RefHits<'a> {
    fn new(locations: &'a [Location], infos: &'a [HitInfo]) -> Self {
        let mut order: Vec<usize> = (0..locations.len()).collect();
        // Stable: files stay contiguous inside each group.
        order.sort_by_key(|&i| infos[i].test);
        Self {
            locations,
            infos,
            order,
            ctx: refs_render::Context::default(),
        }
    }

    fn group_counts(&self, test: bool) -> (usize, usize) {
        let mut files: Vec<&str> = Vec::new();
        let mut n = 0;
        for &i in &self.order {
            if self.infos[i].test == test {
                n += 1;
                let p = self.locations[i].path.as_str();
                if files.last() != Some(&p) && !files.contains(&p) {
                    files.push(p);
                }
            }
        }
        (n, files.len())
    }

    fn file_count(&self, path: &str, test: bool) -> usize {
        self.order
            .iter()
            .filter(|&&i| self.infos[i].test == test && self.locations[i].path.as_str() == path)
            .count()
    }

    fn summary_line(&self) -> String {
        let (prod, prod_files) = self.group_counts(false);
        let (test, test_files) = self.group_counts(true);
        let files: std::collections::HashSet<&str> =
            self.locations.iter().map(|l| l.path.as_str()).collect();
        let decl = self.infos.iter().filter(|h| h.declaration).count();
        let noncode = self
            .infos
            .iter()
            .filter(|h| h.kind != HitKind::Code)
            .count();
        let hit_word = |n: usize| if n == 1 { "hit" } else { "hits" };
        let file_word = |n: usize| if n == 1 { "file" } else { "files" };
        let mut out = format!(
            "{} {} / {} {} total",
            self.locations.len(),
            hit_word(self.locations.len()),
            files.len(),
            file_word(files.len())
        );
        let mut parts: Vec<String> = Vec::new();
        if prod > 0 && test > 0 {
            parts.push(format!(
                "non-test {prod} {} / {prod_files} {}, test {test} {} / {test_files} {}",
                hit_word(prod),
                file_word(prod_files),
                hit_word(test),
                file_word(test_files)
            ));
        } else if test > 0 {
            parts.push("all in test code".to_string());
        }
        if decl > 0 {
            parts.push(format!("[declaration] {decl} {}", hit_word(decl)));
        }
        if noncode > 0 {
            parts.push(format!(
                "comment/string {noncode} {}, tagged",
                hit_word(noncode)
            ));
        }
        if !parts.is_empty() {
            out.push_str(&format!(" ({})", parts.join("; ")));
        }
        out.push_str(".\n");
        out
    }

    fn mixed(&self) -> bool {
        self.infos.iter().any(|h| h.test) && self.infos.iter().any(|h| !h.test)
    }

    fn section_header(&self, test: bool) -> String {
        if !self.mixed() {
            return String::new();
        }
        let (n, files) = self.group_counts(test);
        let title = if test { "test" } else { "non-test" };
        let hw = if n == 1 { "hit" } else { "hits" };
        let fw = if files == 1 { "file" } else { "files" };
        format!("## {title} ({n} {hw} / {files} {fw})\n")
    }

    fn file_header(&self, path: &str, test: bool) -> String {
        format!("{path} ({})\n", self.file_count(path, test))
    }

    fn hit_line(&self, i: usize) -> String {
        let loc = &self.locations[i];
        let info = &self.infos[i];
        let mut line = format!(
            "  L{}:{}",
            loc.range.start.line.saturating_add(1),
            loc.range.start.character.saturating_add(1)
        );
        if info.declaration {
            line.push_str(" [declaration]");
        }
        match info.kind {
            HitKind::Comment => line.push_str(" [comment]"),
            HitKind::Str => line.push_str(" [string]"),
            HitKind::Code => {}
        }
        if let Some((name, start, end)) = &info.enclosing {
            line.push_str(&format!(" in {name}[{start}-{end}]"));
        }
        // The line already shows in an inline context block.
        if let Some(text) = info
            .text
            .as_deref()
            .filter(|t| !t.is_empty() && !self.ctx.covered.contains(&i))
        {
            line.push_str(": ");
            line.push_str(text);
        }
        line
    }

    /// One string per hit in display order; the first hit of a group or
    /// file carries the group / file header, so the budget charges it.
    fn items(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.order.len());
        let mut prev: Option<(bool, &str)> = None;
        for &i in &self.order {
            let test = self.infos[i].test;
            let path = self.locations[i].path.as_str();
            let mut item = String::new();
            if prev.is_none_or(|(t, _)| t != test) {
                item.push_str(&self.section_header(test));
            }
            if prev != Some((test, path)) {
                item.push_str(&self.file_header(path, test));
            }
            item.push_str(&self.hit_line(i));
            if let Some(block) = self.ctx.blocks.get(&i) {
                item.push('\n');
                item.push_str(block.trim_end_matches('\n'));
            }
            prev = Some((test, path));
            out.push(item);
        }
        out
    }

    /// Group / file headers for a page that starts mid-file.
    fn continuation_headers(&self, offset: usize) -> String {
        if offset == 0 || offset >= self.order.len() {
            return String::new();
        }
        let i = self.order[offset];
        let p = self.order[offset - 1];
        let (test, path) = (self.infos[i].test, self.locations[i].path.as_str());
        let mut out = String::new();
        if self.infos[p].test == test {
            out.push_str(&self.section_header(test));
            if self.locations[p].path.as_str() == path {
                out.push_str(&self.file_header(path, test));
            }
        }
        out
    }

    /// Wide result: declarations, then per-file counts (non-test first).
    fn wide_summary(&self) -> String {
        let mut out = format!(
            "Over {WIDE_REFERENCES} hits; per-file counts first. Pass offset=0 for row listing (paged by budget_tokens, non-test first).\n"
        );
        let decls: Vec<usize> = self
            .order
            .iter()
            .copied()
            .filter(|&i| self.infos[i].declaration)
            .take(5)
            .collect();
        if !decls.is_empty() {
            out.push_str("[declaration]:\n");
            for i in decls {
                out.push_str(&format!(
                    "  {} {}\n",
                    self.locations[i].path.as_str(),
                    self.hit_line(i).trim_start()
                ));
            }
        }
        for (test, cap) in [(false, 25usize), (true, 15usize)] {
            let mut files: Vec<(&str, usize)> = Vec::new();
            for &i in &self.order {
                if self.infos[i].test != test {
                    continue;
                }
                let p = self.locations[i].path.as_str();
                match files.iter_mut().find(|(f, _)| *f == p) {
                    Some((_, n)) => *n += 1,
                    None => files.push((p, 1)),
                }
            }
            if files.is_empty() {
                continue;
            }
            files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let (n, nf) = self.group_counts(test);
            let title = if test { "test" } else { "non-test" };
            let hw = if n == 1 { "hit" } else { "hits" };
            let fw = if nf == 1 { "file" } else { "files" };
            out.push_str(&format!("## {title} ({n} {hw} / {nf} {fw}), by file:\n"));
            for (f, n) in files.iter().take(cap) {
                out.push_str(&format!("  {n:>5}  {f}\n"));
            }
            if files.len() > cap {
                let rest: usize = files[cap..].iter().map(|(_, n)| n).sum();
                out.push_str(&format!(
                    "  …{} more files, {rest} hits\n",
                    files.len() - cap
                ));
            }
        }
        out
    }
}

/// Read-only rename preview: per-file checklist of the sites that hold the
/// old name (1-based line and column, UTF-16 units, as in the reference list). Nothing
/// is written; the caller edits file by file and verifies with
/// get_diagnostics.
fn render_rename_preview(
    symbol: &str,
    new_name: &str,
    locations: &[Location],
    confidence: Confidence,
    ambiguous: bool,
    budget_tokens: usize,
) -> (String, usize) {
    let new_name = new_name.trim();
    let valid = !new_name.is_empty()
        && !new_name.chars().next().is_some_and(|c| c.is_ascii_digit())
        && new_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    if !valid {
        return (
            format!("Rename preview not generated:dry_run_rename=`{new_name}` is not a valid identifier.\n"),
            0,
        );
    }
    if new_name == symbol {
        return (
            "Rename preview not generated: new name is the same as the old name; no changes needed.\n".to_string(),
            0,
        );
    }
    if confidence == Confidence::Unknown {
        return (
            format!(
                "Rename preview not generated: reference query for `{symbol}` has no trusted result (confidence: Unknown); cannot decide whether edits are needed.\n"
            ),
            0,
        );
    }
    if ambiguous {
        return (
            format!(
                "Rename preview not generated: `{symbol}` has multiple same-name definitions in this file; references were merged and are not a single-symbol rename list. Pass line/character to pick one, then preview again.\n"
            ),
            0,
        );
    }
    if locations.is_empty() {
        return (
            format!("Rename preview: language server returned no references for `{symbol}`; nothing to change.\n"),
            0,
        );
    }
    let mut by_file: Vec<(&str, Vec<String>)> = Vec::new();
    for loc in locations {
        let site = format!(
            "L{}:{}",
            loc.range.start.line.saturating_add(1),
            loc.range.start.character.saturating_add(1)
        );
        match by_file.last_mut() {
            Some((path, sites)) if *path == loc.path.as_str() => sites.push(site),
            _ => by_file.push((loc.path.as_str(), vec![site])),
        }
    }
    let mut out = format!(
        "Rename preview (read-only, not written): `{symbol}` → `{new_name}`, {} hits / {} files total. Per file, replace `{symbol}` with `{new_name}` at:\n",
        locations.len(),
        by_file.len()
    );
    let lines: Vec<String> = by_file
        .iter()
        .map(|(path, sites)| format!("  {path} ×{}: {}", sites.len(), sites.join(" ")))
        .collect();
    let (kept, omitted) = truncate_ranked(&lines, budget_tokens, |l| format!("{l}\n"));
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    if omitted > 0 {
        out.push_str(&format!(
            "(incomplete) {omitted} more files omitted due to budget_tokens; raise budget_tokens for the full list.\n"
        ));
    }
    if confidence == Confidence::Exact {
        out.push_str("Checklist from the language server (precise); after replacements run build/tests to verify (use get_diagnostics per file only if you cannot).\n");
    } else {
        out.push_str("Note: reference confidence is below exact; the checklist may miss or false-positive — verify before replacing.\n");
    }
    (out, by_file.len())
}

pub(crate) fn run_goto_definition(
    mut params: GotoDefinitionParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    params.budget_tokens = effective_budget(params.budget_tokens);
    if params.path.trim().is_empty() {
        return text_result(
            unknown_preamble_definition(Some("path cannot be empty."), None),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let has_position = params.line.is_some() && params.character.is_some();
    let symbol = params
        .symbol
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if !has_position && symbol.is_none() {
        let hint = if params.line.is_some() ^ params.character.is_some() {
            "Provide both line and character, or a non-empty symbol."
        } else {
            "Provide symbol, or both line and character."
        };
        return text_result(
            unknown_preamble_definition(Some(hint), Some(params.path.as_str())),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let result = backend.definition(
        &params.path,
        symbol,
        if has_position { params.line } else { None },
        if has_position { params.character } else { None },
    );
    let locations = result.value;

    let preamble = if result.confidence == Confidence::Unknown {
        unknown_preamble_definition(result.note.as_deref(), Some(params.path.as_str()))
    } else {
        let mut header = confidence_header(result.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        if locations.is_empty() {
            let label = symbol.unwrap_or("this location");
            header.push_str(&format!("Definition not found for `{label}`.\n"));
        }
        header
    };

    let offset = params.offset.unwrap_or(0);
    let items: Vec<String> = locations.iter().map(render_location).collect();
    let overhead = estimate_tokens(&preamble)
        + estimate_tokens(COORD_NOTE)
        + estimate_tokens(&omitted_line(items.len(), items.len()));
    let (kept, omitted) = page(
        &items,
        offset,
        params.budget_tokens.saturating_sub(overhead),
    );

    let mut body = preamble;
    if !kept.is_empty() {
        body.push_str(COORD_NOTE);
    }
    if offset > 0 && offset >= items.len() && !items.is_empty() {
        body.push_str(&format!(
            "offset={offset} beyond total result count {}.\n",
            items.len()
        ));
    }
    for line in &kept {
        body.push_str(line);
        body.push('\n');
    }
    let next_offset = offset + kept.len();
    if omitted > 0 {
        body.push_str(&omitted_line(omitted, next_offset));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": locations.len(),
            "offset": offset,
            "returned": kept.len(),
            "omitted": omitted,
            "truncated": omitted > 0,
            "next_offset": (omitted > 0).then_some(next_offset),
        }),
    )
}

pub(crate) fn run_get_diagnostics(
    params: GetDiagnosticsParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    run_get_diagnostics_with(params, backend, true)
}

/// [`run_get_diagnostics`] with an explicit toolchain policy.
pub(crate) fn run_get_diagnostics_with(
    params: GetDiagnosticsParams,
    backend: &dyn PreciseCapability,
    toolchain: bool,
) -> CallToolResult {
    if params.path.trim().is_empty() {
        return text_result(
            unknown_preamble(Some("path cannot be empty."), None, false),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let min_severity = match params.severity.as_deref() {
        None => None,
        Some(raw) => match parse_severity(raw) {
            Some(severity) => Some(severity),
            None => {
                return text_result(
                    unknown_preamble(
                        Some(&format!(
                            "Unrecognized severity: {raw}. Allowed: error, warning, information, hint."
                        )),
                        Some(params.path.as_str()),
                        false,
                    ),
                    json!({
                        "confidence": "unknown",
                        "budget_tokens": params.budget_tokens,
                        "matches": 0,
                    }),
                );
            }
        },
    };

    let result =
        backend.diagnostics_with_options(&params.path, params.lsp.unwrap_or(false), toolchain);
    if let Some(fault) = crate::diag_env::detect(&params.path, &result.value) {
        return env_fault_result(&params, &result.value, &fault);
    }
    let mut diagnostics = result.value;
    if let Some(min) = min_severity {
        diagnostics.retain(|d| d.severity <= min);
    }
    diagnostics.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.range.cmp(&b.range))
            .then(a.severity.cmp(&b.severity))
            .then(a.message.cmp(&b.message))
    });

    let preamble = if result.confidence == Confidence::Unknown {
        unknown_preamble(result.note.as_deref(), Some(params.path.as_str()), false)
    } else {
        let mut header = confidence_header(result.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        if diagnostics.is_empty() {
            header.push_str(&format!("No matching diagnostics for `{}`.\n", params.path));
        }
        header
    };

    let items: Vec<String> = diagnostics.iter().map(render_diagnostic).collect();
    let (kept, omitted) = truncate_ranked(&items, params.budget_tokens, |line| format!("{line}\n"));
    let mut body = preamble;
    for line in kept {
        body.push_str(line);
        body.push('\n');
    }
    if omitted > 0 {
        body.push_str(&format!("{omitted} omitted due to budget_tokens.\n"));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": diagnostics.len(),
        }),
    )
}

/// The toolchain environment is broken (read-only GOCACHE, modules not
/// downloadable, wrong venv, no node_modules…), so the diagnostics are
/// cascade noise. Say so in a few lines: the root-cause line, how many were
/// dropped, and any parser errors (those do not depend on the environment).
fn env_fault_result(
    params: &GetDiagnosticsParams,
    diags: &[Diagnostic],
    fault: &crate::diag_env::EnvFault,
) -> CallToolResult {
    const ROOT_MAX_CHARS: usize = 400;
    const SYNTAX_MAX: usize = 10;
    let mut root_line = render_diagnostic(&diags[fault.root]);
    if root_line.chars().count() > ROOT_MAX_CHARS {
        root_line = root_line.chars().take(ROOT_MAX_CHARS).collect::<String>() + "…";
    }
    let syntax: Vec<&Diagnostic> = diags
        .iter()
        .enumerate()
        .filter(|(i, d)| *i != fault.root && crate::diag_env::is_syntax_error(d))
        .map(|(_, d)| d)
        .collect();
    let suppressed = diags.len() - 1 - syntax.len();
    let mut body = confidence_header(Confidence::Unknown);
    body.push_str(&format!(
        "Environment issue; diagnostics are not trustworthy:{}.\nRoot cause: {root_line}\n",
        fault.kind
    ));
    if suppressed > 0 {
        body.push_str(&format!(
            "{suppressed} more diagnostics are mostly cascade errors from it and were omitted.\n"
        ));
    }
    if !syntax.is_empty() {
        body.push_str(
            "The following syntax errors are unrelated to the environment and still trustworthy:\n",
        );
        for d in syntax.iter().take(SYNTAX_MAX) {
            body.push_str(&render_diagnostic(d));
            body.push('\n');
        }
    }
    body.push_str(fault.hint);
    body.push('\n');
    text_result(
        body,
        json!({
            "confidence": "unknown",
            "budget_tokens": params.budget_tokens,
            "matches": syntax.len(),
            "environment_fault": fault.kind,
            "suppressed": suppressed,
        }),
    )
}

fn text_result(body: String, structured: Value) -> CallToolResult {
    let body = if body.trim().is_empty() {
        "No matches. confidence: unknown".to_string()
    } else {
        body
    };
    let mut result = CallToolResult::success(vec![ContentBlock::text(body)]);
    result.structured_content = Some(structured);
    result
}

fn confidence_header(confidence: Confidence) -> String {
    let note = confidence_note(confidence);
    if note.is_empty() {
        format!("confidence: {confidence:?}\n")
    } else {
        format!("confidence: {confidence:?} {note}\n")
    }
}

fn unknown_preamble(note: Option<&str>, path: Option<&str>, suggest_search: bool) -> String {
    let mut body = confidence_header(Confidence::Unknown);
    if let Some(note) = note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            body.push_str(trimmed);
            body.push('\n');
        }
    }
    if server_missing(note) {
        body.push_str(&unavailable_ensure_gate(language_of(path)));
        // Discovery's own message (e.g. a rustup proxy without a toolchain)
        // already says what to run; a generic hint would contradict it.
        if !note_has_install_command(note) {
            body.push_str("Install hints:\n");
            body.push_str(&install_hint(language_of(path)));
        }
    }
    if suggest_search {
        body.push_str(SEARCH_CODE_FALLBACK);
        body.push('\n');
    }
    body
}

fn unknown_preamble_definition(note: Option<&str>, path: Option<&str>) -> String {
    let mut body = confidence_header(Confidence::Unknown);
    if let Some(note) = note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            body.push_str(trimmed);
            body.push('\n');
        }
    }
    if server_missing(note) {
        body.push_str(&unavailable_ensure_gate(language_of(path)));
        // Discovery's own message (e.g. a rustup proxy without a toolchain)
        // already says what to run; a generic hint would contradict it.
        if !note_has_install_command(note) {
            body.push_str("Install hints:\n");
            body.push_str(&install_hint(language_of(path)));
        }
    }
    body.push_str(DEFINITION_FALLBACK);
    body.push('\n');
    body
}

/// True when the note says the language server itself is missing or cannot
/// start (or when there is no note at all). Only then is an install hint
/// useful; a bad symbol, position, or timeout with a working server is not
/// fixed by installing anything.
fn server_missing(note: Option<&str>) -> bool {
    let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) else {
        return true;
    };
    // A timeout means the server is present; installing again does not help.
    // Match before the "not installed" substring checks so a carefully worded
    // timeout note cannot trip an install hint.
    if note.contains("timed out") || note.contains("timed out") {
        return false;
    }
    [
        "not installed",
        "not configured",
        "no language server available",
        "failed to start",
        "not wired yet",
        "Language server unavailable",
    ]
    .iter()
    .any(|m| note.contains(m))
}

/// True when the note already carries a concrete command to run.
fn note_has_install_command(note: Option<&str>) -> bool {
    let Some(note) = note else { return false };
    note.contains('`')
        || [
            "rustup ",
            "go install",
            "npm i",
            "brew install",
            "pip install",
            "apt-get install",
            "gem install",
        ]
        .iter()
        .any(|m| note.contains(m))
}

/// Prompt contract when a precise tool hits Unavailable: tell the model to
/// install the language server locally, or inspect `get_languages`.
fn unavailable_ensure_gate(language: Option<Language>) -> String {
    let (server, _lang_name) = match language {
        Some(lang) => (
            astrolabe_core::lsp::router::primary_server_name(lang),
            lang.name(),
        ),
        None => ("the language server", "<language>"),
    };
    format!(
        "Language server unavailable ({server}). Install it on this machine and retry, or use get_languages for Ready / needs_install / AST-only.\n"
    )
}

fn language_of(path: Option<&str>) -> Option<Language> {
    path.and_then(|p| Language::from_path(&RelPath::new(p)))
}

/// Human-readable install commands, mirroring `ServerSpec.install_hint`.
///
/// `discovery` will own the authoritative strings once that workstream
/// lands; these are the fallback so an agent can still act today.
pub(crate) fn install_hint(language: Option<Language>) -> String {
    match language {
        Some(language) => format!("- {}\n", install_hint_one(language)),
        None => {
            let mut out = String::new();
            for language in [
                Language::Python,
                Language::Go,
                Language::Rust,
                Language::TypeScript,
                Language::Java,
                Language::Php,
                Language::Dart,
            ] {
                out.push_str("- ");
                out.push_str(&install_hint_one(language));
                out.push('\n');
            }
            out
        }
    }
}

fn install_hint_one(language: Language) -> String {
    match language {
        Language::Python => {
            "Python (pyright): `npm i -g pyright`, stdio `pyright-langserver --stdio`. Or `pip install pyright`.".into()
        }
        Language::Go => {
            "Go(gopls):`go install golang.org/x/tools/gopls@latest`.".into()
        }
        Language::Rust => {
            "Rust(rust-analyzer):`rustup component add rust-analyzer`.".into()
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            "TypeScript/JavaScript (typescript-language-server): `npm i -g typescript-language-server typescript`, stdio `typescript-language-server --stdio`.".into()
        }
        Language::Java => {
            "Java (Eclipse JDT LS / jdtls): launch args depend on the distribution; Homebrew: `brew install jdtls`. Not auto-started by default.".into()
        }
        Language::Swift => {
            "Swift (sourcekit-lsp): `xcode-select --install` or full Xcode; override with ASTROLABE_LSP_SWIFT.".into()
        }
        Language::ObjC | Language::ObjCpp => {
            "ObjC/ObjC++: prefer clangd (`brew install llvm` / `apt-get install clangd`), fall back to sourcekit-lsp (`xcode-select --install`).".into()
        }
        Language::C | Language::Cpp => {
            "C/C++ (clangd): `brew install llvm`, or set ASTROLABE_LSP_C.".into()
        }
        Language::Kotlin => {
            "Kotlin: prefer kotlin-lsp (JetBrains, `brew install JetBrains/utils/kotlin-lsp`), fall back to kotlin-language-server; override with ASTROLABE_LSP_KOTLIN.".into()
        }
        Language::Shell => {
            "Shell/Bash (bash-language-server): `npm i -g bash-language-server`; optional shellcheck.".into()
        }
        Language::Ruby => {
            "Ruby (ruby-lsp): `gem install ruby-lsp`, fall back to solargraph.".into()
        }
        Language::Lua => {
            "Lua(lua-language-server):`brew install lua-language-server`.".into()
        }
        Language::Proto => {
            "Protobuf: no dedicated language server; message / enum / service / rpc are tree-sitter indexed — use find_symbol / search_code.".into()
        }
        Language::Php => {
            "PHP(intelephense):`npm i -g intelephense`.".into()
        }
        Language::Vue => {
            "Vue (Volar / vue-language-server): `npm i -g @vue/language-server typescript-language-server typescript`, stdio `vue-language-server --stdio`. Optional ASTROLABE_LSP_VUE. 2.x single process (hybridMode=false); 3.x auto-bridges typescript-language-server + @vue/typescript-plugin.".into()
        }
        Language::Dart => "Dart (dart language-server): bundled with the Dart / Flutter SDK.".into(),
        Language::CSharp => {
            "C# (Microsoft.CodeAnalysis.LanguageServer): `dotnet <dll> --stdio`; needs .NET 10 runtime (analyzed target frameworks may be older); set ASTROLABE_LSP_CSHARP. Not OmniSharp / csharp-ls.".into()
        }
        Language::VisualBasic => {
            "Visual Basic .NET (same Microsoft.CodeAnalysis.LanguageServer): `dotnet <dll> --stdio`; shares ASTROLABE_LSP_CSHARP with C#; .vb / .vbproj / .sln. Not OmniSharp.".into()
        }
    }
}

fn parse_severity(raw: &str) -> Option<Severity> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" | "err" => Some(Severity::Error),
        "warning" | "warn" => Some(Severity::Warning),
        "information" | "info" | "informational" => Some(Severity::Information),
        "hint" => Some(Severity::Hint),
        _ => None,
    }
}

fn render_location(loc: &Location) -> String {
    // LSP positions are 0-based; every position we print is 1-based
    // (line in the anchor, UTF-16 column after `col`).
    format!(
        "{}  col {}",
        anchor(&loc.path, loc.range.start.line.saturating_add(1)),
        loc.range.start.character.saturating_add(1)
    )
}

fn render_diagnostic(diag: &Diagnostic) -> String {
    let code = diag
        .code
        .as_deref()
        .map(|c| format!(" {c}"))
        .unwrap_or_default();
    format!(
        "{} [{}{code}] {}",
        anchor(&diag.path, diag.range.start.line.saturating_add(1)),
        severity_label(diag.severity),
        diag.message
    )
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "information",
        Severity::Hint => "hint",
    }
}

fn confidence_json(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Exact => "exact",
        Confidence::Scoped => "scoped",
        Confidence::Syntactic => "syntactic",
        Confidence::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::lsp::{Position, Range};

    fn loc(path: &str, line: u32, character: u32) -> Location {
        Location {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character },
                end: Position {
                    line,
                    character: character + 1,
                },
            },
        }
    }

    fn diag(path: &str, line: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity,
            message: message.into(),
            code: None,
        }
    }

    #[derive(Debug)]
    struct Fake {
        references: Precise<Vec<Location>>,
        definition: Precise<Vec<Location>>,
        diagnostics: Precise<Vec<Diagnostic>>,
    }

    impl PreciseCapability for Fake {
        fn references(
            &self,
            _symbol: &str,
            _path: Option<&str>,
            _position: Option<Position>,
            _exact: bool,
        ) -> Precise<Vec<Location>> {
            self.references.clone()
        }
        fn definition(
            &self,
            _path: &str,
            _symbol: Option<&str>,
            _line: Option<u32>,
            _character: Option<u32>,
        ) -> Precise<Vec<Location>> {
            self.definition.clone()
        }
        fn diagnostics(&self, _path: &str) -> Precise<Vec<Diagnostic>> {
            self.diagnostics.clone()
        }
    }

    fn tools_with(fake: Fake) -> PreciseTools {
        PreciseTools::with_backend(Arc::new(fake))
    }

    fn first_text(result: &CallToolResult) -> &str {
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|content| content.text.as_str())
            .unwrap_or_default()
    }

    fn assert_first_text(result: CallToolResult) -> CallToolResult {
        let text = first_text(&result);
        assert!(
            !text.trim().is_empty(),
            "content[0].text must always be non-empty"
        );
        assert!(
            !result.is_error.unwrap_or(false),
            "precise tools report missing LSP as a result, not isError"
        );
        result
    }

    fn schema_for_tool(name: &str) -> Value {
        let tools = PreciseTools::new();
        let listed = tools.tool_router.list_all();
        let tool = listed
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        serde_json::to_value(&tool.input_schema).unwrap()
    }

    fn required_fields(schema: &Value) -> Vec<String> {
        schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn tool_count_and_names_match_catalog_constant() {
        let tools = PreciseTools::new().tool_router.list_all();
        assert_eq!(tools.len(), crate::PRECISE_TOOL_COUNT);
        let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["find_references", "get_diagnostics", "goto_definition"]
        );
        assert!(
            !names.iter().any(|n| n == "rename_symbol"),
            "rename_symbol is not a catalog name"
        );
    }

    #[test]
    fn schemas_required_and_optional_fields() {
        let refs = schema_for_tool("find_references");
        assert!(refs.pointer("/properties/symbol").is_some());
        assert!(refs.pointer("/properties/path").is_some());
        assert!(refs.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&refs);
        assert!(required.iter().any(|f| f == "symbol"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "path"),
            "path stays optional (session inherit / index fix / candidates): {required:?}"
        );
        let path_desc = refs
            .pointer("/properties/path/description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            path_desc.contains("Strongly preferred") || path_desc.contains("session"),
            "path description should steer models: {path_desc}"
        );

        let diags = schema_for_tool("get_diagnostics");
        assert!(diags.pointer("/properties/path").is_some());
        assert!(diags.pointer("/properties/severity").is_some());
        assert!(diags.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&diags);
        assert!(required.iter().any(|f| f == "path"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "severity"),
            "severity must be optional: {required:?}"
        );

        let defs = schema_for_tool("goto_definition");
        assert!(defs.pointer("/properties/path").is_some());
        assert!(defs.pointer("/properties/symbol").is_some());
        assert!(defs.pointer("/properties/line").is_some());
        assert!(defs.pointer("/properties/character").is_some());
        assert!(defs.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&defs);
        assert!(required.iter().any(|f| f == "path"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "symbol"),
            "symbol must be optional: {required:?}"
        );
        assert!(
            !required.iter().any(|f| f == "line"),
            "line must be optional: {required:?}"
        );
        assert!(
            !required.iter().any(|f| f == "budget_tokens"),
            "budget_tokens must be optional: {required:?}"
        );
    }

    #[test]
    fn every_tool_returns_nonempty_first_text_when_unwired() {
        let tools = PreciseTools::new();
        assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "Handler".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            index_root: None,
            path: "src/main.go".into(),
            symbol: Some("Handler".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        })));
        assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            index_root: None,
            path: "src/main.go".into(),
            severity: None,
            budget_tokens: 800,
            lsp: None,
            toolchain: true,
        })));
    }

    #[test]
    fn lsp_unavailable_degrades_with_install_hint() {
        let tools = tools_with(Fake {
            references: Precise::unknown(Vec::new(), "gopls is not installed"),
            definition: Precise::unknown(Vec::new(), "gopls is not installed"),
            diagnostics: Precise::unknown(Vec::new(), "gopls is not installed"),
        });

        let refs = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "Handler".into(),
            path: Some("pkg/server.go".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        let text = first_text(&refs);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(
            text.contains(confidence_note(Confidence::Unknown)),
            "{text}"
        );
        assert!(text.contains("gopls"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(text.contains("get_languages"), "{text}");
        assert!(text.contains("search_code"), "{text}");
        assert!(text.contains("false positive"), "{text}");
        assert!(
            text.contains("not wired yet") || text.contains("not installed"),
            "{text}"
        );

        let diags = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            index_root: None,
            path: "pkg/server.go".into(),
            severity: None,
            budget_tokens: 800,
            lsp: None,
            toolchain: true,
        })));
        let text = first_text(&diags);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(text.contains("get_languages"), "{text}");
        assert!(!text.contains("search_code"), "{text}");

        let defs = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            index_root: None,
            path: "pkg/server.go".into(),
            symbol: Some("Handler".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        })));
        let text = first_text(&defs);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(
            text.contains("find_symbol") || text.contains("binding resolution"),
            "{text}"
        );
    }

    #[test]
    fn budget_truncation_keeps_nonempty_text() {
        let many: Vec<Location> = (0..20).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = tools_with(Fake {
            references: Precise::exact(many.clone()),
            definition: Precise::exact(many),
            diagnostics: Precise::exact(
                (0..20)
                    .map(|i| diag("src/lib.rs", i, Severity::Error, "boom"))
                    .collect(),
            ),
        });

        let refs = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "x".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 0,
            offset: None,
            dry_run_rename: None,
        })));
        let text = first_text(&refs);
        assert!(text.contains("omitted due to budget_tokens"), "{text}");
        let structured = refs.structured_content.unwrap();
        assert_eq!(structured["budget_tokens"], json!(0));
        assert_eq!(structured["truncated"], json!(true));
        assert!(structured["omitted"].as_u64().unwrap() >= 1);
        assert_eq!(
            structured["returned"].as_u64().unwrap() + structured["omitted"].as_u64().unwrap(),
            structured["matches"].as_u64().unwrap()
        );

        let diags = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            index_root: None,
            path: "src/lib.rs".into(),
            severity: None,
            budget_tokens: 0,
            lsp: None,
            toolchain: true,
        })));
        let text = first_text(&diags);
        assert!(text.contains("omitted due to budget_tokens"), "{text}");
    }

    #[test]
    fn exact_references_use_path_line_anchors() {
        let tools = tools_with(Fake {
            references: Precise::exact(vec![loc("src/b.rs", 9, 4), loc("src/a.rs", 2, 0)]),
            definition: Precise::exact(vec![loc("src/a.rs", 2, 0)]),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "x".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("src/a.rs (1)\n  L3:1"), "{text}");
        assert!(text.contains("src/b.rs (1)\n  L10:5"), "{text}");
        // Backend order is kept (origin file first), not re-sorted by path.
        let a = text.find("src/a.rs (1)").unwrap();
        let b = text.find("src/b.rs (1)").unwrap();
        assert!(b < a, "backend order must be preserved: {text}");
    }

    #[test]
    fn diagnostics_severity_filter_and_invalid_value() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(vec![
                diag("src/a.rs", 1, Severity::Error, "broken"),
                diag("src/a.rs", 2, Severity::Hint, "style"),
            ]),
        });
        let filtered = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            index_root: None,
            path: "src/a.rs".into(),
            severity: Some("error".into()),
            budget_tokens: 800,
            lsp: None,
            toolchain: true,
        })));
        let text = first_text(&filtered);
        assert!(text.contains("[error]"), "{text}");
        assert!(!text.contains("[hint]"), "{text}");
        assert!(text.contains("@src/a.rs:2"), "{text}");

        let invalid = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            index_root: None,
            path: "src/a.rs".into(),
            severity: Some("fatal".into()),
            budget_tokens: 800,
            lsp: None,
            toolchain: true,
        })));
        let text = first_text(&invalid);
        assert!(text.contains("Unrecognized severity"), "{text}");
        assert!(text.contains("confidence: Unknown"), "{text}");
    }

    #[test]
    fn dry_run_rename_lists_edits_per_file_without_writing() {
        let tools = tools_with(Fake {
            references: Precise::exact(vec![
                loc("src/a.rs", 3, 4),
                loc("src/a.rs", 9, 8),
                loc("src/b.rs", 0, 12),
            ]),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "old_name".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: Some("new_name".into()),
        })));
        let text = first_text(&result);
        assert!(
            text.contains("Rename preview (read-only, not written)"),
            "{text}"
        );
        assert!(
            text.contains("`old_name` → `new_name`, 3 hits / 2 files total"),
            "{text}"
        );
        assert!(text.contains("  src/a.rs ×2: L4:5 L10:9"), "{text}");
        assert!(text.contains("  src/b.rs ×1: L1:13"), "{text}");
        assert!(text.contains("get_diagnostics"), "{text}");
        // No preview without the parameter.
        let plain = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "old_name".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        assert!(!first_text(&plain).contains("Rename preview"));
    }

    #[test]
    fn dry_run_rename_rejects_bad_names_and_flags_low_confidence() {
        let (bad, _) = render_rename_preview(
            "a",
            "1x",
            &[loc("a.rs", 0, 0)],
            Confidence::Exact,
            false,
            800,
        );
        assert!(bad.contains("is not a valid identifier"), "{bad}");
        let (same, _) = render_rename_preview(
            "a",
            "a",
            &[loc("a.rs", 0, 0)],
            Confidence::Exact,
            false,
            800,
        );
        assert!(same.contains("same"), "{same}");
        let (none, n) = render_rename_preview("a", "b", &[], Confidence::Exact, false, 800);
        assert!(none.contains("nothing to change") && n == 0, "{none}");
        let (low, _) = render_rename_preview(
            "a",
            "b",
            &[loc("a.rs", 0, 0)],
            Confidence::Syntactic,
            false,
            800,
        );
        assert!(low.contains("below exact"), "{low}");
    }

    #[test]
    fn empty_exact_references_are_not_unknown() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "neverDefined".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 100,
            offset: None,
            dry_run_rename: None,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("not found"), "{text}");
        assert!(!text.contains("search_code"), "{text}");
        assert!(!text.contains("not wired yet"), "{text}");
    }

    #[test]
    fn goto_definition_exact_hits_use_anchors() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(vec![loc("src/lib.rs", 11, 4)]),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            index_root: None,
            path: "src/main.rs".into(),
            symbol: Some("Widget".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("@src/lib.rs:12"), "{text}");
        assert!(text.contains("col 5"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["matches"],
            json!(1)
        );
    }

    #[test]
    fn empty_exact_definitions_are_not_unknown() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            index_root: None,
            path: "src/main.rs".into(),
            symbol: Some("neverDefined".into()),
            line: None,
            character: None,
            budget_tokens: 100,
            offset: None,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("not found"), "{text}");
        assert!(!text.contains("not wired yet"), "{text}");
        assert!(!text.contains("search_code"), "{text}");
    }

    #[test]
    fn goto_definition_missing_path_or_locator_is_unknown() {
        let tools = PreciseTools::new();
        let missing_path =
            assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
                root: None,
                index_root: None,
                path: "".into(),
                symbol: Some("x".into()),
                line: None,
                character: None,
                budget_tokens: 100,
                offset: None,
            })));
        let text = first_text(&missing_path);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("path"), "{text}");

        let missing_locator =
            assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
                root: None,
                index_root: None,
                path: "src/main.rs".into(),
                symbol: None,
                line: None,
                character: None,
                budget_tokens: 100,
                offset: None,
            })));
        let text = first_text(&missing_locator);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("symbol"), "{text}");
        assert!(
            text.contains("find_symbol") || text.contains("search_code"),
            "{text}"
        );
    }

    #[test]
    fn goto_definition_budget_zero_keeps_nonempty_text() {
        let many: Vec<Location> = (0..20).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(many),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            index_root: None,
            path: "src/lib.rs".into(),
            symbol: Some("x".into()),
            line: None,
            character: None,
            budget_tokens: 0,
            offset: None,
        })));
        let text = first_text(&result);
        assert!(
            text.contains("omitted due to budget_tokens") || text.contains("confidence:"),
            "{text}"
        );
    }

    #[test]
    fn default_install_hint_includes_dart() {
        let hint = install_hint(None);
        assert!(hint.contains("Dart"));
        assert!(hint.contains("dart language-server"));
    }

    fn refs_params(
        budget: usize,
        offset: Option<usize>,
        rename: Option<&str>,
    ) -> FindReferencesParams {
        FindReferencesParams {
            root: None,
            index_root: None,
            symbol: "Run".into(),
            path: Some("a.go".into()),
            line: None,
            character: None,
            budget_tokens: budget,
            offset,
            dry_run_rename: rename.map(str::to_string),
        }
    }

    fn fake_refs(references: Precise<Vec<Location>>) -> PreciseTools {
        tools_with(Fake {
            references,
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        })
    }

    #[test]
    fn rename_preview_refused_when_references_unknown() {
        let tools = fake_refs(Precise::unknown(Vec::new(), "gopls is not installed"));
        let r = tools.find_references(Parameters(refs_params(800, None, Some("Go"))));
        let text = first_text(&r);
        assert!(text.contains("Rename preview not generated"), "{text}");
        assert!(!text.contains("no changes needed"), "{text}");
    }

    #[test]
    fn rename_preview_refused_for_merged_definitions_and_not_called_exact() {
        let note = format!("{AMBIGUOUS_DEFINITIONS_MARK} `Run` has 3 definitions");
        let tools = fake_refs(Precise {
            value: vec![loc("a.go", 5, 11), loc("a.go", 9, 11)],
            confidence: Confidence::Scoped,
            note: Some(note),
        });
        let r = tools.find_references(Parameters(refs_params(800, None, Some("Go"))));
        let text = first_text(&r);
        assert!(text.contains("multiple same-name definitions"), "{text}");
        assert!(
            !text.contains("Checklist from language server (precise)"),
            "{text}"
        );
        assert!(!text.contains("Rename preview (read-only"), "{text}");
        assert!(text.contains("confidence: Scoped"), "{text}");
    }

    #[test]
    fn truncation_keeps_backend_order_queried_file_first() {
        // Backend order: queried file (zzz.go) first, then aaa.go.
        let mut v = vec![loc("zzz.go", 0, 0)];
        v.extend((0..16).map(|i| loc("aaa.go", i, 0)));
        let tools = fake_refs(Precise::exact(v));
        let r = tools.find_references(Parameters(refs_params(8, None, None)));
        let text = first_text(&r);
        assert!(text.contains("zzz.go (1)\n  L1:1"), "{text}");
    }

    #[test]
    fn budget_is_capped_and_preview_shares_it() {
        let many: Vec<Location> = (0..30_000).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = fake_refs(Precise::exact(many));
        let r = tools.find_references(Parameters(refs_params(100_000_000, None, None)));
        let s = r.structured_content.clone().unwrap();
        assert_eq!(s["budget_tokens"], json!(MAX_BUDGET_TOKENS));
        assert!(s["truncated"].as_bool().unwrap());
        let text = first_text(&r);
        assert!(
            estimate_tokens(text) <= MAX_BUDGET_TOKENS + 300,
            "{}",
            estimate_tokens(text)
        );

        // Preview: list + preview together stay within one budget.
        let r = tools.find_references(Parameters(refs_params(400, None, Some("Go"))));
        let text = first_text(&r);
        assert!(
            estimate_tokens(text) <= 400 + 120,
            "{}",
            estimate_tokens(text)
        );
    }

    #[test]
    fn find_references_output_including_header_fits_budget() {
        let many: Vec<Location> = (0..2_000).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = fake_refs(Precise::exact(many));
        for budget in [60usize, 150, 400, 1000] {
            let r = tools.find_references(Parameters(refs_params(budget, None, None)));
            let tokens = estimate_tokens(first_text(&r));
            assert!(
                tokens <= budget,
                "budget {budget}: output is {tokens} tokens"
            );
        }
    }

    #[test]
    fn find_references_pages_with_offset() {
        let many: Vec<Location> = (0..40).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = fake_refs(Precise::exact(many));
        let first = tools.find_references(Parameters(refs_params(60, None, None)));
        let s1 = first.structured_content.clone().unwrap();
        assert_eq!(s1["truncated"], json!(true));
        let next = s1["next_offset"].as_u64().unwrap() as usize;
        assert!(next > 0);
        let t1 = first_text(&first).to_string();
        assert!(t1.contains(&format!("offset={next}")), "{t1}");
        let second = tools.find_references(Parameters(refs_params(60, Some(next), None)));
        let t2 = first_text(&second);
        assert!(t2.contains(&format!("\n  L{}:1", next + 1)), "{t2}");
        assert!(!t2.contains("\n  L1:1\n"), "{t2}");
        // A page that starts mid-file repeats the file header.
        assert!(t2.contains("src/lib.rs (40)\n"), "{t2}");
        // Zero budget still advances.
        let z = tools.find_references(Parameters(refs_params(0, Some(3), None)));
        assert_eq!(z.structured_content.unwrap()["next_offset"], json!(4));
    }

    #[test]
    fn goto_definition_pages_and_lists_all_definitions() {
        let defs: Vec<Location> = (0..3).map(|i| loc("a.go", i * 10, 5)).collect();
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise {
                value: defs,
                confidence: Confidence::Scoped,
                note: Some(format!(
                    "{AMBIGUOUS_DEFINITIONS_MARK} `Run` has 3 declarations"
                )),
            },
            diagnostics: Precise::exact(Vec::new()),
        });
        let r = tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            index_root: None,
            path: "a.go".into(),
            symbol: Some("Run".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        }));
        let text = first_text(&r);
        assert!(
            text.contains("@a.go:1 ") && text.contains("@a.go:11 ") && text.contains("@a.go:21 "),
            "{text}"
        );
        assert!(text.contains("3 declarations"), "{text}");
        assert!(!text.contains("confidence: Exact"), "{text}");
    }

    #[test]
    fn coordinates_are_one_based_and_stated() {
        let tools = fake_refs(Precise::exact(vec![loc("a.go", 5, 11)]));
        let r = tools.find_references(Parameters(refs_params(800, None, Some("Go"))));
        let text = first_text(&r);
        assert!(text.contains("a.go (1)\n  L6:12"), "{text}");
        assert!(text.contains("L6:12"), "{text}");
        assert!(text.contains("UTF-16"), "{text}");
    }

    #[test]
    fn install_hint_only_when_server_is_missing() {
        let tools = fake_refs(Precise::unknown(
            Vec::new(),
            "symbol `NoSuchThing` not found in a.go; cannot provide precise references.",
        ));
        let r = tools.find_references(Parameters(refs_params(800, None, None)));
        let text = first_text(&r);
        assert!(text.contains("not found in a.go"), "{text}");
        assert!(!text.contains("go install"), "{text}");
        assert!(!text.contains("Language server unavailable"), "{text}");
    }

    #[derive(Debug)]
    struct Annotated {
        refs: Vec<Location>,
        infos: Vec<HitInfo>,
    }

    impl PreciseCapability for Annotated {
        fn references(
            &self,
            _symbol: &str,
            _path: Option<&str>,
            _position: Option<Position>,
            _exact: bool,
        ) -> Precise<Vec<Location>> {
            Precise::exact(self.refs.clone())
        }
        fn annotate(&self, _symbol: &str, _locations: &[Location]) -> Vec<HitInfo> {
            self.infos.clone()
        }
        fn definition(
            &self,
            _path: &str,
            _symbol: Option<&str>,
            _line: Option<u32>,
            _character: Option<u32>,
        ) -> Precise<Vec<Location>> {
            Precise::exact(Vec::new())
        }
        fn diagnostics(&self, _path: &str) -> Precise<Vec<Diagnostic>> {
            Precise::exact(Vec::new())
        }
    }

    fn info(
        text: &str,
        func: Option<(&str, u32, u32)>,
        decl: bool,
        kind: HitKind,
        test: bool,
    ) -> HitInfo {
        HitInfo {
            text: Some(text.into()),
            enclosing: func.map(|(n, a, b)| (n.to_string(), a, b)),
            declaration: decl,
            kind,
            test,
        }
    }

    #[test]
    fn reference_hits_are_self_contained_and_tests_come_last() {
        let tools = PreciseTools::with_backend(Arc::new(Annotated {
            refs: vec![
                loc("command_test.go", 40, 5),
                loc("command.go", 1069, 17),
                loc("command.go", 1147, 8),
                loc("command.go", 200, 3),
            ],
            infos: vec![
                info(
                    "c.Execute()",
                    Some(("TestX", 30, 50)),
                    false,
                    HitKind::Code,
                    true,
                ),
                info(
                    "func (c *Command) Execute() error {",
                    Some(("Execute", 1070, 1074)),
                    true,
                    HitKind::Code,
                    false,
                ),
                info(
                    "err = cmd.Execute(flags)",
                    Some(("ExecuteC", 1076, 1200)),
                    false,
                    HitKind::Code,
                    false,
                ),
                info("// Execute runs it", None, false, HitKind::Comment, false),
            ],
        }));
        let r = tools.find_references(Parameters(refs_params(2_000, None, None)));
        let text = first_text(&r);
        assert!(text.contains("4 hits / 2 files total (non-test 3 hits / 1 file, test 1 hit / 1 file; [declaration] 1 hit; comment/string 1 hit, tagged)"), "{text}");
        assert!(
            text.contains(
                "  L1070:18 [declaration] in Execute[1070-1074]: func (c *Command) Execute() error {"
            ),
            "{text}"
        );
        assert!(
            text.contains("  L1148:9 in ExecuteC[1076-1200]: err = cmd.Execute(flags)"),
            "{text}"
        );
        assert!(
            text.contains("  L201:4 [comment]: // Execute runs it"),
            "{text}"
        );
        let prod = text.find("## non-test (3 hits / 1 file)").expect(text);
        let test = text.find("## test (1 hit / 1 file)").expect(text);
        assert!(
            prod < test && text.find("command_test.go (1)").unwrap() > test,
            "{text}"
        );
    }

    #[test]
    fn very_large_reference_lists_start_with_per_file_counts() {
        let mut refs = Vec::new();
        let mut infos = Vec::new();
        for i in 0..120 {
            refs.push(loc("x_test.go", i, 1));
            infos.push(info("x", None, false, HitKind::Code, true));
        }
        refs.push(loc("a.go", 2, 5));
        infos.push(info(
            "func Run() {",
            Some(("Run", 3, 9)),
            true,
            HitKind::Code,
            false,
        ));
        for i in 0..60 {
            refs.push(loc("b.go", i, 1));
            infos.push(info("Run()", None, false, HitKind::Code, false));
        }
        let tools = PreciseTools::with_backend(Arc::new(Annotated { refs, infos }));
        let r = tools.find_references(Parameters(refs_params(2_500, None, None)));
        let text = first_text(&r);
        assert!(text.contains("181 hits / 3 files total"), "{text}");
        assert!(text.contains("Pass offset=0 for row listing"), "{text}");
        assert!(
            text.contains("  a.go L3:6 [declaration] in Run[3-9]: func Run() {"),
            "{text}"
        );
        let prod = text
            .find("## non-test (61 hits / 2 files), by file:\n     60  b.go\n      1  a.go")
            .expect(text);
        let test = text
            .find("## test (120 hits / 1 file), by file:\n    120  x_test.go")
            .expect(text);
        assert!(prod < test, "{text}");
        assert!(text.len() < 1_200, "{}", text.len());
        let s = r.structured_content.unwrap();
        assert_eq!(s["wide"], json!(true));
        assert_eq!(s["next_offset"], json!(0));
        // offset=0 lists hits, non-test first.
        let tools2 = PreciseTools::with_backend(Arc::new(Annotated {
            refs: (0..200).map(|i| loc("b.go", i, 1)).collect(),
            infos: vec![HitInfo::default(); 200],
        }));
        let r = tools2.find_references(Parameters(refs_params(500, Some(0), None)));
        let text = first_text(&r);
        assert!(text.contains("b.go (200)\n  L1:2\n"), "{text}");
    }

    #[test]
    fn discovery_hint_is_not_followed_by_a_generic_one() {
        let note = "no language server available for rust: rust-analyzer at ~/.cargo/bin is a rustup proxy with no toolchain; run `rustup default stable`";
        let text = unknown_preamble(Some(note), Some("src/lib.rs"), false);
        assert!(text.contains("rustup default stable"), "{text}");
        assert!(!text.contains("Install hints"), "{text}");
        let text = unknown_preamble(Some("gopls is not installed"), Some("a.go"), false);
        assert!(
            text.contains("Install hints") && text.contains("go install"),
            "{text}"
        );
    }

    #[test]
    fn diagnostics_timeout_note_does_not_trigger_install_hint() {
        let note = "language server timed out after 2.5s waiting for textDocument/publishDiagnostics on src/a.ts;                     the server never published — this is a timeout (server present), not a missing install";
        let text = unknown_preamble(Some(note), Some("src/a.ts"), false);
        assert!(text.contains("timed out"), "{text}");
        assert!(
            !text.contains("Install hints"),
            "timeout must not append an install block: {text}"
        );
        assert!(
            !server_missing(Some(note)),
            "server_missing must treat timed-out notes as present"
        );
        assert!(server_missing(Some(
            "language server not installed or not configured for TypeScript"
        )));
    }
}
