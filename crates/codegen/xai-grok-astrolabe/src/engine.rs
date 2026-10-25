//! In-process Astrolabe engine.
//!
//! One [`AstrolabeEngine`] per (index root, `.gitignore` policy, deny set),
//! shared by every session and subagent that agrees on all three. The engine
//! owns the index and the tool semantics;
//! it speaks plain Rust to `astrolabe-mcp`'s native facade, so no MCP transport
//! and no `astrolabe` child process are involved.
//!
//! Lifecycle: [`AstrolabeEngine::open`] creates the server without indexing;
//! the first query calls `start_indexing` and waits (bounded) for the index.
//! [`AstrolabeEngine::shutdown`] stops the watcher. The process-wide registry
//! keeps engines alive across sessions: re-opening a root is cheap and the
//! index stays warm.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use astrolabe_core::scan::count_files;
use astrolabe_mcp::AstrolabeServer;
use astrolabe_mcp::native::{
    NativeCancel, NativeDiagnostics, NativeFindReferences, NativeFindSymbol, NativeGotoDefinition,
    NativeIndexStatus, NativeResolveContext, NativeSearchCode,
};

use crate::config::{AstrolabeConfig, IndexPersist, LspFallback};
use crate::paths::{DenyPolicy, PathError, PathMap};
use crate::types::{
    Anchor, CancelHandle, DiagnosticsRequest, EngineMeta, EngineOutput, FindReferencesRequest,
    FindSymbolRequest, GotoDefinitionRequest, LanguagesRequest, PreciseOutcome,
    ResolveContextRequest, SearchCodeRequest,
};

/// Engine-level failures that are worth telling the model about.
#[derive(Debug, Clone, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("astrolabe index unavailable for {root}: {message}")]
    IndexFailed { root: String, message: String },
    #[error(
        "astrolabe refuses to index {root}: more than {limit} files. \
         Narrow index_root to a subdirectory, raise [toolset.astrolabe].max_files, \
         or use grep for this workspace."
    )]
    TooManyFiles { root: String, limit: u64 },
}

/// A shared index for one root, one ignore policy and one deny set. Sessions
/// that disagree about `.gitignore` or `DenyReadGlobs` get separate indexes:
/// the admitted file set is fixed at build time, so serving both from one
/// engine would answer one of them wrong.
pub struct AstrolabeEngine {
    root: PathBuf,
    server: Arc<AstrolabeServer>,
    cfg: AstrolabeConfig,
    respect_gitignore: bool,
    /// The deny policy this index was built with. Also filters
    /// [`Self::note_edited`]: a host-side edit of a denied file must not
    /// re-enter the graph the way a live watcher event (already excluded by
    /// the scan gates) cannot.
    deny: DenyPolicy,
    started: AtomicBool,
}

impl std::fmt::Debug for AstrolabeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AstrolabeEngine")
            .field("root", &self.root)
            .field("respect_gitignore", &self.respect_gitignore)
            .field("deny_excludes", &self.deny.engine_excludes())
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EngineKey {
    root: PathBuf,
    respect_gitignore: bool,
    /// Normalized deny excludes (sorted, deduplicated); equal policies map
    /// to one engine, differing policies to separate indexes.
    deny_excludes: Vec<String>,
}

static ENGINES: LazyLock<Mutex<HashMap<EngineKey, Arc<AstrolabeEngine>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CONFIG: OnceLock<AstrolabeConfig> = OnceLock::new();

/// Process-wide engine configuration: `[toolset.astrolabe]` installed by the
/// session at startup, or the environment when nothing was installed.
pub fn process_config() -> &'static AstrolabeConfig {
    CONFIG.get_or_init(AstrolabeConfig::from_env)
}

/// Install the session's `[toolset.astrolabe]` configuration. The first
/// install wins: the registry and `by_kind` are process-wide, so a later
/// session with a different file cannot flip tools out from under the first.
/// Returns `false` when a configuration was already installed.
pub fn install_config(cfg: AstrolabeConfig) -> bool {
    if CONFIG.get().is_some() {
        return false;
    }
    CONFIG.set(cfg).is_ok()
}

