//! JSON-RPC 2.0 client over an LSP stdio subprocess.
//!
//! [`LspTransport`] is the wire client. [`StdioSpawn`] implements
//! [`super::pool::SpawnServer`] so the pool can store `Box<dyn LanguageServer>`
//! without naming this module. Document lifetime and LSP method mapping live
//! on [`LspSession`].
//!
//! # Framing
//!
//! LSP is **not** newline-delimited JSON. Each message is
//! `Content-Length: N\r\n\r\n` plus `N` bytes of UTF-8 JSON. A single `read`
//! can yield a partial header, a header with a truncated body, or several
//! complete messages. A byte-buffer state machine ingests whatever arrived,
//! pulls zero or more complete bodies, and leaves the rest.
//!
//! # Threads
//!
//! `astrolabe-core` is synchronous; there is no tokio here. Three OS threads
//! plus the caller's:
//!
//! * **stdout reader** — frames messages and dispatches by kind: responses
//!   complete a pending `id`, notifications are stored (diagnostics) or logged,
//!   server-initiated requests are answered immediately.
//! * **stderr drainer** — reads the pipe to completion. A language server
//!   that fills the stderr buffer deadlocks if nobody consumes it.
//! * **caller threads** — `request` is `&self` and blocks on a oneshot until
//!   the matching `id` arrives, the timeout fires, or the process dies.
//!
//! Writes to the child's stdin are serialized with a mutex so frames cannot
//! interleave. Concurrent in-flight requests are matched by `id`, not by
//! arrival order.
//!
//! # Lifecycle
//!
//! [`LspTransport::connect`] spawns, then runs `initialize` / `initialized`.
//! [`LspTransport::shutdown`] sends `shutdown` / `exit` and reaps the child.
//! Drop always kills whatever is left so a forgotten server cannot leak.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memmem;
use serde::Serialize;
use serde_json::{json, Value};

use super::pool::SpawnServer;
use super::{
    Diagnostic, FileChange, FileChangeKind, LanguageServer, Location, LspError, Position, Range,
    ServerSpec, Severity, TextEdit, WorkspaceEdit,
};
use crate::types::{Language, RelPath};

/// Default per-request timeout. Pool and tests override this.
///
/// Aligned with Serena's DEFAULT_LS_REQUEST_TIMEOUT (300s). Pyright and other LSPs on a cold
/// workspace before `textDocument/definition` / `references` answer. Override
/// with `ASTROLABE_LSP_TIMEOUT` (seconds) on [`StdioSpawn::new`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const LIFE_RUNNING: u8 = 0;
const LIFE_SHUTDOWN: u8 = 1;
const LIFE_DEAD: u8 = 2;

/// JSON-RPC / LSP stdio client. `Send + Sync`; the pool holds `Arc<Self>`.
pub struct LspTransport {
    shared: Arc<Shared>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    spec: ServerSpec,
    pid: u32,
    timeout: Duration,
    child: Mutex<Child>,
    /// Frames queued for the stdin writer thread. A bounded channel keeps a
    /// wedged server from making callers block inside `write(2)`: requests
    /// time out on their own clock instead.
    stdin: Mutex<Option<mpsc::SyncSender<Vec<u8>>>>,
    /// Milliseconds since `started` (+1) at which the writer thread began its
    /// current `write_all`; 0 while idle. A write that outlives the request
    /// timeout means the server stopped reading its stdin.
    write_started: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, LspError>>>>,
    next_id: AtomicU64,
    life: AtomicU8,
    diagnostics: Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>,
    diag_versions: Mutex<HashMap<String, i64>>,
    notifications: Mutex<Vec<(String, Value)>>,
    initialize_result: Mutex<Option<Value>>,
    workspace_folders: Mutex<Option<Value>>,
    stderr_bytes: AtomicU64,
    /// Last few KiB of the server's stderr, so a server that exits during
    /// `initialize` can say why (missing Gemfile.lock, no tsserver, ...)
    /// instead of a bare "exited unexpectedly".
    stderr_tail: Mutex<Vec<u8>>,
    /// Server notifications handed to another thread instead of the log
    /// (Volar 3.x `tsserver/request`, answered by the Vue bridge).
    relays: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    cwd: Option<PathBuf>,
    /// Whether the server has ever announced background work.
    ///
    /// Distinguishes "finished indexing" from "has not started yet": both
    /// show zero active tokens, but only the first means the answers are
    /// trustworthy.
    saw_progress: AtomicBool,
    /// Work-done progress tokens the server has begun but not ended.
    ///
    /// A server that is still indexing answers queries with an empty result
    /// rather than an error, which is indistinguishable from a genuine "no
    /// results" unless we track this.
    active_progress: Mutex<HashSet<String>>,
    ready_confirmed: AtomicBool,
    /// Roslyn (C# / VB) has not yet sent `workspace/projectInitializationComplete`.
    ///
    /// Roslyn loads the solution / projects after `solution/open`, and its
    /// `$/progress` tokens come and go several times while it does, so the
    /// generic quiet-period check can call it ready before the project model
    /// exists and cross-file references come back partial. This is Roslyn's
    /// own "projects loaded" signal. Bounded by [`ROSLYN_INIT_CAP`] so an
    /// older build that never sends it costs a bounded wait, not a hang.
    roslyn_init_pending: AtomicBool,
    /// When the process was spawned. Servers that never report progress are
    /// treated as warming up for [`SETTLE_WINDOW`] after this.
    started: Instant,
}

impl std::fmt::Debug for LspTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspTransport")
            .field("language", &self.shared.spec.language)
            .field("pid", &self.shared.pid)
            .finish()
    }
}

impl LspTransport {
    /// Spawn `spec.command` with piped stdio. Does not handshake.
    pub fn spawn(spec: &ServerSpec, timeout: Duration) -> Result<Self, LspError> {
        Self::start(spec, None, timeout)
    }

    /// Spawn with `cwd` as the child's working directory.
    pub fn spawn_in(
        spec: &ServerSpec,
        cwd: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        Self::start(spec, Some(cwd.as_ref()), timeout)
    }

    /// Spawn in `root` and complete `initialize` / `initialized`.
    pub fn connect(
        spec: &ServerSpec,
        root: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        Self::connect_with(spec, root, timeout, None)
    }

    /// [`connect`] with optional LSP `initializationOptions`.
    pub fn connect_with(
        spec: &ServerSpec,
        root: impl AsRef<Path>,
        timeout: Duration,
        initialization_options: Option<Value>,
    ) -> Result<Self, LspError> {
        let root = root.as_ref();
        let transport = Self::spawn_in(spec, root, timeout)?;
        match transport.initialize_with(root, initialization_options) {
            Ok(_) => Ok(transport),
            Err(err) => {
                transport.force_stop();
                Err(transport.explain_startup_error(err))
            }
        }
    }

    fn start(spec: &ServerSpec, cwd: Option<&Path>, timeout: Duration) -> Result<Self, LspError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args);
        if let Some(dir) = cwd.and_then(|root| kotlin_system_path(spec, root)) {
            let _ = std::fs::create_dir_all(&dir);
            cmd.arg("--system-path").arg(dir);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // No console window: an in-process host (Grok Build) must not flash a
        // terminal per language server on Windows.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        // Own process group: wrappers (typescript-language-server -> tsserver,
        // JVM launchers, dotnet) spawn grandchildren that inherit our pipes.
        // Killing only the direct child leaves them alive and keeps the
        // stdout reader from ever seeing EOF.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
            // Roslyn's design-time builds would otherwise write obj/ into
            // the user's project (see `csharp::msbuild_redirect`).
            if is_roslyn(spec.language) {
                if let Some((key, props)) = super::csharp::msbuild_redirect(cwd) {
                    cmd.env(key, props);
                }
            }
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(LspError::Unavailable(
                    spec.language,
                    spec.install_hint.clone(),
                ));
            }
            Err(err) => return Err(LspError::Startup(spec.language, err.to_string())),
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Startup(spec.language, "child stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Startup(spec.language, "child stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LspError::Startup(spec.language, "child stderr was not piped".into()))?;
        let pid = child.id();
        #[cfg(windows)]
        job::adopt(pid, &child);

        let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Vec<u8>>(STDIN_QUEUE_FRAMES);
        let shared = Arc::new(Shared {
            spec: spec.clone(),
            pid,
            timeout,
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin_tx)),
            write_started: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            life: AtomicU8::new(LIFE_RUNNING),
            diagnostics: Mutex::new(HashMap::new()),
            diag_versions: Mutex::new(HashMap::new()),
            notifications: Mutex::new(Vec::new()),
            saw_progress: AtomicBool::new(false),
            active_progress: Mutex::new(HashSet::new()),
            ready_confirmed: AtomicBool::new(false),
            roslyn_init_pending: AtomicBool::new(is_roslyn(spec.language)),
            started: Instant::now(),
            initialize_result: Mutex::new(None),
            workspace_folders: Mutex::new(None),
            stderr_bytes: AtomicU64::new(0),
            stderr_tail: Mutex::new(Vec::new()),
            relays: Mutex::new(HashMap::new()),
            cwd: cwd.map(Path::to_path_buf),
        });

        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("astrolabe-lsp-stdout".into())
            .spawn(move || read_stdout(stdout, reader_shared))
            .map_err(|err| LspError::Startup(spec.language, err.to_string()))?;

        let writer_shared = Arc::clone(&shared);
        let writer = thread::Builder::new()
            .name("astrolabe-lsp-stdin".into())
            .spawn(move || write_stdin(stdin, stdin_rx, writer_shared))
            .map_err(|err| LspError::Startup(spec.language, err.to_string()))?;

        let stderr_shared = Arc::clone(&shared);
        let stderr_thread = match thread::Builder::new()
            .name("astrolabe-lsp-stderr".into())
            .spawn(move || drain_stderr(stderr, stderr_shared))
        {
            Ok(handle) => handle,
            Err(err) => {
                let transport = Self {
                    shared,
                    reader: Mutex::new(Some(reader)),
                    stderr: Mutex::new(None),
                    writer: Mutex::new(Some(writer)),
                };
                transport.force_stop();
                return Err(LspError::Startup(spec.language, err.to_string()));
            }
        };

        Ok(Self {
            shared,
            reader: Mutex::new(Some(reader)),
            stderr: Mutex::new(Some(stderr_thread)),
            writer: Mutex::new(Some(writer)),
        })
    }

    pub fn language(&self) -> Language {
        self.shared.spec.language
    }

    pub fn spec(&self) -> &ServerSpec {
        &self.shared.spec
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// Working directory the child was spawned in, if one was set.
    pub fn root(&self) -> Option<&Path> {
        self.shared.cwd.as_deref()
    }

    pub fn is_alive(&self) -> bool {
        self.shared.life.load(Ordering::SeqCst) == LIFE_RUNNING
    }

    /// Recent stderr output, trimmed to the last few non-empty lines.
    pub fn stderr_tail(&self) -> String {
        // Give the drain thread a beat to pick up the dying server's last words.
        thread::sleep(Duration::from_millis(50));
        let tail = lock(&self.shared.stderr_tail);
        summarize_stderr(&String::from_utf8_lossy(&tail))
    }

    /// Turn a bare crash during startup into a [`LspError::Startup`] that
    /// carries the server's own explanation from stderr, when it printed one.
    fn explain_startup_error(&self, err: LspError) -> LspError {
        match err {
            LspError::Crashed => {
                let tail = self.stderr_tail();
                if tail.is_empty() {
                    LspError::Crashed
                } else {
                    LspError::Startup(
                        self.shared.spec.language,
                        format!("exited during initialize: {tail}"),
                    )
                }
            }
            other => other,
        }
    }

    /// Route every `method` notification from the server to `tx` instead of
    /// the notification log. Used by the Vue 3.x bridge, which must answer
    /// Volar's `tsserver/request` from a separate thread so the reader never
    /// blocks on another server.
    pub fn set_notification_relay(&self, method: &str, tx: mpsc::Sender<Value>) {
        lock(&self.shared.relays).insert(method.to_string(), tx);
    }

    /// `serverInfo.version` from the `initialize` result, when reported.
    pub fn server_version(&self) -> Option<String> {
        lock(&self.shared.initialize_result)
            .as_ref()?
            .pointer("/serverInfo/version")?
            .as_str()
            .map(str::to_string)
    }

    /// Bytes the stderr thread has consumed. Used by tests; also a cheap
    /// signal that the drain loop is actually running.
    pub fn stderr_bytes(&self) -> u64 {
        self.shared.stderr_bytes.load(Ordering::Relaxed)
    }

    /// Raw `initialize` result, when a handshake has completed.
    pub fn initialize_result(&self) -> Option<Value> {
        lock(&self.shared.initialize_result).clone()
    }

    /// Latest `textDocument/publishDiagnostics` per document URI.
    /// `version` the server attached to its last publish for `uri`, if any.
    pub fn published_diagnostic_version(&self, uri: &str) -> Option<i64> {
        lock(&self.shared.diag_versions).get(uri).copied()
    }

    pub fn published_diagnostics(&self) -> HashMap<String, Vec<lsp_types::Diagnostic>> {
        lock(&self.shared.diagnostics).clone()
    }

    /// Whether the server has background work outstanding.
    ///
    /// Callers use this to avoid reporting an empty result as authoritative
    /// while the server is still building its index.
    pub fn is_busy(&self) -> bool {
        if roslyn_init_pending(&self.shared) {
            return true;
        }
        if !lock(&self.shared.active_progress).is_empty() {
            return true;
        }
        // A server that has never reported progress may simply not support
        // it. Its first seconds are still a warm-up: answers given then can
        // come from a half-built index, so do not call them authoritative.
        !self.shared.saw_progress.load(Ordering::SeqCst)
            && !self.shared.ready_confirmed.load(Ordering::Acquire)
            && self.shared.started.elapsed() < SETTLE_WINDOW
    }

    /// Poll until no background work is outstanding, or `deadline` elapses.
    ///
    /// Polling rather than waiting on a condvar keeps this off the reader
    /// thread's hot path; indexing takes seconds, so 50 ms granularity costs
    /// nothing and cannot deadlock if a server never ends a token.
    pub fn wait_until_ready(&self, deadline: Duration) -> bool {
        if self.shared.ready_confirmed.load(Ordering::Acquire) {
            return true;
        }

        let until = Instant::now() + deadline;
        let tick = Duration::from_millis(50).min(deadline);

        // A server that has just started has no active tokens *yet*, which
        // is indistinguishable from having finished. Give it a window to
        // announce its work before believing it is idle.
        let settle_until = Instant::now() + SETTLE_WINDOW.min(deadline);
        while !self.shared.saw_progress.load(Ordering::SeqCst)
            && !roslyn_init_pending(&self.shared)
            && Instant::now() < settle_until
        {
            std::thread::sleep(tick);
        }
        // Servers that never report progress have no startup phase to wait
        // out; the window above was the whole cost. Roslyn still owes its
        // project-load signal, which the loop below waits for via `is_busy`.
        if !self.shared.saw_progress.load(Ordering::SeqCst) && !roslyn_init_pending(&self.shared) {
            self.shared.ready_confirmed.store(true, Ordering::Release);
            return true;
        }

        // Require the server to be *continuously* quiet, not merely idle at
        // the instant we look. Measured against rust-analyzer on a 10-line
        // crate, the active-token set empties 11 separate times between
        // 0.18 s and 1.57 s before indexing is genuinely done, so a single
        // sample lands in one of those gaps and reports a cold server ready.
        let mut quiet_since: Option<Instant> = None;
        loop {
            if self.is_busy() {
                quiet_since = None;
            } else {
                let since = *quiet_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= QUIET_PERIOD {
                    self.shared.ready_confirmed.store(true, Ordering::Release);
                    return true;
                }
            }
            if Instant::now() >= until {
                let ready = !self.is_busy();
                if ready {
                    self.shared.ready_confirmed.store(true, Ordering::Release);
                }
                return ready;
            }
            std::thread::sleep(tick);
        }
    }

    /// Server-to-client notifications observed so far (capped).
    pub fn notifications(&self) -> Vec<(String, Value)> {
        lock(&self.shared.notifications).clone()
    }

    /// Resident-set size of the child, when the OS makes it observable.
    pub fn memory_bytes(&self) -> Option<u64> {
        if self.shared.life.load(Ordering::SeqCst) == LIFE_DEAD {
            return None;
        }
        rss_bytes(self.shared.pid)
    }

    /// JSON-RPC request. Blocks until the matching response, a timeout, or a crash.
    pub fn request<P: Serialize>(&self, method: &str, params: P) -> Result<Value, LspError> {
        self.request_timeout(method, params, self.shared.timeout)
    }

    pub fn request_timeout<P: Serialize>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let params =
            serde_json::to_value(params).map_err(|err| LspError::Protocol(err.to_string()))?;
        self.request_value(method, params, timeout)
    }

    /// Typed helper so the pool can write `call::<lsp_types::request::References>(params)`.
    pub fn call<R>(&self, params: R::Params) -> Result<R::Result, LspError>
    where
        R: lsp_types::request::Request,
        R::Params: Serialize,
    {
        let result = self.request(R::METHOD, params)?;
        serde_json::from_value(result).map_err(|err| LspError::Protocol(err.to_string()))
    }

    pub fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspError> {
        let params =
            serde_json::to_value(params).map_err(|err| LspError::Protocol(err.to_string()))?;
        self.send_message(rpc_notification(method, params))
    }

    pub fn notify_typed<N>(&self, params: N::Params) -> Result<(), LspError>
    where
        N: lsp_types::notification::Notification,
        N::Params: Serialize,
    {
        self.notify(N::METHOD, params)
    }

    /// `initialize` request plus `initialized` notification.
    pub fn initialize(&self, root: &Path) -> Result<Value, LspError> {
        self.initialize_with(root, None)
    }

    /// `initialize` request plus `initialized` notification.
    ///
    /// `initialization_options` overrides the per-language defaults from
    /// [`initialization_options_for`]; `None` falls back to them, so callers
    /// that never heard of those defaults keep today's behaviour.
    pub fn initialize_with(
        &self,
        root: &Path,
        initialization_options: Option<Value>,
    ) -> Result<Value, LspError> {
        let root_uri = path_to_file_uri(root);
        let folder_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace");
        let folders = json!([{ "uri": root_uri, "name": folder_name }]);
        *lock(&self.shared.workspace_folders) = Some(folders.clone());

        let mut params = json!({
            "processId": std::process::id(),
            "rootPath": root.to_string_lossy(),
            "rootUri": root_uri,
            "capabilities": client_capabilities(),
            "workspaceFolders": folders,
            "clientInfo": {
                "name": "astrolabe",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "locale": "en",
            "trace": "off",
        });
        if !send_root_uri(self.shared.spec.language) {
            // Null is the spec value for "no root". The folders above keep
            // carrying the workspace, which is the root cause fix — see
            // `send_root_uri`.
            params["rootPath"] = Value::Null;
            params["rootUri"] = Value::Null;
        }
        if let Some(options) = initialization_options.or_else(|| {
            initialization_options_for(self.shared.spec.language, root, &self.shared.spec.command)
        }) {
            params["initializationOptions"] = options;
        }

        let result = self.request("initialize", params)?;
        *lock(&self.shared.initialize_result) = Some(result.clone());
        self.notify("initialized", json!({}))?;
        // Roslyn requires solution/open or project/open after initialized.
        if matches!(
            self.shared.spec.language,
            Language::CSharp | Language::VisualBasic
        ) {
            super::csharp::open_csharp_workspace(self, root)?;
        }
        Ok(result)
    }

    /// `shutdown` request, `exit` notification, then reap the child.
    pub fn shutdown(&self) -> Result<(), LspError> {
        if self.shared.life.load(Ordering::SeqCst) != LIFE_RUNNING {
            self.force_stop();
            return Ok(());
        }
        let result = self.request_timeout("shutdown", Value::Null, SHUTDOWN_REQUEST_TIMEOUT);
        self.shared.life.store(LIFE_SHUTDOWN, Ordering::SeqCst);
        let _ = self.notify("exit", Value::Null);
        self.stop_with_grace(exit_grace(self.shared.spec.language));
        match result {
            Ok(_) | Err(LspError::Crashed) | Err(LspError::Timeout(_)) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Send `method`, retrying while the server answers `ContentModified`.
    ///
    /// The spec designates that code retryable and expects the client to
    /// re-issue. `rust-analyzer` returns it for the first query after a file
    /// is opened, so without this the opening query of every session fails
    /// — which is exactly the case an agent hits first.
    fn request_value(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let mut attempt = 0;
        let started = Instant::now();
        loop {
            // One deadline for the request and its retries, not one timeout each.
            let remaining = timeout.saturating_sub(started.elapsed());
            if attempt > 0 && remaining.is_zero() {
                return Err(LspError::Timeout(timeout));
            }
            match self.request_once(
                method,
                params.clone(),
                remaining.max(Duration::from_millis(1)),
            ) {
                Err(LspError::ContentModified)
                    if attempt + 1 < CONTENT_MODIFIED_ATTEMPTS
                        && started.elapsed() + CONTENT_MODIFIED_BACKOFF < timeout =>
                {
                    attempt += 1;
                    tracing::debug!(
                        method,
                        attempt,
                        "language server reported content modified; retrying"
                    );
                    std::thread::sleep(CONTENT_MODIFIED_BACKOFF);
                }
                other => return other,
            }
        }
    }

    fn request_once(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        match self.shared.life.load(Ordering::SeqCst) {
            LIFE_DEAD => return Err(LspError::Crashed),
            LIFE_SHUTDOWN if method != "shutdown" => {
                return Err(LspError::Protocol("language server has shut down".into()));
            }
            _ => {}
        }
        if child_exited(&self.shared) {
            if self.shared.life.load(Ordering::SeqCst) == LIFE_RUNNING {
                self.shared.life.store(LIFE_DEAD, Ordering::SeqCst);
                fail_pending(&self.shared);
            }
            if method != "shutdown" {
                return Err(LspError::Crashed);
            }
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.pending).insert(id, tx);

        if let Err(err) = self.send_message(rpc_request(id, method, params)) {
            lock(&self.shared.pending).remove(&id);
            return Err(err);
        }

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Disconnected) => Err(LspError::Crashed),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.pending).remove(&id);
                if let Ok(late) = rx.try_recv() {
                    return late;
                }
                if stdin_write_stuck(&self.shared, timeout.min(Duration::from_secs(1))) {
                    // The request never even left: the server is not reading.
                    kill_stuck_server(&self.shared);
                    return Err(LspError::Crashed);
                }
                let _ = self.notify("$/cancelRequest", json!({ "id": id }));
                Err(LspError::Timeout(timeout))
            }
        }
    }

    fn send_message(&self, payload: Value) -> Result<(), LspError> {
        write_rpc(&self.shared, &payload)
    }

    fn force_stop(&self) {
        self.stop_with_grace(Duration::from_millis(200));
    }

    /// Close stdin and give the child `grace` to exit on its own before it
    /// is killed.
    fn stop_with_grace(&self, grace: Duration) {
        self.shared.ready_confirmed.store(false, Ordering::Release);
        let previous = self.shared.life.swap(LIFE_SHUTDOWN, Ordering::SeqCst);
        if previous == LIFE_DEAD {
            self.shared.life.store(LIFE_DEAD, Ordering::SeqCst);
        }
        {
            *lock(&self.shared.stdin) = None;
        }
        {
            let mut child = lock(&self.shared.child);
            reap_child(&mut child, grace);
        }
        self.shared.life.store(LIFE_DEAD, Ordering::SeqCst);
        fail_pending(&self.shared);
        for slot in [&self.reader, &self.stderr, &self.writer] {
            if let Some(handle) = lock(slot).take() {
                join_with_timeout(handle, THREAD_JOIN_TIMEOUT);
            }
        }
    }
}

