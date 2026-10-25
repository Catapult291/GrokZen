//! Real backend for the precise tools: a pooled language server plus the
//! scope-aware precise queries.
//!
//! Everything here is on-demand. No server starts until a precise tool is
//! actually called, and the pool reclaims it once idle or over its memory
//! budget — the whole point of the split is that a language server is a
//! short-lived resource, not a resident one.
//!
//! When a server is missing this layer still answers: the error carries the
//! install hint discovery produced, and the tool renders it as
//! `confidence: unknown` with the command to run. An empty result would read
//! as "there is nothing here", which is a different and wrong claim.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use astrolabe_core::graph::{paired_files, CodeGraph};
use astrolabe_core::lsp::diagnostics::{query_server, FetchOptions};
use astrolabe_core::lsp::discovery::Discovery;
use astrolabe_core::lsp::pool::{LspPool, LspPoolConfig};
use astrolabe_core::lsp::queries::{
    apple_references_fallback_in, find_references, goto_definition, identifier_at,
    text_fallback_references_in, SymbolAt,
};
use astrolabe_core::lsp::router::primary_server_name;
use astrolabe_core::lsp::transport::StdioSpawn;
use astrolabe_core::lsp::Position;
use astrolabe_core::lsp::Range;
use astrolabe_core::lsp::{Diagnostic, LanguageServer, Location, LspError, Precise};
use astrolabe_core::types::{Confidence, EdgeKind, FileId, Language, RelPath};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::index::RepoIndex;
use crate::precise_tools::PreciseCapability;
use crate::refs_graph::{self, PathFix};

/// Current tree-sitter index, when one has been built.
pub(crate) type SyntaxIndex = Arc<dyn Fn() -> Option<Arc<RepoIndex>> + Send + Sync>;

/// Cap on syntactic stand-in answers so a common name cannot flood a reply.
const MAX_SYNTACTIC_DEFINITIONS: usize = 30;
/// Go implementors listed in a `find_references` note.
const GO_IMPLEMENTORS_LISTED: usize = 12;
/// Note returned by the precise tools when this process is index-only: the
/// host's own language-server pool answers the part the index cannot decide.
const HOST_LSP_NOTE: &str =
    "索引无法判定这条精确查询；本进程未启用内置语言服务器，需由宿主的语言服务器作答。";

pub(crate) struct LiveBackend {
    root: PathBuf,
    pool: Arc<LspPool>,
    /// Syntax index used to answer while a server is still importing or
    /// indexing (kotlin-lsp's first Gradle import runs for many minutes).
    syntax: Option<SyntaxIndex>,
    /// Set while a slow-starting server (kotlin-lsp: JVM boot + Gradle
    /// import) is being started on a background thread. Queries meanwhile
    /// get the syntactic fallback instead of blocking on the start.
    starting: Arc<AtomicBool>,
    /// Set when the background start failed (server missing / crashed). Calls
    /// then take the synchronous `acquire` path so the real error (install
    /// hint) reaches the user instead of an endless "still importing".
    start_failed: Arc<AtomicBool>,
    /// The session's search_code file cache; `find_references` reads
    /// through it instead of re-reading each file several times per call.
    file_cache: Option<Arc<astrolabe_core::cache::FileCache>>,
    /// `false`: never start the pool; answer index-only and let the host
    /// run the exact query. See [`HOST_LSP_NOTE`].
    lsp_enabled: bool,
}

impl std::fmt::Debug for LiveBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveBackend")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl LiveBackend {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let pool = LspPool::from_discovery_and_transport(
            Discovery::from_process().with_project_root(&root),
            StdioSpawn::new(&root),
            root.clone(),
            LspPoolConfig::default(),
        );
        LiveBackend {
            root,
            pool: Arc::new(pool),
            syntax: None,
            starting: Arc::new(AtomicBool::new(false)),
            start_failed: Arc::new(AtomicBool::new(false)),
            file_cache: None,
            lsp_enabled: true,
        }
    }

    /// Index-only precise tools: never start the pool. Used by in-process
    /// hosts that already own a language-server manager.
    pub(crate) fn with_lsp_enabled(mut self, enabled: bool) -> Self {
        self.lsp_enabled = enabled;
        self
    }

    /// Serve `find_references` file reads from the session's file cache.
    pub(crate) fn with_file_cache(mut self, cache: Arc<astrolabe_core::cache::FileCache>) -> Self {
        self.file_cache = Some(cache);
        self
    }

    /// Answer from `index` while a language server is still warming up.
    pub(crate) fn with_syntax_index(mut self, index: SyntaxIndex) -> Self {
        self.syntax = Some(index);
        self
    }

    /// Declarations named `name` in the syntax index, same language family,
    /// `origin`'s file first.
    fn syntactic_definitions(
        &self,
        language: Language,
        origin: &RelPath,
        name: &str,
    ) -> Vec<(Location, String)> {
        let Some(index) = self.syntax.as_ref().and_then(|f| f()) else {
            return Vec::new();
        };
        let family = |l: Option<Language>| l.is_some_and(|l| same_family(l, language));
        let near = near_files(index.graph(), origin);
        let mut hits: Vec<(u8, Location, String)> = Vec::new();
        for symbol in &index.graph().symbols {
            if symbol.name != name {
                continue;
            }
            let Some(file) = index.file(symbol.file) else {
                continue;
            };
            if !family(file.language) {
                continue;
            }
            let line = symbol.start_line.saturating_sub(1);
            let at = Position { line, character: 0 };
            // Origin first, then its header/source partner and the files it
            // includes (with their partners), then the rest.
            let rank = if file.path == *origin {
                0
            } else if near.contains(&symbol.file) {
                1
            } else {
                2
            };
            hits.push((
                rank,
                Location {
                    path: file.path.clone(),
                    range: Range { start: at, end: at },
                },
                symbol.signature.clone(),
            ));
        }
        hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        hits.into_iter()
            .take(MAX_SYNTACTIC_DEFINITIONS)
            .map(|(_, loc, sig)| (loc, sig))
            .collect()
    }

    /// C family: a server without a compile database (clangd with no
    /// `compile_commands.json`) only knows the header it parsed, so "go to
    /// definition" on a call lands on the prototype. When every answer is in
    /// a header, add the same-name declarations from that header's paired
    /// source file ([`EdgeKind::Pair`] in the syntax index). Labelled
    /// syntactic: the pairing is by file name, the match by symbol name.
    fn with_paired_definitions(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        result: Precise<Vec<Location>>,
    ) -> Precise<Vec<Location>> {
        if !is_c_family(language)
            || result.value.is_empty()
            || !result.value.iter().all(|l| is_c_header(l.path.as_str()))
        {
            return result;
        }
        let Some(index) = self.syntax.as_ref().and_then(|f| f()) else {
            return result;
        };
        let Some(name) = self.target_name(rel, target) else {
            return result;
        };
        let extra = paired_definitions(index.graph(), &result.value, &name);
        if extra.is_empty() {
            return result;
        }
        let partners: BTreeSet<&str> = extra.iter().map(|l| l.path.as_str()).collect();
        let note = format!(
            "{}语言服务器只给出了头文件里的声明；后面追加的 {} 处是配对源文件 {} 里的同名定义（syntactic：按头文件/源文件同名配对和符号名匹配）。",
            result.note.as_deref().map(|n| format!("{n} ")).unwrap_or_default(),
            extra.len(),
            partners.into_iter().collect::<Vec<_>>().join("、")
        );
        let mut value = result.value;
        value.extend(extra);
        Precise {
            confidence: result.confidence.max(Confidence::Syntactic),
            note: Some(note),
            value,
        }
    }

    /// C family: the server "resolved" a call to the call site itself. clangd
    /// does this for a function it has no declaration of (missing include
    /// path, no compile database): C's implicit declaration makes the call
    /// its own declaration. Not an answer when the syntax index has no
    /// declaration of that name on that line.
    fn unresolved_call(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        found: &[Location],
    ) -> bool {
        let SymbolAt::Position(pos) = target else {
            return false;
        };
        if !is_c_family(language)
            || found.is_empty()
            || !found
                .iter()
                .all(|l| l.path == *rel && l.range.start.line == pos.line)
        {
            return false;
        }
        let Some(index) = self.syntax.as_ref().and_then(|f| f()) else {
            return false;
        };
        let Some(name) = self.target_name(rel, target) else {
            return false;
        };
        let g = index.graph();
        let Some(file) = g.files.iter().find(|f| f.path == *rel).map(|f| f.id) else {
            return false;
        };
        let line = pos.line + 1;
        !g.symbols
            .iter()
            .any(|s| s.file == file && s.name == name && s.start_line <= line && line <= s.end_line)
    }

    /// A Go interface named by the query in `path`: its implementing types
    /// from the syntax index (references never include them; Go has no
    /// `implements` clause to refer to).
    fn go_implementors_line(&self, path: &RelPath, target: SymbolAt<'_>) -> Option<String> {
        let name = self.target_name(path, target)?;
        let index = self.syntax.as_ref().and_then(|f| f())?;
        let graph = index.graph();
        let file = graph.files.iter().find(|f| f.path == *path)?.id;
        let ifaces: std::collections::HashSet<u32> = graph
            .symbols
            .iter()
            .filter(|s| {
                s.file == file && s.name == name && s.kind == astrolabe_core::SymbolKind::Interface
            })
            .map(|s| s.id.0)
            .collect();
        let by = astrolabe_core::go_iface::implementors_of(graph, &ifaces);
        let text: String = by
            .iter()
            .map(|(iface, impls)| {
                astrolabe_core::go_iface::render_implementors(
                    graph,
                    *iface,
                    impls,
                    GO_IMPLEMENTORS_LISTED,
                )
            })
            .collect();
        (!text.is_empty()).then_some(text)
    }

    /// `find_references` through the language server, with the text
    /// fallbacks for a warming / empty server and the Apple tree-sitter
    /// fallback.
    fn server_references(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        index: Option<&RepoIndex>,
    ) -> Precise<Vec<Location>> {
        // The text fallbacks walk the file list of the current (barrier-
        // fresh) index instead of re-scanning the whole tree per query.
        let files = index
            .and_then(RepoIndex::incremental)
            .map(|inc| inc.admitted_paths());
        let result = match self.with_server(language, |server| {
            let result = find_references(server, &self.root, rel, target);
            (result, server.is_none_or(|s| s.is_busy()))
        }) {
            Ok((result, busy)) if busy && warming_unknown(&result.value, &result) => {
                let reason = warming_note(language, 0, "");
                match text_fallback_references_in(&self.root, rel, target, &reason, files) {
                    Some(fallback) if !fallback.value.is_empty() => fallback,
                    _ => result,
                }
            }
            // An idle server that finds no reference at all, while the name
            // occurs elsewhere in the repo, usually does not have the file
            // in its project model (unimported module, KMP / Android source
            // set). That is not an authoritative zero.
            // kotlin-lsp answers a file outside its project model with just
            // the declaration itself, so one hit is as suspect as none there.
            Ok((result, false))
                if result.value.is_empty()
                    || (language == Language::Kotlin && result.value.len() <= 1) =>
            {
                let reason = empty_note(language, "引用");
                match text_fallback_references_in(&self.root, rel, target, &reason, files) {
                    Some(fallback) if fallback.value.len() > result.value.len().max(1) => fallback,
                    _ => result,
                }
            }
            Ok((result, _)) => result,
            // No server: answer from the index with one hint line. Only when
            // the index has nothing either does the install hint stand
            // alone (verbatim, so "not found" stays actionable).
            Err(error) => self
                .references_without_server(language, rel, target, index, &error)
                .unwrap_or_else(|| Precise::unknown(Vec::new(), error.to_string())),
        };
        // Swift / ObjC servers without an index answer nothing; fall back to
        // a clearly labelled tree-sitter search instead of an empty Unknown.
        apple_references_fallback_in(&self.root, rel, target, result, files)
    }

    fn target_name(&self, path: &RelPath, target: SymbolAt<'_>) -> Option<String> {
        match target {
            SymbolAt::Name(name) if !name.is_empty() => Some(name.to_string()),
            SymbolAt::Name(_) => None,
            SymbolAt::Position(pos) => identifier_at(&self.root, path, pos),
        }
    }

    /// Run `op` against a checked-out server. The guard lives for the whole
    /// call, which is what stops the pool reclaiming a server mid-request.
    fn with_server<T>(
        &self,
        language: Language,
        op: impl FnOnce(Option<&dyn LanguageServer>) -> T,
    ) -> Result<T, LspError> {
        if language == Language::Kotlin
            && self.syntax.is_some()
            && self.start_in_background(language)
        {
            // `None` = server still starting; callers treat it as warming
            // and answer from the syntax index.
            return Ok(op(None));
        }
        let guard = self.pool.acquire(language)?;
        self.start_failed.store(false, Ordering::Release);
        Ok(op(Some(&guard as &dyn LanguageServer)))
    }

    /// For servers whose start alone takes seconds (kotlin-lsp boots a JVM
    /// before it even answers `initialize`): start it on a background thread
    /// and report `true` while that is in progress, so the first queries are
    /// not held up by the start plus the first-checkout ready wait.
    fn start_in_background(&self, language: Language) -> bool {
        if self.starting.load(Ordering::Acquire) {
            return true;
        }
        if self.start_failed.load(Ordering::Acquire) {
            return false;
        }
        if self.pool.is_resident(language) {
            return false;
        }
        if self
            .starting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return true;
        }
        let pool = Arc::clone(&self.pool);
        let starting = Arc::clone(&self.starting);
        let start_failed = Arc::clone(&self.start_failed);
        let spawned = std::thread::Builder::new()
            .name(format!("lsp-start-{}", language.name()))
            .spawn(move || {
                if let Err(error) = pool.acquire(language) {
                    tracing::warn!(language = language.name(), %error, "background start failed");
                    start_failed.store(true, Ordering::Release);
                }
                starting.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.starting.store(false, Ordering::Release);
            return false;
        }
        true
    }
}

