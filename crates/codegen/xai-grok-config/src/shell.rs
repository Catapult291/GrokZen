//! Windows shell detection for terminal command execution.
//!
//! The persisted default is Git Bash. The preference is read from `[ui].default_shell`;
//! `GROK_SHELL` remains an explicit process-level override. The resolved result is
//! cached for the process lifetime, so changing the setting requires a restart.
//!
//! niubash (`niu.exe`) is available as an opt-in preference, never as the default:
//! it is a third-party native Windows Bash with no MSYS translation layer, so a
//! machine without it installed must keep working. When the configured preference
//! cannot be resolved, the cascade falls back to Git Bash.
//!
//! PowerShell 7+ is intentionally represented by the executable name `pwsh`, not
//! a major-version number. A future PowerShell 8 or 9 that keeps the `pwsh.exe`
//! command name therefore works without a configuration migration.

/// A user-selectable Windows shell family.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowsShellPreference {
    /// Git Bash, bundled with Git for Windows.
    #[default]
    GitBash,
    /// niubash (`niu.exe`), a native Windows Bash that resolves POSIX dialect
    /// paths itself and has no MSYS translation layer to disable.
    Niu,
    /// PowerShell 7 or newer (`pwsh.exe`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    PowerShell,
}

impl WindowsShellPreference {
    /// Canonical value persisted in `[ui].default_shell`.
    pub fn as_canonical(self) -> &'static str {
        match self {
            Self::GitBash => "git-bash",
            Self::Niu => "niubash",
            Self::Pwsh => "pwsh",
            Self::PowerShell => "powershell",
        }
    }

    /// User-facing label. The `+` deliberately avoids pinning future PowerShell
    /// majors to this build's label.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::GitBash => "Git Bash",
            Self::Niu => "Niubash",
            Self::Pwsh => "PowerShell 7+",
            Self::PowerShell => "Windows PowerShell 5.1",
        }
    }

    /// Parse a canonical value or one of the historical `GROK_SHELL` aliases.
    /// Unknown, blank, and absent values use the product default, Git Bash.
    pub fn parse(value: Option<&str>) -> Self {
        let normalized = value.unwrap_or_default().trim().to_ascii_lowercase();
        match normalized.as_str() {
            "bash" | "gitbash" | "git-bash" => Self::GitBash,
            "niubash" | "niu" | "niu.exe" => Self::Niu,
            "pwsh" | "powershell-7" | "powershell-7+" | "powershell-core" => Self::Pwsh,
            "powershell" | "windows-powershell" | "powershell-5" | "powershell-5.1" => {
                Self::PowerShell
            }
            _ => Self::GitBash,
        }
    }
}

/// Canonicalize a raw persisted shell preference.
pub fn canonical_windows_shell(value: Option<&str>) -> &'static str {
    WindowsShellPreference::parse(value).as_canonical()
}

/// Detected Windows shell and how to invoke it.
#[cfg(not(unix))]
#[derive(Clone, Debug)]
pub enum WindowsShell {
    GitBash(String),
    /// niubash, resolved to the absolute path of `niu.exe`.
    Niu(String),
    Pwsh,
    PowerShell,
    Cmd,
}

/// How the active shell treats path-like and switch-like arguments.
///
/// A single boolean cannot express this: there are three real states, not two.
/// An MSYS shell with translation on rewrites both POSIX paths *and* `/flag`
/// switches; an MSYS shell with translation off rewrites neither; and a native
/// shell such as niubash resolves POSIX dialect paths while leaving every
/// argument otherwise untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathGuidance {
    /// No path-translation layer is involved (Unix, `pwsh`, `powershell.exe`, `cmd.exe`).
    NoTranslationLayer,
    /// An MSYS translation layer is active. A POSIX path handed to a native tool
    /// is rewritten to a Windows path, and so is a `/flag` argument — which is why
    /// a single-slash switch must be doubled (`//c`) to survive.
    MsysTranslating,
    /// A native shell that resolves POSIX dialect paths itself and never rewrites
    /// an argument (niubash): `/c/...` reaches native tools as a Windows path
    /// while `/flag` and literal `/words` pass through untouched.
    DialectResolving,
}

impl PathGuidance {
    /// Value exposed to description templates as `path_guidance`.
    pub fn as_template_value(self) -> &'static str {
        match self {
            Self::NoTranslationLayer => "none",
            Self::MsysTranslating => "msys_translating",
            Self::DialectResolving => "dialect_resolving",
        }
    }
}

#[cfg(not(unix))]
fn configured_windows_shell_preference() -> WindowsShellPreference {
    let Ok(layers) = crate::ConfigLayers::load() else {
        return WindowsShellPreference::default();
    };
    let effective = layers.effective_config_base();
    let raw = effective
        .get("ui")
        .and_then(|ui| ui.get("default_shell"))
        .and_then(toml::Value::as_str);
    WindowsShellPreference::parse(raw)
}

#[cfg(not(unix))]
fn windows_command_exists(name: &str) -> bool {
    let Ok(output) = ({
        let mut cmd = std::process::Command::new("where");
        xai_tty_utils::detach_std_command(&mut cmd);
        cmd.arg(name).stdin(std::process::Stdio::null());
        cmd.output()
    }) else {
        return false;
    };
    output.status.success() || which::which(name).is_ok()
}

#[cfg(not(unix))]
fn shell_for_preference(preference: WindowsShellPreference) -> Option<WindowsShell> {
    match preference {
        WindowsShellPreference::GitBash => find_git_bash().map(WindowsShell::GitBash),
        WindowsShellPreference::Niu => find_niu().map(WindowsShell::Niu),
        WindowsShellPreference::Pwsh => {
            windows_command_exists("pwsh.exe").then_some(WindowsShell::Pwsh)
        }
        WindowsShellPreference::PowerShell => {
            if std::path::Path::new(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
            )
            .exists()
                || windows_command_exists("powershell.exe")
            {
                Some(WindowsShell::PowerShell)
            } else {
                None
            }
        }
    }
}

#[cfg(not(unix))]
fn fallback_windows_shell() -> WindowsShell {
    if let Some(path) = find_git_bash() {
        tracing::info!(shell = path, "Windows shell: Git Bash");
        return WindowsShell::GitBash(path);
    }
    if windows_command_exists("pwsh.exe") {
        tracing::info!("Windows shell: pwsh (fallback)");
        return WindowsShell::Pwsh;
    }
    tracing::info!("Windows shell: powershell.exe (fallback)");
    WindowsShell::PowerShell
}

