//! `get_diagnostics` without a language server (batch 8).
//!
//! A language server is the memory-heavy fallback (gopls 120 MB-1 GB and a
//! 1-2 s cold start for one file's diagnostics). Unless one is already
//! running, or the caller passes `lsp=true`, diagnostics come from:
//!
//! - Go: `go test -c -o <null> -vet=off -gcflags=-e .` in the file's package
//!   (compiler errors of the package and its tests, all of them), then
//!   `go vet .` when it compiles (vet findings as warnings).
//! - Rust: `cargo check --message-format=json` in the file's crate.
//! - TypeScript: `tsc --noEmit -p <nearest tsconfig>`.
//! - Python: `compile()` of the file (syntax only, no bytecode written).
//! - Everything else, and whenever the toolchain is missing or times out:
//!   the first tree-sitter syntax error, labelled syntactic.
//!
//! The output is plain [`Diagnostic`]s for the same renderer, so the
//! environment-fault detection in [`crate::diag_env`] sees them unchanged.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use astrolabe_core::lsp::{Diagnostic, Position, Precise, Range, Severity};
use astrolabe_core::{Confidence, Language, RelPath};

/// Env override for every toolchain timeout, in seconds.
const TIMEOUT_ENV: &str = "ASTROLABE_DIAG_TIMEOUT_SECS";
/// `lsp` (or `on`) restores the old default: always ask a language server.
pub(crate) const MODE_ENV: &str = "ASTROLABE_DIAGNOSTICS";
/// Other files' errors named in the note.
const OTHER_FILES_LISTED: usize = 3;
/// Cap on one message (cargo/tsc can print long ones).
const MESSAGE_MAX_CHARS: usize = 600;

const LSP_HINT: &str = "要语言服务器诊断时传 lsp=true";

/// `ASTROLABE_DIAGNOSTICS=lsp`: language server by default, as before.
pub(crate) fn lsp_by_default() -> bool {
    std::env::var(MODE_ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "lsp" | "on" | "server"
        )
    })
}

fn timeout_for(default_secs: u64) -> Duration {
    let secs = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

/// Syntax-only diagnostics: the tree-sitter parse, no toolchain process.
pub(crate) fn check_syntax_only(
    root: &Path,
    rel: &RelPath,
    language: Language,
) -> Precise<Vec<Diagnostic>> {
    syntax_only(root, rel, language, "宿主设置只允许语法诊断")
}

/// Diagnostics for `rel` from the toolchain or the parser. Never starts a
/// language server.
pub(crate) fn check(root: &Path, rel: &RelPath, language: Language) -> Precise<Vec<Diagnostic>> {
    let started = Instant::now();
    let outcome = match language {
        Language::Go => go_check(root, rel),
        Language::Rust => rust_check(root, rel),
        Language::TypeScript | Language::Tsx => ts_check(root, rel),
        Language::Python => python_check(root, rel),
        _ => Outcome::Skipped(String::new()),
    };
    let elapsed = started.elapsed();
    match outcome {
        Outcome::Ran { tool, scope, diags } => finish(rel, tool, &scope, diags, elapsed),
        Outcome::Skipped(why) => syntax_only(root, rel, language, &why),
    }
}

enum Outcome {
    /// The toolchain answered. `diags` may include other files.
    Ran {
        tool: &'static str,
        scope: String,
        diags: Vec<Diagnostic>,
    },
    /// No toolchain answer (missing, timed out, not applicable): why, or
    /// empty when the language simply has no cheap checker here.
    Skipped(String),
}

/// Keep `rel`'s diagnostics; name the others in the note.
fn finish(
    rel: &RelPath,
    tool: &str,
    scope: &str,
    diags: Vec<Diagnostic>,
    elapsed: Duration,
) -> Precise<Vec<Diagnostic>> {
    let (mine, others): (Vec<Diagnostic>, Vec<Diagnostic>) =
        diags.into_iter().partition(|d| d.path == *rel);
    let mut note = format!(
        "来源：{tool}（{scope}，{:.1} s），未启动语言服务器；{LSP_HINT}。",
        elapsed.as_secs_f64()
    );
    let other_errors: Vec<&Diagnostic> = others
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    if !other_errors.is_empty() {
        let listed: Vec<String> = other_errors
            .iter()
            .take(OTHER_FILES_LISTED)
            .map(|d| format!("{}:{}", d.path.as_str(), d.range.start.line + 1))
            .collect();
        note.push_str(&format!(
            "\n其他文件另有 {} 条错误（{}{}）。",
            other_errors.len(),
            listed.join("、"),
            if other_errors.len() > OTHER_FILES_LISTED {
                " 等"
            } else {
                ""
            }
        ));
    }
    Precise {
        confidence: Confidence::Exact,
        note: Some(note),
        value: mine,
    }
}

/// First tree-sitter syntax error of the file, labelled syntactic.
fn syntax_only(
    root: &Path,
    rel: &RelPath,
    language: Language,
    why: &str,
) -> Precise<Vec<Diagnostic>> {
    let source = match astrolabe_core::text::read_source(&root.join(rel.as_str())) {
        Ok(s) => s,
        Err(e) => {
            return Precise::unknown(Vec::new(), format!("无法读取 `{}`：{e}", rel.as_str()));
        }
    };
    let mut value = Vec::new();
    if let Some((line, what)) = astrolabe_core::parse::first_syntax_error(language, &source) {
        value.push(diag_at(
            rel.clone(),
            line.saturating_sub(1),
            0,
            Severity::Error,
            format!("syntax error (tree-sitter): {what}"),
            None,
        ));
    }
    let mut note = String::new();
    if !why.is_empty() {
        note.push_str(why);
        if !why.ends_with(['。', '\n']) {
            note.push('。');
        }
    }
    note.push_str(&format!(
        "只做了语法检查（tree-sitter，只报第一处），没有类型检查；跑构建/测试，或{LSP_HINT}。"
    ));
    Precise {
        confidence: Confidence::Syntactic,
        note: Some(note),
        value,
    }
}

fn diag_at(
    path: RelPath,
    line: u32,
    character: u32,
    severity: Severity,
    message: String,
    code: Option<String>,
) -> Diagnostic {
    let message = if message.chars().count() > MESSAGE_MAX_CHARS {
        message.chars().take(MESSAGE_MAX_CHARS).collect::<String>() + "…"
    } else {
        message
    };
    Diagnostic {
        path,
        range: Range {
            start: Position { line, character },
            end: Position {
                line,
                character: character.saturating_add(1),
            },
        },
        severity,
        message,
        code,
    }
}

// ------------------------------------------------------------ processes

struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

enum RunError {
    /// The program is not installed (spawn failed with NotFound).
    Missing,
    TimedOut(Duration),
    Other(String),
}

/// Run `cmd` with a wall-clock limit; the whole process group is killed on
/// timeout (go / cargo / tsc spawn children).
fn run(mut cmd: Command, timeout: Duration) -> Result<Output, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunError::Missing,
        _ => RunError::Other(e.to_string()),
    })?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let read_out = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut s);
        }
        String::from_utf8_lossy(&s).into_owned()
    });
    let read_err = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_end(&mut s);
        }
        String::from_utf8_lossy(&s).into_owned()
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group(&mut child);
                let _ = child.wait();
                let _ = read_out.join();
                let _ = read_err.join();
                return Err(RunError::TimedOut(timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(RunError::Other(e.to_string())),
        }
    };
    Ok(Output {
        status: status.code(),
        stdout: read_out.join().unwrap_or_default(),
        stderr: read_err.join().unwrap_or_default(),
    })
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn skipped(tool: &str, error: RunError) -> Outcome {
    Outcome::Skipped(match error {
        RunError::Missing => format!("找不到 {tool}"),
        RunError::TimedOut(t) => format!("{tool} 超过 {} s 未完成，已终止", t.as_secs()),
        RunError::Other(e) => format!("{tool} 无法运行：{e}"),
    })
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// `dir/../x` → `x`, without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Repo-relative path of a tool-printed `file` (relative to `cwd` or
/// absolute); `None` outside the repo.
fn repo_rel(root: &Path, cwd: &Path, file: &str) -> Option<RelPath> {
    let p = Path::new(file);
    let abs = if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&cwd.join(p))
    };
    let root = normalize(root);
    let rel = abs.strip_prefix(&root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then(|| RelPath::new(s))
}