fn is_c_family(language: Language) -> bool {
    matches!(
        language,
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp
    )
}

fn is_c_header(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, e)| {
        ["h", "hh", "hpp", "hxx", "h++"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

/// Files a definition lookup from `origin` should try before the rest of
/// the repo: `origin`'s header/source partner, the files it includes, and
/// their partners.
fn near_files(g: &CodeGraph, origin: &RelPath) -> HashSet<FileId> {
    let Some(origin) = g.files.iter().find(|f| f.path == *origin).map(|f| f.id) else {
        return HashSet::new();
    };
    let mut near: HashSet<FileId> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import && e.from == origin.0)
        .map(|e| FileId(e.to))
        .collect();
    near.insert(origin);
    let partners: Vec<FileId> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Pair)
        .filter_map(|e| {
            if near.contains(&FileId(e.from)) {
                Some(FileId(e.to))
            } else if near.contains(&FileId(e.to)) {
                Some(FileId(e.from))
            } else {
                None
            }
        })
        .collect();
    near.extend(partners);
    near.remove(&origin);
    near
}

/// Declarations named `name` in the paired source files of the headers in
/// `found`, not already in `found`.
fn paired_definitions(g: &CodeGraph, found: &[Location], name: &str) -> Vec<Location> {
    let mut out: Vec<Location> = Vec::new();
    let headers: Vec<FileId> = g
        .files
        .iter()
        .filter(|f| found.iter().any(|l| l.path == f.path))
        .map(|f| f.id)
        .collect();
    let partners: BTreeSet<FileId> = headers
        .iter()
        .flat_map(|&h| paired_files(g, h))
        .filter(|p| !headers.contains(p))
        .collect();
    if partners.is_empty() {
        return out;
    }
    for symbol in &g.symbols {
        if symbol.name != name || !partners.contains(&symbol.file) {
            continue;
        }
        let Some(file) = g.files.iter().find(|f| f.id == symbol.file) else {
            continue;
        };
        let at = Position {
            line: symbol.start_line.saturating_sub(1),
            character: 0,
        };
        let loc = Location {
            path: file.path.clone(),
            range: Range { start: at, end: at },
        };
        if !found.contains(&loc) && !out.contains(&loc) {
            out.push(loc);
        }
        if out.len() >= MAX_SYNTACTIC_DEFINITIONS {
            break;
        }
    }
    out
}

/// An empty, unknown answer: what a server gives while still importing.
fn warming_unknown<T>(value: &[T], result: &Precise<Vec<T>>) -> bool {
    value.is_empty() && result.confidence == Confidence::Unknown
}

/// Note for answers given from the syntax index while the server warms up.
fn warming_note(language: Language, n: usize, what: &str) -> String {
    let server = astrolabe_core::lsp::router::primary_server_name(language);
    let head = format!(
        "{server} 仍在导入项目 / 建立索引，精确结果稍后可用（首次导入大型 Gradle 项目可能需要十几分钟，之后有缓存）"
    );
    if n == 0 {
        head
    } else {
        format!("{head}。先给出 tree-sitter 语法索引里的{what} {n} 处（syntactic：按名字匹配，可能含同名的其他实体）。")
    }
}

/// Note for an idle server that returned nothing. The project-model
/// examples name the build system of `language`; Kotlin / Android wording
/// only for Kotlin.
fn empty_note(language: Language, what: &str) -> String {
    let server = astrolabe_core::lsp::router::primary_server_name(language);
    let why = match language {
        Language::Kotlin => "该文件可能不在它导入的项目模型里（例如 Kotlin 多平台 / Android 源集未被导入、未安装 Android SDK）",
        Language::Java => "该文件可能不在它导入的 Maven / Gradle 项目里",
        Language::Go => "该文件可能不在 go.mod 模块或当前构建标签里",
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Vue => {
            "该文件可能不在 tsconfig / jsconfig 覆盖的范围里"
        }
        Language::Python => "该文件可能不在 pyright 的分析范围里（解释器 / 虚拟环境未配置）",
        Language::Rust => "该文件可能不在 Cargo 工作区的 crate 里",
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp => {
            "该文件可能没有编译参数（缺 compile_commands.json）"
        }
        Language::CSharp | Language::VisualBasic => "该文件可能不在已加载的 .csproj / .sln 里",
        _ => "该文件可能不在它加载的项目里",
    };
    format!("{server} 没有返回{what}：{why}，所以空结果不作为精确结论。")
}

/// Languages whose declarations can answer for each other (mixed projects).
fn same_family(a: Language, b: Language) -> bool {
    refs_graph::same_family(a, b)
}

/// Note shown when a tool needs a path but was not given one.
fn needs_path(what: &str) -> String {
    format!(
        "无法确定语言：{what} 需要 `path` 来选择语言服务器。\
         请补上仓库相对路径，例如 `crates/core/src/lib.rs`。"
    )
}

/// Note when a path was given but its extension is not mapped to a language.
fn unknown_language(what: &str, path: &str) -> String {
    format!(
        "无法确定语言：路径 `{path}` 未映射到已知语言，{what} 无法选择语言服务器。\
         请改用带已知扩展名的源文件（如 `.rs` / `.ts` / `.py`）。"
    )
}

/// Language whose server should answer for `path`: like
/// [`Language::from_path`], except a `.h` header follows its content
/// ([`Language::from_path_and_source`]), so an Objective-C header goes to the
/// ObjC server (clangd in ObjC mode) instead of the C one.
fn server_language(root: &Path, path: &str) -> Option<Language> {
    let rel = RelPath::new(path);
    let lang = Language::from_path(&rel)?;
    if lang == Language::C {
        if let Ok(source) = std::fs::read_to_string(root.join(path)) {
            return Language::from_path_and_source(&rel, &source);
        }
    }
    Some(lang)
}

/// Resolve language-server routing from an optional path.
fn server_language_from_opt_path(
    root: &Path,
    path: Option<&str>,
    what: &str,
) -> Result<Language, String> {
    let Some(path) = path else {
        return Err(needs_path(what));
    };
    server_language(root, path).ok_or_else(|| unknown_language(what, path))
}

impl PreciseCapability for LiveBackend {
    fn files_changed(&self, changes: &astrolabe_core::watch::ChangeSet) {
        use astrolabe_core::lsp::{FileChange, FileChangeKind};
        use astrolabe_core::watch::ChangeKind;
        let events: Vec<FileChange> = changes
            .iter()
            .map(|(kind, path)| FileChange {
                path: path.clone(),
                kind: match kind {
                    ChangeKind::Added => FileChangeKind::Created,
                    ChangeKind::Modified => FileChangeKind::Changed,
                    ChangeKind::Removed => FileChangeKind::Deleted,
                },
            })
            .collect();
        self.pool.notify_files_changed(&events);
    }

    fn references(
        &self,
        symbol: &str,
        path: Option<&str>,
        position: Option<Position>,
        exact: bool,
    ) -> Precise<Vec<Location>> {
        refs_graph::with_file_cache(self.file_cache.as_ref(), || {
            self.references_inner(symbol, path, position, exact)
        })
    }

    fn annotate(&self, symbol: &str, locations: &[Location]) -> Vec<refs_graph::HitInfo> {
        let index = self.syntax.as_ref().and_then(|f| f());
        refs_graph::with_file_cache(self.file_cache.as_ref(), || {
            refs_graph::annotate(index.as_deref(), &self.root, symbol, locations)
        })
    }

    fn source_lines(&self, path: &str) -> Option<Arc<Vec<String>>> {
        refs_graph::with_file_cache(self.file_cache.as_ref(), || {
            refs_graph::read_lines(&self.root, path)
        })
    }

    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let Some(language) = server_language(&self.root, path) else {
            return Precise::unknown(Vec::new(), unknown_language("精确定义查询", path));
        };
        let rel = RelPath::new(path);
        // The symbol wins over coordinates that disagree with it.
        let mut snap_note = None;
        let (line, character) = match (line, character, symbol) {
            (Some(l), Some(c), Some(name)) if !name.trim().is_empty() => {
                let snap = refs_graph::snap_position(
                    &self.root,
                    path,
                    name,
                    Position {
                        line: l,
                        character: c,
                    },
                );
                snap_note = snap.note;
                match snap.position {
                    Some(p) => (Some(p.line), Some(p.character)),
                    None => (None, None),
                }
            }
            other => (other.0, other.1),
        };
        let mut result = self.definition_at(language, &rel, symbol, line, character);
        if let Some(note) = snap_note {
            result.note = Some(match result.note.take().filter(|n| !n.is_empty()) {
                Some(rest) => format!("{note}\n{rest}"),
                None => note,
            });
        }
        result
    }

    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>> {
        self.diagnostics_with(path, false)
    }

    /// Language server only when asked (`lsp`), when one is already running
    /// for the language (in this session, or the shared gopls daemon), or
    /// with `ASTROLABE_DIAGNOSTICS=lsp`. Otherwise the toolchain / parser
    /// answers (`diag_toolchain`) and nothing memory-heavy starts.
    fn diagnostics_with(&self, path: &str, lsp: bool) -> Precise<Vec<Diagnostic>> {
        let rel = RelPath::new(path);
        let Some(language) = server_language(&self.root, path) else {
            return Precise::unknown(Vec::new(), unknown_language("诊断", path));
        };
        let use_server = self.lsp_enabled
            && (lsp
                || crate::diag_toolchain::lsp_by_default()
                || self.pool.is_resident(language)
                || (language == Language::Go && crate::diag_toolchain::gopls_daemon_running()));
        if !use_server {
            return crate::diag_toolchain::check(&self.root, &rel, language);
        }
        match self.with_server(language, |server| {
            query_server(server, &rel, FetchOptions::default())
        }) {
            Ok(result) => result,
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }

    /// `toolchain = false`: the parse only. The host runs builds and tests
    /// itself, so a diagnostics call must not start one.
    fn diagnostics_with_options(
        &self,
        path: &str,
        lsp: bool,
        toolchain: bool,
    ) -> Precise<Vec<Diagnostic>> {
        if toolchain {
            return self.diagnostics_with(path, lsp);
        }
        let rel = RelPath::new(path);
        let Some(language) = server_language(&self.root, path) else {
            return Precise::unknown(Vec::new(), unknown_language("诊断", path));
        };
        if lsp && self.lsp_enabled {
            return self.diagnostics_with(path, true);
        }
        crate::diag_toolchain::check_syntax_only(&self.root, &rel, language)
    }
}

