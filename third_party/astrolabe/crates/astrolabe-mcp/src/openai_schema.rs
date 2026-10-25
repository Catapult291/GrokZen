//! OpenAI / Codex tool-schema sanitizer (Serena `_sanitize_for_openai_tools` port).
//!
//! OpenAI-compatible tool APIs reject JSON Schema `integer` and nullable unions.
//! When a client context opts in ([`ClientContext::openai_tool_compatible`]),
//! `list_tools` rewrites each tool's `inputSchema` through
//! [`sanitize_for_openai_tools`] after the `excluded_tools` filter. Domain Rust
//! types (e.g. `usize` for `budget_tokens`) stay unchanged.

use serde_json::{json, Map, Value};

/// Rewrite a JSON Schema value for OpenAI-compatible tool parameters.
///
/// - `integer` → `number` with `multipleOf: 1` when absent
/// - strip `null` from `type` arrays
/// - coerce int-only `enum` lists to `number`
/// - collapse trivial `oneOf` / `anyOf` (type+null, or identical after sanitize)
pub fn sanitize_for_openai_tools(schema: Value) -> Value {
    walk(schema)
}

fn walk(node: Value) -> Value {
    let Value::Object(mut obj) = node else {
        return node;
    };

    sanitize_type_field(&mut obj);
    sanitize_int_enum(&mut obj);
    simplify_one_of_any_of(&mut obj);

    for child_key in ["properties", "patternProperties", "definitions", "$defs"] {
        if let Some(Value::Object(children)) = obj.get_mut(child_key) {
            let keys: Vec<String> = children.keys().cloned().collect();
            for k in keys {
                if let Some(child) = children.remove(&k) {
                    children.insert(k, walk(child));
                }
            }
        }
    }

    if let Some(items) = obj.remove("items") {
        obj.insert("items".into(), walk(items));
    }

    if let Some(Value::Array(all_of)) = obj.get_mut("allOf") {
        *all_of = all_of.drain(..).map(walk).collect();
    }

    for key in ["if", "then", "else"] {
        if let Some(child) = obj.remove(key) {
            obj.insert(key.into(), walk(child));
        }
    }

    Value::Object(obj)
}

fn sanitize_type_field(obj: &mut Map<String, Value>) {
    match obj.get("type").cloned() {
        Some(Value::String(t)) if t == "integer" => {
            obj.insert("type".into(), Value::String("number".into()));
            obj.entry("multipleOf".to_string())
                .or_insert_with(|| json!(1));
        }
        Some(Value::Array(types)) => {
            let had_integer = types.iter().any(|x| x.as_str() == Some("integer"));
            let mut t2: Vec<Value> = types
                .into_iter()
                .filter(|x| x.as_str() != Some("null"))
                .map(|x| match x.as_str() {
                    Some("integer") => Value::String("number".into()),
                    _ => x,
                })
                .collect();
            if t2.is_empty() {
                t2.push(Value::String("object".into()));
            }
            let next_type = if t2.len() == 1 {
                t2.pop().unwrap()
            } else {
                Value::Array(t2)
            };
            obj.insert("type".into(), next_type);
            // Only force multipleOf:1 when an integer was present — not for
            // pure number unions (e.g. ["number","string"]).
            if had_integer {
                obj.entry("multipleOf".to_string())
                    .or_insert_with(|| json!(1));
            }
        }
        _ => {}
    }
}

fn sanitize_int_enum(obj: &mut Map<String, Value>) {
    let Some(Value::Array(vals)) = obj.get("enum") else {
        return;
    };
    if vals.is_empty() || !vals.iter().all(is_json_int) {
        return;
    }
    obj.entry("type".to_string())
        .or_insert_with(|| Value::String("number".into()));
    obj.entry("multipleOf".to_string())
        .or_insert_with(|| json!(1));
}

fn is_json_int(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.is_i64() || n.is_u64(),
        _ => false,
    }
}

