//! Thin per-client context (Serena-inspired, without Serena's prompt/mode system).
//!
//! A context is a named YAML file with:
//!
//! * `name`
//! * `structured_tool_output` (`true` / `false` / `null`=auto → off)
//! * optional `excluded_tools` (catalog filter; may be empty)
//! * optional `deny_text_search` (hooks remind hard-deny Grep/Glob/shell text-search)
//! * short `notes`
//!
//! Named contexts `codex` and any `oaicompat*` also opt into OpenAI tool-schema
//! sanitization at `list_tools` time (see `openai_schema`).
//!
//! Built-ins live in `crates/astrolabe-mcp/contexts/*.yml` and are embedded
//! at compile time. `--context=NAME` / `ASTROLABE_CONTEXT` selects one.
//!
//! Precedence for structured output:
//! 1. `ASTROLABE_STRUCTURED` when set to a recognised true/false token
//! 2. context `structured_tool_output` when bool
//! 3. `null`/auto → off (safe for Claude Code)
//!
//! To add a client: drop `<name>.yml` next to the others, add a row to
//! [`BUILTINS`], and mention it in the README. Custom files work too:
//! `--context=/path/to/mine.yml`.

use std::path::Path;

const BUILTINS: &[(&str, &str)] = &[
    ("claude-code", include_str!("../contexts/claude-code.yml")),
    ("codex", include_str!("../contexts/codex.yml")),
    ("cursor", include_str!("../contexts/cursor.yml")),
    ("default", include_str!("../contexts/default.yml")),
    ("readonly", include_str!("../contexts/readonly.yml")),
];

/// Client-specific MCP behaviour. Intentionally small: no prompt templates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientContext {
    pub name: String,
    /// Builtin name or YAML file stem used to select this context (for
    /// `openai_tool_compatible` when the YAML `name:` field differs).
    pub load_key: String,
    /// `None` means auto (resolve as off unless `ASTROLABE_STRUCTURED` overrides).
    pub structured_tool_output: Option<bool>,
    pub excluded_tools: Vec<String>,
    pub notes: String,
    /// Hard-deny host Grep / Glob / shell text-search on first use (hooks remind).
    /// Default false: keep soft GREP_THRESHOLD behaviour. Overridable by
    /// `ASTROLABE_DENY_TEXT_SEARCH` and `hooks remind --deny-text-search`.
    pub deny_text_search: bool,
}

impl ClientContext {
    /// Built-in `default` context (structured off, no exclusions).
    pub fn default_builtin() -> Self {
        Self::builtin("default").expect("default context is embedded")
    }

    pub fn builtin(name: &str) -> Option<Self> {
        let src = BUILTINS.iter().find(|(n, _)| *n == name)?.1;
        Some(
            parse_context_yaml(src, name)
                .unwrap_or_else(|err| panic!("embedded context {name}.yml is invalid: {err}")),
        )
    }

    pub fn builtin_names() -> Vec<&'static str> {
        BUILTINS.iter().map(|(n, _)| *n).collect()
    }

    /// Load a built-in name, or a YAML file when `name_or_path` looks like a path.
    pub fn load(name_or_path: &str) -> anyhow::Result<Self> {
        let trimmed = name_or_path.trim();
        if trimmed.is_empty() {
            anyhow::bail!("context name is empty");
        }
        if looks_like_yaml_path(trimmed) {
            let path = Path::new(trimmed);
            let src = std::fs::read_to_string(path).map_err(|err| {
                anyhow::anyhow!("failed to read context file {}: {err}", path.display())
            })?;
            let fallback = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("custom");
            return parse_context_yaml(&src, fallback);
        }
        Self::builtin(trimmed).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown context `{trimmed}`. built-in: {}. or pass a path to a .yml file",
                Self::builtin_names().join(", ")
            )
        })
    }

    pub fn excludes(&self, tool: &str) -> bool {
        self.excluded_tools.iter().any(|name| name == tool)
    }

    /// Effective structured flag before env override: `null`/auto → false.
    pub fn structured_or_auto_off(&self) -> bool {
        self.structured_tool_output.unwrap_or(false)
    }

    /// Whether `list_tools` should rewrite input schemas for OpenAI/Codex.
    ///
    /// Matches Serena's `openai_tool_compatible` trigger for `codex` and any
    /// future `oaicompat*` context name (e.g. `oaicompat-agent`). Checks both
    /// the YAML `name` and the load key / file stem so a custom file named
    /// `oaicompat-*.yml` still opts in even if its `name:` field differs.
    pub fn openai_tool_compatible(&self) -> bool {
        is_openai_compat_name(&self.name) || is_openai_compat_name(&self.load_key)
    }
}