impl LiveBackend {
    fn references_inner(
        &self,
        symbol: &str,
        path: Option<&str>,
        position: Option<Position>,
        exact: bool,
    ) -> Precise<Vec<Location>> {
        let index = self.syntax.as_ref().and_then(|f| f());
        let mut notes: Vec<String> = Vec::new();
        let mut path: Option<String> = path.map(str::to_string);
        let mut position = position;
        // A missing / wrong path is fixed from the index when the symbol has
        // one declaring file; several candidates are listed instead.
        if let Some(ix) = index.as_deref() {
            match refs_graph::fix_path(ix, &self.root, symbol, path.as_deref()) {
                PathFix::Keep => {}
                PathFix::Use { path: fixed, note } => {
                    path = Some(fixed);
                    // The coordinates belonged to the other file.
                    position = None;
                    notes.push(note);
                }
                PathFix::Ambiguous { note } => {
                    // Go: per-definition groups from the index when their
                    // references do not overlap (batch 8, E1).
                    let grouped = (!exact)
                        .then(|| {
                            refs_graph::grouped_references_without_path(ix, &self.root, symbol)
                        })
                        .flatten();
                    return grouped.unwrap_or_else(|| Precise::unknown(Vec::new(), note));
                }
            }
        }
        let language =
            match server_language_from_opt_path(&self.root, path.as_deref(), "精确引用查询") {
                Ok(lang) => lang,
                Err(note) => return Precise::unknown(Vec::new(), note),
            };
        let rel = RelPath::new(path.as_deref().expect("language resolved"));
        // The symbol wins over coordinates that disagree with it.
        if let Some(pos) = position {
            let snap = refs_graph::snap_position(&self.root, rel.as_str(), symbol, pos);
            position = snap.position;
            notes.extend(snap.note);
        }
        let target = match position {
            Some(position) => SymbolAt::Position(position),
            None => SymbolAt::Name(symbol),
        };
        // Graph first: the language server is the memory-heavy fallback and
        // is not started when the index alone resolves the name. Renames
        // (`exact`) always go to the server. TypeScript: unique top-level
        // declarations only (`refs_graph_ts::ts_graph_first`).
        let graph_first = refs_graph::graph_first_language(language)
            || index.as_deref().is_some_and(|ix| {
                crate::refs_graph_ts::ts_graph_first(ix, language, symbol.trim())
            })
            // Python: module-level names with exactly one definition.
            || (language == Language::Python
                && index
                    .as_deref()
                    .is_some_and(|ix| crate::py_refs::graph_first(ix, &self.root, symbol.trim())));
        let graph = (!exact && graph_first)
            .then_some(index.as_deref())
            .flatten()
            .and_then(|ix| {
                let name = symbol.trim();
                (!name.is_empty())
                    .then(|| {
                        let line = position.map(|p| p.line);
                        refs_graph::graph_references_at(ix, &self.root, name, &rel, line)
                    })
                    .flatten()
            });
        let mut result = match graph {
            Some(result) => result,
            // Index-only session: stop here and let the host's language
            // server answer. An empty list would read as "no references".
            None if !self.lsp_enabled => Precise::unknown(Vec::new(), HOST_LSP_NOTE.to_string()),
            None => self.server_references(language, &rel, target, index.as_deref()),
        };
        if language == Language::Go {
            if let Some(line) = self.go_implementors_line(&rel, target) {
                let note = result.note.get_or_insert_with(String::new);
                if !note.is_empty() && !note.ends_with('\n') {
                    note.push('\n');
                }
                note.push_str(&line);
            }
        }
        if !notes.is_empty() {
            let mut head = notes.join("\n");
            if let Some(rest) = result.note.take().filter(|n| !n.is_empty()) {
                head.push('\n');
                head.push_str(&rest);
            }
            result.note = Some(head);
        }
        result
    }
}

impl LiveBackend {
    fn definition_at(
        &self,
        language: Language,
        rel: &RelPath,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let rel = rel.clone();
        let target = match (line, character, symbol) {
            (Some(line), Some(character), _) => SymbolAt::Position(Position { line, character }),
            (_, _, Some(name)) if !name.is_empty() => SymbolAt::Name(name),
            _ => {
                return Precise::unknown(
                    Vec::new(),
                    "需要提供 symbol，或同时提供 line 与 character。",
                );
            }
        };
        // Index first: the server starts only when the index cannot decide.
        if let Some(found) = self.index_definition(language, &rel, target) {
            return found;
        }
        if !self.lsp_enabled {
            return Precise::unknown(Vec::new(), HOST_LSP_NOTE.to_string());
        }
        match self.with_server(language, |server| {
            let result = goto_definition(server, &self.root, &rel, target);
            (result, server.is_none_or(|s| s.is_busy()))
        }) {
            Ok((result, busy))
                if (result.value.is_empty()
                    && (!busy || result.confidence == Confidence::Unknown))
                    || self.unresolved_call(language, &rel, target, &result.value) =>
            {
                let Some(name) = self.target_name(&rel, target) else {
                    return result;
                };
                let hits = self.syntactic_definitions(language, &rel, &name);
                if hits.is_empty() {
                    return result;
                }
                let note = if !result.value.is_empty() {
                    format!(
                        "{}没能解析这个调用（只指回了调用处本身，常见于没有 compile_commands.json 或缺 include 路径）；先给出 tree-sitter 语法索引里的同名声明 {} 处，按头文件/源文件配对和 include 关系排在前面（syntactic：按名字匹配，可能含同名的其他实体）。",
                        primary_server_name(language),
                        hits.len()
                    )
                } else if busy {
                    warming_note(language, hits.len(), "同名声明")
                } else {
                    format!("{}先给出 tree-sitter 语法索引里的同名声明 {} 处（syntactic：按名字匹配，可能含同名的其他实体）。", empty_note(language, "定义"), hits.len())
                };
                Precise {
                    confidence: Confidence::Syntactic,
                    note: Some(note),
                    value: hits.into_iter().map(|(loc, _)| loc).collect(),
                }
            }
            Ok((result, _)) => self.with_paired_definitions(language, &rel, target, result),
            Err(error) => self
                .definition_without_server(language, &rel, target, &error)
                .unwrap_or_else(|| Precise::unknown(Vec::new(), error.to_string())),
        }
    }
}

// ------------------------------------------------------------------
// Index-first goto_definition and the no-server fallbacks (batch 7, L).
//
// The language server is the memory-heavy fallback: gopls 120 MB-1 GB and
// 1.2-2.2 s for the first call, while the index answers the common case
// (an unqualified or package-qualified name with one declaration in scope)
// in about a millisecond. The server is started only when the index has no
// answer or an ambiguous one: a member access, a possible local binding,
// several candidate declarations, or none.

/// Most declarations a `symbol`-only index answer lists from the origin file.
const MAX_INDEX_ANCHORS: usize = 5;
/// Cap on the hint line carried over from a missing server's error.
const HINT_CHARS: usize = 160;

impl LiveBackend {
    /// `goto_definition` from the index alone, or `None` when only the
    /// language server can decide.
    fn index_definition(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
    ) -> Option<Precise<Vec<Location>>> {
        if std::env::var("ASTROLABE_DEFINITION_INDEX_FIRST")
            .is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no"))
        {
            return None;
        }
        // Go only for now. Measured against typescript-language-server on
        // express, a same-file declaration is too often an import binding
        // (`const { x } = require('./utils')`), so JS / TS / Python keep the
        // server first; other languages have not been measured.
        if language != Language::Go {
            return None;
        }
        let index = self.syntax.as_ref().and_then(|f| f())?;
        let lines = read_lines(&self.root, rel.as_str())?;
        let (name, line, byte) = match target {
            SymbolAt::Position(pos) => {
                let text = lines.get(pos.line as usize)?;
                let (start, end) = ident_span(text, pos.character)?;
                (text[start..end].to_string(), pos.line as usize, start)
            }
            SymbolAt::Name(name) => {
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                // The origin file declares it: those declarations are the
                // answer (what the server returns for each of them too).
                let own = index_decls(&index, name, |f| f.path == *rel);
                if !own.is_empty() && own.len() <= MAX_INDEX_ANCHORS {
                    return Some(index_answer(
                        &self.root,
                        &index,
                        &own,
                        name,
                        "声明在该文件内",
                    ));
                }
                if !own.is_empty() {
                    return None;
                }
                let (line, byte) = first_code_occurrence(&lines, name)?;
                (name.to_string(), line, byte)
            }
        };
        let text = &lines[line];
        if in_string_or_comment(text, byte, language) {
            return None;
        }
        let symbols = &index.graph().symbols;
        // Enclosing function / method of the occurrence, for the local
        // binding check.
        let origin_id = index
            .graph()
            .files
            .iter()
            .find(|f| f.path == *rel)
            .map(|f| f.id)?;
        let line1 = line as u32 + 1;
        let enclosing = symbols
            .iter()
            .filter(|s| {
                s.file == origin_id
                    && matches!(
                        s.kind,
                        astrolabe_core::SymbolKind::Function | astrolabe_core::SymbolKind::Method
                    )
                    && s.start_line <= line1
                    && line1 <= s.end_line
            })
            .min_by_key(|s| s.end_line - s.start_line);
        if let Some(f) = enclosing {
            let from = f.start_line.saturating_sub(1) as usize;
            if maybe_local(&lines, from, line, &name, true) {
                return None;
            }
        }
        match qualifier(text, byte) {
            Qualifier::None => {
                let dir = dir_of(rel.as_str());
                let decls: Vec<usize> = index_decls(&index, &name, |f| {
                    f.language == Some(Language::Go) && dir_of(f.path.as_str()) == dir
                })
                .into_iter()
                .filter(|&i| !is_member(symbols[i].kind))
                .collect();
                (decls.len() == 1)
                    .then(|| index_answer(&self.root, &index, &decls, &name, "同包唯一声明"))
            }
            Qualifier::Selector(base) => {
                let dir = go_package_dir(&index, &self.root, rel, &lines, base)?;
                let decls: Vec<usize> = index_decls(&index, &name, |f| {
                    f.language == Some(Language::Go) && dir_of(f.path.as_str()) == dir
                })
                .into_iter()
                .filter(|&i| !is_member(symbols[i].kind))
                .collect();
                (decls.len() == 1)
                    .then(|| index_answer(&self.root, &index, &decls, &name, "按导入包解析"))
            }
            // Member access: the receiver's type decides.
            Qualifier::Member => None,
        }
    }

