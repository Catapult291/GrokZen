//! Static query router: the question type picks the data source, not a retry loop.
//!
//! File-level import edges recall 100% of what a language server reports, so those
//! questions never start a server. Name-matched call edges recall 66% on
//! TypeScript and 18% on Python — for "where is this symbol referenced" that is
//! a silent miss, not a recoverable failure. The graph always returns something;
//! a dynamic "try graph, fall back to LSP" loop cannot tell a complete answer from
//! a ⅔ answer. Routing is therefore a pure function of [`QueryKind`].
//!
//! When a language-server query cannot run (binary missing, startup failure,
//! timeout), the router may still surface graph name matches, but it must label
//! them [`Confidence::Syntactic`] and say which server to install. Presenting
//! those matches as [`Confidence::Exact`] is the failure mode this module exists
//! to prevent.
//!
//! `pool.rs` / `queries.rs` / `diagnostics.rs` are separate workstreams. This
//! module talks to them only through [`LspProvider`] and
//! [`crate::lsp::LanguageServer`]. The graph is already available and is called
//! directly.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::graph::{self, Centrality, CodeGraph};
use crate::types::{
    CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, Language, RelPath, SymbolId,
};

use super::{
    Diagnostic, LanguageServer, Location, LspError, Position, Precise, Range, WorkspaceEdit,
};

/// Where a [`QueryKind`] is sent. Fixed per kind; never rewritten after a lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataSource {
    /// Import graph: `dependents`, `dependencies`, centrality, task rank.
    /// Measured recall against language servers is 100% at file granularity.
    Graph,
    /// Language server: references, definition, diagnostics, rename.
    /// Required once the question is about a symbol's actual bindings.
    LanguageServer,
}

/// The question being asked. The variant *is* the routing key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QueryKind {
    /// Transitive imports of a file (`graph::dependencies`).
    ///
    /// Graph: file-level import edges match language-server include graphs
    /// at 100% recall in the measurements this crate is built on.
    FileDependencies,
    /// Transitive importers of a file (`graph::dependents`).
    ///
    /// Graph: this is the file-level impact question; starting a language
    /// server would only add latency (constraint 3: never block the graph).
    FileImpact,
    /// PageRank over import edges (`graph::compute_centrality`).
    ///
    /// Graph: a whole-repo ranking has no language-server equivalent worth
    /// the resident-set cost, and import centrality is deterministic.
    RepoSkeleton,
    /// Task-personalized ranking (`graph::rank_for_task`).
    ///
    /// Graph: restart-vector personalization is a graph walk, not a
    /// compiler query.
    TaskRank,
    /// Substring / name lookup over indexed symbols.
    ///
    /// Graph: this is a directory of names, not a binding query. Agents that
    /// need actual references must use [`QueryKind::PreciseReferences`].
    FuzzySymbol,
    /// `textDocument/references`.
    ///
    /// Language server: name-matched call edges recall 66% (TypeScript) and
    /// 18% (Python). The graph will not "fail" on the missing third; it will
    /// just omit it. Static route, not a fallback.
    PreciseReferences,
    /// `textDocument/definition`.
    ///
    /// Language server: the same recall gap as references. Same-named
    /// declarations in other packages are the typical false positive.
    GotoDefinition,
    /// `textDocument/diagnostic` (pull) or last published push.
    ///
    /// Language server: the graph has no type checker and cannot invent
    /// diagnostics. Missing server → [`Confidence::Unknown`], never an empty
    /// Exact list that looks like "the file is clean".
    Diagnostics,
    /// `textDocument/rename` computed, not applied.
    ///
    /// Language server: applying a name-matched rename is how unrelated
    /// symbols get rewritten. No graph substitute is offered as Exact.
    PrepareRename,
}

impl QueryKind {
    /// Every kind, in the order of the routing table below. Completeness
    /// tests iterate this rather than a hand-written subset.
    pub const ALL: [QueryKind; 9] = [
        QueryKind::FileDependencies,
        QueryKind::FileImpact,
        QueryKind::RepoSkeleton,
        QueryKind::TaskRank,
        QueryKind::FuzzySymbol,
        QueryKind::PreciseReferences,
        QueryKind::GotoDefinition,
        QueryKind::Diagnostics,
        QueryKind::PrepareRename,
    ];

    /// Static routing table. Destination depends only on `self`.
    pub fn source(self) -> DataSource {
        match self {
            QueryKind::FileDependencies
            | QueryKind::FileImpact
            | QueryKind::RepoSkeleton
            | QueryKind::TaskRank
            | QueryKind::FuzzySymbol => DataSource::Graph,
            QueryKind::PreciseReferences
            | QueryKind::GotoDefinition
            | QueryKind::Diagnostics
            | QueryKind::PrepareRename => DataSource::LanguageServer,
        }
    }
}