/// Tell every already-open engine that `paths` changed on disk. Each engine
/// keeps only the paths under its own root, so callers do not have to know
/// which index owns a file. Roots with no live engine are skipped: there is no
/// index to refresh, and creating one would pay a full build on a single edit.
pub fn note_edited_on_disk(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let open: Vec<Arc<AstrolabeEngine>> = {
        let engines = ENGINES.lock().unwrap_or_else(|e| e.into_inner());
        engines.values().map(Arc::clone).collect()
    };
    for engine in open {
        engine.note_edited(paths);
    }
}

/// Engine for `root`, creating it on first use. The root must already be
/// canonical (see [`root_for_cwd`]). No read-deny policy: see
/// [`engine_for_session`].
pub fn engine_for(root: &Path, cfg: &AstrolabeConfig) -> Result<Arc<AstrolabeEngine>, EngineError> {
    engine_for_session(
        root,
        cfg,
        cfg.respect_gitignore,
        &DenyPolicy::default(),
    )
}

/// [`engine_for`] with the session's `.gitignore` policy and `DenyReadGlobs`.
/// `respect_gitignore` comes from the host's `RespectGitignore` resource, so
/// a session that told `grep` to look at ignored files gets an index that
/// contains them. `deny` becomes part of the engine key and of the scan's
/// `extra_excludes`: denied files are never parsed into this index, and two
/// sessions with different deny sets cannot answer from each other's index.
pub fn engine_for_session(
    root: &Path,
    cfg: &AstrolabeConfig,
    respect_gitignore: bool,
    deny: &DenyPolicy,
) -> Result<Arc<AstrolabeEngine>, EngineError> {
    let key = EngineKey {
        root: root.to_path_buf(),
        respect_gitignore,
        deny_excludes: deny.engine_excludes(),
    };
    let mut engines = ENGINES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(engine) = engines.get(&key) {
        return Ok(Arc::clone(engine));
    }
    let engine = Arc::new(AstrolabeEngine::open(
        root,
        cfg.clone(),
        respect_gitignore,
        deny.clone(),
    )?);
    engines.insert(key, Arc::clone(&engine));
    Ok(engine)
}

