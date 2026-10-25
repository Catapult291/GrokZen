//! One index per repository across sessions: "leader hosts, followers forward".
//!
//! Several MCP sessions on the same repository (Claude Code + Cursor, or a
//! handful of sub-agents) used to each build the full graph, NameIndex,
//! FileCache, watcher and language servers: N sessions, N copies. Here the
//! process that wins `indexer.lock` (see `astrolabe_core::elect`) also serves
//! a user-private Unix socket. Later sessions connect, hand-shake on version,
//! and forward `tools/call` / `resources/read` instead of building anything;
//! they are a thin stdio shim of a few MB.
//!
//! * **Socket:** `$XDG_RUNTIME_DIR/astrolabe/<hash(root)>.sock` on Linux
//!   (`~/.cache/astrolabe/run/` when unset), `~/Library/Caches/astrolabe/` on
//!   macOS. Directory 0700, socket 0600. Never under the repo: macOS
//!   `sun_path` is 104 bytes. `ASTROLABE_SHARED_INDEX_DIR` overrides the
//!   directory (tests).
//! * **Handshake:** protocol, package version, `EXTRACTOR_VERSION`,
//!   `SCHEMA_VERSION`, `CACHE_FORMAT_VERSION`, a hash of the full tool
//!   catalog, a hash of the `ASTROLABE_*` settings, and the canonical root.
//!   Any difference → this session indexes on its own (the old behaviour).
//! * **Per-session state** (once-only legends, coverage note, loop guard,
//!   dedup tables, resources, the client context and its output shape) lives
//!   in a per-connection view on the leader, keyed by the connection.
//! * **Leader exit:** close the socket (followers see EOF), then release the
//!   lock. Followers race for the lock: the winner warm-loads from redb and
//!   binds the socket, the others reconnect. Calls arriving meanwhile wait up
//!   to [`QUEUE_BUDGET`], then the session falls back to its own index.
//! * **Fault isolation:** a leader that stops answering (tool timeout + 30 s)
//!   is abandoned and the follower indexes on its own.
//! * `ASTROLABE_SHARED_INDEX=off` disables all of it. Multi-project mode and
//!   Windows (no named-pipe transport yet) always run independently.
// Windows has only the stub below; the protocol types are unused there.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use astrolabe_core::elect::IndexerLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::ClientContext;

/// Wire protocol version; bump on any frame change.
pub(crate) const PROTO: u32 = 2;
/// How long a call waits for a leader hand-over before going local.
pub(crate) const QUEUE_BUDGET: Duration = Duration::from_secs(3);
/// At startup, a held lock with no reachable socket (an older or opted-out
/// process holds it) is waited on this long before indexing independently.
pub(crate) const STARTUP_GRACE: Duration = Duration::from_millis(500);

/// Env vars that do not change what a tool answers, or are carried per
/// session, and so may differ between leader and follower.
const SETTINGS_IGNORED: &[&str] = &[
    "ASTROLABE_SHARED_INDEX",
    "ASTROLABE_SHARED_INDEX_DIR",
    "ASTROLABE_ROOT",
    "ASTROLABE_CONTEXT",
];

/// Environment a language server or toolchain launched for a forwarded
/// call would inherit (`get_diagnostics` toolchain path, `lsp=true`, rename,
/// LSP fallbacks). The leader spawns them with *its* environment, so a
/// follower whose toolchain env differs must not share.
const TOOLCHAIN_ENV: &[&str] = &[
    "PATH",
    "GOPATH",
    "GOROOT",
    "GOFLAGS",
    "GOWORK",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "VIRTUAL_ENV",
    "NODE_PATH",
];

/// Hash of [`TOOLCHAIN_ENV`] (unset and empty are distinct).
pub(crate) fn toolchain_env_hash() -> String {
    let mut buf = Vec::new();
    for key in TOOLCHAIN_ENV {
        buf.extend_from_slice(key.as_bytes());
        match std::env::var_os(key) {
            Some(value) => {
                buf.push(b'=');
                buf.extend_from_slice(value.to_string_lossy().as_bytes());
            }
            None => buf.push(b'!'),
        }
        buf.push(0);
    }
    format!("{:016x}", fnv64(&buf))
}

/// `ASTROLABE_SHARED_INDEX` (default on; `off`/`0`/`false`/`no` disables).
pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("ASTROLABE_SHARED_INDEX")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "false" | "no"
    )
}