fn is_openai_compat_name(s: &str) -> bool {
    s == "codex" || s.starts_with("oaicompat")
}

/// CLI `--context` wins over `ASTROLABE_CONTEXT`; both empty → `default`.
pub fn resolve_context_name<'a>(cli: Option<&'a str>, env: Option<&'a str>) -> &'a str {
    if let Some(name) = cli.map(str::trim).filter(|s| !s.is_empty()) {
        return name;
    }
    if let Some(name) = env.map(str::trim).filter(|s| !s.is_empty()) {
        return name;
    }
    "default"
}

fn looks_like_yaml_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.ends_with(".yml") || s.ends_with(".yaml")
}

fn parse_context_yaml(src: &str, fallback_name: &str) -> anyhow::Result<ClientContext> {
    let mut name = fallback_name.to_string();
    let mut notes = String::new();
    let mut structured_tool_output: Option<bool> = None;
    let mut excluded_tools = Vec::new();
    let mut list_key: Option<String> = None;
    let mut saw_structured = false;
    let mut deny_text_search = false;

    for (idx, raw) in src.lines().enumerate() {
        let line_no = idx + 1;
        let stripped = raw.trim_end().trim_end_matches('\r');
        let trimmed = stripped.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = list_item(trimmed) {
            match list_key.as_deref() {
                Some("excluded_tools") => excluded_tools.push(unquote(item)),
                // Forward-compatible: unknown list keys skip their items
                // rather than bricking startup on a future/custom field.
                Some(_) => {}
                None => anyhow::bail!("line {line_no}: list item without a key"),
            }
            continue;
        }
        list_key = None;
        let Some((key, rest)) = split_key_value(trimmed) else {
            anyhow::bail!("line {line_no}: expected `key: value`, got `{trimmed}`");
        };
        match key {
            "name" => {
                if !rest.is_empty() {
                    name = unquote(rest);
                }
            }
            "notes" | "description" | "comment" => {
                if !rest.is_empty() {
                    notes = unquote(rest);
                }
            }
            "structured_tool_output" => {
                saw_structured = true;
                structured_tool_output = parse_bool_or_null(rest).map_err(|err| {
                    anyhow::anyhow!("line {line_no}: structured_tool_output: {err}")
                })?;
            }
            "deny_text_search" => {
                deny_text_search = parse_bool_or_null(rest)
                    .map_err(|err| anyhow::anyhow!("line {line_no}: deny_text_search: {err}"))?
                    .unwrap_or(false);
            }
            "excluded_tools" => {
                // Re-key clears previous entries so the last block wins.
                excluded_tools.clear();
                if rest.is_empty() {
                    list_key = Some(key.to_string());
                } else if rest == "[]" {
                    // already cleared
                } else if let Some(items) = parse_flow_string_list(rest) {
                    excluded_tools.extend(items);
                } else {
                    anyhow::bail!(
                        "line {line_no}: excluded_tools must be a YAML list (use `- item` lines or a flow list like `[a, b]`)"
                    );
                }
            }
            _ => {
                // Forward-compatible: ignore unknown scalars so a future field
                // in a custom file does not brick startup. If the value is empty,
                // subsequent `- item` lines are also skipped (see list_key match).
                if rest.is_empty() {
                    list_key = Some(key.to_string());
                }
            }
        }
    }

    // Missing key → auto (None), same as explicit null.
    let _ = saw_structured;

    Ok(ClientContext {
        name,
        load_key: fallback_name.to_string(),
        structured_tool_output,
        excluded_tools,
        notes,
        deny_text_search,
    })
}