/// 1-based byte (`utf8 = true`) or char column on 1-based `line` of
/// `root/rel` → 0-based UTF-16 column.
fn utf16_column(root: &Path, rel: &RelPath, line: u32, col: u32, utf8: bool) -> u32 {
    let col0 = col.saturating_sub(1) as usize;
    let Ok(text) = std::fs::read_to_string(root.join(rel.as_str())) else {
        return col0 as u32;
    };
    let Some(l) = text.split('\n').nth(line.saturating_sub(1) as usize) else {
        return col0 as u32;
    };
    let prefix: String = if utf8 {
        let mut end = col0.min(l.len());
        while end > 0 && !l.is_char_boundary(end) {
            end -= 1;
        }
        l[..end].to_string()
    } else {
        l.chars().take(col0).collect()
    };
    prefix.encode_utf16().count() as u32
}

/// Nearest ancestor of `start` (inclusive, not above `root`) holding `name`.
fn find_up(root: &Path, start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(name).is_file() {
            return Some(d.to_path_buf());
        }
        if d == root {
            break;
        }
        dir = d.parent().filter(|p| p.starts_with(root));
    }
    None
}

// ------------------------------------------------------------------- Go

fn go_check(root: &Path, rel: &RelPath) -> Outcome {
    let file = root.join(rel.as_str());
    let Some(dir) = file.parent().map(Path::to_path_buf) else {
        return Outcome::Skipped(String::new());
    };
    let Some(mod_dir) = find_up(root, &dir, "go.mod") else {
        return Outcome::Skipped("文件不在 Go 模块里（找不到 go.mod），没法用 go 编译检查".into());
    };
    let timeout = timeout_for(30);
    let started = Instant::now();
    // A module that is not a `use` member of the active go.work (a nested
    // module pulled in by `replace`, e.g. aicon's src/third_party/go-qrcode)
    // only builds with the workspace off.
    let mut work_off = go_needs_work_off(&dir, &mod_dir);
    let pkg = dir
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".into());
    let scope = format!("包 {pkg}");
    // 1. `go vet .`: type-checks the package with its tests and runs the
    //    analyzers, without linking (30-50 MiB, 0.1-0.8 s on a warm cache).
    let vet = match run_go(&["vet", "."], &dir, &mut work_off, timeout) {
        Ok(out) => out,
        Err(e) => return skipped("go vet", e),
    };
    let vet_text = format!("{}\n{}", vet.stdout, vet.stderr);
    if go_workspace_trouble(&vet_text) {
        return go_workspace_skip(&vet_text);
    }
    if vet.status == Some(0) {
        return Outcome::Ran {
            tool: "go vet（含测试文件的类型检查）",
            scope,
            diags: parse_go(root, &dir, rel, &vet_text, Severity::Warning),
        };
    }
    if !go_compile_trouble(&vet_text) {
        // Only analyzer findings.
        return Outcome::Ran {
            tool: "go vet（含测试文件的类型检查）",
            scope,
            diags: parse_go(root, &dir, rel, &vet_text, Severity::Warning),
        };
    }
    // 2. It does not compile, and vet stops at the first type error: the
    //    compiler with -gcflags=-e lists all of them (tests included).
    let vet_diags = parse_go(root, &dir, rel, &vet_text, Severity::Error);
    let left = timeout.saturating_sub(started.elapsed());
    if left.is_zero() {
        return Outcome::Ran {
            tool: "go vet（只报第一处类型错误）",
            scope,
            diags: vet_diags,
        };
    }
    let compile = run_go(
        &[
            "test",
            "-c",
            "-o",
            null_device(),
            "-vet=off",
            "-gcflags=-e",
            ".",
        ],
        &dir,
        &mut work_off,
        left,
    );
    match compile {
        Ok(out)
            if out.status != Some(0)
                && !go_workspace_trouble(&format!("{}\n{}", out.stdout, out.stderr)) =>
        {
            let diags = parse_go(
                root,
                &dir,
                rel,
                &format!("{}\n{}", out.stdout, out.stderr),
                Severity::Error,
            );
            Outcome::Ran {
                tool: "go 编译器（含测试文件，-gcflags=-e 列全部错误）",
                scope,
                diags: if diags.is_empty() { vet_diags } else { diags },
            }
        }
        // It compiles after all: vet's `# pkg` header (Go <= 1.25 prints it
        // for analyzer findings too) was not a build failure, so the located
        // findings are analyzer warnings.
        Ok(out) if out.status == Some(0) => Outcome::Ran {
            tool: "go vet（含测试文件的类型检查）",
            scope,
            diags: parse_go(root, &dir, rel, &vet_text, Severity::Warning),
        },
        _ => Outcome::Ran {
            tool: "go vet（只报第一处类型错误）",
            scope,
            diags: vet_diags,
        },
    }
}

