//! Restart-surviving background terminal jobs.
//!
//! A normal [`LocalTerminalBackend`](super::LocalTerminalBackend) owns its
//! children in an in-process actor. Explicit background commands instead go
//! through this small supervisor: the current Grok executable is re-entered as
//! a hidden worker, and the worker owns the shell's process group while the
//! parent only keeps a durable task record. This makes task ids useful after a
//! Grok/MCP process restart without exposing a terminal window.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use super::terminal::LocalTerminalBackend;
use crate::computer::task_log;
use crate::computer::types::{
    BackgroundHandle, ComputerError, KillOutcome, KillSource, OutputEncoding, TaskKind,
    TaskSnapshot, TerminalBackend, TerminalRunRequest,
};
use crate::notification::types::ToolNotificationHandle;
use crate::util::ShellEnvironmentPolicy;

pub const WORKER_SUBCOMMAND: &str = "__grok-terminal-background";
const SCHEMA_VERSION: u32 = 1;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Exit polling for a durable worker. The registry only reconciles an exit when
/// a caller asks for the task, so the owning process watches the worker itself.
const COMPLETION_WATCH_INTERVAL: Duration = Duration::from_millis(500);
/// Retry cadence for publishing the durable record.
const PUBLISH_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Retries for a routine publish: about five seconds of a transiently locked
/// record. Another process holding the record open without delete sharing (an
/// indexer, a scanner, a backup tool) only makes `MoveFileExW` fail while it
/// holds it, so retrying is enough.
const PUBLISH_RETRY_ATTEMPTS: u32 = 50;
/// Retries for a finished task's result: about five minutes, so a command that
/// succeeded is never reported as a lost worker just because the record stayed
/// locked for longer.
const PUBLISH_FINAL_ATTEMPTS: u32 = 3_000;
/// Cap on worker diagnostics written per run so a permanently locked record
/// cannot fill the task directory.
const MAX_WORKER_LOG_LINES: u32 = 20;

/// Marker written into a task directory once that task's completion has been
/// delivered, so it is delivered exactly once.
///
/// Absence means "not delivered yet" — that is the whole `pending` state, and
/// it needs no writer: a detached task that finishes while nobody is listening
/// is pending by construction. Creation is O_EXCL, so two sessions racing for
/// the same completion (or a session racing the dying-of-old-age case) produce
/// exactly one claim.
const DELIVERY_MARKER: &str = "delivery.json";

/// Shared handle used by a local terminal backend to manage durable jobs.
#[derive(Clone)]
pub struct TaskRegistry {
    root: Arc<PathBuf>,
    executable: Arc<PathBuf>,
    search_shadows: super::SearchShadowConfig,
    login_shell_capture: bool,
    shell_env_policy: Option<ShellEnvironmentPolicy>,
    persistent_shell: bool,
}

impl TaskRegistry {
    pub(crate) fn new_with_config(
        search_shadows: super::SearchShadowConfig,
        login_shell_capture: bool,
        shell_env_policy: Option<ShellEnvironmentPolicy>,
        persistent_shell: bool,
    ) -> Self {
        let root = std::env::var_os("GROK_BACKGROUND_TASK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::util::grok_home::grok_home().join("background-tasks"));
        let executable = std::env::var_os("GROK_BACKGROUND_WORKER_EXECUTABLE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("grok-zh"));
        Self {
            root: Arc::new(root),
            executable: Arc::new(executable),
            search_shadows,
            login_shell_capture,
            shell_env_policy,
            persistent_shell,
        }
    }

    pub fn new() -> Self {
        Self::new_with_config(
            crate::computer::local::SearchShadowConfig::default(),
            true,
            None,
            false,
        )
    }

    #[cfg(test)]
    fn with_root(root: PathBuf) -> Self {
        let mut registry = Self::new_with_config(
            crate::computer::local::SearchShadowConfig::default(),
            true,
            None,
            false,
        );
        registry.root = Arc::new(root);
        registry
    }

    /// A registry whose worker is the built `grok-zh` binary.
    ///
    /// Only `xai-grok-pager-bin`'s `main` calls `maybe_run_worker`, so spawning
    /// a real durable task needs that binary rather than the test executable.
    /// `None` means it has not been built, and the caller must skip rather than
    /// fail: a test binary is not a defect.
    #[cfg(test)]
    fn for_live_worker() -> Option<Self> {
        // Windows turns the crate name `grok-zh` into `grok_zh.exe`, so try
        // both spellings rather than assuming the dashed one exists.
        let mut candidates = Vec::new();
        let mut dir = std::env::current_exe()
            .ok()?
            .parent()
            .map(Path::to_path_buf);
        while let Some(current) = dir {
            for name in ["grok-zh", "grok_zh"] {
                candidates.push(current.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)));
            }
            dir = current.parent().map(Path::to_path_buf);
        }
        let executable = candidates.into_iter().find(|path| path.is_file())?;
        let mut registry = Self::new_with_config(
            crate::computer::local::SearchShadowConfig::default(),
            true,
            None,
            false,
        );
        registry.executable = Arc::new(executable);
        Some(registry)
    }

    fn job_dir(&self, task_id: &str) -> PathBuf {
        let valid = !task_id.is_empty()
            && task_id.len() <= 128
            && task_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if valid {
            self.root.join(task_id)
        } else {
            PathBuf::new()
        }
    }