#[cfg(not(unix))]
fn resolve_windows_shell() -> WindowsShell {
    if let Ok(value) = std::env::var("GROK_SHELL") {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "cmd" | "cmd.exe" => {
                tracing::info!("Windows shell (GROK_SHELL override): cmd.exe");
                return WindowsShell::Cmd;
            }
            "bash" | "gitbash" | "git-bash" => {
                if let Some(path) = find_git_bash() {
                    tracing::info!(
                        shell = path,
                        "Windows shell (GROK_SHELL override): Git Bash"
                    );
                    return WindowsShell::GitBash(path);
                }
                tracing::warn!(
                    "GROK_SHELL={value} but Git Bash was not found; using the configured fallback"
                );
            }
            "niubash" | "niu" | "niu.exe" => {
                if let Some(path) = find_niu() {
                    tracing::info!(shell = path, "Windows shell (GROK_SHELL override): niubash");
                    return WindowsShell::Niu(path);
                }
                tracing::warn!(
                    "GROK_SHELL={value} but niu.exe was not found; using the configured fallback"
                );
            }
            "pwsh" | "powershell-7" | "powershell-7+" | "powershell-core" => {
                if windows_command_exists("pwsh.exe") {
                    tracing::info!("Windows shell (GROK_SHELL override): pwsh");
                    return WindowsShell::Pwsh;
                }
                tracing::warn!(
                    "GROK_SHELL={value} but pwsh.exe was not found; using the configured fallback"
                );
            }
            "powershell" | "windows-powershell" | "powershell-5" | "powershell-5.1" => {
                if shell_for_preference(WindowsShellPreference::PowerShell).is_some() {
                    tracing::info!("Windows shell (GROK_SHELL override): powershell.exe");
                    return WindowsShell::PowerShell;
                }
                tracing::warn!(
                    "GROK_SHELL={value} but powershell.exe was not found; using the configured fallback"
                );
            }
            other => {
                tracing::warn!(
                    "GROK_SHELL={other} is not recognized \
                     (expected pwsh|powershell|bash|cmd); using the configured default"
                );
            }
        }
    }

    let preference = configured_windows_shell_preference();
    match shell_for_preference(preference) {
        Some(shell) => {
            tracing::info!(
                preference = preference.as_canonical(),
                shell = shell.name(),
                "Windows shell selected by config"
            );
            shell
        }
        None => {
            tracing::warn!(
                preference = preference.as_canonical(),
                "configured Windows shell is unavailable; falling back"
            );
            fallback_windows_shell()
        }
    }
}

/// Detect the selected Windows shell.
///
/// Precedence is `GROK_SHELL` > `[ui].default_shell` > Git Bash default.
/// The result is cached for the process lifetime.
#[cfg(not(unix))]
pub fn detect_windows_shell() -> &'static WindowsShell {
    use std::sync::OnceLock;
    static CACHED: OnceLock<WindowsShell> = OnceLock::new();
    CACHED.get_or_init(resolve_windows_shell)
}

/// Checks common install paths, then falls back to `where bash.exe` (filtering for Git paths to avoid WSL bash).
#[cfg(not(unix))]
fn find_git_bash() -> Option<String> {
    let candidates = [
        std::env::var("PROGRAMFILES")
            .map(|pf| format!("{pf}\\Git\\bin\\bash.exe"))
            .unwrap_or_default(),
        std::env::var("PROGRAMFILES(X86)")
            .map(|pf| format!("{pf}\\Git\\bin\\bash.exe"))
            .unwrap_or_default(),
        std::env::var("LOCALAPPDATA")
            .map(|la| format!("{la}\\Programs\\Git\\bin\\bash.exe"))
            .unwrap_or_default(),
    ];
    for candidate in &candidates {
        if !candidate.is_empty() && std::path::Path::new(candidate).exists() {
            return Some(candidate.clone());
        }
    }
    // Fall back to PATH; prefer Git Bash over WSL bash.
    if let Ok(output) = {
        let mut cmd = std::process::Command::new("where");
        xai_tty_utils::detach_std_command(&mut cmd);
        cmd.arg("bash.exe").stdin(std::process::Stdio::null());
        cmd.output()
    } {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let line = line.trim();
                if line.to_ascii_lowercase().contains("git") {
                    return Some(line.to_string());
                }
            }
        }
    }
    None
}

/// Locates `niu.exe` for the opt-in niubash shell.
///
/// niubash is third-party and never bundled, so discovery is deliberately
/// permissive: an explicit `GROK_NIU` path wins, then the installer's default
/// locations, then `PATH`. There is no probe spawn here — the portable archive
/// can live anywhere, and a candidate that exists but misbehaves surfaces as a
/// failed command with the shell's own error rather than as a silent fallback.
#[cfg(not(unix))]
fn find_niu() -> Option<String> {
    /// Absolute path to `niu.exe`, for portable layouts that never register on `PATH`.
    const GROK_NIU: &str = "GROK_NIU";

    if let Ok(explicit) = std::env::var(GROK_NIU) {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            if niu_candidate_is_usable(std::path::Path::new(trimmed)) {
                return Some(trimmed.to_string());
            }
            tracing::warn!(
                path = trimmed,
                "GROK_NIU does not point at a usable niu.exe; falling back to discovery"
            );
        }
    }

    // The per-user install directory the Inno Setup installer uses
    // (`{localappdata}\Programs\Niubash`), then PATH for portable layouts.
    let candidates = [std::env::var("LOCALAPPDATA")
        .map(|dir| format!("{dir}\\Programs\\Niubash\\niu.exe"))
        .unwrap_or_default()];
    for candidate in &candidates {
        if !candidate.is_empty() && niu_candidate_is_usable(std::path::Path::new(candidate)) {
            return Some(candidate.clone());
        }
    }

    // Fall back to PATH, taking the first entry that is really an executable file.
    if let Ok(output) = {
        let mut cmd = std::process::Command::new("where");
        xai_tty_utils::detach_std_command(&mut cmd);
        cmd.arg("niu.exe").stdin(std::process::Stdio::null());
        cmd.output()
    } {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let line = line.trim();
                if niu_candidate_is_usable(std::path::Path::new(line)) {
                    return Some(line.to_string());
                }
            }
        }
    }
    None
}

/// Whether a candidate path is the `niu.exe` file itself, not a directory or a
/// differently named neighbour a stray `GROK_NIU` value may have pointed at.
#[cfg(not(unix))]
fn niu_candidate_is_usable(path: &std::path::Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("niu.exe"))
}