/// Index root for a session: the git root above `cwd`, or `cwd` itself when
/// there is no repository (or when `index_root = cwd` is configured).
pub fn root_for_cwd(cwd: &Path, cfg: &AstrolabeConfig) -> PathBuf {
    let cwd = canonical(cwd);
    if cfg.index_root == crate::config::IndexRoot::Cwd {
        return cwd;
    }
    let mut probe = cwd.as_path();
    loop {
        if probe.join(".git").exists() {
            return probe.to_path_buf();
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return cwd,
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| crate::paths::normalize(path))
}

impl AstrolabeEngine {
    fn open(
        root: &Path,
        cfg: AstrolabeConfig,
        respect_gitignore: bool,
        deny: DenyPolicy,
    ) -> Result<Self, EngineError> {
        let root = canonical(root);
        // The upstream MCP binary refuses a root above `max_files` while it
        // resolves one; the in-process facade must apply the same guard, or a
        // fifty-thousand-file tree would be parsed before the first answer.
        // Denied files carry the same scan excludes as the index build, so a
        // tree full of `.env`s is not counted toward the cap either.
        let deny_excludes = deny.engine_excludes();
        let scan = astrolabe_core::scan::ScanOptions {
            respect_gitignore,
            extra_excludes: deny_excludes.clone(),
            ..Default::default()
        };
        let count = count_files(&root, &scan, cfg.max_files.saturating_add(1));
        if count.files > cfg.max_files {
            return Err(EngineError::TooManyFiles {
                root: root.display().to_string(),
                limit: cfg.max_files,
            });
        }
        // The host's LSP manager is the default precise backend, so the
        // engine must not start Astrolabe's own pool in that mode.
        let builtin_lsp = cfg.lsp_fallback == LspFallback::Astrolabe;
        let persist = cfg.persist == IndexPersist::Repo;
        let server = AstrolabeServer::new_native_full(
            root.clone(),
            cfg.watch,
            builtin_lsp,
            persist,
            respect_gitignore,
            deny_excludes,
        );
        Ok(Self {
            root,
            server: Arc::new(server),
            cfg,
            respect_gitignore,
            deny,
            started: AtomicBool::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The ignore policy this index was built with.
    pub fn respects_gitignore(&self) -> bool {
        self.respect_gitignore
    }

    /// `true` when this process holds the exclusive `<root>/.astrolabe/`
    /// indexer lock and may write the parse cache. A follower (another engine
    /// for the same root already elected) serves tools from its own in-memory
    /// graph and may read the leader's store, but never writes it.
    pub fn is_leader(&self) -> bool {
        self.server.native_is_leader()
    }

    pub fn config(&self) -> &AstrolabeConfig {
        &self.cfg
    }

    /// Start indexing on first use. Subsequent calls are no-ops.
    fn ensure_started(&self) {
        if !self.started.swap(true, Ordering::SeqCst) {
            self.server.start_indexing();
        }
    }

    fn await_ready(&self) -> NativeIndexStatus {
        self.ensure_started();
        if self.cfg.ready_timeout.is_zero() {
            return self.server.native_index_status();
        }
        self.server.native_wait_ready(self.cfg.ready_timeout)
    }

    fn failed_status(&self, status: &NativeIndexStatus) -> Option<EngineError> {
        (status.state == "failed").then(|| EngineError::IndexFailed {
            root: self.root.display().to_string(),
            message: status
                .message
                .clone()
                .unwrap_or_else(|| "index build failed".to_string()),
        })
    }

    /// `search_code`. Blocking; callers run it on a blocking thread.
    pub fn search_code(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: SearchCodeRequest,
    ) -> Result<EngineOutput, EngineError> {
        self.search_code_with_cancel(paths, deny, req, None)
    }

    /// `search_code` that stops at the next file when `cancel` is flipped.
    pub fn search_code_cancellable(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: SearchCodeRequest,
        cancel: &CancelHandle,
    ) -> Result<EngineOutput, EngineError> {
        self.search_code_with_cancel(paths, deny, req, Some(cancel))
    }

    fn search_code_with_cancel(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: SearchCodeRequest,
        cancel: Option<&CancelHandle>,
    ) -> Result<EngineOutput, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        let mut exclude = req.exclude.unwrap_or_default();
        exclude.extend(deny.engine_excludes());
        let mut native = NativeSearchCode::new(req.query);
        native.regex = req.regex;
        native.path_filter = match req.path.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() && path != "." => Some(paths.engine_rel(path)?),
            _ => None,
        };
        native.glob = req.glob.filter(|g| !g.trim().is_empty());
        native.exclude = (!exclude.is_empty()).then_some(exclude);
        native.case_sensitive = req.case_sensitive;
        native.context_lines = req.context_lines.map(|n| n.min(10));
        native.aggregate = req.aggregate;
        native.skip_comments = req.skip_comments;
        native.offset = req.offset;
        native.budget_tokens = self.cfg.budget_tokens;
        let out = match cancel {
            Some(cancel) => self
                .server
                .native_search_code_cancellable(native, &NativeCancel::from_flag(cancel.flag())),
            None => self.server.native_search_code(native),
        };
        Ok(self.finish(paths, deny, out))
    }

    /// `find_symbol`. Blocking.
    pub fn find_symbol(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: FindSymbolRequest,
    ) -> Result<EngineOutput, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        let mut native = NativeFindSymbol::new(req.query);
        native.include_body = req.include_body;
        native.kind = req.kind;
        native.path_filter = match req.path.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() && path != "." => Some(paths.engine_rel(path)?),
            _ => None,
        };
        native.depth = req.depth.unwrap_or(0);
        native.offset = req.offset;
        native.budget_tokens = self.cfg.budget_tokens;
        let out = self.server.native_find_symbol(native);
        Ok(self.finish(paths, deny, out))
    }

    /// `resolve_context`. Blocking.
    pub fn resolve_context(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: ResolveContextRequest,
    ) -> Result<EngineOutput, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        let mut native = NativeResolveContext::new(req.task);
        native.path_filter = match req.path.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() && path != "." => Some(paths.engine_rel(path)?),
            _ => None,
        };
        native.offset = req.offset;
        native.budget_tokens = self.cfg.budget_tokens;
        let out = self.server.native_resolve_context(native);
        Ok(self.finish(paths, deny, out))
    }

    /// `get_languages`. Blocking; the answer only needs the scan report.
    /// `host_lsp` is the session's registered language servers (from
    /// `lsp.json`); when non-empty the LSP column reports that pool
    /// instead of Astrolabe's PATH probe.
    pub fn get_languages(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        _req: LanguagesRequest,
        host_lsp: &[crate::types::HostLspServer],
    ) -> Result<EngineOutput, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        let host: Vec<astrolabe_mcp::HostLspServer> = host_lsp
            .iter()
            .map(|s| astrolabe_mcp::HostLspServer {
                name: s.name.clone(),
                command: s.command.clone(),
                languages: s.languages.clone(),
                running: s.running,
            })
            .collect();
        let out = self
            .server
            .native_get_languages_with_budget(self.cfg.budget_tokens, host);
        Ok(self.finish(paths, deny, out))
    }

    /// `find_references`. Blocking; index-first, with `needs_host_lsp` set
    /// when only a language server can answer.
    pub fn find_references(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: FindReferencesRequest,
        host_lsp: bool,
    ) -> Result<PreciseOutcome, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        let anchor = self.anchor_for(
            paths,
            req.path.as_deref(),
            req.line,
            req.character,
            Some(&req.symbol),
        )?;
        let mut native = NativeFindReferences::new(req.symbol.clone());
        native.path = match req.path.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() && path != "." => Some(paths.engine_rel(path)?),
            _ => anchor.as_ref().map(|a| a.engine_path.clone()),
        };
        // Model coordinates are 1-based; the engine takes 0-based ones.
        native.line = req
            .line
            .map(|line| line.saturating_sub(1))
            .or_else(|| anchor.as_ref().map(|a| a.line));
        native.character = req
            .character
            .map(|column| column.saturating_sub(1))
            .or_else(|| anchor.as_ref().map(|a| a.character));
        native.dry_run_rename = req.dry_run_rename.clone();
        native.offset = Some(req.offset);
        native.budget_tokens = self.cfg.budget_tokens;
        let out = self.server.native_find_references(native);
        let unknown = confidence_of(&out.structured) == Some("unknown");
        let output = self.finish(paths, deny, out);
        Ok(PreciseOutcome {
            needs_host_lsp: unknown && host_lsp,
            anchor: unknown.then_some(anchor).flatten(),
            candidates: Vec::new(),
            unknown_reason: unknown.then(|| {
                "the index could not decide this query; exact references need a language server"
                    .to_string()
            }),
            output,
        })
    }

    /// `goto_definition`. Blocking; returns declarations from the index or
    /// asks the host server, depending on what the index can decide.
    pub fn goto_definition(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: GotoDefinitionRequest,
        host_lsp: bool,
    ) -> Result<PreciseOutcome, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        // A path pins the search; a name alone is answered from the index
        // (possibly several candidates) before any server is involved.
        let mut candidates: Vec<Anchor> = Vec::new();
        let mut anchor = None;
        if let Some(raw) = req
            .path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != ".")
        {
            anchor = self.anchor_for(
                paths,
                Some(raw),
                req.line,
                req.character,
                req.symbol.as_deref(),
            )?;
        } else if let Some(symbol) = req
            .symbol
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            candidates = self.symbol_anchors(symbol, None, 5);
            if candidates.len() == 1 {
                anchor = candidates.pop();
            }
        }

        let Some(anchor) = anchor else {
            if candidates.is_empty() {
                let text = "goto_definition needs `path` (with `line`) or `symbol`. \
                            Use find_symbol to locate an indexed symbol first."
                    .to_string();
                return Ok(PreciseOutcome {
                    output: EngineOutput {
                        text,
                        meta: EngineMeta {
                            is_error: true,
                            confidence: Some("unknown".to_string()),
                            ..EngineMeta::default()
                        },
                    },
                    needs_host_lsp: false,
                    anchor: None,
                    candidates,
                    unknown_reason: None,
                });
            }
            return Ok(PreciseOutcome {
                output: crate::precise::render_candidates(
                    paths,
                    deny,
                    req.symbol.as_deref(),
                    &candidates,
                ),
                needs_host_lsp: false,
                anchor: None,
                candidates,
                unknown_reason: None,
            });
        };

        let mut native = NativeGotoDefinition {
            path: anchor.engine_path.clone(),
            symbol: req.symbol.clone().filter(|s| !s.trim().is_empty()),
            line: Some(anchor.line),
            character: Some(anchor.character),
            offset: Some(req.offset),
            budget_tokens: self.cfg.budget_tokens,
        };
        if req.line.is_some() {
            // Explicit position wins over the anchor's symbol snap.
            native.line = req.line.map(|line| line.saturating_sub(1));
            native.character = req
                .character
                .map(|column| column.saturating_sub(1))
                .or(Some(anchor.character));
        }
        let out = self.server.native_goto_definition(native);
        let unknown = confidence_of(&out.structured) == Some("unknown");
        let output = self.finish(paths, deny, out);
        Ok(PreciseOutcome {
            needs_host_lsp: unknown && host_lsp,
            anchor: Some(anchor),
            candidates: Vec::new(),
            unknown_reason: unknown.then(|| {
                "the index could not resolve this definition; a language server can".to_string()
            }),
            output,
        })
    }

    /// `get_diagnostics` from the engine's own parser/toolchain (and, in
    /// `lsp_fallback = astrolabe`, its built-in server).
    pub fn get_diagnostics(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        req: DiagnosticsRequest,
    ) -> Result<EngineOutput, EngineError> {
        let status = self.await_ready();
        if let Some(err) = self.failed_status(&status) {
            return Err(err);
        }
        let mut native = NativeDiagnostics::new(paths.engine_rel(&req.path)?);
        native.severity = req.severity;
        native.lsp = Some(self.cfg.lsp_fallback == LspFallback::Astrolabe);
        // The host owns build and test commands: a diagnostics call must not
        // start `cargo check` / `go test -c` / `tsc` on its own.
        native.toolchain = false;
        native.budget_tokens = self.cfg.budget_tokens;
        let out = self.server.native_get_diagnostics(native);
        Ok(self.finish(paths, deny, out))
    }

    /// Absolute on-disk path of a model-supplied file path, for a host LSP.
    pub fn absolute_path(&self, paths: &PathMap, input: &str) -> Result<PathBuf, EngineError> {
        Ok(paths.absolute(input)?)
    }

    /// 0-based anchor from an explicit position or a symbol name.
    fn anchor_for(
        &self,
        paths: &PathMap,
        path: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
        symbol: Option<&str>,
    ) -> Result<Option<Anchor>, EngineError> {
        if let Some(raw) = path.map(str::trim).filter(|p| !p.is_empty() && *p != ".") {
            let engine_path = paths.engine_rel(raw)?;
            let absolute = self.root.join(&engine_path);
            let Some(line1) = line.filter(|line| *line >= 1) else {
                return Ok(symbol.filter(|s| !s.trim().is_empty()).and_then(|s| {
                    self.symbol_anchors(s, Some(&engine_path), 1)
                        .into_iter()
                        .next()
                }));
            };
            let line0 = line1 - 1;
            let character0 = match character.filter(|column| *column >= 1) {
                Some(column) => column - 1,
                None => crate::precise::snap_character(&absolute, line0, symbol).unwrap_or(0),
            };
            return Ok(Some(Anchor {
                engine_path,
                absolute,
                line: line0,
                character: character0,
            }));
        }
        let Some(symbol) = symbol.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        Ok(self.symbol_anchors(symbol, None, 1).into_iter().next())
    }

    /// Index declarations named `symbol`, as host-queryable anchors.
    fn symbol_anchors(&self, symbol: &str, path_filter: Option<&str>, limit: usize) -> Vec<Anchor> {
        self.ensure_started();
        self.server
            .native_symbol_anchors(symbol, path_filter, limit)
            .into_iter()
            .map(|anchor| Anchor {
                absolute: self.root.join(&anchor.path),
                engine_path: anchor.path,
                line: anchor.line,
                character: anchor.character,
            })
            .collect()
    }

    /// Index status without blocking on a build.
    pub fn status(&self) -> NativeIndexStatus {
        self.ensure_started();
        self.server.native_index_status()
    }

    /// Apply edits the host just made, so the next query sees them without
    /// waiting for the watcher. Paths are absolute; paths outside the root are
    /// ignored, and so is any path this index was built to deny — the watcher
    /// drops those through the scan gates, and this hook must agree.
    pub fn note_edited(&self, edited: &[PathBuf]) -> bool {
        if edited.is_empty() {
            return true;
        }
        self.ensure_started();
        let rels: Vec<String> = edited
            .iter()
            .map(|path| crate::paths::canonical_or_normalize(path))
            .filter_map(|path| crate::paths::relative_posix(&self.root, &path))
            .filter(|rel| rel != "." && !rel.starts_with(".."))
            .filter(|rel| !self.deny.denies(rel))
            .collect();
        if rels.is_empty() {
            return true;
        }
        self.server.native_note_edited(&rels)
    }

    /// Stop watching and release the indexer lock. The cached store stays on
    /// disk.
    pub fn shutdown(&self) {
        self.server.stop_watching();
        self.server.shutdown_indexing(Duration::from_secs(5));
    }

    fn finish(
        &self,
        paths: &PathMap,
        deny: &DenyPolicy,
        out: astrolabe_mcp::native::NativeOutput,
    ) -> EngineOutput {
        let outcome = paths.rewrite(&out.text, deny);
        let mut text = crate::render::finalize(
            outcome.text,
            outcome.hidden,
            out.is_error,
            self.cfg.budget_bytes,
        );
        let meta = meta_of(&out.structured, out.is_error);
        if meta.is_error && text.trim().is_empty() {
            text = "(astrolabe returned an empty error result)".to_string();
        }
        EngineOutput { text, meta }
    }
}