/// A query plus the arguments the chosen source needs.
#[derive(Clone, Debug)]
pub enum Query {
    FileDependencies {
        target: FileId,
    },
    FileImpact {
        target: FileId,
    },
    RepoSkeleton,
    TaskRank {
        task: String,
    },
    FuzzySymbol {
        query: String,
    },
    PreciseReferences {
        path: RelPath,
        position: Position,
        /// Used to name-match when the language server is absent. When omitted,
        /// the innermost graph symbol covering `position` is used.
        symbol: Option<String>,
    },
    GotoDefinition {
        path: RelPath,
        position: Position,
        symbol: Option<String>,
    },
    Diagnostics {
        path: RelPath,
    },
    PrepareRename {
        path: RelPath,
        position: Position,
        new_name: String,
    },
}

impl Query {
    pub fn kind(&self) -> QueryKind {
        match self {
            Query::FileDependencies { .. } => QueryKind::FileDependencies,
            Query::FileImpact { .. } => QueryKind::FileImpact,
            Query::RepoSkeleton => QueryKind::RepoSkeleton,
            Query::TaskRank { .. } => QueryKind::TaskRank,
            Query::FuzzySymbol { .. } => QueryKind::FuzzySymbol,
            Query::PreciseReferences { .. } => QueryKind::PreciseReferences,
            Query::GotoDefinition { .. } => QueryKind::GotoDefinition,
            Query::Diagnostics { .. } => QueryKind::Diagnostics,
            Query::PrepareRename { .. } => QueryKind::PrepareRename,
        }
    }
}

/// Payload of a routed answer. The surrounding [`Routed`] carries confidence.
#[derive(Clone, Debug)]
pub enum Answer {
    Files(Vec<FileId>),
    RankedFiles(Vec<(FileId, f64)>),
    Symbols(Vec<SymbolId>),
    Locations(Vec<Location>),
    Diagnostics(Vec<Diagnostic>),
    Edits(WorkspaceEdit),
}

/// One routed answer. `planned` is the static table; `used` is where the
/// bytes actually came from (Graph after a language-server miss).
#[derive(Clone, Debug)]
pub struct Routed {
    pub kind: QueryKind,
    pub planned: DataSource,
    pub used: DataSource,
    pub value: Answer,
    pub confidence: Confidence,
    /// Set whenever `confidence` is below [`Confidence::Exact`].
    pub note: Option<String>,
}

impl Routed {
    pub fn precise(self) -> Precise<Answer> {
        Precise {
            value: self.value,
            confidence: self.confidence,
            note: self.note,
        }
    }
}

/// Injected language-server access. `LspPool` is a sibling workstream; tests
/// and this router share this trait instead of a concrete type.
pub trait LspProvider: Send + Sync {
    /// First use of a language may start a server. Must not be called for
    /// graph-routed kinds — that would violate constraint 3.
    ///
    /// Returns an owned handle rather than a borrow. A pool has to know a
    /// server is in use before it may reclaim it, and a bare `&dyn` cannot
    /// carry that: the borrow ends at the call site, so idle-timeout or
    /// memory-pressure reclamation could stop the process mid-request.
    /// `LspPool::acquire` hands back a guard that decrements its in-flight
    /// count on drop.
    fn acquire(&self, language: Language) -> Result<Box<dyn LanguageServer>, LspError>;

    /// What the agent should tell the user to install. Surfaced verbatim in
    /// degradation notes so "not found" is an actionable state.
    fn install_hint(&self, language: Language) -> String {
        install_hint_for(language).to_string()
    }
}

/// Fallback install text used when no provider is wired (and as the default
/// trait method). Names match the binaries discovery is expected to probe.
pub fn install_hint_for(language: Language) -> &'static str {
    match language {
        Language::Python => "pyright (`npm i -g pyright` or set ASTROLABE_LSP_PYTHON)",
        Language::Go => "gopls (`go install golang.org/x/tools/gopls@latest`)",
        Language::Java => {
            "jdtls (Eclipse JDT Language Server); set ASTROLABE_LSP_JAVA if installed"
        }
        Language::Rust => "rust-analyzer (`rustup component add rust-analyzer`)",
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            "typescript-language-server (`npm i -g typescript-language-server typescript`)"
        }
        Language::Swift => {
            "sourcekit-lsp (`xcode-select --install` / Xcode; set ASTROLABE_LSP_SWIFT)"
        }
        Language::ObjC | Language::ObjCpp => {
            "clangd (preferred; `brew install llvm` / `apt-get install clangd`) or sourcekit-lsp (`xcode-select --install`)"
        }
        Language::C | Language::Cpp => "clangd (`brew install llvm` or set ASTROLABE_LSP_C)",
        Language::Php => "intelephense (`npm i -g intelephense` or set ASTROLABE_LSP_PHP)",
        Language::Vue => {
            "vue-language-server (`npm i -g @vue/language-server` or set ASTROLABE_LSP_VUE)"
        }
        Language::Dart => {
            "dart language-server (ships with the Dart SDK; set ASTROLABE_LSP_DART to override path)"
        }
        Language::CSharp => {
            "Microsoft.CodeAnalysis.LanguageServer via `dotnet <dll> --stdio` (.NET 10 runtime; set ASTROLABE_LSP_CSHARP)"
        }
        Language::VisualBasic => {
            "Microsoft.CodeAnalysis.LanguageServer via `dotnet <dll> --stdio` (.NET 10 runtime; set ASTROLABE_LSP_CSHARP; same server as C#)"
        }
        Language::Kotlin => {
            "kotlin-lsp (JetBrains) or kotlin-language-server (fwcd); set ASTROLABE_LSP_KOTLIN"
        }
        Language::Shell => "bash-language-server (`npm i -g bash-language-server`)",
        Language::Ruby => "ruby-lsp (`gem install ruby-lsp`) or solargraph",
        Language::Lua => "lua-language-server (`brew install lua-language-server`)",
    }
}

