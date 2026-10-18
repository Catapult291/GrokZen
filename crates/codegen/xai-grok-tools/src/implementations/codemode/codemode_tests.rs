//! `codemode` meta-tool tests (registration + sandbox round-trip with a
//! stub tool index).

use std::sync::Arc;

use crate::types::output::ToolOutput;
use crate::types::tool::ToolKind;
use crate::types::tool_index::{
    SearchSnapshot, ServerSummary, ToolIndex, ToolSearchIndex, ToolSearchResult,
};
use crate::types::tool_metadata::ToolMetadata;
use xai_tool_runtime::Tool;

struct StaticToolIndex {
    snapshot: SearchSnapshot,
}

impl ToolSearchIndex for StaticToolIndex {
    fn search_snapshot(&self, _query: &str, _limit: usize) -> SearchSnapshot {
        self.snapshot.clone()
    }

    fn list_server_summaries(&self) -> Vec<ServerSummary> {
        vec![ServerSummary {
            name: "sentry".into(),
            description: Some("error tracking".into()),
            tool_count: 1,
            tool_names: vec!["find_organizations".into()],
        }]
    }
}

fn one_tool_snapshot() -> SearchSnapshot {
    SearchSnapshot {
        results: vec![ToolSearchResult {
            tool_name: "sentry__find_organizations".into(),
            server_name: "sentry".into(),
            description: "List Sentry organizations".into(),
            score: 1.0,
            parameters: vec![],
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            managed_gateway_tool: None,
        }],
        total_hidden_tools: 1,
        is_ready: true,
    }
}

#[tokio::test]
async fn codemode_kind_is_codemode() {
    assert_eq!(super::Codemode.kind(), ToolKind::Codemode);
}

#[tokio::test]
async fn servers_and_search_visible_in_sandbox() {
    let resources = crate::types::resources::Resources::default().into_shared();
    resources
        .lock()
        .await
        .insert(ToolIndex(Arc::new(StaticToolIndex {
            snapshot: one_tool_snapshot(),
        })));
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions.insert(resources);

    let output = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: r#"
const servers = await host.servers();
const found = await tools.searchTool({ query: "sentry", limit: 5 });
return {
  serverName: servers[0].name,
  toolCount: servers[0].toolCount,
  firstTool: found.results[0].tools[0].tool_name,
};
"#
                .into(),
            },
        )
        .await
        .expect("codemode run");
    let ToolOutput::Text(text) = output else {
        panic!("expected text output");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text.text).expect("json envelope");
    assert_eq!(
        envelope.pointer("/result/serverName"),
        Some(&serde_json::json!("sentry"))
    );
    assert_eq!(
        envelope.pointer("/result/toolCount"),
        Some(&serde_json::json!(1))
    );
    assert_eq!(
        envelope.pointer("/result/firstTool"),
        Some(&serde_json::json!("sentry__find_organizations"))
    );
    assert_eq!(
        envelope.get("toolsCalled"),
        Some(&serde_json::json!([])),
        "search-only script must not list useTool calls"
    );
    assert_eq!(envelope.get("truncated"), Some(&serde_json::json!(false)));
}

#[tokio::test]
async fn host_error_surfaces_original_code() {
    let resources = crate::types::resources::Resources::default().into_shared();
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions.insert(resources);

    // No InnerDispatch and no gateway: local dispatch fails with a clear
    // error that must reach the model with its original code/detail.
    let err = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: r#"return await tools.useTool("no_such__tool", {});"#.into(),
            },
        )
        .await
        .expect_err("dispatch must fail without inner dispatch");
    let details = err.details.expect("custom details");
    let code = details.get("code").and_then(serde_json::Value::as_str);
    assert!(
        code.is_some(),
        "host error must keep a code in details: {details}"
    );
}

#[tokio::test]
async fn syntax_error_maps_to_codemode_syntax() {
    let resources = crate::types::resources::Resources::default().into_shared();
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions.insert(resources);

    let err = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: "const = ; return 1".into(),
            },
        )
        .await
        .expect_err("syntax error must fail");
    let details = err.details.expect("custom details");
    assert_eq!(
        details.get("code").and_then(serde_json::Value::as_str),
        Some("codemode_syntax")
    );
}

#[tokio::test]
async fn configured_timeout_overrides_default() {
    use crate::types::resources::Params;
    let resources = crate::types::resources::Resources::default().into_shared();
    resources.lock().await.insert(Params(super::CodemodeParams {
        timeout_secs: Some(1),
        ..Default::default()
    }));
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions.insert(resources);

    let started = std::time::Instant::now();
    let err = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: "while (true) {}".into(),
            },
        )
        .await
        .expect_err("dead loop must be killed");
    let elapsed = started.elapsed();
    let details = err.details.expect("custom details");
    assert_eq!(
        details.get("code").and_then(serde_json::Value::as_str),
        Some("codemode_timeout")
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "configured 1s timeout not applied: {elapsed:?}"
    );
}

