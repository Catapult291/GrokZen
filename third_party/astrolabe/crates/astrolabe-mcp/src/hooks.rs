//! Serena 式防漂移 hooks：PreToolUse 计数连续 grep/read 滥用，超阈值 deny + 提醒。
//! 入口由 cli 分发；本模块只做协议与计数。
//!
//! 可通过环境变量覆盖默认阈值：
//! - `ASTROLABE_SLICE_READ_MAX`: 切片精读最大行数（默认 200）
//! - `ASTROLABE_SLICE_READ_THRESHOLD`: 连续切片读提醒阈值（默认 10，期间无任何 astrolabe 工具调用）
//! - `ASTROLABE_READ_THRESHOLD`: 连续全文件 Read deny 阈值（默认 3）
//! - `ASTROLABE_GREP_THRESHOLD`: 连续 Grep/shell 文本搜索 deny 阈值（默认 3）
//! - `ASTROLABE_DENY_SILENCE_SECS`: deny 后静默窗口秒数（默认 15）
//! - `ASTROLABE_DENY_TEXT_SEARCH`: 硬禁用 Grep/Glob/shell 文本搜索（true/1/yes/on；默认 off）。
//!   开启后首次即 deny（不再等阈值）；覆盖 soft DEFAULT_GREP_THRESHOLD。亦可通过
//!   `hooks remind --deny-text-search` / 上下文 YAML `deny_text_search` 配置。
//!
//! 子代理计数隔离：并发子 Agent 触发的 PreToolUse payload 携带 `agent_id` / `agentId`
//! （主线程 payload 不含该字段）时，计数状态按 `~/.astrolabe/hook_data/<session_id>/<agent分量>/`
//! 分片落盘，各子代理独立计数。字段缺失、空白或类型不符（非 String/Number）时静默回退会话级
//! 平面路径；标识含非法路径字符或超长（>128 字节）时收敛为 `ag-<fnv1a64 hex>` 哈希分片
//! （尽力而为，不报错）。
//!
//! 支持客户端：claude-code / codebuddy / vscode / codex / grok，以及 CC 协议同构的
//! kimicode（Moonshot Kimi Code CLI）、zcode（智谱 Z.ai ZCode）、cursor（Cursor CLI/Agent，
//! 原生扁平输出格式）、opencode（插件桥接调用）。session_id 提取链含 `conversation_id` /
//! `conversationId` fallback：Cursor 原生 preToolUse 只带 conversation_id，缺失该 fallback
//! 会导致 exit 2，而 Cursor 语义下 exit 2 = deny，将误拦一切工具调用。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// 阈值常量（照抄 Serena 数值）
pub(crate) const DEFAULT_GREP_THRESHOLD: u32 = 3;
pub(crate) const DEFAULT_READ_THRESHOLD: u32 = 3;
pub(crate) const NON_SYMBOLIC_THRESHOLD: u32 = 4;
/// 连续切片读提醒阈值默认值：连续 N 次切片读且期间无任何 astrolabe 工具调用 → deny 提醒。
/// 实测漂移会话出现过 259 次切片读零拦截（旧实现完全豁免），故切片读改为连续额度制。
pub(crate) const DEFAULT_SLICE_READ_THRESHOLD: u32 = 10;

/// 重置周期（秒）：两次同类调用间隔超过该值才重置计数
pub(crate) const GREP_RESET_PERIOD_SECONDS: f64 = 1000.0;
pub(crate) const READ_RESET_PERIOD_SECONDS: f64 = 1000.0;
pub(crate) const NON_SYMBOLIC_RESET_PERIOD_SECONDS: f64 = 2000.0;
/// 切片读额度重置周期（秒）：与 non_symbolic 同档
pub(crate) const SLICE_RESET_PERIOD_SECONDS: f64 = 2000.0;

/// deny 后静默窗口（秒）默认值：窗口内整个 hook 变为 no-op（不增计数、不发 deny）
pub(crate) const DEFAULT_MIN_DENY_INTERVAL_SECONDS: f64 = 15.0;

/// 切片精读（slice read）最大 limit 默认值：显式带 limit 且 limit<=200 的局部阅读视为切片读。
/// 切片读不再无限豁免：Read 工具切片计入连续切片额度（`DEFAULT_SLICE_READ_THRESHOLD`），
/// 连续 10 次且期间无任何 astrolabe 工具调用即触发提醒式 deny；
/// shell 切片打印（sed -n / head -n / tail -n）保持中立不计数。
/// 注意：仅有 offset 而无合法 limit 时不算切片精读（Claude Code 默认会读约 2000 行）。
pub(crate) const DEFAULT_SLICE_READ_MAX_LIMIT: u64 = 200;

/// 环境变量名称
pub(crate) const ENV_SLICE_READ_MAX: &str = "ASTROLABE_SLICE_READ_MAX";
pub(crate) const ENV_SLICE_READ_THRESHOLD: &str = "ASTROLABE_SLICE_READ_THRESHOLD";
pub(crate) const ENV_READ_THRESHOLD: &str = "ASTROLABE_READ_THRESHOLD";
pub(crate) const ENV_GREP_THRESHOLD: &str = "ASTROLABE_GREP_THRESHOLD";
pub(crate) const ENV_DENY_SILENCE_SECS: &str = "ASTROLABE_DENY_SILENCE_SECS";
pub(crate) const ENV_DENY_TEXT_SEARCH: &str = "ASTROLABE_DENY_TEXT_SEARCH";

/// 运行时配置（从环境变量解析，进程启动后只读）
#[derive(Debug, Clone, Copy)]
pub(crate) struct HooksConfig {
    pub slice_read_max_limit: u64,
    pub slice_read_threshold: u32,
    pub read_threshold: u32,
    pub grep_threshold: u32,
    /// Hard-deny Grep/Glob/shell text-search on first use.
    pub deny_text_search: bool,
    pub min_deny_interval_seconds: f64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            slice_read_max_limit: DEFAULT_SLICE_READ_MAX_LIMIT,
            slice_read_threshold: DEFAULT_SLICE_READ_THRESHOLD,
            read_threshold: DEFAULT_READ_THRESHOLD,
            grep_threshold: DEFAULT_GREP_THRESHOLD,
            deny_text_search: false,
            min_deny_interval_seconds: DEFAULT_MIN_DENY_INTERVAL_SECONDS,
        }
    }
}

impl HooksConfig {
    /// 从环境变量解析配置；无效值回退默认并 tracing::warn
    pub fn from_env() -> Self {
        let slice_read_max_limit = std::env::var(ENV_SLICE_READ_MAX)
            .ok()
            .and_then(|s| {
                s.trim().parse::<u64>().ok().or_else(|| {
                    tracing::warn!(
                        "invalid {ENV_SLICE_READ_MAX}={s:?}; using default {}",
                        DEFAULT_SLICE_READ_MAX_LIMIT
                    );
                    None
                })
            })
            .unwrap_or(DEFAULT_SLICE_READ_MAX_LIMIT);

        let slice_read_threshold = std::env::var(ENV_SLICE_READ_THRESHOLD)
            .ok()
            .and_then(|s| {
                s.trim().parse::<u32>().ok().or_else(|| {
                    tracing::warn!(
                        "invalid {ENV_SLICE_READ_THRESHOLD}={s:?}; using default {}",
                        DEFAULT_SLICE_READ_THRESHOLD
                    );
                    None
                })
            })
            .unwrap_or(DEFAULT_SLICE_READ_THRESHOLD);

        let read_threshold = std::env::var(ENV_READ_THRESHOLD)
            .ok()
            .and_then(|s| {
                s.trim().parse::<u32>().ok().or_else(|| {
                    tracing::warn!(
                        "invalid {ENV_READ_THRESHOLD}={s:?}; using default {}",
                        DEFAULT_READ_THRESHOLD
                    );
                    None
                })
            })
            .unwrap_or(DEFAULT_READ_THRESHOLD);

        let grep_threshold = std::env::var(ENV_GREP_THRESHOLD)
            .ok()
            .and_then(|s| {
                s.trim().parse::<u32>().ok().or_else(|| {
                    tracing::warn!(
                        "invalid {ENV_GREP_THRESHOLD}={s:?}; using default {}",
                        DEFAULT_GREP_THRESHOLD
                    );
                    None
                })
            })
            .unwrap_or(DEFAULT_GREP_THRESHOLD);
        if grep_threshold == 0 {
            tracing::warn!(
                "{ENV_GREP_THRESHOLD}=0 is treated as 1 (first grep denied); use ASTROLABE_DENY_TEXT_SEARCH for hard deny"
            );
        }

        let deny_text_search =
            parse_env_bool(std::env::var(ENV_DENY_TEXT_SEARCH).ok().as_deref()).unwrap_or(false);

        let min_deny_interval_seconds = std::env::var(ENV_DENY_SILENCE_SECS)
            .ok()
            .and_then(|s| {
                s.trim().parse::<f64>().ok().or_else(|| {
                    tracing::warn!(
                        "invalid {ENV_DENY_SILENCE_SECS}={s:?}; using default {}",
                        DEFAULT_MIN_DENY_INTERVAL_SECONDS
                    );
                    None
                })
            })
            .unwrap_or(DEFAULT_MIN_DENY_INTERVAL_SECONDS);

        Self {
            slice_read_max_limit,
            slice_read_threshold,
            read_threshold,
            grep_threshold,
            deny_text_search,
            min_deny_interval_seconds,
        }
    }
}

/// Parse true/false env tokens: true/1/yes/on vs false/0/no/off.
/// Unrecognised values → None (caller keeps default / warns).
pub(crate) fn parse_env_bool(raw: Option<&str>) -> Option<bool> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty())?;
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            tracing::warn!(value = %other, "invalid bool env token; ignoring");
            None
        }
    }
}

/// Resolve `deny_text_search` with precedence: CLI > env > context YAML > false.
pub(crate) fn resolve_deny_text_search(
    cli: Option<bool>,
    env_raw: Option<&str>,
    context_flag: Option<bool>,
) -> bool {
    if let Some(v) = cli {
        return v;
    }
    if let Some(v) = parse_env_bool(env_raw) {
        return v;
    }
    context_flag.unwrap_or(false)
}

/// 全局配置单例（进程生命周期内只初始化一次）
static CONFIG: OnceLock<HooksConfig> = OnceLock::new();

/// 获取当前配置
pub(crate) fn config() -> &'static HooksConfig {
    CONFIG.get_or_init(HooksConfig::from_env)
}

/// 非符号工具子串：Astrolabe 工具名包含这些子串时不重置 burst 计数
pub(crate) const NON_SYMBOLIC_ASTROLABE_SUBSTRINGS: &[&str] =
    &["search_code", "diagnostics", "languages"];

/// 非 Claude-Code 客户端判定 read_file 的动作动词子串
pub(crate) const READ_FILE_VERB_SUBSTRINGS: &[&str] = &["read", "view", "open", "show"];

/// Codex / Grok 下的 grep 类 shell 命令
pub(crate) const GREP_SHELL_COMMANDS: &[&str] = &[
    "grep",
    "rg",
    "ag",
    "ack",
    "fgrep",
    "egrep",
    "zgrep",
    "findstr",
    "select-string",
    "search_for_pattern",
];

/// Codex / Grok 下的 read 类 shell 命令
pub(crate) const READ_SHELL_COMMANDS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "sed",
    "less",
    "more",
    "bat",
    "get-content",
    "gc",
    // Windows cmd 的 `type file`
    "type",
];

/// 仅 Astrolabe 支持语言的源码扩展名（hook 拦截口径）。
/// hook 的目的是引导用 Astrolabe，对 Astrolabe 不能索引的语言不该拦截；
/// 与 core types.rs 的 Language::from_path 保持同步（新语言落地时此表必须同步加）。
/// 未支持语言统计词表见 astrolabe_core::index::KNOWN_CODE_EXTENSIONS。
pub(crate) const SUPPORTED_CODE_EXTENSIONS: &[&str] = &[
    "py",
    "pyi",
    "go",
    "java",
    "rs",
    "ts",
    "mts",
    "cts",
    "tsx",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "c",
    "h",
    "cpp",
    "cc",
    "cxx",
    "hpp",
    "hh",
    "hxx",
    "ipp",
    "m",
    "mm",
    "swift",
    "php",
    "vue",
    "dart",
    "cs",
    "cshtml",
    "csx",
    "razor",
    "vb",
    "kt",
    "kts",
    "sh",
    "bash",
    "zsh",
    "zsh-theme",
    "bats",
    "rb",
    "rake",
    "gemspec",
    "lua",
];

/// 触发 hook 的客户端类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Client {
    ClaudeCode,
    Codebuddy,
    Vscode,
    Codex,
    Grok,
    /// Moonshot Kimi Code CLI（协议与 Claude Code 同构）
    KimiCode,
    /// 智谱 Z.ai ZCode（协议与 Claude Code 同构）
    ZCode,
    /// Cursor CLI/Agent（含原生 hooks，扁平 deny 输出）
    Cursor,
    /// OpenCode（经 TS 插件桥接调用）
    OpenCode,
    Other,
}

impl Client {
    pub(crate) fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude_code" | "claudecode" => Client::ClaudeCode,
            "codebuddy" => Client::Codebuddy,
            "vscode" => Client::Vscode,
            "codex" => Client::Codex,
            "grok" | "grokbuild" | "grok-build" => Client::Grok,
            "kimicode" | "kimi-code" | "kimi_code" | "kimi" => Client::KimiCode,
            "zcode" | "z-code" | "zai" => Client::ZCode,
            "cursor" | "cursor-agent" | "cursor_agent" | "cursor-cli" => Client::Cursor,
            "opencode" => Client::OpenCode,
            _ => Client::Other,
        }
    }
}

/// 计数状态持久化结构体（保存在 ~/.astrolabe/hook_data/<session_id>/counter.json）
/// `#[serde(default)]` 兼容旧版 counter.json（缺 n_slice/last_slice_ts 字段时取默认值）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct CounterState {
    pub(crate) n_grep: u32,
    pub(crate) n_read: u32,
    pub(crate) n_non_symbolic: u32,
    pub(crate) n_slice: u32,
    pub(crate) last_grep_ts: Option<f64>,
    pub(crate) last_read_ts: Option<f64>,
    pub(crate) last_non_symbolic_ts: Option<f64>,
    pub(crate) last_slice_ts: Option<f64>,
    pub(crate) last_deny_ts: Option<f64>,
}

impl CounterState {
    /// 清零连续突发计数（保留 last_deny_ts 以维持静默窗口）
    pub(crate) fn reset_burst(&mut self) {
        self.n_grep = 0;
        self.n_read = 0;
        self.n_non_symbolic = 0;
        self.n_slice = 0;
        self.last_grep_ts = None;
        self.last_read_ts = None;
        self.last_non_symbolic_ts = None;
        self.last_slice_ts = None;
    }

    /// 判定当前是否处于静默窗口之外（可正常触发 hook）
    pub(crate) fn is_hook_active(&self, now: f64) -> bool {
        match self.last_deny_ts {
            None => true,
            Some(ts) => (now - ts) >= config().min_deny_interval_seconds,
        }
    }
}

/// 工具分类结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    /// Astrolabe 符号工具（find_symbol, find_references 等，重置计数）
    AstrolabeSymbolic,
    /// Astrolabe 非符号工具（search_code / diagnostics / instructions 等）：
    /// 只清零切片读额度，不清零 grep/read/non_symbolic 计数
    AstrolabeNonSymbolic,
    /// grep 类工具（含 shell 文本搜索）
    Grep,
    /// Glob / file_search 类工具（仅 deny_text_search 开启时分类；关闭时保持 Neutral）
    Glob,
    /// read 类工具，附带是否为代码文件判定
    Read { is_code_file: bool },
    /// Read 工具切片读（显式 limit <= slice_read_max_limit）：计入连续切片额度
    SliceRead,
    /// 中立工具（Edit, Write, Bash 等非 grep/read 工具，不增不减计数；
    /// shell 切片打印 sed -n / head -n / tail -n 亦归此类）
    Neutral,
}

/// deny 类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DenyKind {
    Grep,
    /// Hard-deny host Glob（deny_text_search）
    Glob,
    /// Hard-deny Grep / shell 文本搜索（deny_text_search）
    TextSearch,
    Read,
    Mixed,
    /// 连续切片读超额度（期间无任何 astrolabe 工具调用）
    Slice,
}

/// 决策结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// 位于静默窗口内，直接放行无输出且不修改状态
    Silenced,
    /// 符号工具，计数清零并保存，无输出
    ResetSymbolic,
    /// 非符号 astrolabe 工具，切片读额度清零并保存，无输出
    ResetSlice,
    /// 中立工具，直接放行不写盘
    NeutralAllow,
    /// grep/read/slice 调用未超阈值，更新计数并保存，无输出
    Allow,
    /// 达到阈值 deny，计数清零并记录 last_deny_ts，输出对应 deny JSON
    Deny(DenyKind),
}

/// 获取秒级 UNIX 时间戳
fn current_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 读取用户主目录：优先 `HOME`，其次 Windows 的 `USERPROFILE`，
/// 最后 `HOMEDRIVE`+`HOMEPATH`（原生 Windows 宿主通常没有 HOME）。
fn get_user_home() -> Option<PathBuf> {
    home_from_lookup(|k| std::env::var(k).ok())
}

fn home_from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let nonempty = |key: &str| {
        get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    nonempty("HOME")
        .or_else(|| nonempty("USERPROFILE"))
        .or_else(|| match (nonempty("HOMEDRIVE"), nonempty("HOMEPATH")) {
            (Some(d), Some(p)) => Some(format!("{d}{p}")),
            _ => None,
        })
        .map(PathBuf::from)
}

/// 校验 session_id 可作为单一路径分量使用。
///
/// 拒绝空串、`/`、`\`、`..`，以及任何不在 `[A-Za-z0-9_-]` 内的字符。
pub(crate) fn sanitize_session_id(session_id: &str) -> Result<&str, &'static str> {
    if session_id.is_empty() {
        return Err("Session ID is empty");
    }
    if session_id == "/" || session_id == "\\" || session_id == ".." {
        return Err("Session ID is invalid");
    }
    if !session_id
        .chars()
        .all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-'))
    {
        return Err("Session ID contains invalid characters");
    }
    // Defense in depth: reject embedded ".." even though '.' is already disallowed.
    if session_id.contains("..") {
        return Err("Session ID is invalid");
    }
    Ok(session_id)
}

/// FNV-1a 64 位哈希：把任意 agent_id 收敛为固定长度、路径安全的十六进制分量
fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// session id 合法（`[A-Za-z0-9_-]`）则原样返回，否则哈希成 `s-<fnv1a64 十六进制>`，
/// 保证作为单一路径分量安全且同一 id 稳定映射到同一目录（remind 与 cleanup 一致）。
fn hashed_session_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if sanitize_session_id(trimmed).is_ok() {
        trimmed.to_string()
    } else {
        format!("s-{:016x}", fnv1a64(trimmed))
    }
}

/// 将 agent_id 收敛为单一安全路径分量。
///
/// trim 后非空、长度 ≤ 128、且能通过 [`sanitize_session_id`] 校验（`[A-Za-z0-9_-]`）
/// 时原样返回；否则返回 `ag-<fnv1a64 十六进制>`（防路径穿越，兼容带冒号/点/斜杠的 agent 名）。
fn sanitize_agent_component(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.is_empty() && trimmed.len() <= 128 && sanitize_session_id(trimmed).is_ok() {
        trimmed.to_string()
    } else {
        format!("ag-{:016x}", fnv1a64(trimmed))
    }
}

/// 构造会话持久化目录路径：`~/.astrolabe/hook_data/<session_id>`；
/// `agent_id` 为 Some 时追加子代理分片 `~/.astrolabe/hook_data/<session_id>/<agent分量>`。
///
/// `session_id` 必须通过 [`sanitize_session_id`]；失败时返回错误信息。
/// agent 分量由 [`sanitize_agent_component`] 收敛，永不失败。
fn get_hook_data_dir(
    home: &Path,
    session_id: &str,
    agent_id: Option<&str>,
) -> Result<PathBuf, &'static str> {
    let session_id = sanitize_session_id(session_id)?;
    let dir = home.join(".astrolabe").join("hook_data").join(session_id);
    Ok(match agent_id {
        Some(agent) => dir.join(sanitize_agent_component(agent)),
        None => dir,
    })
}

/// 从磁盘读取 CounterState
pub(crate) fn load_counter(path: &Path) -> CounterState {
    if let Ok(file) = std::fs::File::open(path) {
        let reader = std::io::BufReader::new(file);
        if let Ok(counter) = serde_json::from_reader(reader) {
            return counter;
        }
    }
    CounterState::default()
}