    async fn start(&self, request: TerminalRunRequest) -> Result<BackgroundHandle, ComputerError> {
        std::fs::create_dir_all(self.root.as_ref())
            .map_err(|e| ComputerError::io(format!("create background-task registry: {e}")))?;
        let task_id = uuid::Uuid::now_v7().to_string();
        let directory = self.job_dir(&task_id);
        if directory.as_os_str().is_empty() {
            return Err(ComputerError::io("invalid background task id"));
        }
        std::fs::create_dir(&directory)
            .map_err(|e| ComputerError::io(format!("create background-task directory: {e}")))?;

        // Cloned before `from_request` consumes the request. The worker cannot
        // carry this handle (it is not persistable), so the completion
        // notification for a durable task can only originate in this process.
        let notification_handle = request.notification_handle.clone();
        let spec = PersistentTaskSpec::from_request(
            task_id.clone(),
            request,
            self.search_shadows,
            self.login_shell_capture,
            self.shell_env_policy.clone(),
            self.persistent_shell,
        );
        write_json(&directory.join("spec.json"), &spec)?;
        let mut child = spawn_worker(&self.executable, &directory)?;
        let worker_pid = child.id();

        let state_path = directory.join("state.json");
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(state) = read_state(&state_path)
                && state.ready
                && state.worker_pid == Some(worker_pid)
            {
                drop(child);
                spawn_completion_watcher(
                    directory.clone(),
                    task_id.clone(),
                    worker_pid,
                    spec.detach,
                    notification_handle,
                );
                return Ok(BackgroundHandle {
                    task_id,
                    output_file: spec.output_file,
                    pid: state.worker_pid,
                });
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|e| ComputerError::io(format!("poll background worker: {e}")))?
            {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(ComputerError::io(format!(
                    "background worker exited before becoming ready (status {status})"
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&directory);
                return Err(ComputerError::io(
                    "background worker did not become ready within 10 seconds",
                ));
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    #[cfg(test)]
    pub(crate) fn worker_is_alive(&self, task_id: &str) -> bool {
        let directory = self.job_dir(task_id);
        if directory.as_os_str().is_empty() {
            return false;
        }
        read_state(&directory.join("state.json"))
            .is_some_and(|state| state.worker_pid.is_some_and(worker_process_is_alive))
    }

    pub(crate) async fn get_task(&self, task_id: &str) -> Option<TaskSnapshot> {
        let directory = self.job_dir(task_id);
        if directory.as_os_str().is_empty() {
            return None;
        }
        let mut state = read_state(&directory.join("state.json"))?;
        if state.worker_pid != Some(std::process::id()) {
            reconcile_worker_state(&mut state);
        }
        if !state.ready {
            return None;
        }
        hydrate_output(&mut state.snapshot).await;
        Some(state.snapshot)
    }

    pub(crate) async fn list_tasks(&self) -> Vec<TaskSnapshot> {
        self.list_tasks_inner(true).await
    }

    /// [`Self::list_tasks`] without reading each task's output log.
    ///
    /// The full list hydrates every record from disk; a UI enumerating the whole
    /// durable directory cannot afford that, and it only needs ids, owners and
    /// completion state.
    pub(crate) async fn list_tasks_light(&self) -> Vec<TaskSnapshot> {
        self.list_tasks_inner(false).await
    }

    async fn list_tasks_inner(&self, hydrate: bool) -> Vec<TaskSnapshot> {
        let Ok(entries) = std::fs::read_dir(self.root.as_ref()) else {
            return Vec::new();
        };
        let mut tasks = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path().join("state.json");
            if let Some(mut state) = read_state(&path) {
                if state.worker_pid != Some(std::process::id()) {
                    reconcile_worker_state(&mut state);
                }
                if state.ready {
                    if hydrate {
                        hydrate_output(&mut state.snapshot).await;
                    }
                    tasks.push(state.snapshot);
                }
            }
        }
        tasks.sort_by_key(|task| task.start_time);
        tasks
    }

    /// Claim completions of detached tasks that no session has delivered yet.
    ///
    /// A detached task is confirmed to outlive its session, so the completion
    /// watcher that would have reported it in-session is gone once that session
    /// ends; without this pass the result is lost. The claiming session gets
    /// the recent ones back as plain snapshots (no output hydration — the log
    /// is still on disk) and marks the rest delivered silently, so the window
    /// only rations attention: a task that finished longer ago than `window` is
    /// consumed without being reported, never reported later.
    ///
    /// `claimed_by` names the claiming session in the marker for diagnostics.
    pub(crate) fn claim_late_deliveries(
        &self,
        window: Duration,
        claimed_by: Option<&str>,
    ) -> Vec<TaskSnapshot> {
        let Ok(entries) = std::fs::read_dir(self.root.as_ref()) else {
            return Vec::new();
        };
        let now = std::time::SystemTime::now();
        let mut claimed = Vec::new();
        for entry in entries.flatten() {
            let directory = entry.path();
            if directory.join(DELIVERY_MARKER).exists() {
                continue;
            }
            // The durable spec is the authority on `detach` (see `is_detached`).
            let Some(spec) = read_spec(&directory.join("spec.json")) else {
                continue;
            };
            if !spec.detach {
                continue;
            }
            let Some(state) = read_state(&directory.join("state.json")) else {
                continue;
            };
            // Persisted state only: completion is written by the worker itself,
            // and a record that was never published is not a result to deliver.
            if !state.ready || !state.snapshot.completed {
                continue;
            }
            let ended = state.snapshot.end_time.unwrap_or(now);
            let recent = now
                .duration_since(ended)
                .map(|age| age <= window)
                .unwrap_or(true);
            if !claim_delivery(&directory, true, claimed_by) {
                continue;
            }
            if recent {
                claimed.push(state.snapshot);
            }
        }
        claimed.sort_by_key(|task| task.end_time);
        claimed
    }

    pub(crate) async fn wait_for_completion(
        &self,
        task_id: &str,
        timeout: Option<Duration>,
    ) -> Option<TaskSnapshot> {
        if self.job_dir(task_id).as_os_str().is_empty() {
            return None;
        }
        let timeout = match timeout {
            None => None,
            Some(value) => Some(
                tokio::time::Instant::now()
                    .checked_add(value)
                    .unwrap_or(tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600)),
            ),
        };
        loop {
            let snapshot = self.get_task(task_id).await?;
            if snapshot.completed {
                return Some(snapshot);
            }
            if timeout.is_none_or(|deadline| tokio::time::Instant::now() >= deadline) {
                return Some(snapshot);
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    pub(crate) async fn kill_task(&self, task_id: &str, _source: KillSource) -> KillOutcome {
        let directory = self.job_dir(task_id);
        if directory.as_os_str().is_empty() {
            return KillOutcome::NotFound;
        }
        let Some(state) = read_state(&directory.join("state.json")) else {
            return KillOutcome::NotFound;
        };
        if state.snapshot.completed {
            return KillOutcome::AlreadyExited;
        }
        if !state.worker_pid.is_some_and(worker_process_is_alive) {
            return KillOutcome::AlreadyExited;
        }
        if std::fs::write(directory.join("kill.request"), b"1").is_err() {
            return KillOutcome::NotFound;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(next) = read_state(&directory.join("state.json")) {
                if next.snapshot.completed {
                    return if next.snapshot.explicitly_killed {
                        KillOutcome::Killed
                    } else {
                        KillOutcome::AlreadyExited
                    };
                }
            }
            if !state.worker_pid.is_some_and(worker_process_is_alive) {
                return KillOutcome::Killed;
            }
            if tokio::time::Instant::now() >= deadline {
                // The worker may be stuck in its shell's own teardown. Force
                // termination by PID so an explicit kill remains bounded.
                if let Some(pid) = state.worker_pid {
                    terminate_worker_process(pid);
                }
                return KillOutcome::Killed;
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    /// Delete a task's whole record directory.
    ///
    /// Restricted to finished tasks: a live worker would keep writing into a
    /// directory that no longer exists. Worker liveness is deliberately not
    /// consulted — the published exit is what makes a record removable, and a
    /// finished task's `worker_pid` may already belong to an unrelated process by
    /// the time a later session looks at it. A record still marked running is
    /// reconciled to completed by the listing once its worker is gone, so a
    /// crashed task cannot strand its own record here. A record whose state
    /// cannot be read at all is removed: nothing else can consume it, and the
    /// caller asked for the row to go away.
    pub(crate) async fn delete_task(&self, task_id: &str) -> bool {
        let directory = self.job_dir(task_id);
        if directory.as_os_str().is_empty() || !directory.exists() {
            return false;
        }
        let unfinished = read_state(&directory.join("state.json"))
            .is_some_and(|state| !state.snapshot.completed);
        if unfinished {
            return false;
        }
        std::fs::remove_dir_all(&directory).is_ok()
    }

    /// Whether a task was explicitly detached from its session.
    ///
    /// Read from the durable spec rather than the snapshot: this flag decides
    /// whether teardown is allowed to stop the process, and a missing or
    /// unreadable spec must fail toward "not detached" so cleanup still runs.
    fn is_detached(&self, task_id: &str) -> bool {
        let directory = self.job_dir(task_id);
        if directory.as_os_str().is_empty() {
            return false;
        }
        read_spec(&directory.join("spec.json")).is_some_and(|spec| spec.detach)
    }

    pub(crate) async fn kill_all_by_owner(&self, owner: Option<&str>) {
        for task in self.list_tasks().await {
            if !task.completed
                && owner.is_none_or(|value| task.owner_session_id.as_deref() == Some(value))
                // A detached task was confirmed by the user to outlive the
                // session, so teardown leaves it running.
                && !self.is_detached(&task.task_id)
            {
                let _ = self.kill_task(&task.task_id, KillSource::Teardown).await;
            }
        }
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistentTaskSpec {
    schema_version: u32,
    task_id: String,
    command: String,
    cwd: PathBuf,
    env: HashMap<String, String>,
    timeout_ms: Option<u64>,
    output_byte_limit: usize,
    output_file: PathBuf,
    output_encoding: Option<OutputEncoding>,
    tool_call_id: String,
    display_command: Option<String>,
    kind: TaskKind,
    owner_session_id: Option<String>,
    description: Option<String>,
    /// Survives session teardown. Set only after a user confirmation; see
    /// [`crate::computer::types::TerminalRunRequest::detach`].
    detach: bool,
    login_shell_capture: bool,
    shell_env_policy: Option<ShellEnvironmentPolicy>,
    persistent_shell: bool,
    search_shadows: super::SearchShadowConfig,
}

impl PersistentTaskSpec {
    fn from_request(
        task_id: String,
        request: TerminalRunRequest,
        search_shadows: super::SearchShadowConfig,
        login_shell_capture: bool,
        shell_env_policy: Option<ShellEnvironmentPolicy>,
        persistent_shell: bool,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            task_id,
            command: request.command,
            cwd: request.working_directory,
            env: request.env,
            timeout_ms: (request.timeout != Duration::MAX)
                .then(|| request.timeout.as_millis().min(u64::MAX as u128) as u64),
            output_byte_limit: request.output_byte_limit,
            output_file: request.output_file,
            output_encoding: request.output_encoding,
            tool_call_id: request.tool_call_id,
            display_command: request.display_command,
            kind: request.kind,
            owner_session_id: request.owner_session_id,
            description: request.description,
            detach: request.detach,
            login_shell_capture,
            shell_env_policy,
            persistent_shell,
            search_shadows,
        }
    }

    fn into_request(self) -> TerminalRunRequest {
        let timeout = self
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::MAX);
        TerminalRunRequest {
            command: self.command,
            working_directory: self.cwd,
            env: self.env,
            timeout,
            output_byte_limit: self.output_byte_limit,
            output_file: self.output_file,
            output_encoding: self.output_encoding,
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: self.tool_call_id,
            display_command: self.display_command,
            auto_background_on_timeout: false,
            // The worker runs the command once; re-entering the registry from
            // inside a worker would spawn a nested worker.
            detach: self.detach,
            foreground_block_budget: None,
            kind: self.kind,
            owner_session_id: self.owner_session_id,
            description: self.description,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistentTaskState {
    ready: bool,
    worker_pid: Option<u32>,
    snapshot: TaskSnapshot,
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ComputerError> {
    atomic_write(
        path,
        &serde_json::to_vec(value)
            .map_err(|e| ComputerError::io(format!("encode background-task record: {e}")))?,
    )
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ComputerError> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes)
        .map_err(|e| ComputerError::io(format!("write background-task record: {e}")))?;
    replace_file(&temp, path)
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, path: &Path) -> Result<(), ComputerError> {
    std::fs::rename(temp, path)
        .map_err(|e| ComputerError::io(format!("publish background-task record: {e}")))
}

#[cfg(windows)]
fn replace_file(temp: &Path, path: &Path) -> Result<(), ComputerError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};
    let temp_wide = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both strings are NUL-terminated and live for the call.
    let moved = unsafe {
        MoveFileExW(
            windows::core::PCWSTR(temp_wide.as_ptr()),
            windows::core::PCWSTR(path_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    };
    if moved.is_ok() {
        Ok(())
    } else {
        let _ = std::fs::remove_file(temp);
        Err(ComputerError::io(format!(
            "publish background-task record: {}",
            std::io::Error::last_os_error()
        )))
    }
}

fn read_state(path: &Path) -> Option<PersistentTaskState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_spec(path: &Path) -> Option<PersistentTaskSpec> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomically claim a task's completion for delivery.
///
/// `true` means this caller owns the one delivery; `false` means it already
/// happened, or a concurrent session won the race. The body is written for
/// whoever inspects a directory later — existence alone is the signal
/// (see [`DELIVERY_MARKER`]) — so it records whether the completion was
/// reported to a later session (`late`) and by whom.
fn claim_delivery(directory: &Path, late: bool, claimed_by: Option<&str>) -> bool {
    use std::io::Write as _;

    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(DELIVERY_MARKER))
    else {
        return false;
    };
    let body = serde_json::json!({
        "state": "delivered",
        "late": late,
        "claimed_by": claimed_by,
        "claimed_at": chrono::Utc::now().to_rfc3339(),
    });
    if let Ok(bytes) = serde_json::to_vec(&body) {
        let _ = file.write_all(&bytes);
    }
    true
}

fn reconcile_worker_state(state: &mut PersistentTaskState) {
    if state.snapshot.completed {
        return;
    }
    if state.worker_pid.is_some_and(worker_process_is_alive) {
        return;
    }
    state.snapshot.completed = true;
    state
        .snapshot
        .end_time
        .get_or_insert_with(std::time::SystemTime::now);
    if state.snapshot.signal.is_none() {
        state.snapshot.signal = Some("worker_exited".to_string());
    }
}

/// Reports a durable task's exit to the session that started it.
///
/// The worker runs as a separate process and its own completion notification
/// goes to a no-op handle (see [`PersistentTaskSpec::into_request`]), while this
/// registry only reconciles an exit when a caller asks for the task. Without a
/// watcher nothing ever emits `TaskCompleted`, so the pager's task row stays
/// running until the session ends. The handle lives in this process on purpose:
/// a task re-discovered by a later session must not notify again.
///
/// A detached task additionally has the cross-session path, because the watcher
/// dies with its session: see [`TaskRegistry::claim_late_deliveries`]. Both
/// paths take the same one-shot delivery claim, so whichever reaches the
/// completion first is the only one that reports it.
fn spawn_completion_watcher(
    directory: PathBuf,
    task_id: String,
    worker_pid: u32,
    detach: bool,
    notification_handle: ToolNotificationHandle,
) {
    tokio::spawn(async move {
        while worker_process_is_alive(worker_pid) {
            sleep(COMPLETION_WATCH_INTERVAL).await;
        }
        let Some(mut state) = read_state(&directory.join("state.json")) else {
            return;
        };
        if !state.ready {
            return;
        }
        reconcile_worker_state(&mut state);
        if !state.snapshot.completed {
            return;
        }
        if detach
            && !claim_delivery(
                &directory,
                false,
                state.snapshot.owner_session_id.as_deref(),
            )
        {
            // A later session already claimed this completion as a late
            // delivery; reporting it here too would double-report it.
            return;
        }
        hydrate_output(&mut state.snapshot).await;
        let mut snapshot = state.snapshot;
        snapshot.task_id = task_id;
        notification_handle.send_task_complete(snapshot);
    });
}

fn terminate_worker_process(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }) else {
            return;
        };
        let _ = unsafe { TerminateProcess(handle, 1) };
        let _ = unsafe { CloseHandle(handle) };
    }
}

fn worker_process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        !xai_tty_utils::process_not_running(pid)
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
            return false;
        };
        let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
        let _ = unsafe { CloseHandle(handle) };
        running
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

async fn hydrate_output(snapshot: &mut TaskSnapshot) {
    if snapshot.output_file.as_os_str().is_empty() {
        return;
    }
    let (output, short) =
        task_log::read_prefix(&snapshot.output_file, task_log::MAX_SNAPSHOT_BYTES).await;
    snapshot.output = output;
    snapshot.truncated |= short;
}

fn spawn_worker(executable: &Path, directory: &Path) -> Result<Child, ComputerError> {
    let mut command = Command::new(executable);
    command
        .arg(WORKER_SUBCOMMAND)
        .arg(directory)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        xai_tty_utils::detach_std_command(&mut command);
        command.spawn().map_err(|error| {
            ComputerError::io(format!("spawn persistent background worker: {error}"))
        })
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
        };
        let base = DETACHED_PROCESS.0 | CREATE_NEW_PROCESS_GROUP.0 | CREATE_NO_WINDOW.0;
        command.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB.0);
        match command.spawn() {
            Ok(child) => Ok(child),
            Err(error) if error.raw_os_error() == Some(5) => {
                command.creation_flags(base);
                command.spawn().map_err(|error| {
                    ComputerError::io(format!("spawn persistent background worker: {error}"))
                })
            }
            Err(error) => Err(ComputerError::io(format!(
                "spawn persistent background worker: {error}"
            ))),
        }
    }
}

/// Entry point intercepted by the composition-root binary before normal CLI
/// startup. It is intentionally not exposed as a public CLI command.
pub fn maybe_run_worker() -> Option<i32> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).and_then(|arg| arg.to_str()) != Some(WORKER_SUBCOMMAND) {
        return None;
    }
    let Some(directory) = args.get(2).map(PathBuf::from) else {
        return Some(2);
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return Some(1),
    };
    Some(match runtime.block_on(run_worker(directory.clone())) {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(%error, "persistent background worker failed");
            // The worker's own stdio is null and it starts before logging is
            // configured, so the task directory is the only place this can land.
            note_worker_event(&directory, &format!("worker failed: {error}"));
            1
        }
    })
}