/// End-to-end through `InnerDispatch`: the sandbox's `useTool` really
/// dispatches, and the script's `toolsCalled` audit lists the target.
#[tokio::test]
async fn use_tool_dispatches_through_inner_dispatch() {
    use std::sync::Mutex;
    use xai_tool_runtime::{ToolDispatch, ToolStream, TypedToolOutput};

    struct EchoDispatch {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait::async_trait]
    impl ToolDispatch for EchoDispatch {
        async fn call(
            &self,
            tool_id: xai_tool_protocol::ToolId,
            args: serde_json::Value,
            _ctx: xai_tool_runtime::ToolCallContext,
        ) -> ToolStream<TypedToolOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((tool_id.as_str().to_string(), args.clone()));
            let out = serde_json::json!({
                "type": "Text",
                "text": format!("echo:{}", tool_id.as_str()),
            });
            xai_tool_runtime::terminal_only(Ok(TypedToolOutput::from_value(tool_id, out)))
        }
    }

    let dispatch = Arc::new(EchoDispatch {
        calls: Mutex::new(Vec::new()),
    });
    let resources = crate::types::resources::Resources::default().into_shared();
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions
        .insert(crate::types::resources::InnerDispatch(dispatch.clone()));
    ctx.extensions.insert(resources);

    let output = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: r#"
const a = await tools.useTool("mock__echo", { x: 1 });
const b = await tools.useTool("mock__echo", { x: 2 });
return { a: a.text, b: b.text };
"#
                .into(),
            },
        )
        .await
        .expect("codemode run with dispatch");

    let calls = dispatch.calls.lock().unwrap().clone();
    let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["mock__echo", "mock__echo"]);
    assert_eq!(calls[0].1, serde_json::json!({ "x": 1 }));
    assert_eq!(calls[1].1, serde_json::json!({ "x": 2 }));

    let ToolOutput::Text(text) = output else {
        panic!("expected text output");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text.text).expect("json envelope");
    assert_eq!(
        envelope.pointer("/result/a"),
        Some(&serde_json::json!("echo:mock__echo"))
    );
    assert_eq!(
        envelope.get("toolsCalled"),
        Some(&serde_json::json!(["mock__echo", "mock__echo"]))
    );
}

/// §6-2: the default budget is 64 host calls; the 65th is refused.
#[tokio::test]
async fn default_host_call_budget_is_64() {
    use xai_tool_runtime::{ToolDispatch, ToolStream, TypedToolOutput};

    struct OkDispatch;

    #[async_trait::async_trait]
    impl ToolDispatch for OkDispatch {
        async fn call(
            &self,
            tool_id: xai_tool_protocol::ToolId,
            _args: serde_json::Value,
            _ctx: xai_tool_runtime::ToolCallContext,
        ) -> ToolStream<TypedToolOutput> {
            let out = serde_json::json!({ "type": "Text", "text": "ok" });
            xai_tool_runtime::terminal_only(Ok(TypedToolOutput::from_value(tool_id, out)))
        }
    }

    assert_eq!(
        xai_grok_codemode::Limits::default().max_host_calls,
        64,
        "handoff pins the default budget at 64"
    );
    let resources = crate::types::resources::Resources::default().into_shared();
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions
        .insert(crate::types::resources::InnerDispatch(Arc::new(OkDispatch)));
    ctx.extensions.insert(resources);

    // 64 calls succeed; the 65th must trip codemode_call_limit.
    let err = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: r#"
const out = [];
for (let i = 0; i < 65; i++) {
  out.push(await tools.useTool("mock__t", { i }));
}
return out.length;
"#
                .into(),
            },
        )
        .await
        .expect_err("call 65 must be refused");
    let details = err.details.expect("custom details");
    assert_eq!(
        details.get("code").and_then(serde_json::Value::as_str),
        Some("codemode_call_limit")
    );
}

/// §6-2: a killed script leaves the tool usable — the next run succeeds
/// (per-run runtime, so a timeout cannot poison the session).
#[tokio::test]
async fn dead_loop_kill_leaves_next_run_usable() {
    use crate::types::resources::Params;
    let resources = crate::types::resources::Resources::default().into_shared();
    resources.lock().await.insert(Params(super::CodemodeParams {
        timeout_secs: Some(1),
        ..Default::default()
    }));
    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions.insert(resources.clone());

    let err = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: "while (true) {}".into(),
            },
        )
        .await
        .expect_err("dead loop must be killed");
    let details = err.details.expect("custom details");
    assert_eq!(
        details.get("code").and_then(serde_json::Value::as_str),
        Some("codemode_timeout")
    );

    let mut ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
    ctx.extensions.insert(resources);
    let output = super::Codemode
        .run(
            ctx,
            super::CodemodeInput {
                code: "return 6 * 7;".into(),
            },
        )
        .await
        .expect("run after a kill must succeed");
    let ToolOutput::Text(text) = output else {
        panic!("expected text output");
    };
    let envelope: serde_json::Value = serde_json::from_str(&text.text).expect("json envelope");
    assert_eq!(envelope.pointer("/result"), Some(&serde_json::json!(42)));
}

