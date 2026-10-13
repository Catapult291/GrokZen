pub fn try_extract_concatenated_json_objects(arguments: &str) -> Option<Vec<serde_json::Value>> {
    let trimmed = arguments.trim();

    if !trimmed.starts_with('{') {
        return None;
    }

    // If it parses as valid JSON already, no recovery needed.
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return None;
    }

    // The stream deserializer handles nested braces correctly (unlike naive string splitting on "}{")
    let stream = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();

    let mut objects = Vec::new();
    for result in stream {
        match result {
            Ok(value) if value.is_object() => objects.push(value),
            _ => break,
        }
    }

    // At least 2 objects are needed for this to be concatenated JSON
    if objects.len() >= 2 {
        Some(objects)
    } else {
        None
    }
}

/// Zero-arg MCP tools (e.g. `get_me`) sometimes receive `""` from the model instead of `"{}"`, which fails JSON parsing.
pub fn normalize_empty_arguments(arguments: &str) -> &str {
    if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    }
}

/// Conservatively repair EOF-truncated tool-argument JSON (observed with cline-pass / mimo:
/// emission stops mid-call, e.g. `{"command": "ls", "timeout": `).
///
/// Drops the incomplete trailing member and closes the containers that were still open.
/// Never invents or extends a truncated string/number value: an unterminated string is
/// discarded whole, and a number cut off at EOF is discarded (`1` could have been `12`).
/// Returns `None` when nothing salvageable remains or the result still doesn't parse.
pub fn try_repair_truncated_json(arguments: &str) -> Option<serde_json::Value> {
    repair_truncated_json_impl(arguments, false)
}

/// Like [`try_repair_truncated_json`], but keeps an unterminated **string value**
/// (closes it at EOF) so a residual `{"pattern": "createToolCall` still yields
/// `pattern`. Used only after the conservative repair parses but fails the tool
/// schema (missing required field). Keys and numbers follow the conservative rules.
pub fn try_repair_truncated_json_keep_partial_strings(
    arguments: &str,
) -> Option<serde_json::Value> {
    repair_truncated_json_impl(arguments, true)
}