/// Primary binary name mentioned in degradation notes so an agent can grep
/// for what to install without parsing a full hint.
pub fn primary_server_name(language: Language) -> &'static str {
    match language {
        Language::Python => "pyright",
        Language::Go => "gopls",
        Language::Java => "jdtls",
        Language::Rust => "rust-analyzer",
        Language::TypeScript | Language::Tsx | Language::JavaScript => "typescript-language-server",
        Language::Swift => "sourcekit-lsp",
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp => "clangd",
        Language::Php => "intelephense",
        Language::Vue => "vue-language-server",
        Language::Dart => "dart language-server",
        Language::CSharp | Language::VisualBasic => "Microsoft.CodeAnalysis.LanguageServer",
        Language::Kotlin => "kotlin-lsp",
        Language::Shell => "bash-language-server",
        Language::Ruby => "ruby-lsp",
        Language::Lua => "lua-language-server",
    }
}

/// Confidence is [`Ord`] with Exact < Scoped < Syntactic < Unknown: a larger
/// value is *less* trusted. Never report a more trusted label than the source.
pub fn never_raise(source: Confidence, claimed: Confidence) -> Confidence {
    source.max(claimed)
}

/// Routes queries against one graph and an optional language-server pool.
pub struct Router<'a> {
    graph: &'a CodeGraph,
    lsp: Option<&'a dyn LspProvider>,
}

impl<'a> Router<'a> {
    pub fn new(graph: &'a CodeGraph, lsp: Option<&'a dyn LspProvider>) -> Self {
        Self { graph, lsp }
    }

    pub fn route(&self, query: &Query) -> Routed {
        let kind = query.kind();
        match query {
            Query::FileDependencies { target } => self.file_walk(kind, *target, false),
            Query::FileImpact { target } => self.file_walk(kind, *target, true),
            Query::RepoSkeleton => {
                let ranked = files_by_centrality(self.graph, &graph::cached_centrality(self.graph));
                self.graph_answer(
                    kind,
                    import_confidence(self.graph),
                    Answer::RankedFiles(ranked),
                    Some(scoped_note()),
                )
            }
            Query::TaskRank { task } => {
                let base = graph::cached_centrality(self.graph);
                let ranked =
                    files_by_centrality(self.graph, &graph::rank_for_task(self.graph, task, &base));
                self.graph_answer(
                    kind,
                    import_confidence(self.graph),
                    Answer::RankedFiles(ranked),
                    Some(scoped_note()),
                )
            }
            Query::FuzzySymbol { query: needle } => {
                let ids = fuzzy_symbols(self.graph, needle);
                self.graph_answer(
                    kind,
                    Confidence::Syntactic,
                    Answer::Symbols(ids),
                    Some(
                        "Fuzzy symbol lookup is name matching against the graph index, \
                         not a language-server search. Homonyms and missed aliases are expected."
                            .into(),
                    ),
                )
            }
            Query::PreciseReferences {
                path,
                position,
                symbol,
            } => self.lsp_locations(kind, path, *position, symbol.as_deref(), LspOp::References),
            Query::GotoDefinition {
                path,
                position,
                symbol,
            } => self.lsp_locations(kind, path, *position, symbol.as_deref(), LspOp::Definition),
            Query::Diagnostics { path } => self.lsp_diagnostics(path),
            Query::PrepareRename {
                path,
                position,
                new_name,
            } => self.lsp_rename(path, *position, new_name),
        }
    }

    fn file_walk(&self, kind: QueryKind, target: FileId, reverse: bool) -> Routed {
        let ids = if reverse {
            graph::dependents(self.graph, target)
        } else {
            graph::dependencies(self.graph, target)
        };
        self.graph_answer(
            kind,
            import_confidence(self.graph),
            Answer::Files(ids),
            Some(scoped_note()),
        )
    }

    fn graph_answer(
        &self,
        kind: QueryKind,
        source: Confidence,
        value: Answer,
        note: Option<String>,
    ) -> Routed {
        finish(kind, DataSource::Graph, source, source, value, note)
    }