/// 写回 CounterState 到磁盘（先写 `.tmp` 再 rename，保证原子替换）
pub(crate) fn save_counter(path: &Path, counter: &CounterState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = {
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    };
    let write_ok = (|| -> std::io::Result<()> {
        let file = std::fs::File::create(&tmp_path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer(writer, counter).map_err(std::io::Error::other)?;
        Ok(())
    })();
    match write_ok {
        Ok(()) => {
            if std::fs::rename(&tmp_path, path).is_err() {
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

/// 独占锁：与 `counter.json` 并列的 `counter.json.lock`，用操作系统文件锁（flock / LockFileEx）。
///
/// 进程被 kill/断电时内核自动释放锁，不会留下"陈旧锁"；锁文件本身不删除（删除会引入竞态）。
struct CounterFileLock {
    _file: std::fs::File,
}

impl CounterFileLock {
    /// 在 counter 路径旁加独占锁；短暂重试后仍失败则返回 None（降级无锁）。
    fn acquire(counter_path: &Path) -> Option<Self> {
        if let Some(parent) = counter_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut lock_path = counter_path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .ok()?;
        for _ in 0..200 {
            match file.try_lock() {
                Ok(()) => return Some(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // 文件系统不支持锁等：降级无锁，不要空等
                Err(std::fs::TryLockError::Error(_)) => return None,
            }
        }
        None
    }
}

/// 在文件锁保护下执行 load → 回调 →（可选）save。
///
/// 若无法获取锁，仍降级执行一次（避免 hook 永久卡住），但不保证互斥。
fn with_counter_locked<F, R>(path: &Path, f: F) -> R
where
    F: FnOnce(CounterState) -> (CounterState, bool, R),
{
    let _guard = CounterFileLock::acquire(path);
    let counter = load_counter(path);
    let (counter, should_save, result) = f(counter);
    if should_save {
        save_counter(path, &counter);
    }
    result
}

/// `command` 字段：字符串原样；字符串数组按 argv 拼接（含空白的元素加单引号）
fn command_value_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) if !a.is_empty() => {
            let parts: Option<Vec<String>> = a
                .iter()
                .map(|x| {
                    x.as_str().map(|t| {
                        if t.is_empty() || t.contains(char::is_whitespace) {
                            format!("'{}'", t.replace('\'', "'\\''"))
                        } else {
                            t.to_string()
                        }
                    })
                })
                .collect();
            parts.map(|p| p.join(" "))
        }
        _ => None,
    }
}

/// 本服务固定目录里的 7 个工具名
const ASTROLABE_CATALOG_TOOLS: &[&str] = &[
    "resolve_context",
    "search_code",
    "find_symbol",
    "find_references",
    "goto_definition",
    "get_diagnostics",
    "get_languages",
];

/// 工具名是否属于 Astrolabe：含 "astrolabe" 且含固定目录里的某个工具名。
/// 第三方 `mcp__astrolabe_docs__fetch` 之类仅名字撞上的工具不算。
pub(crate) fn is_astrolabe_tool(tool_name: &str) -> bool {
    tool_name.contains("astrolabe")
        && ASTROLABE_CATALOG_TOOLS
            .iter()
            .any(|t| tool_name.contains(t))
}

/// 判定工具名是否为 Astrolabe 符号工具
///
/// tool_name 含 "astrolabe" 且不含子串
/// {pattern, search_code, diagnostics, languages, instructions}
pub(crate) fn is_astrolabe_symbolic_tool(tool_name: &str) -> bool {
    is_astrolabe_tool(tool_name)
        && !NON_SYMBOLIC_ASTROLABE_SUBSTRINGS
            .iter()
            .any(|sub| tool_name.contains(sub))
}

/// 判定路径是否为代码文件
pub(crate) fn is_code_file_path(path: &str) -> bool {
    let cleaned = path.trim().trim_matches(|c| c == '\'' || c == '"');
    if cleaned.is_empty() {
        return false;
    }
    if let Some(ext) = Path::new(cleaned).extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        SUPPORTED_CODE_EXTENSIONS.contains(&ext_lower.as_str())
    } else {
        false
    }
}

/// 从 shell 命令行参数中提取非选项路径参数
pub(crate) fn iter_shell_path_arguments(args_str: &str) -> Vec<String> {
    args_str
        .split_whitespace()
        .filter_map(|raw| {
            let cleaned = raw.trim().trim_matches(|c| c == '\'' || c == '"');
            if cleaned.is_empty() || cleaned.starts_with('-') {
                None
            } else {
                Some(cleaned.to_string())
            }
        })
        .collect()
}

/// 判定 shell 命令行参数是否包含写重定向（>、>>、<<、<<<、>file、>>file 等）
///
/// 注意：< 与 <file 为输入重定向（属于读），不视为写重定向；-> 为文本箭头，不视为重定向。
fn has_write_redirection(args_str: &str) -> bool {
    args_str
        .split_whitespace()
        .any(|token| token.starts_with('>') || token.starts_with("<<"))
}

/// 判定 sed 命令参数是否包含原地写参数（-i、-i''、-i.bak、--in-place 等）
fn is_sed_in_place(args_str: &str) -> bool {
    args_str.split_whitespace().any(|token| {
        let cleaned = token.trim_matches(|c| c == '\'' || c == '"');
        cleaned.starts_with("-i") || cleaned.starts_with("--in-place")
    })
}

/// 从 JSON 值提取非空 id 字符串（String trim 非空或 Number→字符串）。
/// 用于 session/conversation/agent 标识的跨键 fallback 解析：按"第一个合法值"而非
/// "第一个存在的键"取值，null/空白占位键会被跳过继续找后续键。
fn json_nonempty_id(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 从 tool_input JSON 字段提取 u64（接受 Number 或可解析为数字的 String）
fn json_value_as_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 解析纯数字字符串为 u64（拒绝 `+N`/`-N`/混入其它字符的形式：
/// 如 tail -n +5 表示从第 5 行打印到 EOF，不是有界切片）
fn parse_digits(s: &str) -> Option<u64> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse::<u64>().ok()
    } else {
        None
    }
}

/// 解析 head/tail 参数中的行数：-n N、-nN、--lines=N（只接受有界行数，
/// 不接受 tail 的 `+N` 起始行形式——那会打印到 EOF，不是有界切片）
fn parse_head_tail_line_count(args_str: &str) -> Option<u64> {
    let tokens: Vec<&str> = args_str.split_whitespace().collect();
    for (i, raw) in tokens.iter().enumerate() {
        let cleaned = raw.trim_matches(|c| c == '\'' || c == '"');
        if cleaned == "-n" || cleaned == "--lines" {
            if let Some(next) = tokens.get(i + 1) {
                let n = next.trim_matches(|c| c == '\'' || c == '"');
                if let Some(v) = parse_digits(n) {
                    return Some(v);
                }
            }
        } else if let Some(rest) = cleaned.strip_prefix("--lines=") {
            if let Some(v) = parse_digits(rest) {
                return Some(v);
            }
        } else if cleaned.len() > 2 && cleaned.starts_with("-n") {
            if let Some(v) = parse_digits(&cleaned[2..]) {
                return Some(v);
            }
        }
    }
    None
}

/// 判定单个 sed 脚本 token 是否为"纯数字地址 + p 打印"命令（如 300,340p / 50p / 50p;80p）
///
/// 检查 sed 脚本是否为受限的切片打印 token（如 `'300,340p'`、`'50p'` 或 `'50p;80p'`）。
/// 仅放行纯数字单行或跨度 <= slice_read_max_limit（默认 200）的明确行号区间；
/// 拒绝 `$p`、`/re/p`、`1,$p` 以及超限大范围倾倒。
fn is_sed_print_script_token(cleaned: &str) -> bool {
    let mut total: u64 = 0;
    for part in cleaned.split(';') {
        match sed_print_part_lines(part.trim()) {
            Some(n) => total = total.saturating_add(n),
            None => return false,
        }
    }
    // 多段合计行数也不得超过切片上限，防止 `1,200p;201,400p;...` 拼接绕过额度
    total <= config().slice_read_max_limit
}

/// 单段 `Np` / `A,Bp` 的行数；非纯数字打印返回 None；区间超上限返回 None。
fn sed_print_part_lines(s_raw: &str) -> Option<u64> {
    let s = s_raw.strip_suffix('p')?;
    if s.is_empty() {
        return None;
    }
    // 单行形式：如 "50p"
    if parse_digits(s).is_some() {
        return Some(1);
    }
    // 区间形式：如 "300,340p"
    if let Some((start_str, end_str)) = s.split_once(',') {
        if let (Some(start), Some(end)) = (parse_digits(start_str), parse_digits(end_str)) {
            if end >= start && (end - start + 1) <= config().slice_read_max_limit {
                return Some(end - start + 1);
            }
        }
    }
    None
}

/// 是否为 sed 的静默 flag：`-n`、`--quiet`、`--silent`，或合并短 flag（如 `-ne`、`-nE`）。
fn is_sed_quiet_flag(cleaned: &str) -> bool {
    if cleaned == "--quiet" || cleaned == "--silent" {
        return true;
    }
    cleaned.len() >= 2
        && cleaned.starts_with('-')
        && !cleaned.starts_with("--")
        && cleaned[1..].chars().all(|c| c.is_ascii_alphabetic())
        && cleaned[1..].contains('n')
        && !cleaned[1..].contains('i')
}

/// 判定 sed 参数是否为切片打印：带 -n（--quiet/--silent）静默且脚本含 p 打印命令，
/// 且不含 -i 原地写（如 `sed -n '300,340p' src/a.rs`）
fn is_sed_slice_print(args_str: &str) -> bool {
    if args_str.trim().is_empty() || is_sed_in_place(args_str) {
        return false;
    }
    let mut has_quiet = false;
    let mut has_print_script = false;
    for raw in args_str.split_whitespace() {
        let cleaned = raw.trim_matches(|c| c == '\'' || c == '"');
        if is_sed_quiet_flag(cleaned) {
            has_quiet = true;
        } else if is_sed_print_script_token(cleaned) {
            has_print_script = true;
        }
    }
    has_quiet && has_print_script
}

/// 判定调用是否为局部切片阅读（slice read）
///
/// - 直接 read 类工具（tool_name 为 "read" 或含 "read_file"）：
///   - 必须显式带 limit 且 limit <= slice_read_max_limit（默认 200）→ 切片读；
///     Read 工具切片计入连续切片额度（见 `DEFAULT_SLICE_READ_THRESHOLD`），
///     连续超阈值且期间无任何 astrolabe 工具调用触发 Slice deny；
///   - 仅有 offset、无合法 limit 时不算切片精读；
/// - shell 命令（保持中立不计数，避免 shell 场景复杂化）：
///   - `sed -n 'Np'` / `sed -n 'A,Bp'`（纯数字地址，不含 -i）→ 切片打印；
///   - `head -n N` / `tail -n N` 当 N <= slice_read_max_limit（默认 200）→ 切片打印。
pub(crate) fn is_slice_read(
    tool_name: &str,
    command_name: Option<&str>,
    command_args_str: Option<&str>,
    limit: Option<u64>,
    _offset: Option<u64>,
) -> bool {
    let lower_name = tool_name.to_ascii_lowercase();
    let max_limit = config().slice_read_max_limit;

    // 1. 直接 read 类工具：仅按 limit 判定（offset 忽略）
    if lower_name == "read" || lower_name.contains("read_file") {
        // 切片阅读放行规则：必须显式带 limit 且 limit <= max_limit；
        // 不带 limit（即使有 offset）在 Claude Code 中默认会读 2000 行，不属于小范围切片。
        if let Some(l) = limit {
            if l <= max_limit {
                return true;
            }
        }
        return false;
    }

    // 2. shell 命令切片打印
    if let Some(cmd) = command_name {
        let args = command_args_str.unwrap_or("");
        match cmd {
            "sed" => return is_sed_slice_print(args),
            "head" | "tail" => {
                return parse_head_tail_line_count(args)
                    .map(|n| n <= max_limit)
                    .unwrap_or(false);
            }
            _ => {}
        }
    }

    false
}

/// 判定 read 工具调用是否针对代码文件
pub(crate) fn is_read_code_file_call(
    is_read: bool,
    file_path: Option<&str>,
    client: Client,
    command_args_str: Option<&str>,
) -> bool {
    if !is_read {
        return false;
    }

    if let Some(args_str) = command_args_str {
        if has_write_redirection(args_str) || is_sed_in_place(args_str) {
            return false;
        }
    }

    if let Some(fp) = file_path {
        return is_code_file_path(fp);
    }

    if matches!(
        client,
        Client::Codex
            | Client::Grok
            | Client::ClaudeCode
            | Client::Codebuddy
            | Client::KimiCode
            | Client::ZCode
            | Client::Cursor
            | Client::OpenCode
    ) {
        if let Some(args_str) = command_args_str {
            let args = iter_shell_path_arguments(args_str);
            return args.iter().any(|arg| is_code_file_path(arg));
        }
    }

    // 保守处理：其它客户端无明确路径信息时视为代码文件
    true
}

/// shell 命令里的一个"命令位置"调用：规范化后的命令名与其参数原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellInvocation {
    /// 小写、去目录、去 `.exe`/`.cmd`/`.bat`/`.com` 后缀的命令名
    pub(crate) command: String,
    /// 命令名之后到本段结束的原文（保留引号，不含 `;`/`&&`/`|` 之后的内容）
    pub(crate) args: String,
    /// 该调用是否处于管道后续位置（`a | grep x` 里的 grep）
    pub(crate) after_pipe: bool,
}

struct RawToken {
    text: String,
    end: usize,
}

struct RawSegment {
    tokens: Vec<RawToken>,
    raw: String,
    raw_start: usize,
    after_pipe: bool,
}

/// 按 shell 规则分词并按 `&& || ; | & 换行` 切段。引号内的运算符不切分；
/// 反斜杠只在后接空白/引号/反斜杠时当转义（Windows 路径保持字面）。
fn split_shell_segments(cmd: &str) -> Vec<RawSegment> {
    let chars: Vec<(usize, char)> = cmd.char_indices().collect();
    let mut segs: Vec<RawSegment> = Vec::new();
    let mut tokens: Vec<RawToken> = Vec::new();
    let mut cur = String::new();
    let mut tok_start: Option<usize> = None;
    let mut seg_start = 0usize;
    let mut seg_after_pipe = false;
    let mut quote: Option<char> = None;
    let mut i = 0;

    macro_rules! end_token {
        ($end:expr) => {
            if tok_start.take().is_some() {
                tokens.push(RawToken {
                    text: std::mem::take(&mut cur),
                    end: $end,
                });
            }
        };
    }
    macro_rules! end_segment {
        ($end:expr, $next_pipe:expr) => {
            end_token!($end);
            if !tokens.is_empty() {
                segs.push(RawSegment {
                    tokens: std::mem::take(&mut tokens),
                    raw: cmd[seg_start..$end].to_string(),
                    raw_start: seg_start,
                    after_pipe: seg_after_pipe,
                });
            }
            tokens.clear();
            seg_after_pipe = $next_pipe;
        };
    }

    while i < chars.len() {
        let (pos, c) = chars[i];
        let next = chars.get(i + 1).map(|(_, n)| *n);
        let next_pos = chars.get(i + 1).map(|(p, _)| *p).unwrap_or(cmd.len());
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' && matches!(next, Some('"') | Some('\\')) {
                cur.push(next.unwrap());
                i += 1;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                if tok_start.is_none() {
                    tok_start = Some(pos);
                }
                quote = Some(c);
            }
            '\\' if matches!(next, Some(n) if n.is_whitespace() || n == '"' || n == '\'' || n == '\\') =>
            {
                if tok_start.is_none() {
                    tok_start = Some(pos);
                }
                cur.push(next.unwrap());
                i += 1;
            }
            c if c.is_whitespace() && c != '\n' => {
                end_token!(pos);
            }
            '\n' | ';' => {
                end_segment!(pos, false);
                seg_start = next_pos;
            }
            '|' => {
                let double = next == Some('|');
                end_segment!(pos, !double);
                if double {
                    i += 1;
                }
                seg_start = chars.get(i + 1).map(|(p, _)| *p).unwrap_or(cmd.len());
            }
            '&' => {
                // `>&2`、`2>&1`、`&>file` 里的 & 属于重定向，不是命令分隔符
                let prev = if pos > 0 {
                    cmd[..pos].chars().last()
                } else {
                    None
                };
                if matches!(prev, Some('>') | Some('<')) || next == Some('>') {
                    if tok_start.is_none() {
                        tok_start = Some(pos);
                    }
                    cur.push(c);
                } else {
                    end_segment!(pos, false);
                    if next == Some('&') {
                        i += 1;
                    }
                    seg_start = chars.get(i + 1).map(|(p, _)| *p).unwrap_or(cmd.len());
                }
            }
            _ => {
                if tok_start.is_none() {
                    tok_start = Some(pos);
                }
                cur.push(c);
            }
        }
        i += 1;
    }
    end_token!(cmd.len());
    if !tokens.is_empty() {
        segs.push(RawSegment {
            tokens,
            raw: cmd[seg_start..].to_string(),
            raw_start: seg_start,
            after_pipe: seg_after_pipe,
        });
    }
    segs
}

fn normalize_command_name(tok: &str) -> String {
    let base = tok
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(tok)
        .to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stripped) = base.strip_suffix(ext) {
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    base
}

fn is_env_assignment(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

const SHELL_WRAPPERS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "fish"];
const POWERSHELL_WRAPPERS: &[&str] = &["powershell", "pwsh"];
const TRANSPARENT_PREFIXES: &[&str] = &[
    "sudo", "doas", "env", "time", "nohup", "nice", "command", "exec", "xargs", "stdbuf", "builtin",
];