fn confidence_of(structured: &serde_json::Value) -> Option<&str> {
    structured
        .get("confidence")
        .and_then(serde_json::Value::as_str)
}

fn meta_of(structured: &serde_json::Value, is_error: bool) -> EngineMeta {
    let get = |key: &str| structured.get(key).and_then(serde_json::Value::as_u64);
    EngineMeta {
        total: get("total").or_else(|| get("matches")),
        shown: get("shown"),
        omitted: get("omitted"),
        offset: get("offset"),
        next_offset: get("next_offset"),
        generation: get("generation"),
        confidence: confidence_of(structured).map(str::to_owned),
        truncated: structured
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        is_error,
    }
}

/// Shut every cached engine down (tests, and process exit).
pub fn shutdown_all() {
    let engines: Vec<Arc<AstrolabeEngine>> = {
        let mut map = ENGINES.lock().unwrap_or_else(|e| e.into_inner());
        map.drain().map(|(_, engine)| engine).collect()
    };
    for engine in engines {
        engine.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_walks_up_to_the_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        let cfg = AstrolabeConfig::default();
        assert_eq!(root_for_cwd(&deep, &cfg), canonical(&root));
    }

    #[test]
    fn root_falls_back_to_cwd_without_git() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = AstrolabeConfig::default();
        assert_eq!(root_for_cwd(&dir, &cfg), canonical(&dir));
    }

    #[test]
    fn cwd_index_root_pins_the_session_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let cfg = AstrolabeConfig {
            index_root: crate::config::IndexRoot::Cwd,
            ..AstrolabeConfig::default()
        };
        assert_eq!(root_for_cwd(&sub, &cfg), canonical(&sub));
    }
}