async fn run_worker(directory: PathBuf) -> Result<(), String> {
    let spec: PersistentTaskSpec = serde_json::from_slice(
        &std::fs::read(directory.join("spec.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("read background task spec: {e}"))?;
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported background task schema {}",
            spec.schema_version
        ));
    }
    let backend = LocalTerminalBackend::new_local_worker_backend(
        spec.search_shadows,
        spec.login_shell_capture,
        spec.shell_env_policy.clone(),
        None,
        spec.persistent_shell,
    );
    let request = spec.clone().into_request();
    let handle = backend
        .run_background(request)
        .await
        .map_err(|e| e.to_string())?;
    let worker_pid = std::process::id();
    let persistent_task_id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "background task directory has no valid name".to_string())?
        .to_string();
    let mut snapshot = backend
        .get_task(&handle.task_id)
        .await
        .ok_or_else(|| "background worker could not read its task".to_string())?;
    // The worker owns an in-process task id; the durable registry address is
    // the id returned to callers and persisted in `spec.json`.
    snapshot.task_id = persistent_task_id.clone();
    snapshot.is_backgrounded = true;
    snapshot.output.clear();
    let mut state = PersistentTaskState {
        ready: true,
        worker_pid: Some(worker_pid),
        snapshot,
    };
    // The parent only learns the task exists by reading this record, so the first
    // publish gets the generous budget too rather than failing the whole task.
    if let Some(error) = publish_state(&directory, &state, PUBLISH_FINAL_ATTEMPTS).await {
        return Err(format!("publish background-task record: {error}"));
    }

    let mut revision = record_revision(&state.snapshot);
    let mut logged_failures = 0u32;
    loop {
        if directory.join("kill.request").exists() {
            let _ = backend.kill_task(&handle.task_id).await;
        }
        let Some(mut next) = backend.get_task(&handle.task_id).await else {
            return Err("background worker lost its task".to_string());
        };
        next.task_id.clone_from(&persistent_task_id);
        next.is_backgrounded = true;
        next.output.clear();
        state.snapshot = next;
        let next_revision = record_revision(&state.snapshot);
        let completed = state.snapshot.completed;
        if completed || next_revision != revision {
            let attempts = if completed {
                PUBLISH_FINAL_ATTEMPTS
            } else {
                PUBLISH_RETRY_ATTEMPTS
            };
            match publish_state(&directory, &state, attempts).await {
                None => {
                    revision = next_revision;
                    logged_failures = 0;
                }
                Some(error) => {
                    // A record that cannot be replaced is a hiccup, not the end of
                    // the task: keep running and try again on the next tick. Only a
                    // finished task, whose result would otherwise be lost, gives up
                    // after the long budget above — and says so in `worker.log`.
                    if logged_failures < MAX_WORKER_LOG_LINES {
                        logged_failures += 1;
                        note_worker_event(
                            &directory,
                            &format!("publish deferred, record still locked ({error})"),
                        );
                    }
                    if completed {
                        return Err(format!("publish background-task record: {error}"));
                    }
                    sleep(POLL_INTERVAL).await;
                    continue;
                }
            }
        }
        if completed {
            return Ok(());
        }
        sleep(POLL_INTERVAL).await;
    }
}