/// How long a server gets after `exit` before it is killed. kotlin-lsp
/// persists its index (RocksDB) on exit; the flush of a freshly built okhttp
/// index takes a few seconds, and killing it after 200 ms threw the whole
/// index away, so the next start re-indexed ~400k files (~90 s).
/// `ASTROLABE_KOTLIN_EXIT_GRACE_MS` overrides the Kotlin value.
fn exit_grace(language: Language) -> Duration {
    match language {
        Language::Kotlin => std::env::var("ASTROLABE_KOTLIN_EXIT_GRACE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(KOTLIN_EXIT_GRACE),
        _ => Duration::from_millis(500),
    }
}

const KOTLIN_EXIT_GRACE: Duration = Duration::from_secs(10);

/// The `shutdown` request on exit; a wedged server must not hold the exit.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

impl Drop for LspTransport {
    fn drop(&mut self) {
        self.force_stop();
    }
}

/// Production [`SpawnServer`]: handshake a stdio language server for one workspace.
///
/// [`SpawnServer::spawn`] does not receive a workspace root (that is a gap in
/// the pool trait). The root is stored here instead.
/// LSP error code for `ContentModified`.
const LSP_CONTENT_MODIFIED: i64 = -32801;

/// Attempts for a request the server answered with `ContentModified`.
const CONTENT_MODIFIED_ATTEMPTS: u32 = 4;

/// Backoff between those attempts. The server is re-analysing the file, which
/// takes milliseconds once the index is warm.
const CONTENT_MODIFIED_BACKOFF: Duration = Duration::from_millis(250);

/// How long a freshly started server is given to announce background work
/// before we accept its silence as "already indexed".
const SETTLE_WINDOW: Duration = Duration::from_secs(3);

/// Longest a Roslyn server is treated as busy while it has not sent
/// `workspace/projectInitializationComplete` (Serena waits 30 s for the same
/// signal). Past this its answers are used as they are, still subject to the
/// usual progress tracking.
const ROSLYN_INIT_CAP: Duration = Duration::from_secs(30);

/// Roslyn's "projects loaded" notification (VS Code C# `roslynProtocol.ts`).
const ROSLYN_PROJECT_INIT_COMPLETE: &str = "workspace/projectInitializationComplete";

/// Roslyn asks the client to run `dotnet restore` for these projects.
const ROSLYN_PROJECT_NEEDS_RESTORE: &str = "workspace/_roslyn_projectNeedsRestore";
/// `workspace/configuration` section through which Roslyn asks whether to run
/// a NuGet restore on its own after loading projects (VS Code's
/// `dotnet.projects.enableAutomaticRestore`). Always answered `false`.
const ROSLYN_AUTOMATIC_RESTORE_SECTION: &str = "projects.dotnet_enable_automatic_restore";

fn is_roslyn(language: Language) -> bool {
    matches!(language, Language::CSharp | Language::VisualBasic)
}

/// Roslyn's project load is still outstanding (and within its cap).
fn roslyn_init_pending(shared: &Shared) -> bool {
    shared.roslyn_init_pending.load(Ordering::Acquire) && shared.started.elapsed() < ROSLYN_INIT_CAP
}

/// Methods we re-issue on `ContentModified` (see [`LspTransport::request_value`]),
/// declared through `general.staleRequestSupport` so a server that follows the
/// spec answers -32801 instead of a result computed from stale text.
const RETRY_ON_CONTENT_MODIFIED: &[&str] = &[
    "textDocument/references",
    "textDocument/definition",
    "textDocument/hover",
    "textDocument/diagnostic",
    "textDocument/rename",
    "textDocument/documentSymbol",
];

/// How long the server must stay quiet before its index is trusted.
///
/// Covers the transient gaps between a server's startup tasks; measured at
/// under 900 ms of gap for rust-analyzer, so 1 s clears them with margin.
const QUIET_PERIOD: Duration = Duration::from_secs(1);

fn timeout_from_env() -> Option<Duration> {
    let raw = std::env::var("ASTROLABE_LSP_TIMEOUT").ok()?;
    match raw.parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => {
            tracing::warn!(
                value = %raw,
                "ASTROLABE_LSP_TIMEOUT is not a valid integer (seconds); using default"
            );
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct StdioSpawn {
    pub root: PathBuf,
    pub timeout: Duration,
    /// Bound on a single query (`references`, `definition`, …). Kept far
    /// below [`DEFAULT_TIMEOUT`]: a query that hangs for minutes is worse
    /// than a clearly-labelled text-search fallback.
    pub query_timeout: Duration,
}

/// Default bound on one query request. pyright was observed hanging on
/// `textDocument/references` for more than 120 s.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

fn query_timeout_from_env() -> Option<Duration> {
    let raw = std::env::var("ASTROLABE_LSP_QUERY_TIMEOUT").ok()?;
    match raw.parse::<u64>() {
        Ok(0) | Err(_) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
    }
}

impl StdioSpawn {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let timeout = timeout_from_env().unwrap_or(DEFAULT_TIMEOUT);
        Self {
            root: root.into(),
            timeout,
            query_timeout: query_timeout_from_env()
                .unwrap_or(DEFAULT_QUERY_TIMEOUT)
                .min(timeout),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.query_timeout = self.query_timeout.min(timeout);
        self
    }

    pub fn with_query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = timeout;
        self
    }
}

impl SpawnServer for StdioSpawn {
    // `root` comes from the caller rather than `self.root`: the pool knows
    // which workspace it is indexing, and a spawner should not have to be
    // rebuilt to point at a different one.
    fn spawn(
        &self,
        spec: &ServerSpec,
        root: &std::path::Path,
    ) -> Result<Box<dyn LanguageServer>, LspError> {
        if spec.language == Language::Vue {
            return crate::lsp::vue_bridge::spawn_vue(spec, root, self.timeout, self.query_timeout);
        }
        let spec = super::apple::prepare_spec(spec, root);
        let transport = LspTransport::connect(&spec, root, self.timeout)?;
        let session = LspSession::new(transport, root.to_path_buf(), self.query_timeout);
        if let Some(file) = super::apple::warm_up_file(&spec, root) {
            // Best effort: a failure here only means indexing starts on the
            // first real query instead.
            let _ = session.ensure_open(&file);
        }
        Ok(Box::new(session))
    }
}

/// [`LanguageServer`] over [`LspTransport`]: `didOpen`, method calls, URI conversion.
pub struct LspSession {
    transport: LspTransport,
    root: PathBuf,
    /// Documents we have `didOpen`ed, keyed by repo-relative path.
    ///
    /// Once a document is open the server answers from *our* copy, not from
    /// disk, so every query must first push whatever changed on disk;
    /// otherwise positions we compute from disk are matched against stale
    /// text and fresh references are silently missed.
    opened: Mutex<HashMap<String, OpenDoc>>,
    tick: AtomicU64,
    query_timeout: Duration,
}

#[derive(Clone, Copy, Debug)]
struct OpenDoc {
    version: i64,
    hash: [u8; 32],
    last_used: u64,
}

/// Most documents kept open in one server; beyond this the least recently
/// used is `didClose`d.
const MAX_OPEN_DOCS: usize = 512;

/// Watched-file events per notification, so a checkout touching thousands of
/// files does not become one giant frame.
const WATCHED_FILES_BATCH: usize = 500;

/// Newly created files announced with didOpen/didClose per watcher batch. A
/// branch switch that adds thousands of files is left to the watched-file
/// events alone rather than streamed through the server one by one.
const MAX_CREATED_ANNOUNCE: usize = 64;

/// Larger new files (generated bundles, fixtures) are not announced.
const MAX_ANNOUNCE_BYTES: u64 = 1024 * 1024;

/// Whether a server started for `server` handles files of `file`.
fn serves(server: Language, file: Language) -> bool {
    use Language::*;
    let family = |l: Language| match l {
        TypeScript | Tsx | JavaScript => 1,
        C | Cpp | ObjC | ObjCpp => 2,
        CSharp | VisualBasic => 3,
        _ => 0,
    };
    server == file || (family(server) != 0 && family(server) == family(file))
}

/// `FileChangeType` values from the LSP spec.
const FILE_CREATED: u8 = 1;
const FILE_CHANGED: u8 = 2;
const FILE_DELETED: u8 = 3;

fn content_hash(text: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).into()
}

impl LspSession {
    pub fn new(transport: LspTransport, root: PathBuf, query_timeout: Duration) -> Self {
        LspSession {
            transport,
            root,
            opened: Mutex::new(HashMap::new()),
            tick: AtomicU64::new(0),
            query_timeout,
        }
    }

    fn file_uri(&self, path: &RelPath) -> String {
        path_to_file_uri(&self.root.join(path.as_str()))
    }

    pub fn transport(&self) -> &LspTransport {
        &self.transport
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn query_timeout(&self) -> Duration {
        self.query_timeout
    }

    fn query(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.transport
            .request_timeout(method, params, self.query_timeout)
    }

    /// Make the server's copy of `path` match the disk.
    ///
    /// First use sends `didOpen`; later uses compare a content hash and send
    /// a full-text `didChange` with a bumped version when it moved. A file
    /// that vanished is `didClose`d so the server stops answering from a
    /// ghost buffer.
    pub(crate) fn ensure_open(&self, path: &RelPath) -> Result<(), LspError> {
        let key = path.as_str().to_string();
        let abs = self.root.join(path.as_str());
        // Read the file *inside* the lock. Reading first let a slow caller
        // that had read older text send it after a fresher didChange.
        let mut opened = lock(&self.opened);
        let text = match std::fs::read(&abs) {
            // Non-UTF-8 sources (GBK, Latin-1) are still synced, transcoded to UTF-8.
            // `decode_source` (BOM / chardetng) matches the text the index and
            // the fallbacks see, so positions agree for GBK / UTF-16 files.
            Ok(bytes) => crate::text::decode_source(bytes),
            Err(err) => {
                if opened.remove(&key).is_some() {
                    let _ = self.transport.notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": self.file_uri(path) } }),
                    );
                }
                return Err(LspError::Protocol(format!(
                    "could not read {} ({err})",
                    path.as_str()
                )));
            }
        };
        let hash = content_hash(&text);
        if !opened.contains_key(&key) && opened.len() >= MAX_OPEN_DOCS {
            // Bound the server's memory: close the least recently used doc.
            let oldest = opened
                .iter()
                .min_by_key(|(_, d)| d.last_used)
                .map(|(k, _)| k.clone());
            if let Some(old) = oldest {
                opened.remove(&old);
                let _ = self.transport.notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": self.file_uri(&RelPath::new(old)) } }),
                );
            }
        }
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        if let Some(doc) = opened.get_mut(&key) {
            doc.last_used = tick;
        }
        match opened.get_mut(&key) {
            Some(doc) if doc.hash == hash => Ok(()),
            Some(doc) => {
                let version = doc.version + 1;
                self.transport.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": self.file_uri(path), "version": version },
                        "contentChanges": [ { "text": text } ],
                    }),
                )?;
                *doc = OpenDoc {
                    version,
                    hash,
                    last_used: tick,
                };
                // Servers that run their checks on save (rust-analyzer's
                // flycheck) only refresh diagnostics after didSave; we
                // advertise it, so send it.
                let _ = self.transport.notify(
                    "textDocument/didSave",
                    json!({ "textDocument": { "uri": self.file_uri(path) } }),
                );
                Ok(())
            }
            None => {
                self.transport.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": self.file_uri(path),
                            "languageId": language_id(self.transport.language()),
                            "version": 1,
                            "text": text,
                        }
                    }),
                )?;
                opened.insert(
                    key,
                    OpenDoc {
                        version: 1,
                        hash,
                        last_used: tick,
                    },
                );
                Ok(())
            }
        }
    }

    /// `didOpen` then `didClose` a file that was just created on disk.
    ///
    /// Some servers (pyright is the documented case; Serena works around it
    /// the same way) do not add a file to their program on a watched-file
    /// `Created` event alone, so references from the new file stay invisible
    /// until it is opened. Opening and closing it once makes the server read
    /// it. Only files this server's language handles, below
    /// [`MAX_ANNOUNCE_BYTES`], are announced. Never starts a server: this
    /// runs only on one that is already resident.
    fn announce_created(&self, path: &RelPath) -> bool {
        let Some(file_language) = Language::from_path(path) else {
            return false;
        };
        if !serves(self.transport.language(), file_language) {
            return false;
        }
        let abs = self.root.join(path.as_str());
        match std::fs::metadata(&abs) {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_ANNOUNCE_BYTES => {}
            _ => return false,
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            return false;
        };
        let text = crate::text::decode_source(bytes);
        let uri = self.file_uri(path);
        // Hold the open-doc lock so a concurrent query's didOpen of the same
        // file cannot interleave with our close.
        let opened = lock(&self.opened);
        if opened.contains_key(path.as_str()) {
            return false;
        }
        let sent = self.transport.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id(file_language),
                    "version": 1,
                    "text": text,
                }
            }),
        );
        if sent.is_err() {
            return false;
        }
        let _ = self.transport.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
        drop(opened);
        true
    }

    fn close_if_open(&self, path: &RelPath) {
        if lock(&self.opened).remove(path.as_str()).is_some() {
            let _ = self.transport.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": self.file_uri(path) } }),
            );
        }
    }

    /// Version the server currently holds for `path`, if open. Test hook.
    pub fn open_version(&self, path: &RelPath) -> Option<i64> {
        lock(&self.opened).get(path.as_str()).map(|d| d.version)
    }
}

