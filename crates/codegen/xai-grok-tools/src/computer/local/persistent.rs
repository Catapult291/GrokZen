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
        let mut dir = std::env::current_exe().ok()?.parent().map(Path::to_path_buf);
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
                    hydrate_output(&mut state.snapshot).await;
                    tasks.push(state.snapshot);
                }
            }
        }
        tasks.sort_by_key(|task| task.start_time);
        tasks
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
        std::fs::read(directory.join("spec.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistentTaskSpec>(&bytes).ok())
            .is_some_and(|spec| spec.detach)
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
    Some(match runtime.block_on(run_worker(directory)) {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(%error, "persistent background worker failed");
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
    write_state(&directory, &state).map_err(|e| e.to_string())?;

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
        write_state(&directory, &state).map_err(|e| e.to_string())?;
        if state.snapshot.completed {
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
            while registry.worker_is_alive(&handle.task_id) && std::time::Instant::now() < deadline {
                sleep(Duration::from_millis(100)).await;
            }
            let still_running = registry.worker_is_alive(&handle.task_id);
            if *detach {
                assert!(
                    still_running,
                    "a detached task was confirmed to outlive the session, so teardown \
                     must leave its worker running"
                );
                // Clean up the survivor the teardown deliberately spared.
                let _ = registry
                    .kill_task(&handle.task_id, KillSource::Teardown)
                    .await;
            } else {
                assert!(
                    !still_running,
                    "an ordinary background task is session-scoped; teardown must \
                     have killed its worker"
                );
            }
        }
    }
}