fn repair_truncated_json_impl(
    arguments: &str,
    keep_partial_strings: bool,
) -> Option<serde_json::Value> {
    let trimmed = arguments.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return None;
    }

    let bytes = trimmed.as_bytes();
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Object,
        Array,
    }
    struct Frame {
        kind: Kind,
        /// Byte index just past the last COMPLETE value in this frame (or just past
        /// the opening bracket when no value has completed yet).
        last_value_end: usize,
        /// Innermost-state mirror: true while a value (not a key) is expected next.
        next_is_value: bool,
    }

    let mut stack: Vec<Frame> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
                // A closed string is a value iff the innermost frame expects one.
                if let Some(frame) = stack.last() {
                    if frame.next_is_value {
                        if let Some(frame) = stack.last_mut() {
                            frame.last_value_end = i + 1;
                        }
                        // After a value: expect `,` or the container close.
                        if let Some(frame) = stack.last_mut() {
                            frame.next_is_value = false;
                        }
                    }
                    // Key strings leave next_is_value false; `:` flips it below.
                }
            }
            i += 1;
            continue;
        }
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'{' => {
                stack.push(Frame {
                    kind: Kind::Object,
                    last_value_end: i + 1,
                    next_is_value: false,
                });
                i += 1;
            }
            b'[' => {
                stack.push(Frame {
                    kind: Kind::Array,
                    last_value_end: i + 1,
                    next_is_value: true,
                });
                i += 1;
            }
            b'}' | b']' => {
                let want_object = b == b'}';
                match stack.last() {
                    Some(frame)
                        if (frame.kind == Kind::Object) == want_object && !frame.next_is_value =>
                    {
                        stack.pop();
                        if let Some(parent) = stack.last_mut() {
                            parent.last_value_end = i + 1;
                            parent.next_is_value = false;
                        } else {
                            // Root container closed cleanly; remaining text (if any)
                            // made the overall parse fail for another reason.
                            return None;
                        }
                        i += 1;
                    }
                    // `{"a":}`-style close while a value is still due: stop scanning
                    // and repair from the current stack state.
                    _ => break,
                }
            }
            b':' => {
                match stack.last_mut() {
                    Some(frame) if frame.kind == Kind::Object && !frame.next_is_value => {
                        frame.next_is_value = true;
                        i += 1;
                    }
                    // Colon where it can't belong: stop and repair what we have.
                    _ => break,
                }
            }
            b',' => match stack.last_mut() {
                Some(frame) => {
                    frame.next_is_value = frame.kind == Kind::Array;
                    i += 1;
                }
                None => break,
            },
            b't' | b'f' | b'n' => {
                match stack.last() {
                    Some(frame) if frame.next_is_value => {}
                    _ => break,
                }
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                match std::str::from_utf8(&bytes[start..i]).ok() {
                    Some("true") | Some("false") | Some("null") => {
                        if let Some(frame) = stack.last_mut() {
                            frame.last_value_end = i;
                            frame.next_is_value = false;
                        }
                    }
                    // Incomplete or invalid literal: stop; the member is dropped.
                    _ => break,
                }
            }
            b'-' | b'0'..=b'9' => {
                match stack.last() {
                    Some(frame) if frame.next_is_value => {}
                    _ => break,
                }
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                // A number cut off at EOF may have had more digits — discard it.
                if i >= bytes.len() {
                    break;
                }
                match std::str::from_utf8(&bytes[start..i])
                    .ok()
                    .and_then(|t| t.parse::<f64>().ok())
                {
                    Some(_) => {
                        if let Some(frame) = stack.last_mut() {
                            frame.last_value_end = i;
                            frame.next_is_value = false;
                        }
                    }
                    None => break,
                }
            }
            // Anything else where a token was expected: stop and repair.
            _ => break,
        }
    }

    // EOF inside a string value: optionally keep the emitted prefix (salvage path).
    // A key string at EOF is still dropped — incomplete keys cannot be completed.
    let mut closed_partial_string = false;
    if keep_partial_strings && in_string {
        if let Some(frame) = stack.last_mut()
            && frame.next_is_value
        {
            frame.last_value_end = bytes.len();
            frame.next_is_value = false;
            closed_partial_string = true;
        }
    }

    if stack.is_empty() {
        return None;
    }

    // Truncate at the innermost frame's last complete value and close every open
    // container. The prefix always ends right after a complete value (or right after
    // an opening bracket), and every outer frame is mid-value, so the closers are
    // syntactically valid.
    let mut out = trimmed[..stack.last()?.last_value_end].to_string();
    if closed_partial_string {
        // The prefix ends mid-string; add the closing quote before container closers.
        // A trailing lone backslash would escape it — drop that byte instead.
        if out.ends_with('\\') && !out.ends_with("\\\\") {
            out.pop();
        }
        out.push('"');
    }
    for frame in stack.iter().rev() {
        out.push(match frame.kind {
            Kind::Object => '}',
            Kind::Array => ']',
        });
    }
    let value = serde_json::from_str::<serde_json::Value>(&out).ok()?;
    if value.is_object() { Some(value) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_objects() {
        let args = r#"{"target_file": "a.java"}{"target_file": "b.java"}{"target_file": "c.java"}"#;
        let objects = try_extract_concatenated_json_objects(args).unwrap();
        assert_eq!(objects.len(), 3);
        assert_eq!(
            objects.first().and_then(|o| o.get("target_file")),
            Some(&serde_json::json!("a.java"))
        );
    }

    #[test]
    fn test_no_extract_for_valid_single_object() {
        assert!(
            try_extract_concatenated_json_objects(r#"{"target_file": "src/main.rs"}"#).is_none()
        );
    }

    #[test]
    fn test_no_extract_for_valid_object_with_braces_in_value() {
        assert!(
            try_extract_concatenated_json_objects(r#"{"command": "echo '}{' && ls"}"#).is_none()
        );
    }

    #[test]
    fn test_no_extract_for_array() {
        assert!(
            try_extract_concatenated_json_objects(
                r#"[{"target_file": "a.java"}, {"target_file": "b.java"}]"#
            )
            .is_none()
        );
    }

    #[test]
    fn test_no_extract_for_empty_or_non_json() {
        assert!(try_extract_concatenated_json_objects("").is_none());
        assert!(try_extract_concatenated_json_objects("not json").is_none());
    }

    #[test]
    fn test_extract_with_nested_braces() {
        let args = r#"{"file": "a.rs", "opts": {"line": 1}}{"file": "b.rs", "opts": {"line": 2}}"#;
        let objects = try_extract_concatenated_json_objects(args).unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(
            objects
                .first()
                .and_then(|o| o.get("opts"))
                .and_then(|o| o.get("line")),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn test_extract_with_whitespace_between_objects() {
        let objects = try_extract_concatenated_json_objects(r#"{"a": 1} {"b": 2}"#).unwrap();
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn test_extract_real_world_20_files() {
        let mut args = String::new();
        for i in 0..20 {
            args.push_str(&format!(r#"{{"target_file": "src/File{i}.java"}}"#));
        }
        let objects = try_extract_concatenated_json_objects(&args).unwrap();
        assert_eq!(objects.len(), 20);
    }

    #[test]
    fn test_no_extract_for_truncated_json() {
        assert!(try_extract_concatenated_json_objects(r#"{"a": 1} garbage"#).is_none());
    }

    /// Parse after normalizing; mirrors the production pattern in handle_tool_call.
    fn normalize_and_parse(arguments: &str) -> serde_json::Value {
        let normalized = normalize_empty_arguments(arguments);
        serde_json::from_str(normalized).unwrap_or_else(|_| serde_json::json!({"raw": arguments}))
    }

    #[test]
    fn empty_string_becomes_empty_object() {
        assert_eq!(normalize_and_parse(""), serde_json::json!({}));
    }

    #[test]
    fn whitespace_only_becomes_empty_object() {
        assert_eq!(normalize_and_parse("   "), serde_json::json!({}));
        assert_eq!(normalize_and_parse("\n\t"), serde_json::json!({}));
    }

    #[test]
    fn valid_json_unchanged() {
        assert_eq!(
            normalize_and_parse(r#"{"query": "test"}"#),
            serde_json::json!({"query": "test"})
        );
    }

    #[test]
    fn empty_object_string_unchanged() {
        assert_eq!(normalize_and_parse("{}"), serde_json::json!({}));
    }

    #[test]
    fn invalid_json_falls_back_to_raw() {
        let result = normalize_and_parse("not json");
        assert_eq!(result.get("raw"), Some(&serde_json::json!("not json")));
    }

    #[test]
    fn complex_args_with_arrays_unchanged() {
        let args = r#"{"pages": [{"title": "Test"}], "limit": 10}"#;
        let result = normalize_and_parse(args);
        assert!(result.get("pages").is_some_and(|p| p.is_array()));
        assert_eq!(result.get("limit"), Some(&serde_json::json!(10)));
    }

    #[test]
    fn normalize_empty_returns_braces() {
        assert_eq!(normalize_empty_arguments(""), "{}");
        assert_eq!(normalize_empty_arguments("   "), "{}");
        assert_eq!(normalize_empty_arguments("\n\t"), "{}");
    }

    #[test]
    fn normalize_non_empty_passthrough() {
        assert_eq!(normalize_empty_arguments(r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(normalize_empty_arguments("not json"), "not json");
    }

    // ── try_repair_truncated_json ─────────────────────────────────────

    #[test]
    fn repair_drops_dangling_key_with_no_value() {
        // Live cline-pass signature: emission stops right after `"timeout": `.
        let repaired =
            try_repair_truncated_json(r#"{"command": "ls", "description": "d", "timeout": "#)
                .expect("repair");
        assert_eq!(repaired["command"], "ls");
        assert_eq!(repaired["description"], "d");
        assert!(repaired.get("timeout").is_none());
    }

    #[test]
    fn repair_drops_unterminated_string_value() {
        let repaired = try_repair_truncated_json(r#"{"path": "C:\Users"#).expect("repair");
        assert!(repaired.as_object().unwrap().is_empty());
    }

    #[test]
    fn repair_drops_ambiguous_truncated_number() {
        // `12` could have been `123` — must not be kept.
        let repaired = try_repair_truncated_json(r#"{"a": 1, "limit": 12"#).expect("repair");
        assert_eq!(repaired["a"], 1);
        assert!(repaired.get("limit").is_none());
    }

    #[test]
    fn repair_keeps_complete_number_before_delimiter() {
        let repaired = try_repair_truncated_json(r#"{"limit": 10, "x": "#).expect("repair");
        assert_eq!(repaired["limit"], 10);
    }

    #[test]
    fn repair_closes_nested_open_object() {
        // Live signature: `{"path": ` from a spliced second call.
        let repaired = try_repair_truncated_json(r#"{"path": "#).expect("repair");
        assert!(repaired.as_object().unwrap().is_empty());
    }

    #[test]
    fn repair_nested_object_drops_inner_dangling_member() {
        let repaired =
            try_repair_truncated_json(r#"{"opts": {"line": 1, "col": "#).expect("repair");
        assert_eq!(repaired["opts"]["line"], 1);
        assert!(repaired["opts"].get("col").is_none());
    }

    #[test]
    fn repair_keeps_complete_literals() {
        let repaired = try_repair_truncated_json(r#"{"flag": true, "mode": "#).expect("repair");
        assert_eq!(repaired["flag"], true);
    }

    #[test]
    fn repair_drops_incomplete_literal() {
        let repaired = try_repair_truncated_json(r#"{"a": 1, "flag": tr"#).expect("repair");
        assert_eq!(repaired["a"], 1);
        assert!(repaired.get("flag").is_none());
    }

    #[test]
    fn repair_handles_array_element_truncation() {
        let repaired = try_repair_truncated_json(r#"{"pages": [1, 2"#).expect("repair");
        assert_eq!(repaired["pages"], serde_json::json!([1]));
    }

    #[test]
    fn repair_none_for_valid_json() {
        assert!(try_repair_truncated_json(r#"{"a": 1}"#).is_none());
    }

    #[test]
    fn repair_none_for_non_object() {
        assert!(try_repair_truncated_json("[1, 2").is_none());
        assert!(try_repair_truncated_json("not json").is_none());
        assert!(try_repair_truncated_json("").is_none());
    }

    #[test]
    fn repair_none_when_root_already_closed() {
        // Root closed then garbage: not a truncation case.
        assert!(try_repair_truncated_json(r#"{"a": 1} {"b": "#).is_none());
    }

    #[test]
    fn repair_none_for_empty_object_truncation_only() {
        // Nothing salvageable beyond `{}` — still valid, returns the empty object.
        let repaired = try_repair_truncated_json(r#"{"timeout": "#).expect("repair");
        assert!(repaired.as_object().unwrap().is_empty());
    }

    #[test]
    fn repair_preserves_escaped_quotes_in_complete_string() {
        let repaired =
            try_repair_truncated_json(r#"{"command": "echo \"hi\"", "timeout": "#).expect("repair");
        assert_eq!(repaired["command"], "echo \"hi\"");
    }

    #[test]
    fn salvage_keeps_partial_string_value() {
        let salvaged = try_repair_truncated_json_keep_partial_strings(
            r#"{"head_limit": 5, "path": "proxy.mjs", "pattern": "createToolCall"#,
        )
        .expect("salvage");
        assert_eq!(salvaged["head_limit"], 5);
        assert_eq!(salvaged["path"], "proxy.mjs");
        assert_eq!(salvaged["pattern"], "createToolCall");
        // Conservative repair still drops the incomplete pattern member.
        let conservative = try_repair_truncated_json(
            r#"{"head_limit": 5, "path": "proxy.mjs", "pattern": "createToolCall"#,
        )
        .expect("repair");
        assert!(conservative.get("pattern").is_none());
    }

    #[test]
    fn salvage_still_drops_dangling_key_and_number() {
        let salvaged =
            try_repair_truncated_json_keep_partial_strings(r#"{"a": 1, "b": "#).expect("salvage");
        assert_eq!(salvaged["a"], 1);
        assert!(salvaged.get("b").is_none());

        let salvaged =
            try_repair_truncated_json_keep_partial_strings(r#"{"limit": 12"#).expect("salvage");
        assert!(salvaged.as_object().unwrap().is_empty());
    }

    #[test]
    fn salvage_none_for_valid_json() {
        assert!(try_repair_truncated_json_keep_partial_strings(r#"{"a": 1}"#).is_none());
    }
}