    fn lsp_locations(
        &self,
        kind: QueryKind,
        path: &RelPath,
        position: Position,
        symbol: Option<&str>,
        op: LspOp,
    ) -> Routed {
        let language = language_of(self.graph, path);
        match self.with_server(language, |server| match op {
            LspOp::References => server.references(path, position),
            LspOp::Definition => server.definition(path, position),
        }) {
            Ok(mut locs) => {
                // The transport keeps outside-the-workspace results as raw
                // `file:` URIs; they are not repo paths, so report them
                // instead of returning them (or silently nothing).
                let before = locs.len();
                locs.retain(|loc| {
                    !loc.path.as_str()[..loc.path.as_str().len().min(5)]
                        .eq_ignore_ascii_case("file:")
                });
                let external = before - locs.len();
                sort_locations(&mut locs);
                if external > 0 {
                    let note = format!(
                        "{external} 条结果在仓库外（标准库或依赖），已省略；{} result(s) sit outside the repository",
                        external
                    );
                    return finish(
                        kind,
                        DataSource::LanguageServer,
                        Confidence::Scoped,
                        Confidence::Scoped,
                        Answer::Locations(locs),
                        Some(note),
                    );
                }
                finish(
                    kind,
                    DataSource::LanguageServer,
                    Confidence::Exact,
                    Confidence::Exact,
                    Answer::Locations(locs),
                    None,
                )
            }
            Err(err) => {
                let name = resolved_name(self.graph, path, position, symbol);
                let mut locs = name_matched_locations(self.graph, &name, op);
                sort_locations(&mut locs);
                let action = match op {
                    LspOp::References => "Precise references",
                    LspOp::Definition => "Go-to-definition",
                };
                finish(
                    kind,
                    DataSource::Graph,
                    Confidence::Syntactic,
                    Confidence::Syntactic,
                    Answer::Locations(locs),
                    Some(degrade_note(action, language, &err, &self.hint(language))),
                )
            }
        }
    }

    fn lsp_diagnostics(&self, path: &RelPath) -> Routed {
        let kind = QueryKind::Diagnostics;
        let language = language_of(self.graph, path);
        match self.with_server(language, |server| server.diagnostics(path)) {
            Ok(mut diags) => {
                sort_diagnostics(&mut diags);
                finish(
                    kind,
                    DataSource::LanguageServer,
                    Confidence::Exact,
                    Confidence::Exact,
                    Answer::Diagnostics(diags),
                    None,
                )
            }
            Err(err) => {
                // There is no syntactic diagnostic. An empty Exact list would
                // look like "the file is clean"; Unknown is the honest label.
                finish(
                    kind,
                    DataSource::Graph,
                    Confidence::Unknown,
                    Confidence::Unknown,
                    Answer::Diagnostics(Vec::new()),
                    Some(unavailable_note(
                        "Diagnostics",
                        language,
                        &err,
                        &self.hint(language),
                    )),
                )
            }
        }
    }

    fn lsp_rename(&self, path: &RelPath, position: Position, new_name: &str) -> Routed {
        let kind = QueryKind::PrepareRename;
        let language = language_of(self.graph, path);
        match self.with_server(language, |server| {
            server.prepare_rename(path, position, new_name)
        }) {
            Ok(edits) => finish(
                kind,
                DataSource::LanguageServer,
                Confidence::Exact,
                Confidence::Exact,
                Answer::Edits(edits),
                None,
            ),
            Err(err) => finish(
                kind,
                DataSource::Graph,
                Confidence::Unknown,
                Confidence::Unknown,
                Answer::Edits(WorkspaceEdit::default()),
                Some(unavailable_note(
                    "Rename",
                    language,
                    &err,
                    &self.hint(language),
                )),
            ),
        }
    }

    fn with_server<T>(
        &self,
        language: Language,
        op: impl FnOnce(&dyn LanguageServer) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        let Some(provider) = self.lsp else {
            return Err(LspError::Unavailable(language, self.hint(language)));
        };
        // The guard stays alive for the whole call and drops afterwards, which
        // is what tells the pool the request is over.
        let server = provider.acquire(language)?;
        op(server.as_ref())
    }

    fn hint(&self, language: Language) -> String {
        self.lsp
            .map(|p| p.install_hint(language))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| install_hint_for(language).to_string())
    }
}

#[derive(Clone, Copy)]
enum LspOp {
    References,
    Definition,
}

fn finish(
    kind: QueryKind,
    used: DataSource,
    source: Confidence,
    claimed: Confidence,
    value: Answer,
    note: Option<String>,
) -> Routed {
    let planned = kind.source();
    let confidence = never_raise(source, claimed);
    debug_assert!(
        confidence >= source,
        "confidence {confidence:?} raised above source {source:?}"
    );
    debug_assert!(
        !(used == DataSource::Graph
            && planned == DataSource::LanguageServer
            && confidence == Confidence::Exact),
        "degraded graph answers must not be labelled Exact"
    );
    let note = if confidence == Confidence::Exact {
        None
    } else {
        Some(note.unwrap_or_else(|| fallback_note(confidence)))
    };
    Routed {
        kind,
        planned,
        used,
        value,
        confidence,
        note,
    }
}

fn scoped_note() -> String {
    "File-level import graph (scoped): backed by build-config resolution, not a compiler. \
     Measured recall against language servers is 100% at this granularity."
        .into()
}

fn fallback_note(confidence: Confidence) -> String {
    match confidence {
        Confidence::Exact => String::new(),
        Confidence::Scoped => scoped_note(),
        Confidence::Syntactic => {
            "Syntactic name match; may include false positives and misses.".into()
        }
        Confidence::Unknown => "Unable to determine this; the data source was unavailable.".into(),
    }
}