impl LanguageServer for LspSession {
    fn language(&self) -> Language {
        self.transport.language()
    }

    fn references(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError> {
        self.ensure_open(path)?;
        let result = self.query(
            "textDocument/references",
            json!({
                "textDocument": { "uri": self.file_uri(path) },
                "position": { "line": position.line, "character": position.character },
                "context": { "includeDeclaration": true },
            }),
        )?;
        Ok(locations_from_value(&result, &self.root))
    }

    fn definition(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError> {
        self.ensure_open(path)?;
        let result = self.query(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": self.file_uri(path) },
                "position": { "line": position.line, "character": position.character },
            }),
        )?;
        Ok(locations_from_value(&result, &self.root))
    }

    fn hover(&self, path: &RelPath, position: Position) -> Result<Value, LspError> {
        self.ensure_open(path)?;
        self.query(
            crate::lsp::queries::HOVER_METHOD,
            crate::lsp::queries::hover_params(&self.file_uri(path), position),
        )
    }

    fn diagnostics(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
        self.ensure_open(path)?;
        let uri = self.file_uri(path);
        match self.query(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
        ) {
            Ok(value) => Ok(diagnostics_from_pull(&value, path)),
            Err(LspError::Protocol(_)) => {
                let diags = self.push_diagnostics(path, &uri);
                if diags.is_empty() {
                    Err(LspError::Protocol(
                        "pull diagnostic unsupported and push diagnostics empty".into(),
                    ))
                } else {
                    Ok(diags)
                }
            }
            Err(LspError::Timeout(timeout)) => {
                // Prefer any push diagnostics that arrived while the pull hung,
                // but keep Timeout visible when the fallback is empty.
                let diags = self.push_diagnostics(path, &uri);
                if diags.is_empty() {
                    Err(LspError::Timeout(timeout))
                } else {
                    Ok(diags)
                }
            }
            Err(err) => Err(err),
        }
    }

    fn prepare_rename(
        &self,
        path: &RelPath,
        position: Position,
        new_name: &str,
    ) -> Result<WorkspaceEdit, LspError> {
        self.ensure_open(path)?;
        let result = self.query(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": self.file_uri(path) },
                "position": { "line": position.line, "character": position.character },
                "newName": new_name,
            }),
        )?;
        Ok(workspace_edit_from_value(&result, &self.root))
    }

    fn is_busy(&self) -> bool {
        self.transport.is_busy()
    }

    fn files_changed(&self, changes: &[FileChange]) {
        if changes.is_empty() {
            return;
        }
        let events: Vec<Value> = changes
            .iter()
            .map(|change| {
                let kind = match change.kind {
                    FileChangeKind::Created => FILE_CREATED,
                    FileChangeKind::Changed => FILE_CHANGED,
                    FileChangeKind::Deleted => FILE_DELETED,
                };
                json!({ "uri": self.file_uri(&change.path), "type": kind })
            })
            .collect();
        for batch in events.chunks(WATCHED_FILES_BATCH) {
            let _ = self.transport.notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": batch }),
            );
        }
        let mut announced = 0usize;
        for change in changes {
            match change.kind {
                FileChangeKind::Deleted => self.close_if_open(&change.path),
                FileChangeKind::Created | FileChangeKind::Changed => {
                    if lock(&self.opened).contains_key(change.path.as_str()) {
                        let _ = self.ensure_open(&change.path);
                    } else if change.kind == FileChangeKind::Created
                        && announced < MAX_CREATED_ANNOUNCE
                        && self.announce_created(&change.path)
                    {
                        announced += 1;
                    }
                }
            }
        }
    }

    fn wait_until_ready(&self, deadline: Duration) -> bool {
        self.transport.wait_until_ready(deadline)
    }

    fn memory_bytes(&self) -> Option<u64> {
        self.transport.memory_bytes()
    }

    fn shutdown(&self) -> Result<(), LspError> {
        self.transport.shutdown()
    }
}

impl LspSession {
    fn push_diagnostics(&self, path: &RelPath, uri: &str) -> Vec<Diagnostic> {
        let snap = self.transport.published_diagnostics();
        // Exact URI first, then any key that names the very same file once
        // decoded (case/percent-encoding differences). Never a suffix match:
        // `lib.rs` must not pick up `mylib.rs`.
        let key = if snap.contains_key(uri) {
            Some(uri.to_string())
        } else {
            snap.keys()
                .find(|k| uri_to_rel(k, &self.root).as_ref() == Some(path))
                .cloned()
        };
        let Some(key) = key else {
            return Vec::new();
        };
        // A publish for an older document version describes text we have
        // since replaced.
        if let (Some(published), Some(current)) = (
            self.transport.published_diagnostic_version(&key),
            lock(&self.opened).get(path.as_str()).map(|d| d.version),
        ) {
            if published < current {
                return Vec::new();
            }
        }
        let Some(items) = snap.get(&key) else {
            return Vec::new();
        };
        items
            .iter()
            .map(|d| diagnostic_from_lsp(path.clone(), d))
            .collect()
    }
}

fn language_id(language: Language) -> &'static str {
    match language {
        Language::Python => "python",
        Language::Go => "go",
        Language::Java => "java",
        Language::Rust => "rust",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::JavaScript => "javascript",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::ObjC => "objective-c",
        Language::ObjCpp => "objective-cpp",
        Language::Swift => "swift",
        Language::Php => "php",
        Language::Vue => "vue",
        Language::Dart => "dart",
        Language::CSharp => "csharp",
        Language::VisualBasic => "vb",
        Language::Kotlin => "kotlin",
        Language::Shell => "shellscript",
        Language::Ruby => "ruby",
        Language::Lua => "lua",
    }
}

/// Whether `initialize` should name the workspace root via `rootUri` /
/// `rootPath` for this language.
///
/// The Dart analysis server counts `rootUri` as an analysis root *in addition
/// to* `workspaceFolders` and never dedupes the two (oraios/serena#2045): in a
/// monorepo the server analyses the whole repository twice and spins at full
/// CPU while it does. Dart therefore initialises with a null root and keeps
/// `workspaceFolders` as the single analysis root. Every other language keeps
/// sending the root exactly as before.
fn send_root_uri(language: Language) -> bool {
    !matches!(language, Language::Dart)
}

/// Per-language `initializationOptions` merged into `initialize` when the
/// caller has none of its own.
///
/// Mirrors Serena's Dart setup: the optional per-file niceties (closing
/// labels, Dart and Flutter outlines) are switched off so the analysis server
/// does the minimum work. `None` means "no options" and `initialize` then
/// omits the field entirely, exactly as before this hook existed.
pub(crate) fn initialization_options_for(
    language: Language,
    root: &Path,
    command: &Path,
) -> Option<Value> {
    match language {
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            let tsserver = find_tsserver(root, command)?;
            Some(json!({ "tsserver": { "path": tsserver.to_string_lossy() } }))
        }
        Language::Shell => Some(shell_settings()),
        Language::Kotlin if super::kotlin::is_jetbrains_server(command) => {
            let exported =
                super::kotlin::exported_workspace(root, &super::kotlin::system_path(root));
            super::kotlin::initialization_options(
                root,
                std::env::var(super::kotlin::ENV_BUILD_TOOL).ok(),
                exported.as_deref(),
            )
        }
        // sourcekit-lsp only answers cross-file queries from its index, and on
        // a bare checkout (no prior `swift build` / Xcode build) that index is
        // empty: references come back `[]`. Background indexing makes the
        // server run SwiftPM's prepare step and index the package itself; it
        // reports progress through `$/progress`, which `is_busy` tracks.
        Language::Swift | Language::ObjC | Language::ObjCpp if is_sourcekit(command) => {
            Some(json!({
                "backgroundIndexing": true,
                "backgroundPreparationMode": "enabled",
            }))
        }
        // Volar 2.x. `hybridMode: false` makes it run its own embedded TS
        // service; sent even when the workspace ships TypeScript, otherwise
        // 2.x defaults to hybrid mode and waits for a TS client that never
        // comes. Volar 3.x is started by `vue_bridge` with its own options.
        Language::Vue => {
            let mut options = json!({ "vue": { "hybridMode": false } });
            let tsdk = find_tsserver(root, command)
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .or_else(|| {
                    let local = root.join("node_modules/typescript/lib");
                    local.is_dir().then_some(local)
                });
            if let Some(tsdk) = tsdk {
                options["typescript"] = json!({ "tsdk": tsdk.to_string_lossy() });
            }
            Some(options)
        }
        Language::Dart => Some(json!({
            "onlyAnalyzeProjectsWithOpenFiles": false,
            "closingLabels": false,
            "outline": false,
            "flutterOutline": false,
        })),
        _ => None,
    }
}

fn is_sourcekit(command: &Path) -> bool {
    command
        .file_stem()
        .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("sourcekit-lsp"))
}

/// Locate a `typescript/lib/tsserver.js` when the workspace has none.
///
/// typescript-language-server only looks in the workspace's `node_modules`
/// and then gives up with "Could not find a valid TypeScript installation",
/// so repos without `npm install` (fresh clones, CI checkouts, read-only
/// browsing) get no precise TS answers at all. A global
/// `npm i -g typescript-language-server typescript` puts `typescript` next to
/// the server package; point the server at it. Returns `None` when the
/// workspace already ships TypeScript so its own version wins.
pub(crate) fn find_tsserver(root: &Path, command: &Path) -> Option<PathBuf> {
    const REL: &str = "typescript/lib/tsserver.js";
    if root.join("node_modules").join(REL).is_file() {
        return None;
    }
    if let Some(explicit) = std::env::var_os("ASTROLABE_TSSERVER_PATH") {
        let p = PathBuf::from(explicit);
        let p = if p.is_dir() { p.join("tsserver.js") } else { p };
        if p.is_file() {
            return Some(p);
        }
    }
    let resolved = std::fs::canonicalize(command).unwrap_or_else(|_| command.to_path_buf());
    let mut starts = vec![resolved];
    if let Some(parent) = command.parent() {
        // `<prefix>/bin/tsls` -> `<prefix>/lib/node_modules` (unix npm -g layout)
        if let Some(prefix) = parent.parent() {
            starts.push(prefix.join("lib").join("node_modules").join("x"));
            starts.push(prefix.join("node_modules").join("x"));
        }
        // Windows npm -g: shims live directly in the prefix.
        starts.push(parent.join("node_modules").join("x"));
    }
    for start in &starts {
        for dir in start.ancestors().skip(1) {
            let candidate = dir.join(REL);
            if candidate.is_file() {
                return Some(candidate);
            }
            let nested = dir.join("node_modules").join(REL);
            if nested.is_file() {
                return Some(nested);
            }
        }
    }
    None
}

fn locations_from_value(value: &Value, root: &Path) -> Vec<Location> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| location_from_value(item, root))
            .collect(),
        Value::Object(_) => location_from_value(value, root).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn location_from_value(value: &Value, root: &Path) -> Option<Location> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))?;
    let path = match uri_to_rel(uri, root) {
        Some(path) => path,
        // Outside the workspace (stdlib, node_modules, registry): keep the
        // raw `file://` URI so the query layer can classify it and say
        // "N results are outside the repository" instead of seeing nothing.
        None if uri.len() >= 5 && uri[..5].eq_ignore_ascii_case("file:") => RelPath::new(uri),
        None => return None,
    };
    Some(Location {
        path,
        range: range_from_value(range)?,
    })
}

fn range_from_value(value: &Value) -> Option<Range> {
    Some(Range {
        start: position_from_value(value.get("start")?)?,
        end: position_from_value(value.get("end")?)?,
    })
}

fn position_from_value(value: &Value) -> Option<Position> {
    Some(Position {
        line: value.get("line")?.as_u64()? as u32,
        character: value.get("character")?.as_u64()? as u32,
    })
}

fn diagnostics_from_pull(value: &Value, path: &RelPath) -> Vec<Diagnostic> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            let range = range_from_value(item.get("range")?)?;
            let message = item.get("message")?.as_str()?.to_string();
            let severity = severity_from_u64(item.get("severity").and_then(Value::as_u64));
            let code = item.get("code").and_then(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            Some(Diagnostic {
                path: path.clone(),
                range,
                severity,
                message,
                code,
            })
        })
        .collect()
}

