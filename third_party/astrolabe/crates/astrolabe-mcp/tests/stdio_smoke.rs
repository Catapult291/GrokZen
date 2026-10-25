use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use astrolabe_mcp::{TOOL_CATALOG_TTL_MS, TOOL_COUNT};
use serde_json::{json, Value};

fn unique_temp_dir() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "astrolabe-mcp-stdio-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        id
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("hello.py"), "def greet():\n    return 'hi'\n").unwrap();
    dir.canonicalize().unwrap_or(dir)
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    root: PathBuf,
}

impl Server {
    fn spawn() -> Self {
        let root = unique_temp_dir();
        // Mirror Claude Code's global config: `args: ["."]` plus the client's cwd.
        let mut child = Command::new(env!("CARGO_BIN_EXE_astrolabe"))
            .arg(".")
            .current_dir(&root)
            .env("RUST_LOG", "warn")
            .env_remove("ASTROLABE_STRUCTURED")
            .env_remove("ASTROLABE_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start astrolabe MCP server");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
            root,
        }
    }

    fn rpc_line(&mut self, line: &str) -> Value {
        writeln!(self.stdin, "{line}").expect("write JSON-RPC request");
        self.stdin.flush().expect("flush JSON-RPC request");
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .expect("read JSON-RPC response");
        assert!(
            !response.trim().is_empty(),
            "server returned no JSON-RPC line for {line}"
        );
        serde_json::from_str(&response).unwrap_or_else(|error| {
            panic!("invalid JSON-RPC response for {line}: {error}; line={response:?}")
        })
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.rpc_line(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn assert_tool_catalog(response: &Value) {
    assert_eq!(
        response.get("error"),
        None,
        "unexpected JSON-RPC error: {response}"
    );
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array missing: {response}"));
    assert_ne!(
        tools.len(),
        0,
        "tools/list must not silently return an empty catalog: {response}"
    );
    assert_eq!(tools.len(), TOOL_COUNT, "unexpected tool count: {response}");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    let mut expected = vec![
        "find_references",
        "find_symbol",
        "get_diagnostics",
        "get_languages",
        "goto_definition",
        "resolve_context",
        "search_code",
    ];
    expected.sort_unstable();
    assert_eq!(names, expected, "tools/list must be exactly the 7 tools");
    assert_eq!(response["result"]["ttlMs"], TOOL_CATALOG_TTL_MS);
}

#[test]
fn stdio_tools_list_without_initialize_returns_catalog() {
    let mut server = Server::spawn();
    // Bare request: no `initialize`, no `_meta`, no `params`. This is the
    // 2026-07-28 client path that previously produced a silent empty catalog.
    let response = server.rpc_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert_eq!(response["id"], 1);
    assert_tool_catalog(&response);
}

#[test]
fn stdio_tools_list_with_inline_protocol_meta() {
    let mut server = Server::spawn();
    let response = server.rpc(
        1,
        "tools/list",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "astrolabe-smoke-test",
                    "version": "1.0.0"
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }),
    );
    assert_eq!(response["id"], 1);
    assert_tool_catalog(&response);
    assert_eq!(response["result"]["cacheScope"], "public");
}

#[test]
fn stdio_tools_list_after_initialize_includes_ttl() {
    let mut server = Server::spawn();
    let init = server.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "astrolabe-smoke-test", "version": "1.0.0"}
        }),
    );
    assert_eq!(init.get("error"), None, "initialize failed: {init}");
    let response = server.rpc(2, "tools/list", json!({}));
    assert_eq!(response["id"], 2);
    assert_tool_catalog(&response);
}