/// go-command messages that mean the workspace (go.work) does not cover the
/// module, not that the code is wrong. Never shown as diagnostics.
const GO_WORKSPACE_MARKERS: &[&str] = &[
    "does not contain package",
    "not one of the workspace modules",
    "outside modules listed in go.work",
    "directory prefix . does not contain modules listed in go.work",
];

fn go_workspace_trouble(text: &str) -> bool {
    GO_WORKSPACE_MARKERS.iter().any(|m| text.contains(m))
}

fn go_workspace_skip(text: &str) -> Outcome {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| go_workspace_trouble(l))
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();
    Outcome::Skipped(format!(
        "go 工作区（go.work）不覆盖这个模块，GOWORK=off 也没能编译检查（{line}）"
    ))
}

/// Run `go args` in `dir`; `work_off` adds `GOWORK=off`. When the output
/// says the workspace does not cover the module, retry once with the
/// workspace off (within the same time budget) and remember that.
fn run_go(
    args: &[&str],
    dir: &Path,
    work_off: &mut bool,
    timeout: Duration,
) -> Result<Output, RunError> {
    let started = Instant::now();
    let go = |off: bool, t: Duration| {
        let mut cmd = Command::new("go");
        cmd.args(args).current_dir(dir);
        if off {
            cmd.env("GOWORK", "off");
        }
        run(cmd, t)
    };
    let out = go(*work_off, timeout)?;
    if *work_off || !go_workspace_trouble(&format!("{}\n{}", out.stdout, out.stderr)) {
        return Ok(out);
    }
    let left = timeout.saturating_sub(started.elapsed());
    if left.is_zero() {
        return Ok(out);
    }
    *work_off = true;
    go(true, left)
}

/// The active go.work for `dir`: `$GOWORK` when set (`off` = none), else the
/// nearest go.work up to the file-system root (what the go command uses).
fn active_go_work(dir: &Path) -> Option<PathBuf> {
    match std::env::var("GOWORK") {
        Ok(v) if v.trim() == "off" => return None,
        Ok(v) if !v.trim().is_empty() => {
            let p = PathBuf::from(v.trim());
            return p.is_file().then_some(p);
        }
        _ => {}
    }
    dir.ancestors()
        .map(|d| d.join("go.work"))
        .find(|p| p.is_file())
}

/// Directories named by `use` in a go.work file (single-line and block
/// forms), resolved against the file's directory.
pub(crate) fn go_work_uses(work: &Path, text: &str) -> Vec<PathBuf> {
    let base = work.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let spec = if in_block {
            if line.starts_with(')') {
                in_block = false;
                continue;
            }
            line
        } else if let Some(rest) = line
            .strip_prefix("use")
            .filter(|r| r.starts_with([' ', '\t', '(']))
        {
            let rest = rest.trim_start();
            match rest.strip_prefix('(') {
                Some(inner) => {
                    let inner = inner.trim();
                    if let Some(one) = inner.strip_suffix(')') {
                        one.trim()
                    } else {
                        in_block = true;
                        if inner.is_empty() {
                            continue;
                        }
                        inner
                    }
                }
                None => rest,
            }
        } else {
            continue;
        };
        let spec = spec.trim_matches(|c| c == '"' || c == '`');
        if spec.is_empty() {
            continue;
        }
        let p = Path::new(spec);
        out.push(normalize(&if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        }));
    }
    out
}