/// FNV-1a 64: stable across builds and toolchains (std's hasher is not),
/// which matters because two releases must agree on the socket name to see
/// each other's version and refuse politely.
pub(crate) fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Hash of every `ASTROLABE_*` setting that can change an answer.
pub(crate) fn settings_hash() -> String {
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.to_string_lossy().into_owned())))
        .filter(|(k, _)| k.starts_with("ASTROLABE_") && !SETTINGS_IGNORED.contains(&k.as_str()))
        .collect();
    vars.sort();
    let mut buf = String::new();
    for (k, v) in vars {
        buf.push_str(&k);
        buf.push('=');
        buf.push_str(&v);
        buf.push('\0');
    }
    format!("{:016x}", fnv64(buf.as_bytes()))
}

/// [`ClientContext`] on the wire: the leader rebuilds the follower's own
/// context (a custom YAML path may not resolve from the leader's cwd).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WireContext {
    pub name: String,
    pub load_key: String,
    pub structured_tool_output: Option<bool>,
    pub excluded_tools: Vec<String>,
    pub notes: String,
    pub deny_text_search: bool,
}

impl From<&ClientContext> for WireContext {
    fn from(c: &ClientContext) -> Self {
        Self {
            name: c.name.clone(),
            load_key: c.load_key.clone(),
            structured_tool_output: c.structured_tool_output,
            excluded_tools: c.excluded_tools.clone(),
            notes: c.notes.clone(),
            deny_text_search: c.deny_text_search,
        }
    }
}

impl From<WireContext> for ClientContext {
    fn from(w: WireContext) -> Self {
        ClientContext {
            name: w.name,
            load_key: w.load_key,
            structured_tool_output: w.structured_tool_output,
            excluded_tools: w.excluded_tools,
            notes: w.notes,
            deny_text_search: w.deny_text_search,
        }
    }
}

/// First frame a follower sends.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub proto: u32,
    pub pkg: String,
    pub extractor: u32,
    pub schema: u32,
    pub cache_format: u32,
    pub tools_hash: String,
    pub settings_hash: String,
    /// [`toolchain_env_hash`].
    #[serde(default)]
    pub toolchain_hash: String,
    pub root: String,
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub context: WireContext,
}

impl Hello {
    pub(crate) fn new(root: &Path, tools_hash: String, context: &ClientContext) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self {
            proto: PROTO,
            pkg: env!("CARGO_PKG_VERSION").to_string(),
            extractor: astrolabe_core::store::EXTRACTOR_VERSION,
            schema: astrolabe_core::store::SCHEMA_VERSION,
            cache_format: astrolabe_core::store::CACHE_FORMAT_VERSION,
            tools_hash,
            settings_hash: settings_hash(),
            toolchain_hash: toolchain_env_hash(),
            root: root.display().to_string(),
            session_id: format!("{pid}-{nanos:x}"),
            pid,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            context: context.into(),
        }
    }

    /// Why `other` (a follower) cannot share this leader, if it cannot.
    pub(crate) fn mismatch(&self, other: &Hello) -> Option<String> {
        let checks: [(&str, String, String); 8] = [
            ("proto", self.proto.to_string(), other.proto.to_string()),
            ("version", self.pkg.clone(), other.pkg.clone()),
            (
                "extractor",
                self.extractor.to_string(),
                other.extractor.to_string(),
            ),
            (
                "schema",
                format!("{}/{}", self.schema, self.cache_format),
                format!("{}/{}", other.schema, other.cache_format),
            ),
            ("tools", self.tools_hash.clone(), other.tools_hash.clone()),
            (
                "settings",
                self.settings_hash.clone(),
                other.settings_hash.clone(),
            ),
            (
                "toolchain env",
                self.toolchain_hash.clone(),
                other.toolchain_hash.clone(),
            ),
            ("root", self.root.clone(), other.root.clone()),
        ];
        checks
            .into_iter()
            .find(|(_, mine, theirs)| mine != theirs)
            .map(|(what, mine, theirs)| {
                format!("{what} differs (leader {mine}, follower {theirs})")
            })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HelloReply {
    pub ok: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub leader_pid: u32,
}

/// Follower → leader frames (one JSON object per line).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Req {
    Call {
        id: u64,
        name: String,
        #[serde(default)]
        args: Option<serde_json::Map<String, Value>>,
    },
    Read {
        id: u64,
        uri: String,
    },
    Cancel {
        id: u64,
    },
}

/// Leader → follower frame: `ok` is the result JSON, `err` an `ErrorData`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Resp {
    pub id: u64,
    #[serde(default)]
    pub ok: Option<Value>,
    #[serde(default)]
    pub err: Option<Value>,
}

/// What a forwarded call needs, without the id.
#[derive(Clone, Debug)]
pub(crate) enum Op {
    Call {
        name: String,
        args: Option<serde_json::Map<String, Value>>,
    },
    Read {
        uri: String,
    },
}

/// Outcome of [`Shared::forward`].
#[derive(Debug)]
pub(crate) enum Forwarded {
    /// The leader answered: `Ok(result json)` or `Err(ErrorData json)`.
    Done(Result<Value, Value>),
    /// Serve locally (this process is the leader, or fell back).
    Local,
    /// The client cancelled while waiting.
    Cancelled,
}