fn write_state(directory: &Path, state: &PersistentTaskState) -> Result<(), ComputerError> {
    let bytes = serde_json::to_vec(state)
        .map_err(|e| ComputerError::io(format!("encode background-task state: {e}")))?;
    atomic_write(&directory.join("state.json"), &bytes)
}

/// The parts of a snapshot the durable record exposes to the parent.
///
/// A publish is skipped while these are unchanged, so a long silent command
/// does not create ten files per second. That churn is what gives another
/// process a chance to be holding the record at the wrong moment, and it buys
/// nothing: the fields below are the ones a reader can observe changing.
#[derive(PartialEq, Eq)]
struct RecordRevision {
    completed: bool,
    exit_code: Option<i32>,
    signal: Option<String>,
    output_total_bytes: usize,
    truncated: bool,
    explicitly_killed: bool,
    kill_result_delivered: bool,
    block_waited: bool,
}

fn record_revision(snapshot: &TaskSnapshot) -> RecordRevision {
    RecordRevision {
        completed: snapshot.completed,
        exit_code: snapshot.exit_code,
        signal: snapshot.signal.clone(),
        output_total_bytes: snapshot.output_total_bytes,
        truncated: snapshot.truncated,
        explicitly_killed: snapshot.explicitly_killed,
        kill_result_delivered: snapshot.kill_result_delivered,
        block_waited: snapshot.block_waited,
    }
}

