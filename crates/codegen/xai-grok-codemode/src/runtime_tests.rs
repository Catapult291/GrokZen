//! Sandbox unit tests: eval, interruption, host injection, limits.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::host::{HostCallError, HostFuture, HostTools};

struct MockHost {
    /// Every (tool_name, input) dispatched through use_tool.
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    /// When set, every use_tool call fails with this error.
    fail: Option<HostCallError>,
    /// Artificial latency per use_tool call.
    latency: Duration,
}

impl MockHost {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: None,
            latency: Duration::ZERO,
        }
    }

    fn failing(err: HostCallError) -> Self {
        Self {
            fail: Some(err),
            ..Self::new()
        }
    }

    fn with_latency(latency: Duration) -> Self {
        Self {
            latency,
            ..Self::new()
        }
    }

    fn call_names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }
}

impl HostTools for MockHost {
    fn servers(&self) -> serde_json::Value {
        json!([{ "name": "mock", "description": "mock server", "toolCount": 2 }])
    }

    fn search(
        &self,
        _query: String,
        _limit: usize,
    ) -> HostFuture<Result<serde_json::Value, HostCallError>> {
        Box::pin(async {
            Ok(json!({
                "results": [{
                    "server": "mock",
                    "tools": [{ "tool_name": "mock__echo", "input_schema": { "type": "object" } }]
                }],
                "total_hidden_tools": 1,
                "status": "ready",
                "note": null
            }))
        })
    }

    fn use_tool(
        &self,
        tool_name: String,
        input: serde_json::Value,
    ) -> HostFuture<Result<serde_json::Value, HostCallError>> {
        let fail = self.fail.clone();
        let latency = self.latency;
        // Record synchronously before boxing so assertions do not race the future.
        self.calls
            .lock()
            .unwrap()
            .push((tool_name.clone(), input.clone()));
        Box::pin(async move {
            if latency > Duration::ZERO {
                tokio::time::sleep(latency).await;
            }
            if let Some(err) = fail {
                return Err(err);
            }
            Ok(json!({
                "text": format!("echo:{tool_name}"),
                "isError": false
            }))
        })
    }
}

fn fast_limits() -> Limits {
    Limits {
        timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}

#[tokio::test]
async fn eval_simple_return() {
    let out = execute("return 1 + 1;", Arc::new(MockHost::new()), &fast_limits())
        .await
        .unwrap();
    assert_eq!(out.value, json!(2));
    assert!(!out.truncated);
    assert_eq!(out.host_calls, 0);
}

#[tokio::test]
async fn async_await_and_promise_all() {
    let host = Arc::new(MockHost::with_latency(Duration::from_millis(80)));
    let code = r#"
const a = await tools.useTool("mock__a", { x: 1 });
const [b, c] = await Promise.all([
  tools.useTool("mock__b", {}),
  tools.useTool("mock__c", {}),
]);
return { a: a.text, b: b.text, c: c.text };
"#;
    let started = Instant::now();
    let out = execute(code, host.clone(), &fast_limits()).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        out.value,
        json!({ "a": "echo:mock__a", "b": "echo:mock__b", "c": "echo:mock__c" })
    );
    assert_eq!(out.host_calls, 3);
    // Sequential would be >= 240ms; concurrency keeps it near 160ms.
    assert!(
        elapsed < Duration::from_millis(220),
        "Promise.all host calls did not overlap: {elapsed:?}"
    );
    assert_eq!(host.call_names(), vec!["mock__a", "mock__b", "mock__c"]);
}

#[tokio::test]
async fn host_error_passthrough_keeps_code() {
    let host = Arc::new(MockHost::failing(HostCallError::new(
        "permission_denied",
        "user rejected the MCP call",
    )));
    let err = execute(
        r#"return await tools.useTool("sentry__find_organizations", {});"#,
        host,
        &fast_limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Host);
    assert_eq!(err.code, "permission_denied");
    assert!(
        err.message.contains("user rejected"),
        "got: {}",
        err.message
    );
    assert_eq!(err.kind_code(), "codemode_host_error");
}

#[tokio::test]
async fn host_call_limit_enforced() {
    let limits = Limits {
        max_host_calls: 3,
        ..fast_limits()
    };
    let err = execute(
        r#"
for (let i = 0; i < 10; i++) {
  await tools.useTool("mock__t", { i });
}
return "unreachable";
"#,
        Arc::new(MockHost::new()),
        &limits,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::CallLimit);
    assert_eq!(err.code, "codemode_call_limit");
}

#[tokio::test]
async fn dead_loop_is_killed_by_timeout() {
    let limits = Limits {
        timeout: Duration::from_millis(400),
        ..Limits::default()
    };
    let started = Instant::now();
    let err = execute("while (true) {}", Arc::new(MockHost::new()), &limits)
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(err.kind, CodemodeErrorKind::Timeout, "got: {err:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "interrupt did not fire promptly: {elapsed:?}"
    );
}