/// One follower's view on the leader. Implemented by the server.
pub(crate) trait SessionView: Send + Sync {
    fn call(
        &self,
        name: &str,
        args: Option<serde_json::Map<String, Value>>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Value, Value>;
    fn read(&self, uri: &str) -> Result<Value, Value>;
}

/// The server side of sharing, implemented by the server.
pub(crate) trait Host: Send + Sync {
    /// Fresh per-session state over the shared index.
    fn open_session(&self, context: ClientContext) -> Arc<dyn SessionView>;
    /// Hold `lock` as this root's indexer leader.
    fn adopt_lock(&self, lock: IndexerLock);
    /// Start (once) the local watcher + index build.
    fn start_local(&self);
    /// Hold this root's indexer lock if free (`refresh_leader`); `true` when
    /// this process holds it afterwards.
    fn try_take_lock(&self) -> bool;
    /// Drop the indexer lock (a promotion that lost a race with shutdown).
    fn release_lock(&self);
}

/// Socket directory: see the module docs.
pub(crate) fn socket_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ASTROLABE_SHARED_INDEX_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library").join("Caches").join("astrolabe"));
    }
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        let run = PathBuf::from(run);
        if run.is_dir() {
            return Some(run.join("astrolabe"));
        }
    }
    home.map(|h| h.join(".cache").join("astrolabe").join("run"))
}

/// Longest socket path accepted (macOS `sun_path` is 104 bytes incl. NUL).
const MAX_SOCKET_PATH: usize = 100;

/// Socket for `root` inside `dir`.
pub(crate) fn socket_path_in(dir: &Path, root: &Path) -> Option<PathBuf> {
    let name = format!(
        "{:016x}.sock",
        fnv64(root.as_os_str().to_string_lossy().as_bytes())
    );
    let path = dir.join(name);
    (path.as_os_str().len() <= MAX_SOCKET_PATH).then_some(path)
}

#[cfg(unix)]
mod imp;
#[cfg(unix)]
pub(crate) use imp::{private_socket_path, Shared};

/// Windows: no named-pipe transport yet, every session indexes on its own.
#[cfg(not(unix))]
mod stub {
    use super::*;

    #[derive(Debug)]
    pub(crate) struct Shared;

    impl Shared {
        pub(crate) fn forward(
            &self,
            _op: Op,
            _reply_timeout: Option<Duration>,
            _cancelled: &dyn Fn() -> bool,
        ) -> Forwarded {
            Forwarded::Local
        }
        pub(crate) fn local_since(&self) -> Option<std::time::Instant> {
            None
        }
        pub(crate) fn shutdown(&self) {}
        pub(crate) fn is_follower(&self) -> bool {
            false
        }
    }

    pub(crate) fn private_socket_path(_root: &Path) -> Option<PathBuf> {
        None
    }
}
#[cfg(not(unix))]
pub(crate) use stub::{private_socket_path, Shared};

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello::new(
            Path::new("/repo"),
            "t".into(),
            &ClientContext::default_builtin(),
        )
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn handshake_mismatch_names_the_field() {
        let a = hello();
        let mut b = hello();
        assert_eq!(a.mismatch(&b), None, "session id / pid / cwd may differ");
        b.pkg = "0.0.1".into();
        assert!(a.mismatch(&b).unwrap().starts_with("version"));
        let mut c = hello();
        c.extractor += 1;
        assert!(a.mismatch(&c).unwrap().starts_with("extractor"));
        let mut d = hello();
        d.tools_hash = "x".into();
        assert!(a.mismatch(&d).unwrap().starts_with("tools"));
        let mut e = hello();
        e.root = "/other".into();
        assert!(a.mismatch(&e).unwrap().starts_with("root"));
        let mut f = hello();
        f.toolchain_hash = "other-path".into();
        assert!(a.mismatch(&f).unwrap().starts_with("toolchain env"));
    }

    #[test]
    fn wire_context_round_trips() {
        let ctx = ClientContext::builtin("cursor").unwrap();
        let back: ClientContext = WireContext::from(&ctx).into();
        assert_eq!(back, ctx);
    }

    #[test]
    fn socket_path_is_short_and_per_root() {
        let dir = Path::new("/run/user/1000/astrolabe");
        let a = socket_path_in(dir, Path::new("/a")).unwrap();
        let b = socket_path_in(dir, Path::new("/b")).unwrap();
        assert_ne!(a, b);
        assert!(a.as_os_str().len() <= MAX_SOCKET_PATH);
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        assert!(socket_path_in(&long, Path::new("/a")).is_none());
    }
}