fn simplify_one_of_any_of(obj: &mut Map<String, Value>) {
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Array(subs)) = obj.get(key).cloned() else {
            continue;
        };

        // Special case: schema | null → fold to the full non-null schema
        // (all fields: type, minimum, format, …), then sanitize via walk.
        if subs.len() == 2 {
            let null_i = subs
                .iter()
                .position(|s| s.get("type").and_then(Value::as_str) == Some("null"));
            let non_null_i = subs
                .iter()
                .position(|s| s.get("type").and_then(Value::as_str) != Some("null"));
            if let (Some(ni), Some(nni)) = (null_i, non_null_i) {
                if ni != nni {
                    let walked = walk(subs[nni].clone());
                    obj.remove(key);
                    if let Value::Object(only_obj) = walked {
                        for (k, v) in only_obj {
                            obj.entry(k).or_insert(v);
                        }
                    }
                    continue;
                }
            }
        }

        let simplified: Vec<Value> = subs.into_iter().map(walk).collect();
        let canon: Vec<String> = simplified
            .iter()
            .map(|x| serde_json::to_string(x).unwrap_or_default())
            .collect();
        let all_same = !canon.is_empty() && canon.iter().all(|c| c == &canon[0]);
        if all_same {
            let only = simplified.into_iter().next().unwrap();
            obj.remove(key);
            if let Value::Object(only_obj) = only {
                for (k, v) in only_obj {
                    obj.entry(k).or_insert(v);
                }
            }
        } else {
            obj.insert(key.into(), Value::Array(simplified));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contains_integer_type(v: &Value) -> bool {
        match v {
            Value::Object(o) => {
                match o.get("type") {
                    Some(Value::String(s)) if s == "integer" => return true,
                    Some(Value::Array(a)) if a.iter().any(|x| x.as_str() == Some("integer")) => {
                        return true
                    }
                    _ => {}
                }
                o.values().any(contains_integer_type)
            }
            Value::Array(a) => a.iter().any(contains_integer_type),
            _ => false,
        }
    }

    fn contains_null_in_type_array(v: &Value) -> bool {
        match v {
            Value::Object(o) => {
                if let Some(Value::Array(a)) = o.get("type") {
                    if a.iter().any(|x| x.as_str() == Some("null")) {
                        return true;
                    }
                }
                o.values().any(contains_null_in_type_array)
            }
            Value::Array(a) => a.iter().any(contains_null_in_type_array),
            _ => false,
        }
    }

    #[test]
    fn integer_becomes_number_with_multiple_of() {
        let out = sanitize_for_openai_tools(json!({
            "type": "integer",
            "format": "uint",
            "minimum": 0
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 1);
        assert_eq!(out["format"], "uint");
        assert_eq!(out["minimum"], 0);
    }

    #[test]
    fn preserves_existing_multiple_of() {
        let out = sanitize_for_openai_tools(json!({
            "type": "integer",
            "multipleOf": 2
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 2);
    }

    #[test]
    fn strips_null_from_type_union() {
        let out = sanitize_for_openai_tools(json!({
            "type": ["string", "null"]
        }));
        assert_eq!(out["type"], "string");
        assert!(!contains_null_in_type_array(&out));
    }

    #[test]
    fn strips_null_and_rewrites_integer_in_union() {
        let out = sanitize_for_openai_tools(json!({
            "type": ["integer", "null"]
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 1);
    }

    #[test]
    fn int_only_enum_gets_number_type() {
        let out = sanitize_for_openai_tools(json!({
            "enum": [1, 2, 3]
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 1);
        assert_eq!(out["enum"], json!([1, 2, 3]));
    }

    #[test]
    fn collapses_type_null_oneof() {
        let out = sanitize_for_openai_tools(json!({
            "oneOf": [
                {"type": "string"},
                {"type": "null"}
            ]
        }));
        assert_eq!(out["type"], "string");
        assert!(out.get("oneOf").is_none());
    }

    #[test]
    fn collapses_integer_null_anyof_to_number() {
        let out = sanitize_for_openai_tools(json!({
            "anyOf": [
                {"type": "integer"},
                {"type": "null"}
            ]
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 1);
        assert!(out.get("anyOf").is_none());
    }

    #[test]
    fn collapses_identical_oneof_after_integer_rewrite() {
        let out = sanitize_for_openai_tools(json!({
            "oneOf": [
                {"type": "integer", "minimum": 0},
                {"type": "integer", "minimum": 0}
            ]
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 1);
        assert_eq!(out["minimum"], 0);
        assert!(out.get("oneOf").is_none());
    }

    #[test]
    fn recurses_into_properties() {
        let out = sanitize_for_openai_tools(json!({
            "type": "object",
            "properties": {
                "budget_tokens": {
                    "type": "integer",
                    "format": "uint",
                    "minimum": 0
                },
                "path_filter": {
                    "type": ["string", "null"]
                }
            }
        }));
        assert!(!contains_integer_type(&out));
        assert!(!contains_null_in_type_array(&out));
        assert_eq!(out["properties"]["budget_tokens"]["type"], "number");
        assert_eq!(out["properties"]["budget_tokens"]["multipleOf"], 1);
        assert_eq!(out["properties"]["path_filter"]["type"], "string");
    }

    #[test]
    fn empty_type_array_after_null_strip_becomes_object() {
        let out = sanitize_for_openai_tools(json!({
            "type": ["null"]
        }));
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn null_oneof_merges_full_non_null_fields() {
        let out = sanitize_for_openai_tools(json!({
            "oneOf": [
                {"type": "integer", "minimum": 0, "format": "uint"},
                {"type": "null"}
            ]
        }));
        assert_eq!(out["type"], "number");
        assert_eq!(out["multipleOf"], 1);
        assert_eq!(out["minimum"], 0);
        assert_eq!(out["format"], "uint");
        assert!(out.get("oneOf").is_none());
    }

    #[test]
    fn pure_number_union_does_not_get_multiple_of() {
        let out = sanitize_for_openai_tools(json!({
            "type": ["number", "string"]
        }));
        assert!(out.get("multipleOf").is_none(), "{out}");
        assert_eq!(out["type"], json!(["number", "string"]));
    }
}