fn diagnostic_from_lsp(path: RelPath, d: &lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        path,
        range: Range {
            start: Position {
                line: d.range.start.line,
                character: d.range.start.character,
            },
            end: Position {
                line: d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: match d.severity {
            Some(s) if s == lsp_types::DiagnosticSeverity::ERROR => Severity::Error,
            Some(s) if s == lsp_types::DiagnosticSeverity::WARNING => Severity::Warning,
            Some(s) if s == lsp_types::DiagnosticSeverity::INFORMATION => Severity::Information,
            Some(s) if s == lsp_types::DiagnosticSeverity::HINT => Severity::Hint,
            _ => Severity::Information,
        },
        message: d.message.clone(),
        code: match &d.code {
            Some(lsp_types::NumberOrString::Number(n)) => Some(n.to_string()),
            Some(lsp_types::NumberOrString::String(s)) => Some(s.clone()),
            None => None,
        },
    }
}

fn severity_from_u64(n: Option<u64>) -> Severity {
    match n {
        Some(1) => Severity::Error,
        Some(2) => Severity::Warning,
        Some(3) => Severity::Information,
        Some(4) => Severity::Hint,
        _ => Severity::Information,
    }
}

fn workspace_edit_from_value(value: &Value, root: &Path) -> WorkspaceEdit {
    let mut edits: Vec<(RelPath, Vec<TextEdit>)> = Vec::new();
    if let Some(doc_changes) = value
        .get("documentChanges")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
    {
        for change in doc_changes {
            let uri = change
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(Value::as_str);
            let Some(uri) = uri else {
                continue;
            };
            let Some(path) = uri_to_rel(uri, root) else {
                continue;
            };
            let file_edits: Vec<TextEdit> = change
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(text_edit_from_value)
                .collect();
            if !file_edits.is_empty() {
                edits.push((path, file_edits));
            }
        }
    } else if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, list) in changes {
            let Some(path) = uri_to_rel(uri, root) else {
                continue;
            };
            let Some(items) = list.as_array() else {
                continue;
            };
            let file_edits: Vec<TextEdit> = items.iter().filter_map(text_edit_from_value).collect();
            if !file_edits.is_empty() {
                edits.push((path, file_edits));
            }
        }
    }
    WorkspaceEdit { edits }
}

fn text_edit_from_value(value: &Value) -> Option<TextEdit> {
    Some(TextEdit {
        range: range_from_value(value.get("range")?)?,
        new_text: value.get("newText")?.as_str()?.to_string(),
    })
}

fn uri_to_rel(uri: &str, root: &Path) -> Option<RelPath> {
    let decoded = if let Some(rest) = uri.strip_prefix("file://") {
        percent_decode(rest)
    } else {
        percent_decode(uri)
    };
    // `file:///C:/x` decodes to `/C:/x`; drop the slash before a drive letter.
    let decoded = match decoded.as_bytes() {
        [b'/', d, b':', ..] if d.is_ascii_alphabetic() => decoded[1..].to_string(),
        _ => decoded,
    };
    let path = PathBuf::from(decoded);
    // Fast path: servers report paths under the root as we sent them, so a
    // plain prefix strip answers without two canonicalize syscalls per hit.
    if path.is_absolute() {
        if let Ok(rel) = path.strip_prefix(root) {
            if !rel.as_os_str().is_empty()
                && !rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Some(RelPath::new(rel.to_string_lossy()));
            }
        }
    }
    let root_abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let abs = if path.is_absolute() {
        path.canonicalize().unwrap_or(path)
    } else {
        return Some(RelPath::new(path.to_string_lossy()));
    };
    let rel = abs.strip_prefix(&root_abs).ok()?;
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(RelPath::new(rel.to_string_lossy()))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn client_capabilities() -> Value {
    json!({
        "workspace": {
            "applyEdit": false,
            "workspaceFolders": true,
            "configuration": true,
            "symbol": {},
        },
        "textDocument": {
            "publishDiagnostics": { "relatedInformation": true },
            "definition": { "linkSupport": true },
            "references": {},
            "rename": { "prepareSupport": true },
            "diagnostic": {},
            "synchronization": { "didSave": true },
        },
        "window": {
            "workDoneProgress": true,
            "showMessage": { "messageActionItem": { "additionalPropertiesSupport": false } },
        },
        "general": {
            "positionEncodings": ["utf-16"],
            // We cancel timed-out requests (`$/cancelRequest`) and retry
            // these on ContentModified.
            "staleRequestSupport": {
                "cancel": true,
                "retryOnContentModified": RETRY_ON_CONTENT_MODIFIED,
            },
        },
    })
}

fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), json!("2.0"));
    obj.insert("id".into(), json!(id));
    obj.insert("method".into(), json!(method));
    obj.insert("params".into(), params);
    Value::Object(obj)
}

fn rpc_notification(method: &str, params: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), json!("2.0"));
    obj.insert("method".into(), json!(method));
    if !params.is_null() {
        obj.insert("params".into(), params);
    }
    Value::Object(obj)
}

fn write_rpc(shared: &Shared, payload: &Value) -> Result<(), LspError> {
    let body = serde_json::to_vec(payload).map_err(|err| LspError::Protocol(err.to_string()))?;
    if shared.life.load(Ordering::SeqCst) == LIFE_DEAD {
        return Err(LspError::Crashed);
    }
    let mut frame = Vec::with_capacity(32 + body.len());
    write!(frame, "Content-Length: {}\r\n\r\n", body.len())
        .map_err(|err| LspError::Protocol(err.to_string()))?;
    frame.extend_from_slice(&body);
    let guard = lock(&shared.stdin);
    let tx = guard.as_ref().ok_or(LspError::Crashed)?;
    match tx.try_send(frame) {
        Ok(()) => Ok(()),
        Err(mpsc::TrySendError::Disconnected(_)) => {
            shared.life.store(LIFE_DEAD, Ordering::SeqCst);
            fail_pending(shared);
            Err(LspError::Crashed)
        }
        // The writer is wedged behind a server that does not read.
        Err(mpsc::TrySendError::Full(_)) => {
            drop(guard);
            kill_stuck_server(shared);
            Err(LspError::Crashed)
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail_pending(shared: &Shared) {
    let waiters: Vec<_> = lock(&shared.pending).drain().map(|(_, tx)| tx).collect();
    for tx in waiters {
        let _ = tx.send(Err(LspError::Crashed));
    }
}

fn child_exited(shared: &Shared) -> bool {
    match lock(&shared.child).try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(_) => true,
    }
}

fn reap_child(child: &mut Child, timeout: Duration) {
    let pid = child.id();
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                // The wrapper exited; whatever it spawned may still be alive.
                kill_process_group(pid);
                return;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                kill_process_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(_) => {
                kill_process_group(pid);
                return;
            }
        }
    }
}

/// SIGKILL the whole process group the server was started in (it is its own
/// group leader, see `start`). No-op off Unix, where `Child::kill` is all
/// there is.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        // Never signal pgid 0/1 or something that does not fit pid_t.
        if pid > 1 && pid <= i32::MAX as u32 {
            // SAFETY: plain syscall with integer arguments.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    job::terminate(pid);
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// Windows counterpart of the Unix process group: every server runs inside a
/// Job Object created with `KILL_ON_JOB_CLOSE`, so terminating (or closing)
/// the job takes the whole descendant tree with it, even if Astrolabe itself
/// dies abnormally.
#[cfg(windows)]
mod job {
    use std::collections::HashMap;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct JobHandle(HANDLE);
    // SAFETY: a job handle is a process-wide kernel object handle, usable from any thread.
    unsafe impl Send for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // SAFETY: we own the handle; closing kills the job (KILL_ON_JOB_CLOSE).
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn jobs() -> &'static Mutex<HashMap<u32, JobHandle>> {
        static JOBS: OnceLock<Mutex<HashMap<u32, JobHandle>>> = OnceLock::new();
        JOBS.get_or_init(Default::default)
    }

    /// Put the freshly spawned server into its own kill-on-close job.
    /// Best effort: on failure the caller falls back to `Child::kill`.
    pub fn adopt(pid: u32, child: &Child) {
        // SAFETY: plain Win32 calls; the handle is checked and closed on every path.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return;
            }
            let job = JobHandle(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                return;
            }
            if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                return;
            }
            jobs()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(pid, job);
        }
    }

    /// Kill every process in the server's job and release the handle.
    pub fn terminate(pid: u32) {
        let job = jobs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&pid);
        if let Some(job) = job {
            // SAFETY: valid job handle owned by `job`.
            unsafe {
                TerminateJobObject(job.0, 1);
            }
        }
    }
}

/// How long a stopping transport waits for its helper threads. They end once
/// the pipes close; if a stray descendant still holds one open we detach the
/// thread rather than hang `drop`/`shutdown` (and the pool monitor with it).
const THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Frames the stdin writer may have queued before the server is declared stuck.
const STDIN_QUEUE_FRAMES: usize = 4096;

fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            tracing::debug!("lsp helper thread did not stop in time; detaching");
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.join();
}

fn write_stdin(
    mut stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    while let Ok(frame) = rx.recv() {
        let stamp = shared.started.elapsed().as_millis() as u64 + 1;
        shared.write_started.store(stamp, Ordering::SeqCst);
        let result = stdin.write_all(&frame).and_then(|_| stdin.flush());
        shared.write_started.store(0, Ordering::SeqCst);
        if result.is_err() {
            shared.life.store(LIFE_DEAD, Ordering::SeqCst);
            fail_pending(&shared);
            return;
        }
    }
}

/// The server stopped draining its stdin: kill it (and its group) without
/// touching the child lock, which `stop_with_grace` may be holding.
fn kill_stuck_server(shared: &Shared) {
    shared.life.store(LIFE_DEAD, Ordering::SeqCst);
    *lock(&shared.stdin) = None;
    kill_process_group(shared.pid);
    fail_pending(shared);
}

/// True when the writer thread has been inside one `write_all` for at least `min`.
fn stdin_write_stuck(shared: &Shared, min: Duration) -> bool {
    let stamp = shared.write_started.load(Ordering::SeqCst);
    if stamp == 0 {
        return false;
    }
    let now = shared.started.elapsed().as_millis() as u64 + 1;
    now.saturating_sub(stamp) >= min.as_millis() as u64
}

fn read_stdout(stdout: std::process::ChildStdout, shared: Arc<Shared>) {
    let mut reader = stdout;
    let mut parser = FrameParser::new();
    loop {
        loop {
            match parser.next() {
                Ok(Some(body)) => dispatch(&shared, &body),
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(error = %err, "lsp framing error");
                    on_reader_exit(&shared);
                    return;
                }
            }
        }
        let mut tmp = [0u8; 8192];
        match reader.read(&mut tmp) {
            Ok(0) => {
                on_reader_exit(&shared);
                return;
            }
            Ok(n) => parser.ingest(&tmp[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                on_reader_exit(&shared);
                return;
            }
        }
    }
}

fn on_reader_exit(shared: &Shared) {
    let previous =
        shared
            .life
            .compare_exchange(LIFE_RUNNING, LIFE_DEAD, Ordering::SeqCst, Ordering::SeqCst);
    if previous == Ok(LIFE_RUNNING) {
        fail_pending(shared);
    }
}

const STDERR_TAIL_BYTES: usize = 4096;

fn summarize_stderr(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(6);
    let mut out = lines[start..].join(" | ");
    if out.chars().count() > 600 {
        out = out.chars().take(600).collect::<String>() + "…";
    }
    out
}

fn drain_stderr(mut stderr: std::process::ChildStderr, shared: Arc<Shared>) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                shared.stderr_bytes.fetch_add(n as u64, Ordering::Relaxed);
                {
                    let mut tail = lock(&shared.stderr_tail);
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > STDERR_TAIL_BYTES {
                        let cut = tail.len() - STDERR_TAIL_BYTES;
                        tail.drain(..cut);
                    }
                }
                tracing::debug!(
                    target: "astrolabe_lsp_stderr",
                    "{}",
                    String::from_utf8_lossy(&buf[..n])
                );
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn dispatch(shared: &Shared, body: &[u8]) {
    let msg: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(error = %err, "lsp: invalid JSON body");
            return;
        }
    };
    if let Some(batch) = msg.as_array() {
        for item in batch {
            dispatch_one(shared, item);
        }
        return;
    }
    dispatch_one(shared, &msg);
}

fn dispatch_one(shared: &Shared, msg: &Value) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id");
    match (method, id) {
        (Some(method), Some(id)) => handle_server_request(shared, method, msg.get("params"), id),
        (Some(method), None) => handle_notification(shared, method, msg.get("params")),
        (None, Some(id)) => handle_response(shared, id, msg),
        (None, None) => {}
    }
}

fn handle_response(shared: &Shared, id: &Value, msg: &Value) {
    let Some(n) = rpc_id_u64(id) else {
        tracing::debug!(?id, "lsp response with unrecognised id");
        return;
    };
    let tx = lock(&shared.pending).remove(&n);
    let Some(tx) = tx else {
        return;
    };
    let reply = if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(Value::as_i64);
        if code == Some(LSP_CONTENT_MODIFIED) {
            Err(LspError::ContentModified)
        } else {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("language server returned an error");
            Err(LspError::Protocol(message.to_string()))
        }
    } else {
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    };
    let _ = tx.send(reply);
}

fn handle_notification(shared: &Shared, method: &str, params: Option<&Value>) {
    let params = params.cloned().unwrap_or(Value::Null);
    if let Some(tx) = lock(&shared.relays).get(method) {
        let _ = tx.send(params);
        return;
    }
    if method == "$/progress" {
        track_progress(shared, &params);
    }
    if method == "$/analyzerStatus" {
        track_analyzer_status(shared, &params);
    }
    if method == "experimental/serverStatus" {
        track_server_status(shared, &params);
    }
    if method == "intellij/workspaceImportState" {
        track_kotlin_import(shared, &params);
    }
    if method == ROSLYN_PROJECT_INIT_COMPLETE {
        // Counts as reported work: readiness then only needs the usual quiet
        // period, not the silent-server settle window.
        shared.saw_progress.store(true, Ordering::SeqCst);
        shared.roslyn_init_pending.store(false, Ordering::Release);
    }
    if method == "textDocument/publishDiagnostics" {
        if let Ok(parsed) = serde_json::from_value::<PublishDiagnosticsLoose>(params.clone()) {
            {
                let mut versions = lock(&shared.diag_versions);
                match parsed.version {
                    Some(v) => versions.insert(parsed.uri.clone(), v),
                    None => versions.remove(&parsed.uri),
                };
            }
            lock(&shared.diagnostics).insert(parsed.uri, parsed.diagnostics);
        }
    }
    let mut log = lock(&shared.notifications);
    if log.len() < 64 {
        log.push((method.to_string(), params));
    }
}