#[cfg(not(unix))]
impl WindowsShell {
    /// Short display name for user-facing contexts (e.g. "bash", "pwsh").
    ///
    /// niubash reports `bash` on purpose: it executes Bash, and the system
    /// prompt's `Shell:` line is what keeps the model writing Bash idiom.
    pub fn name(&self) -> &'static str {
        match self {
            Self::GitBash(_) | Self::Niu(_) => "bash",
            Self::Pwsh => "pwsh",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd.exe",
        }
    }

    /// Whether this shell supports the `&&` pipeline chain operator for error-propagating command chaining.
    /// True for pwsh (`&&` arrived in PS 7.0), Git Bash, and niubash; powershell.exe 5.1 has no `&&`.
    /// `cmd.exe` has `&&` but we use `;` there for uniformity with the `-Command` invocation style used elsewhere.
    pub fn supports_chain_operator(&self) -> bool {
        matches!(self, Self::Pwsh | Self::GitBash(_) | Self::Niu(_))
    }

    /// Whether `grep`, `head`, `tail`, `sed`, `awk`, `find` are usable from this shell.
    /// True for Git Bash, where MSYS2 bundles them inside the bash subprocess, and for
    /// niubash, which puts the winuxcmd binaries on the shell's own `PATH`.
    pub fn has_unix_utilities(&self) -> bool {
        matches!(self, Self::GitBash(_) | Self::Niu(_))
    }

    /// Which path/switch guidance this shell needs.
    ///
    /// Git Bash reports [`PathGuidance::MsysTranslating`] because the resolver leaves
    /// MSYS path translation at its default (on): `/c/Users/...` reaches a native tool
    /// as `C:/Users/...`, and a `/flag` argument is rewritten too. The escape hatches
    /// stay the MSYS ones — double the slash (`//flag`) for a switch the converter
    /// would eat, or `export MSYS2_ARG_CONV_EXCL='...'` earlier in the same command.
    /// `/c` must never be excluded: the list matches by prefix, so excluding `/c`
    /// would also stop `/c/Users/...` from being converted.
    ///
    /// niubash reports [`PathGuidance::DialectResolving`]: it has no converter, so no
    /// argument is rewritten at all and a single-slash switch is already correct.
    pub fn path_guidance(&self) -> PathGuidance {
        match self {
            Self::GitBash(_) => PathGuidance::MsysTranslating,
            Self::Niu(_) => PathGuidance::DialectResolving,
            Self::Pwsh | Self::PowerShell | Self::Cmd => PathGuidance::NoTranslationLayer,
        }
    }

    /// How this shell interprets a bare `&` token.
    /// Drives the `run_terminal_cmd` background-operator validation, which must differ per shell.
    pub fn ampersand_semantics(&self) -> AmpersandSemantics {
        match self {
            Self::GitBash(_) | Self::Niu(_) => AmpersandSemantics::PosixBackground,
            Self::Pwsh => AmpersandSemantics::PowerShellCore,
            Self::PowerShell => AmpersandSemantics::WindowsPowerShell,
            Self::Cmd => AmpersandSemantics::CmdSeparator,
        }
    }
}

/// Returns the command chaining separator for the current platform and detected shell.
///
/// - Unix: always `"&&"` (bash/zsh).
/// - Windows with pwsh or Git Bash: `"&&"` (both support pipeline chain operators).
/// - Windows with powershell.exe (5.1) or cmd.exe: `";"`.
pub fn chain_separator() -> &'static str {
    #[cfg(unix)]
    {
        "&&"
    }
    #[cfg(not(unix))]
    {
        if detect_windows_shell().supports_chain_operator() {
            "&&"
        } else {
            ";"
        }
    }
}

/// Whether `grep`, `head`, `tail`, `sed`, `awk`, `find` are usable from the active shell.
/// True on Unix and on Windows with Git Bash; false on Windows with PowerShell or `cmd.exe`.
///
/// Tool descriptions branch on this to swap Unix-centric guidance for shell-aware guidance and avoid `'grep' is not recognized` failures.
pub fn has_unix_utilities() -> bool {
    #[cfg(unix)]
    {
        true
    }
    #[cfg(not(unix))]
    {
        detect_windows_shell().has_unix_utilities()
    }
}

/// Which path/switch guidance describes the active shell.
///
/// Unix reports [`PathGuidance::NoTranslationLayer`]. Tool descriptions branch on
/// this through the `path_guidance` template variable.
pub fn path_guidance() -> PathGuidance {
    #[cfg(unix)]
    {
        PathGuidance::NoTranslationLayer
    }
    #[cfg(not(unix))]
    {
        detect_windows_shell().path_guidance()
    }
}

/// Whether `name` resolves to an executable on the current `$PATH`.
///
/// The truncated-MCP steer uses this to name only tools present on the tool server's `$PATH`, with no "if available" hedge.
/// `which` handles the platform details (PATHEXT and App Execution Aliases on Windows).
/// Probes this process's environment (the tool server and the shell tool are co-located in production).
/// Per-session `export PATH` changes inside the persistent shell are not reflected (uncommon for `jq`/`python`/`sed`/`cut`).
pub fn is_command_available(name: &str) -> bool {
    which::which(name).is_ok()
}

/// How a shell interprets a bare `&` token.
/// Drives `run_terminal_cmd` background-operator detection and remediation, which must differ per shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmpersandSemantics {
    /// Bash/POSIX: a bare `&` backgrounds the command (Unix shells, Git Bash).
    PosixBackground,
    /// PowerShell 7+ (`pwsh`): a *leading* `&` is the call/invocation operator; a *trailing* `&` starts a background job.
    PowerShellCore,
    /// Windows PowerShell 5.1 (`powershell.exe`): a *leading* `&` is the call operator; a *trailing* `&` is a parse error.
    WindowsPowerShell,
    /// `cmd.exe`: `&` is an unconditional sequential command separator.
    CmdSeparator,
}

/// How the active shell interprets a bare `&`.
/// Unix shells are always [`AmpersandSemantics::PosixBackground`]; on Windows it depends on the detected shell.
pub fn ampersand_semantics() -> AmpersandSemantics {
    #[cfg(unix)]
    {
        AmpersandSemantics::PosixBackground
    }
    #[cfg(not(unix))]
    {
        detect_windows_shell().ampersand_semantics()
    }
}