/// `mod_dir` has an active go.work that does not `use` it.
fn go_needs_work_off(dir: &Path, mod_dir: &Path) -> bool {
    let Some(work) = active_go_work(dir) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&work) else {
        return false;
    };
    let module = normalize(mod_dir);
    !go_work_uses(&work, &text).contains(&module)
}

/// `go vet` output that means the package (or a dependency) does not build:
/// vet's own type-check failure (`vet: …`), a compile section header
/// (`# pkg`, not vet's `# [pkg]`), or a go-command failure without location.
fn go_compile_trouble(text: &str) -> bool {
    let re = located_go_re();
    text.lines().map(str::trim_end).any(|l| {
        l.starts_with("vet: ")
            || (l.starts_with("# ") && !l.starts_with("# ["))
            || (!l.is_empty()
                && !l.starts_with('\t')
                && !l.starts_with('#')
                && !l.starts_with("go: downloading")
                && !l.starts_with("go: finding module")
                && !re.is_match(l))
    })
}

/// `go test -c` / `go vet` output: `./a.go:7:9: msg`, `vet: ./a.go:7:9: msg`,
/// tab-indented continuation lines, `# pkg` headers. Lines without a
/// location that are not progress or summary (build-cache, module and
/// toolchain failures) are attached to `rel` line 1 as errors, so the
/// environment-fault check sees the root cause.
pub(crate) fn parse_go(
    root: &Path,
    cwd: &Path,
    rel: &RelPath,
    text: &str,
    located: Severity,
) -> Vec<Diagnostic> {
    let re = located_go_re();
    let mut out: Vec<Diagnostic> = Vec::new();
    let mut last_located = false;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            last_located = false;
            continue;
        }
        if raw.starts_with('\t') || raw.starts_with("    ") {
            if last_located {
                if let Some(d) = out.last_mut() {
                    d.message.push('\n');
                    d.message.push_str(line.trim());
                }
            }
            continue;
        }
        last_located = false;
        if line.starts_with('#')
            || line.starts_with("FAIL")
            || line.starts_with("ok ")
            || line.starts_with("? ")
            || line.starts_with("go: downloading")
            || line.starts_with("go: finding module")
            || line.starts_with("go: extracting")
        {
            continue;
        }
        if let Some(c) = re.captures(line) {
            let vet_typecheck = c.get(1).is_some();
            let Some(path) = repo_rel(root, cwd, &c[2]) else {
                continue;
            };
            let lno: u32 = c[3].parse().unwrap_or(1);
            let col = c
                .get(4)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .map(|col| utf16_column(root, &path, lno, col, true))
                .unwrap_or(0);
            let severity = if vet_typecheck {
                Severity::Error
            } else {
                located
            };
            out.push(diag_at(
                path,
                lno.saturating_sub(1),
                col,
                severity,
                c[5].to_string(),
                None,
            ));
            last_located = true;
            continue;
        }
        let msg = line.strip_prefix("vet: ").unwrap_or(line);
        out.push(diag_at(
            rel.clone(),
            0,
            0,
            Severity::Error,
            msg.to_string(),
            None,
        ));
    }
    out
}

fn located_go_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^(vet: )?(.+?\.go):(\d+)(?::(\d+))?: (.*)$").expect("go location regex")
    })
}

// ----------------------------------------------------------------- Rust

fn rust_check(root: &Path, rel: &RelPath) -> Outcome {
    let file = root.join(rel.as_str());
    let Some(dir) = file.parent() else {
        return Outcome::Skipped(String::new());
    };
    let Some(krate) = find_up(root, dir, "Cargo.toml") else {
        return Outcome::Skipped("文件不在 Cargo 包里（找不到 Cargo.toml）".into());
    };
    let in_targets = ["tests", "benches", "examples"]
        .iter()
        .any(|t| file.starts_with(krate.join(t)));
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--message-format=json", "--quiet"]);
    if in_targets {
        cmd.arg("--all-targets");
    }
    cmd.current_dir(&krate);
    let out = match run(cmd, timeout_for(90)) {
        Ok(out) => out,
        Err(e) => return skipped("cargo check", e),
    };
    // rustc paths are relative to the workspace root, which is the topmost
    // Cargo.toml with a [workspace] table (or the crate itself).
    let ws = workspace_root(root, &krate);
    let diags = parse_cargo(root, &ws, rel, &out.stdout, &out.stderr);
    let scope = krate
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".into());
    Outcome::Ran {
        tool: if in_targets {
            "cargo check --all-targets"
        } else {
            "cargo check（lib/bin，不含 #[cfg(test)]）"
        },
        scope: format!("crate {scope}"),
        diags,
    }
}

fn workspace_root(root: &Path, krate: &Path) -> PathBuf {
    let mut best = krate.to_path_buf();
    let mut dir = krate.parent();
    while let Some(d) = dir.filter(|d| d.starts_with(root)) {
        if std::fs::read_to_string(d.join("Cargo.toml")).is_ok_and(|t| t.contains("[workspace]")) {
            best = d.to_path_buf();
        }
        dir = d.parent();
    }
    best
}