/// Follow `begin` / `end` of a work-done progress token.
///
/// `rust-analyzer` and `gopls` both report initial indexing this way, so an
/// outstanding token is the one portable signal that the server's answers are
/// not yet complete.
fn track_progress(shared: &Shared, params: &Value) {
    let Some(token) = params.get("token") else {
        return;
    };
    // Tokens may be strings or integers; normalise so both compare equal.
    let token = match token {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match params.pointer("/value/kind").and_then(Value::as_str) {
        Some("begin") => {
            shared.saw_progress.store(true, Ordering::SeqCst);
            // Indexing (or other work) resumed — do not keep a stale ready latch.
            shared.ready_confirmed.store(false, Ordering::Release);
            lock(&shared.active_progress).insert(token);
        }
        Some("end") => {
            lock(&shared.active_progress).remove(&token);
        }
        _ => {}
    }
}

/// Stable active-progress token shared by the Dart analysis notifications.
///
/// Both Dart notifications report the same internal analysis state machine, so
/// they drive one token: whichever fires last wins, matching the server's own
/// view. The name is server-specific enough never to collide with a genuine
/// `$/progress` token.
const ANALYZER_TOKEN: &str = "dart-analyzer";

/// Follow `$/analyzerStatus` (`{"isAnalyzing": bool}`) from the Dart analysis
/// server.
///
/// Dart does not guarantee `$/progress`. Without this mapping the settle
/// window in [`LspTransport::wait_until_ready`] expires while startup analysis
/// is still running, the server is declared ready too early, and the first
/// definition / references query comes back empty because the index is not
/// built yet. Tracked for every language — only the Dart server emits the
/// method, so the branch is inert elsewhere.
fn track_analyzer_status(shared: &Shared, params: &Value) {
    if let Some(is_analyzing) = params.get("isAnalyzing").and_then(Value::as_bool) {
        set_analyzer_active(shared, is_analyzing);
    }
}

/// Follow `experimental/serverStatus` (`{"analysis": {"isAnalyzing": bool},
/// "quiescent": bool}`), the newer Dart readiness notification.
fn track_server_status(shared: &Shared, params: &Value) {
    if let Some(is_analyzing) = params
        .pointer("/analysis/isAnalyzing")
        .and_then(Value::as_bool)
    {
        set_analyzer_active(shared, is_analyzing);
    }
    // `quiescent` is the server's own "everything has settled" signal.
    if params.get("quiescent").and_then(Value::as_bool) == Some(true) {
        lock(&shared.active_progress).remove(ANALYZER_TOKEN);
    }
}

/// `--system-path` for a JetBrains kotlin-lsp launch in `root`, unless the
/// user already passed one.
fn kotlin_system_path(spec: &ServerSpec, root: &Path) -> Option<PathBuf> {
    if spec.language != Language::Kotlin
        || !super::kotlin::is_jetbrains_server(&spec.command)
        || spec.args.iter().any(|a| a.starts_with("--system-path"))
    {
        return None;
    }
    Some(super::kotlin::system_path(root))
}

/// Progress token held while kotlin-lsp reports a project import in flight.
const KOTLIN_IMPORT_TOKEN: &str = "astrolabe:intellij-import";
/// Request id for the fire-and-forget `exportWorkspace` command; its reply
/// matches no pending waiter and is dropped.
const KOTLIN_EXPORT_ID: u64 = u64::MAX - 7;

/// Follow kotlin-lsp's `intellij/workspaceImportState`
/// (`{"phase": "...", "folders": [{"tool": "gradle", "status": "SUCCESS"}]}`).
///
/// Until the phase is `FINISHED` the server answers from a half-built (or
/// empty) project model, so the import counts as background work. After a
/// successful Gradle / Maven import the model is exported so the next start
/// can re-import it in seconds (see [`super::kotlin::exported_workspace`]).
fn track_kotlin_import(shared: &Shared, params: &Value) {
    let phase = params.get("phase").and_then(Value::as_str).unwrap_or("");
    if phase != "FINISHED" {
        shared.saw_progress.store(true, Ordering::SeqCst);
        shared.ready_confirmed.store(false, Ordering::Release);
        lock(&shared.active_progress).insert(KOTLIN_IMPORT_TOKEN.to_string());
        return;
    }
    lock(&shared.active_progress).remove(KOTLIN_IMPORT_TOKEN);
    let folders = params
        .get("folders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let built = !folders.is_empty()
        && folders.iter().all(|f| {
            f.get("status").and_then(Value::as_str) == Some("SUCCESS")
                && f.get("tool").and_then(Value::as_str) != Some("json")
        });
    let Some(root) = shared.cwd.as_deref() else {
        return;
    };
    if !built || shared.spec.language != Language::Kotlin {
        return;
    }
    let dir = super::kotlin::export_dir(&super::kotlin::system_path(root));
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = write_rpc(
        shared,
        &json!({
            "jsonrpc": "2.0",
            "id": KOTLIN_EXPORT_ID,
            "method": "workspace/executeCommand",
            "params": { "command": "exportWorkspace", "arguments": [dir.to_string_lossy()] },
        }),
    );
}

/// Apply a Dart analyzing transition to the shared progress state, mirroring
/// the `begin` / `end` handling of `$/progress`.
fn set_analyzer_active(shared: &Shared, is_analyzing: bool) {
    if is_analyzing {
        shared.saw_progress.store(true, Ordering::SeqCst);
        // Analysis started (or resumed) — any earlier ready latch is stale.
        shared.ready_confirmed.store(false, Ordering::Release);
        lock(&shared.active_progress).insert(ANALYZER_TOKEN.to_string());
    } else {
        lock(&shared.active_progress).remove(ANALYZER_TOKEN);
    }
}

fn handle_server_request(shared: &Shared, method: &str, params: Option<&Value>, id: &Value) {
    let params = params.cloned().unwrap_or(Value::Null);
    tracing::debug!(method, "lsp server request");
    let result = reverse_request_result(shared, method, &params);
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    if let Err(err) = write_rpc(shared, &response) {
        tracing::debug!(error = %err, method, "failed to answer server request");
    }
}

fn reverse_request_result(shared: &Shared, method: &str, params: &Value) -> Value {
    match method {
        "workspace/configuration" => {
            let items = params
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Value::Array(
                items
                    .iter()
                    .map(|item| {
                        let section = item.get("section").and_then(Value::as_str);
                        configuration_section(shared.spec.language, section)
                    })
                    .collect(),
            )
        }
        "workspace/workspaceFolders" => lock(&shared.workspace_folders)
            .clone()
            .unwrap_or(Value::Null),
        "workspace/applyEdit" => json!({ "applied": false }),
        "window/showDocument" => json!({ "success": false }),
        // VS Code answers by running `dotnet restore`. We never run builds or
        // package restores on the user's tree from a read-only query; an
        // empty answer lets Roslyn carry on with what is already restored.
        ROSLYN_PROJECT_NEEDS_RESTORE => {
            let projects = params
                .get("projectFilePaths")
                .map(Value::to_string)
                .unwrap_or_default();
            tracing::info!(
                projects = %projects,
                "Roslyn reports projects needing `dotnet restore`; not restoring"
            );
            Value::Null
        }
        _ => Value::Null,
    }
}

/// Answers for `workspace/configuration` pulls. Anything not listed gets
/// `null` ("use your defaults"), which is what every server expects from a
/// client without settings.
fn configuration_section(language: Language, section: Option<&str>) -> Value {
    match (language, section) {
        // bash-language-server only resolves symbols across files reachable
        // via `source` unless this is on, so go-to-definition from a test
        // script into the library it exercises comes back empty. It pulls
        // `bashIde` after `initialized` and a null answer resets the value
        // passed in initializationOptions back to false.
        (Language::Shell, Some("bashIde")) => shell_settings(),
        // Roslyn defaults this to true and, right after the project load,
        // runs a NuGet restore of the whole solution: on serilog it stayed
        // busy for over 100 s (so the first C# query waits for the readiness
        // timeout) and wrote `project.assets.json` / `*.nuget.*` into the
        // user's `obj/`. A read-only query must not restore packages; Roslyn
        // carries on with whatever is already restored.
        (language, Some(ROSLYN_AUTOMATIC_RESTORE_SECTION)) if is_roslyn(language) => {
            Value::Bool(false)
        }
        _ => Value::Null,
    }
}

fn shell_settings() -> Value {
    json!({ "includeAllWorkspaceSymbols": true })
}

fn rpc_id_u64(id: &Value) -> Option<u64> {
    match id {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok())),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[derive(serde::Deserialize)]
struct PublishDiagnosticsLoose {
    uri: String,
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    diagnostics: Vec<lsp_types::Diagnostic>,
}

/// Incremental LSP header/body splitter. Incomplete input is `Ok(None)`.
struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn ingest(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn next(&mut self) -> Result<Option<Vec<u8>>, LspError> {
        let Some(header_end) = find_header_end(&self.buf) else {
            if self.buf.len() > MAX_HEADER_BYTES {
                return Err(LspError::Protocol(format!(
                    "LSP header exceeded {MAX_HEADER_BYTES} bytes without a terminator"
                )));
            }
            return Ok(None);
        };
        let header = &self.buf[..header_end];
        let Some(content_len) = parse_content_length(header) else {
            return Err(LspError::Protocol(
                "LSP message missing Content-Length header".into(),
            ));
        };
        if content_len > MAX_BODY_BYTES {
            return Err(LspError::Protocol(format!(
                "LSP payload {content_len} exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        let total = header_end.saturating_add(content_len);
        if self.buf.len() < total {
            return Ok(None);
        }
        let body = self.buf[header_end..total].to_vec();
        self.buf.drain(..total);
        Ok(Some(body))
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    let crlf = memmem::find(buf, b"\r\n\r\n").map(|i| i + 4);
    let lf = memmem::find(buf, b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_content_length(header: &[u8]) -> Option<usize> {
    for line in header.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = memchr::memchr(b':', line) else {
            continue;
        };
        let name = line[..colon].trim_ascii();
        if !name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }
        let value = line[colon + 1..].trim_ascii();
        let s = std::str::from_utf8(value).ok()?;
        return s.parse().ok();
    }
    None
}

pub(crate) fn path_to_file_uri(path: &Path) -> String {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let rendered = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file://");
    if cfg!(windows) && !rendered.starts_with('/') {
        uri.push('/');
    }
    for &b in rendered.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

/// One process as seen by the OS: `(pid, ppid, pgid, rss_bytes)`.
type ProcRow = (u32, u32, u32, u64);

/// Sum RSS over `root` and everything below it: members of its process group
/// plus any descendant reached through parent links (a child that called
/// `setsid` leaves the group but not the tree).
fn tree_rss(rows: &[ProcRow], root: u32) -> Option<u64> {
    let mut members: HashSet<u32> = HashSet::new();
    members.insert(root);
    loop {
        let before = members.len();
        for &(pid, ppid, pgid, _) in rows {
            if pgid == root || members.contains(&ppid) {
                members.insert(pid);
            }
        }
        if members.len() == before {
            break;
        }
    }
    let mut total = 0u64;
    let mut seen_root = false;
    for &(pid, _, _, rss) in rows {
        if members.contains(&pid) {
            seen_root |= pid == root;
            total = total.saturating_add(rss);
        }
    }
    seen_root.then_some(total)
}

/// Resident-set size of the server's whole process tree, not just the wrapper.
fn rss_bytes(pid: u32) -> Option<u64> {
    tree_rss(&process_table()?, pid)
}

#[cfg(target_os = "linux")]
fn process_table() -> Option<Vec<ProcRow>> {
    // SAFETY: plain syscall.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut rows = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `comm` may contain spaces and parens: split after the last ')'.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // fields[0] is field 3 (state); ppid=4, pgrp=5, rss=24.
        let (Some(ppid), Some(pgid), Some(rss)) = (fields.get(1), fields.get(2), fields.get(21))
        else {
            continue;
        };
        let (Ok(ppid), Ok(pgid), Ok(rss)) =
            (ppid.parse::<u32>(), pgid.parse::<u32>(), rss.parse::<i64>())
        else {
            continue;
        };
        rows.push((pid, ppid, pgid, (rss.max(0) as u64).saturating_mul(page)));
    }
    Some(rows)
}

#[cfg(target_os = "macos")]
fn process_table() -> Option<Vec<ProcRow>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let nums: Vec<u64> = line
            .split_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        if let [pid, ppid, pgid, kb] = nums[..] {
            rows.push((
                pid as u32,
                ppid as u32,
                pgid as u32,
                kb.saturating_mul(1024),
            ));
        }
    }
    Some(rows)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_table() -> Option<Vec<ProcRow>> {
    None
}

#[cfg(test)]
mod tests {

    #[test]
    fn exit_grace_is_long_only_for_kotlin() {
        assert_eq!(exit_grace(Language::Kotlin), KOTLIN_EXIT_GRACE);
        assert!(exit_grace(Language::Rust) <= Duration::from_millis(500));
        assert!(exit_grace(Language::Python) <= Duration::from_millis(500));
    }

    use super::*;
    use crate::types::Confidence;
    use std::fs;
    use std::sync::atomic::AtomicU64;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    const FAKE_SERVER: &str = r#"
import json, os, sys, time

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if b":" in line:
            k, v = line.decode("utf-8", "replace").split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(n)
    if len(body) != n:
        return None
    return json.loads(body)

def frame(obj):
    data = json.dumps(obj, separators=(",", ":")).encode()
    return ("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data

def send(obj):
    sys.stdout.buffer.write(frame(obj))
    sys.stdout.buffer.flush()

def send_split(obj):
    blob = frame(obj)
    idx = blob.find(b"\r\n\r\n") + 4
    sys.stdout.buffer.write(blob[:idx])
    sys.stdout.buffer.flush()
    time.sleep(0.05)
    sys.stdout.buffer.write(blob[idx:])
    sys.stdout.buffer.flush()

def reply(msg, result):
    send({"jsonrpc": "2.0", "id": msg["id"], "result": result})

if "--flood" in sys.argv:
    chunk = b"e" * 65536
    for _ in range(64):
        sys.stderr.buffer.write(chunk)
        sys.stderr.buffer.flush()

BATCH = []
INITIALIZE_PARAMS = None
DOCS = {}
WATCHED = []
SAVES = []
EVENTS = []
BUFFERS = "--buffers" in sys.argv
HANG_REFS = "--hang-refs" in sys.argv

def is_ident(c):
    return c.isalnum() or c == "_"

def word_hits(uri, text, name):
    out = []
    for ln, line in enumerate(text.split("\n")):
        i = line.find(name)
        while i >= 0:
            j = i + len(name)
            if (i == 0 or not is_ident(line[i-1])) and (j >= len(line) or not is_ident(line[j])):
                out.append({"uri": uri, "range": {"start": {"line": ln, "character": i}, "end": {"line": ln, "character": j}}})
            i = line.find(name, i + 1)
    return out

while True:
    msg = read_msg()
    if msg is None:
        break
    method = msg.get("method")
    if method is None:
        continue
    if method == "exit":
        break
    if method in ("textDocument/didOpen", "textDocument/didClose"):
        EVENTS.append([method, msg["params"]["textDocument"]["uri"], msg["params"]["textDocument"].get("languageId")])
    if method == "textDocument/didOpen":
        td = msg["params"]["textDocument"]
        DOCS[td["uri"]] = {"version": td["version"], "text": td["text"]}
        continue
    if method == "textDocument/didChange":
        td = msg["params"]["textDocument"]
        DOCS[td["uri"]] = {"version": td["version"], "text": msg["params"]["contentChanges"][-1]["text"]}
        continue
    if method == "textDocument/didSave":
        SAVES.append(msg["params"]["textDocument"]["uri"])
        continue
    if method == "textDocument/didClose":
        DOCS.pop(msg["params"]["textDocument"]["uri"], None)
        continue
    if method == "workspace/didChangeWatchedFiles":
        WATCHED.extend(msg["params"]["changes"])
        continue
    if method == "initialized" or "id" not in msg:
        continue
    if method == "initialize":
        INITIALIZE_PARAMS = msg.get("params")
        reply(msg, {"capabilities": {"textDocumentSync": 1}, "serverInfo": {"name": "astrolabe-fake"}})
    elif method == "last-initialize":
        reply(msg, INITIALIZE_PARAMS)
    elif method == "listen-analyzer":
        send({"jsonrpc": "2.0", "method": "$/analyzerStatus", "params": {"isAnalyzing": True}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"analysis": {"isAnalyzing": True}}})
        reply(msg, {"ok": True})
    elif method == "finish-analyzer":
        send({"jsonrpc": "2.0", "method": "$/analyzerStatus", "params": {"isAnalyzing": False}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"analysis": {"isAnalyzing": False}, "quiescent": True}})
        reply(msg, {"ok": True})
    elif method == "quiesce-analyzer":
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"quiescent": True}})
        reply(msg, {"ok": True})
    elif method == "shutdown":
        reply(msg, None)
    elif method == "ping":
        params = msg.get("params") or {}
        if isinstance(params, dict) and params.get("batch"):
            BATCH.append(msg)
            if len(BATCH) >= int(params["batch"]):
                for m in reversed(list(BATCH)):
                    reply(m, m.get("params"))
                BATCH.clear()
        else:
            delay = 0.0
            if isinstance(params, dict):
                delay = float(params.get("delay") or 0)
            if delay:
                time.sleep(delay)
            reply(msg, params)
    elif method == "hang":
        time.sleep(30)
        reply(msg, {})
    elif method == "crash":
        os._exit(1)
    elif method == "fail":
        send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32000, "message": "intentional failure"}})
    elif method == "probe-reverse":
        send({"jsonrpc": "2.0", "id": "rev-1", "method": "window/workDoneProgress/create", "params": {"token": "t1"}})
        resp = read_msg()
        reply(msg, {"reverse_id": None if resp is None else resp.get("id"), "reverse_result": None if resp is None else resp.get("result")})
    elif method == "probe-config-items":
        items = (msg.get("params") or {}).get("items") or []
        send({"jsonrpc": "2.0", "id": 78, "method": "workspace/configuration", "params": {"items": items}})
        resp = read_msg()
        reply(msg, resp)
    elif method == "probe-config":
        send({"jsonrpc": "2.0", "id": 77, "method": "workspace/configuration", "params": {"items": [{"section": "a"}, {"section": "b"}]}})
        resp = read_msg()
        reply(msg, resp)
    elif method == "listen-diag":
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": "file:///tmp/fake.py",
            "diagnostics": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "message": "boom", "severity": 1}]
        }})
        reply(msg, {"ok": True})
    elif method == "listen-split":
        send_split({"jsonrpc": "2.0", "id": msg["id"], "result": {"split": True}})
    elif method == "listen-sticky":
        note = {"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": "file:///sticky.py", "diagnostics": []}}
        resp = {"jsonrpc": "2.0", "id": msg["id"], "result": {"sticky": True}}
        sys.stdout.buffer.write(frame(note) + frame(resp))
        sys.stdout.buffer.flush()
    elif method == "blob":
        n = int((msg.get("params") or {}).get("n") or 0)
        reply(msg, {"blob": "x" * n})
    elif method == "fake-state":
        reply(msg, {"docs": DOCS, "watched": WATCHED, "saves": SAVES, "events": EVENTS})
    elif method == "roslyn-init-done":
        send({"jsonrpc": "2.0", "method": "workspace/projectInitializationComplete"})
        reply(msg, {"ok": True})
    elif method == "probe-restore":
        send({"jsonrpc": "2.0", "id": 88, "method": "workspace/_roslyn_projectNeedsRestore", "params": {"projectFilePaths": ["/x/a.csproj"]}})
        resp = read_msg()
        reply(msg, resp)
    elif method == "textDocument/references" and HANG_REFS:
        time.sleep(30)
        reply(msg, [])
    elif method == "textDocument/references" and BUFFERS:
        params = msg.get("params") or {}
        doc = (params.get("textDocument") or {}).get("uri")
        pos = params.get("position") or {"line": 0, "character": 0}
        text = DOCS.get(doc, {}).get("text", "")
        line = text.split("\n")[pos["line"]] if pos["line"] < len(text.split("\n")) else ""
        a = b = pos["character"]
        while a > 0 and is_ident(line[a-1]):
            a -= 1
        while b < len(line) and is_ident(line[b]):
            b += 1
        name = line[a:b]
        hits = []
        for uri, d in DOCS.items():
            hits.extend(word_hits(uri, d["text"], name))
        reply(msg, hits)
    elif method in ("textDocument/references", "textDocument/definition"):
        params = msg.get("params") or {}
        doc = (params.get("textDocument") or {}).get("uri")
        pos = params.get("position") or {"line": 0, "character": 0}
        reply(msg, [{"uri": doc, "range": {"start": pos, "end": pos}}])
    elif method == "textDocument/diagnostic":
        reply(msg, {"kind": "full", "items": []})
    elif method == "textDocument/rename":
        params = msg.get("params") or {}
        doc = (params.get("textDocument") or {}).get("uri")
        pos = params.get("position") or {"line": 0, "character": 0}
        new = params.get("newName") or "x"
        reply(msg, {"changes": {doc: [{"range": {"start": pos, "end": pos}, "newText": new}]}})
    else:
        reply(msg, None)