/// Publishes the durable record, retrying while the file cannot be replaced.
///
/// Returns the last error only once the retry budget is exhausted. Callers must
/// not treat that as a reason to stop: the task outlives a locked record, and
/// the parent reports `worker_exited` — with no exit code and no output — only
/// when this process is gone.
async fn publish_state(
    directory: &Path,
    state: &PersistentTaskState,
    attempts: u32,
) -> Option<ComputerError> {
    let mut last_error = None;
    for attempt in 0..attempts {
        match write_state(directory, state) {
            Ok(()) => {
                if let Some(error) = last_error {
                    note_worker_event(
                        directory,
                        &format!("record published again after {attempt} retries ({error})"),
                    );
                }
                return None;
            }
            Err(error) => last_error = Some(error),
        }
        sleep(PUBLISH_RETRY_INTERVAL).await;
    }
    last_error
}

/// Appends a worker-side diagnostic line to the task directory.
///
/// The worker runs with null stdio and is started before logging is configured,
/// so it never reaches the unified log; without this file a failure inside it
/// leaves no trace anywhere. Best effort: a diagnostic must never fail the task.
fn note_worker_event(directory: &Path, line: &str) {
    use std::io::Write;

    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("worker.log"))
    else {
        return;
    };
    let _ = writeln!(file, "{stamp} {line}");
}