/// How to invoke a command in the detected Windows shell.
#[cfg(not(unix))]
pub struct ShellInvocation {
    pub program: String,
    pub args: Vec<String>,
    /// Env vars to set on the child process. The MSYS path-translation toggles
    /// deliberately never appear here: leaving translation at its default is the
    /// contract, because that is what rewrites a POSIX path for a native tool. See
    /// [`WindowsShell::path_guidance`] for what each shell needs instead.
    pub env: Vec<(&'static str, &'static str)>,
    /// Env vars to **remove** from the child process, which callers must apply with
    /// `env_remove`.
    ///
    /// Unsetting is not enough for the MSYS toggles: Git for Windows treats the mere
    /// *presence* of `MSYS_NO_PATHCONV` as "translation off", so a parent that still
    /// carries it — a `grok-zh` launched from an older session's command inherits it,
    /// and in leader mode every session inherits the leader's environment — would
    /// silently restore the behaviour this contract removes. Values cannot fix that
    /// either: `MSYS_NO_PATHCONV=0` and `MSYS_NO_PATHCONV=` both leave translation off.
    pub remove_env: Vec<&'static str>,
}

/// Build `(program, args, env)` for running `command` in the detected shell.
///
/// A command past the shell's inline ceiling is staged in a temporary script and run
/// through a wrapper instead of being passed as one `-c` argument; see
/// [`stage_command_script`] for why.
#[cfg(not(unix))]
pub fn shell_command_argv(command: &str) -> ShellInvocation {
    let shell = detect_windows_shell();
    if command.len() > inline_command_ceiling(shell) {
        if let Some(staged) = stage_command_script(shell, command) {
            return invocation_for(shell, &staged);
        }
    }
    invocation_for(shell, command)
}

/// Longest command *text* that may be passed inline as one shell argument.
///
/// Both Bash-family shells limit this below what most callers assume, and they fail
/// in different ways:
///
/// - Git Bash silently truncates a single argument past 8192 bytes: a 9000-byte `-c`
///   reached `bash` as 8186 bytes and still exited 0, so the tail of the command never
///   ran and nothing reported a problem. `-lc` cuts at the same argument length as
///   `-c`, so the limit belongs to the argument, not to the command line.
/// - niubash hands the whole line to `CreateProcess` and fails outright once it passes
///   32767 UTF-16 units, with `WinError 206`.
///
/// The PowerShell and `cmd.exe` arms keep the inline form: each has its own script
/// convention (`-File`, a `.cmd` file) that would need a separate cleanup path, and
/// neither has been measured for where it breaks.
#[cfg(not(unix))]
fn inline_command_ceiling(shell: &WindowsShell) -> usize {
    match shell {
        WindowsShell::GitBash(_) => 8_000,
        WindowsShell::Niu(_) => 24_000,
        WindowsShell::Pwsh | WindowsShell::PowerShell | WindowsShell::Cmd => usize::MAX,
    }
}

/// Prefix shared by every staged command script, so leftovers can be swept.
#[cfg(not(unix))]
const STAGED_SCRIPT_PREFIX: &str = "grok-zh-cmd-";