"#;

    struct TmpDir(PathBuf);

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unique_temp() -> TmpDir {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-lsp-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        TmpDir(dir)
    }

    fn python3() -> PathBuf {
        for name in ["python3", "python"] {
            if let Ok(status) = Command::new(name)
                .args([
                    "-c",
                    "import sys; raise SystemExit(0 if sys.version_info[0] >= 3 else 1)",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                if status.success() {
                    return PathBuf::from(name);
                }
            }
        }
        panic!("transport tests require python3 on PATH");
    }

    fn spawn_fake(flood: bool, timeout: Duration) -> (LspTransport, TmpDir) {
        spawn_fake_as(Language::Python, flood, timeout)
    }

    fn spawn_fake_as(language: Language, flood: bool, timeout: Duration) -> (LspTransport, TmpDir) {
        let tmp = unique_temp();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).expect("write fake server");
        let mut args = vec!["-u".into(), script.to_string_lossy().into_owned()];
        if flood {
            args.push("--flood".into());
        }
        let spec = ServerSpec {
            language,
            command: python3(),
            args,
            install_hint: "install python3".into(),
        };
        let transport =
            LspTransport::spawn_in(&spec, &tmp.0, timeout).expect("spawn fake language server");
        (transport, tmp)
    }

    fn encode_frame(body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write!(buf, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        buf.extend_from_slice(body);
        buf
    }

    struct Chunked {
        chunks: Vec<Vec<u8>>,
        index: usize,
        offset: usize,
    }

    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.index][self.offset..];
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            self.offset += n;
            if self.offset >= self.chunks[self.index].len() {
                self.index += 1;
                self.offset = 0;
            }
            Ok(n)
        }
    }

    fn read_all_messages(reader: &mut impl Read) -> Vec<Vec<u8>> {
        let mut parser = FrameParser::new();
        let mut out = Vec::new();
        loop {
            while let Some(body) = parser.next().expect("frame") {
                out.push(body);
            }
            let mut tmp = [0u8; 64];
            match reader.read(&mut tmp).expect("read") {
                0 => break,
                n => parser.ingest(&tmp[..n]),
            }
        }
        while let Some(body) = parser.next().expect("frame") {
            out.push(body);
        }
        out
    }

    #[test]
    fn parser_half_packet_header_then_body() {
        let body = br#"{"ok":true}"#;
        let frame = encode_frame(body);
        let split = frame.iter().position(|&b| b == b'{').unwrap();
        let mut parser = FrameParser::new();
        parser.ingest(&frame[..split]);
        assert!(parser.next().unwrap().is_none());
        parser.ingest(&frame[split..]);
        assert_eq!(parser.next().unwrap().unwrap(), body);
    }

    #[test]
    fn parser_sticky_packets() {
        let a = encode_frame(br#"{"a":1}"#);
        let b = encode_frame(br#"{"b":2}"#);
        let mut combined = a;
        combined.extend_from_slice(&b);
        let mut parser = FrameParser::new();
        parser.ingest(&combined);
        assert_eq!(parser.next().unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(parser.next().unwrap().unwrap(), br#"{"b":2}"#);
        assert!(parser.next().unwrap().is_none());
    }

    #[test]
    fn parser_large_message() {
        let blob = "x".repeat(1_000_000);
        let json = serde_json::to_vec(&json!({ "blob": blob })).unwrap();
        let frame = encode_frame(&json);
        let mut parser = FrameParser::new();
        parser.ingest(&frame);
        assert_eq!(parser.next().unwrap().unwrap(), json);
    }

    #[test]
    fn parser_extra_headers_and_lf_only() {
        let body = br#"{}"#;
        let mut frame =
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\nContent-Length: 2\n\n"
                .to_vec();
        frame.extend_from_slice(body);
        let mut parser = FrameParser::new();
        parser.ingest(&frame);
        assert_eq!(parser.next().unwrap().unwrap(), body);
    }

    #[test]
    fn parser_rejects_missing_content_length() {
        let mut parser = FrameParser::new();
        parser.ingest(b"X-Ignore: 1\r\n\r\n{}");
        let err = parser.next().unwrap_err();
        assert!(matches!(err, LspError::Protocol(_)));
    }

    #[test]
    fn parser_rejects_oversize_payload() {
        let header = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        let mut parser = FrameParser::new();
        parser.ingest(header.as_bytes());
        let err = parser.next().unwrap_err();
        assert!(matches!(err, LspError::Protocol(_)));
    }

    #[test]
    fn parser_one_byte_reads_reassemble_two_frames() {
        let frames = [encode_frame(br#"{"a":1}"#), encode_frame(br#"{"b":2}"#)].concat();
        let mut reader = Chunked {
            chunks: frame_as_bytes(frames),
            index: 0,
            offset: 0,
        };
        let messages = read_all_messages(&mut reader);
        assert_eq!(
            messages,
            vec![br#"{"a":1}"#.to_vec(), br#"{"b":2}"#.to_vec()]
        );
    }

    fn frame_as_bytes(bytes: Vec<u8>) -> Vec<Vec<u8>> {
        bytes.into_iter().map(|b| vec![b]).collect()
    }

    #[test]
    fn missing_binary_is_unavailable() {
        let spec = ServerSpec {
            language: Language::Rust,
            command: PathBuf::from("/this/does/not/exist/astrolabe-lsp-missing"),
            args: vec![],
            install_hint: "install rust-analyzer".into(),
        };
        let err = LspTransport::spawn(&spec, Duration::from_secs(1)).unwrap_err();
        match err {
            LspError::Unavailable(Language::Rust, hint) => {
                assert!(hint.contains("rust-analyzer"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn ping_pairs_request_and_response() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("ping", json!({"token": "abc"})).unwrap();
        assert_eq!(result["token"], "abc");
        t.shutdown().unwrap();
    }

    #[test]
    fn handshake_initialize_and_shutdown() {
        let tmp = unique_temp();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: "install python3".into(),
        };
        let t = LspTransport::connect(&spec, &tmp.0, Duration::from_secs(5)).unwrap();
        let info = t.initialize_result().unwrap();
        assert_eq!(info["serverInfo"]["name"], "astrolabe-fake");
        assert!(t.is_alive());
        t.shutdown().unwrap();
    }

    #[test]
    fn root_uri_policy_omits_root_only_for_dart() {
        assert!(!send_root_uri(Language::Dart));
        for language in [
            Language::Python,
            Language::Go,
            Language::Java,
            Language::Rust,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::C,
            Language::Cpp,
            Language::ObjC,
            Language::ObjCpp,
            Language::Swift,
            Language::Php,
            Language::Vue,
        ] {
            assert!(send_root_uri(language), "{language:?} must keep its root");
        }
    }

    #[test]
    fn sourcekit_lsp_gets_background_indexing() {
        let nowhere = Path::new("/nonexistent-astrolabe-root");
        let sk = Path::new("/usr/bin/sourcekit-lsp");
        for language in [Language::Swift, Language::ObjC, Language::ObjCpp] {
            let opts = initialization_options_for(language, nowhere, sk).expect("options");
            assert_eq!(opts["backgroundIndexing"], json!(true));
        }
        // clangd serving ObjC takes flags, not initializationOptions.
        assert_eq!(
            initialization_options_for(Language::ObjC, nowhere, Path::new("clangd")),
            None
        );
    }

    #[test]
    fn initialization_options_are_dart_only() {
        let nowhere = Path::new("/nonexistent-astrolabe-root");
        let cmd = Path::new("/nonexistent-astrolabe-bin/server");
        assert_eq!(
            initialization_options_for(Language::Dart, nowhere, cmd),
            Some(json!({
                "onlyAnalyzeProjectsWithOpenFiles": false,
                "closingLabels": false,
                "outline": false,
                "flutterOutline": false,
            }))
        );
        for language in [Language::Python, Language::Go, Language::Rust] {
            assert_eq!(initialization_options_for(language, nowhere, cmd), None);
        }
    }

    #[test]
    fn typescript_points_at_global_tsserver_when_workspace_has_none() {
        let tmp = std::env::temp_dir().join(format!("astro-tsserver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let prefix = tmp.join("prefix");
        let tsserver = prefix.join("lib/node_modules/typescript/lib/tsserver.js");
        std::fs::create_dir_all(tsserver.parent().unwrap()).unwrap();
        std::fs::write(&tsserver, "").unwrap();
        let bin = prefix.join("bin/typescript-language-server");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        let root = tmp.join("repo");
        std::fs::create_dir_all(&root).unwrap();

        let opts = initialization_options_for(Language::TypeScript, &root, &bin).unwrap();
        let got = PathBuf::from(opts["tsserver"]["path"].as_str().unwrap());
        assert_eq!(
            std::fs::canonicalize(got).unwrap(),
            std::fs::canonicalize(&tsserver).unwrap()
        );
        let vue = initialization_options_for(Language::Vue, &root, &bin).unwrap();
        assert!(vue["typescript"]["tsdk"].as_str().unwrap().ends_with("lib"));

        // A workspace-local TypeScript wins: send nothing.
        let local = root.join("node_modules/typescript/lib/tsserver.js");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "").unwrap();
        assert_eq!(
            initialization_options_for(Language::TypeScript, &root, &bin),
            None
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stderr_summary_keeps_last_lines() {
        let text = "a\n\nb\nc\nd\ne\nf\nProject contains a Gemfile, but no Gemfile.lock\n";
        let s = summarize_stderr(text);
        assert!(s.ends_with("Project contains a Gemfile, but no Gemfile.lock"));
        assert!(!s.starts_with("a |"));
    }

    #[test]
    fn dart_initialize_nulls_root_but_keeps_workspace_folders() {
        let (t, tmp) = spawn_fake_as(Language::Dart, false, Duration::from_secs(5));
        t.initialize(&tmp.0).unwrap();
        let params = t.request("last-initialize", json!({})).unwrap();
        assert_eq!(params["rootUri"], Value::Null);
        assert_eq!(params["rootPath"], Value::Null);
        assert_eq!(
            params["workspaceFolders"],
            json!([{
                "uri": path_to_file_uri(&tmp.0),
                "name": tmp.0.file_name().and_then(|s| s.to_str()).unwrap_or("workspace"),
            }])
        );
        assert_eq!(
            params["initializationOptions"],
            json!({
                "onlyAnalyzeProjectsWithOpenFiles": false,
                "closingLabels": false,
                "outline": false,
                "flutterOutline": false,
            })
        );
        t.shutdown().unwrap();
    }

    #[test]
    fn non_dart_initialize_params_are_unchanged() {
        let (t, tmp) = spawn_fake_as(Language::Rust, false, Duration::from_secs(5));
        t.initialize(&tmp.0).unwrap();
        // Rebuild the exact pre-Dart-support shape: full-value equality here is
        // the byte-for-byte regression proof for every non-Dart language, as
        // equal JSON values serialise identically.
        let root_uri = path_to_file_uri(&tmp.0);
        let expected = json!({
            "processId": std::process::id(),
            "rootPath": tmp.0.to_string_lossy(),
            "rootUri": root_uri,
            "capabilities": client_capabilities(),
            "workspaceFolders": [{
                "uri": root_uri,
                "name": tmp.0.file_name().and_then(|s| s.to_str()).unwrap_or("workspace"),
            }],
            "clientInfo": { "name": "astrolabe", "version": env!("CARGO_PKG_VERSION") },
            "locale": "en",
            "trace": "off",
        });
        let params = t.request("last-initialize", json!({})).unwrap();
        assert_eq!(params, expected);

        // Explicit caller options still win over the per-language defaults.
        t.initialize_with(&tmp.0, Some(json!({ "custom": true })))
            .unwrap();
        let params = t.request("last-initialize", json!({})).unwrap();
        assert_eq!(params["initializationOptions"], json!({ "custom": true }));
        t.shutdown().unwrap();
    }

    /// S4: the client declares the ContentModified retries it performs.
    #[test]
    fn initialize_declares_stale_request_support() {
        let (session, _tmp) = spawn_session(&[], Duration::from_secs(10));
        let params = session
            .transport
            .request("last-initialize", json!({}))
            .unwrap();
        let stale = &params["capabilities"]["general"]["staleRequestSupport"];
        assert_eq!(stale["cancel"], json!(true));
        let methods: Vec<&str> = stale["retryOnContentModified"]
            .as_array()
            .expect("retryOnContentModified")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for m in [
            "textDocument/references",
            "textDocument/definition",
            "textDocument/hover",
        ] {
            assert!(methods.contains(&m), "{m} missing from {methods:?}");
        }
    }

    /// S4: Roslyn counts as busy until `workspace/projectInitializationComplete`,
    /// however quiet its `$/progress` is; other servers are unaffected.
    #[test]
    fn roslyn_is_busy_until_project_initialization_complete() {
        let (t, _tmp) = spawn_fake_as(Language::CSharp, false, Duration::from_secs(5));
        assert!(t.is_busy());
        assert!(
            !t.wait_until_ready(Duration::from_millis(300)),
            "must not be ready before the project-load signal"
        );
        assert!(!t.shared.ready_confirmed.load(Ordering::Acquire));
        t.request("roslyn-init-done", json!({})).unwrap();
        assert!(!roslyn_init_pending(&t.shared));
        let started = Instant::now();
        assert!(t.wait_until_ready(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "after the signal only the quiet period remains: {:?}",
            started.elapsed()
        );
        t.shutdown().unwrap();

        let (py, _tmp) = spawn_fake(false, Duration::from_secs(5));
        assert!(!roslyn_init_pending(&py.shared));
        py.shutdown().unwrap();
    }

    /// Roslyn's automatic NuGet restore is switched off through the
    /// configuration pull; other sections and other servers keep `null`.
    #[test]
    fn roslyn_automatic_restore_is_disabled() {
        let items = json!({"items": [
            {"section": "projects.dotnet_enable_automatic_restore"},
            {"section": "projects.dotnet_binary_log_path"},
            {"section": "navigation.dotnet_navigate_to_decompiled_sources"},
        ]});
        for lang in [Language::CSharp, Language::VisualBasic] {
            let (t, _tmp) = spawn_fake_as(lang, false, Duration::from_secs(5));
            let resp = t.request("probe-config-items", items.clone()).unwrap();
            assert_eq!(resp["id"], json!(78));
            assert_eq!(
                resp["result"],
                json!([false, null, null]),
                "{lang:?}: {resp}"
            );
            // The obj/ redirect lands in the scratch root, not in $HOME.
            assert!(
                _tmp.0
                    .join(".astrolabe-roslyn-test")
                    .join("astrolabe-obj.props")
                    .is_file(),
                "{lang:?}"
            );
            t.shutdown().unwrap();
        }
        let (py, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let resp = py.request("probe-config-items", items).unwrap();
        assert_eq!(resp["result"], json!([null, null, null]), "{resp}");
        py.shutdown().unwrap();
    }

    /// S4: `_roslyn_projectNeedsRestore` gets an empty success, not an error
    /// and not a restore.
    #[test]
    fn roslyn_project_needs_restore_is_answered_empty() {
        let (t, _tmp) = spawn_fake_as(Language::CSharp, false, Duration::from_secs(5));
        let resp = t.request("probe-restore", json!({})).unwrap();
        assert_eq!(resp["id"], json!(88));
        assert!(resp.get("error").is_none(), "{resp}");
        assert_eq!(resp["result"], Value::Null);
        t.shutdown().unwrap();
    }

    /// S4: a file created on disk is opened and closed once so servers that
    /// ignore watched-file `Created` events still read it; files of other
    /// languages, oversized files and files past the per-batch cap are not.
    #[test]
    fn created_files_are_announced_with_open_and_close() {
        let (session, tmp) = spawn_session(&[], Duration::from_secs(10));
        fs::write(tmp.0.join("new.py"), "def fresh():\n    pass\n").unwrap();
        fs::write(tmp.0.join("other.rs"), "fn main() {}\n").unwrap();
        fs::write(tmp.0.join("big.py"), "x = 1\n".repeat(200_000)).unwrap();
        let mut changes: Vec<FileChange> = ["new.py", "other.rs", "big.py", "missing.py"]
            .iter()
            .map(|p| FileChange {
                path: RelPath::new(*p),
                kind: FileChangeKind::Created,
            })
            .collect();
        for i in 0..(MAX_CREATED_ANNOUNCE + 6) {
            let name = format!("gen_{i:03}.py");
            fs::write(tmp.0.join(&name), "y = 2\n").unwrap();
            changes.push(FileChange {
                path: RelPath::new(name),
                kind: FileChangeKind::Created,
            });
        }
        session.files_changed(&changes);
        let state = session.transport.request("fake-state", json!({})).unwrap();
        let events = state["events"].as_array().unwrap();
        let new_uri = session.file_uri(&RelPath::new("new.py"));
        assert_eq!(
            events[0],
            json!(["textDocument/didOpen", new_uri, "python"])
        );
        assert_eq!(events[1], json!(["textDocument/didClose", new_uri, null]));
        let opens: Vec<&str> = events
            .iter()
            .filter(|e| e[0] == "textDocument/didOpen")
            .map(|e| e[1].as_str().unwrap())
            .collect();
        assert_eq!(opens.len(), MAX_CREATED_ANNOUNCE);
        assert!(!opens
            .iter()
            .any(|u| u.ends_with("other.rs") || u.ends_with("big.py")));
        assert_eq!(
            events.len(),
            2 * MAX_CREATED_ANNOUNCE,
            "every open is closed"
        );
        assert!(state["docs"].as_object().unwrap().is_empty());
        assert_eq!(state["watched"].as_array().unwrap().len(), changes.len());
        // Announced files are not tracked as open: the next query opens them
        // for real.
        assert_eq!(session.open_version(&RelPath::new("new.py")), None);
    }

    #[test]
    fn dart_analyzer_notifications_gate_readiness() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));

        t.request("listen-analyzer", json!({})).unwrap();
        assert!(t.is_busy(), "$/analyzerStatus must register active work");
        assert!(t.shared.saw_progress.load(Ordering::SeqCst));

        // quiescent alone must clear the shared token.
        t.request("quiesce-analyzer", json!({})).unwrap();
        assert!(!t.is_busy(), "quiescent must clear the analyzer token");

        // The token re-arms when analysis resumes.
        t.request("listen-analyzer", json!({})).unwrap();
        assert!(t.is_busy());
        t.request("finish-analyzer", json!({})).unwrap();
        assert!(!t.is_busy(), "isAnalyzing=false must clear the token");

        assert!(t.wait_until_ready(Duration::from_secs(3)));
        t.shutdown().unwrap();
    }

    #[test]
    fn concurrent_requests_match_by_id() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let t = Arc::new(t);
        let start = Instant::now();
        let mut joins = Vec::new();
        for i in 0..8 {
            let t = Arc::clone(&t);
            joins.push(thread::spawn(move || {
                t.request("ping", json!({"i": i, "batch": 8})).unwrap()
            }));
        }
        let mut seen = Vec::new();
        for join in joins {
            let value = join.join().expect("thread");
            seen.push(value["i"].as_u64().unwrap());
        }
        seen.sort();
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "batched in-flight pings took {:?}",
            start.elapsed()
        );
        t.shutdown().unwrap();
    }

    #[test]
    fn publish_diagnostics_are_captured() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        t.request("listen-diag", json!({})).unwrap();
        let diags = t.published_diagnostics();
        let items = diags.get("file:///tmp/fake.py").expect("uri");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "boom");
        t.shutdown().unwrap();
    }

    #[test]
    fn reverse_progress_request_is_answered() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("probe-reverse", json!({})).unwrap();
        assert_eq!(result["reverse_id"], "rev-1");
        t.shutdown().unwrap();
    }

    #[test]
    fn reverse_configuration_returns_matching_nulls() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("probe-config", json!({})).unwrap();
        assert_eq!(result["result"], json!([null, null]));
        t.shutdown().unwrap();
    }

    #[test]
    fn process_split_frame_still_completes() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("listen-split", json!({})).unwrap();
        assert_eq!(result["split"], true);
        t.shutdown().unwrap();
    }

    #[test]
    fn process_sticky_frames_dispatch_notification_and_response() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("listen-sticky", json!({})).unwrap();
        assert_eq!(result["sticky"], true);
        assert!(t.published_diagnostics().contains_key("file:///sticky.py"));
        t.shutdown().unwrap();
    }

    #[test]
    fn large_payload_round_trip() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(10));
        let n = 200_000;
        let result = t.request("blob", json!({ "n": n })).unwrap();
        assert_eq!(result["blob"].as_str().unwrap().len(), n);
        t.shutdown().unwrap();
    }

    #[test]
    fn timeout_returns_timeout_error() {
        let (t, _tmp) = spawn_fake(false, Duration::from_millis(150));
        let start = Instant::now();
        let err = t.request("hang", json!({})).unwrap_err();
        assert!(matches!(err, LspError::Timeout(_)));
        assert!(start.elapsed() < Duration::from_secs(2));
        t.shutdown().unwrap();
    }

    #[test]
    fn crash_surfaces_as_crashed_not_hang() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let err = t.request("crash", json!({})).unwrap_err();
        assert!(matches!(err, LspError::Crashed));
        let err = t.request("ping", json!({})).unwrap_err();
        assert!(matches!(err, LspError::Crashed));
    }

    #[test]
    fn protocol_error_is_surfaced() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let err = t.request("fail", json!({})).unwrap_err();
        match err {
            LspError::Protocol(message) => assert!(message.contains("intentional")),
            other => panic!("expected Protocol, got {other:?}"),
        }
        t.shutdown().unwrap();
    }

    #[test]
    fn stderr_flood_does_not_deadlock() {
        let (t, _tmp) = spawn_fake(true, Duration::from_secs(10));
        let result = t.request("ping", json!({"ok": true})).unwrap();
        assert_eq!(result["ok"], true);
        // Give the stderr drain thread a moment to finish counting bytes.
        // The atomic counter may lag slightly behind actual reads.
        std::thread::sleep(Duration::from_millis(50));
        let bytes = t.stderr_bytes();
        assert!(
            bytes >= 60 * 65536,
            "stderr was not drained: {} (expected at least {})",
            bytes,
            60 * 65536
        );
        t.shutdown().unwrap();
    }

    #[test]
    fn memory_bytes_is_observable_on_this_os() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let bytes = t.memory_bytes();
        assert!(bytes.is_some() && bytes.unwrap() > 0, "rss={bytes:?}");
        t.shutdown().unwrap();
    }

    #[test]
    fn transport_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LspTransport>();
        assert_send_sync::<LspSession>();
        assert_send_sync::<StdioSpawn>();
    }

    #[test]
    fn stdio_spawn_language_server_queries() {
        let tmp = unique_temp();
        fs::write(tmp.0.join("hello.py"), "def greet():\n    return 1\n").unwrap();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: "install python3".into(),
        };
        let spawn = StdioSpawn::new(&tmp.0).with_timeout(Duration::from_secs(5));
        let server = spawn.spawn(&spec, tmp.0.as_path()).unwrap();
        let path = RelPath::new("hello.py");
        let pos = Position {
            line: 0,
            character: 4,
        };
        let refs = server.references(&path, pos).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path.as_str(), "hello.py");
        let defs = server.definition(&path, pos).unwrap();
        assert_eq!(defs.len(), 1);
        let edit = server.prepare_rename(&path, pos, "hi").unwrap();
        assert_eq!(edit.edits.len(), 1);
        assert_eq!(edit.edits[0].1[0].new_text, "hi");
        assert!(server.diagnostics(&path).unwrap().is_empty());
        assert!(server.memory_bytes().unwrap() > 0);
        server.shutdown().unwrap();
    }

    #[test]
    fn workspace_edit_prefers_document_changes() {
        let tmp = unique_temp();
        let file_a = tmp.0.join("a.rs");
        let file_b = tmp.0.join("b.rs");
        fs::write(&file_a, "").unwrap();
        fs::write(&file_b, "").unwrap();
        let uri_a = path_to_file_uri(&file_a);
        let uri_b = path_to_file_uri(&file_b);

        let val = json!({
            "changes": {
                uri_a.clone(): [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "from_changes"
                    }
                ]
            },
            "documentChanges": [
                {
                    "textDocument": { "uri": uri_b },
                    "edits": [
                        {
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 5 }
                            },
                            "newText": "from_doc_changes"
                        }
                    ]
                }
            ]
        });
        let edit = workspace_edit_from_value(&val, &tmp.0);
        assert_eq!(edit.edits.len(), 1);
        assert_eq!(edit.edits[0].0.as_str(), "b.rs");
        assert_eq!(edit.edits[0].1[0].new_text, "from_doc_changes");

        // When documentChanges is empty, fall back to changes
        let val_empty_doc = json!({
            "changes": {
                uri_a: [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "from_changes"
                    }
                ]
            },
            "documentChanges": []
        });
        let edit_empty_doc = workspace_edit_from_value(&val_empty_doc, &tmp.0);
        assert_eq!(edit_empty_doc.edits.len(), 1);
        assert_eq!(edit_empty_doc.edits[0].0.as_str(), "a.rs");
        assert_eq!(edit_empty_doc.edits[0].1[0].new_text, "from_changes");
    }

    #[test]
    fn warm_lsp_wait_until_ready_fast_path() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        assert!(t.wait_until_ready(Duration::from_millis(500)));
        assert!(t.shared.ready_confirmed.load(Ordering::Acquire));

        // Subsequent call returns immediately via fast path
        let start = Instant::now();
        assert!(t.wait_until_ready(Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_millis(10));
        t.shutdown().unwrap();
    }

    #[test]
    fn write_rpc_failure_fails_pending_requests() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let (tx, rx) = mpsc::channel();
        lock(&t.shared.pending).insert(999, tx);

        // Kill child so write_frame fails on broken pipe
        let _ = lock(&t.shared.child).kill();
        let _ = lock(&t.shared.child).wait();

        // Small writes might succeed initially into kernel pipe buffers; write until EPIPE trips
        let mut res = Ok(());
        for _ in 0..100 {
            res = write_rpc(&t.shared, &json!({"test": 1, "payload": "x".repeat(4096)}));
            if res.is_err() {
                break;
            }
            // The writer thread notices the broken pipe asynchronously.
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(res, Err(LspError::Crashed)));
        assert_eq!(t.shared.life.load(Ordering::SeqCst), LIFE_DEAD);

        // Verify the pending request was woken up and failed with Crashed immediately
        let pending_res = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("pending request woke up");
        assert!(matches!(pending_res, Err(LspError::Crashed)));
    }

    #[test]
    fn diagnostics_fallback_empty_returns_protocol_error() {
        let tmp = unique_temp();
        let file = tmp.0.join("test.py");
        fs::write(&file, "x = 1\n").unwrap();

        let script = tmp.0.join("fake_lsp.py");
        // Fake server where textDocument/diagnostic fails with protocol error, and no publishDiagnostics are pushed
        let fake_code = r#"
import json, sys

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if b":" in line:
            k, v = line.decode("utf-8", "replace").split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(n)
    if len(body) != n:
        return None
    return json.loads(body)

def reply_err(msg, code, message):
    data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": code, "message": message}}, separators=(",", ":")).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if msg is None or msg.get("method") == "exit":
        break
    if "id" not in msg:
        continue
    if msg.get("method") == "initialize":
        data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}}, separators=(",", ":")).encode()
        sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data)
        sys.stdout.buffer.flush()
    elif msg.get("method") == "textDocument/diagnostic":
        reply_err(msg, -32601, "Method not found")
    else:
        data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": None}, separators=(",", ":")).encode()
        sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data)
        sys.stdout.buffer.flush()