/// §6-1 (structural accounting): for a Sentry-like "list orgs + projects"
/// orchestration, the tool-result bytes that re-enter model context drop by
/// well over 40% under codemode, because intermediate JSON stays sandbox-side
/// and only the compact final envelope is returned. This is a deterministic
/// structural measurement of the same mechanism the live A/B would measure;
/// it does not replace a 5-run usage-telemetry comparison on a real MCP.
#[tokio::test]
async fn token_accounting_structural_reduction_ge_40_percent() {
    use serde_json::json;

    /// Realistic-size Sentry org payload (~2 KiB) as an MCP tool would return.
    fn org_result(i: usize) -> serde_json::Value {
        let i = i % 10;
        // Padding lives inside a JSON string field so the payload stays parseable.
        let pad = "x".repeat(1200);
        let body = format!(
            r#"{{"id":"450000000{i}","slug":"org-{i}","name":"Example Org {i}","region":"us","platform":"javascript","dateCreated":"2024-01-0{i}T00:00:00Z","status":"active","links":{{"organization":"https://sentry.io/organizations/org-{i}/"}},"avatar":{{"avatarType":"letteravatar"}},"require2FA":false,"isEarlyAdopter":true,"codecovAcceptancePassed":true,"dataScrubbing":true,"scrubIpAddresses":true,"sensitiveFields":[],"relayPiiConfig":null,"roles":["member","admin","owner"],"teams":[{{"id":"4{i}00","slug":"team-frontend-{i}","name":"Frontend {i}"}},{{"id":"4{i}01","slug":"team-backend-{i}","name":"Backend {i}"}}],"projects":[{{"id":"9{i}00","slug":"web-app-{i}","name":"Web App {i}","platform":"javascript","dateCreated":"2024-02-0{i}T00:00:00Z","isPublic":false,"status":"active"}},{{"id":"9{i}01","slug":"mobile-{i}","name":"Mobile {i}","platform":"android","dateCreated":"2024-03-0{i}T00:00:00Z","isPublic":false,"status":"active"}}],"keys":[],"options":{{}},"pad":"{pad}"}}"#
        );
        serde_json::json!({
            "type": "text",
            "text": body,
        })
    }

    struct SentryishHost;

    impl xai_grok_codemode::HostTools for SentryishHost {
        fn servers(&self) -> serde_json::Value {
            json!([{ "name": "sentry", "description": "error tracking", "toolCount": 2 }])
        }
        fn search(
            &self,
            _query: String,
            _limit: usize,
        ) -> xai_grok_codemode::HostFuture<
            Result<serde_json::Value, xai_grok_codemode::HostCallError>,
        > {
            Box::pin(async {
                Ok(json!({ "results": [], "total_hidden_tools": 2, "status": "ready" }))
            })
        }
        fn use_tool(
            &self,
            tool_name: String,
            _input: serde_json::Value,
        ) -> xai_grok_codemode::HostFuture<
            Result<serde_json::Value, xai_grok_codemode::HostCallError>,
        > {
            Box::pin(async move {
                if tool_name.contains("projects") {
                    Ok(org_result(0))
                } else {
                    // list-organizations: 5 orgs, each entry ~2 KiB
                    Ok(json!({
                        "type": "text",
                        "text": format!(
                            "[{}]",
                            (0..5).map(|i| org_result(i)["text"].as_str().unwrap().to_string())
                                .collect::<Vec<_>>().join(",")
                        )
                    }))
                }
            })
        }
    }

    // Baseline: one list-organizations call + one get-projects call per org.
    // Every result re-enters the model context and stays for later turns.
    let baseline_bytes: usize = (0..=5).map(|_| org_result(0).to_string().len()).sum();

    // Codemode path: same orchestration in one script; only the compact
    // final envelope re-enters context.
    let code = r#"
const orgsJson = await tools.useTool("sentry__list_organizations", {});
const orgs = JSON.parse(orgsJson.text);
const slugs = [];
for (const org of orgs) {
  const p = await tools.useTool("sentry__list_projects", { organization: org.slug });
  slugs.push({ org: org.slug, projects: JSON.parse(p.text).length });
}
return slugs;
"#;
    let out = std::sync::Arc::new(SentryishHost);
    let outcome = xai_grok_codemode::execute(code, out, &xai_grok_codemode::Limits::default())
        .await
        .expect("orchestration script runs");
    let codemode_bytes = serde_json::to_string(&outcome.value).unwrap().len();

    let ratio = codemode_bytes as f64 / baseline_bytes as f64;
    println!(
        "structural token accounting: baseline tool-result bytes = {baseline_bytes}, \
         codemode result bytes = {codemode_bytes}, reduction = {:.1}%",
        (1.0 - ratio) * 100.0
    );
    assert!(
        ratio <= 0.6,
        "codemode context bytes {codemode_bytes} must be <=60% of baseline {baseline_bytes}"
    );
}