    /// `find_references` when the language server is missing or cannot
    /// start: Go from the import graph, everything else by whole-word text
    /// match. `None` when the index has nothing either.
    fn references_without_server(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        index: Option<&RepoIndex>,
        error: &LspError,
    ) -> Option<Precise<Vec<Location>>> {
        let name = self.target_name(rel, target)?;
        let hint = missing_server_line(language, error);
        if let Some(ix) = index {
            if let Some(mut graph) = refs_graph::graph_references(ix, &self.root, &name, rel) {
                if !graph.value.is_empty() {
                    // Keep the graph's own note (receiver checks, markers
                    // the completion line keys off); add the hint after it.
                    let note = graph.note.get_or_insert_with(String::new);
                    if !note.is_empty() && !note.ends_with('\n') {
                        note.push('\n');
                    }
                    note.push_str(&hint);
                    return Some(graph);
                }
            }
        }
        let files = index
            .and_then(RepoIndex::incremental)
            .map(|inc| inc.admitted_paths());
        let text = text_fallback_references_in(&self.root, rel, target, "", files)?;
        if text.value.is_empty() {
            return None;
        }
        Some(Precise {
            confidence: Confidence::Syntactic,
            note: Some(format!(
                "以下为索引结果（syntactic：同语言文件里 `{name}` 的全词匹配，可能含注释、字符串和同名的其他实体）。\n{hint}"
            )),
            value: text.value,
        })
    }

    /// `goto_definition` when the language server is missing or cannot
    /// start: same-name declarations from the index.
    fn definition_without_server(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        error: &LspError,
    ) -> Option<Precise<Vec<Location>>> {
        let name = self.target_name(rel, target)?;
        let hits = self.syntactic_definitions(language, rel, &name);
        if hits.is_empty() {
            return None;
        }
        Some(Precise {
            confidence: Confidence::Syntactic,
            note: Some(format!(
                "以下为索引里的同名声明 {} 处（syntactic：按名字匹配，本文件和 include/import 的文件排在前面）。\n{}",
                hits.len(),
                missing_server_line(language, error)
            )),
            value: hits.into_iter().map(|(loc, _)| loc).collect(),
        })
    }
}

/// One short line on why the server did not answer and how to get it. Must
/// not contain the renderer's "server missing" markers, which would append
/// the long install block to an answer that is not empty.
fn missing_server_line(language: Language, error: &LspError) -> String {
    let server = primary_server_name(language);
    let (what, detail) = match error {
        LspError::Unavailable(_, hint) => ("未安装", hint.as_str()),
        LspError::Startup(_, why) => ("启动失败", why.as_str()),
        _ => ("没有应答", ""),
    };
    let mut detail: String = detail
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(HINT_CHARS)
        .collect();
    for marker in [
        "no language server available",
        "failed to start",
        "not installed",
    ] {
        detail = detail.replace(marker, "unavailable");
    }
    if detail.is_empty() {
        format!("语言服务器 {server} {what}，精确结果需安装后重试。")
    } else {
        format!("语言服务器 {server} {what}，精确结果需安装后重试：{detail}")
    }
}

fn read_lines(root: &Path, path: &str) -> Option<Vec<String>> {
    let source = astrolabe_core::text::read_source(&root.join(path)).ok()?;
    Some(
        source
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
            .collect(),
    )
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_member(kind: astrolabe_core::SymbolKind) -> bool {
    matches!(
        kind,
        astrolabe_core::SymbolKind::Method | astrolabe_core::SymbolKind::Field
    )
}

/// Byte span of the identifier under UTF-16 column `character`.
fn ident_span(line: &str, character: u32) -> Option<(usize, usize)> {
    let mut utf16 = 0u32;
    let mut at = None;
    for (b, c) in line.char_indices() {
        if utf16 >= character {
            at = Some(b);
            break;
        }
        utf16 += c.len_utf16() as u32;
    }
    let at = at?;
    if !line[at..].chars().next().is_some_and(is_ident_char) {
        return None;
    }
    let start = line[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident_char(*c))
        .last()
        .map_or(at, |(i, _)| i);
    let end = line[at..]
        .char_indices()
        .find(|(_, c)| !is_ident_char(*c))
        .map_or(line.len(), |(i, _)| at + i);
    Some((start, end))
}

/// `byte` sits inside a string literal or after a line comment on `line`
/// (single-line view; a block comment line starting with `*` counts too).
fn in_string_or_comment(line: &str, byte: usize, language: Language) -> bool {
    let t = line.trim_start();
    if t.starts_with("/*") || t.starts_with("* ") || t == "*" {
        return true;
    }
    let hash_comment = matches!(
        language,
        Language::Python | Language::Ruby | Language::Shell
    );
    let mut quote: Option<char> = None;
    let mut chars = line[..byte].chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q != '`' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '`' => quote = Some(c),
                '\'' if language != Language::Rust => quote = Some(c),
                '/' if chars.peek() == Some(&'/') => return true,
                '#' if hash_comment => return true,
                _ => {}
            },
        }
    }
    quote.is_some()
}

fn utf16_col(line: &str, byte: usize) -> u32 {
    line[..byte].encode_utf16().count() as u32
}

/// First whole-word occurrence of `name` outside an obvious comment line.
fn first_code_occurrence(lines: &[String], name: &str) -> Option<(usize, usize)> {
    lines.iter().enumerate().find_map(|(i, l)| {
        let t = l.trim_start();
        if ["//", "#", "/*", "*", "--"]
            .iter()
            .any(|p| t.starts_with(p))
        {
            return None;
        }
        refs_graph::word_offsets(l, name).first().map(|&b| (i, b))
    })
}

/// Positions in `graph().symbols` of declarations named `name` whose file
/// passes `keep`.
fn index_decls(
    index: &RepoIndex,
    name: &str,
    keep: impl Fn(&astrolabe_core::types::CodeFile) -> bool,
) -> Vec<usize> {
    let symbols = &index.graph().symbols;
    let mut out: Vec<usize> = index
        .names()
        .by_lower(&name.to_lowercase())
        .iter()
        .map(|&i| i as usize)
        .filter(|&i| symbols[i].name == name)
        .filter(|&i| index.file(symbols[i].file).is_some_and(&keep))
        .collect();
    out.sort_by_key(|&i| {
        (
            index.file(symbols[i].file).map(|f| f.path.clone()),
            symbols[i].start_line,
        )
    });
    out
}

/// Location of each declaration: its line, at the name when the line shows it.
fn index_answer(
    root: &Path,
    index: &RepoIndex,
    decls: &[usize],
    name: &str,
    why: &str,
) -> Precise<Vec<Location>> {
    let symbols = &index.graph().symbols;
    let units = name.encode_utf16().count() as u32;
    let mut value = Vec::new();
    for &i in decls {
        let s = &symbols[i];
        let Some(file) = index.file(s.file) else {
            continue;
        };
        let line = s.start_line.saturating_sub(1);
        let col = read_lines(root, file.path.as_str()).and_then(|ls| {
            let l = ls.get(line as usize)?;
            refs_graph::word_offsets(l, name)
                .first()
                .map(|&b| utf16_col(l, b))
        });
        let (start, end) = match col {
            Some(c) => (c, c + units),
            None => (0, 0),
        };
        value.push(Location {
            path: file.path.clone(),
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
        });
    }
    Precise {
        confidence: Confidence::Scoped,
        note: Some(format!("索引解析（{why}），未启动语言服务器。")),
        value,
    }
}

enum Qualifier<'a> {
    /// Bare name.
    None,
    /// `base.name`, base an identifier.
    Selector(&'a str),
    /// `x->name`, `x::name`, `).name`, `this.name`: a member, decided by a type.
    Member,
}

fn qualifier(line: &str, byte: usize) -> Qualifier<'_> {
    let before = line[..byte].trim_end();
    if before.ends_with("->") || before.ends_with("::") || before.ends_with("?.") {
        return Qualifier::Member;
    }
    let Some(rest) = before.strip_suffix('.') else {
        return Qualifier::None;
    };
    let rest = rest.trim_end();
    let start = rest
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident_char(*c))
        .last()
        .map(|(i, _)| i);
    match start {
        Some(s) if !rest[..s].trim_end().ends_with(['.', '>', ':']) => {
            Qualifier::Selector(&rest[s..])
        }
        _ => Qualifier::Member,
    }
}

const NOT_A_TYPE: &[&str] = &[
    "return", "case", "yield", "throw", "await", "in", "of", "else", "not", "and", "or", "is",
    "if", "while", "new", "delete", "go", "defer", "print", "echo", "do", "then",
];
const BINDERS: &[&str] = &[
    "var", "let", "const", "val", "auto", "mut", "my", "local", "final", "global", "nonlocal",
];

/// `name` may be bound locally in the function starting at line `from`
/// (0-based) before or at line `to`: a parameter, `:=`, `var x`, `T x =`, …
/// Errs on the side of "yes" (the server then decides).
fn maybe_local(lines: &[String], from: usize, to: usize, name: &str, go: bool) -> bool {
    for (i, line) in lines.iter().enumerate().take(to + 1).skip(from) {
        for b in refs_graph::word_offsets(line, name) {
            let before = line[..b].trim_end();
            let after = line[b + name.len()..].trim_start();
            if before.ends_with('.') || before.ends_with("->") || after.starts_with('(') {
                continue;
            }
            let prev_word: &str = {
                let s = before
                    .char_indices()
                    .rev()
                    .take_while(|(_, c)| is_ident_char(*c))
                    .last()
                    .map_or(before.len(), |(k, _)| k);
                &before[s..]
            };
            if BINDERS.contains(&prev_word) {
                return true;
            }
            let next = after.chars().next();
            let next2 = after.chars().nth(1);
            if go {
                // `x :=`, `a, x :=`, `for _, x := range`, struct keys `X:`.
                if let Some(k) = line.find(":=") {
                    if b < k {
                        return true;
                    }
                }
                if next == Some(':') && next2 != Some('=') {
                    return true;
                }
                if i == from {
                    // Parameters / receiver: `x T`, `x, y T`, `x *T`.
                    if next.is_some_and(|c| is_ident_char(c) || matches!(c, '*' | '[' | ','))
                        && before.ends_with(['(', ','])
                    {
                        return true;
                    }
                }
                continue;
            }
            let assigns = next == Some('=') && !matches!(next2, Some('=' | '>'));
            let annotated = next == Some(':') && next2 != Some(':');
            if assigns || annotated {
                return true;
            }
            if i == from && matches!(next, Some(',' | ')')) {
                return true;
            }
            // `T x;` / `T x,` / `T x)` (C family, Java, C#).
            let typed = before
                .ends_with(|c: char| is_ident_char(c) || matches!(c, '>' | ']' | '*' | '&'))
                && !NOT_A_TYPE.contains(&prev_word);
            if typed && matches!(next, Some(';' | ',' | ')' | '=')) {
                return true;
            }
        }
    }
    false
}

/// Directory of the Go package `base` names in `origin` (an import of this
/// repo), from the origin's import edges and its import block.
fn go_package_dir<'a>(
    index: &'a RepoIndex,
    root: &Path,
    origin: &RelPath,
    lines: &[String],
    base: &str,
) -> Option<&'a str> {
    let g = index.graph();
    let origin_id = g.files.iter().find(|f| f.path == *origin)?.id;
    let mut dirs: Vec<&str> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import && e.from == origin_id.0)
        .filter_map(|e| g.files.iter().find(|f| f.id.0 == e.to))
        .map(|f| dir_of(f.path.as_str()))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    let imports = go_imports(lines);
    let mut found: Vec<&str> = Vec::new();
    for dir in dirs {
        let Some((alias, _)) = imports.iter().find(|(_, path)| {
            !dir.is_empty() && (path == dir || path.ends_with(&format!("/{dir}")))
        }) else {
            continue;
        };
        let local = match alias {
            Some(a) => a.clone(),
            None => go_package_name(index, root, dir)
                .unwrap_or_else(|| dir.rsplit('/').next().unwrap_or(dir).to_string()),
        };
        if local == base {
            found.push(dir);
        }
    }
    match found.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// `(alias, path)` of every import spec in a Go file.