"#;
        fs::write(&script, fake_code).unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: "install python3".into(),
        };
        let spawn = StdioSpawn::new(&tmp.0).with_timeout(Duration::from_secs(5));
        let server = spawn.spawn(&spec, tmp.0.as_path()).unwrap();
        let path = RelPath::new("test.py");
        let res = server.diagnostics(&path);
        match res {
            Err(LspError::Protocol(msg)) => {
                assert!(msg.contains("pull diagnostic unsupported and push diagnostics empty"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
        server.shutdown().unwrap();
    }

    /// End-to-end proof against the real `dart language-server` on this
    /// machine: the customised handshake (null root, Dart options) must let
    /// the server initialise, the analyzer-status notifications must drive
    /// `wait_until_ready`, and a definition query on `main` must return a
    /// location inside the repository. Run manually with
    /// `cargo test -p astrolabe-core dart_language_server_live -- --ignored --nocapture`.
    #[test]
    #[ignore = "spawns the local dart language-server against /Users/zhoulei/Project/AI-con"]
    fn dart_language_server_live() {
        let root = PathBuf::from("/Users/zhoulei/Project/AI-con");
        let main_abs = root.join("src/app/lib/main.dart");
        assert!(
            main_abs.exists(),
            "live test target missing: {}",
            main_abs.display()
        );

        let spec = ServerSpec {
            language: Language::Dart,
            command: PathBuf::from("dart"),
            args: vec!["language-server".into()],
            install_hint: "install the Dart SDK (https://dart.dev/get-dart)".into(),
        };
        let t = LspTransport::spawn_in(&spec, &root, Duration::from_secs(60))
            .expect("spawn dart language-server");
        let started = Instant::now();
        t.initialize(&root)
            .expect("initialize dart language-server");

        assert!(
            t.wait_until_ready(Duration::from_secs(180)),
            "dart server never became ready; notifications: {:?}",
            t.notifications()
        );
        eprintln!(
            "dart ready after {:?} (saw_progress={})",
            started.elapsed(),
            t.shared.saw_progress.load(Ordering::SeqCst)
        );
        for (method, params) in t.notifications() {
            if method.contains("Status") || method == "$/progress" {
                eprintln!("dart sent {method}: {params}");
            }
        }

        let text = fs::read_to_string(&main_abs).unwrap();
        let line = text
            .lines()
            .position(|l| l.contains("main("))
            .expect("main( in main.dart");
        let character = text.lines().nth(line).unwrap().find("main").unwrap() as u32;
        t.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_file_uri(&main_abs),
                    "languageId": "dart",
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .expect("didOpen main.dart");
        let result = t
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&main_abs) },
                    "position": { "line": line as u32, "character": character },
                }),
            )
            .expect("textDocument/definition on main");
        let locations = locations_from_value(&result, &root);
        assert!(
            !locations.is_empty(),
            "definition returned nothing: {result}"
        );
        for loc in &locations {
            let abs = root.join(loc.path.as_str());
            assert!(
                abs.starts_with(&root) && abs.exists(),
                "definition outside the repo or missing: {}",
                abs.display()
            );
        }
        eprintln!(
            "dart live definition of main: {}",
            locations
                .iter()
                .map(|l| format!("{}:{:?}", l.path.as_str(), l.range.start))
                .collect::<Vec<_>>()
                .join(", ")
        );
        t.shutdown().unwrap();
    }

    fn spawn_session(extra: &[&str], query_timeout: Duration) -> (LspSession, TmpDir) {
        let tmp = unique_temp();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).expect("write fake server");
        let mut args = vec!["-u".into(), script.to_string_lossy().into_owned()];
        args.extend(extra.iter().map(|a| a.to_string()));
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args,
            install_hint: "install python3".into(),
        };
        let transport = LspTransport::connect(&spec, &tmp.0, Duration::from_secs(10))
            .expect("connect fake language server");
        let session = LspSession::new(transport, tmp.0.clone(), query_timeout);
        (session, tmp)
    }

    fn ref_paths_and_lines(hits: &[Location]) -> Vec<(String, u32)> {
        hits.iter()
            .map(|l| (l.path.as_str().to_string(), l.range.start.line))
            .collect()
    }

    /// Regression: a document opened once used to stay frozen at its first
    /// contents, so a reference added afterwards was never found.
    #[test]
    fn edit_then_query_sees_new_reference() {
        use crate::lsp::queries::{find_references, SymbolAt};
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        assert!(session.wait_until_ready(Duration::from_secs(10)));
        let path = RelPath::new("m.py");
        fs::write(tmp.0.join("m.py"), "def target():\n    pass\n").unwrap();
        let at = SymbolAt::Position(Position {
            line: 0,
            character: 4,
        });

        let first = find_references(Some(&session), &tmp.0, &path, at);
        assert_eq!(first.confidence, Confidence::Exact, "{:?}", first.note);
        assert_eq!(ref_paths_and_lines(&first.value), vec![("m.py".into(), 0)]);
        assert_eq!(session.open_version(&path), Some(1));

        fs::write(tmp.0.join("m.py"), "def target():\n    pass\n\ntarget()\n").unwrap();
        let second = find_references(Some(&session), &tmp.0, &path, at);
        assert_eq!(second.confidence, Confidence::Exact, "{:?}", second.note);
        assert_eq!(
            ref_paths_and_lines(&second.value),
            vec![("m.py".into(), 0), ("m.py".into(), 3)],
            "the reference added after the first query must be found"
        );
        assert_eq!(session.open_version(&path), Some(2));

        // Unchanged content: no new version.
        let _ = find_references(Some(&session), &tmp.0, &path, at);
        assert_eq!(session.open_version(&path), Some(2));
    }

    #[test]
    fn did_save_follows_did_change_but_not_did_open() {
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        let path = RelPath::new("s.py");
        fs::write(tmp.0.join("s.py"), "x = 1\n").unwrap();
        session.ensure_open(&path).unwrap();
        let saves = |s: &LspSession| {
            let st = s.transport.request("fake-state", json!({})).unwrap();
            st["saves"].as_array().unwrap().len()
        };
        assert_eq!(saves(&session), 0, "first open sends no didSave");
        session.ensure_open(&path).unwrap();
        assert_eq!(saves(&session), 0, "unchanged content sends nothing");
        fs::write(tmp.0.join("s.py"), "x = 2\n").unwrap();
        session.ensure_open(&path).unwrap();
        assert_eq!(saves(&session), 1, "didChange is followed by didSave");
    }

    #[test]
    fn non_utf8_file_is_transcoded_for_the_server() {
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        let path = RelPath::new("g.py");
        let (bytes, _, _) = encoding_rs::GBK.encode("# 你好，这是一个注释\nx = 1\n");
        fs::write(tmp.0.join("g.py"), &bytes).unwrap();
        session.ensure_open(&path).unwrap();
        let st = session.transport.request("fake-state", json!({})).unwrap();
        let text = st["docs"].as_object().unwrap().values().next().unwrap()["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("你好"), "got {text:?}");
    }

    #[test]
    fn open_documents_are_capped_lru() {
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        for i in 0..=MAX_OPEN_DOCS {
            fs::write(tmp.0.join(format!("f{i}.py")), format!("x = {i}\n")).unwrap();
            session
                .ensure_open(&RelPath::new(format!("f{i}.py")))
                .unwrap();
        }
        assert_eq!(session.open_version(&RelPath::new("f0.py")), None);
        assert_eq!(session.open_version(&RelPath::new("f1.py")), Some(1));
        let state = session.transport.request("fake-state", json!({})).unwrap();
        assert_eq!(state["docs"].as_object().unwrap().len(), MAX_OPEN_DOCS);
    }

    #[test]
    fn uri_to_rel_fast_path_and_drive_letters() {
        let root = Path::new("/definitely/not/a/real/root");
        assert_eq!(
            uri_to_rel("file:///definitely/not/a/real/root/src/a%20b.rs", root),
            Some(RelPath::new("src/a b.rs"))
        );
        assert_eq!(uri_to_rel("file:///elsewhere/x.rs", root), None);
        assert_eq!(
            uri_to_rel("file:///definitely/not/a/real/root/../x.rs", root),
            None
        );
    }

    #[test]
    fn watched_file_events_are_batched() {
        let (session, _tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        let changes: Vec<FileChange> = (0..WATCHED_FILES_BATCH + 10)
            .map(|i| FileChange {
                path: RelPath::new(format!("w{i}.py")),
                kind: FileChangeKind::Created,
            })
            .collect();
        session.files_changed(&changes);
        let state = session.transport.request("fake-state", json!({})).unwrap();
        assert_eq!(
            state["watched"].as_array().unwrap().len(),
            WATCHED_FILES_BATCH + 10
        );
    }

    #[test]
    fn watcher_events_forwarded_and_deleted_docs_closed() {
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        let path = RelPath::new("m.py");
        fs::write(tmp.0.join("m.py"), "x = 1\n").unwrap();
        session.ensure_open(&path).unwrap();
        fs::remove_file(tmp.0.join("m.py")).unwrap();
        session.files_changed(&[
            FileChange {
                path: RelPath::new("n.py"),
                kind: FileChangeKind::Created,
            },
            FileChange {
                path: path.clone(),
                kind: FileChangeKind::Deleted,
            },
        ]);
        assert_eq!(session.open_version(&path), None);
        let state = session.transport.request("fake-state", json!({})).unwrap();
        let watched = state["watched"].as_array().unwrap();
        let types: Vec<u64> = watched
            .iter()
            .map(|c| c["type"].as_u64().unwrap())
            .collect();
        assert_eq!(types, vec![1, 3]);
        assert!(
            state["docs"].as_object().unwrap().is_empty(),
            "deleted doc must be closed"
        );
    }

    #[test]
    fn non_empty_results_while_warming_are_not_exact() {
        use crate::lsp::queries::{find_references, SymbolAt, STILL_INDEXING_NOTE};
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        fs::write(tmp.0.join("m.py"), "def target():\n    target()\n").unwrap();
        let at = SymbolAt::Position(Position {
            line: 0,
            character: 4,
        });
        // Fake server reports no progress: its first seconds count as warm-up.
        let r = find_references(Some(&session), &tmp.0, &RelPath::new("m.py"), at);
        assert_eq!(r.value.len(), 2);
        assert_ne!(r.confidence, Confidence::Exact);
        assert!(r.note.unwrap().contains(STILL_INDEXING_NOTE));
    }

    #[test]
    fn hung_references_fall_back_to_text_search() {
        use crate::lsp::queries::{find_references, SymbolAt};
        let (session, tmp) = spawn_session(&["--hang-refs"], Duration::from_millis(500));
        fs::write(tmp.0.join("m.py"), "def target():\n    pass\n").unwrap();
        fs::write(tmp.0.join("other.py"), "from m import target\ntarget()\n").unwrap();
        let at = SymbolAt::Position(Position {
            line: 0,
            character: 4,
        });
        let started = Instant::now();
        let r = find_references(Some(&session), &tmp.0, &RelPath::new("m.py"), at);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(r.confidence, Confidence::Syntactic);
        let note = r.note.unwrap();
        assert!(
            note.contains("timed out") && note.contains("text search"),
            "{note}"
        );
        assert_eq!(
            ref_paths_and_lines(&r.value),
            vec![
                ("m.py".into(), 0),
                ("other.py".into(), 0),
                ("other.py".into(), 1)
            ]
        );
    }

    #[test]
    fn hung_references_by_name_fall_back_to_text_search() {
        use crate::lsp::queries::{find_references, SymbolAt};
        let (session, tmp) = spawn_session(&["--hang-refs"], Duration::from_millis(500));
        fs::write(
            tmp.0.join("m.py"),
            "# target is documented here\ndef target():\n    pass\n",
        )
        .unwrap();
        fs::write(tmp.0.join("other.py"), "from m import target\ntarget()\n").unwrap();
        let started = Instant::now();
        let r = find_references(
            Some(&session),
            &tmp.0,
            &RelPath::new("m.py"),
            SymbolAt::Name("target"),
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(r.confidence, Confidence::Syntactic);
        let note = r.note.unwrap();
        assert!(
            note.contains("timed out") && note.contains("text search"),
            "{note}"
        );
        let hits = ref_paths_and_lines(&r.value);
        assert!(hits.contains(&("m.py".into(), 1)), "{hits:?}");
        assert!(hits.contains(&("other.py".into(), 1)), "{hits:?}");
    }

    // ---- audit fixes (lsp.md H1/H2/M3/M5/M6/M7/L1) ----

    fn process_gone(pid: i32) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                .unwrap_or(true),
        }
    }

    fn wait_gone(pid: i32, within: Duration) -> bool {
        let until = Instant::now() + within;
        while Instant::now() < until {
            if process_gone(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        process_gone(pid)
    }

    /// H1: a wrapper that leaves a grandchild holding our pipes used to make
    /// `drop` hang forever and leak the grandchild.
    #[cfg(target_os = "linux")]
    #[test]
    fn drop_kills_grandchildren_and_does_not_hang() {
        let tmp = unique_temp();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).unwrap();
        let pidfile = tmp.0.join("grandchild.pid");
        let spec = ServerSpec {
            language: Language::Python,
            command: PathBuf::from("sh"),
            args: vec![
                "-c".into(),
                format!(
                    "sleep 123 & echo $! > {}; exec {} -u {}",
                    pidfile.display(),
                    python3().display(),
                    script.display()
                ),
            ],
            install_hint: String::new(),
        };
        let transport = LspTransport::spawn_in(&spec, &tmp.0, Duration::from_secs(5)).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !pidfile.exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        let grandchild: i32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!process_gone(grandchild));

        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            drop(transport);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(8))
            .expect("drop must return even with a grandchild holding the pipes");
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(
            wait_gone(grandchild, Duration::from_secs(3)),
            "grandchild leaked"
        );
    }

    /// H2: a server that never reads stdin must not make a request outlive
    /// its timeout, nor make `drop` block.
    #[cfg(unix)]
    #[test]
    fn blocked_stdin_write_times_out_and_server_is_killed() {
        let tmp = unique_temp();
        let spec = ServerSpec {
            language: Language::Python,
            command: PathBuf::from("sleep"),
            args: vec!["60".into()],
            install_hint: String::new(),
        };
        let transport = LspTransport::spawn_in(&spec, &tmp.0, Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let big = "x".repeat(2 * 1024 * 1024);
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let res =
                transport.request_timeout("noop", json!({ "blob": big }), Duration::from_secs(2));
            let _ = tx.send(res.is_err());
            drop(transport);
        });
        let failed = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("request must return after its timeout, not block in write(2)");
        assert!(failed);
        assert!(started.elapsed() < Duration::from_secs(6));
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("drop of a stuck server must not hang");
    }

    /// M7: memory accounting covers the whole tree, not only the wrapper.
    #[test]
    fn tree_rss_sums_group_and_descendants() {
        let rows: Vec<ProcRow> = vec![
            (100, 1, 100, 10),   // wrapper, group leader
            (101, 100, 100, 50), // child in the group
            (102, 101, 102, 70), // grandchild that left the group via setsid
            (200, 1, 200, 999),  // unrelated
        ];
        assert_eq!(tree_rss(&rows, 100), Some(130));
        assert_eq!(tree_rss(&rows, 999), None);
    }

    /// M7 (live): a real wrapper's child shows up in the total.
    #[cfg(target_os = "linux")]
    #[test]
    fn memory_bytes_includes_child_processes() {
        let tmp = unique_temp();
        let spec = ServerSpec {
            language: Language::Python,
            command: PathBuf::from("sh"),
            args: vec![
                "-c".into(),
                format!(
                    "{} -c 'import time; x = bytearray(80*1024*1024); time.sleep(30)' & wait",
                    python3().display()
                ),
            ],
            install_hint: String::new(),
        };
        let transport = LspTransport::spawn_in(&spec, &tmp.0, Duration::from_secs(5)).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut seen = 0;
        while Instant::now() < until {
            seen = transport.memory_bytes().unwrap_or(0);
            if seen > 60 * 1024 * 1024 {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(seen > 60 * 1024 * 1024, "tree rss was {seen}");
    }

    /// M3: results outside the workspace keep their file: URI instead of
    /// vanishing.
    #[test]
    fn locations_outside_root_are_kept_as_file_uris() {
        let tmp = unique_temp();
        let value = json!([
            { "uri": format!("file://{}/a.py", tmp.0.display()),
              "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}} },
            { "uri": "file:///usr/lib/python3/os.py",
              "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}} },
        ]);
        let locs = locations_from_value(&value, &tmp.0);
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[0].path.as_str(), "a.py");
        assert!(locs[1].path.as_str().starts_with("file:///usr/lib"));
    }

    /// M5: a non-UTF-8 source file is still synced.
    #[test]
    fn non_utf8_file_is_synced_lossily() {
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        fs::write(tmp.0.join("gbk.py"), b"# \xc4\xe3\xba\xc3\nx = 1\n").unwrap();
        let path = RelPath::new("gbk.py");
        session
            .ensure_open(&path)
            .expect("non-UTF-8 file must open");
        assert_eq!(session.open_version(&path), Some(1));
    }

    /// M6: the push-diagnostics fallback must not match a different file
    /// whose path merely ends the same way.
    #[test]
    fn push_diagnostics_does_not_match_by_suffix() {
        let (session, tmp) = spawn_session(&[], Duration::from_secs(10));
        let other = format!("file://{}/src/mylib.rs", tmp.0.display());
        lock(&session.transport.shared.diagnostics).insert(
            other,
            vec![lsp_types::Diagnostic {
                message: "belongs to mylib.rs".into(),
                ..Default::default()
            }],
        );
        let path = RelPath::new("lib.rs");
        let uri = session.file_uri(&path);
        assert!(session.push_diagnostics(&path, &uri).is_empty());

        let own = format!("file://{}/lib.rs", tmp.0.display());
        lock(&session.transport.shared.diagnostics).insert(
            own,
            vec![lsp_types::Diagnostic {
                message: "mine".into(),
                ..Default::default()
            }],
        );
        let got = session.push_diagnostics(&path, &uri);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message, "mine");
    }

    /// M6: a publish for an older document version is not current.
    #[test]
    fn push_diagnostics_ignores_stale_versions() {
        let (session, tmp) = spawn_session(&["--buffers"], Duration::from_secs(10));
        let path = RelPath::new("v.py");
        fs::write(tmp.0.join("v.py"), "x = 1\n").unwrap();
        session.ensure_open(&path).unwrap();
        fs::write(tmp.0.join("v.py"), "x = 2\n").unwrap();
        session.ensure_open(&path).unwrap();
        assert_eq!(session.open_version(&path), Some(2));
        let uri = session.file_uri(&path);
        lock(&session.transport.shared.diagnostics).insert(
            uri.clone(),
            vec![lsp_types::Diagnostic {
                message: "old".into(),
                ..Default::default()
            }],
        );
        lock(&session.transport.shared.diag_versions).insert(uri.clone(), 1);
        assert!(session.push_diagnostics(&path, &uri).is_empty());
        lock(&session.transport.shared.diag_versions).insert(uri.clone(), 2);
        assert_eq!(session.push_diagnostics(&path, &uri).len(), 1);
    }

    /// L1: ContentModified retries share one deadline.
    #[cfg(unix)]
    #[test]
    fn content_modified_retries_share_one_deadline() {
        let tmp = unique_temp();
        let script = tmp.0.join("cm.py");
        fs::write(
            &script,
            r#"
import json, sys, time
def read_msg():
    h = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: return None
        if line in (b"\r\n", b"\n"): break
        k, v = line.decode().split(":", 1); h[k.strip().lower()] = v.strip()
    return json.loads(sys.stdin.buffer.read(int(h["content-length"])))
while True:
    m = read_msg()
    if m is None: break
    if "id" in m and m.get("method"):
        time.sleep(0.4)
        d = json.dumps({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32801,"message":"content modified"}}).encode()
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(d) + d); sys.stdout.buffer.flush()
"#,
        )
        .unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: String::new(),
        };
        let transport = LspTransport::spawn_in(&spec, &tmp.0, Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        let res =
            transport.request_timeout("textDocument/definition", json!({}), Duration::from_secs(1));
        assert!(res.is_err());
        // 4 attempts x (0.4s + 0.25s backoff) would be ~2.6s.
        assert!(
            started.elapsed() < Duration::from_millis(1900),
            "{:?}",
            started.elapsed()
        );
    }
}