#[test]
fn stdio_get_languages_returns_nonempty_text() {
    let mut server = Server::spawn();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last;
    let mut id = 1;
    // Ready gate: wait until the index leaves Building (not "text contains python").
    // An empty / non-Python workspace would otherwise spin until the deadline.
    loop {
        last = server.rpc(
            id,
            "tools/call",
            json!({
                "name": "get_languages",
                "arguments": {"budget_tokens": 2500}
            }),
        );
        id += 1;
        assert_eq!(last.get("error"), None, "tools/call protocol error: {last}");
        let text = last["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !text.trim().is_empty(),
            "content[0].text must be non-empty: {last}"
        );
        assert!(
            !text.contains("尚未实现"),
            "index path still reports unimplemented: {text}"
        );
        let still_building = text.contains("索引正在后台构建")
            || text.to_lowercase().contains("still building")
            || text.to_lowercase().contains("index is building");
        if still_building {
            if Instant::now() >= deadline {
                panic!("timed out waiting for index; last response: {last}");
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        // Index is ready (or failed). Assert on the fixture contents.
        assert!(
            text.to_lowercase().contains("python"),
            "expected python language after index ready: {text}"
        );
        assert!(
            text.contains("index_root:"),
            "tool text must declare the absolute index root: {text}"
        );
        let root = server
            .root
            .canonicalize()
            .unwrap_or_else(|_| server.root.clone());
        assert!(
            text.contains(&root.display().to_string()),
            "index_root should be the temp project root (unique_temp_dir creates .git): {text}"
        );
        assert!(
            last["result"].get("structuredContent").is_none()
                || last["result"]["structuredContent"].is_null(),
            "default wire format is text-only; leftover metadata hides hits in Claude Code: {last}"
        );
        return;
    }
}

/// Startup failures must reach the client as MCP results, not a bare
/// "connection closed": the handshake and tools/list work, tools/call returns
/// the reason with `isError`, and the process exits non-zero afterwards.
#[test]
fn stdio_startup_failure_is_reported_as_tool_error() {
    let missing = std::env::temp_dir().join("astrolabe-definitely-missing-dir-9f3a");
    let mut child = Command::new(env!("CARGO_BIN_EXE_astrolabe"))
        .arg(&missing)
        .env("RUST_LOG", "warn")
        .env_remove("ASTROLABE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start astrolabe");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |id: u64, method: &str, params: Value| -> Value {
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut response = String::new();
        stdout.read_line(&mut response).unwrap();
        assert!(!response.trim().is_empty(), "no response to {method}");
        serde_json::from_str(&response).unwrap()
    };
    let list = rpc(1, "tools/list", json!({}));
    assert_tool_catalog(&list);
    let call = rpc(
        2,
        "tools/call",
        json!({"name": "search_code", "arguments": {"query": "x"}}),
    );
    assert_eq!(call["result"]["isError"], json!(true), "{call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("not a directory"), "{text}");
    assert!(text.contains("启动失败"), "{text}");
    drop(stdin);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
}

/// A parent directory holding exactly one git repo serves that repo directly:
/// no `root` argument, no "cannot route" error.
#[test]
fn stdio_single_nested_repo_is_selected_automatically() {
    let parent = unique_temp_dir();
    // `unique_temp_dir` makes `parent` itself a repo; build a clean parent.
    let outer = parent.join("outer");
    let solo = outer.join("solo");
    std::fs::create_dir_all(solo.join(".git")).unwrap();
    std::fs::write(
        solo.join("lib.py"),
        "def only_here_marker():\n    return 1\n",
    )
    .unwrap();
    std::fs::remove_dir_all(parent.join(".git")).unwrap();
    // `outer` has no marker of its own, but `parent/..` might: run from outer
    // with a temp dir that is not inside a repo.
    let mut child = Command::new(env!("CARGO_BIN_EXE_astrolabe"))
        .arg(".")
        .current_dir(&outer)
        .env("RUST_LOG", "warn")
        .env_remove("ASTROLABE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start astrolabe");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut id = 1;
    let text = loop {
        let line = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"find_symbol","arguments":{"query":"only_here_marker"}}})
        .to_string();
        id += 1;
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut response = String::new();
        stdout.read_line(&mut response).unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        let text = value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if text.contains("索引正在后台构建") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        break text;
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&parent);
    assert!(text.contains("only_here_marker"), "{text}");
    assert!(!text.contains("无法路由"), "{text}");
}