pub(crate) fn parse_cargo(
    root: &Path,
    ws: &Path,
    rel: &RelPath,
    stdout: &str,
    stderr: &str,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let m = &v["message"];
        let severity = match m["level"].as_str().unwrap_or("") {
            "error" | "error: internal compiler error" => Severity::Error,
            "warning" => Severity::Warning,
            "note" | "help" => Severity::Information,
            _ => continue,
        };
        let message = m["message"].as_str().unwrap_or("").to_string();
        if message.starts_with("aborting due to")
            || message.contains("warnings emitted")
            || message.contains("warning emitted")
        {
            continue;
        }
        let code = m["code"]["code"].as_str().map(str::to_string);
        let span = m["spans"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["is_primary"] == true));
        match span {
            Some(span) => {
                let Some(path) = span["file_name"]
                    .as_str()
                    .and_then(|f| repo_rel(root, ws, f))
                else {
                    continue;
                };
                let lno = span["line_start"].as_u64().unwrap_or(1) as u32;
                let col = span["column_start"].as_u64().unwrap_or(1) as u32;
                let col = utf16_column(root, &path, lno, col, false);
                out.push(diag_at(
                    path,
                    lno.saturating_sub(1),
                    col,
                    severity,
                    message,
                    code,
                ));
            }
            None if severity == Severity::Error => {
                out.push(diag_at(rel.clone(), 0, 0, severity, message, code));
            }
            None => {}
        }
    }
    // Cargo's own failures (manifest, lock file, registry) are plain text.
    for line in stderr.lines() {
        let t = line.trim();
        if let Some(msg) = t.strip_prefix("error: ") {
            if !msg.starts_with("could not compile") {
                out.push(diag_at(
                    rel.clone(),
                    0,
                    0,
                    Severity::Error,
                    msg.to_string(),
                    None,
                ));
            }
        }
    }
    out
}

// ----------------------------------------------------------- TypeScript

fn ts_check(root: &Path, rel: &RelPath) -> Outcome {
    let file = root.join(rel.as_str());
    let Some(dir) = file.parent() else {
        return Outcome::Skipped(String::new());
    };
    let Some(project) = find_up(root, dir, "tsconfig.json") else {
        return Outcome::Skipped("找不到 tsconfig.json".into());
    };
    // The project's own TypeScript first: a global tsc may be another version.
    let local = {
        let mut d = Some(project.as_path());
        let mut found = None;
        while let Some(p) = d {
            let bin =
                p.join("node_modules/.bin")
                    .join(if cfg!(windows) { "tsc.cmd" } else { "tsc" });
            if bin.is_file() {
                found = Some(bin);
                break;
            }
            d = p.parent().filter(|x| x.starts_with(root));
        }
        found
    };
    let mut cmd = Command::new(local.as_deref().unwrap_or(Path::new("tsc")));
    cmd.args(["--noEmit", "--pretty", "false", "-p", "tsconfig.json"])
        .current_dir(&project);
    let out = match run(cmd, timeout_for(45)) {
        Ok(out) => out,
        Err(e) => return skipped("tsc", e),
    };
    let diags = parse_tsc(
        root,
        &project,
        rel,
        &format!("{}\n{}", out.stdout, out.stderr),
    );
    let scope = project
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}/tsconfig.json"))
        .unwrap_or_else(|| "tsconfig.json".into());
    Outcome::Ran {
        tool: "tsc --noEmit",
        scope,
        diags,
    }
}

pub(crate) fn parse_tsc(root: &Path, cwd: &Path, rel: &RelPath, text: &str) -> Vec<Diagnostic> {
    static RE: std::sync::OnceLock<(regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (located, global) = RE.get_or_init(|| {
        (
            regex::Regex::new(r"^(.+?)\((\d+),(\d+)\): (error|warning|message) TS(\d+): (.*)$")
                .expect("tsc regex"),
            regex::Regex::new(r"^(error|warning) TS(\d+): (.*)$").expect("tsc regex"),
        )
    });
    let mut out: Vec<Diagnostic> = Vec::new();
    let mut last = false;
    for raw in text.lines() {
        if raw.starts_with(' ') && last {
            if let Some(d) = out.last_mut() {
                d.message.push('\n');
                d.message.push_str(raw.trim());
            }
            continue;
        }
        last = false;
        let line = raw.trim_end();
        let sev = |s: &str| match s {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            _ => Severity::Information,
        };
        if let Some(c) = located.captures(line) {
            let Some(path) = repo_rel(root, cwd, &c[1]) else {
                continue;
            };
            let lno: u32 = c[2].parse().unwrap_or(1);
            // tsc columns are 1-based UTF-16 already.
            let col = c[3].parse::<u32>().unwrap_or(1).saturating_sub(1);
            out.push(diag_at(
                path,
                lno.saturating_sub(1),
                col,
                sev(&c[4]),
                c[6].to_string(),
                Some(c[5].to_string()),
            ));
            last = true;
        } else if let Some(c) = global.captures(line) {
            out.push(diag_at(
                rel.clone(),
                0,
                0,
                sev(&c[1]),
                c[3].to_string(),
                Some(c[2].to_string()),
            ));
            last = true;
        }
    }
    out
}

// --------------------------------------------------------------- Python

const PY_SCRIPT: &str = "import sys\n\
p=sys.argv[1]\n\
try:\n compile(open(p,'rb').read(),p,'exec')\n\
except SyntaxError as e:\n print('%s\\t%s\\t%s: %s'%(e.lineno or 1,e.offset or 1,type(e).__name__,e.msg))\n\
except ValueError as e:\n print('1\\t1\\tValueError: %s'%e)\n";

fn python_check(root: &Path, rel: &RelPath) -> Outcome {
    let interpreter = [
        ".venv/bin/python",
        "venv/bin/python",
        ".venv/Scripts/python.exe",
    ]
    .iter()
    .map(|p| root.join(p))
    .find(|p| p.is_file())
    .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }));
    let mut cmd = Command::new(&interpreter);
    cmd.args(["-B", "-c", PY_SCRIPT])
        .arg(root.join(rel.as_str()))
        .current_dir(root);
    let out = match run(cmd, timeout_for(10)) {
        Ok(out) => out,
        Err(e) => return skipped("python", e),
    };
    if out.status != Some(0) {
        return Outcome::Skipped(format!(
            "python 语法检查失败：{}",
            out.stderr.lines().last().unwrap_or("").trim()
        ));
    }
    let mut diags = Vec::new();
    for line in out.stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(l), Some(c), Some(msg)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let lno: u32 = l.parse().unwrap_or(1);
        let col: u32 = c.parse().unwrap_or(1);
        let col = utf16_column(root, rel, lno, col, false);
        diags.push(diag_at(
            rel.clone(),
            lno.saturating_sub(1),
            col,
            Severity::Error,
            msg.to_string(),
            None,
        ));
    }
    Outcome::Ran {
        tool: "python compile()（只查语法，没有类型检查）",
        scope: "单文件".into(),
        diags,
    }
}

