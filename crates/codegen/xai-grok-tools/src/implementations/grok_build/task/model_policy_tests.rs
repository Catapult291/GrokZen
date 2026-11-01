use pretty_assertions::assert_eq;

use super::*;

#[test]
fn task_schema_offers_thoroughness_levels() {
    let schema = task_input_schema();
    assert!(
        property(&schema, "thoroughness").is_some(),
        "thoroughness must be model-facing: {schema}"
    );
    let rendered = schema.to_string();
    for level in ["quick", "medium", "very_thorough"] {
        assert!(rendered.contains(level), "missing {level}: {rendered}");
    }
}

#[test]
fn exported_schema_hides_model_only_under_inherited_selection() {
    let schema = task_input_schema();

    let selectable = exported_input_schema(&schema, &TaskParams::default());
    assert!(property(&selectable, "model").is_some());

    let inherited = TaskParams {
        model_selection: TaskModelSelection::Inherited,
        ..TaskParams::default()
    };
    let hidden = exported_input_schema(&schema, &inherited);
    assert!(property(&hidden, "model").is_none());
    assert!(property(&hidden, "prompt").is_some());
}

#[test]
fn exported_schema_has_no_subagent_type_without_selectable_types() {
    let schema = task_input_schema();

    let exported = exported_input_schema(&schema, &TaskParams::default());

    assert_eq!(schema, exported);
    assert!(property(&exported, SUBAGENT_TYPE_PARAM).is_none());
}

#[test]
fn exported_schema_offers_selectable_types_as_an_optional_enum() {
    let params = TaskParams {
        selectable_subagent_types: vec![
            descriptor("merlin:stash", "Searches Stash.\nUse for code history."),
            descriptor("reviewer", "Reviews diffs."),
        ],
        ..TaskParams::default()
    };

    let exported = exported_input_schema(&task_input_schema(), &params);

    assert_eq!(
        Some(&serde_json::json!({
            "type": "string",
            "enum": ["merlin:stash", "reviewer"],
            "description": "Omit for a general-purpose subagent. Pick explore for read-only investigation, plan for implementation design, and implementer for implementation plus verification; other types hand the task to the named agent:\n\
                            - merlin:stash: Searches Stash. Use for code history.\n\
                            - reviewer: Reviews diffs.",
        })),
        property(&exported, SUBAGENT_TYPE_PARAM)
    );
    let required = exported
        .get("required")
        .and_then(serde_json::Value::as_array)
        .expect("required list");
    assert!(!required.iter().any(|name| name == SUBAGENT_TYPE_PARAM));
}

#[test]
fn exported_schema_caps_listed_types_and_descriptions() {
    let long = "x".repeat(MAX_SUBAGENT_TYPE_DESCRIPTION_BYTES * 2);
    let params = TaskParams {
        selectable_subagent_types: (0..MAX_SELECTABLE_SUBAGENT_TYPES + 3)
            .map(|i| descriptor(&format!("agent-{i:03}"), &long))
            .collect(),
        ..TaskParams::default()
    };

    let exported = exported_input_schema(&task_input_schema(), &params);

    let subagent_type = property(&exported, SUBAGENT_TYPE_PARAM).expect("subagent_type");
    let names = subagent_type
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .expect("enum");
    assert_eq!(MAX_SELECTABLE_SUBAGENT_TYPES, names.len());
    let description = subagent_type
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("description");
    let first = description.lines().nth(1).expect("first type line");
    assert!(first.len() <= "- agent-000: ".len() + MAX_SUBAGENT_TYPE_DESCRIPTION_BYTES);
}

#[test]
fn exported_schema_leaves_out_types_with_overlong_names() {
    let long_name = format!("plugin:{}", "n".repeat(MAX_SUBAGENT_TYPE_NAME_BYTES));
    let params = TaskParams {
        selectable_subagent_types: vec![
            descriptor(&long_name, "Too long to list."),
            descriptor("reviewer", "Reviews diffs."),
        ],
        ..TaskParams::default()
    };

    let exported = exported_input_schema(&task_input_schema(), &params);

    let subagent_type = property(&exported, SUBAGENT_TYPE_PARAM).expect("subagent_type");
    assert_eq!(
        Some(&serde_json::json!(["reviewer"])),
        subagent_type.get("enum")
    );
    let description = subagent_type
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("description");
    assert!(!description.contains(&long_name));

    let only_long = TaskParams {
        selectable_subagent_types: vec![descriptor(&long_name, "Too long to list.")],
        ..TaskParams::default()
    };
    let schema = task_input_schema();
    assert_eq!(schema, exported_input_schema(&schema, &only_long));
}

#[test]
fn schema_preserves_builtin_before_custom_order() {
    let mut types: Vec<SubagentDescriptor> = xai_tool_types::BUILTIN_SUBAGENTS
        .iter()
        .map(|builtin| {
            builtin.to_descriptor(&xai_tool_types::SubagentToolNaming {
                execute: "run_terminal_command",
                read: "read_file",
                edit: "search_replace",
                list: "list_dir",
                search: "search_code",
                web_search: "web_search",
                plan: "todo_write",
            })
        })
        .collect();
    types.push(descriptor("reviewer", "Reviews diffs."));
    let params = TaskParams {
        selectable_subagent_types: types,
        ..TaskParams::default()
    };

    let exported = exported_input_schema(&task_input_schema(), &params);
    let names: Vec<&str> = property(&exported, SUBAGENT_TYPE_PARAM)
        .and_then(|p| p.get("enum"))
        .and_then(serde_json::Value::as_array)
        .expect("subagent_type enum")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();

    assert_eq!(
        names,
        vec![
            "general-purpose",
            "explore",
            "plan",
            "implementer",
            "reviewer"
        ]
    );
    let description = property(&exported, SUBAGENT_TYPE_PARAM)
        .and_then(|p| p.get("description"))
        .and_then(serde_json::Value::as_str)
        .expect("subagent_type description");
    assert!(
        description.contains("Has access to: run_terminal_command"),
        "tool summaries use the session's real tool names: {description}"
    );
}

fn task_input_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(xai_tool_types::TaskToolInput))
        .expect("task input schema serializes")
}

fn descriptor(name: &str, description: &str) -> SubagentDescriptor {
    SubagentDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        tools: None,
    }
}

fn property<'a>(schema: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    schema.get("properties")?.get(name)
}