/// Parse a simple YAML flow sequence of scalars: `[a, b, "c"]`.
fn parse_flow_string_list(raw: &str) -> Option<Vec<String>> {
    let s = raw.trim();
    let inner = s.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    let mut items = Vec::new();
    for part in inner.split(',') {
        let item = unquote(part.trim());
        if item.is_empty() {
            return None;
        }
        items.push(item);
    }
    Some(items)
}

fn list_item(trimmed: &str) -> Option<&str> {
    trimmed.strip_prefix("- ").map(str::trim)
}

fn split_key_value(trimmed: &str) -> Option<(&str, &str)> {
    let (key, rest) = trimmed.split_once(':')?;
    Some((key.trim(), rest.trim()))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\'')
        {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn parse_bool_or_null(raw: &str) -> anyhow::Result<Option<bool>> {
    match unquote(raw).as_str() {
        "true" | "True" | "TRUE" | "yes" | "on" | "1" => Ok(Some(true)),
        "false" | "False" | "FALSE" | "no" | "off" | "0" => Ok(Some(false)),
        "null" | "Null" | "NULL" | "~" | "" => Ok(None),
        other => anyhow::bail!("expected true/false/null, got `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_loads() {
        for name in ClientContext::builtin_names() {
            let ctx = ClientContext::builtin(name).expect(name);
            assert_eq!(ctx.name, name);
        }
    }

    #[test]
    fn readonly_excludes_nothing_catalog_is_already_read_only() {
        let ctx = ClientContext::builtin("readonly").unwrap();
        assert!(ctx.excluded_tools.is_empty());
        assert!(!ctx.excludes("find_references"));
        assert_eq!(ctx.structured_tool_output, Some(false));
    }

    #[test]
    fn claude_code_disables_structured() {
        let ctx = ClientContext::builtin("claude-code").unwrap();
        assert_eq!(ctx.structured_tool_output, Some(false));
        assert!(!ctx.structured_or_auto_off());
        assert!(ctx.excluded_tools.is_empty());
    }

    #[test]
    fn default_matches_claude_code_structured_flag() {
        let default = ClientContext::default_builtin();
        let claude = ClientContext::builtin("claude-code").unwrap();
        assert_eq!(
            default.structured_tool_output, claude.structured_tool_output,
            "product default must stay safe for Claude Code"
        );
        assert!(!default.structured_or_auto_off());
    }

    #[test]
    fn cursor_and_codex_enable_structured() {
        assert_eq!(
            ClientContext::builtin("cursor")
                .unwrap()
                .structured_tool_output,
            Some(true)
        );
        assert_eq!(
            ClientContext::builtin("codex")
                .unwrap()
                .structured_tool_output,
            Some(true)
        );
    }

    #[test]
    fn openai_tool_compatible_for_codex_and_oaicompat() {
        assert!(ClientContext::builtin("codex")
            .unwrap()
            .openai_tool_compatible());
        assert!(!ClientContext::builtin("cursor")
            .unwrap()
            .openai_tool_compatible());
        assert!(!ClientContext::default_builtin().openai_tool_compatible());
        let mut custom = ClientContext::default_builtin();
        custom.name = "oaicompat-agent".into();
        assert!(custom.openai_tool_compatible());
        // Load key / stem also opts in when YAML name differs.
        let mut via_stem = ClientContext::default_builtin();
        via_stem.name = "custom-agent".into();
        via_stem.load_key = "oaicompat-agent".into();
        assert!(via_stem.openai_tool_compatible());
    }

    #[test]
    fn parse_null_structured_is_auto_off() {
        let ctx = parse_context_yaml(
            "name: auto\nstructured_tool_output: null\nnotes: auto\n",
            "auto",
        )
        .unwrap();
        assert_eq!(ctx.structured_tool_output, None);
        assert!(!ctx.structured_or_auto_off());
    }

    #[test]
    fn parse_excluded_tools_list() {
        let ctx = parse_context_yaml(
            r#"
name: demo
notes: filter hook
structured_tool_output: false
excluded_tools:
  - find_references
  - get_languages
"#,
            "demo",
        )
        .unwrap();
        assert!(ctx.excludes("find_references"));
        assert!(ctx.excludes("get_languages"));
        assert!(!ctx.excludes("find_symbol"));
    }

    #[test]
    fn resolve_name_prefers_cli_then_env() {
        assert_eq!(
            resolve_context_name(Some("cursor"), Some("codex")),
            "cursor"
        );
        assert_eq!(resolve_context_name(None, Some("codex")), "codex");
        assert_eq!(resolve_context_name(Some("  "), Some("codex")), "codex");
        assert_eq!(resolve_context_name(None, None), "default");
        assert_eq!(resolve_context_name(Some(""), Some("")), "default");
    }

    #[test]
    fn load_unknown_name_lists_builtins() {
        let err = ClientContext::load("not-a-client").unwrap_err().to_string();
        assert!(err.contains("claude-code"), "{err}");
        assert!(err.contains("default"), "{err}");
    }

    #[test]
    fn load_from_yaml_path() {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-ctx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        std::fs::write(
            &path,
            "name: mine\nstructured_tool_output: true\nexcluded_tools: []\nnotes: custom\n",
        )
        .unwrap();
        let ctx = ClientContext::load(path.to_str().unwrap()).unwrap();
        assert_eq!(ctx.name, "mine");
        assert_eq!(ctx.load_key, "mine");
        assert_eq!(ctx.structured_tool_output, Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_list_keys_are_skipped() {
        let ctx = parse_context_yaml(
            r#"
name: demo
future_list:
  - ignored
excluded_tools:
  - goto_definition
"#,
            "demo",
        )
        .unwrap();
        assert!(ctx.excludes("goto_definition"));
        assert_eq!(ctx.excluded_tools, vec!["goto_definition".to_string()]);
    }

    #[test]
    fn excluded_tools_rekey_replaces() {
        let ctx = parse_context_yaml(
            r#"
name: demo
excluded_tools:
  - old_tool
excluded_tools:
  - new_tool
"#,
            "demo",
        )
        .unwrap();
        assert!(!ctx.excludes("old_tool"));
        assert!(ctx.excludes("new_tool"));
        assert_eq!(ctx.excluded_tools, vec!["new_tool".to_string()]);
    }

    #[test]
    fn excluded_tools_flow_list() {
        let ctx = parse_context_yaml(
            "name: demo\nexcluded_tools: [find_references, get_languages]\nnotes: flow\n",
            "demo",
        )
        .unwrap();
        assert!(ctx.excludes("find_references"));
        assert!(ctx.excludes("get_languages"));
    }

    #[test]
    fn deny_text_search_parses_from_yaml() {
        let on =
            parse_context_yaml("name: demo\ndeny_text_search: true\nnotes: x\n", "demo").unwrap();
        assert!(on.deny_text_search);
        let off =
            parse_context_yaml("name: demo\ndeny_text_search: false\nnotes: x\n", "demo").unwrap();
        assert!(!off.deny_text_search);
        let missing = parse_context_yaml("name: demo\nnotes: x\n", "demo").unwrap();
        assert!(!missing.deny_text_search);
    }

    #[test]
    fn openai_compat_from_path_stem() {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-ctx-oaicompat-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oaicompat-agent.yml");
        std::fs::write(
            &path,
            "name: custom\nstructured_tool_output: true\nexcluded_tools: []\nnotes: x\n",
        )
        .unwrap();
        let ctx = ClientContext::load(path.to_str().unwrap()).unwrap();
        assert_eq!(ctx.name, "custom");
        assert_eq!(ctx.load_key, "oaicompat-agent");
        assert!(ctx.openai_tool_compatible());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