// ------------------------------------------------------ shared gopls

/// A shared gopls daemon (`-remote=auto`) is listening for this user: a
/// diagnostics request then costs a thin forwarder, not a fresh gopls.
pub(crate) fn gopls_daemon_running() -> bool {
    // Only when this session's gopls would join it (`-remote=auto`).
    let setting = std::env::var(astrolabe_core::lsp::discovery::GOPLS_REMOTE_ENV)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !matches!(setting.as_str(), "" | "auto" | "on" | "1" | "true" | "yes") {
        return false;
    }
    #[cfg(unix)]
    {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("gopls-")
                && name.contains("-daemon.")
                && std::os::unix::net::UnixStream::connect(entry.path()).is_ok()
            {
                return true;
            }
        }
        false
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrolabe-diagtc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    fn have(bin: &str) -> bool {
        Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
            || Command::new(bin)
                .arg("version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
    }

    #[test]
    fn go_output_parses_locations_continuations_and_root_causes() {
        let root = Path::new("/r");
        let cwd = Path::new("/r/p");
        let rel = RelPath::new("p/a.go");
        let text = "# ex.com/m/p [ex.com/m/p.test]\n\
./a.go:7:9: undefined: undefinedThing\n\
./a_test.go:5:40: cannot use \"s\" (untyped string constant) as int value\n\
../q/b.go:3: something\n\
\thave (int)\n\
\twant (string)\n\
vet: ./a.go:2:9: expected ')', found '{'\n\
go: downloading example.com/x v1.0.0\n\
failed to initialize build cache at /c: mkdir /c/00: permission denied\n\
FAIL\tex.com/m/p [build failed]\n";
        let d = parse_go(root, cwd, &rel, text, Severity::Error);
        let got: Vec<String> = d
            .iter()
            .map(|d| {
                format!(
                    "{}:{}:{}",
                    d.path.as_str(),
                    d.range.start.line + 1,
                    d.range.start.character
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                "p/a.go:7:8",
                "p/a_test.go:5:39",
                "q/b.go:3:0",
                "p/a.go:2:8",
                "p/a.go:1:0"
            ]
        );
        assert_eq!(d[2].message, "something\nhave (int)\nwant (string)");
        assert!(d[4].message.starts_with("failed to initialize build cache"));
        // Vet findings are warnings, vet's type-check failures stay errors.
        let d = parse_go(
            root,
            cwd,
            &rel,
            "a.go:6:14: fmt.Printf format %d has arg\nvet: a.go:1:1: undefined: x\n",
            Severity::Warning,
        );
        assert_eq!(d[0].severity, Severity::Warning);
        assert_eq!(d[1].severity, Severity::Error);
        // vet output that needs the compiler pass vs plain analyzer findings.
        assert!(go_compile_trouble(
            "# ex.com/m/q\n../q/q.go:2:23: undefined: nope\n"
        ));
        assert!(go_compile_trouble(
            "# ex.com/m/p\n# [ex.com/m/p]\nvet: ./a.go:10:17: undefined: zz\n"
        ));
        assert!(go_compile_trouble(
            "failed to initialize build cache at /c: permission denied\n"
        ));
        assert!(!go_compile_trouble(
            "a.go:9:14: fmt.Printf format %d has arg \"x\" of wrong type string\n"
        ));
        assert!(!go_compile_trouble("# [ex.com/m/p]\na.go:9:14: printf\n"));
        // The go command's read-only cache failure is an environment fault.
        let d = parse_go(
            root,
            cwd,
            &rel,
            "failed to initialize build cache at /c: mkdir /c/00: permission denied\n",
            Severity::Error,
        );
        assert!(crate::diag_env::detect("p/a.go", &d).is_some());
    }

    #[test]
    fn cargo_and_tsc_output_parse() {
        let root = Path::new("/r");
        let rel = RelPath::new("src/lib.rs");
        let stdout = r#"{"reason":"compiler-artifact"}
{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `x` in this scope","code":{"code":"E0425"},"spans":[{"file_name":"src/lib.rs","line_start":3,"column_start":5,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"warning","message":"unused variable: `y`","code":null,"spans":[{"file_name":"/home/u/.cargo/registry/dep.rs","line_start":1,"column_start":1,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error","code":null,"spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"can't find crate for `std`","code":{"code":"E0463"},"spans":[]}}"#;
        let d = parse_cargo(
            root,
            root,
            &rel,
            stdout,
            "error: could not compile `x`\nerror: failed to load manifest\n",
        );
        let got: Vec<(String, u32, Option<String>)> = d
            .iter()
            .map(|d| {
                (
                    d.path.as_str().to_string(),
                    d.range.start.line + 1,
                    d.code.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/lib.rs".into(), 3, Some("E0425".into())),
                ("src/lib.rs".into(), 1, Some("E0463".into())),
                ("src/lib.rs".into(), 1, None),
            ]
        );
        assert!(
            crate::diag_env::detect("src/lib.rs", &d).is_some(),
            "sysroot fault"
        );

        let rel = RelPath::new("web/src/a.ts");
        let text = "src/a.ts(3,5): error TS2304: Cannot find name 'x'.\n  Did you mean 'y'?\nsrc/b.ts(1,1): error TS2307: Cannot find module 'express'.\nerror TS2318: Cannot find global type 'Array'.\n";
        let d = parse_tsc(root, Path::new("/r/web"), &rel, text);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].path.as_str(), "web/src/a.ts");
        assert_eq!((d[0].range.start.line, d[0].range.start.character), (2, 4));
        assert_eq!(d[0].code.as_deref(), Some("2304"));
        assert!(d[0].message.ends_with("Did you mean 'y'?"));
        assert_eq!(d[2].path.as_str(), "web/src/a.ts");
        let fault = crate::diag_env::detect("web/src/a.ts", &d).expect("no lib");
        assert!(fault.kind.contains("lib"));
    }

    #[test]
    fn finish_keeps_the_file_and_names_other_files() {
        let rel = RelPath::new("p/a.go");
        let d = |p: &str, l: u32| diag_at(RelPath::new(p), l, 0, Severity::Error, "x".into(), None);
        let r = finish(
            &rel,
            "go",
            "包 p",
            vec![d("p/a.go", 1), d("p/b.go", 4), d("q/c.go", 9)],
            Duration::from_millis(420),
        );
        assert_eq!(r.confidence, Confidence::Exact);
        assert_eq!(r.value.len(), 1);
        let note = r.note.unwrap();
        assert!(
            note.contains("未启动语言服务器") && note.contains("lsp=true"),
            "{note}"
        );
        assert!(
            note.contains("其他文件另有 2 条错误（p/b.go:5、q/c.go:10）"),
            "{note}"
        );
    }

    #[test]
    fn go_real_toolchain_errors_and_clean() {
        if !have("go") {
            return;
        }
        let dir = repo(
            "go",
            &[
                ("go.mod", "module ex.com/m\n\ngo 1.21\n"),
                ("p/a.go", "package p\n\nimport \"fmt\"\n\nfunc A() int {\n\tfmt.Printf(\"%d\\n\", \"x\")\n\treturn 1\n}\n"),
                ("p/a_test.go", "package p\n\nimport \"testing\"\n\nfunc TestA(t *testing.T) { var x int = \"s\"; _ = x; _ = undefinedY }\n"),
            ],
        );
        let rel = RelPath::new("p/a_test.go");
        let r = check(&dir, &rel, Language::Go);
        assert_eq!(r.confidence, Confidence::Exact, "{r:?}");
        assert_eq!(r.value.len(), 2, "both errors (-gcflags=-e): {r:?}");
        assert!(r
            .value
            .iter()
            .all(|d| d.path == rel && d.severity == Severity::Error));
        // Fix the test file: compiles, vet's printf finding shows on a.go.
        std::fs::write(
            dir.join("p/a_test.go"),
            "package p\n\nimport \"testing\"\n\nfunc TestA(t *testing.T) {}\n",
        )
        .unwrap();
        let r = check(&dir, &RelPath::new("p/a.go"), Language::Go);
        assert_eq!(r.confidence, Confidence::Exact, "{r:?}");
        assert_eq!(r.value.len(), 1, "{r:?}");
        assert_eq!(r.value[0].severity, Severity::Warning);
        // Line 6; the printf analyzer reports the bad argument (col 14) on
        // recent Go and the call (col 2, after the tab) on older ones.
        assert_eq!(r.value[0].range.start.line, 5, "{r:?}");
        assert!(matches!(r.value[0].range.start.character, 1 | 13), "{r:?}");
        assert!(r.note.as_deref().unwrap().contains("go vet"));
        let r = check(&dir, &rel, Language::Go);
        assert!(r.value.is_empty(), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_work_use_lines_parse() {
        let work = Path::new("/r/src/go.work");
        let text = "go 1.26.0\n\nuse .\nuse ./tools // dev\nuse (\n\t./a\n\t\"../b\"\n)\nuse (./c)\nuser x\nreplace x => ./y\n";
        let got: Vec<String> = go_work_uses(work, text)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            vec!["/r/src", "/r/src/tools", "/r/src/a", "/r/b", "/r/src/c"]
        );
        assert!(go_workspace_trouble(
            "main module (github.com/x/AI-con) does not contain package github.com/x/AI-con/third_party/q"
        ));
        assert!(!go_workspace_trouble("./a.go:3:1: undefined: x"));
    }

    /// aicon layout: src/go.work `use .`, src/go.mod replaces a nested module
    /// (src/third_party/lib, its own go.mod, not a `use` member). Its files
    /// get real diagnostics (GOWORK=off), never the workspace error.
    #[test]
    fn go_nested_replaced_module_outside_go_work() {
        if !have("go") || std::env::var_os("GOWORK").is_some() {
            return;
        }
        let dir = repo(
            "gowork",
            &[
                ("src/go.work", "go 1.21\n\nuse .\n"),
                (
                    "src/go.mod",
                    "module ex.com/app\n\ngo 1.21\n\nrequire ex.com/lib v0.0.0\n\nreplace ex.com/lib => ./third_party/lib\n",
                ),
                (
                    "src/main.go",
                    "package main\n\nimport \"ex.com/lib\"\n\nfunc main() { _ = lib.L() }\n",
                ),
                ("src/third_party/lib/go.mod", "module ex.com/lib\n\ngo 1.21\n"),
                (
                    "src/third_party/lib/lib.go",
                    "package lib\n\nfunc L() int { return missing }\n",
                ),
                ("src/third_party/lib/ok/ok.go", "package ok\n\nfunc O() int { return 1 }\n"),
            ],
        );
        let mod_dir = dir.join("src/third_party/lib");
        assert!(go_needs_work_off(&mod_dir, &mod_dir));
        assert!(!go_needs_work_off(&dir.join("src"), &dir.join("src")));
        let rel = RelPath::new("src/third_party/lib/lib.go");
        let r = check(&dir, &rel, Language::Go);
        assert_eq!(r.confidence, Confidence::Exact, "{r:?}");
        assert_eq!(r.value.len(), 1, "{r:?}");
        assert!(r.value[0].message.contains("undefined: missing"), "{r:?}");
        assert!(r.value.iter().all(|d| !go_workspace_trouble(&d.message)));
        // A clean package of the nested module: clean, not the workspace error.
        let r = check(
            &dir,
            &RelPath::new("src/third_party/lib/ok/ok.go"),
            Language::Go,
        );
        assert_eq!(r.confidence, Confidence::Exact, "{r:?}");
        assert!(r.value.is_empty(), "{r:?}");
        // The workspace member still builds through go.work.
        let r = check(&dir, &RelPath::new("src/main.go"), Language::Go);
        assert!(
            r.value.iter().all(|d| !go_workspace_trouble(&d.message)),
            "{r:?}"
        );
        // No go.mod at all (only a go.work): skipped, syntax only.
        std::fs::create_dir_all(dir.join("loose")).unwrap();
        std::fs::write(dir.join("go.work"), "go 1.21\n").unwrap();
        std::fs::write(dir.join("loose/x.go"), "package x\n").unwrap();
        let r = check(&dir, &RelPath::new("loose/x.go"), Language::Go);
        assert_eq!(r.confidence, Confidence::Syntactic, "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn python_and_syntax_only_paths() {
        let dir = repo(
            "py",
            &[
                ("a.py", "def f(:\n    pass\n"),
                ("ok.py", "x = 1\n"),
                ("Main.java", "class Main { void f( { } }\n"),
            ],
        );
        if have("python3") {
            let r = check(&dir, &RelPath::new("a.py"), Language::Python);
            assert_eq!(r.confidence, Confidence::Exact, "{r:?}");
            assert_eq!(r.value.len(), 1);
            assert!(r.value[0].message.starts_with("SyntaxError"), "{r:?}");
            assert!(!dir.join("__pycache__").exists(), "no bytecode written");
            let r = check(&dir, &RelPath::new("ok.py"), Language::Python);
            assert!(r.value.is_empty(), "{r:?}");
        }
        let r = check(&dir, &RelPath::new("Main.java"), Language::Java);
        assert_eq!(r.confidence, Confidence::Syntactic, "{r:?}");
        assert_eq!(r.value.len(), 1, "{r:?}");
        assert!(r.value[0].message.starts_with("syntax error (tree-sitter)"));
        assert!(crate::diag_env::is_syntax_error(&r.value[0]));
        assert!(r.note.unwrap().contains("lsp=true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ts_real_tsc_reports_type_errors() {
        if !have("tsc") {
            return;
        }
        let dir = repo(
            "ts",
            &[
                ("tsconfig.json", "{\"compilerOptions\":{\"strict\":true,\"noEmit\":true},\"include\":[\"src\"]}\n"),
                ("src/a.ts", "export const n: number = \"s\";\n"),
                ("src/b.ts", "export const ok = 1;\n"),
            ],
        );
        let r = check(&dir, &RelPath::new("src/a.ts"), Language::TypeScript);
        assert_eq!(r.confidence, Confidence::Exact, "{r:?}");
        assert_eq!(r.value.len(), 1, "{r:?}");
        assert_eq!(r.value[0].code.as_deref(), Some("2322"));
        let r = check(&dir, &RelPath::new("src/b.ts"), Language::TypeScript);
        assert!(r.value.is_empty(), "{r:?}");
        assert!(r
            .note
            .unwrap()
            .contains("其他文件另有 1 条错误（src/a.ts:1）"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_kills_and_falls_back() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let t = Instant::now();
        match run(cmd, Duration::from_millis(200)) {
            Err(RunError::TimedOut(_)) => {}
            Err(RunError::Missing) => return,
            _ => panic!("expected timeout"),
        }
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(matches!(
            run(
                Command::new("astrolabe-no-such-tool"),
                Duration::from_secs(1)
            ),
            Err(RunError::Missing)
        ));
    }
}