fn collect_invocations(cmd: &str, depth: usize, out: &mut Vec<ShellInvocation>) {
    for seg in split_shell_segments(cmd) {
        let toks = &seg.tokens;
        let mut i = 0;
        // 跳过前缀：环境变量赋值、sudo/env/time/xargs 及其选项
        while let Some(t) = toks.get(i) {
            if is_env_assignment(&t.text) {
                i += 1;
                continue;
            }
            let name = normalize_command_name(&t.text);
            if TRANSPARENT_PREFIXES.contains(&name.as_str()) {
                i += 1;
                while let Some(o) = toks.get(i) {
                    if o.text.starts_with('-') && o.text.len() > 1 {
                        // sudo -u USER / -g GROUP 等带值选项：多吞一个值
                        let takes_value = matches!(name.as_str(), "sudo" | "doas")
                            && matches!(
                                o.text.as_str(),
                                "-u" | "-g" | "-h" | "-p" | "-C" | "-D" | "-r" | "-t" | "-U"
                            );
                        i += 1;
                        if takes_value {
                            i += 1;
                        }
                    } else if name == "env" && is_env_assignment(&o.text) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            break;
        }
        let Some(cmd_tok) = toks.get(i) else { continue };
        let name = normalize_command_name(&cmd_tok.text);
        let args = seg.raw[cmd_tok.end - seg.raw_start..].trim().to_string();
        out.push(ShellInvocation {
            command: name.clone(),
            args,
            after_pipe: seg.after_pipe,
        });
        if depth < 3 {
            // bash -c "grep foo ." / sh -lc '...'
            if SHELL_WRAPPERS.contains(&name.as_str()) {
                let mut j = i + 1;
                while let Some(o) = toks.get(j) {
                    let t = o.text.as_str();
                    let is_c_flag = t.starts_with('-')
                        && !t.starts_with("--")
                        && t.len() >= 2
                        && t[1..].chars().all(|c| c.is_ascii_alphabetic())
                        && t.contains('c');
                    if is_c_flag {
                        if let Some(script) = toks.get(j + 1) {
                            collect_invocations(&script.text, depth + 1, out);
                        }
                        break;
                    }
                    if !t.starts_with('-') {
                        break;
                    }
                    j += 1;
                }
            } else if POWERSHELL_WRAPPERS.contains(&name.as_str()) || name == "cmd" {
                let mut j = i + 1;
                while let Some(o) = toks.get(j) {
                    let lower = o.text.to_ascii_lowercase();
                    if matches!(lower.as_str(), "-c" | "-command" | "/c" | "/k") {
                        let rest: Vec<&str> =
                            toks[j + 1..].iter().map(|t| t.text.as_str()).collect();
                        collect_invocations(&rest.join(" "), depth + 1, out);
                        break;
                    }
                    j += 1;
                }
            }
        }
    }
}

/// 把命令串拆成各"命令位置"的调用（跳过 env 赋值/sudo/time/xargs 前缀，递归进入 `bash -c`）。
pub(crate) fn shell_invocations(cmd: &str) -> Vec<ShellInvocation> {
    let mut out = Vec::new();
    collect_invocations(cmd, 0, &mut out);
    out
}

/// 文本搜索命令名（硬模式拦截 / 软模式计数共用口径）。
pub(crate) fn is_text_search_command(name: &str) -> bool {
    matches!(
        name,
        "grep" | "egrep" | "fgrep" | "zgrep" | "rg" | "ag" | "ack" | "findstr" | "select-string"
    )
}

/// `git [opts…] grep …`
fn is_git_grep(inv: &ShellInvocation) -> bool {
    if inv.command != "git" {
        return false;
    }
    let toks = split_shell_segments(&inv.args)
        .into_iter()
        .next()
        .map(|s| s.tokens)
        .unwrap_or_default();
    let mut it = toks.into_iter();
    while let Some(t) = it.next() {
        if t.text.starts_with('-') {
            // `git -C dir grep` / `-c k=v` / `--git-dir X` 等带值全局选项
            if matches!(
                t.text.as_str(),
                "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                it.next();
            }
            continue;
        }
        return t.text == "grep";
    }
    false
}

/// 为软模式选出代表命令：第一个文本搜索调用，其次第一个读类调用，否则第一个调用。
/// 管道后续段（`cat a | grep x` 的 grep）不参与，保持"管道只在硬模式拦"的既有语义。
pub(crate) fn effective_shell_command(cmd: &str) -> Option<(String, Option<String>)> {
    let invs = shell_invocations(cmd);
    let pick = invs
        .iter()
        .filter(|i| !i.after_pipe)
        .find(|i| GREP_SHELL_COMMANDS.contains(&i.command.as_str()))
        .or_else(|| {
            invs.iter()
                .filter(|i| !i.after_pipe)
                .find(|i| READ_SHELL_COMMANDS.contains(&i.command.as_str()))
        })
        .or_else(|| invs.first())?;
    let args = if pick.args.is_empty() {
        None
    } else {
        Some(pick.args.clone())
    };
    Some((pick.command.clone(), args))
}

/// 硬模式文本搜索检测：只看各"命令位置"的命令名（含 `bash -c`、`sudo`、`.exe`、`git grep`），
/// 不对整串做子串匹配，因此 `echo grep is great` / `git commit -m "fix grep bug"` 不误伤，
/// 路径 `src/grep_utils.rs` 也不会命中。
pub(crate) fn command_contains_text_search(cmd: &str) -> bool {
    shell_invocations(cmd)
        .iter()
        .any(|i| is_text_search_command(&i.command) || is_git_grep(i))
}

pub(crate) fn is_glob_tool_name(tool_name: &str) -> bool {
    let n = tool_name.trim().to_ascii_lowercase();
    n == "glob" || n == "file_search" || n == "filesearch"
}

/// 工具分类函数
///
/// `limit` / `offset` 来自 tool_input 的切片参数（数字或数字字符串）。
/// 当前切片精读判定只使用 `limit`（须存在且 <= slice_read_max_limit，默认 200）；`offset` 保留传入以兼容调用方。
/// `deny_text_search`：开启时把 Glob 与 shell 文本搜索（含管道/`git grep`）纳入拦截。
/// Soft-mode 包装（`deny_text_search=false`）；生产路径走 [`classify_tool_ex`]。
#[cfg(test)]
pub(crate) fn classify_tool(
    tool_name: &str,
    client: Client,
    file_path: Option<&str>,
    command_name: Option<&str>,
    command_args_str: Option<&str>,
    limit: Option<u64>,
    offset: Option<u64>,
) -> ToolKind {
    classify_tool_ex(
        tool_name,
        client,
        file_path,
        command_name,
        command_args_str,
        None,
        limit,
        offset,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_tool_ex(
    tool_name: &str,
    client: Client,
    file_path: Option<&str>,
    command_name: Option<&str>,
    command_args_str: Option<&str>,
    full_command: Option<&str>,
    limit: Option<u64>,
    offset: Option<u64>,
    deny_text_search: bool,
) -> ToolKind {
    // 1. 先检查是否为 Astrolabe 符号工具
    if is_astrolabe_symbolic_tool(tool_name) {
        return ToolKind::AstrolabeSymbolic;
    }

    // 1.5 非符号 astrolabe 工具（search_code / diagnostics / instructions 等）：
    // 只清零切片读额度（decide 中处理），不清零 grep/read/non_symbolic 计数
    if is_astrolabe_tool(tool_name) {
        return ToolKind::AstrolabeNonSymbolic;
    }

    // 1.7 Hard text-search mode: Glob + shell token scan (pipelines / git grep)
    if deny_text_search {
        if is_glob_tool_name(tool_name) {
            return ToolKind::Glob;
        }
        if let Some(cmd) = full_command {
            if command_contains_text_search(cmd) {
                return ToolKind::Grep;
            }
        } else if let Some(cmd) = command_name {
            let reconstructed = match command_args_str {
                Some(args) => format!("{cmd} {args}"),
                None => cmd.to_string(),
            };
            if command_contains_text_search(&reconstructed) {
                return ToolKind::Grep;
            }
        }
    }

    // 2. 检查是否为 grep 类工具
    // KimiCode / ZCode / Cursor / OpenCode 协议与 CC 同构：工具名 read/grep/search_for_pattern
    // （lowercase 后命中），命令型工具的 tool_input 带 command 字段（cmd/command 提取已覆盖）；
    // Cursor 的命令工具名是 "Shell"，靠 command 内容识别 grep/cat 等，无需特判工具名。
    let is_grep = match client {
        Client::ClaudeCode
        | Client::Codebuddy
        | Client::KimiCode
        | Client::ZCode
        | Client::Cursor
        | Client::OpenCode => {
            tool_name == "grep"
                || tool_name.contains("search_for_pattern")
                || command_name
                    .map(|cmd| GREP_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Grok => {
            tool_name == "grep"
                || command_name
                    .map(|cmd| GREP_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Codex => command_name
            .map(|cmd| GREP_SHELL_COMMANDS.contains(&cmd))
            .unwrap_or(false),
        Client::Vscode | Client::Other => tool_name.contains("grep"),
    };

    if is_grep {
        return ToolKind::Grep;
    }

    // 3. 检查是否为 read 类工具
    let is_read = match client {
        Client::ClaudeCode
        | Client::Codebuddy
        | Client::KimiCode
        | Client::ZCode
        | Client::Cursor
        | Client::OpenCode => {
            tool_name == "read"
                || tool_name.contains("read_file")
                || command_name
                    .map(|cmd| READ_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Grok => {
            tool_name == "read_file"
                || command_name
                    .map(|cmd| READ_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Codex => command_name
            .map(|cmd| READ_SHELL_COMMANDS.contains(&cmd))
            .unwrap_or(false),
        Client::Vscode | Client::Other => {
            tool_name.contains("file")
                && READ_FILE_VERB_SUBSTRINGS
                    .iter()
                    .any(|verb| tool_name.contains(verb))
        }
    };

    if is_read {
        // 切片精读（slice read）：Read 工具带合法 limit 的局部读取计入连续切片额度；
        // 非代码文件（未索引语言/markdown 等）的切片保持中立——与全量读口径一致
        // （Read{is_code_file:false} 不计 n_read），把它们推向 search_code 没有意义；
        // shell 切片打印（head -n / tail -n / sed -n）同样保持中立，不计任何计数
        if is_slice_read(tool_name, command_name, command_args_str, limit, offset) {
            let lower_name = tool_name.to_ascii_lowercase();
            let is_read_tool_slice = lower_name == "read" || lower_name.contains("read_file");
            if is_read_tool_slice {
                let is_code = is_read_code_file_call(true, file_path, client, command_args_str);
                return if is_code {
                    ToolKind::SliceRead
                } else {
                    ToolKind::Neutral
                };
            }
            return ToolKind::Neutral;
        }

        let is_write_redirection = command_args_str.map(has_write_redirection).unwrap_or(false);
        let is_sed_write =
            command_name == Some("sed") && command_args_str.map(is_sed_in_place).unwrap_or(false);

        if is_write_redirection || is_sed_write {
            return ToolKind::Neutral;
        }

        let is_code = is_read_code_file_call(true, file_path, client, command_args_str);
        return ToolKind::Read {
            is_code_file: is_code,
        };
    }

    ToolKind::Neutral
}

/// 纯逻辑决策函数：按输入状态与工具类型推进计数并生成决策（用全局 config）。
/// 生产路径走 [`decide_with`]；本包装供单测与兼容调用。
#[cfg(test)]
pub(crate) fn decide(counter: &mut CounterState, tool_kind: ToolKind, now: f64) -> Decision {
    decide_with(counter, tool_kind, now, config())
}

/// 可注入配置的决策函数（测试与 CLI/context 覆盖用）。
pub(crate) fn decide_with(
    counter: &mut CounterState,
    tool_kind: ToolKind,
    now: f64,
    cfg: &HooksConfig,
) -> Decision {
    // 0. 硬禁用模式：Grep/Glob 一律拦截，不受静默窗约束
    //    （否则首个 grep 被拦后 min_deny_interval 内的重试会全部放行，等于没禁用）
    if cfg.deny_text_search {
        match tool_kind {
            ToolKind::Grep => {
                counter.reset_burst();
                counter.last_deny_ts = Some(now);
                return Decision::Deny(DenyKind::TextSearch);
            }
            ToolKind::Glob => {
                counter.reset_burst();
                counter.last_deny_ts = Some(now);
                return Decision::Deny(DenyKind::Glob);
            }
            _ => {}
        }
    }

    // 1. 静默窗内 → no-op（allow，无输出更新）
    if !counter.is_hook_active(now) {
        return Decision::Silenced;
    }

    // 2. astrolabe 符号工具 → 重置 burst 计数（含切片额度），保留 last_deny_ts
    if tool_kind == ToolKind::AstrolabeSymbolic {
        counter.reset_burst();
        return Decision::ResetSymbolic;
    }

    // 2.5 非符号 astrolabe 工具 → 只清零切片读额度；
    // 不清零 n_grep/n_read/n_non_symbolic（维持既有符号重置语义不变）
    if tool_kind == ToolKind::AstrolabeNonSymbolic {
        counter.n_slice = 0;
        counter.last_slice_ts = None;
        return Decision::ResetSlice;
    }

    // 2.7 切片读（Read 带 limit<=max）→ 连续额度制：
    // 不并入 n_non_symbolic（保持 mixed 语义不变）、不计 read/grep；
    // 间隔超 SLICE_RESET_PERIOD_SECONDS 则重置为 1 否则 +1；
    // 连续达阈值且期间无任何 astrolabe 工具调用（额度未被清零）→ Slice deny
    if tool_kind == ToolKind::SliceRead {
        if let Some(last_ts) = counter.last_slice_ts {
            if (now - last_ts) <= SLICE_RESET_PERIOD_SECONDS {
                counter.n_slice += 1;
            } else {
                counter.n_slice = 1;
            }
        } else {
            counter.n_slice = 1;
        }
        counter.last_slice_ts = Some(now);

        if counter.n_slice >= cfg.slice_read_threshold {
            counter.reset_burst();
            counter.last_deny_ts = Some(now);
            return Decision::Deny(DenyKind::Slice);
        }
        return Decision::Allow;
    }

    // 3. 非追踪工具（既非 grep 类也非 read 类）→ 中立：不增不重置，直接 allow
    // Glob 在 soft 模式下不会出现（classify 仅 hard 时返回 Glob）；若漏网则放行。
    let (is_grep, is_code_file_read, is_read) = match tool_kind {
        ToolKind::Grep => (true, false, false),
        ToolKind::Read { is_code_file } => (false, is_code_file, true),
        ToolKind::Glob => return Decision::NeutralAllow,
        _ => return Decision::NeutralAllow,
    };

    // 4. grep 类/read 类 → 按间隔-period 规则更新对应计数（及 non_symbolic 计数）
    if is_grep {
        if let Some(last_ts) = counter.last_grep_ts {
            if (now - last_ts) <= GREP_RESET_PERIOD_SECONDS {
                counter.n_grep += 1;
            } else {
                counter.n_grep = 1;
            }
        } else {
            counter.n_grep = 1;
        }
        counter.last_grep_ts = Some(now);
    }

    if is_code_file_read {
        if let Some(last_ts) = counter.last_read_ts {
            if (now - last_ts) <= READ_RESET_PERIOD_SECONDS {
                counter.n_read += 1;
            } else {
                counter.n_read = 1;
            }
        } else {
            counter.n_read = 1;
        }
        counter.last_read_ts = Some(now);
    }

    if is_grep || is_read {
        if let Some(last_ts) = counter.last_non_symbolic_ts {
            if (now - last_ts) <= NON_SYMBOLIC_RESET_PERIOD_SECONDS {
                counter.n_non_symbolic += 1;
            } else {
                counter.n_non_symbolic = 1;
            }
        } else {
            counter.n_non_symbolic = 1;
        }
        counter.last_non_symbolic_ts = Some(now);
    }

    // 阈值判定（grep_threshold 可经 ASTROLABE_GREP_THRESHOLD 调整；0/1 ≈ hard deny for grep）
    let grep_threshold = cfg.grep_threshold.max(1);
    let too_many_greps = counter.n_grep >= grep_threshold;
    let too_many_reads = counter.n_read >= cfg.read_threshold;
    let too_many_non_symbolic = counter.n_non_symbolic >= NON_SYMBOLIC_THRESHOLD;

    // 依次判定：
    // 当前调用是 grep 类且 n_grep≥threshold → grep deny；
    // 当前是 read-代码文件 且 n_read≥threshold → read deny；
    // 否则 n_grep≥threshold → grep deny；
    // n_read≥threshold → read deny；
    // n_non_symbolic≥4 → mixed deny。
    let deny_kind = if is_grep && too_many_greps {
        Some(DenyKind::Grep)
    } else if is_code_file_read && too_many_reads {
        Some(DenyKind::Read)
    } else if too_many_greps {
        Some(DenyKind::Grep)
    } else if too_many_reads {
        Some(DenyKind::Read)
    } else if too_many_non_symbolic {
        Some(DenyKind::Mixed)
    } else {
        None
    };

    if let Some(kind) = deny_kind {
        // 触发即：burst 计数清零、记录 last_deny_ts
        counter.reset_burst();
        counter.last_deny_ts = Some(now);
        Decision::Deny(kind)
    } else {
        Decision::Allow
    }
}

/// 按 client 与 deny 种类构建单行 JSON 输出。
///
/// additionalContext 末尾统一带一句只读声明（实测：只读探索代理被拦时会把
/// 提醒误读为"与我只读任务冲突"而选择绕过——讲清楚 astrolabe 本身只读，
/// 就不是禁止探索而是给出更省 token 的探索方式）。
/// 文案口径：直接给出切换指令（Stop using ... Call ... now），不再出现
/// "You can continue using ... the counter was reset" 软口径。
pub(crate) fn build_output(client: Client, deny_kind: DenyKind) -> String {
    let max_limit = config().slice_read_max_limit;
    let slice_threshold = config().slice_read_threshold;
    let readonly_note = format!(" Note: all Astrolabe tools are read-only and safe for exploration tasks. Also note: slice reads with limit <= {max_limit} are still permitted, but {slice_threshold} consecutive ones without any Astrolabe tool call trigger a reminder (tunable via ASTROLABE_SLICE_READ_THRESHOLD).");
    let unindexed_lang_note = " If your target files are in a language Astrolabe does not index (see the get_languages tool), grep is the correct tool for them — ignore this reminder.";
    let switch_instruction =
        "Stop using grep/read for code discovery. Call mcp__astrolabe__search_code or mcp__astrolabe__find_symbol now (read-only, safe).";
    let (reason, ctx) = match deny_kind {
        DenyKind::Grep => (
            "Too many consecutive grep calls without using symbolic tools. Stop using grep for code discovery. Call mcp__astrolabe__search_code or mcp__astrolabe__find_symbol now (read-only, safe).",
            format!(
                "You were using many grep calls recently. Use Astrolabe's symbolic mcp tools instead for more code-centric search (search_code / find_references are read-only and return path:line anchors). {switch_instruction}{unindexed_lang_note}"
            ),
        ),
        DenyKind::TextSearch => (
            "Text search with Grep/grep/rg/ag/ack/git grep is disabled (ASTROLABE_DENY_TEXT_SEARCH). Use Astrolabe MCP tools instead: search_code, find_symbol, find_references, goto_definition. Reading known files with Read/cat, ls, and build/test commands are still allowed.",
            format!(
                "Host text-search tools (Grep, grep, rg, ag, ack, git grep, zgrep) are hard-disabled for this Astrolabe process. Call mcp__astrolabe__search_code (literal/regex, enclosing bodies), mcp__astrolabe__find_symbol, mcp__astrolabe__find_references, or mcp__astrolabe__goto_definition now (read-only, safe).{unindexed_lang_note}"
            ),
        ),
        DenyKind::Glob => (
            "Glob / file_search is disabled (ASTROLABE_DENY_TEXT_SEARCH). Use Astrolabe MCP tools instead: search_code, find_symbol, find_references, goto_definition.",
            "Host Glob / file_search is hard-disabled for this Astrolabe process. Prefer mcp__astrolabe__search_code or mcp__astrolabe__find_symbol to locate symbols and paths (read-only, safe).".to_string(),
        ),
        DenyKind::Read => (
            "Too many consecutive read calls of files without using symbolic tools. Stop using read for code discovery. Call mcp__astrolabe__search_code or mcp__astrolabe__find_symbol now (read-only, safe).",
            format!(
                "You were using many read calls on files recently. Use Astrolabe's symbolic mcp tools instead for more targeted reads (read-only exploration included: find_symbol / search_code return exact bodies and path:line anchors without whole-file reads). {switch_instruction}"
            ),
        ),
        DenyKind::Mixed => (
            "Too many consecutive non-symbolic tool calls (mixed grep and read). Stop using grep/read for code discovery. Call mcp__astrolabe__search_code or mcp__astrolabe__find_symbol now (read-only, safe).",
            format!(
                "You were alternating between grep and read file calls recently without using Astrolabe's symbolic mcp tools. Use symbolic search and targeted symbol reads instead for more code-centric exploration. {switch_instruction}{unindexed_lang_note}"
            ),
        ),
        DenyKind::Slice => (
            "Too many consecutive file-slice reads without using any Astrolabe tool. Call search_code or find_symbol now — they are read-only and return exact bodies with path:line anchors.",
            "You were using many slice reads (small limit-bounded Read calls) recently without calling any Astrolabe tool. Slice reads are still permitted for precision work, but locate the region first: mcp__astrolabe__search_code or mcp__astrolabe__find_symbol are read-only and return exact bodies with path:line anchors, usually cheaper than repeated slice reads. Call them now (read-only, safe).".to_string(),
        ),
    };
    let ctx = format!("{ctx}{readonly_note}");

    match client {
        Client::Grok | Client::OpenCode => serde_json::json!({
            "decision": "deny",
            "reason": reason,
        })
        .to_string(),
        Client::Codex => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string(),
        // Cursor 原生扁平格式：不映射 additionalContext，长引导文案必须放 agent_message
        Client::Cursor => serde_json::json!({
            "permission": "deny",
            "user_message": reason,
            "agent_message": ctx,
        })
        .to_string(),
        Client::ClaudeCode
        | Client::Codebuddy
        | Client::Vscode
        | Client::KimiCode
        | Client::ZCode
        | Client::Other => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
                "additionalContext": ctx,
            }
        })
        .to_string(),
    }
}

/// 清除特定 session 的数据目录（幂等）
///
/// `session_id` 非法时返回 `InvalidInput`；删除失败时向上传播 IO 错误。
pub(crate) fn cleanup_session(home: &Path, session_id: &str) -> std::io::Result<()> {
    // agent 子目录随会话目录 remove_dir_all 一并递归删除，无需按 agent 分片
    let dir = get_hook_data_dir(home, session_id, None)
        .map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// 过期会话 GC 默认阈值（小时）：会话目录内最新 mtime 早于该阈值即视为陈旧并删除。
/// 用于不提供 SessionEnd 事件的宿主（如 ZCode），以及 SessionEnd 未触发（崩溃/强退）的兜底。
pub(crate) const DEFAULT_GC_MAX_AGE_HOURS: u64 = 24;
/// remind 中机会式 GC 的最小间隔（秒）：避免每次 PreToolUse 都扫描目录
pub(crate) const GC_MIN_INTERVAL_SECONDS: u64 = 3600;
/// GC 阈值环境变量（小时；0 表示禁用 remind 中的机会式 GC）
pub(crate) const ENV_GC_HOURS: &str = "ASTROLABE_HOOK_GC_HOURS";
/// 机会式 GC 节流戳文件名（放在 `~/.astrolabe/` 下，不混入 hook_data 会话目录）
const GC_STAMP_FILE: &str = "hook_gc.stamp";

/// 解析 GC 阈值（小时）：无效值回退默认
pub(crate) fn gc_max_age_hours_from_env() -> u64 {
    match std::env::var(ENV_GC_HOURS) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or_else(|_| {
            tracing::warn!(
                "invalid {ENV_GC_HOURS}={s:?}; using default {}",
                DEFAULT_GC_MAX_AGE_HOURS
            );
            DEFAULT_GC_MAX_AGE_HOURS
        }),
        Err(_) => DEFAULT_GC_MAX_AGE_HOURS,
    }
}

/// 递归求目录树内文件的最新 mtime（活动以 counter.json 等文件写入为准）；
/// 目录内无任何文件时回退目录自身 mtime。
fn newest_mtime(path: &Path) -> Option<std::time::SystemTime> {
    fn newest_file(path: &Path) -> Option<std::time::SystemTime> {
        let mut newest: Option<std::time::SystemTime> = None;
        for entry in std::fs::read_dir(path).ok()?.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let t = if ft.is_dir() {
                newest_file(&entry.path())
            } else {
                entry.metadata().ok().and_then(|m| m.modified().ok())
            };
            if let Some(t) = t {
                if newest.is_none_or(|n| t > n) {
                    newest = Some(t);
                }
            }
        }
        newest
    }
    newest_file(path).or_else(|| std::fs::metadata(path).ok()?.modified().ok())
}

/// 删除 `~/.astrolabe/hook_data/` 下最新 mtime 早于 `now - max_age` 的会话目录。
///
/// `keep` 指定的会话目录（当前会话）永不删除。返回删除的目录数。
/// 单个目录删除失败不中断整体扫描（尽力而为）。
pub(crate) fn gc_stale_sessions(
    home: &Path,
    now: std::time::SystemTime,
    max_age: std::time::Duration,
    keep: Option<&str>,
) -> std::io::Result<usize> {
    let root = home.join(".astrolabe").join("hook_data");
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let Some(cutoff) = now.checked_sub(max_age) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        if keep.is_some_and(|k| entry.file_name().to_str() == Some(k)) {
            continue;
        }
        let Some(mtime) = newest_mtime(&path) else {
            continue;
        };
        if mtime < cutoff {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed += 1,
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "hook GC 删除失败")
                }
            }
        }
    }
    Ok(removed)
}

/// remind 中的机会式 GC：按节流戳每小时至多扫描一次；任何错误静默忽略。
fn maybe_gc_opportunistic(home: &Path, now: std::time::SystemTime, keep: &str, max_age_hours: u64) {
    if max_age_hours == 0 {
        return;
    }
    let stamp = home.join(".astrolabe").join(GC_STAMP_FILE);
    if let Some(last) = std::fs::metadata(&stamp)
        .ok()
        .and_then(|m| m.modified().ok())
    {
        if now
            .duration_since(last)
            .is_ok_and(|d| d.as_secs() < GC_MIN_INTERVAL_SECONDS)
        {
            return;
        }
    }
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 先写戳再扫描：并发 remind 下尽量只有一个进程扫描
    let _ = std::fs::write(&stamp, b"");
    let max_age = std::time::Duration::from_secs(max_age_hours.saturating_mul(3600));
    match gc_stale_sessions(home, now, max_age, Some(keep)) {
        Ok(n) if n > 0 => tracing::debug!(removed = n, "hook GC 清理过期会话"),
        _ => {}
    }
}

/// `astrolabe hooks gc` 纯实现：不读 stdin，按阈值清理过期会话目录。
pub(crate) fn run_gc_impl<W: Write, E: Write>(
    mut stdout: W,
    mut stderr: E,
    now: std::time::SystemTime,
    max_age_hours: u64,
    home_override: Option<&Path>,
) -> i32 {
    let Some(home) = home_override.map(PathBuf::from).or_else(get_user_home) else {
        let _ = writeln!(stderr, "Cannot locate home directory");
        return 2;
    };
    let max_age = std::time::Duration::from_secs(max_age_hours.saturating_mul(3600));
    match gc_stale_sessions(&home, now, max_age, None) {
        Ok(n) => {
            let _ = writeln!(
                stdout,
                "removed {n} stale hook session dir(s) older than {max_age_hours}h"
            );
            0
        }
        Err(err) => {
            let _ = writeln!(stderr, "Failed to gc hook data: {err}");
            2
        }
    }
}

/// `astrolabe hooks gc`：清理超过阈值（默认 24h，`ASTROLABE_HOOK_GC_HOURS` 可调）未活动的会话目录。
pub fn run_gc() -> i32 {
    run_gc_impl(
        std::io::stdout().lock(),
        std::io::stderr().lock(),
        std::time::SystemTime::now(),
        gc_max_age_hours_from_env(),
        None,
    )
}

/// PreToolUse remind hook 纯实现（支持依赖注入方便测试；生产走 [`run_remind`]）。
#[cfg(test)]
pub(crate) fn run_remind_impl<R: Read, W: Write, E: Write>(
    client_name: &str,
    reader: R,
    stdout: W,
    stderr: E,
    now: Option<f64>,
    home_override: Option<&Path>,
) -> i32 {
    run_remind_impl_with(
        client_name,
        reader,
        stdout,
        stderr,
        now,
        home_override,
        config(),
    )
}

pub(crate) fn run_remind_impl_with<R: Read, W: Write, E: Write>(
    client_name: &str,
    mut reader: R,
    mut stdout: W,
    mut stderr: E,
    now: Option<f64>,
    home_override: Option<&Path>,
    cfg: &HooksConfig,
) -> i32 {
    // fail-open：宿主语义里 exit 2 = 拦截工具调用（Claude Code/Cursor/ZCode），
    // 任何输入问题都不应拦住 agent 的工具调用，只在 stderr 提示并返回 0。
    let mut raw = String::new();
    if let Err(err) = reader.read_to_string(&mut raw) {
        let _ = writeln!(
            stderr,
            "astrolabe hook: failed to read stdin ({err}); skipping (fail-open)"
        );
        return 0;
    }

    if raw.trim().is_empty() {
        let _ = writeln!(
            stderr,
            "astrolabe hook: hook input data is empty; skipping (fail-open)"
        );
        return 0;
    }

    let input_data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            let _ = writeln!(
                stderr,
                "astrolabe hook: invalid JSON in hook input ({err}); skipping (fail-open)"
            );
            return 0;
        }
    };

    // 提取 session_id / sessionId / conversation_id / conversationId
    let session_id = [
        "session_id",
        "sessionId",
        "conversation_id",
        "conversationId",
    ]
    .iter()
    .find_map(|k| json_nonempty_id(input_data.get(*k)));

    let session_id = match session_id {
        Some(s) => s,
        None => {
            let _ = writeln!(
                stderr,
                "astrolabe hook: session id is missing in hook input; skipping (fail-open)"
            );
            return 0;
        }
    };

    // 非法字符（点、冒号、空格…）不拒绝：哈希成路径安全的 `s-<fnv>` 分量
    let session_id = hashed_session_id(&session_id);

    // 提取 agent_id / agentId（子代理内触发的 PreToolUse payload 才携带，主线程不含）。
    // 尽力而为：缺失或类型不符时视为无分片，落回会话级平面路径，不报错。
    let agent_id = ["agent_id", "agentId"]
        .iter()
        .find_map(|k| json_nonempty_id(input_data.get(*k)));

    // 提取 tool_name / toolName
    let raw_tool_name = input_data
        .get("tool_name")
        .or_else(|| input_data.get("toolName"))
        .and_then(|v| v.as_str());

    let tool_name = match raw_tool_name {
        Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
        _ => {
            let _ = writeln!(
                stderr,
                "astrolabe hook: tool name is missing in hook input; skipping (fail-open)"
            );
            return 0;
        }
    };

    let client = Client::from_str(client_name);

    // 提取 tool_input 中的 file_path、切片参数（limit/offset）与 shell cmd
    let mut file_path: Option<String> = None;
    let mut command_name: Option<String> = None;
    let mut command_args_str: Option<String> = None;
    let mut full_command: Option<String> = None;
    let mut tool_input_limit: Option<u64> = None;
    let mut tool_input_offset: Option<u64> = None;

    let raw_tool_input = input_data
        .get("tool_input")
        .or_else(|| input_data.get("toolInput"));

    if let Some(serde_json::Value::Object(map)) = raw_tool_input {
        // 取 file_path / filePath / target_file / targetFile / path
        // （path 放最后兜底：Cursor 原生 Read 的字段是 tool_input.path；
        //  grep 类工具的 path 参数不影响判定——grep 分类先于 read 判定）
        let fp = map
            .get("file_path")
            .or_else(|| map.get("filePath"))
            .or_else(|| map.get("target_file"))
            .or_else(|| map.get("targetFile"))
            .or_else(|| map.get("path"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(fp_str) = fp {
            file_path = Some(fp_str.to_string());
        }

        // 取 limit / offset（数字或可解析为数字的字符串），用于切片精读判定
        tool_input_limit = map.get("limit").and_then(json_value_as_u64);
        tool_input_offset = map.get("offset").and_then(json_value_as_u64);

        // 取 cmd / command
        // Codex 可能把 command 传成 argv 数组（["bash","-lc","grep …"]）：拼成一条命令串
        let cmd_owned: Option<String> = map
            .get("cmd")
            .or_else(|| map.get("command"))
            .and_then(command_value_to_string)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let cmd = cmd_owned.as_deref();
        if let Some(cmd_str) = cmd {
            full_command = Some(cmd_str.to_string());
            if let Some((name, args)) = effective_shell_command(cmd_str) {
                command_name = Some(name);
                command_args_str = args;
            }
        }
    }

    let tool_kind = classify_tool_ex(
        &tool_name,
        client,
        file_path.as_deref(),
        command_name.as_deref(),
        command_args_str.as_deref(),
        full_command.as_deref(),
        tool_input_limit,
        tool_input_offset,
        cfg.deny_text_search,
    );

    tracing::debug!(
        client = %client_name,
        session_id = %session_id,
        ?agent_id,
        tool_name = %tool_name,
        ?tool_kind,
        "处理 PreToolUse hook 调用"
    );

    let now_ts = now.unwrap_or_else(current_timestamp);

    // 机会式 GC：仅真实运行（未注入 now）时执行，避免测试受环境影响
    if now.is_none() {
        if let Some(h) = home_override.map(PathBuf::from).or_else(get_user_home) {
            maybe_gc_opportunistic(
                &h,
                std::time::SystemTime::now(),
                &session_id,
                gc_max_age_hours_from_env(),
            );
        }
    }

    // 计算持久化路径（session_id 已 sanitize；agent 分量由 sanitize_agent_component 收敛）
    let home = home_override.map(PathBuf::from).or_else(get_user_home);
    let persistence_path = match home.as_ref() {
        Some(h) => match get_hook_data_dir(h, &session_id, agent_id.as_deref()) {
            Ok(dir) => Some(dir.join("counter.json")),
            Err(msg) => {
                let _ = writeln!(stderr, "astrolabe hook: {msg}; skipping (fail-open)");
                return 0;
            }
        },
        None => None,
    };

    // load → decide → save 在同一把文件锁内串行化
    let decision = match persistence_path.as_ref() {
        Some(path) => with_counter_locked(path, |mut counter| {
            let before = counter.clone();
            let decision = decide_with(&mut counter, tool_kind, now_ts, cfg);
            // 重置类决定在状态本来就是零时不落盘（省一次 rename + 锁）
            let should_save = match decision {
                Decision::ResetSymbolic | Decision::ResetSlice => counter != before,
                Decision::Allow | Decision::Deny(_) => true,
                _ => false,
            };
            (counter, should_save, decision)
        }),
        None => {
            let mut counter = CounterState::default();
            decide_with(&mut counter, tool_kind, now_ts, cfg)
        }
    };

    match decision {
        Decision::Silenced | Decision::NeutralAllow => {
            // 静默窗内或中立工具：不写盘，无输出
            0
        }
        Decision::ResetSymbolic => {
            // 符号工具：已清零 burst 计数并保存，无输出
            0
        }
        Decision::ResetSlice => {
            // 非符号 astrolabe 工具：已清零切片读额度并保存，无输出
            0
        }
        Decision::Allow => {
            // 正常累加计数未超阈值：已保存，无输出
            0
        }
        Decision::Deny(deny_kind) => {
            // 超阈值触发 deny：已清零并保存，输出 JSON
            let output_str = build_output(client, deny_kind);
            tracing::info!(?deny_kind, client = %client_name, "触发防漂移 deny 提示");
            let _ = writeln!(stdout, "{output_str}");
            0
        }
    }
}

/// SessionEnd cleanup hook 纯实现（支持依赖注入方便测试）
pub(crate) fn run_cleanup_impl<R: Read, E: Write>(
    mut reader: R,
    mut stderr: E,
    home_override: Option<&Path>,
) -> i32 {
    let mut raw = String::new();
    if let Err(err) = reader.read_to_string(&mut raw) {
        let _ = writeln!(
            stderr,
            "astrolabe hook: failed to read stdin ({err}); skipping (fail-open)"
        );
        return 0;
    }

    if raw.trim().is_empty() {
        let _ = writeln!(
            stderr,
            "astrolabe hook: hook input data is empty; skipping (fail-open)"
        );
        return 0;
    }

    let input_data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            let _ = writeln!(
                stderr,
                "astrolabe hook: invalid JSON in hook input ({err}); skipping (fail-open)"
            );
            return 0;
        }
    };

    // 与 remind 同构：conversation_id / conversationId fallback 兼容 Cursor 原生 payload
    let session_id = [
        "session_id",
        "sessionId",
        "conversation_id",
        "conversationId",
    ]
    .iter()
    .find_map(|k| json_nonempty_id(input_data.get(*k)));

    let session_id = match session_id {
        Some(s) => s,
        None => {
            let _ = writeln!(
                stderr,
                "astrolabe hook: session id is missing in hook input; skipping (fail-open)"
            );
            return 0;
        }
    };
    let session_id = hashed_session_id(&session_id);

    let home = home_override.map(PathBuf::from).or_else(get_user_home);
    if let Some(h) = home {
        match cleanup_session(&h, &session_id) {
            Ok(()) => {
                tracing::debug!(session_id = %session_id, "SessionEnd cleanup 成功");
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "Failed to cleanup session data: {err}");
                2
            }
        }
    } else {
        // 无 HOME 时无法定位状态目录；不视为成功清理，但无可清理目标
        tracing::debug!(session_id = %session_id, "SessionEnd cleanup skipped: no home dir");
        0
    }
}

/// PreToolUse remind hook：stdin 读 hook payload JSON，按 client 输出决策 JSON。
/// 返回进程退出码（0 正常，包括 allow；2 输入不合法）。
///
/// `deny_text_search_cli`：CLI `--deny-text-search` 显式开关（优先于 env / context）。
/// `context_name`：`--context=NAME` 或 path；用于读取 YAML `deny_text_search`。
pub fn run_remind(
    client_name: &str,
    deny_text_search_cli: Option<bool>,
    context_name: Option<&str>,
) -> i32 {
    let context_flag = context_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|name| crate::context::ClientContext::load(name).ok())
        .map(|c| c.deny_text_search);
    // Also honour ASTROLABE_CONTEXT when --context omitted (same as MCP server).
    let context_flag = context_flag.or_else(|| {
        if context_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
        {
            return None;
        }
        std::env::var("ASTROLABE_CONTEXT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .and_then(|name| crate::context::ClientContext::load(&name).ok())
            .map(|c| c.deny_text_search)
    });
    let deny = resolve_deny_text_search(
        deny_text_search_cli,
        std::env::var(ENV_DENY_TEXT_SEARCH).ok().as_deref(),
        context_flag,
    );
    let mut cfg = HooksConfig::from_env();
    cfg.deny_text_search = deny;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_remind_impl_with(
        client_name,
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
        None,
        None,
        &cfg,
    )
}

/// SessionEnd cleanup hook：按 stdin payload 的 session_id 删除状态目录。返回退出码。
pub fn run_cleanup() -> i32 {
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    run_cleanup_impl(stdin.lock(), stderr.lock(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_1_consecutive_grep_deny_and_burst_reset() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        // 第 1 次 grep
        let d1 = decide(&mut counter, ToolKind::Grep, t0);
        assert_eq!(d1, Decision::Allow);
        assert_eq!(counter.n_grep, 1);
        assert_eq!(counter.n_non_symbolic, 1);

        // 第 2 次 grep（间隔 1s）
        let d2 = decide(&mut counter, ToolKind::Grep, t0 + 1.0);
        assert_eq!(d2, Decision::Allow);
        assert_eq!(counter.n_grep, 2);
        assert_eq!(counter.n_non_symbolic, 2);

        // 第 3 次 grep（间隔 1s）-> 触发 grep deny
        let d3 = decide(&mut counter, ToolKind::Grep, t0 + 2.0);
        assert_eq!(d3, Decision::Deny(DenyKind::Grep));
        // burst 计数必须清零，且记录 last_deny_ts
        assert_eq!(counter.n_grep, 0);
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 0);
        assert_eq!(counter.last_grep_ts, None);
        assert_eq!(counter.last_deny_ts, Some(t0 + 2.0));
    }

    #[test]
    fn test_2_astrolabe_symbolic_resets_burst_counters() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        // 前 2 次 grep
        assert_eq!(decide(&mut counter, ToolKind::Grep, t0), Decision::Allow);
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 1.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 2);

        // 插入 Astrolabe 符号工具调用（如 find_references）
        let d_reset = decide(&mut counter, ToolKind::AstrolabeSymbolic, t0 + 2.0);
        assert_eq!(d_reset, Decision::ResetSymbolic);
        assert_eq!(counter.n_grep, 0);
        assert_eq!(counter.n_non_symbolic, 0);

        // 之后第 3、4 次 grep 重新累积，仍为 allow
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 3.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 1);
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 4.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 2);
    }

    #[test]
    fn test_3_search_code_does_not_reset_counter() {
        // search_code / diagnostics 等非导航类工具不应重置符号计数
        assert!(!is_astrolabe_symbolic_tool("mcp__astrolabe__search_code"));
        assert!(!is_astrolabe_symbolic_tool(
            "mcp__astrolabe__get_diagnostics"
        ));
        assert!(!is_astrolabe_symbolic_tool("mcp__astrolabe__get_languages"));

        // 导航类工具正常重置
        assert!(is_astrolabe_symbolic_tool("mcp__astrolabe__find_symbol"));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe__find_references"
        ));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe__goto_definition"
        ));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe__resolve_context"
        ));

        let kind = classify_tool(
            "mcp__astrolabe__search_code",
            Client::ClaudeCode,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(kind, ToolKind::AstrolabeNonSymbolic);

        let mut counter = CounterState {
            n_grep: 2,
            n_slice: 5,
            last_grep_ts: Some(100.0),
            last_slice_ts: Some(100.5),
            ..CounterState::default()
        };

        // 调用 search_code：grep 计数不重置（维持既有符号重置语义），
        // 但切片读额度清零（任何 astrolabe 工具调用都清零切片额度）
        let d = decide(&mut counter, kind, 101.0);
        assert_eq!(d, Decision::ResetSlice);
        assert_eq!(counter.n_grep, 2);
        assert_eq!(counter.n_slice, 0);
        assert_eq!(counter.last_slice_ts, None);
    }

    #[test]
    fn test_4_read_non_code_file_no_read_deny_counts_non_symbolic() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        let non_code_kind = ToolKind::Read {
            is_code_file: false,
        };

        // 连续 3 次读取 .md 文件（非代码文件）
        assert_eq!(decide(&mut counter, non_code_kind, t0), Decision::Allow);
        assert_eq!(counter.n_read, 0); // 不计入 read 计数
        assert_eq!(counter.n_non_symbolic, 1); // 计入 non_symbolic

        assert_eq!(
            decide(&mut counter, non_code_kind, t0 + 1.0),
            Decision::Allow
        );
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 2);

        // 第 3 次：不触发 read deny
        assert_eq!(
            decide(&mut counter, non_code_kind, t0 + 2.0),
            Decision::Allow
        );
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 3);

        // 第 4 次：non_symbolic 达到 4 阈值，触发 mixed deny
        let d4 = decide(&mut counter, non_code_kind, t0 + 3.0);
        assert_eq!(d4, Decision::Deny(DenyKind::Mixed));
        assert_eq!(counter.n_non_symbolic, 0);
    }

    #[test]
    fn test_5_mixed_grep_and_read_burst_triggers_mixed_deny() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        let code_read = ToolKind::Read { is_code_file: true };

        // 4 次交替调用：grep -> read -> grep -> read
        assert_eq!(decide(&mut counter, ToolKind::Grep, t0), Decision::Allow);
        assert_eq!(counter.n_grep, 1);
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 1);

        assert_eq!(decide(&mut counter, code_read, t0 + 1.0), Decision::Allow);
        assert_eq!(counter.n_grep, 1);
        assert_eq!(counter.n_read, 1);
        assert_eq!(counter.n_non_symbolic, 2);

        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 2.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 2);
        assert_eq!(counter.n_read, 1);
        assert_eq!(counter.n_non_symbolic, 3);

        // 第 4 次到达 non_symbolic=4，触发 mixed deny
        let d4 = decide(&mut counter, code_read, t0 + 3.0);
        assert_eq!(d4, Decision::Deny(DenyKind::Mixed));
        assert_eq!(counter.n_grep, 0);
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 0);
        assert_eq!(counter.last_deny_ts, Some(t0 + 3.0));
    }

    #[test]
    fn test_6_silent_window_within_15s() {
        let mut counter = CounterState {
            last_deny_ts: Some(1000.0),
            ..CounterState::default()
        };

        // 10s 后调用处于 15s 静默窗口内，应当为 Silenced，不更新计数
        let d1 = decide(&mut counter, ToolKind::Grep, 1010.0);
        assert_eq!(d1, Decision::Silenced);
        assert_eq!(counter.n_grep, 0);

        // 15s 后调用离开静默窗口，正常放行并更新计数
        let d2 = decide(&mut counter, ToolKind::Grep, 1015.0);
        assert_eq!(d2, Decision::Allow);
        assert_eq!(counter.n_grep, 1);
    }

    #[test]
    fn test_7_interval_greater_than_1000s_resets_counter() {
        let mut counter = CounterState::default();
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, 1000.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 1);

        // 超过 1000s（GREP_RESET_PERIOD_SECONDS）后再次 grep，计数重置为 1 而非递增至 2
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, 2001.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 1);
    }

    #[test]
    fn test_8_client_output_formats() {
        // 1. Claude Code
        let cc_out = build_output(Client::ClaudeCode, DenyKind::Grep);
        let cc_val: serde_json::Value = serde_json::from_str(&cc_out).unwrap();
        assert_eq!(cc_val["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(cc_val["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(cc_val["hookSpecificOutput"]["permissionDecisionReason"].is_string());
        assert!(cc_val["hookSpecificOutput"]["additionalContext"].is_string());

        // 2. Codex（无 additionalContext）
        let codex_out = build_output(Client::Codex, DenyKind::Read);
        let codex_val: serde_json::Value = serde_json::from_str(&codex_out).unwrap();
        assert_eq!(
            codex_val["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
        assert_eq!(
            codex_val["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert!(codex_val["hookSpecificOutput"]["permissionDecisionReason"].is_string());
        assert!(codex_val["hookSpecificOutput"]
            .get("additionalContext")
            .is_none());

        // 3. Grok（decision / reason）
        let grok_out = build_output(Client::Grok, DenyKind::Mixed);
        let grok_val: serde_json::Value = serde_json::from_str(&grok_out).unwrap();
        assert_eq!(grok_val["decision"], "deny");
        assert!(grok_val["reason"].is_string());
        assert!(grok_val.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_client_from_str_aliases() {
        // grok 的别名与大小写/空白变体都应映射到 Client::Grok
        for s in [
            "grok",
            "grokbuild",
            "grok-build",
            "Grok",
            "GROK",
            "GrokBuild",
            "GROKBUILD",
            "Grok-Build",
            "  grokbuild  ",
        ] {
            assert_eq!(Client::from_str(s), Client::Grok, "input: {s:?}");
        }
        // 未知名仍回落 Other；带空格的 "grok build" 不在别名表内
        assert_eq!(Client::from_str("unknown-client"), Client::Other);
        assert_eq!(Client::from_str("grok build"), Client::Other);
    }

    #[test]
    fn test_9_cleanup_session_is_idempotent() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_hook_test_{rand_id}"));
        let sess_dir = get_hook_data_dir(&temp_home, "sess_123", None).unwrap();
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::write(sess_dir.join("counter.json"), "{}").unwrap();
        assert!(sess_dir.exists());

        // 第一次删除
        let res1 = cleanup_session(&temp_home, "sess_123");
        assert!(res1.is_ok());
        assert!(!sess_dir.exists());

        // 第二次删除不存在的目录（幂等）
        let res2 = cleanup_session(&temp_home, "sess_123");
        assert!(res2.is_ok());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_10_missing_session_id_returns_exit_code_2() {
        let input_no_session = r#"{"tool_name": "grep"}"#;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "claude-code",
            input_no_session.as_bytes(),
            &mut out,
            &mut err,
            None,
            None,
        );
        // fail-open：exit 2 在宿主语义里等于拦截工具调用，缺 session id 只提示不拦截
        assert_eq!(code, 0);
        let err_str = String::from_utf8_lossy(&err);
        assert!(err_str.contains("session id is missing"), "{err_str}");

        let mut clean_err = Vec::new();
        let clean_code = run_cleanup_impl("{}".as_bytes(), &mut clean_err, None);
        assert_eq!(clean_code, 0);
        let clean_err_str = String::from_utf8_lossy(&clean_err);
        assert!(clean_err_str.contains("session id is missing"));
    }

    #[test]
    fn test_sanitize_session_id_accepts_safe_ids() {
        assert_eq!(sanitize_session_id("sess_123").unwrap(), "sess_123");
        assert_eq!(sanitize_session_id("abc-XYZ_09").unwrap(), "abc-XYZ_09");
        assert_eq!(
            sanitize_session_id("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_sanitize_session_id_rejects_unsafe_ids() {
        assert!(sanitize_session_id("").is_err());
        assert!(sanitize_session_id("/").is_err());
        assert!(sanitize_session_id("\\").is_err());
        assert!(sanitize_session_id("..").is_err());
        assert!(sanitize_session_id("../etc").is_err());
        assert!(sanitize_session_id("a/b").is_err());
        assert!(sanitize_session_id("a\\b").is_err());
        assert!(sanitize_session_id("has space").is_err());
        assert!(sanitize_session_id("dot.dot").is_err());
        assert!(get_hook_data_dir(Path::new("/tmp"), "../x", None).is_err());
    }

    #[test]
    fn test_invalid_session_id_is_hashed_not_rejected() {
        let tmp = std::env::temp_dir().join(format!("astro-h3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        for sid in ["../evil", "a.b:c", "has space"] {
            let payload = format!(
                r#"{{"session_id":"{sid}","tool_name":"read","tool_input":{{"file_path":"a.rs"}}}}"#
            );
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(1000.0),
                Some(&tmp),
            );
            assert_eq!(code, 0, "sid={sid}");
            assert!(out.is_empty());
            let hashed = hashed_session_id(sid);
            assert!(hashed.starts_with("s-"));
            assert!(
                tmp.join(".astrolabe/hook_data")
                    .join(&hashed)
                    .join("counter.json")
                    .exists(),
                "state should live under hashed dir for {sid}"
            );
            let mut cerr = Vec::new();
            let cp = format!(r#"{{"session_id":"{sid}"}}"#);
            assert_eq!(run_cleanup_impl(cp.as_bytes(), &mut cerr, Some(&tmp)), 0);
            assert!(!tmp.join(".astrolabe/hook_data").join(&hashed).exists());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn h3_malformed_input_is_fail_open_exit_0() {
        for raw in ["", "   ", "not json", "{}", r#"{"session_id":"s1"}"#] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = run_remind_impl("cursor", raw.as_bytes(), &mut out, &mut err, None, None);
            assert_eq!(code, 0, "raw={raw:?}");
            assert!(out.is_empty());
            assert!(!err.is_empty(), "stderr hint expected for {raw:?}");
        }
    }

    #[test]
    fn test_save_counter_atomic_writes_via_tmp_rename() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("astrolabe_atomic_save_{rand_id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("counter.json");

        let state = CounterState {
            n_grep: 7,
            n_read: 2,
            ..CounterState::default()
        };
        save_counter(&path, &state);
        assert!(path.exists());
        assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists());

        let loaded = load_counter(&path);
        assert_eq!(loaded.n_grep, 7);
        assert_eq!(loaded.n_read, 2);

        // Locked load→mutate→save round-trip
        with_counter_locked(&path, |mut c| {
            c.n_grep += 1;
            (c, true, ())
        });
        let loaded2 = load_counter(&path);
        assert_eq!(loaded2.n_grep, 8);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_end_to_end_remind_flow_with_tempdir() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_e2e_test_{rand_id}"));
        let sess_id = "e2e_sess";

        let payload_grep = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 1st grep
        let code1 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code1, 0);
        assert!(out.is_empty());

        // 2nd grep
        out.clear();
        let code2 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(code2, 0);
        assert!(out.is_empty());

        // 3rd grep -> Deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));

        // 4th grep at 110.0 (8s after the deny at 102.0, inside the 15s silent window)
        // -> Silenced (no output, no counter update)
        out.clear();
        let code4 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(110.0),
            Some(&temp_home),
        );
        assert_eq!(code4, 0);
        assert!(out.is_empty());

        // cleanup
        let clean_payload = serde_json::json!({ "session_id": sess_id }).to_string();
        let mut clean_err = Vec::new();
        let clean_code =
            run_cleanup_impl(clean_payload.as_bytes(), &mut clean_err, Some(&temp_home));
        assert_eq!(clean_code, 0);

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_11_claude_code_bash_grep_denies_on_3rd() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_grep_{rand_id}"));
        let sess_id = "cc_bash_grep_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "grep -rn foo src/"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 1st call
        let c1 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(c1, 0);
        assert!(out.is_empty());

        // 2nd call
        out.clear();
        let c2 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(c2, 0);
        assert!(out.is_empty());

        // 3rd call -> deny
        out.clear();
        let c3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(c3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive grep calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_12_claude_code_bash_rg_path_prefix() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_rg_{rand_id}"));
        let sess_id = "cc_bash_rg_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "/usr/bin/rg pattern"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 1st call
        let c1 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(c1, 0);
        assert!(out.is_empty());

        // 2nd call
        out.clear();
        let c2 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(c2, 0);
        assert!(out.is_empty());

        // 3rd call -> deny
        out.clear();
        let c3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(c3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive grep calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_13_claude_code_bash_neutral_command_no_deny() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_neutral_{rand_id}"));
        let sess_id = "cc_bash_neutral_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "ls -la"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 连续 5 次中性命令
        for i in 0..5 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "第 {} 次调用不应产生 deny 输出", i + 1);
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_14_claude_code_bash_cat_code_vs_non_code() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_cat_{rand_id}"));

        // Case 1: cat src/main.rs (代码文件) 连续 3 次 -> 第 3 次 read deny
        let code_payload = serde_json::json!({
            "session_id": "sess_cat_code",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat src/main.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                code_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次触发 read deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            code_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        // Case 2: cat README.md (非代码文件) 连续 3 次 -> 不触发 read deny
        let doc_payload = serde_json::json!({
            "session_id": "sess_cat_doc",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat README.md"
            }
        })
        .to_string();

        for i in 0..3 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                doc_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(200.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "README.md 读取不应触发 read deny");
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_15_codex_grok_bash_regression() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_codex_grok_{rand_id}"));

        // Codex: grep shell command 3 times -> codex deny format
        let codex_payload = serde_json::json!({
            "session_id": "sess_codex",
            "tool_name": "bash",
            "tool_input": {
                "command": "grep pattern lib.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            assert_eq!(
                run_remind_impl(
                    "codex",
                    codex_payload.as_bytes(),
                    &mut out,
                    &mut err,
                    Some(100.0 + i as f64),
                    Some(&temp_home)
                ),
                0
            );
            assert!(out.is_empty());
        }

        out.clear();
        assert_eq!(
            run_remind_impl(
                "codex",
                codex_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(102.0),
                Some(&temp_home)
            ),
            0
        );
        let codex_out_str = String::from_utf8_lossy(&out);
        let codex_val: serde_json::Value = serde_json::from_str(&codex_out_str).unwrap();
        assert_eq!(
            codex_val["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert!(codex_val["hookSpecificOutput"]
            .get("additionalContext")
            .is_none());

        // Grok: cat code file 3 times -> grok deny format
        let grok_payload = serde_json::json!({
            "session_id": "sess_grok",
            "tool_name": "bash",
            "tool_input": {
                "command": "cat src/lib.rs"
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            assert_eq!(
                run_remind_impl(
                    "grok",
                    grok_payload.as_bytes(),
                    &mut out,
                    &mut err,
                    Some(200.0 + i as f64),
                    Some(&temp_home)
                ),
                0
            );
            assert!(out.is_empty());
        }

        out.clear();
        assert_eq!(
            run_remind_impl(
                "grok",
                grok_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(202.0),
                Some(&temp_home)
            ),
            0
        );
        let grok_out_str = String::from_utf8_lossy(&out);
        let grok_val: serde_json::Value = serde_json::from_str(&grok_out_str).unwrap();
        assert_eq!(grok_val["decision"], "deny");
        assert!(grok_val["reason"].is_string());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_16_has_write_redirection_and_sed_in_place_unit() {
        // 写重定向 token 判定
        assert!(has_write_redirection("> src/main.rs <<'EOF'"));
        assert!(has_write_redirection("x >> src/a.rs"));
        assert!(has_write_redirection(">file.txt"));
        assert!(has_write_redirection(">>file.txt"));
        assert!(has_write_redirection("<<EOF"));
        assert!(has_write_redirection("<<'EOF'"));
        assert!(has_write_redirection("<<<\"test\""));

        // 输入重定向与普通参数不应判定为写重定向
        assert!(!has_write_redirection("< src/main.rs"));
        assert!(!has_write_redirection("<src/main.rs"));
        assert!(!has_write_redirection("src/main.rs -> other.rs"));
        assert!(!has_write_redirection("src/main.rs"));
        assert!(!has_write_redirection(""));

        // sed 原地写参数判定
        assert!(is_sed_in_place("-i 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("-i'' 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("-i.bak 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("--in-place 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("--in-place=.bak 's/a/b/' src/a.rs"));
        assert!(!is_sed_in_place("'s/a/b/' src/a.rs"));
        assert!(!is_sed_in_place("-e 's/a/b/' src/a.rs"));
        assert!(!is_sed_in_place("-n 's/a/b/' src/a.rs"));
    }

    #[test]
    fn test_17_shell_write_redirection_cat_heredoc_no_deny() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cat_heredoc_{rand_id}"));
        let sess_id = "sess_cat_heredoc";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat > src/main.rs <<'EOF'"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 连续 4 次 heredoc 写操作，不应触发 read deny 或 mixed deny（视为中性命令）
        for i in 0..4 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "第 {} 次 heredoc 写操作不应产生 deny 输出",
                i + 1
            );
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_18_shell_write_redirection_echo_append_no_read() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_echo_append_{rand_id}"));
        let sess_id = "sess_echo_append";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "echo x >> src/a.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 连续 4 次 echo 追加写操作，不计 read，不触发 deny
        for i in 0..4 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "第 {} 次 echo 追加操作不应产生 deny 输出",
                i + 1
            );
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_19_shell_input_redirection_cat_counts_as_read() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cat_input_{rand_id}"));
        let sess_id = "sess_cat_input";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat < src/main.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次输入重定向 cat 读取代码文件 -> 触发 read deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_20_shell_sed_inplace_vs_stdout_read() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_sed_test_{rand_id}"));

        // Case 1: sed -i 原地修改（含变体 -i''、-i.bak、--in-place）不计 read
        let sed_i_payload = serde_json::json!({
            "session_id": "sess_sed_i",
            "tool_name": "Bash",
            "tool_input": {
                "command": "sed -i 's/a/b/' src/a.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..4 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                sed_i_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "sed -i 不应计为 read");
        }

        // sed 变体：-i''、-i.bak、--in-place 均不触发 deny
        for (i, cmd) in [
            "sed -i'' 's/a/b/' src/a.rs",
            "sed -i.bak 's/a/b/' src/a.rs",
            "sed --in-place 's/a/b/' src/a.rs",
            "sed --in-place=.bak 's/a/b/' src/a.rs",
        ]
        .into_iter()
        .enumerate()
        {
            let payload = serde_json::json!({
                "session_id": "sess_sed_variants",
                "tool_name": "Bash",
                "tool_input": { "command": cmd }
            })
            .to_string();
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(200.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "sed 原地变体不应计为 read: {cmd}");
        }

        // Case 2: sed 's/a/b/' src/a.rs（无 -i，stdout 读）连续 3 次 -> 触发 read deny
        let sed_read_payload = serde_json::json!({
            "session_id": "sess_sed_read",
            "tool_name": "Bash",
            "tool_input": {
                "command": "sed 's/a/b/' src/a.rs"
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                sed_read_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(300.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            sed_read_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(302.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_21_shell_grep_with_redirection_still_counts_grep() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_grep_redir_{rand_id}"));
        let sess_id = "sess_grep_redir";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "grep -n foo src/a.rs > out.txt"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次 grep（即使命令行含重定向 > out.txt）仍照常触发 grep deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive grep calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_claude_code_read_with_slice_quota_denies_on_10th() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_read_slice_{rand_id}"));
        let sess_id = "cc_read_slice_sess";

        // 单元层：Read + offset/limit 局部切片 → SliceRead（计入连续切片额度，不再 Neutral）
        let kind = classify_tool(
            "read",
            Client::ClaudeCode,
            Some("src/main.rs"),
            None,
            None,
            Some(40),
            Some(300),
        );
        assert_eq!(kind, ToolKind::SliceRead);

        // 单元层：非代码文件（未索引语言/markdown）的切片 → Neutral（与全量读口径一致）
        for non_code in ["src/Main.scala", "README.md", "notes.txt"] {
            assert_eq!(
                classify_tool(
                    "read",
                    Client::ClaudeCode,
                    Some(non_code),
                    None,
                    None,
                    Some(40),
                    Some(300)
                ),
                ToolKind::Neutral,
                "非代码文件切片应保持中立: {non_code}"
            );
        }

        // e2e：前 9 次 Read(offset=300, limit=40) 无输出，第 10 次触发 Slice deny
        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/main.rs",
                "offset": 300,
                "limit": 40
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..9 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "第 {} 次切片 Read 不应产生 deny 输出",
                i + 1
            );
        }

        out.clear();
        let code10 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(109.0),
            Some(&temp_home),
        );
        assert_eq!(code10, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("file-slice reads"));

        // 触发后额度清零并记录 last_deny_ts（进入静默窗）
        let counter_path = get_hook_data_dir(&temp_home, sess_id, None)
            .unwrap()
            .join("counter.json");
        let counter = load_counter(&counter_path);
        assert_eq!(counter.n_slice, 0);
        assert_eq!(counter.last_slice_ts, None);
        assert_eq!(counter.last_deny_ts, Some(109.0));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_slice_quota_triggers_on_10th_and_resets_by_any_astrolabe_tool() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_slice_quota_{rand_id}"));
        let sess_id = "slice_quota_sess";

        let slice_payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Read",
            "tool_input": { "file_path": "src/a.rs", "offset": 10, "limit": 50 }
        })
        .to_string();
        // 非符号 astrolabe 工具（AstrolabeNonSymbolic）：同样清零切片额度
        let search_payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "mcp__astrolabe__search_code",
            "tool_input": { "query": "foo" }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut now = 1000.0;

        // 前 9 次切片读：无输出
        for i in 0..9 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                slice_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(now),
                Some(&temp_home),
            );
            assert_eq!(code, 0, "第 {} 次切片读应正常放行", i + 1);
            assert!(out.is_empty(), "第 {} 次切片读不应有输出", i + 1);
            now += 1.0;
        }

        // 第 10 次：触发 Slice deny
        out.clear();
        let code10 = run_remind_impl(
            "claude-code",
            slice_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(now),
            Some(&temp_home),
        );
        assert_eq!(code10, 0);
        let denied = String::from_utf8_lossy(&out);
        assert!(
            denied.contains("\"permissionDecision\":\"deny\""),
            "第 10 次切片读应触发 deny，got: {denied}"
        );
        assert!(denied.contains("file-slice reads"));
        now += 1.0;

        // 跳出 deny 后 15s 静默窗
        now += 100.0;

        // 累计 5 次切片读（额度=5，未达阈值）
        for i in 0..5 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                slice_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "静默窗后第 {} 次切片读不应有输出", i + 1);
            now += 1.0;
        }

        // 1 次 mcp__astrolabe__search_code（AstrolabeNonSymbolic）→ 切片额度清零
        out.clear();
        let code_search = run_remind_impl(
            "claude-code",
            search_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(now),
            Some(&temp_home),
        );
        assert_eq!(code_search, 0);
        assert!(out.is_empty(), "astrolabe 非符号工具调用本身不应有输出");
        let counter_path = get_hook_data_dir(&temp_home, sess_id, None)
            .unwrap()
            .join("counter.json");
        let counter = load_counter(&counter_path);
        assert_eq!(counter.n_slice, 0, "search_code 调用后切片额度应清零");
        assert_eq!(counter.last_slice_ts, None);
        now += 1.0;

        // 再 9 次切片读：无输出（若额度未清零，其中第 5 次即达 10 阈值触发 deny）
        for i in 0..9 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                slice_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "清零后第 {} 次切片读不应有输出（额度应已被 search_code 清零）",
                i + 1
            );
            now += 1.0;
        }

        // 清零后的第 10 次：再次触发 Slice deny
        out.clear();
        let code_again = run_remind_impl(
            "claude-code",
            slice_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(now),
            Some(&temp_home),
        );
        assert_eq!(code_again, 0);
        assert!(String::from_utf8_lossy(&out).contains("\"permissionDecision\":\"deny\""));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_non_code_file_slices_never_deny() {
        // 非代码文件（未索引语言/markdown）切片与全量读口径一致：不计数、不触发 Slice deny
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_kt_slice_{rand_id}"));

        let payload = serde_json::json!({
            "session_id": "kt_slice_sess",
            "tool_name": "Read",
            "tool_input": { "file_path": "src/Main.scala", "offset": 1, "limit": 50 }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        for i in 0..15 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "第 {} 次 .scala 切片读不应有输出", i + 1);
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_old_counter_json_without_slice_fields_deserializes() {
        // 旧版 counter.json（无 n_slice/last_slice_ts 字段）反序列化落到默认值，不 panic
        let old = r#"{"n_grep":2,"n_read":1,"n_non_symbolic":3,"last_grep_ts":1.0,"last_read_ts":1.0,"last_non_symbolic_ts":1.0,"last_deny_ts":null}"#;
        let c: CounterState = serde_json::from_str(old).expect("旧 counter.json 应可反序列化");
        assert_eq!(c.n_slice, 0);
        assert_eq!(c.last_slice_ts, None);
        assert_eq!(c.n_grep, 2);
    }

    #[test]
    fn test_slice_counter_semantics_edges() {
        // decide() 纯逻辑层：静默窗内不计数、2000s 间隔重置、符号工具清零 n_slice、
        // 阈值默认从 config() 读取
        // 1. Slice deny 后 15s 静默窗内再切片 → Silenced 且计数不变
        let mut counter = CounterState {
            n_slice: 9,
            last_slice_ts: Some(100.0),
            last_deny_ts: Some(99.0),
            ..CounterState::default()
        };
        let d = decide(&mut counter, ToolKind::SliceRead, 105.0);
        assert_eq!(d, Decision::Silenced);
        assert_eq!(counter.n_slice, 9, "静默窗内不应累加切片计数");

        // 2. 间隔 > SLICE_RESET_PERIOD_SECONDS(2000s) → 重置为 1
        let mut counter = CounterState {
            n_slice: 8,
            last_slice_ts: Some(100.0),
            ..CounterState::default()
        };
        let d = decide(&mut counter, ToolKind::SliceRead, 2101.5);
        assert_eq!(d, Decision::Allow);
        assert_eq!(counter.n_slice, 1, "超 2000s 间隔应重置为 1");

        // 3. 符号工具 reset_burst 清零 n_slice
        let mut counter = CounterState {
            n_slice: 7,
            last_slice_ts: Some(100.0),
            ..CounterState::default()
        };
        let d = decide(&mut counter, ToolKind::AstrolabeSymbolic, 101.0);
        assert_eq!(d, Decision::ResetSymbolic);
        assert_eq!(counter.n_slice, 0, "符号工具应清零切片额度");
        assert_eq!(counter.last_slice_ts, None);
    }

    #[test]
    fn test_slice_threshold_env_override() {
        // 只测 HooksConfig::from_env 解析；全局 CONFIG OnceLock 已在其它测试中初始化，
        // 环境变量覆盖对 config() 不生效，故不做依赖新阈值的 e2e 断言
        let orig = std::env::var(ENV_SLICE_READ_THRESHOLD).ok();

        std::env::set_var(ENV_SLICE_READ_THRESHOLD, "3");
        let cfg = HooksConfig::from_env();
        assert_eq!(cfg.slice_read_threshold, 3);

        // 无效值回退默认
        std::env::set_var(ENV_SLICE_READ_THRESHOLD, "not_a_number");
        let cfg2 = HooksConfig::from_env();
        assert_eq!(cfg2.slice_read_threshold, DEFAULT_SLICE_READ_THRESHOLD);

        // 恢复原始环境变量
        match orig {
            Some(v) => std::env::set_var(ENV_SLICE_READ_THRESHOLD, v),
            None => std::env::remove_var(ENV_SLICE_READ_THRESHOLD),
        }
    }

    #[test]
    fn test_deny_texts_direct_switch_instruction() {
        // Grep/Read/Mixed 三种 DenyKind：输出必须给出直接切换指令，
        // 不得残留 "You can continue using ... the counter was reset" 软口径
        for kind in [DenyKind::Grep, DenyKind::Read, DenyKind::Mixed] {
            let out = build_output(Client::ClaudeCode, kind);
            assert!(
                out.contains("search_code"),
                "{kind:?} deny 输出应含 search_code 切换指令: {out}"
            );
            assert!(
                out.contains("Stop using grep/read for code discovery"),
                "{kind:?} deny 输出应含明确停止+切换指令: {out}"
            );
            assert!(
                !out.contains("You can continue using"),
                "{kind:?} deny 输出不应残留软口径: {out}"
            );
            assert!(
                !out.contains("the counter was reset"),
                "{kind:?} deny 输出不应残留计数重置软话术: {out}"
            );
        }
    }

    #[test]
    fn test_slice_deny_client_output_formats() {
        // Claude Code：三字段 hookSpecificOutput
        let cc_out = build_output(Client::ClaudeCode, DenyKind::Slice);
        let cc_val: serde_json::Value = serde_json::from_str(&cc_out).unwrap();
        assert_eq!(cc_val["hookSpecificOutput"]["permissionDecision"], "deny");
        let cc_reason = cc_val["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(cc_reason.contains("file-slice reads"));
        assert!(cc_reason.contains("search_code"));
        assert!(!cc_out.contains("You can continue using"));
        let cc_ctx = cc_val["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(cc_ctx.contains("ASTROLABE_SLICE_READ_THRESHOLD"));
        assert!(cc_ctx.contains("read-only"));

        // Cursor：扁平 permission/user_message/agent_message
        let cur_out = build_output(Client::Cursor, DenyKind::Slice);
        let cur_val: serde_json::Value = serde_json::from_str(&cur_out).unwrap();
        assert_eq!(cur_val["permission"], "deny");
        assert!(cur_val["user_message"].is_string());
        assert!(cur_val["agent_message"].is_string());
        assert!(cur_val.get("hookSpecificOutput").is_none());

        // Grok：扁平 decision/reason
        let grok_out = build_output(Client::Grok, DenyKind::Slice);
        let grok_val: serde_json::Value = serde_json::from_str(&grok_out).unwrap();
        assert_eq!(grok_val["decision"], "deny");
        assert!(grok_val["reason"].is_string());
        assert!(grok_val.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_claude_code_read_without_limit_still_denies() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_read_full_{rand_id}"));

        let mut out = Vec::new();
        let mut err = Vec::new();

        // Case 1: 未传 limit/offset（整文件盲读）连续 3 次 -> 第 3 次 read deny
        let payload_full = serde_json::json!({
            "session_id": "cc_read_full_sess",
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/main.rs"
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_full.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload_full.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        // Case 2: limit=500 > 200（大块读取）连续 3 次 -> 依然触发 read deny
        let payload_big = serde_json::json!({
            "session_id": "cc_read_big_sess",
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/main.rs",
                "offset": 1,
                "limit": 500
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_big.as_bytes(),
                &mut out,
                &mut err,
                Some(200.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code6 = run_remind_impl(
            "claude-code",
            payload_big.as_bytes(),
            &mut out,
            &mut err,
            Some(202.0),
            Some(&temp_home),
        );
        assert_eq!(code6, 0);
        let out_str6 = String::from_utf8_lossy(&out);
        assert!(out_str6.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str6.contains("Too many consecutive read calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_shell_slice_print_and_is_slice_read_unit() {
        let code_read = ToolKind::Read { is_code_file: true };

        // head/tail -n N（N <= 200）切片打印 → Neutral
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("-n 40 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("tail"),
                Some("-n 200 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("-n40 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("--lines=50 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );

        // N > 200、无 -n、tail -n +N（打印到 EOF，非有界切片）→ 仍计 Read
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("-n 500 src/a.rs"),
                None,
                None
            ),
            code_read
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("src/a.rs"),
                None,
                None
            ),
            code_read
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("tail"),
                Some("-n +5 src/a.rs"),
                None,
                None
            ),
            code_read
        );

        // sed -n '地址p' 切片打印 → Neutral
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n '300,340p' src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n '50p;80p' src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n 300,340p src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );

        // sed 无 -n 或脚本无 p 打印命令 → 仍计 Read
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("'s/a/b/' src/a.rs"),
                None,
                None
            ),
            code_read
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n 's/a/b/' src/a.rs"),
                None,
                None
            ),
            code_read
        );

        // cat 不属于切片命令 → 仍计 Read
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("cat"),
                Some("src/a.rs"),
                None,
                None
            ),
            code_read
        );

        // is_slice_read 直接工具判定：必须显式带 limit 且 limit <= 200（默认阈值）
        assert!(is_slice_read("Read", None, None, Some(200), None));
        assert!(is_slice_read("read", None, None, Some(40), Some(300)));
        assert!(!is_slice_read("read", None, None, Some(201), None));
        assert!(!is_slice_read("read_file", None, None, None, Some(2))); // 无 limit 不放行
        assert!(is_slice_read("read_file", None, None, Some(40), Some(2)));
        assert!(!is_slice_read("read", None, None, None, Some(1)));
        assert!(!is_slice_read("read", None, None, None, None));
        assert!(!is_slice_read("read", None, None, Some(500), Some(300)));

        // json_value_as_u64：Number / 数字字符串 / 非数字
        assert_eq!(json_value_as_u64(&serde_json::json!(40)), Some(40));
        assert_eq!(json_value_as_u64(&serde_json::json!("40")), Some(40));
        assert_eq!(json_value_as_u64(&serde_json::json!(" 120 ")), Some(120));
        assert_eq!(json_value_as_u64(&serde_json::json!("abc")), None);
        assert_eq!(json_value_as_u64(&serde_json::json!(true)), None);
        assert_eq!(json_value_as_u64(&serde_json::json!(null)), None);
    }

    #[test]
    fn test_hooks_config_defaults() {
        // 验证默认配置值
        let default_cfg = HooksConfig::default();
        assert_eq!(default_cfg.slice_read_max_limit, 200);
        assert_eq!(default_cfg.slice_read_threshold, 10);
        assert_eq!(default_cfg.read_threshold, 3);
        assert_eq!(default_cfg.grep_threshold, 3);
        assert!(!default_cfg.deny_text_search);
        assert_eq!(default_cfg.min_deny_interval_seconds, 15.0);
    }

    #[test]
    fn test_hooks_config_from_env_parsing() {
        // 测试 HooksConfig 的环境变量解析逻辑
        // 注意：这里不能修改全局 CONFIG 单例，只测试 from_env 的解析行为

        // 保存原始环境变量
        let orig_slice = std::env::var(ENV_SLICE_READ_MAX).ok();
        let orig_threshold = std::env::var(ENV_READ_THRESHOLD).ok();
        let orig_silence = std::env::var(ENV_DENY_SILENCE_SECS).ok();

        // 设置自定义值
        std::env::set_var(ENV_SLICE_READ_MAX, "300");
        std::env::set_var(ENV_READ_THRESHOLD, "5");
        std::env::set_var(ENV_DENY_SILENCE_SECS, "60");

        let cfg = HooksConfig::from_env();
        assert_eq!(cfg.slice_read_max_limit, 300);
        assert_eq!(cfg.read_threshold, 5);
        assert_eq!(cfg.min_deny_interval_seconds, 60.0);

        // 测试无效值回退默认
        std::env::set_var(ENV_SLICE_READ_MAX, "invalid");
        std::env::set_var(ENV_READ_THRESHOLD, "not_a_number");
        std::env::set_var(ENV_DENY_SILENCE_SECS, "bad");

        let cfg2 = HooksConfig::from_env();
        assert_eq!(cfg2.slice_read_max_limit, DEFAULT_SLICE_READ_MAX_LIMIT);
        assert_eq!(cfg2.read_threshold, DEFAULT_READ_THRESHOLD);
        assert_eq!(
            cfg2.min_deny_interval_seconds,
            DEFAULT_MIN_DENY_INTERVAL_SECONDS
        );

        // 恢复原始环境变量
        match orig_slice {
            Some(v) => std::env::set_var(ENV_SLICE_READ_MAX, v),
            None => std::env::remove_var(ENV_SLICE_READ_MAX),
        }
        match orig_threshold {
            Some(v) => std::env::set_var(ENV_READ_THRESHOLD, v),
            None => std::env::remove_var(ENV_READ_THRESHOLD),
        }
        match orig_silence {
            Some(v) => std::env::set_var(ENV_DENY_SILENCE_SECS, v),
            None => std::env::remove_var(ENV_DENY_SILENCE_SECS),
        }
    }

    #[test]
    fn test_is_code_file_path_supported_vs_unsupported() {
        // 未支持语言不应计为代码文件（不拦截）
        assert!(!is_code_file_path("a.ex"));
        assert!(!is_code_file_path("a.scala"));
        assert!(!is_code_file_path("a.hs"));
        assert!(!is_code_file_path("noext"));

        // 支持的语言应计为代码文件（拦截）
        assert!(is_code_file_path("a.dart"));
        assert!(is_code_file_path("a.py"));
        assert!(is_code_file_path("a.rs"));
        assert!(is_code_file_path("scripts/main.csx"));

        // 带引号的路径 trim 逻辑仍正常工作
        assert!(is_code_file_path("'a.py'"));
        assert!(is_code_file_path("\"a.rs\""));
        assert!(!is_code_file_path("'a.scala'"));
    }

    #[test]
    fn test_classify_tool_unsupported_language_read() {
        let kind_kt = classify_tool(
            "read",
            Client::ClaudeCode,
            Some("src/Main.scala"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            kind_kt,
            ToolKind::Read {
                is_code_file: false
            }
        );

        let kind_scala = classify_tool(
            "read",
            Client::ClaudeCode,
            Some("src/Main.scala"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            kind_scala,
            ToolKind::Read {
                is_code_file: false
            }
        );

        let kind_dart = classify_tool(
            "read",
            Client::ClaudeCode,
            Some("lib/main.dart"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(kind_dart, ToolKind::Read { is_code_file: true });
    }

    #[test]
    fn test_build_output_grep_and_mixed_contain_ignore_reminder() {
        let reminder_sub = "ignore this reminder";

        let cc_grep = build_output(Client::ClaudeCode, DenyKind::Grep);
        assert!(
            cc_grep.contains(reminder_sub),
            "Grep deny output should contain '{reminder_sub}', got: {cc_grep}"
        );

        let cc_mixed = build_output(Client::ClaudeCode, DenyKind::Mixed);
        assert!(
            cc_mixed.contains(reminder_sub),
            "Mixed deny output should contain '{reminder_sub}', got: {cc_mixed}"
        );

        let cc_read = build_output(Client::ClaudeCode, DenyKind::Read);
        assert!(
            !cc_read.contains(reminder_sub),
            "Read deny output should NOT contain '{reminder_sub}', got: {cc_read}"
        );
    }

    #[test]
    fn test_agent_scoped_counters_isolate_sibling_subagents() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_agent_iso_{rand_id}"));
        let sess_id = "agent_iso_sess";

        let mk_payload = |agent: &str| {
            serde_json::json!({
                "session_id": sess_id,
                "agent_id": agent,
                "tool_name": "grep",
                "tool_input": {}
            })
            .to_string()
        };
        let payload_a = mk_payload("agent-a");
        let payload_b = mk_payload("agent-b");

        let mut out = Vec::new();
        let mut err = Vec::new();

        // agent-a：3 次 grep（100/101/102）-> 第 3 次触发自己的 deny
        for (i, now) in [100.0, 101.0, 102.0].iter().enumerate() {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_a.as_bytes(),
                &mut out,
                &mut err,
                Some(*now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            if i < 2 {
                assert!(out.is_empty(), "agent-a 第 {} 次 grep 不应 deny", i + 1);
            }
        }
        let out_str = String::from_utf8_lossy(&out);
        assert!(
            out_str.contains("\"permissionDecision\":\"deny\""),
            "agent-a 第 3 次 grep 应触发 deny，got: {out_str}"
        );

        // agent-b：同会话第 1 次 grep（now=103）不受 agent-a 计数影响，无 deny 输出
        out.clear();
        let code_b1 = run_remind_impl(
            "claude-code",
            payload_b.as_bytes(),
            &mut out,
            &mut err,
            Some(103.0),
            Some(&temp_home),
        );
        assert_eq!(code_b1, 0);
        assert!(
            out.is_empty(),
            "agent-b 首次 grep 不应被 agent-a 的 deny 计数/静默窗波及"
        );

        // 补强：agent-b 继续累计到自己的第 3 次（105）必须触发 deny。
        // 若分片失效（共享 counter），agent-b 会整体落在 agent-a deny 的 15s 静默窗内，
        // 三次全部 Silenced 无输出——该断言可区分两种实现。
        for now in [104.0, 105.0] {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_b.as_bytes(),
                &mut out,
                &mut err,
                Some(now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
        }
        let out_b_str = String::from_utf8_lossy(&out);
        assert!(
            out_b_str.contains("\"permissionDecision\":\"deny\""),
            "agent-b 第 3 次 grep 应触发独立 deny，got: {out_b_str}"
        );

        // 落盘路径按 agent 分片
        assert!(get_hook_data_dir(&temp_home, sess_id, Some("agent-a"))
            .unwrap()
            .join("counter.json")
            .exists());
        assert!(get_hook_data_dir(&temp_home, sess_id, Some("agent-b"))
            .unwrap()
            .join("counter.json")
            .exists());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_missing_agent_id_uses_flat_session_counter_path() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_no_agent_{rand_id}"));
        let sess_id = "no_agent_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0);
        assert!(out.is_empty());

        // counter.json 仍落在 hook_data/<sid>/counter.json 平面路径，会话目录下无子目录
        let sess_dir = get_hook_data_dir(&temp_home, sess_id, None).unwrap();
        assert!(sess_dir.join("counter.json").exists());
        let entries: Vec<_> = std::fs::read_dir(&sess_dir)
            .unwrap()
            .filter(|e| {
                // flock 锁文件 counter.json.lock 常驻（删除会引入竞态），不算状态条目
                e.as_ref()
                    .map(|e| e.file_name() != "counter.json.lock")
                    .unwrap_or(true)
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "无 agent_id 时会话目录应只有 counter.json 一个条目"
        );
        assert!(entries[0].as_ref().unwrap().path().is_file());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_camel_case_agent_id_field_shards_counter() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_camel_agent_{rand_id}"));
        let sess_id = "camel_agent_sess";

        // 驼峰 agentId 字段同样命中分片
        let payload = serde_json::json!({
            "session_id": sess_id,
            "agentId": "camel-agent",
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0);
        assert!(out.is_empty());

        assert!(get_hook_data_dir(&temp_home, sess_id, Some("camel-agent"))
            .unwrap()
            .join("counter.json")
            .exists());
        // 未落会话级平面路径
        assert!(!get_hook_data_dir(&temp_home, sess_id, None)
            .unwrap()
            .join("counter.json")
            .exists());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_unsafe_agent_id_falls_back_to_hashed_shard() {
        // 纯函数层：合法 agent_id 原样保留；穿越型/超长串收敛为 ag-<fnv1a64> 哈希分量
        // FNV-1a 64 标准测试向量（写死常数，防算法实现写错而期望值同错仍绿）
        assert_eq!(fnv1a64(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64("a"), 0xaf63dc4c8601ec8c);
        assert_eq!(sanitize_agent_component("agent-a"), "agent-a");
        let expected_evil = format!("ag-{:016x}", fnv1a64("../evil"));
        assert_eq!(sanitize_agent_component("../evil"), expected_evil);
        let long_id = "a".repeat(200);
        let expected_long = format!("ag-{:016x}", fnv1a64(&long_id));
        assert_eq!(sanitize_agent_component(&long_id), expected_long);
        // 边界钉死：`..`/`/`/绝对路径/unicode/空白串全部收敛为哈希分量（不以输入原样出现）；
        // 空白串 trim 后为空 → 哈希的即 offset 基值；恰好 128 字节仍原样、129 字节走哈希
        for bad in ["..", "/", "/etc/passwd", "café", "   "] {
            let got = sanitize_agent_component(bad);
            assert_eq!(
                got.len(),
                3 + 16,
                "应收敛为 ag-<16hex> 哈希分量: {bad} -> {got}"
            );
            assert!(
                got.starts_with("ag-"),
                "应收敛为 ag-<16hex> 哈希分量: {bad} -> {got}"
            );
        }
        assert_eq!(
            sanitize_agent_component("   "),
            format!("ag-{:016x}", 0xcbf29ce484222325u64)
        );
        let id_128 = "a".repeat(128);
        assert_eq!(sanitize_agent_component(&id_128), id_128);
        let id_129 = "a".repeat(129);
        assert_eq!(
            sanitize_agent_component(&id_129),
            format!("ag-{:016x}", fnv1a64(&id_129))
        );

        // e2e 层：非法 agent_id 落哈希分片，未逃出 temp home，且与会话级计数互相隔离
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_evil_agent_{rand_id}"));
        let sess_id = "evil_agent_sess";

        let mk_agent_payload = |agent: &str| {
            serde_json::json!({
                "session_id": sess_id,
                "agent_id": agent,
                "tool_name": "grep",
                "tool_input": {}
            })
            .to_string()
        };
        let payload_evil = mk_agent_payload("../evil");
        let payload_long = mk_agent_payload(&long_id);
        let payload_flat = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 穿越型 agent_id：3 次 grep 触发自己分片内的 deny
        for (i, now) in [100.0, 101.0, 102.0].iter().enumerate() {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_evil.as_bytes(),
                &mut out,
                &mut err,
                Some(*now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            if i < 2 {
                assert!(out.is_empty());
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("\"permissionDecision\":\"deny\""));

        // 超长 agent_id：独立分片，3 次 grep 第 3 次触发自己分片内的 deny
        // （若分片失效退化为共享 counter，evil 的 deny@102 静默窗 [102,117) 会把这里全部消音，
        //   该 deny 断言即可区分"真隔离"与"被共享静默窗消音"）
        for (i, now) in [103.0, 104.0, 105.0].iter().enumerate() {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_long.as_bytes(),
                &mut out,
                &mut err,
                Some(*now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            if i < 2 {
                assert!(out.is_empty());
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("\"permissionDecision\":\"deny\""));

        // 会话级（无 agent_id）第 1 次 grep 不受穿越型/超长 agent 的 deny 静默窗影响
        out.clear();
        let code_flat = run_remind_impl(
            "claude-code",
            payload_flat.as_bytes(),
            &mut out,
            &mut err,
            Some(106.0),
            Some(&temp_home),
        );
        assert_eq!(code_flat, 0);
        assert!(out.is_empty());

        // 落盘断言：两个哈希分片 + 会话级平面 counter 各自存在
        let sess_dir = get_hook_data_dir(&temp_home, sess_id, None).unwrap();
        assert!(sess_dir.join(&expected_evil).join("counter.json").exists());
        assert!(sess_dir.join(&expected_long).join("counter.json").exists());
        assert!(sess_dir.join("counter.json").exists());

        // 未逃出 temp home："../evil" 未在 hook_data 层穿出会话目录
        assert!(!temp_home
            .join(".astrolabe")
            .join("hook_data")
            .join("evil")
            .exists());
        let hook_data_entries: Vec<_> =
            std::fs::read_dir(temp_home.join(".astrolabe").join("hook_data"))
                .unwrap()
                .collect();
        assert_eq!(
            hook_data_entries.len(),
            1,
            "hook_data 下应只有会话目录一个条目"
        );

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_agent_id_extraction_type_and_field_edges() {
        // 提取层边界：空串/空白/Bool/Null → 会话级平面路径；Number → 分片；agent_id 优先于 agentId
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_agent_edge_{rand_id}"));
        let mut err = Vec::new();

        let flat_cases: Vec<(&str, &str)> = vec![
            (
                "edge_empty",
                r#"{"session_id":"edge_empty","agent_id":"","tool_name":"grep","tool_input":{}}"#,
            ),
            (
                "edge_blank",
                r#"{"session_id":"edge_blank","agent_id":"   ","tool_name":"grep","tool_input":{}}"#,
            ),
            (
                "edge_bool",
                r#"{"session_id":"edge_bool","agent_id":true,"tool_name":"grep","tool_input":{}}"#,
            ),
            (
                "edge_null",
                r#"{"session_id":"edge_null","agent_id":null,"tool_name":"grep","tool_input":{}}"#,
            ),
        ];
        for (sess, payload) in &flat_cases {
            let mut out = Vec::new();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0),
                Some(&temp_home),
            );
            assert_eq!(code, 0, "{sess} 应正常处理");
            let sess_dir = get_hook_data_dir(&temp_home, sess, None).unwrap();
            assert!(
                sess_dir.join("counter.json").exists(),
                "{sess} 应落会话级平面 counter"
            );
            let entries: Vec<_> = std::fs::read_dir(&sess_dir)
                .unwrap()
                .filter(|e| {
                    // flock 锁文件 counter.json.lock 常驻（删除会引入竞态），不算状态条目
                    e.as_ref()
                        .map(|e| e.file_name() != "counter.json.lock")
                        .unwrap_or(true)
                })
                .collect();
            assert_eq!(
                entries.len(),
                1,
                "{sess} 会话目录应只有 counter.json 一个条目"
            );
        }

        // Number 类型 agent_id → 转 string 后作为合法分片分量
        let mut out = Vec::new();
        let payload_num =
            r#"{"session_id":"edge_number","agent_id":12345,"tool_name":"grep","tool_input":{}}"#;
        let code = run_remind_impl(
            "claude-code",
            payload_num.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0);
        assert!(get_hook_data_dir(&temp_home, "edge_number", None)
            .unwrap()
            .join("12345")
            .join("counter.json")
            .exists());

        // agent_id 与 agentId 同时出现 → snake_case 优先（与 session_id 提取同构）
        out.clear();
        let payload_both = r#"{"session_id":"edge_both","agent_id":"snake-x","agentId":"camel-y","tool_name":"grep","tool_input":{}}"#;
        let code = run_remind_impl(
            "claude-code",
            payload_both.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0);
        let both_dir = get_hook_data_dir(&temp_home, "edge_both", None).unwrap();
        assert!(
            both_dir.join("snake-x").join("counter.json").exists(),
            "agent_id 应优先于 agentId"
        );
        assert!(!both_dir.join("camel-y").exists());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_session_id_fallback_skips_invalid_placeholder() {
        // null/空白占位键应被跳过继续找后续键（而非取"第一个存在的键"后解析失败 exit 2）
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_id_fallback_{rand_id}"));

        // session_id=null 占位 + conversation_id 合法；agent_id 空串占位 + agentId 合法
        let payload = serde_json::json!({
            "session_id": null,
            "conversation_id": "conv-abc",
            "agent_id": "",
            "agentId": "camel-1",
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "cursor",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0, "应跳过占位键用 conversation_id，而非 exit 2");
        // 计数落盘：会话目录 conv-abc、分片 camel-1（agent_id 空串被跳过）
        let sess_dir = get_hook_data_dir(&temp_home, "conv-abc", None).unwrap();
        assert!(sess_dir.join("camel-1").join("counter.json").exists());
        assert!(!sess_dir.join("counter.json").exists());

        // cleanup 同构：conversation_id 可清理
        let clean_payload = serde_json::json!({ "conversation_id": "conv-abc" }).to_string();
        let mut clean_err = Vec::new();
        let clean_code =
            run_cleanup_impl(clean_payload.as_bytes(), &mut clean_err, Some(&temp_home));
        assert_eq!(clean_code, 0);
        assert!(!sess_dir.exists());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_cursor_native_read_path_field_classification() {
        // Cursor 原生 Read 的 tool_input.path 提取：非代码文件（README.md）不计 read 滥用，
        // 代码文件（.rs）正常累计并在第 3 次 deny（输出为 Cursor 原生扁平格式）
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cursor_path_{rand_id}"));
        let mut err = Vec::new();

        let mk_read_payload = |sess: &str, path: &str| {
            serde_json::json!({
                "conversation_id": sess,
                "tool_name": "Read",
                "tool_input": { "path": path }
            })
            .to_string()
        };

        // 会话 A：README.md ×3 → 非代码文件，不触发 Read deny
        let payload_readme = mk_read_payload("cursor-path-a", "README.md");
        for (i, now) in [100.0, 101.0, 102.0].iter().enumerate() {
            let mut out = Vec::new();
            let code = run_remind_impl(
                "cursor",
                payload_readme.as_bytes(),
                &mut out,
                &mut err,
                Some(*now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "README 非代码文件，第 {} 次不应 deny（path 未提取时会误走保守分支）",
                i + 1
            );
        }

        // 会话 B：src/main.rs ×3 → 代码文件，第 3 次 deny 且为 Cursor 扁平格式
        let payload_code = mk_read_payload("cursor-path-b", "src/main.rs");
        let mut out = Vec::new();
        for (i, now) in [100.0, 101.0, 102.0].iter().enumerate() {
            out.clear();
            let code = run_remind_impl(
                "cursor",
                payload_code.as_bytes(),
                &mut out,
                &mut err,
                Some(*now),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            if i < 2 {
                assert!(out.is_empty());
            }
        }
        let denied = String::from_utf8_lossy(&out);
        assert!(denied.contains("\"permission\":\"deny\""));
        assert!(denied.contains("user_message"));
        assert!(denied.contains("agent_message"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_cleanup_removes_nested_agent_shard_dirs() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_agent_cleanup_{rand_id}"));
        let sess_id = "agent_cleanup_sess";

        // 先带 agent_id remind 造出嵌套分片状态
        let payload = serde_json::json!({
            "session_id": sess_id,
            "agent_id": "agent-x",
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0);
        assert!(out.is_empty());

        let sess_dir = get_hook_data_dir(&temp_home, sess_id, None).unwrap();
        assert!(sess_dir.join("agent-x").join("counter.json").exists());

        // SessionEnd cleanup 只带 session_id：整个会话目录（含 agent 分片）递归删除
        let clean_payload = serde_json::json!({ "session_id": sess_id }).to_string();
        let mut clean_err = Vec::new();
        let clean_code =
            run_cleanup_impl(clean_payload.as_bytes(), &mut clean_err, Some(&temp_home));
        assert_eq!(clean_code, 0);
        assert!(!sess_dir.exists());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_client_from_str_new_client_aliases() {
        // KimiCode（Moonshot Kimi Code CLI，协议 CC 同构）
        for s in [
            "kimicode",
            "kimi-code",
            "kimi_code",
            "kimi",
            "KimiCode",
            "KIMI",
            "  Kimi-Code  ",
        ] {
            assert_eq!(Client::from_str(s), Client::KimiCode, "input: {s:?}");
        }
        // ZCode（智谱 Z.ai ZCode）
        for s in ["zcode", "z-code", "zai", "ZCode", "ZAI", " z-code "] {
            assert_eq!(Client::from_str(s), Client::ZCode, "input: {s:?}");
        }
        // Cursor（Cursor CLI/Agent）
        for s in [
            "cursor",
            "cursor-agent",
            "cursor_agent",
            "cursor-cli",
            "Cursor",
            "Cursor-Agent",
            " cursor-cli ",
        ] {
            assert_eq!(Client::from_str(s), Client::Cursor, "input: {s:?}");
        }
        // OpenCode（插件桥接调用）
        for s in ["opencode", "OpenCode", "OPENCODE", " opencode "] {
            assert_eq!(Client::from_str(s), Client::OpenCode, "input: {s:?}");
        }
        // 未知名仍回落 Other；带空格的 "kimi code" 不在别名表内
        assert_eq!(Client::from_str("unknown-client"), Client::Other);
        assert_eq!(Client::from_str("kimi code"), Client::Other);
    }

    #[test]
    fn test_classify_cursor_opencode_kimi_zcode_cc_isomorphic() {
        // Cursor：命令工具名是 "Shell"，靠 command 内容识别 grep/cat 等
        assert_eq!(
            classify_tool(
                "shell",
                Client::Cursor,
                None,
                Some("grep"),
                Some("-rn foo src/"),
                None,
                None
            ),
            ToolKind::Grep
        );
        // Cursor："Read" 无 limit → Read（代码文件），非切片
        assert_eq!(
            classify_tool(
                "read",
                Client::Cursor,
                Some("src/main.rs"),
                None,
                None,
                None,
                None
            ),
            ToolKind::Read { is_code_file: true }
        );
        // Cursor：原生 grep 工具名直接命中
        assert_eq!(
            classify_tool("grep", Client::Cursor, None, None, None, None, None),
            ToolKind::Grep
        );

        // OpenCode：read / grep 工具名同理
        assert_eq!(
            classify_tool(
                "read",
                Client::OpenCode,
                Some("src/main.rs"),
                None,
                None,
                None,
                None
            ),
            ToolKind::Read { is_code_file: true }
        );
        assert_eq!(
            classify_tool("grep", Client::OpenCode, None, None, None, None, None),
            ToolKind::Grep
        );

        // KimiCode / ZCode 与 CC 同臂
        assert_eq!(
            classify_tool("grep", Client::KimiCode, None, None, None, None, None),
            ToolKind::Grep
        );
        assert_eq!(
            classify_tool(
                "read",
                Client::ZCode,
                Some("src/main.rs"),
                None,
                None,
                None,
                None
            ),
            ToolKind::Read { is_code_file: true }
        );

        // is_read_code_file_call：四家并入 CC 的 shell 路径参数判定臂
        for c in [
            Client::KimiCode,
            Client::ZCode,
            Client::Cursor,
            Client::OpenCode,
        ] {
            assert!(is_read_code_file_call(true, None, c, Some("src/a.rs")));
            assert!(!is_read_code_file_call(true, None, c, Some("README.md")));
        }
    }

    #[test]
    fn test_classify_grok_terminal_command_tool_names() {
        // Grok Build：命令工具名是 run_terminal_command（snake_case），靠 command 内容识别 grep
        assert_eq!(
            classify_tool(
                "run_terminal_command",
                Client::Grok,
                None,
                Some("grep"),
                Some("pattern lib.rs"),
                None,
                None
            ),
            ToolKind::Grep
        );
        // Grok Build：read_file 无 limit → Read 分支（现有逻辑已覆盖，钉死防回归）
        assert_eq!(
            classify_tool(
                "read_file",
                Client::Grok,
                Some("src/lib.rs"),
                None,
                None,
                None,
                None
            ),
            ToolKind::Read { is_code_file: true }
        );
    }

    #[test]
    fn test_build_output_new_client_formats() {
        // Cursor：原生扁平格式，三键齐备且无 hookSpecificOutput；
        // Cursor 不映射 additionalContext，长引导文案必须放 agent_message
        let cursor_out = build_output(Client::Cursor, DenyKind::Grep);
        let cursor_val: serde_json::Value = serde_json::from_str(&cursor_out).unwrap();
        assert_eq!(cursor_val["permission"], "deny");
        assert!(cursor_val["user_message"].is_string());
        assert!(cursor_val["agent_message"].is_string());
        assert!(cursor_val.get("hookSpecificOutput").is_none());
        let agent_msg = cursor_val["agent_message"].as_str().unwrap();
        let user_msg = cursor_val["user_message"].as_str().unwrap();
        assert!(
            agent_msg.len() > user_msg.len(),
            "agent_message 应承载长引导文案: agent={agent_msg} user={user_msg}"
        );
        assert!(agent_msg.contains("read-only"));

        // OpenCode：与 Grok 同臂的扁平 decision/reason
        let oc_out = build_output(Client::OpenCode, DenyKind::Read);
        let oc_val: serde_json::Value = serde_json::from_str(&oc_out).unwrap();
        assert_eq!(oc_val["decision"], "deny");
        assert!(oc_val["reason"].is_string());
        assert!(oc_val.get("hookSpecificOutput").is_none());
        assert!(oc_val.get("permission").is_none());

        // KimiCode / ZCode：与 CC 同臂的三字段标准输出
        for (name, client) in [("kimicode", Client::KimiCode), ("zcode", Client::ZCode)] {
            let out = build_output(client, DenyKind::Mixed);
            let val: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                val["hookSpecificOutput"]["hookEventName"], "PreToolUse",
                "{name}"
            );
            assert_eq!(
                val["hookSpecificOutput"]["permissionDecision"], "deny",
                "{name}"
            );
            assert!(
                val["hookSpecificOutput"]["permissionDecisionReason"].is_string(),
                "{name}"
            );
            assert!(
                val["hookSpecificOutput"]["additionalContext"].is_string(),
                "{name}"
            );
        }
    }

    #[test]
    fn test_conversation_id_session_fallback_remind_and_cleanup() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_conv_id_{rand_id}"));

        // String 型 conversation_id（Cursor 原生形态）：remind 正常计数，写盘路径用该 id
        // （修复前此处 exit 2，Cursor 语义下等于 deny 一切工具调用）
        let payload_str = serde_json::json!({
            "conversation_id": "conv_str_sess",
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "cursor",
            payload_str.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code, 0);
        assert!(out.is_empty());
        assert!(get_hook_data_dir(&temp_home, "conv_str_sess", None)
            .unwrap()
            .join("counter.json")
            .exists());

        // Number 型 conversationId（驼峰）：转字符串后同样落盘
        let payload_num = serde_json::json!({
            "conversationId": 98765,
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();
        out.clear();
        let code_num = run_remind_impl(
            "cursor",
            payload_num.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(code_num, 0);
        assert!(out.is_empty());
        assert!(get_hook_data_dir(&temp_home, "98765", None)
            .unwrap()
            .join("counter.json")
            .exists());

        // cleanup 按 conversation_id 清理（幂等）
        let clean_payload = serde_json::json!({ "conversation_id": "conv_str_sess" }).to_string();
        let mut clean_err = Vec::new();
        let clean_code =
            run_cleanup_impl(clean_payload.as_bytes(), &mut clean_err, Some(&temp_home));
        assert_eq!(clean_code, 0);
        assert!(!get_hook_data_dir(&temp_home, "conv_str_sess", None)
            .unwrap()
            .exists());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_cursor_shell_tool_e2e_flat_deny() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cursor_e2e_{rand_id}"));

        // Cursor 原生 payload：只带 conversation_id + Shell 工具 + command 含 grep
        let payload = serde_json::json!({
            "conversation_id": "cursor_shell_sess",
            "tool_name": "Shell",
            "tool_input": { "command": "grep -rn foo src/" }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "cursor",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次 -> Cursor 扁平 deny：permission / user_message / agent_message
        out.clear();
        let code3 = run_remind_impl(
            "cursor",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let val: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&out).trim()).unwrap();
        assert_eq!(val["permission"], "deny");
        assert!(val["user_message"].is_string());
        assert!(val["agent_message"].is_string());
        assert!(val.get("hookSpecificOutput").is_none());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_opencode_e2e_flat_decision_deny() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_oc_e2e_{rand_id}"));

        // OpenCode：grep 工具 3 次触发与 Grok 同臂的扁平 decision deny
        let payload = serde_json::json!({
            "session_id": "oc_flat_sess",
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "opencode",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code3 = run_remind_impl(
            "opencode",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let val: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&out).trim()).unwrap();
        assert_eq!(val["decision"], "deny");
        assert!(val["reason"].is_string());
        assert!(val.get("hookSpecificOutput").is_none());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    struct GcTmp(PathBuf);
    impl GcTmp {
        fn new(tag: &str) -> Self {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("astrolabe_gc_{tag}_{n}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for GcTmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn set_tree_mtime(path: &Path, t: std::time::SystemTime) {
        for e in std::fs::read_dir(path).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                set_tree_mtime(&p, t);
            } else {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&p)
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
        }
    }

    #[test]
    fn test_gc_removes_only_stale_sessions_and_keeps_current() {
        use std::time::{Duration, SystemTime};
        let tmp = GcTmp::new("a");
        let home = tmp.path();
        let root = home.join(".astrolabe").join("hook_data");
        for sid in ["old", "fresh", "keepme", "old_with_fresh_agent"] {
            std::fs::create_dir_all(root.join(sid).join("agent")).unwrap();
            std::fs::write(root.join(sid).join("counter.json"), b"{}").unwrap();
            std::fs::write(root.join(sid).join("agent").join("counter.json"), b"{}").unwrap();
        }
        let now = SystemTime::now();
        let old = now - Duration::from_secs(48 * 3600);
        set_tree_mtime(&root.join("old"), old);
        set_tree_mtime(&root.join("keepme"), old);
        // 会话根旧、子代理分片新：整体仍视为活跃
        set_tree_mtime(&root.join("old_with_fresh_agent"), old);
        std::fs::write(
            root.join("old_with_fresh_agent")
                .join("agent")
                .join("counter.json"),
            b"{}",
        )
        .unwrap();
        // hook_data 根下的散文件不被当作会话目录
        std::fs::write(root.join("stray.txt"), b"x").unwrap();

        let n =
            gc_stale_sessions(home, now, Duration::from_secs(24 * 3600), Some("keepme")).unwrap();
        assert_eq!(n, 1);
        assert!(!root.join("old").exists());
        assert!(root.join("fresh").exists());
        assert!(root.join("keepme").exists());
        assert!(root.join("old_with_fresh_agent").exists());
        assert!(root.join("stray.txt").exists());
    }

    #[test]
    fn test_gc_missing_hook_data_is_ok() {
        let tmp = GcTmp::new("b");
        let n = gc_stale_sessions(
            tmp.path(),
            std::time::SystemTime::now(),
            std::time::Duration::from_secs(3600),
            None,
        )
        .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_run_gc_impl_reports_and_removes() {
        use std::time::{Duration, SystemTime};
        let tmp = GcTmp::new("c");
        let root = tmp.path().join(".astrolabe").join("hook_data");
        std::fs::create_dir_all(root.join("s1")).unwrap();
        std::fs::write(root.join("s1").join("counter.json"), b"{}").unwrap();
        let now = SystemTime::now() + Duration::from_secs(25 * 3600);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_gc_impl(&mut out, &mut err, now, 24, Some(tmp.path()));
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        assert!(String::from_utf8_lossy(&out).contains("removed 1"));
        assert!(!root.join("s1").exists());
    }

    #[test]
    fn test_opportunistic_gc_throttled_by_stamp_and_disabled_by_zero() {
        use std::time::{Duration, SystemTime};
        let tmp = GcTmp::new("d");
        let home = tmp.path();
        let root = home.join(".astrolabe").join("hook_data");
        let mk = |sid: &str| {
            std::fs::create_dir_all(root.join(sid)).unwrap();
            std::fs::write(root.join(sid).join("counter.json"), b"{}").unwrap();
        };
        let now = SystemTime::now() + Duration::from_secs(48 * 3600);
        mk("a");
        // 0 小时 = 禁用
        maybe_gc_opportunistic(home, now, "cur", 0);
        assert!(root.join("a").exists());
        assert!(!home.join(".astrolabe").join(GC_STAMP_FILE).exists());
        // 首次运行：清理并写戳
        maybe_gc_opportunistic(home, now, "cur", 24);
        assert!(!root.join("a").exists());
        assert!(home.join(".astrolabe").join(GC_STAMP_FILE).exists());
        // 戳仍新鲜（相对真实时钟）：在 now=真实时间 下再次调用应被节流
        mk("b");
        set_tree_mtime(
            &root.join("b"),
            SystemTime::now() - Duration::from_secs(48 * 3600),
        );
        maybe_gc_opportunistic(home, SystemTime::now(), "cur", 24);
        assert!(root.join("b").exists(), "节流窗口内不应扫描");
    }

    #[test]
    fn deny_text_search_env_bool_parsing() {
        assert_eq!(parse_env_bool(Some("true")), Some(true));
        assert_eq!(parse_env_bool(Some("1")), Some(true));
        assert_eq!(parse_env_bool(Some("yes")), Some(true));
        assert_eq!(parse_env_bool(Some("ON")), Some(true));
        assert_eq!(parse_env_bool(Some("false")), Some(false));
        assert_eq!(parse_env_bool(Some("0")), Some(false));
        assert_eq!(parse_env_bool(Some("off")), Some(false));
        assert_eq!(parse_env_bool(None), None);
        assert_eq!(parse_env_bool(Some("")), None);
        assert_eq!(parse_env_bool(Some("maybe")), None);
    }

    #[test]
    fn deny_text_search_precedence_cli_over_env_over_context() {
        assert!(resolve_deny_text_search(
            Some(true),
            Some("false"),
            Some(false)
        ));
        assert!(!resolve_deny_text_search(
            Some(false),
            Some("true"),
            Some(true)
        ));
        assert!(resolve_deny_text_search(None, Some("true"), Some(false)));
        assert!(!resolve_deny_text_search(None, Some("false"), Some(true)));
        assert!(resolve_deny_text_search(None, None, Some(true)));
        assert!(!resolve_deny_text_search(None, None, None));
        assert!(!resolve_deny_text_search(None, None, Some(false)));
    }

    #[test]
    fn command_contains_text_search_token_boundaries() {
        assert!(command_contains_text_search("rg foo"));
        assert!(command_contains_text_search("grep -n bar src/a.rs"));
        assert!(command_contains_text_search("egrep pattern file"));
        assert!(command_contains_text_search("zgrep x y.gz"));
        assert!(command_contains_text_search("cd src && rg foo"));
        assert!(command_contains_text_search("git grep foo"));
        assert!(command_contains_text_search("git --no-pager grep foo"));
        assert!(command_contains_text_search("git -C repo grep foo"));
        assert!(!command_contains_text_search("ls"));
        assert!(!command_contains_text_search("cargo test"));
        assert!(!command_contains_text_search("cat src/grep_utils.rs"));
        assert!(!command_contains_text_search("echo grepping"));
        assert!(!command_contains_text_search("git status"));
        assert!(!command_contains_text_search("git commit -m 'grep later'"));
    }

    #[test]
    fn classify_glob_only_when_deny_text_search() {
        let soft = classify_tool_ex(
            "glob",
            Client::ClaudeCode,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
        );
        assert_eq!(soft, ToolKind::Neutral);
        let hard = classify_tool_ex(
            "Glob",
            Client::ClaudeCode,
            None,
            None,
            None,
            None,
            None,
            None,
            true,
        );
        assert_eq!(hard, ToolKind::Glob);
        let fs = classify_tool_ex(
            "file_search",
            Client::Cursor,
            None,
            None,
            None,
            None,
            None,
            None,
            true,
        );
        assert_eq!(fs, ToolKind::Glob);
    }

    #[test]
    fn classify_git_grep_and_pipeline_only_when_hard() {
        let soft = classify_tool_ex(
            "bash",
            Client::ClaudeCode,
            None,
            Some("git"),
            Some("grep foo"),
            Some("git grep foo"),
            None,
            None,
            false,
        );
        assert_eq!(soft, ToolKind::Neutral);
        let hard = classify_tool_ex(
            "bash",
            Client::ClaudeCode,
            None,
            Some("git"),
            Some("grep foo"),
            Some("git grep foo"),
            None,
            None,
            true,
        );
        assert_eq!(hard, ToolKind::Grep);
        let pipe = classify_tool_ex(
            "shell",
            Client::Cursor,
            None,
            Some("cd"),
            Some("src && rg foo"),
            Some("cd src && rg foo"),
            None,
            None,
            true,
        );
        assert_eq!(pipe, ToolKind::Grep);
    }

    #[test]
    fn decide_hard_deny_first_grep_and_glob() {
        let cfg = HooksConfig {
            deny_text_search: true,
            ..Default::default()
        };
        let mut counter = CounterState::default();
        let d = decide_with(&mut counter, ToolKind::Grep, 100.0, &cfg);
        assert_eq!(d, Decision::Deny(DenyKind::TextSearch));
        assert_eq!(counter.last_deny_ts, Some(100.0));

        let mut counter2 = CounterState::default();
        let d2 = decide_with(&mut counter2, ToolKind::Glob, 200.0, &cfg);
        assert_eq!(d2, Decision::Deny(DenyKind::Glob));
    }

    #[test]
    fn decide_soft_mode_unchanged_needs_three_greps() {
        let cfg = HooksConfig::default();
        assert!(!cfg.deny_text_search);
        let mut counter = CounterState::default();
        assert_eq!(
            decide_with(&mut counter, ToolKind::Grep, 1.0, &cfg),
            Decision::Allow
        );
        assert_eq!(
            decide_with(&mut counter, ToolKind::Grep, 2.0, &cfg),
            Decision::Allow
        );
        assert_eq!(
            decide_with(&mut counter, ToolKind::Grep, 3.0, &cfg),
            Decision::Deny(DenyKind::Grep)
        );
    }

    #[test]
    fn decide_hard_allows_non_search_bash_kinds() {
        let cfg = HooksConfig {
            deny_text_search: true,
            ..Default::default()
        };
        let mut counter = CounterState::default();
        assert_eq!(
            decide_with(&mut counter, ToolKind::Neutral, 1.0, &cfg),
            Decision::NeutralAllow
        );
        assert_eq!(
            decide_with(
                &mut counter,
                ToolKind::Read { is_code_file: true },
                2.0,
                &cfg
            ),
            Decision::Allow
        );
    }

    #[test]
    fn build_output_hard_text_search_and_glob() {
        let out = build_output(Client::ClaudeCode, DenyKind::TextSearch);
        assert!(out.contains("ASTROLABE_DENY_TEXT_SEARCH"), "{out}");
        assert!(out.contains("search_code"), "{out}");
        assert!(out.contains("\"permissionDecision\":\"deny\""));
        let glob = build_output(Client::ClaudeCode, DenyKind::Glob);
        assert!(glob.contains("Glob"), "{glob}");
        assert!(glob.contains("search_code"), "{glob}");
    }

    #[test]
    fn run_remind_hard_denies_first_grep_rg_git_grep_glob() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_deny_ts_{rand_id}"));
        let cfg = HooksConfig {
            deny_text_search: true,
            ..Default::default()
        };
        let cases: &[(&str, serde_json::Value, &str)] = &[
            (
                "sess_grep",
                serde_json::json!({
                    "session_id": "sess_grep",
                    "tool_name": "Grep",
                    "tool_input": { "pattern": "foo" }
                }),
                "ASTROLABE_DENY_TEXT_SEARCH",
            ),
            (
                "sess_rg",
                serde_json::json!({
                    "session_id": "sess_rg",
                    "tool_name": "Bash",
                    "tool_input": { "command": "rg foo src" }
                }),
                "ASTROLABE_DENY_TEXT_SEARCH",
            ),
            (
                "sess_git_grep",
                serde_json::json!({
                    "session_id": "sess_git_grep",
                    "tool_name": "Bash",
                    "tool_input": { "command": "git grep foo" }
                }),
                "ASTROLABE_DENY_TEXT_SEARCH",
            ),
            (
                "sess_glob",
                serde_json::json!({
                    "session_id": "sess_glob",
                    "tool_name": "Glob",
                    "tool_input": { "pattern": "**/*.rs" }
                }),
                "Glob",
            ),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        for (i, (_sid, payload, needle)) in cases.iter().enumerate() {
            out.clear();
            let payload_str = payload.to_string();
            let code = run_remind_impl_with(
                "claude-code",
                payload_str.as_bytes(),
                &mut out,
                &mut err,
                Some(1000.0 + i as f64),
                Some(&temp_home),
                &cfg,
            );
            assert_eq!(code, 0, "case {payload_str}");
            let s = String::from_utf8_lossy(&out);
            assert!(
                s.contains("\"permissionDecision\":\"deny\""),
                "expected deny for {payload_str}: {s}"
            );
            assert!(s.contains(needle), "missing {needle} in {s}");
        }
        let allow_payload = serde_json::json!({
            "session_id": "sess_ls",
            "tool_name": "Bash",
            "tool_input": { "command": "ls -la" }
        })
        .to_string();
        out.clear();
        let code = run_remind_impl_with(
            "claude-code",
            allow_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(2000.0),
            Some(&temp_home),
            &cfg,
        );
        assert_eq!(code, 0);
        assert!(
            out.is_empty(),
            "ls should be allowed: {}",
            String::from_utf8_lossy(&out)
        );

        let cargo = serde_json::json!({
            "session_id": "sess_cargo",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test -p astrolabe-mcp" }
        })
        .to_string();
        out.clear();
        let code = run_remind_impl_with(
            "claude-code",
            cargo.as_bytes(),
            &mut out,
            &mut err,
            Some(2001.0),
            Some(&temp_home),
            &cfg,
        );
        assert_eq!(code, 0);
        assert!(out.is_empty());

        let soft = HooksConfig::default();
        let soft_payload = serde_json::json!({
            "session_id": "sess_soft_grep",
            "tool_name": "Grep",
            "tool_input": { "pattern": "foo" }
        })
        .to_string();
        out.clear();
        let code = run_remind_impl_with(
            "claude-code",
            soft_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(3000.0),
            Some(&temp_home),
            &soft,
        );
        assert_eq!(code, 0);
        assert!(out.is_empty(), "soft first grep must allow");

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn grep_threshold_env_tunable_via_decide_with() {
        let cfg = HooksConfig {
            grep_threshold: 1,
            ..Default::default()
        };
        let mut counter = CounterState::default();
        assert_eq!(
            decide_with(&mut counter, ToolKind::Grep, 1.0, &cfg),
            Decision::Deny(DenyKind::Grep)
        );
    }

    #[test]
    fn h1_hard_mode_ignores_silence_window() {
        let cfg = HooksConfig {
            deny_text_search: true,
            ..Default::default()
        };
        let mut counter = CounterState::default();
        assert_eq!(
            decide_with(&mut counter, ToolKind::Grep, 100.0, &cfg),
            Decision::Deny(DenyKind::TextSearch)
        );
        // 紧接着（静默窗内）重试仍必须被拦
        assert_eq!(
            decide_with(&mut counter, ToolKind::Grep, 101.0, &cfg),
            Decision::Deny(DenyKind::TextSearch)
        );
        assert_eq!(
            decide_with(&mut counter, ToolKind::Glob, 102.0, &cfg),
            Decision::Deny(DenyKind::Glob)
        );
        // 软模式的静默窗语义不变
        let soft = HooksConfig::default();
        let mut c2 = CounterState {
            last_deny_ts: Some(100.0),
            ..Default::default()
        };
        assert_eq!(
            decide_with(&mut c2, ToolKind::Grep, 101.0, &soft),
            Decision::Silenced
        );
    }

    #[test]
    fn h4_leftover_lock_file_does_not_block() {
        let dir = std::env::temp_dir().join(format!("astro-h4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("counter.json");
        // 模拟崩溃遗留的锁文件（没有任何进程持有 flock）
        std::fs::write(dir.join("counter.json.lock"), b"").unwrap();
        let t = std::time::Instant::now();
        let g = CounterFileLock::acquire(&counter);
        assert!(g.is_some(), "stale lock file must not prevent locking");
        assert!(
            t.elapsed() < Duration::from_millis(300),
            "{:?}",
            t.elapsed()
        );
        // 持有期间别人拿不到
        let g2 = CounterFileLock::acquire(&counter);
        assert!(g2.is_none());
        drop(g);
        assert!(CounterFileLock::acquire(&counter).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn h5_home_falls_back_to_userprofile() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(kk, _)| *kk == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            home_from_lookup(env(&[("HOME", "/h"), ("USERPROFILE", "C:\\u")])),
            Some(PathBuf::from("/h"))
        );
        assert_eq!(
            home_from_lookup(env(&[("USERPROFILE", "C:\\Users\\a")])),
            Some(PathBuf::from("C:\\Users\\a"))
        );
        assert_eq!(
            home_from_lookup(env(&[("HOME", "  "), ("USERPROFILE", "C:\\x")])),
            Some(PathBuf::from("C:\\x"))
        );
        assert_eq!(
            home_from_lookup(env(&[("HOMEDRIVE", "C:"), ("HOMEPATH", "\\Users\\b")])),
            Some(PathBuf::from("C:\\Users\\b"))
        );
        assert_eq!(home_from_lookup(env(&[])), None);
    }

    #[test]
    fn h6_soft_mode_command_position_detection() {
        let name = |c: &str| effective_shell_command(c).map(|(n, _)| n);
        assert_eq!(name("grep -rn foo src").as_deref(), Some("grep"));
        assert_eq!(name("cd src && grep -rn foo .").as_deref(), Some("grep"));
        assert_eq!(name("FOO=1 grep foo .").as_deref(), Some("grep"));
        assert_eq!(name("sudo grep foo .").as_deref(), Some("grep"));
        assert_eq!(name("sudo -u bob grep foo .").as_deref(), Some("grep"));
        assert_eq!(name("bash -c \"grep foo .\"").as_deref(), Some("grep"));
        assert_eq!(name("sh -lc 'cd x; rg foo'").as_deref(), Some("rg"));
        assert_eq!(name("rg.exe foo").as_deref(), Some("rg"));
        assert_eq!(
            name("\"C:\\Program Files\\Git\\usr\\bin\\grep.exe\" foo .").as_deref(),
            Some("grep")
        );
        assert_eq!(name("findstr /s foo *.rs").as_deref(), Some("findstr"));
        assert_eq!(
            name("Select-String -Pattern foo a.rs").as_deref(),
            Some("select-string")
        );
        assert_eq!(name("xargs grep foo").as_deref(), Some("grep"));
        // 管道后续段不算（软模式只在硬模式拦管道）
        assert_eq!(name("cat a.rs | grep foo").as_deref(), Some("cat"));
        // 参数不带分隔符尾巴
        let (_, args) = effective_shell_command("cat a.rs;").unwrap();
        assert_eq!(args.as_deref(), Some("a.rs"));
        let (_, args) = effective_shell_command("cat a.rs|head").unwrap();
        assert_eq!(args.as_deref(), Some("a.rs"));
        // 重定向里的 & 不是分隔符
        let (n, args) = effective_shell_command("cat a.rs 2>&1").unwrap();
        assert_eq!(n, "cat");
        assert_eq!(args.as_deref(), Some("a.rs 2>&1"));
        // 单 token 命令
        assert_eq!(name("ls").as_deref(), Some("ls"));
    }

    #[test]
    fn h6_soft_mode_end_to_end_denies_on_third() {
        let cmds = [
            "cd src && grep -rn foo .",
            "sudo grep -rn foo .",
            "bash -c \"grep -rn foo .\"",
            "rg.exe foo",
            "findstr /s foo *.rs",
            "Select-String -Pattern foo a.rs",
        ];
        for (i, c) in cmds.iter().enumerate() {
            let tmp = std::env::temp_dir().join(format!("astro-h6-{}-{i}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).unwrap();
            let payload = serde_json::json!({
                "session_id": "h6",
                "tool_name": "Bash",
                "tool_input": {"command": c},
            })
            .to_string();
            let mut denied_at = None;
            for n in 0..3 {
                let mut out = Vec::new();
                let mut err = Vec::new();
                let code = run_remind_impl_with(
                    "claude-code",
                    payload.as_bytes(),
                    &mut out,
                    &mut err,
                    Some(1000.0 + n as f64),
                    Some(&tmp),
                    &HooksConfig::default(),
                );
                assert_eq!(code, 0);
                if !out.is_empty() && denied_at.is_none() {
                    denied_at = Some(n);
                }
            }
            assert_eq!(denied_at, Some(2), "cmd={c}");
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    #[test]
    fn h7_hard_mode_no_false_positive_and_catches_wrappers() {
        // 误伤
        assert!(!command_contains_text_search("echo grep is great"));
        assert!(!command_contains_text_search(
            "git commit -m \"fix grep bug\""
        ));
        assert!(!command_contains_text_search("npm run grep-check"));
        assert!(!command_contains_text_search("echo 'a; grep b'"));
        // 漏网
        assert!(command_contains_text_search("bash -c \"grep foo .\""));
        assert!(command_contains_text_search("sh -c 'cd x && rg foo'"));
        assert!(command_contains_text_search("grep.exe foo"));
        assert!(command_contains_text_search("sudo grep foo ."));
        assert!(command_contains_text_search("cat a.rs | grep foo"));
        assert!(command_contains_text_search("ls && git -C r grep foo"));
        assert!(command_contains_text_search("findstr /s foo *.rs"));
        assert!(command_contains_text_search("Select-String foo a.rs"));
        assert!(command_contains_text_search("FOO=1 rg foo"));
        assert!(command_contains_text_search("echo hi;grep foo ."));
    }

    #[test]
    fn h8_sed_semicolon_chain_cannot_bypass_slice_quota() {
        // 单段、少量合计仍是切片
        assert!(is_sed_slice_print("-n '300,340p' src/a.rs"));
        assert!(is_sed_slice_print("-n '50p;80p' src/a.rs"));
        // 合计行数超限：不是切片（计为普通读）
        assert!(!is_sed_slice_print("-n '1,200p;201,400p;401,600p' f.rs"));
        assert!(!is_sed_slice_print("-n '1,150p;151,300p' f.rs"));
        // 合并 flag
        assert!(is_sed_slice_print("-ne '10,20p' src/a.rs"));
        assert!(!is_sed_slice_print("-ni '10,20p' src/a.rs"));
        assert!(!is_sed_slice_print("'10,20p' src/a.rs"));
    }

    #[test]
    fn third_party_astrolabe_named_tools_are_not_astrolabe() {
        assert!(!is_astrolabe_tool("mcp__astrolabe_docs__fetch_page"));
        assert!(!is_astrolabe_symbolic_tool(
            "mcp__astrolabe_docs__fetch_page"
        ));
        assert!(is_astrolabe_tool("mcp__astrolabe__find_symbol"));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe-ai__find_references"
        ));
        assert!(is_astrolabe_tool("mcp__astrolabe__search_code"));
        assert!(!is_astrolabe_symbolic_tool("mcp__astrolabe__search_code"));
    }

    #[test]
    fn command_array_is_joined_and_classified() {
        let v = serde_json::json!(["bash", "-lc", "grep -rn foo src"]);
        let s = command_value_to_string(&v).unwrap();
        assert_eq!(s, "bash -lc 'grep -rn foo src'");
        assert_eq!(
            command_value_to_string(&serde_json::json!("ls")).as_deref(),
            Some("ls")
        );
        assert!(command_value_to_string(&serde_json::json!([])).is_none());
        let (name, _) = effective_shell_command(&s).unwrap();
        assert_eq!(name, "grep");
    }

    #[test]
    fn redundant_reset_does_not_write_counter() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("astrolabe_noop_reset_{rand_id}"));
        let payload = br#"{"session_id":"noop1","tool_name":"mcp__astrolabe__find_symbol"}"#;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_remind_impl(
            "claude-code",
            &payload[..],
            &mut out,
            &mut err,
            Some(10.0),
            Some(&home),
        );
        assert_eq!(code, 0);
        let path = get_hook_data_dir(&home, "noop1", None)
            .unwrap()
            .join("counter.json");
        // 状态本就是零：不应创建 counter.json
        assert!(!path.exists(), "no-op reset must not write counter.json");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn h6_residual_windows_type_and_find_exec() {
        let (n, args) = effective_shell_command("type src\\a.rs").unwrap();
        assert_eq!(n, "type");
        assert!(READ_SHELL_COMMANDS.contains(&n.as_str()));
        assert_eq!(args.as_deref(), Some("src\\a.rs"));
        // 硬模式：find -exec grep 属于已知未覆盖（grep 不在命令位置），这里固化现状
        let _ = command_contains_text_search("find . -name '*.rs' -exec grep foo {} +");
    }

    #[test]
    fn resolve_context_resets_all_burst_counters_by_design() {
        // resolve_context 是入口导航工具：它的回答取代了之前 grep/read 的探索，
        // 因此与 find_symbol 等一样清零全部 burst 计数。
        let cfg = HooksConfig::default();
        let mut c = CounterState {
            n_grep: 2,
            n_read: 2,
            n_slice: 1,
            n_non_symbolic: 3,
            ..Default::default()
        };
        let kind = classify_tool(
            "mcp__astrolabe__resolve_context",
            Client::ClaudeCode,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(kind, ToolKind::AstrolabeSymbolic);
        c.reset_burst();
        assert_eq!(
            (c.n_grep, c.n_read, c.n_slice, c.n_non_symbolic),
            (0, 0, 0, 0)
        );
        let _ = cfg;
    }
}