fn go_imports(lines: &[String]) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in lines {
        let t = line.trim();
        let spec = if in_block {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            t
        } else if let Some(rest) = t.strip_prefix("import") {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                in_block = true;
                continue;
            }
            rest
        } else if t.starts_with("func ") || t.starts_with("type ") {
            break;
        } else {
            continue;
        };
        let Some(q) = spec.find('"') else { continue };
        let Some(end) = spec[q + 1..].find('"') else {
            continue;
        };
        let path = spec[q + 1..q + 1 + end].to_string();
        let alias = spec[..q].trim();
        out.push(((!alias.is_empty()).then(|| alias.to_string()), path));
    }
    out
}

/// The `package` clause of a Go file in `dir`.
fn go_package_name(index: &RepoIndex, root: &Path, dir: &str) -> Option<String> {
    let file = index.graph().files.iter().find(|f| {
        f.language == Some(Language::Go)
            && dir_of(f.path.as_str()) == dir
            && !f.path.as_str().ends_with("_test.go")
    })?;
    let lines = read_lines(root, file.path.as_str())?;
    lines.iter().find_map(|l| {
        l.trim()
            .strip_prefix("package ")
            .map(|p| p.split_whitespace().next().unwrap_or("").to_string())
    })
}

#[cfg(test)]
mod start_failure_tests {
    use super::*;