fn degrade_note(action: &str, language: Language, err: &LspError, hint: &str) -> String {
    format!(
        "{action} require {server}, which is currently unavailable ({err}). \
         The following results are name-matched from the graph and may include \
         false positives and misses. Install: {hint}",
        server = primary_server_name(language),
    )
}

fn unavailable_note(action: &str, language: Language, err: &LspError, hint: &str) -> String {
    format!(
        "{action} require {server}, which is currently unavailable ({err}). \
         The graph has no substitute for this query. Install: {hint}",
        server = primary_server_name(language),
    )
}

fn language_of(graph: &CodeGraph, path: &RelPath) -> Language {
    graph
        .files
        .iter()
        .find(|f| f.path == *path)
        .and_then(|f| f.language)
        .or_else(|| Language::from_path(path))
        .unwrap_or(Language::Go)
}

fn import_confidence(graph: &CodeGraph) -> Confidence {
    graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import)
        .map(|e| e.confidence)
        .max()
        .unwrap_or(Confidence::Scoped)
}

fn files_by_centrality(graph: &CodeGraph, centrality: &Centrality) -> Vec<(FileId, f64)> {
    let paths: BTreeMap<FileId, &RelPath> = graph.files.iter().map(|f| (f.id, &f.path)).collect();
    let mut ranked: Vec<(FileId, f64)> = graph
        .files
        .iter()
        .map(|f| (f.id, centrality.by_file.get(&f.id).copied().unwrap_or(0.0)))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| paths.get(&a.0).cmp(&paths.get(&b.0)))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

fn fuzzy_symbols(graph: &CodeGraph, query: &str) -> Vec<SymbolId> {
    if query.is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut hits: Vec<&CodeSymbol> = graph
        .symbols
        .iter()
        .filter(|s| s.name.to_lowercase().contains(&needle))
        .collect();
    hits.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.into_iter().map(|s| s.id).collect()
}

fn resolved_name(
    graph: &CodeGraph,
    path: &RelPath,
    position: Position,
    symbol: Option<&str>,
) -> String {
    if let Some(name) = symbol {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    symbol_at(graph, path, position)
        .map(|s| s.name.clone())
        .unwrap_or_default()
}

fn symbol_at<'g>(
    graph: &'g CodeGraph,
    path: &RelPath,
    position: Position,
) -> Option<&'g CodeSymbol> {
    let file = graph.files.iter().find(|f| f.path == *path)?;
    let line = position.line.saturating_add(1);
    graph
        .symbols
        .iter()
        .filter(|s| s.file == file.id && s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| (s.end_line.saturating_sub(s.start_line), s.id.0))
}

fn name_matched_locations(graph: &CodeGraph, name: &str, op: LspOp) -> Vec<Location> {
    if name.is_empty() {
        return Vec::new();
    }
    let files: BTreeMap<FileId, &CodeFile> = graph.files.iter().map(|f| (f.id, f)).collect();
    let needle = crate::lsp::csharp::strip_roslyn_symbol_name(name);
    let named: Vec<&CodeSymbol> = graph
        .symbols
        .iter()
        .filter(|s| {
            s.name == name
                || s.name == needle
                || crate::lsp::csharp::strip_roslyn_symbol_name(&s.name) == needle
        })
        .collect();
    let mut ids: BTreeSet<SymbolId> = named.iter().map(|s| s.id).collect();
    if matches!(op, LspOp::References) {
        for e in graph.edges.iter().filter(|e| e.kind == EdgeKind::Call) {
            if named.iter().any(|s| s.id.0 == e.to) {
                ids.insert(SymbolId(e.from));
            }
        }
    }
    let mut locs = Vec::new();
    for s in &graph.symbols {
        if ids.contains(&s.id) {
            if let Some(file) = files.get(&s.file) {
                locs.push(location_of(file, s));
            }
        }
    }
    if matches!(op, LspOp::Definition) {
        // Prefer declarations in the queried set; already name-filtered.
        locs.sort();
        locs.dedup();
    }
    locs
}

fn location_of(file: &CodeFile, sym: &CodeSymbol) -> Location {
    Location {
        path: file.path.clone(),
        range: Range {
            start: Position {
                line: sym.start_line.saturating_sub(1),
                character: 0,
            },
            end: Position {
                line: sym.end_line.saturating_sub(1),
                character: 0,
            },
        },
    }
}

fn sort_locations(locs: &mut Vec<Location>) {
    locs.sort();
    locs.dedup();
}

fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.range.cmp(&b.range))
            .then_with(|| a.severity.cmp(&b.severity))
            .then_with(|| a.message.cmp(&b.message))
            .then_with(|| a.code.cmp(&b.code))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{Severity, TextEdit};
    use crate::types::{CodeEdge, CodeFile, SymbolKind};
    use std::sync::{Arc, Mutex};

    fn file(id: u32, path: &str) -> CodeFile {
        CodeFile {
            id: FileId(id),
            path: RelPath::new(path),
            language: Some(Language::Go),
            loc: 10,
            sha: String::new(),
        }
    }

    fn import(from: u32, to: u32, confidence: Confidence) -> CodeEdge {
        CodeEdge {
            from,
            to,
            kind: EdgeKind::Import,
            weight: 1,
            confidence,
        }
    }

    fn call(from: u32, to: u32) -> CodeEdge {
        CodeEdge {
            from,
            to,
            kind: EdgeKind::Call,
            weight: 1,
            confidence: Confidence::Syntactic,
        }
    }

    fn symbol(id: u32, file: u32, name: &str, line: u32) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(id),
            file: FileId(file),
            name: name.into(),
            kind: SymbolKind::Function,
            signature: format!("func {name}()"),
            start_line: line,
            end_line: line,
            exported: true,
        }
    }

    fn loc(path: &str, line: u32) -> Location {
        Location {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
        }
    }

    /// a.go → b.go → c.go, with Foo in a.go calling Bar in b.go.
    fn sample_graph() -> CodeGraph {
        CodeGraph {
            files: vec![file(0, "a.go"), file(1, "b.go"), file(2, "c.go")],
            symbols: vec![
                symbol(10, 0, "Foo", 1),
                symbol(11, 1, "Bar", 3),
                symbol(12, 2, "Baz", 5),
            ],
            edges: vec![
                import(0, 1, Confidence::Scoped),
                import(1, 2, Confidence::Scoped),
                call(10, 11),
            ],
        }
    }

    struct RecordingProvider {
        calls: Mutex<Vec<String>>,
        server: Option<FakeServer>,
        hint: String,
    }

    // `LspProvider` yields an owned handle so a real pool can track in-flight
    // requests. The fake mirrors that by cloning; the shared `Arc` call log
    // keeps assertions working across clones.

    #[derive(Clone)]
    struct FakeServer {
        language: Language,
        references: Vec<Location>,
        definition: Vec<Location>,
        diagnostics: Vec<Diagnostic>,
        rename: WorkspaceEdit,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl LanguageServer for FakeServer {
        fn language(&self) -> Language {
            self.language
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
            self.calls
                .lock()
                .expect("call log poisoned")
                .push("references".into());
            Ok(self.references.clone())
        }

        fn definition(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<Vec<Location>, LspError> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push("definition".into());
            Ok(self.definition.clone())
        }

        fn diagnostics(&self, _path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push("diagnostics".into());
            Ok(self.diagnostics.clone())
        }

        fn prepare_rename(
            &self,
            _path: &RelPath,
            _position: Position,
            _new_name: &str,
        ) -> Result<WorkspaceEdit, LspError> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push("prepare_rename".into());
            Ok(self.rename.clone())
        }

        fn memory_bytes(&self) -> Option<u64> {
            None
        }

        fn shutdown(&self) -> Result<(), LspError> {
            Ok(())
        }
    }

    impl RecordingProvider {
        fn available(server: FakeServer) -> Self {
            Self {
                hint: install_hint_for(server.language).to_string(),
                server: Some(server),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn missing(language: Language) -> Self {
            Self {
                hint: install_hint_for(language).to_string(),
                server: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn acquires(&self) -> Vec<String> {
            self.calls.lock().expect("call log poisoned").clone()
        }

        fn server_calls(&self) -> Vec<String> {
            self.server
                .as_ref()
                .map(|s| s.calls.lock().expect("call log poisoned").clone())
                .unwrap_or_default()
        }
    }

    impl LspProvider for RecordingProvider {
        fn acquire(&self, language: Language) -> Result<Box<dyn LanguageServer>, LspError> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push(format!("acquire:{}", language.name()));
            match &self.server {
                Some(server) => Ok(Box::new(server.clone())),
                None => Err(LspError::Unavailable(language, self.hint.clone())),
            }
        }

        fn install_hint(&self, _language: Language) -> String {
            self.hint.clone()
        }
    }

    fn pos() -> Position {
        Position {
            line: 0,
            character: 0,
        }
    }

    fn refs_query() -> Query {
        Query::PreciseReferences {
            path: RelPath::new("a.go"),
            position: pos(),
            symbol: Some("Foo".into()),
        }
    }

    #[test]
    fn routing_table_covers_every_kind_and_is_static() {
        let mut lsp_kinds = 0;
        let mut graph_kinds = 0;
        for kind in QueryKind::ALL {
            match kind.source() {
                DataSource::Graph => graph_kinds += 1,
                DataSource::LanguageServer => lsp_kinds += 1,
            }
        }
        assert_eq!(QueryKind::ALL.len(), 9);
        assert_eq!(graph_kinds, 5);
        assert_eq!(lsp_kinds, 4);
        // Destination does not depend on whether a server is present.
        assert_eq!(
            QueryKind::PreciseReferences.source(),
            DataSource::LanguageServer
        );
        assert_eq!(QueryKind::FileImpact.source(), DataSource::Graph);
    }

    #[test]
    fn file_level_queries_do_not_touch_lsp() {
        let g = sample_graph();
        let provider = RecordingProvider::available(FakeServer {
            language: Language::Go,
            references: vec![loc("hit.go", 0)],
            definition: vec![loc("hit.go", 0)],
            diagnostics: Vec::new(),
            rename: WorkspaceEdit::default(),
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let router = Router::new(&g, Some(&provider));

        for q in [
            Query::FileDependencies { target: FileId(0) },
            Query::FileImpact { target: FileId(2) },
            Query::RepoSkeleton,
            Query::TaskRank {
                task: "fix Foo in a.go".into(),
            },
            Query::FuzzySymbol {
                query: "Foo".into(),
            },
        ] {
            let routed = router.route(&q);
            assert_eq!(routed.planned, DataSource::Graph, "{:?}", q.kind());
            assert_eq!(routed.used, DataSource::Graph, "{:?}", q.kind());
        }
        assert!(
            provider.acquires().is_empty(),
            "graph-routed kinds started a language server: {:?}",
            provider.acquires()
        );
        assert!(
            provider.server_calls().is_empty(),
            "graph-routed kinds invoked LanguageServer methods: {:?}",
            provider.server_calls()
        );
    }

    #[test]
    fn symbol_level_queries_call_lsp() {
        let g = sample_graph();
        let provider = RecordingProvider::available(FakeServer {
            language: Language::Go,
            references: vec![loc("a.go", 0), loc("b.go", 2)],
            definition: vec![loc("a.go", 0)],
            diagnostics: vec![Diagnostic {
                path: RelPath::new("a.go"),
                range: Range {
                    start: pos(),
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: Severity::Error,
                message: "unused".into(),
                code: None,
            }],
            rename: WorkspaceEdit {
                edits: vec![(
                    RelPath::new("a.go"),
                    vec![TextEdit {
                        range: Range {
                            start: pos(),
                            end: Position {
                                line: 0,
                                character: 3,
                            },
                        },
                        new_text: "Quux".into(),
                    }],
                )],
            },
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let router = Router::new(&g, Some(&provider));

        let refs = router.route(&refs_query());
        assert_eq!(refs.planned, DataSource::LanguageServer);
        assert_eq!(refs.used, DataSource::LanguageServer);
        assert_eq!(refs.confidence, Confidence::Exact);
        assert!(refs.note.is_none());

        let def = router.route(&Query::GotoDefinition {
            path: RelPath::new("a.go"),
            position: pos(),
            symbol: Some("Foo".into()),
        });
        assert_eq!(def.used, DataSource::LanguageServer);
        assert_eq!(def.confidence, Confidence::Exact);

        let diags = router.route(&Query::Diagnostics {
            path: RelPath::new("a.go"),
        });
        assert_eq!(diags.used, DataSource::LanguageServer);
        assert_eq!(diags.confidence, Confidence::Exact);

        let rename = router.route(&Query::PrepareRename {
            path: RelPath::new("a.go"),
            position: pos(),
            new_name: "Quux".into(),
        });
        assert_eq!(rename.used, DataSource::LanguageServer);
        assert_eq!(rename.confidence, Confidence::Exact);

        let acquires = provider.acquires();
        assert_eq!(acquires.len(), 4);
        assert!(acquires.iter().all(|c| c == "acquire:go"));
        let methods = provider.server_calls();
        assert_eq!(
            methods,
            vec!["references", "definition", "diagnostics", "prepare_rename"]
        );
    }

    #[test]
    fn unavailable_lsp_degrades_references_to_syntactic_graph() {
        let g = sample_graph();
        let provider = RecordingProvider::missing(Language::Go);
        let router = Router::new(&g, Some(&provider));
        let routed = router.route(&refs_query());

        assert_eq!(routed.planned, DataSource::LanguageServer);
        assert_eq!(routed.used, DataSource::Graph);
        assert_eq!(routed.confidence, Confidence::Syntactic);
        assert_ne!(routed.confidence, Confidence::Exact);
        match &routed.value {
            Answer::Locations(locs) => {
                // Name match still finds Foo (and Bar via the call edge).
                assert!(
                    !locs.is_empty(),
                    "degradation must surface graph matches, not an empty Exact list"
                );
                assert!(locs.iter().any(|l| l.path.as_str() == "a.go"));
            }
            other => panic!("expected Locations, got {other:?}"),
        }
        assert_eq!(provider.acquires(), vec!["acquire:go".to_string()]);
    }

    #[test]
    fn degrade_note_is_nonempty_and_names_the_server() {
        let g = sample_graph();
        let provider = RecordingProvider::missing(Language::Go);
        let router = Router::new(&g, Some(&provider));
        let routed = router.route(&refs_query());
        let note = routed
            .note
            .expect("degraded answers must explain themselves");
        assert!(!note.is_empty());
        assert!(
            note.contains("gopls"),
            "note must say what to install, got {note:?}"
        );
        assert!(
            note.to_lowercase().contains("install")
                || note.contains("go install")
                || note.contains("unavail"),
            "note must be actionable, got {note:?}"
        );
    }

    #[test]
    fn missing_provider_also_degrades_and_mentions_gopls() {
        let g = sample_graph();
        let router = Router::new(&g, None);
        let routed = router.route(&refs_query());
        assert_eq!(routed.confidence, Confidence::Syntactic);
        assert_eq!(routed.used, DataSource::Graph);
        let note = routed.note.expect("note");
        assert!(note.contains("gopls"), "{note}");
    }

    #[test]
    fn file_level_scoped_is_not_promoted_to_exact() {
        let g = sample_graph();
        let provider = RecordingProvider::available(FakeServer {
            language: Language::Go,
            references: Vec::new(),
            definition: Vec::new(),
            diagnostics: Vec::new(),
            rename: WorkspaceEdit::default(),
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let router = Router::new(&g, Some(&provider));
        let impact = router.route(&Query::FileImpact { target: FileId(2) });
        // 100% recall of import edges is still not a compiler. Exact would be a raise.
        assert_eq!(impact.confidence, Confidence::Scoped);
        assert_ne!(impact.confidence, Confidence::Exact);
        match &impact.value {
            Answer::Files(ids) => {
                assert!(ids.contains(&FileId(0)));
                assert!(ids.contains(&FileId(1)));
            }
            other => panic!("expected Files, got {other:?}"),
        }
        assert!(provider.acquires().is_empty());
    }

    #[test]
    fn syntactic_import_edges_are_not_promoted() {
        let mut g = sample_graph();
        for e in &mut g.edges {
            if e.kind == EdgeKind::Import {
                e.confidence = Confidence::Syntactic;
            }
        }
        let router = Router::new(&g, None);
        let deps = router.route(&Query::FileDependencies { target: FileId(0) });
        assert_eq!(deps.confidence, Confidence::Syntactic);
        assert_ne!(deps.confidence, Confidence::Scoped);
        assert_ne!(deps.confidence, Confidence::Exact);
    }

    #[test]
    fn never_raise_caps_claimed_above_source() {
        assert_eq!(
            never_raise(Confidence::Syntactic, Confidence::Exact),
            Confidence::Syntactic
        );
        assert_eq!(
            never_raise(Confidence::Scoped, Confidence::Exact),
            Confidence::Scoped
        );
        assert_eq!(
            never_raise(Confidence::Unknown, Confidence::Syntactic),
            Confidence::Unknown
        );
        assert_eq!(
            never_raise(Confidence::Exact, Confidence::Exact),
            Confidence::Exact
        );
        assert_eq!(
            never_raise(Confidence::Exact, Confidence::Syntactic),
            Confidence::Syntactic
        );
    }

    #[test]
    fn lsp_empty_result_is_not_backfilled_from_graph() {
        let g = sample_graph();
        let provider = RecordingProvider::available(FakeServer {
            language: Language::Go,
            references: Vec::new(),
            definition: Vec::new(),
            diagnostics: Vec::new(),
            rename: WorkspaceEdit::default(),
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let router = Router::new(&g, Some(&provider));
        let routed = router.route(&refs_query());
        assert_eq!(routed.confidence, Confidence::Exact);
        assert_eq!(routed.used, DataSource::LanguageServer);
        match &routed.value {
            Answer::Locations(locs) => assert!(
                locs.is_empty(),
                "an Exact empty list means 'no references', not 'fill from the graph'"
            ),
            other => panic!("expected Locations, got {other:?}"),
        }
    }

    #[test]
    fn definition_degrades_to_syntactic_name_match() {
        let g = sample_graph();
        let provider = RecordingProvider::missing(Language::Go);
        let router = Router::new(&g, Some(&provider));
        let routed = router.route(&Query::GotoDefinition {
            path: RelPath::new("b.go"),
            position: Position {
                line: 2,
                character: 0,
            },
            symbol: Some("Bar".into()),
        });
        assert_eq!(routed.confidence, Confidence::Syntactic);
        assert_eq!(routed.used, DataSource::Graph);
        match &routed.value {
            Answer::Locations(locs) => {
                assert!(locs.iter().any(|l| l.path.as_str() == "b.go"));
            }
            other => panic!("expected Locations, got {other:?}"),
        }
        let note = routed.note.expect("note");
        assert!(note.contains("gopls"), "{note}");
    }

    #[test]
    fn diagnostics_without_lsp_are_unknown_not_clean() {
        let g = sample_graph();
        let provider = RecordingProvider::missing(Language::Go);
        let router = Router::new(&g, Some(&provider));
        let routed = router.route(&Query::Diagnostics {
            path: RelPath::new("a.go"),
        });
        assert_eq!(routed.planned, DataSource::LanguageServer);
        assert_eq!(routed.confidence, Confidence::Unknown);
        assert_ne!(routed.confidence, Confidence::Exact);
        match &routed.value {
            Answer::Diagnostics(d) => assert!(d.is_empty()),
            other => panic!("expected Diagnostics, got {other:?}"),
        }
        let note = routed.note.expect("note");
        assert!(note.contains("gopls"), "{note}");
    }

    #[test]
    fn file_impact_matches_graph_dependents() {
        let g = sample_graph();
        let router = Router::new(&g, None);
        let routed = router.route(&Query::FileImpact { target: FileId(2) });
        match routed.value {
            Answer::Files(ids) => assert_eq!(ids, graph::dependents(&g, FileId(2))),
            other => panic!("expected Files, got {other:?}"),
        }
    }

    #[test]
    fn precise_wraps_without_raising() {
        let g = sample_graph();
        let routed = Router::new(&g, None).route(&refs_query());
        let precise = routed.precise();
        assert_eq!(precise.confidence, Confidence::Syntactic);
        assert!(precise.note.is_some());
    }
}