#[tokio::test]
async fn memory_hog_hits_oom() {
    let limits = Limits {
        timeout: Duration::from_secs(10),
        memory_bytes: 4 * 1024 * 1024,
        ..Limits::default()
    };
    let err = execute(
        r#"
const a = [];
for (let i = 0; i < 1e9; i++) {
  a.push(new Array(1000).fill("xxxxxxxx"));
}
return a.length;
"#,
        Arc::new(MockHost::new()),
        &limits,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Oom, "got: {err:?}");
}

#[tokio::test]
async fn syntax_error_classified() {
    let err = execute(
        "const = ; return 1",
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Syntax, "got: {err:?}");
    assert_eq!(err.code, "codemode_syntax");
}

#[tokio::test]
async fn bare_throw_is_runtime_error() {
    let err = execute(
        r#"throw new Error("boom");"#,
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Runtime, "got: {err:?}");
    assert!(err.message.contains("boom"), "got: {}", err.message);
}

#[tokio::test]
async fn user_error_with_oom_substring_is_runtime() {
    // Substring keyword classification is only for QuickJS `InternalError`;
    // a user Error whose message merely mentions "stack overflow" stays Runtime.
    let err = execute(
        r#"throw new Error("boom: stack overflow");"#,
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Runtime, "got: {err:?}");
    assert_eq!(err.code, "codemode_error");
    assert!(err.message.contains("boom"), "got: {}", err.message);
}

#[tokio::test]
async fn user_error_with_timeout_substring_is_runtime() {
    let err = execute(
        r#"throw new Error("task interrupted by user");"#,
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Runtime, "got: {err:?}");
    assert_eq!(err.code, "codemode_error");
}

#[tokio::test]
async fn circular_arg_rejects_with_codemode_invalid_argument() {
    let err = execute(
        r#"
const a = {};
a.self = a;
return await tools.useTool("mock__t", a);
"#,
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, CodemodeErrorKind::Host, "got: {err:?}");
    assert_eq!(err.code, "codemode_invalid_argument");
    assert!(
        err.message.contains("circular") || err.message.contains("Converting"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn unserializable_return_degrades_with_flag() {
    // A pure function property is un-serializable and String() becomes
    // the function source text; the degraded flag tells the model data was lost.
    let out = execute(
        "return function () {};",
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap();
    assert_eq!(out.value.get("degraded"), Some(&json!(true)));
    let text = out
        .value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(text.contains("function"), "got: {}", text);
}

#[tokio::test]
async fn circular_return_degrades_with_text_preview() {
    // Circular structures cannot be stringified; wrap_script falls back to
    // String(__ret) and marks degraded:true so the model knows data was lost.
    let out = execute(
        r#"
const a = { n: 1 };
a.self = a;
return a;
"#,
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap();
    assert_eq!(out.value.get("degraded"), Some(&json!(true)));
    assert_eq!(out.value.get("text"), Some(&json!("[object Object]")));
}

#[tokio::test]
async fn oversized_return_is_truncated() {
    let limits = Limits {
        max_return_bytes: 256,
        ..fast_limits()
    };
    let out = execute(
        r#"return "x".repeat(10000);"#,
        Arc::new(MockHost::new()),
        &limits,
    )
    .await
    .unwrap();
    assert!(out.truncated);
    assert_eq!(out.value.get("truncated"), Some(&json!(true)));
    assert!(out.value.get("preview").is_some());
}

#[tokio::test]
async fn console_lines_are_capped() {
    let limits = Limits {
        max_console_lines: 5,
        ..fast_limits()
    };
    let out = execute(
        r#"
for (let i = 0; i < 20; i++) console.log("line", i);
return 1;
"#,
        Arc::new(MockHost::new()),
        &limits,
    )
    .await
    .unwrap();
    assert_eq!(out.console.len(), 5);
    assert_eq!(out.value, json!(1));
}

#[tokio::test]
async fn host_servers_visible() {
    let out = execute(
        "return await host.servers();",
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap();
    assert_eq!(
        out.value,
        json!([{ "name": "mock", "description": "mock server", "toolCount": 2 }])
    );
}

#[tokio::test]
async fn search_tool_round_trip() {
    let out = execute(
        r#"return await tools.searchTool({ query: "echo", limit: 5 });"#,
        Arc::new(MockHost::new()),
        &fast_limits(),
    )
    .await
    .unwrap();
    assert_eq!(out.host_calls, 1);
    assert_eq!(
        out.value.pointer("/results/0/tools/0/tool_name"),
        Some(&json!("mock__echo"))
    );
}