    /// A missing kotlin server must surface its install hint, not loop on
    /// "still importing" forever.
    #[test]
    fn missing_kotlin_server_reports_real_error_after_background_failure() {
        let root = std::env::temp_dir();
        let empty_path = std::env::temp_dir().join("astrolabe-no-such-bin-dir");
        let pool = LspPool::from_discovery_and_transport(
            Discovery::new(&empty_path),
            StdioSpawn::new(&root),
            root.clone(),
            LspPoolConfig::default(),
        );
        let syntax: SyntaxIndex = Arc::new(|| None);
        let backend = LiveBackend {
            root,
            pool: Arc::new(pool),
            syntax: Some(syntax),
            starting: Arc::new(AtomicBool::new(false)),
            start_failed: Arc::new(AtomicBool::new(false)),
            file_cache: None,
            lsp_enabled: true,
        };
        // First call: warming while the background start runs.
        let first = backend.with_server(Language::Kotlin, |s| s.is_none());
        assert!(matches!(first, Ok(true)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !backend.start_failed.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "start never failed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        while backend.starting.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Next call: the real acquire error, every time.
        for _ in 0..2 {
            assert!(backend
                .with_server(Language::Kotlin, |s| s.is_none())
                .is_err());
        }
    }
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use astrolabe_core::types::{CodeEdge, CodeFile, CodeSymbol, SymbolId, SymbolKind};

    fn graph() -> CodeGraph {
        let file = |id: u32, path: &str| CodeFile {
            id: FileId(id),
            path: RelPath::new(path),
            language: Some(Language::C),
            loc: 10,
            sha: String::new(),
        };
        let sym = |id: u32, file: u32, name: &str, line: u32| CodeSymbol {
            id: SymbolId(id),
            file: FileId(file),
            name: name.into(),
            kind: SymbolKind::Function,
            signature: String::new(),
            start_line: line,
            end_line: line + 3,
            exported: true,
        };
        let edge = |from: u32, to: u32, kind: EdgeKind| CodeEdge {
            from,
            to,
            kind,
            weight: 1,
            confidence: Confidence::Scoped,
        };
        CodeGraph {
            files: vec![
                file(0, "src/sds.h"),
                file(1, "src/sds.c"),
                file(2, "src/server.c"),
                file(3, "deps/other/sds.c"),
            ],
            symbols: vec![
                sym(0, 0, "sdsnew", 10),
                sym(1, 1, "sdsnew", 80),
                sym(2, 3, "sdsnew", 5),
                sym(3, 2, "main", 1),
            ],
            edges: vec![edge(2, 0, EdgeKind::Import), edge(0, 1, EdgeKind::Pair)],
        }
    }

    #[test]
    fn header_only_answer_gains_the_paired_source_definition() {
        let g = graph();
        let at = |line| Position { line, character: 0 };
        let found = vec![Location {
            path: RelPath::new("src/sds.h"),
            range: Range {
                start: at(9),
                end: at(9),
            },
        }];
        let extra = paired_definitions(&g, &found, "sdsnew");
        // The partner of sds.h only, not the unrelated deps/other/sds.c.
        assert_eq!(
            extra,
            vec![Location {
                path: RelPath::new("src/sds.c"),
                range: Range {
                    start: at(79),
                    end: at(79),
                },
            }]
        );
        assert!(paired_definitions(&g, &found, "nothere").is_empty());
    }

    #[test]
    fn near_files_follow_includes_and_their_partners() {
        let g = graph();
        let near = near_files(&g, &RelPath::new("src/server.c"));
        assert_eq!(near, HashSet::from([FileId(0), FileId(1)]));
        let near = near_files(&g, &RelPath::new("src/sds.c"));
        assert_eq!(near, HashSet::from([FileId(0)]));
        assert!(near_files(&g, &RelPath::new("missing.c")).is_empty());
        assert!(is_c_header("a/b.HPP") && !is_c_header("a/b.c") && !is_c_header("h"));
    }

    /// The old fallback order: origin file first, then by path.
    fn self_free_syntactic(index: &RepoIndex, origin: &RelPath, name: &str) -> Vec<Location> {
        let g = index.graph();
        let mut hits: Vec<(bool, Location)> = g
            .symbols
            .iter()
            .filter(|s| s.name == name)
            .filter_map(|s| {
                let f = g.files.iter().find(|f| f.id == s.file)?;
                let at = Position {
                    line: s.start_line.saturating_sub(1),
                    character: 0,
                };
                Some((
                    f.path != *origin,
                    Location {
                        path: f.path.clone(),
                        range: Range { start: at, end: at },
                    },
                ))
            })
            .collect();
        hits.sort();
        hits.into_iter().map(|(_, l)| l).collect()
    }

    /// Real clangd, no compile database. Ignored: needs
    /// `ASTROLABE_LSP_C=/path/to/clangd` and `ASTROLABE_TEST_C_REPO=/copy/of/redis`.
    #[test]
    #[ignore = "needs clangd and a C repo (ASTROLABE_LSP_C, ASTROLABE_TEST_C_REPO)"]
    fn live_clangd_definition_hops_from_header_to_source() {
        let Some(root) = std::env::var_os("ASTROLABE_TEST_C_REPO").map(PathBuf::from) else {
            return;
        };
        let max_cases: usize = std::env::var("PAIR_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let t = std::time::Instant::now();
        let index = Arc::new(crate::index::build(&root).expect("index"));
        eprintln!("index {:?}", t.elapsed());
        let g = index.graph();
        let pairs: Vec<(FileId, FileId)> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Pair)
            .map(|e| (FileId(e.from), FileId(e.to)))
            .collect();
        eprintln!("pair edges {}", pairs.len());
        let path_of = |id: FileId| g.files.iter().find(|f| f.id == id).unwrap().path.clone();
        // Cases: a function with a prototype in header h and a body in its
        // partner s, called from another .c file.
        let mut cases: Vec<(RelPath, u32, u32, String, RelPath, u32)> = Vec::new();
        let c_files: Vec<&CodeFile> = g
            .files
            .iter()
            .filter(|f| f.path.as_str().ends_with(".c"))
            .collect();
        'outer: for (h, s) in &pairs {
            let mut names: Vec<(&str, u32)> = g
                .symbols
                .iter()
                .filter(|x| x.file == *s && x.kind == SymbolKind::Function)
                .filter(|x| g.symbols.iter().any(|y| y.file == *h && y.name == x.name))
                .map(|x| (x.name.as_str(), x.start_line))
                .collect();
            names.sort();
            names.dedup_by(|a, b| a.0 == b.0);
            for (name, def_line) in names.into_iter().take(3) {
                for f in &c_files {
                    if f.id == *s {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(root.join(f.path.as_str())) else {
                        continue;
                    };
                    let needle = format!("{name}(");
                    let hit = text.lines().enumerate().find_map(|(i, l)| {
                        let c = l.find(&needle)?;
                        let before = l[..c].chars().last();
                        (!l.trim_start().starts_with('#')
                            && !before.is_some_and(|b| b.is_alphanumeric() || b == '_'))
                        .then_some((i as u32, c as u32))
                    });
                    if let Some((line, col)) = hit {
                        cases.push((
                            f.path.clone(),
                            line,
                            col,
                            name.to_string(),
                            path_of(*s),
                            def_line,
                        ));
                        if cases.len() >= max_cases {
                            break 'outer;
                        }
                        break;
                    }
                }
            }
        }
        eprintln!("cases {}", cases.len());
        let backend = LiveBackend::new(&root).with_syntax_index(Arc::new({
            let index = Arc::clone(&index);
            move || Some(Arc::clone(&index))
        }));
        let (mut raw_ok, mut raw_header_only, mut after_ok, mut raw_empty, mut old_ok) =
            (0, 0, 0, 0, 0);
        let mut hop_time = std::time::Duration::ZERO;
        for (path, line, col, name, src, def_line) in &cases {
            let target = SymbolAt::Position(Position {
                line: *line,
                character: *col,
            });
            let raw = backend
                .with_server(Language::C, |server| {
                    goto_definition(server, &root, path, target)
                })
                .expect("clangd");
            let _ = def_line;
            let hits_def = |locs: &[Location]| locs.iter().any(|l| l.path == *src);
            if raw.value.is_empty() {
                raw_empty += 1;
            }
            // What the previous code answered: the server's list as is, or
            // (empty only) the syntactic list in path order.
            let old: Vec<Location> = if raw.value.is_empty() {
                let mut v: Vec<Location> = self_free_syntactic(&index, path, name);
                v.truncate(MAX_SYNTACTIC_DEFINITIONS);
                v
            } else {
                raw.value.clone()
            };
            if hits_def(&old) {
                old_ok += 1;
            }
            if hits_def(&raw.value) {
                raw_ok += 1;
            }
            if !raw.value.is_empty() && raw.value.iter().all(|l| is_c_header(l.path.as_str())) {
                raw_header_only += 1;
            }
            let t = std::time::Instant::now();
            let after = backend.definition(path.as_str(), None, Some(*line), Some(*col));
            hop_time += t.elapsed();
            if hits_def(&after.value) {
                after_ok += 1;
            } else {
                eprintln!(
                    "miss: {} {}:{} -> {:?}",
                    name,
                    path.as_str(),
                    line + 1,
                    after.value
                );
            }
        }
        eprintln!(
            "cases={} old_correct={} raw_correct={} raw_header_only={} raw_empty={} after_correct={} full_definition_total={:?}",
            cases.len(),
            old_ok,
            raw_ok,
            raw_header_only,
            raw_empty,
            after_ok,
            hop_time
        );
        backend.pool.shutdown();
    }
}

#[cfg(test)]
mod go_implementor_tests {
    use super::*;

    #[test]
    fn go_interface_references_note_lists_implementors() {
        let dir = std::env::temp_dir().join(format!("astrolabe-lb-goiface-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("p")).unwrap();
        std::fs::write(dir.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(
            dir.join("p/i.go"),
            "package p\n\ntype Shape interface {\n\tArea() float64\n}\n\ntype Sq struct{}\n\nfunc (Sq) Area() float64 { return 1 }\n",
        )
        .unwrap();
        let index =
            Arc::new(crate::index::build_with_options(&dir, false, 1 << 20).expect("index"));
        let backend =
            LiveBackend::new(dir.clone()).with_syntax_index(Arc::new(move || Some(index.clone())));
        let rel = RelPath::new("p/i.go");
        let line = backend
            .go_implementors_line(&rel, SymbolAt::Name("Shape"))
            .expect("implementors");
        assert!(line.contains("Shape @p/i.go:3 ← Sq @p/i.go:7"), "{line}");
        assert!(backend
            .go_implementors_line(&rel, SymbolAt::Name("Sq"))
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Graph-vs-language-server recall harness for `find_references`
/// (batch 6). Ignored: needs real servers and corpora.
///
/// `REFS_RECALL_REPO=/abs/repo` and either `REFS_RECALL_CASES="path:sym,…"`
/// or `REFS_RECALL_SAMPLE=<lang ext>:<n>` (unique-declaration symbols from
/// non-test files, every k-th). Prints one row per case and a summary line.
#[cfg(test)]
mod recall_harness {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    #[ignore = "needs language servers and a corpus (REFS_RECALL_REPO)"]
    fn graph_vs_server_recall() {
        let Some(root) = std::env::var_os("REFS_RECALL_REPO").map(PathBuf::from) else {
            return;
        };
        let index = Arc::new(crate::index::build(&root).expect("index"));
        let g = index.graph();
        let mut cases: Vec<(String, String)> = Vec::new();
        if let Ok(list) = std::env::var("REFS_RECALL_CASES") {
            for c in list.split(',').filter(|c| !c.is_empty()) {
                let (p, s) = c.rsplit_once(':').expect("path:sym");
                cases.push((p.to_string(), s.to_string()));
            }
        }
        if let Ok(spec) = std::env::var("REFS_RECALL_SAMPLE") {
            let (exts, n) = spec.rsplit_once(':').expect("ext:n");
            let n: usize = n.parse().unwrap();
            let exts: Vec<&str> = exts.split('|').collect();
            let mut pool: Vec<(String, String)> = Vec::new();
            let mut count: std::collections::HashMap<&str, usize> = Default::default();
            for s in &g.symbols {
                *count.entry(s.name.as_str()).or_default() += 1;
            }
            for s in &g.symbols {
                let Some(f) = index.file(s.file) else {
                    continue;
                };
                let p = f.path.as_str();
                if !exts.iter().any(|e| p.ends_with(e))
                    || crate::explain::is_test_caller(p, &s.name)
                    || count[s.name.as_str()] != 1
                    || s.name.len() < 4
                    || !matches!(
                        s.kind,
                        astrolabe_core::SymbolKind::Function
                            | astrolabe_core::SymbolKind::Method
                            | astrolabe_core::SymbolKind::Class
                            | astrolabe_core::SymbolKind::Interface
                            | astrolabe_core::SymbolKind::Struct
                            | astrolabe_core::SymbolKind::Type
                    )
                {
                    continue;
                }
                pool.push((p.to_string(), s.name.clone()));
            }
            pool.sort();
            let step = (pool.len() / n.max(1)).max(1);
            cases.extend(pool.into_iter().step_by(step).take(n));
        }
        let backend = LiveBackend::new(&root).with_syntax_index(Arc::new({
            let index = Arc::clone(&index);
            move || Some(Arc::clone(&index))
        }));
        let key = |l: &Location| {
            (
                l.path.as_str().to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        };
        let (mut answered, mut deferred, mut exact_eq) = (0usize, 0usize, 0usize);
        let (mut tp, mut ls, mut gs) = (0usize, 0usize, 0usize);
        for (path, sym) in &cases {
            let rel = RelPath::new(path);
            let Some(lang) = server_language(&root, path) else {
                continue;
            };
            let t = std::time::Instant::now();
            let lsp = backend
                .with_server(lang, |s| {
                    find_references(s, &root, &rel, SymbolAt::Name(sym))
                })
                .expect("server");
            let lsp_ms = t.elapsed().as_millis();
            let t = std::time::Instant::now();
            let graph = refs_graph::graph_references(&index, &root, sym, &rel);
            let graph_ms = t.elapsed().as_millis();
            let l: BTreeSet<_> = lsp.value.iter().map(key).collect();
            match graph {
                None => {
                    deferred += 1;
                    println!(
                        "ROW\t{path}\t{sym}\tlsp={}\t{:?}\tgraph=defer\t{lsp_ms}ms",
                        l.len(),
                        lsp.confidence
                    );
                }
                Some(gr) => {
                    answered += 1;
                    let gset: BTreeSet<_> = gr.value.iter().map(key).collect();
                    let inter = l.intersection(&gset).count();
                    tp += inter;
                    ls += l.len();
                    gs += gset.len();
                    if l == gset {
                        exact_eq += 1;
                    }
                    let missed: Vec<_> = l.difference(&gset).take(3).collect();
                    let extra: Vec<_> = gset.difference(&l).take(3).collect();
                    println!(
                        "ROW\t{path}\t{sym}\tlsp={}\t{:?}\tgraph={}\tboth={inter}\t{lsp_ms}ms/{graph_ms}ms\tmissed={missed:?}\textra={extra:?}",
                        l.len(),
                        lsp.confidence,
                        gset.len()
                    );
                }
            }
        }
        println!(
            "SUMMARY\tcases={}\tgraph_answered={answered}\tdeferred={deferred}\tidentical={exact_eq}\trecall={:.3}\tprecision={:.3}\t(tp={tp} lsp={ls} graph={gs})",
            cases.len(),
            tp as f64 / ls.max(1) as f64,
            tp as f64 / gs.max(1) as f64
        );
        backend.pool.shutdown();
    }

    /// Batch 8 H6: Go names with several declarations. Per declaration,
    /// the index group (`refs_graph::go_groups_for_test`) vs gopls at the
    /// declaration; then, for every gopls hit, which declaration the call
    /// site picks (`graph_references_at` with that file and line).
    /// `GROUPED_REPO=/abs/repo`. Starts gopls (measurement only).
    #[test]
    #[ignore = "needs gopls and a corpus (GROUPED_REPO)"]
    fn grouped_vs_server() {
        let Some(root) = std::env::var_os("GROUPED_REPO").map(PathBuf::from) else {
            return;
        };
        let index = Arc::new(crate::index::build(&root).expect("index"));
        let g = index.graph();
        let mut by_name: std::collections::BTreeMap<&str, Vec<&astrolabe_core::CodeSymbol>> =
            Default::default();
        for s in &g.symbols {
            if index
                .file(s.file)
                .is_some_and(|f| f.language == Some(Language::Go))
            {
                by_name.entry(s.name.as_str()).or_default().push(s);
            }
        }
        let backend = LiveBackend::new(&root).with_syntax_index(Arc::new({
            let index = Arc::clone(&index);
            move || Some(Arc::clone(&index))
        }));
        let key = |l: &Location| {
            (
                l.path.as_str().to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        };
        let (mut names, mut grouped, mut eq, mut tp, mut ls, mut gs) = (0, 0, 0, 0, 0, 0);
        let (mut site_right, mut site_wrong, mut site_group, mut site_none) = (0, 0, 0, 0);
        for (name, syms) in by_name {
            let mut lines_seen = BTreeSet::new();
            let syms: Vec<_> = syms
                .into_iter()
                .filter(|s| lines_seen.insert((s.file, s.start_line)))
                .collect();
            if syms.len() < 2
                || syms.len() > 6
                || name.len() < 3
                || syms
                    .iter()
                    .any(|s| s.kind == astrolabe_core::SymbolKind::Field)
            {
                continue;
            }
            names += 1;
            let Some(groups) = refs_graph::go_groups_for_test(&index, &root, name) else {
                println!("NAME\t{name}\tdecls={}\tdefer", syms.len());
                continue;
            };
            grouped += 1;
            for (path, line1, hits) in &groups {
                let rel = RelPath::new(path);
                let Some(lines) = refs_graph::read_lines(&root, path) else {
                    continue;
                };
                let text = &lines[*line1 as usize - 1];
                let Some(&b) = refs_graph::word_offsets(text, name).first() else {
                    continue;
                };
                let pos = Position {
                    line: line1 - 1,
                    character: text[..b].encode_utf16().count() as u32,
                };
                let lsp = backend
                    .with_server(Language::Go, |s| {
                        find_references(s, &root, &rel, SymbolAt::Position(pos))
                    })
                    .expect("server");
                let l: BTreeSet<_> = lsp.value.iter().map(key).collect();
                let gset: BTreeSet<_> = hits.iter().map(key).collect();
                let inter = l.intersection(&gset).count();
                tp += inter;
                ls += l.len();
                gs += gset.len();
                if l == gset {
                    eq += 1;
                }
                let missed: Vec<_> = l.difference(&gset).take(3).collect();
                let extra: Vec<_> = gset.difference(&l).take(3).collect();
                println!(
                    "GROUP\t{name}\t{path}:{line1}\tlsp={}\tgraph={}\tboth={inter}\tmissed={missed:?}\textra={extra:?}",
                    l.len(),
                    gset.len()
                );
                // Call sites: does the line pick this declaration?
                for (p, ln, _) in &l {
                    if p == path && *ln + 1 == *line1 {
                        continue;
                    }
                    let r = refs_graph::graph_references_at(
                        &index,
                        &root,
                        name,
                        &RelPath::new(p),
                        Some(*ln),
                    );
                    let note = r.as_ref().and_then(|r| r.note.clone()).unwrap_or_default();
                    match r {
                        None => site_none += 1,
                        Some(_) if note.contains("已按定义分组") => site_group += 1,
                        Some(r) => {
                            let has_decl = r.value.iter().any(|h| {
                                h.path.as_str() == path && h.range.start.line + 1 == *line1
                            });
                            if has_decl {
                                site_right += 1;
                            } else {
                                site_wrong += 1;
                                println!("SITE_WRONG\t{name}\t{p}:{}\twant {path}:{line1}", ln + 1);
                            }
                        }
                    }
                }
            }
        }
        println!(
            "SUMMARY\tnames={names}\tgrouped={grouped}\tgroups_identical={eq}\trecall={:.3}\tprecision={:.3}\t(tp={tp} lsp={ls} graph={gs})\tsites right={site_right} wrong={site_wrong} grouped={site_group} none={site_none}",
            tp as f64 / ls.max(1) as f64,
            tp as f64 / gs.max(1) as f64
        );
        backend.pool.shutdown();
    }
}

#[cfg(test)]
mod graph_first_tests {
    use super::*;

    fn go_repo() -> (PathBuf, LiveBackend) {
        let dir =
            std::env::temp_dir().join(format!("astrolabe-lb-graphfirst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("service")).unwrap();
        std::fs::create_dir_all(dir.join("controller")).unwrap();
        std::fs::write(dir.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(
            dir.join("service/channel.go"),
            "package service\n\n// ShouldDisableChannel decides.\nfunc ShouldDisableChannel() bool { return false }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("controller/relay.go"),
            "package controller\n\nimport \"example.com/m/service\"\n\nfunc R() {\n\t_ = service.ShouldDisableChannel()\n}\n",
        )
        .unwrap();
        let index =
            Arc::new(crate::index::build_with_options(&dir, false, 1 << 20).expect("index"));
        let backend =
            LiveBackend::new(dir.clone()).with_syntax_index(Arc::new(move || Some(index.clone())));
        (dir, backend)
    }

    /// Unambiguous Go name, wrong path, off coordinates: answered from the
    /// index, the path fixed, and no language server started.
    #[test]
    fn go_references_answer_from_the_index_without_starting_gopls() {
        let (dir, backend) = go_repo();
        let r = backend.references(
            "ShouldDisableChannel",
            Some("relay/channel/disable.go"),
            Some(Position {
                line: 3,
                character: 0,
            }),
            false,
        );
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        let note = r.note.clone().unwrap();
        assert!(
            note.contains("不存在") && note.contains("service/channel.go"),
            "{note}"
        );
        assert!(note.contains("未启动语言服务器"), "{note}");
        assert!(!note.contains("Kotlin"), "{note}");
        let got: Vec<String> = r
            .value
            .iter()
            .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
            .collect();
        assert_eq!(got, vec!["controller/relay.go:6", "service/channel.go:4"]);
        assert!(!backend.pool.is_resident(Language::Go));
        // No path at all: same answer.
        let r = backend.references("ShouldDisableChannel", None, None, false);
        assert_eq!(r.value.len(), 2, "{r:?}");
        // Annotation: declaration and enclosing function.
        let info = backend.annotate("ShouldDisableChannel", &r.value);
        assert!(info.iter().any(|h| h.declaration));
        assert!(info
            .iter()
            .any(|h| h.enclosing.as_ref().is_some_and(|e| e.0 == "R")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unique TS top-level function: answered from the index (barrel
    /// included), no tsserver started.
    #[test]
    fn ts_unique_top_level_answers_from_the_index_without_tsserver() {
        let dir = std::env::temp_dir().join(format!("astrolabe-lb-tsfirst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/icons")).unwrap();
        std::fs::create_dir_all(dir.join("src/pages")).unwrap();
        std::fs::write(
            dir.join("src/icons/discord.tsx"),
            "export function IconDiscord() {\n  return null\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/icons/index.ts"),
            "export * from './discord'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/pages/a.tsx"),
            "import { IconDiscord } from '../icons'\nexport const A = () => IconDiscord()\n",
        )
        .unwrap();
        let index =
            Arc::new(crate::index::build_with_options(&dir, false, 1 << 20).expect("index"));
        let backend =
            LiveBackend::new(dir.clone()).with_syntax_index(Arc::new(move || Some(index.clone())));
        let r = backend.references("IconDiscord", None, None, false);
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        let note = r.note.clone().unwrap();
        assert!(
            note.contains("未启动语言服务器") && note.contains("转导出"),
            "{note}"
        );
        let got: Vec<String> = r
            .value
            .iter()
            .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
            .collect();
        assert_eq!(
            got,
            vec![
                "src/icons/discord.tsx:1",
                "src/pages/a.tsx:1",
                "src/pages/a.tsx:2"
            ]
        );
        assert!(!backend.pool.is_resident(Language::TypeScript));
        assert_eq!(
            crate::refs_render::complete_source(r.confidence, r.note.as_deref()),
            Some("索引：定义文件 + 全部导入方及转导出链")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_note_mentions_kotlin_only_for_kotlin() {
        assert!(empty_note(Language::Kotlin, "引用").contains("Android"));
        for lang in [
            Language::Go,
            Language::TypeScript,
            Language::Python,
            Language::Rust,
        ] {
            let n = empty_note(lang, "引用");
            assert!(!n.contains("Kotlin") && !n.contains("Android"), "{n}");
        }
        assert!(empty_note(Language::Go, "引用").contains("go.mod"));
    }
}

#[cfg(test)]
mod index_first_tests {
    use super::*;

    fn repo(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("astrolabe-lb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    fn index_of(dir: &Path) -> SyntaxIndex {
        let index = Arc::new(crate::index::build_with_options(dir, false, 1 << 20).expect("index"));
        Arc::new(move || Some(index.clone()))
    }

    /// Backend whose discovery finds no server at all (empty PATH).
    fn serverless(dir: &Path) -> LiveBackend {
        let pool = LspPool::from_discovery_and_transport(
            Discovery::new(""),
            StdioSpawn::new(dir),
            dir.to_path_buf(),
            LspPoolConfig::default(),
        );
        let mut backend = LiveBackend::new(dir).with_syntax_index(index_of(dir));
        backend.pool = Arc::new(pool);
        backend
    }

    fn at(r: &Precise<Vec<Location>>) -> Vec<String> {
        r.value
            .iter()
            .map(|l| {
                format!(
                    "{}:{}:{}",
                    l.path.as_str(),
                    l.range.start.line + 1,
                    l.range.start.character
                )
            })
            .collect()
    }

    const GO: &[(&str, &str)] = &[
        ("go.mod", "module example.com/m\n"),
        (
            "service/channel.go",
            "package service\n\n// ShouldDisableChannel decides.\nfunc ShouldDisableChannel() bool { return helper() }\n\nfunc helper() bool { return false }\n\nvar limit = 3\n",
        ),
        (
            "controller/relay.go",
            "package controller\n\nimport (\n\tsvc \"example.com/m/service\"\n)\n\ntype T struct{ limit int }\n\nfunc (t *T) Area() int { return t.limit }\n\nfunc R(limit int) {\n\t_ = svc.ShouldDisableChannel()\n\tx := T{limit: limit}\n\t_ = x.Area()\n\tconfig := 1\n\t_ = config\n}\n\nvar config = 2\n",
        ),
    ];

    /// Go: package-qualified and same-package names are answered from the
    /// index; gopls is never started (it is on PATH on the box, so a start
    /// would show as resident).
    #[test]
    fn go_definition_is_index_first_and_starts_no_server() {
        let dir = repo("deffirst", GO);
        let backend = LiveBackend::new(dir.clone()).with_syntax_index(index_of(&dir));
        // `svc.ShouldDisableChannel()` (aliased import), by position.
        let r = backend.definition("controller/relay.go", None, Some(11), Some(10));
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        assert_eq!(at(&r), vec!["service/channel.go:4:5"]);
        assert!(r.note.as_deref().unwrap().contains("未启动语言服务器"));
        // Same package, unqualified.
        let r = backend.definition("service/channel.go", None, Some(3), Some(44));
        assert_eq!(at(&r), vec!["service/channel.go:6:5"], "{r:?}");
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        // By name only: declared in the file.
        let r = backend.definition("service/channel.go", Some("limit"), None, None);
        assert_eq!(at(&r), vec!["service/channel.go:8:4"], "{r:?}");
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        // By name only: first use resolves through the import.
        let r = backend.definition(
            "controller/relay.go",
            Some("ShouldDisableChannel"),
            None,
            None,
        );
        assert_eq!(at(&r), vec!["service/channel.go:4:5"], "{r:?}");
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        assert!(!backend.pool.is_resident(Language::Go));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cases only the server can decide: a method through a value, a
    /// parameter, a struct literal key, a local `:=` shadowing a package var.
    #[test]
    fn go_index_defers_on_members_locals_and_keys() {
        let dir = repo("defdefer", GO);
        let backend = LiveBackend::new(dir.clone()).with_syntax_index(index_of(&dir));
        let rel = RelPath::new("controller/relay.go");
        let pos = |line, character| SymbolAt::Position(Position { line, character });
        // x.Area()
        assert!(backend
            .index_definition(Language::Go, &rel, pos(13, 7))
            .is_none());
        // T{limit: limit}: the key, then the parameter.
        assert!(backend
            .index_definition(Language::Go, &rel, pos(12, 8))
            .is_none());
        assert!(backend
            .index_definition(Language::Go, &rel, pos(12, 16))
            .is_none());
        // `_ = config` after `config := 1`, package-level `config` exists.
        assert!(backend
            .index_definition(Language::Go, &rel, pos(15, 6))
            .is_none());
        // Receiver `t` in `t.limit`.
        assert!(backend
            .index_definition(Language::Go, &rel, pos(8, 30))
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const JAVA: &[(&str, &str)] = &[
        (
            "src/a/Foo.java",
            "package a;\n\npublic class Foo {\n    public int bar() { return 1; }\n}\n",
        ),
        (
            "src/b/Use.java",
            "package b;\n\nimport a.Foo;\n\npublic class Use {\n    int run() {\n        Foo f = new Foo();\n        return f.bar() + helper();\n    }\n    int helper() { return 2; }\n}\n",
        ),
    ];

    /// No language server installed: references and definitions come from
    /// the index with a single short hint line, not `Unknown` + install
    /// block.
    #[test]
    fn missing_server_answers_from_the_index() {
        let dir = repo("noserver", JAVA);
        let backend = serverless(&dir);
        let r = backend.references("Foo", Some("src/b/Use.java"), None, false);
        assert_ne!(r.confidence, Confidence::Unknown, "{r:?}");
        let got = at(&r);
        assert!(
            got.iter().any(|l| l.starts_with("src/a/Foo.java:3")),
            "{got:?}"
        );
        assert!(
            got.iter().any(|l| l.starts_with("src/b/Use.java:7")),
            "{got:?}"
        );
        let note = r.note.clone().unwrap();
        assert!(
            note.contains("索引作答") && note.contains("jdtls 未安装"),
            "{note}"
        );
        assert_eq!(note.lines().count(), 2, "{note}");
        assert!(!note.contains("no language server available"), "{note}");
        // Method through a value: not index-first, server missing → the
        // index's same-name declarations.
        let r = backend.definition("src/b/Use.java", None, Some(7), Some(17));
        assert_eq!(r.confidence, Confidence::Syntactic, "{r:?}");
        assert_eq!(at(&r), vec!["src/a/Foo.java:4:0"]);
        assert!(r.note.as_deref().unwrap().contains("未安装"), "{r:?}");
        // Index-first is Go only; without a server Java still gets the
        // same-name declarations, origin file first.
        let r = backend.definition("src/b/Use.java", None, Some(7), Some(25));
        assert_eq!(r.confidence, Confidence::Syntactic, "{r:?}");
        assert_eq!(at(&r), vec!["src/b/Use.java:10:0"]);
        assert!(backend
            .index_definition(
                Language::Java,
                &RelPath::new("src/b/Use.java"),
                SymbolAt::Name("helper")
            )
            .is_none());
        // Nothing in the index either: the old Unknown + install hint.
        let r = backend.references("NoSuchThing", Some("src/b/Use.java"), None, false);
        assert_eq!(r.confidence, Confidence::Unknown, "{r:?}");
        assert!(r.note.unwrap().contains("no language server available"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qualifier_and_local_detection() {
        assert!(matches!(qualifier("a.b", 2), Qualifier::Selector("a")));
        assert!(matches!(qualifier("x.y.b", 4), Qualifier::Member));
        assert!(matches!(qualifier("p->b", 3), Qualifier::Member));
        assert!(matches!(qualifier("A::b", 3), Qualifier::Member));
        assert!(matches!(qualifier("f(b)", 2), Qualifier::None));
        let l = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Java local, C param, Python param, TS let.
        assert!(maybe_local(
            &l(&["void f() {", "  Config cfg = x;", "  use(cfg);"]),
            0,
            2,
            "cfg",
            false
        ));
        assert!(maybe_local(
            &l(&["int f(int n) {", "  return n;"]),
            0,
            1,
            "n",
            false
        ));
        assert!(maybe_local(
            &l(&["def f(a, b):", "    return a"]),
            0,
            1,
            "a",
            false
        ));
        assert!(maybe_local(
            &l(&["function f() {", "  let v: T = 1;", "  g(v)"]),
            0,
            2,
            "v",
            false
        ));
        // Plain uses are not bindings.
        assert!(!maybe_local(
            &l(&["void f() {", "  return helper(LIMIT);"]),
            0,
            1,
            "LIMIT",
            false
        ));
        assert!(!maybe_local(
            &l(&["void f() {", "  x = obj.cfg;"]),
            0,
            1,
            "cfg",
            false
        ));
        assert!(!maybe_local(
            &l(&["func f() {", "\treturn Limit + 1"]),
            0,
            1,
            "Limit",
            true
        ));
    }

    #[test]
    fn strings_and_comments_are_not_identifiers() {
        let go = Language::Go;
        let l = "\tx = append(x, \"Name\") // Name";
        assert!(in_string_or_comment(l, l.find("Name").unwrap(), go));
        assert!(in_string_or_comment(l, l.rfind("Name").unwrap(), go));
        assert!(!in_string_or_comment(l, l.find("append").unwrap(), go));
        let l = "s := `a ${Name}` + Name";
        assert!(in_string_or_comment(l, l.find("Name").unwrap(), go));
        assert!(!in_string_or_comment(l, l.rfind("Name").unwrap(), go));
        let l = "x = 'it\\'s' + Name  # Name";
        assert!(!in_string_or_comment(
            l,
            l.find("Name").unwrap(),
            Language::Python
        ));
        assert!(in_string_or_comment(
            l,
            l.rfind("Name").unwrap(),
            Language::Python
        ));
        // Rust lifetimes are not quotes.
        let l = "fn f<'a>(x: &'a Name) {}";
        assert!(!in_string_or_comment(
            l,
            l.find("Name").unwrap(),
            Language::Rust
        ));
    }

    #[test]
    fn hint_line_is_short_and_marker_free() {
        let e = LspError::Unavailable(
            Language::Java,
            "no language server available: brew install jdtls\nsecond line\nthird".into(),
        );
        let line = missing_server_line(Language::Java, &e);
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(
            line.contains("brew install jdtls") && !line.contains("no language server available")
        );
        let e = LspError::Startup(Language::Go, "x".repeat(500));
        assert!(missing_server_line(Language::Go, &e).chars().count() < HINT_CHARS + 40);
    }
}

/// Index-first `goto_definition` against the language server (batch 7).
/// Ignored: needs a server and a corpus. `DEF_INDEX_REPO=/abs/repo`,
/// `DEF_INDEX_EXT=.go`, `DEF_INDEX_N=300`. Prints a summary line.
#[cfg(test)]
mod def_index_harness {
    use super::*;

    const KEYWORDS: &[&str] = &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
        "nil",
        "true",
        "false",
        "err",
        "self",
        "this",
        "new",
        "class",
        "public",
        "private",
        "static",
        "void",
        "int",
        "string",
        "bool",
        "def",
        "let",
        "fn",
        "pub",
        "mut",
        "impl",
        "use",
        "match",
        "in",
        "not",
        "and",
        "or",
    ];

    #[test]
    #[ignore = "needs a language server and a corpus (DEF_INDEX_REPO)"]
    fn index_definition_vs_server() {
        let Some(root) = std::env::var_os("DEF_INDEX_REPO").map(PathBuf::from) else {
            return;
        };
        let ext = std::env::var("DEF_INDEX_EXT").unwrap_or_else(|_| ".go".into());
        let n: usize = std::env::var("DEF_INDEX_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let calls_only = std::env::var_os("DEF_INDEX_CALLS").is_some();
        let by_name = std::env::var_os("DEF_INDEX_BYNAME").is_some();
        let index = Arc::new(crate::index::build(&root).expect("index"));
        let g = index.graph();
        let mut pool: Vec<(RelPath, u32, u32)> = Vec::new();
        for s in &g.symbols {
            if !matches!(
                s.kind,
                astrolabe_core::SymbolKind::Function | astrolabe_core::SymbolKind::Method
            ) {
                continue;
            }
            let Some(f) = index.file(s.file) else {
                continue;
            };
            let p = f.path.as_str();
            if !p.ends_with(&ext) || p.ends_with("_test.go") {
                continue;
            }
            let Some(lines) = read_lines(&root, p) else {
                continue;
            };
            for li in s.start_line..s.end_line.min(s.start_line + 40) {
                let Some(line) = lines.get(li as usize) else {
                    break;
                };
                let t = line.trim_start();
                if t.starts_with("//") {
                    continue;
                }
                let bytes: Vec<(usize, char)> = line.char_indices().collect();
                let mut k = 0;
                while k < bytes.len() {
                    let (i, c) = bytes[k];
                    if is_ident_char(c)
                        && !c.is_ascii_digit()
                        && (i == 0 || !is_ident_char(line[..i].chars().last().unwrap()))
                    {
                        let end = line[i..]
                            .find(|c: char| !is_ident_char(c))
                            .map_or(line.len(), |e| i + e);
                        let word = &line[i..end];
                        let call = line[end..].starts_with('(');
                        if word.len() >= 3 && !KEYWORDS.contains(&word) && (call || !calls_only) {
                            pool.push((f.path.clone(), li, utf16_col(line, i)));
                        }
                        while k < bytes.len() && bytes[k].0 < end {
                            k += 1;
                        }
                        continue;
                    }
                    k += 1;
                }
            }
        }
        let step = (pool.len() / n.max(1)).max(1);
        let cases: Vec<_> = pool.into_iter().step_by(step).take(n).collect();
        let backend = LiveBackend::new(&root).with_syntax_index(Arc::new({
            let index = Arc::clone(&index);
            move || Some(Arc::clone(&index))
        }));
        let lang = server_language(&root, cases[0].0.as_str()).unwrap();
        // Cold start of the server: what the first goto_definition used to cost.
        let t = std::time::Instant::now();
        let _ = backend.with_server(lang, |_| ());
        let cold = t.elapsed();
        let (mut answered, mut agree, mut deferred) = (0usize, 0usize, 0usize);
        let mut deferred_in_repo = 0usize;
        let (mut t_index, mut t_server) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let mut wrong: Vec<String> = Vec::new();
        for (path, line, character) in &cases {
            let word = identifier_at(
                &root,
                path,
                Position {
                    line: *line,
                    character: *character,
                },
            )
            .unwrap_or_default();
            let target = if by_name {
                SymbolAt::Name(&word)
            } else {
                SymbolAt::Position(Position {
                    line: *line,
                    character: *character,
                })
            };
            let t = std::time::Instant::now();
            let ix = backend.index_definition(lang, path, target);
            t_index += t.elapsed();
            let t = std::time::Instant::now();
            let srv = backend
                .with_server(lang, |s| goto_definition(s, &root, path, target))
                .expect("server");
            t_server += t.elapsed();
            let Some(ix) = ix else {
                deferred += 1;
                if !srv.value.is_empty() {
                    deferred_in_repo += 1;
                    if std::env::var_os("DEF_INDEX_SHOW_DEFERRED").is_some() {
                        let word = identifier_at(
                            &root,
                            path,
                            Position {
                                line: *line,
                                character: *character,
                            },
                        );
                        let text = read_lines(&root, path.as_str())
                            .and_then(|l| l.get(*line as usize).cloned())
                            .unwrap_or_default();
                        println!(
                            "DEFER\t{}:{} {:?}\t{}",
                            path.as_str(),
                            line + 1,
                            word,
                            text.trim()
                        );
                    }
                }
                continue;
            };
            answered += 1;
            let key = |l: &Location| (l.path.as_str().to_string(), l.range.start.line);
            let a: BTreeSet<_> = ix.value.iter().map(key).collect();
            let b: BTreeSet<_> = srv.value.iter().map(key).collect();
            if a == b {
                agree += 1;
            } else if wrong.len() < 15 {
                let word = identifier_at(
                    &root,
                    path,
                    Position {
                        line: *line,
                        character: *character,
                    },
                );
                wrong.push(format!(
                    "{}:{}:{} {:?} index={a:?} server={b:?}",
                    path.as_str(),
                    line + 1,
                    character,
                    word
                ));
            }
        }
        for w in &wrong {
            println!("WRONG\t{w}");
        }
        println!(
            "SUMMARY\tcases={}\tindex_answered={answered}\tagree={agree}\tdeferred={deferred}\tdeferred_server_found={deferred_in_repo}\tserver_cold_start={cold:?}\tindex_total={t_index:?}\tserver_total={t_server:?}",
            cases.len()
        );
        backend.pool.shutdown();
    }
}

#[cfg(test)]
mod diagnostics_source_tests {
    use super::*;

    /// get_diagnostics answers from `go` without starting gopls (on PATH on
    /// the box, so a start would show as resident); `lsp=true` still asks
    /// the server.
    #[test]
    fn go_diagnostics_use_the_toolchain_not_gopls() {
        if crate::diag_toolchain::gopls_daemon_running() || crate::diag_toolchain::lsp_by_default()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("astrolabe-lb-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("p")).unwrap();
        std::fs::write(dir.join("go.mod"), "module ex.com/m\n\ngo 1.21\n").unwrap();
        std::fs::write(
            dir.join("p/a.go"),
            "package p\n\nfunc A() int { return missing }\n",
        )
        .unwrap();
        let backend = LiveBackend::new(dir.clone());
        let r = backend.diagnostics_with("p/a.go", false);
        assert!(!backend.pool.is_resident(Language::Go));
        if r.confidence == Confidence::Exact {
            assert_eq!(r.value.len(), 1, "{r:?}");
            assert!(r.value[0].message.contains("undefined: missing"), "{r:?}");
        } else {
            // No go on PATH: the tree-sitter syntax check.
            assert_eq!(r.confidence, Confidence::Syntactic, "{r:?}");
        }
        // Explicit lsp=true goes to the (absent) server.
        let mut serverless = LiveBackend::new(dir.clone());
        serverless.pool = Arc::new(LspPool::from_discovery_and_transport(
            Discovery::new(""),
            StdioSpawn::new(&dir),
            dir.clone(),
            LspPoolConfig::default(),
        ));
        let r = serverless.diagnostics_with("p/a.go", true);
        assert_eq!(r.confidence, Confidence::Unknown, "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Peak RSS (VmHWM, KiB) of live `name` child processes.
    fn peak_rss_kib(name: &str) -> u64 {
        let mut total = 0;
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let p = e.path();
            // Children of this test process only (other sessions' gopls too).
            let ppid = std::fs::read_to_string(p.join("stat"))
                .ok()
                .and_then(|st| {
                    st.rsplit_once(") ")
                        .and_then(|(_, rest)| rest.split_whitespace().nth(1).map(str::to_string))
                })
                .and_then(|v| v.parse::<u32>().ok());
            if ppid == Some(std::process::id())
                && std::fs::read_to_string(p.join("comm")).is_ok_and(|c| c.trim() == name)
            {
                if let Ok(st) = std::fs::read_to_string(p.join("status")) {
                    total += st
                        .lines()
                        .find(|l| l.starts_with("VmHWM:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                }
            }
        }
        total
    }

    /// Toolchain vs language server for get_diagnostics. Ignored: needs
    /// go + gopls and a corpus. `DIAG_BENCH_REPO=/abs/repo`,
    /// `DIAG_BENCH_FILES=a.go,b_test.go`. Run with
    /// `ASTROLABE_GOPLS_REMOTE=off` so gopls is a child we can measure.
    #[test]
    #[ignore = "needs a toolchain, a language server and a corpus (DIAG_BENCH_REPO)"]
    fn toolchain_vs_server_diagnostics() {
        let Some(root) = std::env::var_os("DIAG_BENCH_REPO").map(PathBuf::from) else {
            return;
        };
        let files: Vec<String> = std::env::var("DIAG_BENCH_FILES")
            .unwrap_or_default()
            .split(',')
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect();
        let backend = LiveBackend::new(&root);
        for f in &files {
            let t = std::time::Instant::now();
            let r = backend.diagnostics_with(f, false);
            println!(
                "TOOLCHAIN\t{f}\t{:?}\t{:?}\tn={}\t{}",
                t.elapsed(),
                r.confidence,
                r.value.len(),
                r.note.unwrap_or_default().replace('\n', " ")
            );
        }
        assert!(!backend.pool.is_resident(Language::Go));
        for f in &files {
            let t = std::time::Instant::now();
            let r = backend.diagnostics_with(f, true);
            println!(
                "SERVER\t{f}\t{:?}\t{:?}\tn={}\tgopls_peak_rss={} MiB",
                t.elapsed(),
                r.confidence,
                r.value.len(),
                peak_rss_kib("gopls") / 1024
            );
        }
        backend.pool.shutdown();
    }
}