/// Age past which a staged script counts as a leftover.
///
/// The normal path cleans up after itself: the wrapper's `EXIT` trap removes the file
/// even when the staged command sets its own exit code. Only a shell killed before its
/// trap can run — a command timeout, a cancelled background task — leaves a file, so
/// the sweep may be conservative without ever deleting a file a live command is using.
#[cfg(not(unix))]
const STAGED_SCRIPT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Write `command` to a temporary script and return the wrapper that runs it.
///
/// Only the Bash-family arms have a script form; every other arm returns `None` and
/// keeps the inline invocation. The wrapper sources the script and removes it on
/// `EXIT`, so the exit code survives — including a trailing `exit N` inside the
/// command — and no file is left behind.
#[cfg(not(unix))]
fn stage_command_script(shell: &WindowsShell, command: &str) -> Option<String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    if !matches!(shell, WindowsShell::GitBash(_) | WindowsShell::Niu(_)) {
        return None;
    }

    let temp = std::env::temp_dir();
    sweep_staged_scripts(&temp);
    let path = temp.join(format!(
        "{STAGED_SCRIPT_PREFIX}{}-{}.sh",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut body = command.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    std::fs::write(&path, body).ok()?;
    tracing::debug!(
        path = %path.display(),
        bytes = command.len(),
        "staged a long command in a script"
    );

    // Both shells read a forward-slash Windows path, and holding the path in a
    // variable keeps the `trap` body free of nested quoting.
    let quoted = posix_single_quote(&path.to_string_lossy().replace('\\', "/"));
    Some(format!(
        "__grok_staged_cmd={quoted}; trap 'rm -f \"$__grok_staged_cmd\"' EXIT; . \"$__grok_staged_cmd\""
    ))
}

/// Quote `value` for a POSIX shell, escaping embedded single quotes the usual way.
#[cfg(not(unix))]
fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Delete staged scripts older than [`STAGED_SCRIPT_MAX_AGE`].
#[cfg(not(unix))]
fn sweep_staged_scripts(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(STAGED_SCRIPT_PREFIX) || !name.ends_with(".sh") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STAGED_SCRIPT_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Prefix a Bash-family command so the child's stderr joins its stdout pipe.
///
/// The tool reads stdout and stderr as two pipes and appends a stdout chunk
/// before a stderr chunk on every poll tick, so a command that interleaves the
/// streams reads back as all-of-stdout-then-all-of-stderr: a `grep` diagnostic
/// lands after the match lines it belongs to, or inside a later command's
/// output. The Unix persistent shell merges the streams with `2>&1` around its
/// `eval` for the same reason; on Windows there is no equivalent wrapper, so the
/// merge belongs to the invocation.
///
/// `exec 2>&1` on its own line — not `{ command; } 2>&1`. A group redirection
/// swallows output on niubash: with `{ yes\n} 2>&1` the runaway writer produced
/// 0 bytes in 3 s (Git Bash and the `exec` form each produced ~4.5 GB), so a
/// long-running command's output would stall the size guard and the streaming
/// notifications. `exec` duplicates the descriptor once and leaves the command
/// text untouched, which also keeps `exit N`, `&`, heredocs, trailing
/// backslashes and comments behaving exactly as they do unwrapped.
#[cfg(not(unix))]
fn bash_family_merged_command(command: &str) -> String {
    format!("exec 2>&1\n{command}")
}

/// Pure builder split out of `shell_command_argv` so tests can exercise every `WindowsShell` variant, not just the one installed on the test host.
#[cfg(not(unix))]
fn invocation_for(shell: &WindowsShell, command: &str) -> ShellInvocation {
    // Force UTF-8 for descendant tools
    // Windows' legacy ANSI codepage (cp1252) makes locale-sensitive children mis-decode UTF-8 subprocess output
    // Python's text-mode `subprocess`, for example, raised `UnicodeDecodeError` on `gh` output
    // `PYTHONUTF8=1` is the fix (forces `locale.getpreferredencoding` to utf-8)
    // `PYTHONIOENCODING` covers the interpreter's own stdio, with `surrogateescape` matching UTF-8 Mode's leniency
    // Applied before the per-request env, so an explicit caller value still overrides these defaults
    let utf8_env = [
        ("PYTHONUTF8", "1"),
        ("PYTHONIOENCODING", "utf-8:surrogateescape"),
    ];
    match shell {
        // MSYS path translation is deliberately left at its default (on): a POSIX
        // path handed to a native tool is then rewritten for us, which is exactly
        // what the model's Bash instinct writes. Turning it off inverts that — the
        // same path reaches `python.exe` verbatim and fails. The one clause the
        // converter gets wrong is a `/flag` argument for a native tool; the model
        // doubles the slash (`//flag`) or exports `MSYS2_ARG_CONV_EXCL` in the
        // command. `/c` must never be added to that list: it matches by prefix, so
        // excluding `/c` would also stop `/c/Users/...` from being converted.
        WindowsShell::GitBash(path) => ShellInvocation {
            program: path.clone(),
            args: vec!["-c".to_string(), bash_family_merged_command(command)],
            env: utf8_env.to_vec(),
            remove_env: vec!["MSYS_NO_PATHCONV", "MSYS2_ARG_CONV_EXCL"],
        },
        // niubash has no translation layer, so it needs no MSYS variables at all:
        // its own path model already accepts `/c/...`, `/mnt/c/...`, and `C:\...`,
        // and it never rewrites a switch. Inherited MSYS variables are inert to it,
        // so nothing has to be cleared.
        WindowsShell::Niu(path) => ShellInvocation {
            program: path.clone(),
            args: vec!["-c".to_string(), bash_family_merged_command(command)],
            env: utf8_env.to_vec(),
            remove_env: Vec::new(),
        },
        WindowsShell::Pwsh => ShellInvocation {
            program: "pwsh".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
            env: utf8_env.to_vec(),
            remove_env: Vec::new(),
        },
        WindowsShell::PowerShell => ShellInvocation {
            program: "powershell.exe".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
            env: utf8_env.to_vec(),
            remove_env: Vec::new(),
        },
        WindowsShell::Cmd => ShellInvocation {
            program: "cmd".to_string(),
            args: vec!["/C".to_string(), command.to_string()],
            env: utf8_env.to_vec(),
            remove_env: Vec::new(),
        },
    }
}

// =============================================================================
// Unix shell resolution
// =============================================================================
//
// Locates an absolute path to a bash/zsh binary on Unix:
//
//   1. `$GROK_SHELL` override, if it names the requested kind and is runnable.
//   2. `$SHELL`, if it names the requested kind and is runnable.
//      Covers most NixOS / Homebrew / `nix-darwin` setups
//      There the user's login shell already lives at the resolved path (e.g. `/run/current-system/sw/bin/bash`, `/opt/homebrew/bin/bash`).
//   3. `which::which(name)` walks `$PATH`.
//      Catches NixOS profile shells in `/nix/store/...` or `/etc/profiles/per-user/<u>/bin/` when `/bin/bash` is absent
//   4. A fixed candidate list: `{/bin, /usr/bin, /usr/local/bin, /opt/homebrew/bin} × {bash,zsh}`.
//   5. Hardcoded `/bin/<name>`: historical behavior, only reached when every earlier step has failed.
//
// The result is cached per kind in a process-wide `OnceLock`, so the cascade is run at most once per shell kind per process

/// Bash and zsh are the only kinds supported by the persistent shell-state backend (the dump scripts are bash/zsh-specific).
/// Fish / dash / ksh users fall through to bash.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnixShellKind {
    Bash,
    Zsh,
}

#[cfg(unix)]
impl UnixShellKind {
    /// Binary file name (`"bash"` / `"zsh"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
        }
    }

    /// Hardcoded historical default. Only used as the last-resort fallback.
    fn hardcoded_default(self) -> &'static str {
        match self {
            Self::Bash => "/bin/bash",
            Self::Zsh => "/bin/zsh",
        }
    }
}

/// Detect the user's preferred Unix shell kind from `$SHELL`.
/// Defaults to `Bash` when `$SHELL` is unset or unrecognized; cheap and not cached.
#[cfg(unix)]
pub fn detect_unix_shell_kind() -> UnixShellKind {
    match std::env::var("SHELL") {
        Ok(s) if s.contains("zsh") => UnixShellKind::Zsh,
        _ => UnixShellKind::Bash,
    }
}

/// Absolute path to the requested Unix shell binary, computed via the cascade above.
/// The result is cached for the process lifetime.
#[cfg(unix)]
pub fn unix_shell_path(kind: UnixShellKind) -> &'static str {
    use std::sync::OnceLock;
    static BASH: OnceLock<String> = OnceLock::new();
    static ZSH: OnceLock<String> = OnceLock::new();
    let cache = match kind {
        UnixShellKind::Bash => &BASH,
        UnixShellKind::Zsh => &ZSH,
    };
    cache.get_or_init(|| {
        let path = resolve_unix_shell_path(kind);
        tracing::debug!(kind = ?kind, resolved = %path, "resolved Unix shell path");
        path
    })
}

#[cfg(unix)]
fn resolve_unix_shell_path(kind: UnixShellKind) -> String {
    let name = kind.name();
    let matches_kind = |p: &std::path::Path| p.file_name().and_then(|n| n.to_str()) == Some(name);

    // 1) Explicit override via $GROK_SHELL.
    if let Ok(s) = std::env::var("GROK_SHELL") {
        let p = std::path::PathBuf::from(&s);
        if matches_kind(&p) && is_executable(&p) {
            return s;
        }
    }

    // 2) $SHELL, when it matches the requested kind.
    if let Ok(s) = std::env::var("SHELL") {
        let p = std::path::PathBuf::from(&s);
        if matches_kind(&p) && is_executable(&p) {
            return s;
        }
    }

    // 3) `which` walks $PATH (handles NixOS, Homebrew, custom profiles).
    if let Ok(p) = which::which(name)
        && is_executable(&p)
    {
        return p.to_string_lossy().into_owned();
    }

    // 4) Common install dirs.
    for dir in ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = std::path::PathBuf::from(dir).join(name);
        if is_executable(&p) {
            return p.to_string_lossy().into_owned();
        }
    }

    // 5) Hardcoded fallback, same as historical behavior. Spawn will fail at runtime on a pure NixOS host with no bash.
    kind.hardcoded_default().to_string()
}

