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
    let mut ctx =
        xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
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
    let mut ctx =
        xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
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
    let mut ctx =
        xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new_v7());
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