/// Local backend's durable registry is optional; tests and remote/ACP backends
/// can keep the original in-process behavior by using `LocalTerminalBackend::new`.
pub(crate) async fn run_persistent_background(
    registry: &TaskRegistry,
    request: TerminalRunRequest,
) -> Result<BackgroundHandle, ComputerError> {
    registry.start(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_state_rediscovers_task_and_decodes_gbk_output() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let _registry = TaskRegistry::with_root(root.clone());
        let output_file = root.join("task.log");
        let task_id = "019f-task";
        std::fs::create_dir_all(root.join(task_id)).unwrap();
        let bytes = "中文完成\n".as_bytes();
        std::fs::write(&output_file, bytes).unwrap();
        let encoding = OutputEncoding::parse("gbk").unwrap();
        let state = PersistentTaskState {
            ready: true,
            worker_pid: Some(42),
            snapshot: TaskSnapshot {
                task_id: task_id.to_string(),
                command: "echo".into(),
                display_command: None,
                cwd: ".".into(),
                start_time: std::time::SystemTime::now(),
                end_time: Some(std::time::SystemTime::now()),
                output: String::new(),
                output_file,
                truncated: false,
                output_total_bytes: 13,
                exit_code: Some(0),
                signal: None,
                completed: true,
                kind: TaskKind::Bash,
                block_waited: false,
                explicitly_killed: false,
                kill_result_delivered: false,
                owner_session_id: Some("session".into()),
                description: None,
                output_encoding: Some(encoding),
                is_backgrounded: true,
                detach: false,
            },
        };
        write_state(&root.join(task_id), &state).unwrap();

        let rediscovered = TaskRegistry::with_root(root)
            .get_task(task_id)
            .await
            .expect("durable task");
        assert_eq!(rediscovered.output, "中文完成\n");
        assert_eq!(rediscovered.output_encoding.unwrap().label(), "gbk");
    }

    /// Minimal durable record for publish-behaviour tests.
    fn probe_state(completed: bool) -> PersistentTaskState {
        let now = std::time::SystemTime::now();
        PersistentTaskState {
            ready: true,
            worker_pid: Some(std::process::id()),
            snapshot: TaskSnapshot {
                task_id: "probe-task".into(),
                command: "sleep".into(),
                display_command: None,
                cwd: ".".into(),
                start_time: now,
                end_time: completed.then_some(now),
                output: String::new(),
                output_file: PathBuf::new(),
                truncated: false,
                output_total_bytes: if completed { 5 } else { 0 },
                exit_code: completed.then_some(0),
                signal: None,
                completed,
                kind: TaskKind::Bash,
                block_waited: false,
                explicitly_killed: false,
                kill_result_delivered: false,
                owner_session_id: Some("session".into()),
                description: None,
                output_encoding: None,
                is_backgrounded: true,
                detach: false,
            },
        }
    }

    /// The durable record must not be republished for fields a reader cannot
    /// observe changing; a long silent command would otherwise write ten files
    /// per second for its whole lifetime.
    #[test]
    fn record_revision_ignores_unobservable_snapshot_fields() {
        let mut state = probe_state(false);
        let before = record_revision(&state.snapshot);

        state.snapshot.output = "fresh output".into();
        state.snapshot.task_id = "renamed".into();
        state.snapshot.is_backgrounded = false;
        assert!(record_revision(&state.snapshot) == before);

        state.snapshot.output_total_bytes = 12;
        assert!(record_revision(&state.snapshot) != before);

        let mut finished = probe_state(false);
        finished.snapshot.completed = true;
        assert!(
            record_revision(&finished.snapshot) != record_revision(&probe_state(false).snapshot)
        );
    }

    /// A record another process holds without delete sharing cannot be replaced.
    /// The publish helper must retry through that window instead of reporting
    /// the task dead, and land the record once the holder lets go.
    #[cfg(windows)]
    #[tokio::test]
    async fn publish_state_retries_while_the_record_is_locked() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().to_path_buf();
        let settled = probe_state(false);
        write_state(&directory, &settled).unwrap();

        // Hold the record the way an indexer or scanner does: no delete sharing,
        // so the worker's MoveFileExW cannot replace it.
        let held = std::sync::Arc::new(AtomicBool::new(false));
        let holder = {
            let path = directory.join("state.json");
            let held = held.clone();
            std::thread::spawn(move || {
                let lock = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(&path)
                    .unwrap();
                held.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(400));
                drop(lock);
            })
        };
        while !held.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // While the holder is alive the helper gives up only after its budget.
        assert!(
            publish_state(&directory, &settled, 3).await.is_some(),
            "a locked record has to be reported"
        );

        let finished = probe_state(true);
        assert!(
            publish_state(&directory, &finished, PUBLISH_RETRY_ATTEMPTS)
                .await
                .is_none(),
            "the publish must succeed once the holder releases the record"
        );
        holder.join().unwrap();
        assert!(
            read_state(&directory.join("state.json"))
                .unwrap()
                .snapshot
                .completed
        );
    }

    #[test]
    fn spec_round_trips_encoding_and_unbounded_timeout() {
        let request = TerminalRunRequest {
            command: "echo ok".into(),
            working_directory: ".".into(),
            env: HashMap::new(),
            timeout: Duration::MAX,
            output_byte_limit: 42,
            output_file: "task.log".into(),
            output_encoding: OutputEncoding::parse("gbk").ok(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "call".into(),
            display_command: None,
            auto_background_on_timeout: false,
            detach: true,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: Some("session".into()),
            description: None,
        };
        let spec = PersistentTaskSpec::from_request(
            "task".into(),
            request,
            crate::computer::local::SearchShadowConfig::default(),
            true,
            None,
            false,
        );
        let json = serde_json::to_vec(&spec).unwrap();
        let decoded: PersistentTaskSpec = serde_json::from_slice(&json).unwrap();
        let request = decoded.into_request();
        assert_eq!(request.output_encoding.unwrap().label(), "gbk");
        assert_eq!(request.timeout, Duration::MAX);
        assert!(
            request.detach,
            "a confirmed detach must survive the spec round-trip; teardown reads this from disk after a restart"
        );
    }

    fn spec_for_detach(detach: bool) -> PersistentTaskSpec {
        PersistentTaskSpec::from_request(
            "task".into(),
            TerminalRunRequest {
                // Long enough that the task is still running when teardown runs;
                // a command that exits on its own would make the kill assertion
                // pass for the wrong reason.
                command: "sleep 300".into(),
                working_directory: ".".into(),
                env: HashMap::new(),
                timeout: Duration::MAX,
                output_byte_limit: 42,
                output_file: "task.log".into(),
                output_encoding: None,
                notification_handle: ToolNotificationHandle::noop(),
                tool_call_id: "call".into(),
                display_command: None,
                auto_background_on_timeout: false,
                detach,
                foreground_block_budget: None,
                kind: TaskKind::Bash,
                owner_session_id: Some("session".into()),
                description: None,
            },
            crate::computer::local::SearchShadowConfig::default(),
            true,
            None,
            false,
        )
    }

    #[tokio::test]
    async fn teardown_spares_detached_tasks_and_reaps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::with_root(root.clone());

        // Both tasks are unowned so the sweep selects them by the detach flag
        // alone; neither has a live worker, so a selected task would be reported
        // as already exited.
        for (task_id, detach) in [("detach-yes", true), ("detach-no", false)] {
            let directory = root.join(task_id);
            std::fs::create_dir_all(&directory).unwrap();
            write_json(&directory.join("spec.json"), &spec_for_detach(detach)).unwrap();
            write_state(
                &directory,
                &PersistentTaskState {
                    ready: true,
                    worker_pid: None,
                    snapshot: TaskSnapshot {
                        task_id: task_id.into(),
                        command: "sleep 1".into(),
                        display_command: None,
                        cwd: ".".into(),
                        start_time: std::time::SystemTime::now(),
                        end_time: None,
                        output: String::new(),
                        output_file: directory.join("task.log"),
                        truncated: false,
                        output_total_bytes: 0,
                        exit_code: None,
                        signal: None,
                        completed: false,
                        kind: TaskKind::Bash,
                        block_waited: false,
                        explicitly_killed: false,
                        kill_result_delivered: false,
                        owner_session_id: None,
                        description: None,
                        output_encoding: None,
                        is_backgrounded: true,
                        detach: false,
                    },
                },
            )
            .unwrap();
        }

        registry.kill_all_by_owner(None).await;

        assert!(
            registry.is_detached("detach-yes"),
            "a detached task must be left running by session teardown"
        );
        assert!(
            !registry.is_detached("detach-no"),
            "an ordinary background task is still session-scoped and must be reaped"
        );
    }

    /// Build a durable record for the late-delivery tests: `completed` decides
    /// whether the worker had already published an exit when the session died.
    fn write_late_delivery_probe(
        root: &Path,
        task_id: &str,
        detach: bool,
        completed: bool,
        end_time: Option<std::time::SystemTime>,
    ) {
        let directory = root.join(task_id);
        std::fs::create_dir_all(&directory).unwrap();
        let mut spec = spec_for_detach(detach);
        spec.task_id = task_id.to_string();
        write_json(&directory.join("spec.json"), &spec).unwrap();
        let mut state = probe_state(completed);
        state.snapshot.task_id = task_id.to_string();
        state.snapshot.end_time = end_time;
        state.snapshot.owner_session_id = Some("owner-session".into());
        write_state(&directory, &state).unwrap();
    }

    /// Clearing a row must only ever remove a record that has finished: a live
    /// worker would keep writing into a directory that is no longer there. The
    /// finished probe carries a live `worker_pid` (this test process), which is
    /// how a recycled pid would otherwise strand a completed record forever.
    #[tokio::test]
    async fn delete_task_removes_only_finished_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::with_root(root.clone());
        write_late_delivery_probe(
            &root,
            "finished",
            true,
            true,
            Some(std::time::SystemTime::now()),
        );
        write_late_delivery_probe(&root, "running", true, false, None);

        assert!(
            !registry.delete_task("running").await,
            "a record whose worker never reported an exit must survive"
        );
        assert!(
            root.join("running").exists(),
            "the running record is untouched"
        );

        assert!(
            registry.delete_task("finished").await,
            "a finished record is removable"
        );
        assert!(
            !root.join("finished").exists(),
            "its directory must be gone"
        );

        assert!(
            !registry.delete_task("never-existed").await,
            "an unknown id reports that nothing was deleted"
        );
    }

    /// A detached task's completion is handed to exactly one later session:
    /// ordinary tasks die with their session and running tasks have no result
    /// yet, and a second claim must find nothing.
    #[tokio::test]
    async fn late_delivery_claims_detached_completions_exactly_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::with_root(root.clone());
        let window = Duration::from_secs(3600);
        let now = std::time::SystemTime::now();

        write_late_delivery_probe(&root, "detached-finished", true, true, Some(now));
        write_late_delivery_probe(&root, "ordinary-finished", false, true, Some(now));
        write_late_delivery_probe(&root, "detached-running", true, false, None);

        let first = registry.claim_late_deliveries(window, Some("claimer"));
        let ids: Vec<&str> = first.iter().map(|task| task.task_id.as_str()).collect();
        assert_eq!(ids, vec!["detached-finished"], "only the detached exit");
        assert_eq!(
            first[0].owner_session_id.as_deref(),
            Some("owner-session"),
            "the late report must carry the session that started the task"
        );

        let second = registry.claim_late_deliveries(window, Some("other"));
        assert!(second.is_empty(), "the claim is one-shot: {second:?}");
    }

    /// A completion older than the window is consumed without being reported,
    /// and stays consumed: widening the window later must not replay it.
    #[tokio::test]
    async fn late_delivery_consumes_stale_completions_silently() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::with_root(root.clone());
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
        write_late_delivery_probe(&root, "stale", true, true, Some(long_ago));

        assert!(
            registry
                .claim_late_deliveries(Duration::from_secs(3600), None)
                .is_empty(),
            "an out-of-window completion is not reported"
        );
        assert!(
            registry
                .claim_late_deliveries(Duration::from_secs(10 * 24 * 3600), None)
                .is_empty(),
            "a silent consumption must not become a delivery when the window grows"
        );
    }

    /// The delivery claim is exclusive, so a session racing the dying-of-old-age
    /// scan (or a second session) cannot report the same completion twice.
    #[test]
    fn delivery_claim_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        assert!(claim_delivery(temp.path(), false, Some("first")));
        assert!(!claim_delivery(temp.path(), true, Some("second")));
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temp.path().join(DELIVERY_MARKER)).unwrap())
                .unwrap();
        assert_eq!(marker["state"], "delivered");
        assert_eq!(
            marker["late"], false,
            "the marker records the winning claim's provenance"
        );
        assert_eq!(marker["claimed_by"], "first");
    }

    /// The light list must return the same records as the full one while
    /// skipping the per-task log read: a UI enumerating every durable record
    /// cannot pay one file read each, and it only needs ids/owners/state.
    #[tokio::test]
    async fn light_list_keeps_every_record_and_skips_the_log_read() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::with_root(root.clone());

        let directory = root.join("task-logged");
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("task.log");
        std::fs::write(&log, b"hello from the log").unwrap();
        write_state(
            &directory,
            &PersistentTaskState {
                ready: true,
                worker_pid: None,
                snapshot: TaskSnapshot {
                    task_id: "task-logged".into(),
                    command: "echo hello".into(),
                    display_command: None,
                    cwd: ".".into(),
                    start_time: std::time::SystemTime::now(),
                    end_time: Some(std::time::SystemTime::now()),
                    output: String::new(),
                    output_file: log.clone(),
                    truncated: false,
                    output_total_bytes: 0,
                    exit_code: Some(0),
                    signal: None,
                    completed: true,
                    kind: TaskKind::Bash,
                    block_waited: false,
                    explicitly_killed: false,
                    kill_result_delivered: false,
                    owner_session_id: Some("session-other".into()),
                    description: None,
                    output_encoding: None,
                    is_backgrounded: true,
                    detach: true,
                },
            },
        )
        .unwrap();

        let full = registry.list_tasks().await;
        assert_eq!(full.len(), 1, "the full list must see the record");
        assert_eq!(
            full[0].output, "hello from the log",
            "the full list hydrates from the log file"
        );

        let light = registry.list_tasks_light().await;
        assert_eq!(light.len(), 1, "the light list must see the same record");
        assert_eq!(light[0].task_id, full[0].task_id);
        assert_eq!(light[0].owner_session_id, full[0].owner_session_id);
        assert_eq!(light[0].completed, full[0].completed);
        assert_eq!(light[0].detach, full[0].detach);
        assert!(
            light[0].output.is_empty(),
            "the light list must not read the log: {:?}",
            light[0].output
        );
        assert_eq!(
            light[0].output_file, log,
            "the log path stays so a caller can read it on demand"
        );
    }

    #[tokio::test]
    async fn teardown_treats_unreadable_spec_as_not_detached() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::with_root(root.clone());
        std::fs::create_dir_all(root.join("no-spec")).unwrap();
        // No spec.json at all: cleanup must fail toward stopping the task, never
        // toward leaving an unknown process running.
        assert!(
            !registry.is_detached("no-spec"),
            "a task with no spec must be treated as session-scoped so cleanup still stops it"
        );
        assert!(!registry.is_detached("../../escape"));
    }

    /// The teardown split is only meaningful if it changes what actually runs:
    /// an ordinary background task must stop, a detached one must survive.
    /// `is_detached` reads a flag, so assert on the kill itself.
    ///
    /// The worker is a re-entry of the shipped executable, and only
    /// `xai-grok-pager-bin`'s `main` calls `maybe_run_worker` — a lib test
    /// binary cannot host one, so this needs that binary built. It comes from
    /// the same package as the pager PTY tests, which already require it.
    #[tokio::test]
    async fn teardown_kills_ordinary_tasks_and_spares_detached_ones() {
        let Some(registry) = TaskRegistry::for_live_worker() else {
            eprintln!("skipping: no grok-zh binary with the background-worker entry point");
            return;
        };
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("tasks");
        let registry = TaskRegistry {
            root: Arc::new(root.clone()),
            ..registry
        };

        let mut handles = Vec::new();
        for (label, detach) in [("detached", true), ("ordinary", false)] {
            let request = spec_for_detach(detach).into_request();
            let handle = registry
                .start(request)
                .await
                .unwrap_or_else(|e| panic!("start {label} durable background task: {e}"));
            assert!(
                registry.worker_is_alive(&handle.task_id),
                "{label} task should have a live worker right after start"
            );
            handles.push((label, detach, handle));
        }

        registry.kill_all_by_owner(None).await;

        for (label, detach, handle) in &handles {
            // The worker polls its kill request on a 100 ms tick and then tears
            // the command down, so give it a bounded moment to actually exit
            // rather than racing the very next syscall.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while registry.worker_is_alive(&handle.task_id) && std::time::Instant::now() < deadline
            {
                sleep(Duration::from_millis(100)).await;
            }
            let still_running = registry.worker_is_alive(&handle.task_id);
            if *detach {
                assert!(
                    still_running,
                    "{label}: a detached task was confirmed to outlive the session, so \
                     teardown must leave its worker running"
                );
                // Clean up the survivor the teardown deliberately spared.
                let _ = registry
                    .kill_task(&handle.task_id, KillSource::Teardown)
                    .await;
            } else {
                assert!(
                    !still_running,
                    "{label}: an ordinary background task is session-scoped; teardown \
                     must have killed its worker"
                );
            }
        }
    }

    /// A durable task's completion must reach the session that started it.
    ///
    /// The worker's own handle is a no-op (`PersistentTaskSpec::into_request`),
    /// so this notification can only come from the watcher `start` installs.
    /// Without it the pager keeps the task row running until the session ends.
    #[tokio::test]
    async fn durable_task_reports_completion_to_its_starting_session() {
        let Some(registry) = TaskRegistry::for_live_worker() else {
            eprintln!("skipping: no grok-zh binary with the background-worker entry point");
            return;
        };
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("tasks");
        let registry = TaskRegistry {
            root: Arc::new(root.clone()),
            ..registry
        };

        let (notification_handle, mut notifications) = ToolNotificationHandle::channel();
        let mut request = spec_for_detach(false).into_request();
        // Short command: the watcher only reports once the worker is gone.
        request.command = "echo durable-done".into();
        request.notification_handle = notification_handle;

        let handle = registry
            .start(request)
            .await
            .expect("start durable background task");

        let received = tokio::time::timeout(Duration::from_secs(30), notifications.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "a durable task must report completion, otherwise the pager row for \
                     {} never settles",
                    handle.task_id
                )
            });
        let Some(crate::notification::types::ToolNotification::TaskCompleted(snapshot)) = received
        else {
            panic!("expected a task-completed notification");
        };
        assert_eq!(snapshot.task_id, handle.task_id);
        assert!(
            snapshot.completed,
            "the snapshot reported on completion must be the finished one"
        );
    }
}