/// Whether `path` is an executable file.
///
/// First tries the file's mode bits (any-x); if that's inconclusive, falls back to invoking `<path> --version`.
/// The fallback exists for Nix: some overlay filesystems there expose binaries whose mode bits don't reflect their real executability.
#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.is_file()
        && meta.permissions().mode() & 0o111 != 0
    {
        return true;
    }

    // Nix fallback. Detach from the controlling TTY via xai_tty_utils so the probe cannot leak escapes onto the parent's terminal.
    // The resolver may run this during interactive TUI/pager startup; a misbehaving shell could otherwise spew garbage onto the pager screen
    // See `codegen-conventions` SKILL.md for the workspace-wide subprocess rule
    let mut cmd = std::process::Command::new(path);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    xai_tty_utils::detach_std_command(&mut cmd);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_preference_parser_normalizes_all_supported_aliases() {
        for value in [
            Some("bash"),
            Some("BASH"),
            Some("gitbash"),
            Some("git-bash"),
            Some("  Git-Bash  "),
            None,
            Some(""),
            Some("unknown"),
        ] {
            assert_eq!(canonical_windows_shell(value), "git-bash", "{value:?}");
        }
        for value in ["pwsh", "PWSh", "powershell-core", "powershell-7+"] {
            assert_eq!(canonical_windows_shell(Some(value)), "pwsh", "{value:?}");
        }
        for value in ["powershell", "Windows-PowerShell", "powershell-5.1"] {
            assert_eq!(
                canonical_windows_shell(Some(value)),
                "powershell",
                "{value:?}"
            );
        }
        for value in ["niubash", "Niubash", "niu", "niu.exe", "  NIU  "] {
            assert_eq!(canonical_windows_shell(Some(value)), "niubash", "{value:?}");
        }
    }

    #[test]
    fn preference_display_names_do_not_pin_future_pwsh_major() {
        assert_eq!(WindowsShellPreference::Pwsh.display_name(), "PowerShell 7+");
        assert_eq!(WindowsShellPreference::GitBash.display_name(), "Git Bash");
        assert_eq!(WindowsShellPreference::Niu.display_name(), "Niubash");
        assert_eq!(
            WindowsShellPreference::PowerShell.display_name(),
            "Windows PowerShell 5.1"
        );
    }

    /// The default must stay Git Bash: niubash is opt-in and never assumed present.
    #[test]
    fn git_bash_remains_the_product_default() {
        assert_eq!(WindowsShellPreference::default(), WindowsShellPreference::GitBash);
        assert_eq!(WindowsShellPreference::default().as_canonical(), "git-bash");
    }

    /// `path_guidance` is what tool descriptions branch on, so pin all three states.
    #[cfg(not(unix))]
    #[test]
    fn path_guidance_distinguishes_translation_states() {
        assert_eq!(
            WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into()).path_guidance(),
            PathGuidance::MsysTranslating
        );
        assert_eq!(
            WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()).path_guidance(),
            PathGuidance::DialectResolving
        );
        for shell in [WindowsShell::Pwsh, WindowsShell::PowerShell, WindowsShell::Cmd] {
            assert_eq!(shell.path_guidance(), PathGuidance::NoTranslationLayer);
        }
    }

    #[test]
    fn path_guidance_template_values_are_stable() {
        assert_eq!(PathGuidance::NoTranslationLayer.as_template_value(), "none");
        assert_eq!(
            PathGuidance::MsysTranslating.as_template_value(),
            "msys_translating"
        );
        assert_eq!(
            PathGuidance::DialectResolving.as_template_value(),
            "dialect_resolving"
        );
    }

    #[test]
    fn is_command_available_detects_present_and_absent() {
        // `cmd` resolves via PATHEXT on Windows, `sh` lives on $PATH on Unix
        #[cfg(windows)]
        let present = "cmd";
        #[cfg(not(windows))]
        let present = "sh";
        assert!(is_command_available(present));
        assert!(!is_command_available(
            "xai-definitely-not-a-real-command-xyz"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_shell_path_returns_a_bash() {
        // The resolver guarantees the result's file_name matches the requested kind, even for the hardcoded `/bin/bash` fallback
        let p = unix_shell_path(UnixShellKind::Bash);
        assert!(
            std::path::Path::new(p).file_name().and_then(|n| n.to_str()) == Some("bash"),
            "expected a path ending in 'bash', got {p}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_recognizes_bin_sh() {
        // /bin/sh is the one path POSIX promises across every Unix variant we care about; on macOS and Linux distros it's always executable
        // (Pure NixOS images may lack it, in which case this test is skipped.)
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        assert!(is_executable(std::path::Path::new("/bin/sh")));
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_rejects_non_executable() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(tmp.path()));
    }

    /// The Bash family ships the Unix utilities — Git Bash from MSYS2, niubash from
    /// winuxcmd; the PowerShell and cmd variants do not.
    #[cfg(not(unix))]
    #[test]
    fn has_unix_utilities_true_for_the_bash_family_only() {
        assert!(
            WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into())
                .has_unix_utilities()
        );
        assert!(WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()).has_unix_utilities());
        assert!(!WindowsShell::Pwsh.has_unix_utilities());
        assert!(!WindowsShell::PowerShell.has_unix_utilities());
        assert!(!WindowsShell::Cmd.has_unix_utilities());
    }

    /// niubash executes Bash, so the system prompt's `Shell:` line keeps reading
    /// `bash` and the model keeps writing Bash idiom.
    #[cfg(not(unix))]
    #[test]
    fn niubash_reports_the_bash_prompt_name() {
        assert_eq!(
            WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()).name(),
            "bash"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn configured_shell_preference_reads_effective_ui_layer() {
        let layers = crate::ConfigLayers {
            user: toml::from_str("[ui]\ndefault_shell = \"pwsh\"\n").unwrap(),
            ..Default::default()
        };
        let effective = layers.effective_config_base();
        let raw = effective
            .get("ui")
            .and_then(toml::Value::as_table)
            .and_then(|ui| ui.get("default_shell"))
            .and_then(toml::Value::as_str);
        assert_eq!(
            WindowsShellPreference::parse(raw),
            WindowsShellPreference::Pwsh
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn shell_for_preference_keeps_pwsh_and_windows_powershell_distinct() {
        assert!(matches!(
            shell_for_preference(WindowsShellPreference::Pwsh),
            Some(WindowsShell::Pwsh)
        ));
        assert!(matches!(
            shell_for_preference(WindowsShellPreference::PowerShell),
            Some(WindowsShell::PowerShell)
        ));
    }

    #[cfg(not(unix))]
    #[test]
    fn ampersand_semantics_per_windows_shell() {
        assert_eq!(
            WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into())
                .ampersand_semantics(),
            AmpersandSemantics::PosixBackground
        );
        assert_eq!(
            WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()).ampersand_semantics(),
            AmpersandSemantics::PosixBackground
        );
        assert_eq!(
            WindowsShell::Pwsh.ampersand_semantics(),
            AmpersandSemantics::PowerShellCore
        );
        assert_eq!(
            WindowsShell::PowerShell.ampersand_semantics(),
            AmpersandSemantics::WindowsPowerShell
        );
        assert_eq!(
            WindowsShell::Cmd.ampersand_semantics(),
            AmpersandSemantics::CmdSeparator
        );
    }

    /// niubash chains with `&&` like the other Bash-family shells.
    #[cfg(not(unix))]
    #[test]
    fn niubash_chains_with_double_ampersand() {
        assert!(
            WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()).supports_chain_operator()
        );
    }

    /// Every Windows shell variant, so env tests cover all of them without
    /// depending on the test host's installed shell.
    #[cfg(not(unix))]
    fn windows_shell_variants() -> Vec<WindowsShell> {
        vec![
            WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into()),
            WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()),
            WindowsShell::Pwsh,
            WindowsShell::PowerShell,
            WindowsShell::Cmd,
        ]
    }

    /// Every Windows shell variant injects the UTF-8 env defaults.
    #[cfg(not(unix))]
    #[test]
    fn invocation_for_sets_utf8_env_on_every_variant() {
        for shell in windows_shell_variants() {
            let inv = invocation_for(&shell, "echo hi");
            assert!(
                inv.env.contains(&("PYTHONUTF8", "1")),
                "expected PYTHONUTF8=1 in env for {shell:?}, got {:?}",
                inv.env
            );
            assert!(
                inv.env
                    .contains(&("PYTHONIOENCODING", "utf-8:surrogateescape")),
                "expected PYTHONIOENCODING=utf-8:surrogateescape in env for {shell:?}, got {:?}",
                inv.env
            );
        }
    }

    /// Regression guard for the path contract: no variant may disable MSYS path
    /// translation again. `MSYS_NO_PATHCONV=1` + `MSYS2_ARG_CONV_EXCL=*` left a POSIX
    /// path handed to a native tool verbatim (`python /c/Users/...` → `C:\c\Users\...`),
    /// a shape the model writes constantly; letting the translation layer stand is the
    /// fix, so neither variable may reappear in the spawned environment.
    #[cfg(not(unix))]
    #[test]
    fn invocation_for_injects_no_msys_path_guards() {
        for shell in windows_shell_variants() {
            let inv = invocation_for(&shell, "echo hi");
            for (name, _) in &inv.env {
                assert!(
                    !name.starts_with("MSYS"),
                    "{shell:?} must not inject {name}: {:?}",
                    inv.env
                );
            }
        }
    }

    /// Git Bash must *clear* the MSYS toggles, not merely leave them unset: a parent
    /// that still carries `MSYS_NO_PATHCONV=1` (a nested `grok-zh`, or every session
    /// under a leader started that way) would otherwise keep translation off, because
    /// Git for Windows reads the variable's presence and ignores its value.
    #[cfg(not(unix))]
    #[test]
    fn git_bash_clears_inherited_msys_path_guards() {
        let inv = invocation_for(
            &WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into()),
            "echo hi",
        );
        assert!(inv.remove_env.contains(&"MSYS_NO_PATHCONV"), "{:?}", inv.remove_env);
        assert!(inv.remove_env.contains(&"MSYS2_ARG_CONV_EXCL"), "{:?}", inv.remove_env);
    }

    /// The Bash family runs one `-c <command>` argv node; niubash is invoked exactly
    /// like Git Bash, with no translation-layer variables to carry. The command node
    /// is the stream-merging form, never a second argv node.
    #[cfg(not(unix))]
    #[test]
    fn bash_family_invocations_are_single_argv() {
        for shell in [
            WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into()),
            WindowsShell::Niu("C:\\tools\\niubash\\niu.exe".into()),
        ] {
            let inv = invocation_for(&shell, "echo hi");
            assert_eq!(
                inv.args,
                vec!["-c".to_string(), "exec 2>&1\necho hi".to_string()],
                "{shell:?}"
            );
        }
    }

    /// The merge adds one line ahead of the command and changes nothing after it, so
    /// every command shape survives verbatim — including the ones a `{ …; } 2>&1`
    /// group would have to special-case (`exit N`, a trailing `&`, a heredoc, a
    /// comment-only line, a trailing backslash, nothing at all).
    #[cfg(not(unix))]
    #[test]
    fn bash_family_merge_prefix_leaves_the_command_verbatim() {
        for command in [
            "echo hi",
            "echo before; exit 5",
            "sleep 1 &",
            "cat <<EOF\nhi\nEOF",
            "# note",
            "",
            "   ",
            "echo a \\",
            "echo a \\\\",
            "grep -rn '中文' . | head",
        ] {
            assert_eq!(
                bash_family_merged_command(command),
                format!("exec 2>&1\n{command}"),
                "{command:?}"
            );
        }
    }

    /// End-to-end guard for the reason the wrapper exists: the tool appends a stdout
    /// chunk before a stderr chunk on every tick, so without the merge an interleaved
    /// command reads back reordered. Skips a shell that is not installed.
    #[cfg(not(unix))]
    #[test]
    fn bash_family_invocation_keeps_stdout_and_stderr_in_write_order() {
        for shell in [
            find_git_bash().map(WindowsShell::GitBash),
            find_niu().map(WindowsShell::Niu),
        ]
        .into_iter()
        .flatten()
        {
            let inv = invocation_for(&shell, "echo o1; echo e1 1>&2; echo o2; echo e2 1>&2");
            let mut cmd = std::process::Command::new(&inv.program);
            cmd.args(&inv.args).envs(inv.env);
            for name in &inv.remove_env {
                cmd.env_remove(name);
            }
            let output = cmd.output().expect("spawn the shell");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(
                stdout, "o1\ne1\no2\ne2\n",
                "{shell:?} lost the write order of the two streams"
            );
            assert!(
                output.stderr.is_empty(),
                "{shell:?} kept a separate stderr: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// Pull the staged script path back out of the wrapper produced by
    /// [`stage_command_script`], so tests can inspect and clean up the file.
    #[cfg(not(unix))]
    fn staged_script_path(wrapper: &str) -> std::path::PathBuf {
        const MARKER: &str = "__grok_staged_cmd='";
        let start = wrapper.find(MARKER).expect("wrapper assigns the script path") + MARKER.len();
        let end = start + wrapper[start..].find('\'').expect("assignment is closed");
        std::path::PathBuf::from(&wrapper[start..end])
    }

    /// The ceiling is what decides inline versus staged, so pin its shape: the two
    /// Bash arms are bounded, and the arms without a script form are not.
    #[cfg(not(unix))]
    #[test]
    fn inline_ceiling_bounds_only_the_bash_family() {
        let git_bash = WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into());
        assert!(inline_command_ceiling(&git_bash) < 8_192);
        assert!(inline_command_ceiling(&WindowsShell::Niu("C:\\tools\\niu.exe".into())) < 32_767);
        for shell in [WindowsShell::Pwsh, WindowsShell::PowerShell, WindowsShell::Cmd] {
            assert_eq!(inline_command_ceiling(&shell), usize::MAX, "{shell:?}");
            assert!(stage_command_script(&shell, &"x".repeat(40_000)).is_none(), "{shell:?}");
        }
    }

    /// A staged command reaches the shell verbatim, and the wrapper is shaped so the
    /// script is sourced (shell state and exit code survive) and then removed.
    #[cfg(not(unix))]
    #[test]
    fn staged_wrapper_sources_and_removes_the_script() {
        let shell = WindowsShell::GitBash("C:\\Program Files\\Git\\bin\\bash.exe".into());
        let command = format!("echo BEGIN; echo {} TAIL", "x".repeat(9_000));
        let wrapper = stage_command_script(&shell, &command).expect("Git Bash stages long commands");

        let script = staged_script_path(&wrapper);
        assert_eq!(
            std::fs::read_to_string(&script).expect("staged script is readable"),
            format!("{command}\n"),
            "the script holds the command unchanged, with a newline for the last line"
        );
        assert!(
            wrapper.contains("trap 'rm -f \"$__grok_staged_cmd\"' EXIT"),
            "{wrapper}"
        );
        assert!(wrapper.ends_with(". \"$__grok_staged_cmd\""), "{wrapper}");
        let _ = std::fs::remove_file(&script);

        // Below the ceiling nothing is staged: the invoked shell is the only check.
        assert!(command.len() > inline_command_ceiling(&shell));
        assert!("echo hi".len() <= inline_command_ceiling(&shell));
    }

    /// The dispatch in [`shell_command_argv`] must stage exactly the commands the
    /// detected shell cannot take inline.
    #[cfg(not(unix))]
    #[test]
    fn shell_command_argv_stages_only_past_the_ceiling() {
        let shell = detect_windows_shell();
        let short = shell_command_argv("echo hi");
        // Below the ceiling the command stays inline; only the Bash family wraps it.
        let short_command = short
            .args
            .last()
            .map(String::as_str)
            .expect("argv has a command");
        assert!(
            short_command.contains("echo hi"),
            "{shell:?}: {short_command}"
        );

        let ceiling = inline_command_ceiling(shell);
        if ceiling == usize::MAX {
            return; // pwsh / cmd.exe arm: inline is the only form available
        }
        let inv = shell_command_argv(&format!("echo {}", "x".repeat(ceiling + 1)));
        let staged = inv.args.last().expect("invocation ends with the command text");
        assert!(staged.contains("__grok_staged_cmd"), "{shell:?}: {staged}");
        let _ = std::fs::remove_file(staged_script_path(staged));
    }

    /// End-to-end proof that staging is what saves a long command: each installed
    /// Bash-family shell runs a command past its inline ceiling, and the tail — which
    /// an inline Git Bash invocation drops without an error — must still run. The
    /// command exits non-zero to pin that the staged exit code survives, and the
    /// script must be gone afterwards.
    ///
    /// Skips a shell that is not installed, so a bare machine reports "ok".
    #[cfg(not(unix))]
    #[test]
    fn staged_command_survives_the_inline_ceiling_end_to_end() {
        for shell in [
            find_git_bash().map(WindowsShell::GitBash),
            find_niu().map(WindowsShell::Niu),
        ]
        .into_iter()
        .flatten()
        {
            let command = format!(
                "echo STAGED_BEGIN; echo {}; echo STAGED_END; exit 7",
                "x".repeat(inline_command_ceiling(&shell) + 1_000)
            );
            let wrapper = stage_command_script(&shell, &command).expect("Bash-family shell stages");
            let script = staged_script_path(&wrapper);
            let inv = invocation_for(&shell, &wrapper);

            let mut cmd = std::process::Command::new(&inv.program);
            cmd.args(&inv.args).envs(inv.env);
            for name in &inv.remove_env {
                cmd.env_remove(name);
            }
            let output = cmd.output().expect("spawn the shell");

            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("STAGED_END"),
                "{shell:?} lost the tail of a staged command: {stdout}"
            );
            assert_eq!(output.status.code(), Some(7), "{shell:?}: {output:?}");
            assert!(!script.exists(), "{shell:?} left its staged script behind");
        }
    }

    /// End-to-end check of the whole Git Bash invocation: build it through
    /// [`invocation_for`], spawn it while the parent still carries the MSYS guards
    /// (what a nested `grok-zh` or a leader-spawned session inherits), and require a
    /// POSIX path to reach a native program converted.
    ///
    /// Skips when Git Bash or `python` is unavailable on the test host, so a bare
    /// machine reports "ok" rather than a failure it cannot fix.
    #[cfg(not(unix))]
    #[test]
    fn a_leaked_msys_guard_does_not_survive_the_invocation() {
        let Some(bash) = find_git_bash() else { return };
        let inv = invocation_for(
            &WindowsShell::GitBash(bash),
            "python -c \"import sys;print(sys.argv[1])\" /c/Users/bypassnro",
        );
        let mut cmd = std::process::Command::new(&inv.program);
        // The guard is set before the invocation is applied, mirroring how it arrives
        // from a parent environment; `env_remove` must still win.
        cmd.env("MSYS_NO_PATHCONV", "1")
            .env("MSYS2_ARG_CONV_EXCL", "*");
        cmd.args(&inv.args).envs(inv.env);
        for name in &inv.remove_env {
            cmd.env_remove(name);
        }
        let Ok(output) = cmd.output() else { return };
        if !output.status.success() {
            return; // no python on this host
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("C:/Users/bypassnro") || stdout.contains("C:\\Users\\bypassnro"),
            "an inherited guard must not survive the invocation, got: {stdout}"
        );
    }
}
