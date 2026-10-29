use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use astrolabe_core::{
    budget::{estimate_tokens, truncate_ranked, TokenBudget},
    cache::{FileCache, DEFAULT_MEMORY_BUDGET_BYTES},
    graph::{cached_centrality, rank_and_match},
    render::symbol_line,
    types::RelPath,
    CodeSymbol, FileId,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        CustomRequest, CustomResult, Implementation, InitializeRequestParams, InitializeResult,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
        ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::loop_guard::{LoopGuard, Verdict};

mod search_shape;
mod shared;
mod symbol_outline;
use crate::{
    context::ClientContext,
    index::RepoIndex,
    knobs::McpKnobs,
    openai_schema::sanitize_for_openai_tools,
    root::resolve_call_index_root,
    session::{
        self, activate_absolute_root, append_stale_note, run_barrier, start_idle_ticker,
        start_root_indexing, teardown_all, BarrierEffect, DispatchState, RootSession,
    },
    TOOL_CATALOG_TTL_MS,
};

pub(crate) use crate::session::IndexState;

const DEFAULT_BUDGET: usize = 2_500;
/// `resolve_context` budget for counting/caller tasks (~3 KB of output).
const COMPACT_CONTEXT_BUDGET: usize = 700;
/// 资源卸载阈值基线：正文 token 估算超过阈值（含 SLACK）时转存为
/// MCP resource。`ASTROLABE_RESOURCE_THRESHOLD` 可调，格式错误回退默认。
pub(crate) const DEFAULT_RESOURCE_THRESHOLD_TOKENS: usize = 4_000;
/// budget_tokens 硬上限：动态阈值取 `max(基线, clamp(caller_budget))` 时
/// 防止异常大的 budget 把卸载阈值顶到天上。`ASTROLABE_MAX_BUDGET_TOKENS`
/// 可调，格式错误回退默认。
pub(crate) const DEFAULT_MAX_BUDGET_TOKENS: usize = 32_000;
/// 阈值判定双保险：`chars/4` 估算器对真实 tokenizer 的偏差容忍量。
pub(crate) const RESOURCE_SLACK_TOKENS: usize = 128;
/// 单行展示的最大列宽（字符数）：超长匹配行只展示命中点附近的安全切片。
const MAX_LINE_COLUMNS: usize = 250;
/// 切片窗口在命中点前后各带的上下文字符数。
const CONTEXT_PADDING: usize = 90;
/// 匹配段自身封顶：`.*` 类命中可能横跨上万字符，只展示前 40 字符。
const MAX_MATCH_SPAN_CHARS: usize = 40;
/// search_code 头部（confidence/mode/截断状态行）的固定 token 开销估算，
/// 从 budget 中先扣除，保证整个 body 不在阈值边界溢出。
const SEARCH_HEADER_TOKENS: usize = 35;
const BYTES_PER_MB: u64 = 1024 * 1024;

pub(crate) fn default_budget() -> usize {
    DEFAULT_BUDGET
}

fn default_include_body() -> bool {
    true
}

/// Query looks like a filesystem path (absolute, known code extension, or a
/// trailing-slash directory), not a symbol name / `Type/method` path.
/// Those belong in `path_filter` (or `query="*"` + `path_filter` for outlines).
pub(crate) fn looks_like_filesystem_path(query: &str) -> bool {
    let q = query.trim().trim_start_matches("./");
    if q.is_empty() || q == "*" {
        return false;
    }
    // Absolute / home paths: require a second separator, a trailing slash, or a
    // known code extension. A lone `/Name` (e.g. `/pager`) is a symbol-path
    // probe — normalize_symbol_query strips the slash — not a filesystem path.
    if q.starts_with('/') || q.starts_with('\\') {
        let rest = &q[1..];
        if rest.is_empty() {
            return false;
        }
        if rest.contains('/') || rest.contains('\\') || q.ends_with('/') {
            return true;
        }
        if segment_has_known_code_ext(rest) {
            return true;
        }
        return false;
    }
    if q.starts_with("~/") || q.starts_with("~\\") {
        return true;
    }
    // Windows drive: `C:\...` or `C:/...`
    let bytes = q.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return true;
    }
    if q.contains('\\') {
        return true;
    }
    // Directory written with a trailing slash.
    if q.ends_with('/') && q.contains('/') {
        return true;
    }
    // Last path segment has a known code extension (`pkg/foo.rs`, `cloud.py`).
    let last = q.rsplit('/').next().unwrap_or(q);
    segment_has_known_code_ext(last)
}

fn segment_has_known_code_ext(segment: &str) -> bool {
    let Some((_, ext)) = segment.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty()
        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        && astrolabe_core::index::KNOWN_CODE_EXTENSIONS
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e))
}

pub(crate) fn filesystem_path_as_query_error(query: &str) -> String {
    let shown = {
        let t = query.trim();
        const MAX: usize = 120;
        if t.chars().count() <= MAX {
            t.to_string()
        } else {
            format!("{}…", t.chars().take(MAX - 1).collect::<String>())
        }
    };
    format!(
        "find_symbol query looks like a file path (`{shown}`), not a symbol name.\n\
Put the symbol name in query and use path_filter to limit files/directories;\n\
for a file outline use query=\"*\" + path_filter=<path>."
    )
}

/// Heuristic: query looks like a regex but `regex` was left false.
/// `|` / `.*` / `\b` 是上一会话假阴性的主因；单独的 `.` 不警告（`foo.bar` 常为字面搜索）。
fn looks_like_regex_query(query: &str) -> bool {
    query.contains('|')
        || query.contains(".*")
        || query.contains(".+")
        || query.contains("\\b")
        || query.contains("\\s")
        || query.contains("\\d")
        || query.contains("\\w")
        || query.contains("(?")
        || (query.starts_with('^') && query.len() > 1)
        || (query.ends_with('$') && query.len() > 1 && !query.ends_with("\\$"))
}

/// Heuristic: query looks like a function signature / call / code snippet.
/// Models often pass these with `regex=true` (parens read as "exact"), which
/// either fails to compile (unclosed groups) or matches the wrong thing
/// (`foo(bar)` as a capture group ≠ the literal `foo(bar)`).
fn looks_like_code_fragment(query: &str) -> bool {
    // Leave intentional regex alone.
    if looks_like_regex_query(query) {
        return false;
    }
    // Escaped parens mean the caller wanted a literal `(` under regex mode.
    if query.contains(r"\(") || query.contains(r"\)") {
        return false;
    }
    let Some(paren) = query.find('(') else {
        return false;
    };
    // Leading `(…)` is usually a regex group; only treat as code when the
    // rest of the line is clearly a snippet (arrow / block).
    if paren == 0 {
        return query.contains("=>") || query.contains(") {") || query.contains("){");
    }
    let before = query[..paren].trim_end();
    if before.is_empty() {
        return false;
    }
    // Require an identifier-ish token immediately before `(` (`foo(`, `Type::new(`,
    // `obj.method(`, `fn process(`).
    let last = before.chars().last().unwrap_or('\0');
    last.is_alphanumeric() || last == '_'
}

/// When `regex=true` but the query looks like a code snippet, force literal.
/// Returns whether the flag was downgraded.
fn apply_code_fragment_literal(params: &mut SearchParams) -> bool {
    if params.regex && looks_like_code_fragment(&params.query) {
        params.regex = false;
        true
    } else {
        false
    }
}

fn quote_query_for_note(query: &str) -> String {
    const MAX: usize = 80;
    let count = query.chars().count();
    if count <= MAX {
        return query.to_string();
    }
    let clipped: String = query.chars().take(MAX).collect();
    format!("{clipped}…")
}

fn render_search_mode_header(regex: bool, query: &str, downgraded_from_regex: bool) -> String {
    let mode = if regex { "regex" } else { "literal" };
    let mut header = format!("mode: {mode}\n");
    if downgraded_from_regex {
        header.push_str(&format!(
            "Note: query looks like a function signature/code snippet; ignored regex=true and searched literally for `{}`. For a real regex, write an explicit pattern (e.g. a|b, foo\\(.*\\)); do not pass source snippets as regex.\n",
            quote_query_for_note(query)
        ));
    } else if !regex && looks_like_regex_query(query) {
        header.push_str(&format!(
            "Note: query contains regex metacharacters but regex=false; searched literally for `{}`. Pass regex=true for regex; split multiple symbols into separate literal searches.\n",
            quote_query_for_note(query)
        ));
    }
    header
}

fn invalid_regex_message(error: impl std::fmt::Display) -> String {
    format!(
        "Invalid regex: {error}. If you meant a literal search for this string, pass regex=false (literal is the default); do not treat function signatures or source snippets as regex."
    )
}

/// search_code 的分页状态行：保留 `shown= omitted= truncated=` 机器可读格式，
/// 另给出总数、本页范围和下一页的精确 offset。
/// One `search_code` match before rendering.
#[derive(Debug, Clone)]
struct SearchHit {
    path: String,
    /// 1-based.
    line: usize,
    /// Clipped matching line.
    text: String,
    text_only: bool,
}

/// Upper bound on `context_lines`.
const MAX_SEARCH_CONTEXT: usize = 10;
/// Columns kept per context line.
const CONTEXT_LINE_COLUMNS: usize = 160;

/// Context lines per match: the caller's value (capped), or adaptive by
/// match count. Few hits get enclosing bodies instead (see
/// [`smart_body_max_hits`]); these lines are the fallback for them.
fn search_context_lines(requested: Option<usize>, total: usize) -> usize {
    match requested {
        Some(n) => n.min(MAX_SEARCH_CONTEXT),
        // Ablation: ASTROLABE_SEARCH_ADAPTIVE=0 → bare lines by default.
        None if matches!(
            std::env::var("ASTROLABE_SEARCH_ADAPTIVE").as_deref(),
            Ok("0")
        ) =>
        {
            0
        }
        None if total <= smart_body_max_hits() => 3,
        None if total <= 20 => 1,
        None => 0,
    }
}

/// Hits at or below this count are shown inside their enclosing symbol body
/// (`ASTROLABE_SEARCH_BODY_HITS`, default 5; 0 disables).
fn smart_body_max_hits() -> usize {
    env_usize("ASTROLABE_SEARCH_BODY_HITS").unwrap_or(5)
}

/// Above this many hits an unpaged query gets a per-file summary
/// (`ASTROLABE_SEARCH_BROAD_HITS`, default 60).
fn broad_hits() -> usize {
    env_usize("ASTROLABE_SEARCH_BROAD_HITS").unwrap_or(60)
}

/// Enclosing bodies longer than this fall back to line context.
const SMART_BODY_MAX_LINES: u32 = 160;

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn session_dedup_enabled(param: Option<bool>) -> bool {
    param.unwrap_or(true)
        && !matches!(
            std::env::var("ASTROLABE_SESSION_DEDUP").as_deref(),
            Ok("0") | Ok("false")
        )
}

/// Data / fixture / test-vector file (json, yaml, toml …) by extension and
/// directory: a literal changed in code usually has copies here.
fn is_data_fixture(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let ext = p.rsplit_once('.').map_or("", |(_, e)| e);
    let data_ext = matches!(
        ext,
        "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "toml"
            | "csv"
            | "tsv"
            | "xml"
            | "txt"
            | "golden"
            | "snap"
            | "ini"
            | "proto"
            | "hex"
    );
    let dirs = [
        "test",
        "tests",
        "testdata",
        "fixtures",
        "fixture",
        "vectors",
        "golden",
        "snapshots",
        "__snapshots__",
        "spec",
        "data",
        "testvectors",
    ];
    data_ext && p.split('/').any(|seg| dirs.contains(&seg))
}

/// Compact list of literal hits in data/fixture files outside `filter`.
fn data_hits_outside(
    all_text_paths: &[&RelPath],
    root: &Path,
    filter: &str,
    query: &str,
    sensitive: bool,
    excluded: &dyn Fn(&str) -> bool,
) -> String {
    let rels: Vec<&str> = all_text_paths
        .iter()
        .map(|p| p.as_str())
        .filter(|p| !p.contains(filter) && is_data_fixture(p) && !excluded(p))
        .collect();
    if rels.is_empty() {
        return String::new();
    }
    let Ok(matcher) = astrolabe_core::text_search::build_matcher(query, false, sensitive) else {
        return String::new();
    };
    let found = astrolabe_core::text_search::search_files(
        root,
        &rels,
        &matcher,
        TEXT_SEARCH_MAX_FILE_BYTES,
    );
    let mut files: Vec<(String, Vec<u64>)> = found
        .into_iter()
        .filter(|(_, hits)| !hits.is_empty())
        .map(|(i, hits)| (rels[i].to_string(), hits.iter().map(|h| h.line).collect()))
        .collect();
    if files.is_empty() {
        return String::new();
    }
    files.sort();
    let total: usize = files.iter().map(|(_, l)| l.len()).sum();
    let mut out = format!(
        "Outside path_filter, data/fixture files have {total} more literal hits (usually keep in sync when changing the literal):\n"
    );
    for (path, lines) in files.iter().take(8) {
        let shown: Vec<String> = lines.iter().take(5).map(u64::to_string).collect();
        let more = if lines.len() > 5 {
            format!(" …{} hits total", lines.len())
        } else {
            String::new()
        };
        out.push_str(&format!("  {path}: lines {}{more}\n", shown.join(",")));
    }
    if files.len() > 8 {
        out.push_str(&format!("  …and {} more files\n", files.len() - 8));
    }
    out
}

/// Smallest symbol in `file` that encloses `line` with at least 3 lines.
fn enclosing_symbol(index: &RepoIndex, path: &str, line: usize) -> Option<CodeSymbol> {
    let file = index.file_id(path)?;
    let line = line as u32;
    index
        .file_symbols(file)
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .filter(|s| s.end_line >= s.start_line + 2)
        .min_by_key(|s| s.end_line - s.start_line)
        .cloned()
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    ["test", "spec", "mock", "fixture", "__snapshots__", "e2e"]
        .iter()
        .any(|m| lower.contains(m))
        || [
            "docs/",
            "doc/",
            "examples/",
            "example/",
            "scripts/",
            "probe",
            "vendor/",
            "third_party/",
        ]
        .iter()
        .any(|m| lower.starts_with(m) || lower.contains(&format!("/{m}")))
}

/// Order broad-query hits for the preview: definition lines first, other
/// non-test hits next, test/docs/example files last (stable within each class).
fn rank_broad_hits<'a>(index: &RepoIndex, hits: &'a [SearchHit]) -> Vec<&'a SearchHit> {
    // Definition start lines, built only for the files that have hits.
    let mut defs: std::collections::HashMap<&str, Option<std::collections::HashSet<u32>>> =
        std::collections::HashMap::new();
    let mut is_def = |path: &'a str, line: u32| {
        defs.entry(path)
            .or_insert_with(|| {
                index.file_id(path).map(|f| {
                    index
                        .file_symbols(f)
                        .filter(|s| {
                            !matches!(
                                s.kind,
                                astrolabe_core::SymbolKind::Variable
                                    | astrolabe_core::SymbolKind::Field
                                    | astrolabe_core::SymbolKind::Const
                            )
                        })
                        .map(|s| s.start_line)
                        .collect()
                })
            })
            .as_ref()
            .is_some_and(|set| set.contains(&line))
    };
    let mut ranked: Vec<(u8, usize, &SearchHit)> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let class = if is_test_path(&h.path) {
                2
            } else if !h.text_only && is_def(&h.path, h.line as u32) {
                0
            } else {
                1
            };
            (class, i, h)
        })
        .collect();
    ranked.sort_by_key(|(c, i, _)| (*c, *i));
    ranked.into_iter().map(|(_, _, h)| h).collect()
}

const BROAD_PREVIEW_HITS: usize = 12;
/// `find_symbol`: interfaces per page whose Go implementors are listed, and
/// implementors listed per interface.
const GO_IMPLEMENTOR_IFACES: usize = 3;
const GO_IMPLEMENTORS_LISTED: usize = 12;

fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Symbols whose names are close to a zero-hit literal query: substring /
/// prefix / shared word tokens / small edit distance. Best first, max `limit`.
fn closest_symbols(
    index: &RepoIndex,
    query: &str,
    path_filter: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let q = query.trim().to_lowercase();
    if q.len() < 3 || q.len() > 80 {
        return Vec::new();
    }
    let q_tokens = name_tokens(query);
    let q_chars = q.chars().count();
    let max_distance = (q.len() / 5).clamp(1, 2);
    // Word tokens that can count toward `shared` (len >= 3).
    let long_tokens: Vec<&String> = q_tokens.iter().filter(|t| t.len() >= 3).collect();
    let names = index.names();
    let lower_names = names.lower();
    // The path filter only depends on the file, so check it once per file
    // and before any per-name scoring.
    let mut file_ok: HashMap<FileId, bool> = HashMap::new();
    let mut scored: Vec<(u32, String)> = Vec::new();
    for (pos, sym) in index.graph().symbols.iter().enumerate() {
        if let Some(f) = path_filter {
            let ok = *file_ok.entry(sym.file).or_insert_with(|| {
                index
                    .file(sym.file)
                    .is_some_and(|file| file.path.as_str().contains(f))
            });
            if !ok {
                continue;
            }
        }
        let name = lower_names[pos].as_str();
        let score = if name == q {
            0
        } else if name.starts_with(&q) || q.starts_with(name) && name.len() >= 4 {
            1
        } else if name.contains(&q) || q.contains(name) && name.len() >= 4 {
            2
        } else if (if name.is_ascii() {
            name.len()
        } else {
            name.chars().count()
        })
        .abs_diff(q_chars)
            <= max_distance
            && edit_distance(name, &q) <= max_distance
        {
            3
        } else {
            // An ASCII name's tokens are substrings of its lowercase form,
            // so a name containing none of the query's long tokens cannot
            // share one; skip tokenising it.
            if sym.name.is_ascii() && !long_tokens.iter().any(|t| name.contains(t.as_str())) {
                continue;
            }
            let shared = name_tokens(&sym.name)
                .iter()
                .filter(|t| t.len() >= 3 && q_tokens.contains(t))
                .count();
            if shared >= 1 && shared >= q_tokens.len().min(2) {
                4
            } else {
                continue;
            }
        };
        let Some(file) = index.file(sym.file) else {
            continue;
        };
        if let Some(f) = path_filter {
            if !file.path.as_str().contains(f) {
                continue;
            }
        }
        let line = symbol_line(sym, &file.path);
        let line = match line.rsplit_once("  @") {
            Some((sig, loc)) => format!("{sig}  {loc}"),
            None => line,
        };
        scored.push((score, line));
    }
    scored.sort();
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(limit).map(|(_, l)| l).collect()
}

/// Role of a hit line. Tests: assertion vs plain reference; code: load/read,
/// definition, or other use. Heuristic, text-based.
fn hit_role(is_test: bool, text: &str) -> &'static str {
    hit_role_split(is_test, text, text)
}

/// Lexer-aware view of a hit line: `code` is the line without comment bytes
/// (string contents kept), `bare` also without string contents. `None` view
/// means no lexer for the file type.
struct RoleView {
    code: String,
    bare: String,
}

/// Role of hit lines using the file's `code_mask`: trailing comments no longer
/// leak words into the label, and lines inside multi-line / raw strings (Go
/// backticks, Python triple quotes) are tagged `字符串` instead of `定义`/`加载`.
fn role_views(path: &str, lines: &[String], wanted: &[usize]) -> Option<HashMap<usize, RoleView>> {
    let comments_only = crate::code_mask::code_mask(path, lines, true)?;
    let bare_mask = crate::code_mask::code_mask(path, lines, false)?;
    let pick = |line: &str, mask: &[bool]| -> String {
        let bytes: Vec<u8> = line
            .bytes()
            .zip(mask.iter())
            .filter(|(_, keep)| **keep)
            .map(|(b, _)| b)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut out = HashMap::new();
    for &i in wanted {
        let (Some(line), Some(c), Some(b)) = (lines.get(i), comments_only.get(i), bare_mask.get(i))
        else {
            continue;
        };
        out.insert(
            i,
            RoleView {
                code: pick(line, c),
                bare: pick(line, b),
            },
        );
    }
    Some(out)
}

fn hit_role_view(is_test: bool, raw: &str, view: &RoleView) -> &'static str {
    if view.code.trim().is_empty() {
        return if raw.trim().is_empty() {
            hit_role(is_test, raw)
        } else {
            "comment"
        };
    }
    if view.bare.trim().is_empty() {
        return "string";
    }
    hit_role_split(is_test, &view.bare, &view.code)
}

/// `words`: text used for word keys (strings blanked when known); `lit`: text
/// used for keys that need a string literal (`= "`).
fn hit_role_split(is_test: bool, words: &str, lit: &str) -> &'static str {
    let text = words;
    if is_comment_line(text) {
        return "comment";
    }
    let any = |keys: &[&str]| keys.iter().any(|k| contains_word_start(text, k));
    let any_lit = |keys: &[&str]| keys.iter().any(|k| contains_word_start(lit, k));
    if is_test {
        if any(&[
            "assert", "expect", "require.", "require(", "want", "equal", "eq!(", "should", "tobe",
            "toequal", "t.error", "t.fatal", "== ", "!= ", "verify(",
        ]) {
            "assert"
        } else {
            "ref"
        }
    } else if is_definition_line(text) {
        "def"
    } else if any(&[
        "load",
        "read",
        "open",
        "getenv",
        "environ",
        "env::var",
        "lookup",
        "parse",
        "config",
        "fetch",
        "get(",
        ".get ",
        "unmarshal",
        "decode",
        "from_str",
        "require(",
        "import ",
    ]) {
        "load"
    } else if any(&["const ", "static ", "#define", " := ", "let ", "var "])
        || any_lit(&["= \"", "=\"", ": \""])
    {
        "def"
    } else {
        "use"
    }
}

/// ASCII-case-insensitive `contains` where a key starting with a letter or
/// digit must begin at a word start (`read` matches `read_file` / `readFile` /
/// `.read(` but not `thread`, `spread`, `download`). Keys that start with
/// punctuation are plain substrings.
fn contains_word_start(text: &str, key: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let needs_boundary = key
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    let mut from = 0;
    while let Some(pos) = lower[from..].find(key) {
        let i = from + pos;
        if !needs_boundary || i == 0 {
            return true;
        }
        let prev = bytes[i - 1];
        let cur = bytes[i];
        if !prev.is_ascii_alphanumeric() || (prev.is_ascii_lowercase() && cur.is_ascii_uppercase())
        {
            return true;
        }
        from = i + key.len();
    }
    false
}

/// A line that is only a comment (`//`, `///`, `#`, `--`, `/* */`, `*`, `<!--`).
fn is_comment_line(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with("*/")
        || t.starts_with("<!--")
        || t.starts_with("--")
        || t.starts_with("\"\"\"")
        || t == "*"
        || t.starts_with("* ")
        || t == "#"
        || t.starts_with("# ")
        || t.starts_with("##")
        || t.starts_with("#!")
}

/// A line that declares something: `fn`/`func`/`def`/`class`/`struct`/… .
fn is_definition_line(text: &str) -> bool {
    const MODIFIERS: [&str; 12] = [
        "pub(crate) ",
        "pub ",
        "export ",
        "default ",
        "async ",
        "public ",
        "private ",
        "protected ",
        "static ",
        "final ",
        "abstract ",
        "unsafe ",
    ];
    let mut t = text.trim_start();
    loop {
        let before = t;
        for m in MODIFIERS {
            if let Some(rest) = t.strip_prefix(m) {
                t = rest.trim_start();
            }
        }
        if t == before {
            break;
        }
    }
    [
        "fn ",
        "func ",
        "def ",
        "function ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "interface ",
        "type ",
        "impl ",
        "const ",
        "let ",
        "var ",
        "mod ",
        "module ",
    ]
    .iter()
    .any(|k| t.starts_with(k))
}

/// Below this many hits `group=true` lists every row.
const GROUP_FULL_HITS: usize = 40;

/// Closing line of a hit listing: totals and whether the listing is complete.
/// A cut listing leads with a ready `path_filter` value for what is left.
fn closure_line(hits: &[SearchHit], shown: usize, offset: usize, broad: bool) -> String {
    let files = hits
        .iter()
        .map(|h| h.path.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let total = hits.len();
    let listed = offset + shown;
    if listed >= total {
        let fw = if files == 1 { "file" } else { "files" };
        format!("Summary: {total} lines / {files} {fw}, fully listed.\n")
    } else {
        let rest = if broad {
            hits
        } else {
            &hits[listed.min(total)..]
        };
        let narrow = search_shape::suggest_path_filter(rest.iter().map(|h| h.path.as_str()))
            .map(|p| format!("narrow with path_filter=\"{p}\", "))
            .unwrap_or_default();
        let cont = if broad {
            format!("or page=true then offset={listed} to page")
        } else {
            format!("or offset={listed} to continue")
        };
        let fw = if files == 1 { "file" } else { "files" };
        format!(
            "Summary: listed {listed}/{total} lines / {files} {fw}, incomplete: {narrow}{cont} (or use exclude/aggregate).\n"
        )
    }
}

/// Closing line of a `group=true` page. Grouped output is not a prefix of the
/// hit list, so a truncated page must not suggest `offset=` (which would not
/// continue it). `folded_tests`: test rows counted per file instead of listed.
fn grouped_closure_line(hits: &[SearchHit], shown: usize, folded_tests: usize) -> String {
    let total = hits.len();
    if shown >= total {
        return closure_line(hits, shown, 0, false);
    }
    let files = hits
        .iter()
        .map(|h| h.path.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    if shown + folded_tests >= total {
        let fw = if files == 1 { "file" } else { "files" };
        return format!(
            "Summary: {total} lines / {files} {fw}; {GROUP_TEST_ROWS} hits shown per test file; remaining {folded_tests} counted per file (path_filter=that file for full list).\n"
        );
    }
    let narrow = search_shape::suggest_path_filter(hits.iter().map(|h| h.path.as_str()))
        .map(|p| format!("narrow with path_filter=\"{p}\", "))
        .unwrap_or_default();
    let fw = if files == 1 { "file" } else { "files" };
    format!(
        "Summary: listed {shown}/{total} lines / {files} {fw}, incomplete: {narrow}or group=false then offset={shown} to page (grouped view cannot page via offset; or use exclude/aggregate).\n"
    )
}

/// Next-step line for results with many hits (> `GROUP_FULL_HITS`).
fn many_hits_hint(total: usize) -> String {
    if total > GROUP_FULL_HITS {
        "Next: to count hit lines per directory/file (per line; multiple hits on one line count as 1; for exact occurrence counts use rg -o) use aggregate=top|dir|file (optional skip_comments=true, exclude=[\"_test.go\",\"vendor\"]); for batch rename use find_references dry_run_rename.\n".to_string()
    } else {
        String::new()
    }
}

/// Documentation files: listed apart from production code and tests.
fn is_doc_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let ext = p.rsplit_once('.').map_or("", |(_, e)| e);
    let stem = p
        .rsplit('/')
        .next()
        .and_then(|n| n.split('.').next())
        .unwrap_or("");
    // `.txt` is prose only for README-like names; CMakeLists.txt,
    // requirements.txt etc. are build/config files.
    let readme_like = matches!(
        stem,
        "readme"
            | "notes"
            | "changelog"
            | "changes"
            | "license"
            | "authors"
            | "todo"
            | "contributing"
            | "history"
            | "news"
            | "install"
    );
    matches!(ext, "md" | "mdx" | "rst" | "adoc")
        || (ext == "txt" && readme_like)
        || p.starts_with("docs/")
        || p.starts_with("doc/")
        || p.contains("/docs/")
        || p.contains("/doc/")
}

/// `aggregate=top|dir|file` / `count_only`: counts and a total, no hit lines.
fn render_search_counts(hits: &[SearchHit], aggregate: Option<&str>, offset: usize) -> String {
    const MAX_ROWS: usize = 80;
    let total = hits.len();
    let mut all_files: BTreeSet<&str> = BTreeSet::new();
    let mut dirs: BTreeSet<&str> = BTreeSet::new();
    let mut tops: BTreeSet<&str> = BTreeSet::new();
    // key -> (hits, files)
    let mut rows: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
    for hit in hits {
        let path = hit.path.as_str();
        all_files.insert(path);
        let dir = path.rsplit_once('/').map_or(".", |(d, _)| d);
        dirs.insert(dir);
        let top = path.split_once('/').map_or(".", |(t, _)| t);
        tops.insert(top);
        let key = match aggregate {
            Some("top") => top,
            Some("dir") => dir,
            Some("file") => path,
            _ => continue,
        };
        let row = rows.entry(key).or_default();
        row.0 += 1;
        row.1.insert(path);
    }
    let mut out = format!(
        "Count: total {total} hit lines / {} files / {} dirs / {} top-level dirs (complete; per line — multiple hits on one line count as 1 line, not occurrences).\n",
        all_files.len(),
        dirs.len(),
        tops.len()
    );
    if aggregate.is_some() {
        let mut rows: Vec<(&str, usize, usize)> = rows
            .into_iter()
            .map(|(k, (n, f))| (k, n, f.len()))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let label = match aggregate {
            Some("top") => "per top-level dir",
            Some("dir") => "per directory",
            _ => "per file",
        };
        out.push_str(&format!("{label}: N lines / M files\n"));
        let suffix = if aggregate == Some("file") { "" } else { "/" };
        let start = offset.min(rows.len());
        let end = (start + MAX_ROWS).min(rows.len());
        if start > 0 {
            out.push_str(&format!(
                "  (from item {}, {} items total)\n",
                start + 1,
                rows.len()
            ));
        }
        for (key, n, f) in &rows[start..end] {
            out.push_str(&format!("  {n:>5} lines / {f:>4} files  {key}{suffix}\n"));
        }
        if end < rows.len() {
            let rest: usize = rows[end..].iter().map(|r| r.1).sum();
            out.push_str(&format!(
                "  …{} more items totaling {rest} lines not listed (already in the total); pass offset={end} for the rest, or narrow with path_filter.\n",
                rows.len() - end
            ));
        }
        out.push_str(&format!(
            "  {total:>5} lines / {:>4} files  total\n",
            all_files.len()
        ));
    }
    out
}

/// Glob list -> matcher. One rule for every consumer (search_code main path,
/// aggregate/count, text-only scan, `data_hits_outside`): a bare name
/// (`test`, `vendor`) matches a whole path segment, so a directory or a file
/// of exactly that name (and everything under such a directory); it never
/// matches part of a name (`test` skips neither `contest/` nor `test.rs`).
/// `*` crosses `/`.
fn build_exclude_set(globs: Option<&[String]>) -> Result<Option<globset::GlobSet>, String> {
    let Some(globs) = globs.filter(|g| !g.is_empty()) else {
        return Ok(None);
    };
    let mut builder = globset::GlobSetBuilder::new();
    for raw in globs {
        let pat = raw.trim().trim_start_matches("./");
        if pat.is_empty() {
            continue;
        }
        let base = pat.trim_end_matches('/');
        let mut variants = vec![base.to_string(), format!("{base}/**")];
        // `_test.go` / `.min.js`: a bare name tail matches by file-name suffix.
        if !base.contains(['/', '*', '?', '[']) && base.starts_with(['_', '.', '-']) {
            variants.push(format!("**/*{base}"));
        }
        if !base.starts_with("**/") {
            variants.push(format!("**/{base}"));
            variants.push(format!("**/{base}/**"));
        }
        for v in variants {
            let glob = globset::Glob::new(&v)
                .map_err(|e| format!("Invalid glob `{raw}` in exclude: {e}"))?;
            builder.add(glob);
        }
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("exclude failed to compile: {e}"))
}

/// Include filter for `search_code` (`glob`, e.g. `*.rs`, `*.{ts,tsx}`,
/// `crates/*/src/**`). A pattern without a `/` also matches at any depth, so
/// `*.rs` behaves like `**/*.rs` — the shape a model means by "only Rust files".
fn build_include_set(glob: Option<&str>) -> Result<Option<globset::GlobSet>, String> {
    let Some(raw) = glob.map(str::trim).filter(|g| !g.is_empty()) else {
        return Ok(None);
    };
    let base = raw.trim_start_matches("./").trim_end_matches('/');
    if base.is_empty() {
        return Ok(None);
    }
    let mut variants = vec![base.to_string()];
    if !base.starts_with("**/") && !base.starts_with('/') {
        variants.push(format!("**/{base}"));
    }
    if !base.contains(['/', '*', '?', '[']) {
        variants.push(format!("**/{base}/**"));
    }
    let mut builder = globset::GlobSetBuilder::new();
    for v in variants {
        let glob = globset::Glob::new(&v)
            .map_err(|e| format!("glob `{raw}` 无效：{e}；示例：*.rs、*.{{ts,tsx}}、src/**"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("glob 无法编译：{e}"))
}

/// Gist of a whole result kept for the repeat-query pointer: counts, busiest
/// files, and the first hit with its enclosing symbol signature.
fn search_digest(index: &RepoIndex, hits: &[SearchHit]) -> String {
    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    for h in hits {
        *files.entry(h.path.as_str()).or_default() += 1;
    }
    let n_files = files.len();
    let mut top: Vec<_> = files.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let top: Vec<String> = top
        .iter()
        .take(3)
        .map(|(f, n)| format!("{f}×{n}"))
        .collect();
    let mut out = format!(
        "{} hit lines / {n_files} files; top: {}",
        hits.len(),
        top.join(", ")
    );
    let samples: Vec<String> = hits
        .iter()
        .take(3)
        .map(|h| {
            let text: String = h.text.trim().chars().take(100).collect();
            format!("@{}:{} {text}", h.path, h.line)
        })
        .collect();
    if !samples.is_empty() {
        out.push_str(&format!("; first hits: {}", samples.join(" | ")));
    }
    if let Some(first) = hits.first() {
        out.push_str(&format!("; first {}:{}", first.path, first.line));
        if !first.text_only {
            if let Some(sym) = enclosing_symbol(index, &first.path, first.line) {
                out.push_str(&format!(
                    ", at {} (signature: {})",
                    sym.name,
                    sym.signature.lines().next().unwrap_or("").trim()
                ));
            }
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskKind {
    Rename,
    Refs,
    Count,
    Other,
}

fn ascii_words(t: &str) -> Vec<String> {
    t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// `phrase` (space-separated lowercase words) occurs in `words` as whole words.
fn has_phrase(words: &[String], phrase: &str) -> bool {
    let p: Vec<&str> = phrase.split(' ').collect();
    words
        .windows(p.len())
        .any(|w| w.iter().zip(&p).all(|(a, b)| a == b))
}

/// Classify the task wording. English keywords match whole words only
/// (`account` is not `count`); callers/references win over counting because
/// "how many callers of X" needs the call sites first.
fn classify_task(task: &str) -> TaskKind {
    let t = task.to_lowercase();
    let words = ascii_words(&t);
    let any_word = |keys: &[&str]| keys.iter().any(|k| words.iter().any(|w| w == k));
    let zh = |keys: &[&str]| keys.iter().any(|k| t.contains(k));

    let refs = any_word(&["caller", "callers", "usages", "usage", "references"])
        || has_phrase(&words, "who calls")
        || has_phrase(&words, "called by")
        || zh(&["谁调用", "调用方", "引用"]);
    // "count" is a counting task only in verb/noun-phrase use, not as a bare
    // trailing word (`account balance count`).
    let count_verb = words.iter().enumerate().any(|(i, w)| {
        if w != "count" && w != "counts" && w != "counting" {
            return false;
        }
        let prev = i.checked_sub(1).map(|j| words[j].as_str());
        let next = words.get(i + 1).map(String::as_str);
        let lead = [
            "to", "and", "then", "please", "just", "also", "can", "you", "i", "we", "need", "want",
            "should", "must",
        ];
        let tail = [
            "of",
            "the",
            "all",
            "how",
            "each",
            "every",
            "occurrences",
            "usages",
            "calls",
            "lines",
            "files",
            "matches",
            "per",
            "by",
            "in",
            "total",
        ];
        i == 0 || prev.is_some_and(|p| lead.contains(&p)) || next.is_some_and(|n| tail.contains(&n))
    });
    let count = count_verb
        || has_phrase(&words, "how many")
        || has_phrase(&words, "number of")
        || zh(&["多少", "统计", "每个目录", "分布", "调用次数"]);

    if refs {
        TaskKind::Refs
    } else if count {
        TaskKind::Count
    } else if any_word(&["rename", "renaming"]) || zh(&["重命名", "改名", "批量改", "替换"])
    {
        TaskKind::Rename
    } else {
        TaskKind::Other
    }
}

/// One-line pointer to the follow-up tool that fits the task wording.
fn next_step_hint(task: &str) -> &'static str {
    match classify_task(task) {
        TaskKind::Rename => "Next: find_references(symbol, dry_run_rename=new_name) for a per-file rename checklist; then run build/tests to verify (use get_diagnostics only if you cannot).\n",
        TaskKind::Count => "Next: search_code(query, aggregate=top|dir|file, skip_comments=true, exclude=[…]) for N lines / M files per dir/file (per line; multiple hits on one line count as 1); add count_only=true for totals only.\n",
        TaskKind::Refs => "Next: find_references(symbol) for precise call sites; use goto_definition for the definition.\n",
        TaskKind::Other => "Next: for a known name use find_symbol (include_body=true for source); for literals/copy use search_code; for callers use find_references.\n",
    }
}

/// Test-file rows listed per file in grouped mode when production hits
/// exist; the rest are counted with their test names.
const GROUP_TEST_ROWS: usize = 2;

/// `group=true`: hits grouped per file, production first then tests. Each
/// row names its enclosing symbol with its line range on first mention in a
/// file (`in Relay@89-437`). With production hits present, test files list
/// [`GROUP_TEST_ROWS`] rows and count the rest. `legend` adds the role-tag
/// legend (once per session). Returns the text, rows shown, and test rows
/// folded into counts.
fn render_grouped_hits(
    index: &RepoIndex,
    cache: &FileCache,
    hits: &[SearchHit],
    limit: usize,
    legend: bool,
) -> (String, usize, usize) {
    struct Row {
        line: usize,
        role: &'static str,
        sym: String,
        range: Option<(u32, u32)>,
        text: String,
    }
    let mut files: BTreeMap<&str, Vec<Row>> = BTreeMap::new();
    for hit in hits {
        let sym = if hit.text_only {
            None
        } else {
            enclosing_symbol(index, &hit.path, hit.line)
        };
        files.entry(hit.path.as_str()).or_default().push(Row {
            line: hit.line,
            role: "",
            sym: sym.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            range: sym.as_ref().map(|s| (s.start_line, s.end_line)),
            text: hit.text.clone(),
        });
    }
    let mut prod: Vec<(&str, Vec<Row>)> = Vec::new();
    let mut tests: Vec<(&str, Vec<Row>)> = Vec::new();
    let mut docs: Vec<(&str, Vec<Row>)> = Vec::new();
    for (path, mut rows) in files {
        if is_doc_path(path) {
            for r in &mut rows {
                r.role = "doc";
            }
            docs.push((path, rows));
            continue;
        }
        let file_is_test = crate::explain::is_test_caller(path, "");
        let wanted: Vec<usize> = rows.iter().map(|r| r.line.saturating_sub(1)).collect();
        let views = index
            .cached_lines(cache, path)
            .and_then(|(lines, _)| role_views(path, &lines, &wanted));
        for r in &mut rows {
            let test = file_is_test || crate::explain::is_test_caller(path, &r.sym);
            r.role = match views
                .as_ref()
                .and_then(|v| v.get(&r.line.saturating_sub(1)))
            {
                Some(view) => hit_role_view(test, &r.text, view),
                None => hit_role(test, &r.text),
            };
        }
        if file_is_test {
            tests.push((path, rows));
        } else {
            prod.push((path, rows));
        }
    }
    prod.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    tests.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    docs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let mut out = format!(
        "Grouped: {} hits, {} files (prod {}, test {}, doc {}).\n",
        hits.len(),
        prod.len() + tests.len() + docs.len(),
        prod.len(),
        tests.len(),
        docs.len()
    );
    if legend {
        out.push_str("Hit-line tags: tests [assert]/[ref], code [load]/[def]/[use], comment lines [comment], inside strings [string], docs [doc]; `in func@start-end` is the enclosing function and its line range.\n");
    }
    let mut shown = 0usize;
    let mut folded_tests = 0usize;
    let mut stopped = false;
    // Few hits: show every row of every file (nothing hidden to re-query).
    let row_cap = if hits.len() <= GROUP_FULL_HITS {
        usize::MAX
    } else {
        6
    };
    let cap_tests = !prod.is_empty();
    for (title, group) in [("prod", &prod), ("test", &tests), ("doc", &docs)] {
        if group.is_empty() {
            continue;
        }
        let n: usize = group.iter().map(|(_, r)| r.len()).sum();
        let hw = if n == 1 { "hit" } else { "hits" };
        let fw = if group.len() == 1 { "file" } else { "files" };
        out.push_str(&format!("## {title} ({n} {hw} / {} {fw})\n", group.len()));
        let is_tests = title == "test";
        let cap = if is_tests && cap_tests {
            row_cap.min(GROUP_TEST_ROWS)
        } else {
            row_cap
        };
        for (path, rows) in group {
            let mut block = format!("{path} ({})\n", rows.len());
            let mut named: Vec<&str> = Vec::new();
            for r in rows.iter().take(cap) {
                let sym = match r.range {
                    _ if r.sym.is_empty() => String::new(),
                    Some((a, b)) if !named.contains(&r.sym.as_str()) => {
                        format!(" in {}@{a}-{b}", r.sym)
                    }
                    _ => format!(" in {}", r.sym),
                };
                if !r.sym.is_empty() {
                    named.push(r.sym.as_str());
                }
                let text: String = r.text.trim().chars().take(140).collect();
                block.push_str(&format!("  L{} [{}]{sym}: {text}\n", r.line, r.role));
            }
            if rows.len() > cap {
                let rest = rows.len() - cap;
                if is_tests && cap_tests && cap == GROUP_TEST_ROWS {
                    let mut names: Vec<&str> = Vec::new();
                    for r in rows.iter().skip(cap) {
                        if !r.sym.is_empty() && !names.contains(&r.sym.as_str()) {
                            names.push(r.sym.as_str());
                        }
                    }
                    let more = if names.len() > 3 { " …" } else { "" };
                    let names: Vec<&str> = names.into_iter().take(3).collect();
                    let within = if names.is_empty() {
                        String::new()
                    } else {
                        format!(", in {}{more}", names.join(", "))
                    };
                    block.push_str(&format!(
                        "  (tests have {rest} more hits{within}; path_filter=\"{path}\" for the full list)\n"
                    ));
                } else {
                    block.push_str(&format!(
                        "  (this file has {rest} more hits not listed; path_filter=\"{path}\" for the full list)\n"
                    ));
                }
            }
            if estimate_tokens(&out) + estimate_tokens(&block) > limit {
                stopped = true;
                break;
            }
            let listed = rows.len().min(cap);
            shown += listed;
            if is_tests && cap_tests && row_cap == usize::MAX {
                folded_tests += rows.len() - listed;
            }
            out.push_str(&block);
        }
        if stopped {
            break;
        }
    }
    if stopped {
        out.push_str(
            "(incomplete) remaining files omitted due to budget_tokens; narrow with path_filter/exclude or raise budget_tokens.\n",
        );
    }
    (out, shown, folded_tests)
}

/// Per-file and per-directory hit counts for a broad query.
fn render_search_summary(hits: &[SearchHit], path_filter: Option<&str>) -> String {
    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    for hit in hits {
        *files.entry(hit.path.as_str()).or_default() += 1;
        let dir: Vec<&str> = hit.path.split('/').collect();
        let key = if dir.len() > 2 {
            dir[..2].join("/")
        } else if dir.len() == 2 {
            dir[0].to_string()
        } else {
            ".".into()
        };
        *dirs.entry(key).or_default() += 1;
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out = format!(
        "Wide query: {} hits across {} files. Only the most relevant hits are listed below, not the full set; narrow with path_filter (or a more specific literal / find_symbol); pass page=true for a full page (then offset to page).\n",
        hits.len(),
        files.len()
    );
    out.push_str("by directory:\n");
    for (dir, n) in dirs.iter().take(10) {
        out.push_str(&format!("  {n:>5}  {dir}/\n"));
    }
    out.push_str("By file (top 15):\n");
    for (file, n) in files.iter().take(15) {
        out.push_str(&format!("  {n:>5}  {file}\n"));
    }
    if files.len() > 15 {
        out.push_str(&format!("  …{} more files\n", files.len() - 15));
    }
    if let Some((dir, _)) = dirs.first() {
        // `.` (repo root) matches every path: suggest the top file instead.
        let suggest = if dir == "."
            || path_filter.is_some_and(|f| dir.starts_with(f) && dir.len() <= f.len())
        {
            files
                .first()
                .map(|(f, _)| f.to_string())
                .unwrap_or_default()
        } else {
            dir.clone()
        };
        out.push_str(&format!("Suggestion: path_filter=\"{suggest}\"\n"));
    }
    out
}

/// Render few hits inside their enclosing symbol bodies. Hits in the same
/// symbol share one body; a body sent earlier this session (`seen` returns
/// the earlier call ordinal) is referenced instead of resent. Hits without a
/// usable symbol get `fallback` lines of context. `None` when it does not fit
/// in `limit` tokens.
fn render_symbol_hits(
    hits: &[SearchHit],
    limit: usize,
    fallback: usize,
    enclosing: &dyn Fn(&SearchHit) -> Option<CodeSymbol>,
    file_lines: &mut dyn FnMut(&SearchHit) -> Option<Arc<Vec<String>>>,
    seen: &mut dyn FnMut(&str, u32, u32) -> Option<u64>,
) -> Option<(String, bool)> {
    let mut out = String::new();
    let mut used_symbol = false;
    let found: Vec<Option<CodeSymbol>> = hits
        .iter()
        .map(|h| if h.text_only { None } else { enclosing(h) })
        .collect();
    let usable = |k: usize| {
        found[k]
            .as_ref()
            .filter(|s| s.end_line - s.start_line < SMART_BODY_MAX_LINES)
    };
    let mut i = 0;
    while i < hits.len() {
        let hit = &hits[i];
        let Some(symbol) = usable(i).cloned() else {
            // A run of hits without a printable body renders in one pass, so
            // overlapping windows merge (each line once, `…` between gaps).
            let mut j = i + 1;
            while j < hits.len() && usable(j).is_none() {
                j += 1;
            }
            // Bodies too long to print: still say where they start and end,
            // so nobody pages backwards to find the function header.
            let mut long: Vec<String> = Vec::new();
            for k in i..j {
                if let Some(s) = &found[k] {
                    let label = format!(
                        "{} {} · {}:{}-{}",
                        format!("{:?}", s.kind).to_lowercase(),
                        s.name,
                        hits[k].path,
                        s.start_line,
                        s.end_line
                    );
                    if !long.contains(&label) {
                        long.push(label);
                    }
                }
            }
            if !long.is_empty() {
                out.push_str(&format!(
                    "(enclosing function/type too long; showing only a few lines around the hit: {})\n",
                    long.join("; ")
                ));
            }
            let (text, _) = render_search_hits(&hits[i..j], fallback, usize::MAX, file_lines);
            out.push_str(&text);
            i = j;
            continue;
        };
        let (start, end) = (symbol.start_line as usize, symbol.end_line as usize);
        let mut j = i + 1;
        while j < hits.len()
            && hits[j].path == hit.path
            && hits[j].line >= start
            && hits[j].line <= end
        {
            j += 1;
        }
        let group = &hits[i..j];
        used_symbol = true;
        let kind = format!("{:?}", symbol.kind).to_lowercase();
        if let Some(call) = seen(&hit.path, symbol.start_line, symbol.end_line) {
            for h in group {
                out.push_str(&format!("@{}:{} {}\n", h.path, h.line, h.text));
            }
            out.push_str(&format!(
                "  (enclosing {kind} {} {}:{start}-{end} was already returned on tool call #{call} this session; file unchanged, not resent; pass dedup=false if needed)\n  signature: {}\n",
                symbol.name,
                hit.path,
                symbol.signature.lines().next().unwrap_or("").trim()
            ));
        } else if let Some(lines) = file_lines(hit) {
            out.push_str(&format!(
                "── {kind} {} · {}:{start}-{end} ──\n",
                symbol.name, hit.path
            ));
            for n in start..=end.min(lines.len()) {
                if let Some(h) = group.iter().find(|h| h.line == n) {
                    out.push_str(&format!("@{}:{} {}\n", h.path, h.line, h.text));
                } else {
                    render_context_line(&mut out, n, &lines[n - 1]);
                }
            }
        } else {
            for h in group {
                out.push_str(&format!("@{}:{} {}\n", h.path, h.line, h.text));
            }
        }
        i = j;
    }
    (estimate_tokens(&out) <= limit).then_some((out, used_symbol))
}

fn render_context_line(out: &mut String, line_no: usize, text: &str) {
    let text = text.trim_end();
    let clipped: String = text.chars().take(CONTEXT_LINE_COLUMNS).collect();
    let ellipsis = if clipped.len() < text.len() {
        "…"
    } else {
        ""
    };
    out.push_str(&format!("  {line_no}| {clipped}{ellipsis}\n"));
}

/// Render hits in order within `limit` tokens. Each hit keeps its
/// `@path:line text` anchor line; with `context > 0` the surrounding lines
/// follow as `  N| source`, merged per file so overlapping windows print each
/// line once (a later match inside an earlier window is still printed as its
/// own anchor line). Returns the text and how many hits it covers.
fn render_search_hits(
    hits: &[SearchHit],
    context: usize,
    limit: usize,
    file_lines: &mut dyn FnMut(&SearchHit) -> Option<Arc<Vec<String>>>,
) -> (String, usize) {
    let mut budget = TokenBudget::new(limit);
    let mut out = String::new();
    let mut current: Option<(String, Option<Arc<Vec<String>>>)> = None;
    // Last line (1-based) already printed for the current file.
    let mut printed = 0usize;
    for (i, hit) in hits.iter().enumerate() {
        let anchor = if hit.text_only {
            format!("@{}:{} [text-only] {}\n", hit.path, hit.line, hit.text)
        } else {
            format!("@{}:{} {}\n", hit.path, hit.line, hit.text)
        };
        let same_file = current.as_ref().is_some_and(|(p, _)| *p == hit.path);
        let lines = if context == 0 {
            None
        } else if same_file {
            current.as_ref().and_then(|(_, l)| l.clone())
        } else {
            file_lines(hit)
        };
        let mut chunk = String::new();
        let mut last = if same_file { printed } else { 0 };
        if let Some(lines) = &lines {
            let start = hit.line.saturating_sub(context).max(1).max(last + 1);
            if same_file && start > last + 1 && last > 0 {
                chunk.push_str("  …\n");
            }
            for n in start..hit.line {
                if let Some(text) = lines.get(n - 1) {
                    render_context_line(&mut chunk, n, text);
                }
            }
            chunk.push_str(&anchor);
            // Stop before the next match in this file: it prints its own anchor.
            let next_same = hits
                .get(i + 1)
                .filter(|n| n.path == hit.path)
                .map(|n| n.line);
            let mut end = (hit.line + context).min(lines.len());
            if let Some(next) = next_same {
                end = end.min(next.saturating_sub(1));
            }
            for n in hit.line + 1..=end {
                render_context_line(&mut chunk, n, &lines[n - 1]);
            }
            last = end.max(hit.line);
        } else {
            chunk.push_str(&anchor);
            last = hit.line;
        }
        if !budget.try_add(&chunk) {
            // A window too big for what is left: fall back to the bare line
            // so the page is not cut short by context alone.
            if context > 0 && budget.try_add(&anchor) {
                out.push_str(&anchor);
                current = Some((hit.path.clone(), lines));
                printed = hit.line;
                continue;
            }
            return (out, i);
        }
        out.push_str(&chunk);
        current = Some((hit.path.clone(), lines));
        printed = last;
    }
    (out, hits.len())
}

fn render_search_page_status(
    total: usize,
    offset: usize,
    shown: usize,
    omitted: usize,
    budget_tokens: usize,
    requested_offset: usize,
) -> String {
    render_search_page_status_narrow(
        total,
        offset,
        shown,
        omitted,
        budget_tokens,
        requested_offset,
        None,
    )
}

/// [`render_search_page_status`] whose "not complete" hint leads with a
/// ready `path_filter` value for the unshown rest when one is given.
fn render_search_page_status_narrow(
    total: usize,
    offset: usize,
    shown: usize,
    omitted: usize,
    budget_tokens: usize,
    requested_offset: usize,
    narrow: Option<&str>,
) -> String {
    let truncated = omitted > 0;
    let mut status = format!(
        "shown={shown} omitted={omitted} truncated={truncated} total={total} offset={offset}\n"
    );
    if requested_offset > total {
        status.push_str(&format!(
            "offset={requested_offset} beyond total {total}; this page is empty.\n"
        ));
    }
    if shown > 0 && (offset > 0 || truncated) {
        status.push_str(&format!(
            "This page is items {}–{} of {total}.\n",
            offset + 1,
            offset + shown
        ));
    }
    if truncated {
        let next = offset + shown;
        if shown == 0 {
            status.push_str(&format!(
                "Not the full set: of the remaining {omitted}, the first result alone exceeds budget_tokens={budget_tokens} and was omitted. Raise budget_tokens or tighten path_filter and retry.\n"
            ));
        } else {
            let lead = narrow
                .map(|p| format!("Narrow: path_filter=\"{p}\"; or "))
                .unwrap_or_default();
            status.push_str(&format!(
                "Not the full set: {omitted} more not shown. {lead}reuse the same params with offset={next} for the next page (next_offset={next}).\n"
            ));
        }
    }
    status
}

/// Parse `ASTROLABE_CACHE_MB` (whole megabytes). Invalid values fall back to
/// the core default so a typo cannot silently disable the ceiling.
fn parse_cache_budget_mb(raw: Option<&str>) -> u64 {
    match raw {
        None => DEFAULT_MEMORY_BUDGET_BYTES,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(mb) => mb.saturating_mul(BYTES_PER_MB),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_CACHE_MB; using default"
                );
                DEFAULT_MEMORY_BUDGET_BYTES
            }
        },
    }
}

fn cache_budget_from_env() -> u64 {
    parse_cache_budget_mb(std::env::var("ASTROLABE_CACHE_MB").ok().as_deref())
}

/// Parse a `usize` env var; invalid values warn and fall back to `default`
/// so a typo cannot silently disable the knob.
fn parse_env_usize(raw: Option<&str>, var: &str, default: usize) -> usize {
    match raw {
        None => default,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(value = %raw, var = var, "invalid env var; using default");
                default
            }
        },
    }
}

/// `ASTROLABE_RESOURCE_THRESHOLD`：资源卸载阈值基线（默认 4000）。
fn resource_threshold_base_from_env() -> usize {
    parse_env_usize(
        std::env::var("ASTROLABE_RESOURCE_THRESHOLD")
            .ok()
            .as_deref(),
        "ASTROLABE_RESOURCE_THRESHOLD",
        DEFAULT_RESOURCE_THRESHOLD_TOKENS,
    )
}

/// `ASTROLABE_MAX_BUDGET_TOKENS`：budget_tokens 硬上限（默认 32000）。
fn max_budget_tokens_from_env() -> usize {
    parse_env_usize(
        std::env::var("ASTROLABE_MAX_BUDGET_TOKENS").ok().as_deref(),
        "ASTROLABE_MAX_BUDGET_TOKENS",
        DEFAULT_MAX_BUDGET_TOKENS,
    )
}

/// 纯函数核心（max 显式传入），便于测试在并行环境下不读 env。
fn clamp_budget_within(raw: usize, max: usize) -> usize {
    raw.clamp(1, max)
}

/// 将 budget 限制在 `[1, max_budget_tokens_from_env()]`。
fn clamp_budget(raw: usize) -> usize {
    clamp_budget_within(raw, max_budget_tokens_from_env())
}

/// `ASTROLABE_STRUCTURED` overrides the context default when set to a
/// recognised true/false token. Unset or unrecognised → context value
/// (`null`/auto resolves to off).
fn structured_output_override() -> Option<bool> {
    match std::env::var("ASTROLABE_STRUCTURED")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("1" | "true" | "TRUE" | "yes" | "YES") => Some(true),
        Some("0" | "false" | "FALSE" | "no" | "NO") => Some(false),
        _ => None,
    }
}

fn structured_output_for(context: &ClientContext) -> bool {
    structured_output_override().unwrap_or_else(|| context.structured_or_auto_off())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct BudgetOnly {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    /// Host language servers from the Grok tool layer (`lsp_fallback=grok`).
    /// Native-only: never appears in the MCP schema or client payload.
    #[serde(skip)]
    #[schemars(skip)]
    pub host_lsp: Option<Vec<HostLspServer>>,
}

/// One host language-server registration from `~/.grok/lsp.json` (or plugins),
/// with whether a live client is currently running. Used only on the native
/// path so `get_languages` can report the host pool instead of Astrolabe's
/// PATH probe when `lsp_fallback = grok`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostLspServer {
    /// Config key in `lsp.json` (e.g. `rust-analyzer`).
    pub name: String,
    /// Command that starts (or is running) this server.
    pub command: String,
    /// Language ids from the extension map values (e.g. `rust`, `typescript`).
    pub languages: Vec<String>,
    /// `true` when the host manager holds a live client for this server.
    pub running: bool,
}

impl Default for BudgetOnly {
    fn default() -> Self {
        Self {
            root: None,
            index_root: None,
            budget_tokens: default_budget(),
            host_lsp: None,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ResolveContextParams {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[schemars(description = "Task.")]
    pub task_description: String,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(description = "false: resend a repeated call.")]
    pub dedup: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Next-page offset.")]
    pub offset: usize,
    #[serde(default)]
    #[schemars(description = "Repo-relative path substring.")]
    pub path_filter: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryParams {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[schemars(
        description = "Symbol name, substring, or 'Class/method' / Type.method. Not a file path — use path_filter (outline: query='*' + path_filter)."
    )]
    pub query: String,
    /// 对展示列表前 5 个命中符号附带源码体（include_body，对齐 Serena）。
    /// 默认 true：减少拿到 path:line 后整文件 Read。
    #[serde(default = "default_include_body")]
    #[schemars(description = "Return full bodies (default true; set false for anchors only).")]
    pub include_body: bool,
    /// 子串开关。名称查询：省略=子串匹配（精确名优先排序），false=只要精确名，
    /// true=子串；路径查询：最后段子串匹配（省略/false=精确）。
    #[serde(default)]
    #[schemars(
        description = "Match mode: omitted=ranked prefix/substring, false=exact, true=substring."
    )]
    pub substring: Option<bool>,
    /// kind 过滤（Function/Method/Class/Struct/Trait/...），名称与路径查询均生效。
    #[serde(default)]
    #[schemars(
        description = "Filter by SymbolKind (function, method, class, interface, struct, etc.)."
    )]
    pub kind: Option<String>,
    /// 仓库相对路径子串过滤（与 search_code 相同语义）。
    #[serde(default)]
    #[schemars(description = "Repo-relative path substring (not a substitute for query).")]
    pub path_filter: Option<String>,
    /// 展开成员的层数（0=不展开）；`query="*"` + path_filter 时输出文件大纲。
    #[serde(default)]
    #[schemars(
        description = "Member levels to show (0 = none). query='*' + path_filter: file outline."
    )]
    pub depth: u8,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    /// 0 起始的翻页偏移（与 search_code 一致）。
    #[serde(default)]
    #[schemars(description = "0-based match offset for pagination (see next_offset in result).")]
    pub offset: usize,
}

/// Valid `kind` filter names for path queries (case-insensitive).
const VALID_KIND_NAMES: &str =
    "function, method, class, interface, struct, enum, trait, type, const, module, field, variable";

/// SymbolKind 名字（schema 字符串）→ 枚举。未知名字返回 Err（列出有效值）。
fn kind_from_name(name: &str) -> Result<astrolabe_core::SymbolKind, String> {
    use astrolabe_core::SymbolKind as K;
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "function" => K::Function,
        "method" => K::Method,
        "class" => K::Class,
        "interface" => K::Interface,
        "struct" => K::Struct,
        "enum" => K::Enum,
        "trait" => K::Trait,
        "type" => K::Type,
        "const" => K::Const,
        "module" => K::Module,
        "field" => K::Field,
        "variable" => K::Variable,
        other => {
            return Err(format!(
                "Unknown kind `{other}`; valid values: {VALID_KIND_NAMES}"
            ));
        }
    })
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SearchParams {
    #[serde(default)]
    #[schemars(description = "Sub-repo name or abs path.")]
    pub root: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Absolute or cwd-relative directory to index for this call; omit for the startup root."
    )]
    pub index_root: Option<String>,
    #[schemars(description = "Search string (literal; regex when regex=true).")]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "Default false. Set true to treat query as a regular expression.")]
    pub regex: bool,
    #[serde(default)]
    #[schemars(description = "Repo-relative path substring (case-sensitive, not glob).")]
    pub path_filter: Option<String>,
    #[serde(default)]
    #[schemars(description = "Override case (default: sensitive iff query has uppercase).")]
    pub case_sensitive: Option<bool>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Token budget.")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(description = "0-based match offset for pagination (see next_offset in result).")]
    pub offset: usize,
    #[serde(default)]
    #[schemars(
        description = "Surrounding source lines (0-10, omitted: adaptive 0-3 based on matches)."
    )]
    pub context_lines: Option<usize>,
    #[serde(default)]
    #[schemars(description = "Force match list on broad queries instead of summary counts.")]
    pub page: bool,
    #[serde(default)]
    #[schemars(
        description = "Group hits per file with test/assertion tags (default: auto for wide hits)."
    )]
    pub group: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Default true: reference repeated symbol bodies by location.")]
    pub dedup: Option<bool>,
    #[serde(default)]
    #[schemars(description = "top|dir|file: hit/file counts per dir or file + total.")]
    pub aggregate: Option<String>,
    #[serde(default)]
    #[schemars(description = "Skip paths; bare name=segment: [\"vendor\",\"_test.go\"]")]
    pub exclude: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(skip)]
    pub glob: Option<String>,
    #[serde(default)]
    #[schemars(description = "Only totals (hits, files).")]
    pub count_only: bool,
    #[serde(default)]
    #[schemars(description = "Ignore hits in comments and string literals.")]
    pub skip_comments: bool,
}

/// Max offloaded results kept for `resources/read`.
const RESOURCE_MAX_ENTRIES: usize = 64;
/// Max total bytes of offloaded results kept for `resources/read`.
const RESOURCE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// LRU store for offloaded tool results. Bounded by entry count and bytes;
/// the URI carries a per-process nonce so a counter restarted by a new server
/// process never reuses an old client-cached URI.
#[derive(Debug)]
pub(crate) struct ResourceStore {
    map: HashMap<String, String>,
    /// Least recently used first.
    order: std::collections::VecDeque<String>,
    bytes: usize,
    pub(crate) nonce: String,
    max_entries: usize,
    max_bytes: usize,
}

impl ResourceStore {
    fn new() -> Self {
        Self::with_limits(RESOURCE_MAX_ENTRIES, RESOURCE_MAX_BYTES)
    }

    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let mixed = (nanos as u64) ^ (u64::from(std::process::id()) << 32);
        Self {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
            nonce: format!("{:08x}", (mixed ^ (mixed >> 32)) as u32),
            max_entries,
            max_bytes,
        }
    }

    fn insert(&mut self, uri: String, body: String) {
        if let Some(old) = self.map.remove(&uri) {
            self.bytes = self.bytes.saturating_sub(old.len());
            self.order.retain(|u| u != &uri);
        }
        self.bytes += body.len();
        self.map.insert(uri.clone(), body);
        self.order.push_back(uri);
        while self.order.len() > 1
            && (self.order.len() > self.max_entries || self.bytes > self.max_bytes)
        {
            if let Some(victim) = self.order.pop_front() {
                if let Some(old) = self.map.remove(&victim) {
                    self.bytes = self.bytes.saturating_sub(old.len());
                }
            }
        }
    }

    /// Fetch and mark as most recently used.
    fn get(&mut self, uri: &str) -> Option<String> {
        let text = self.map.get(uri)?.clone();
        self.order.retain(|u| u != uri);
        self.order.push_back(uri.to_string());
        Some(text)
    }

    #[cfg(test)]
    pub(crate) fn values(&self) -> impl Iterator<Item = &String> {
        self.map.values()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

#[derive(Debug, Clone)]
pub struct AstrolabeServer {
    /// Parent (multi) or the single index root. Memory tools use this;
    /// graph tools use the routed session.
    root: PathBuf,
    primary: Arc<RootSession>,
    dispatch: Option<Arc<DispatchState>>,
    /// Lazily created when a single-repo server first receives `index_root`
    /// pointing outside the startup root. Multi-project reuses `dispatch`.
    dynamic_roots: Arc<Mutex<Option<Arc<DispatchState>>>>,
    ticker_stop: Arc<AtomicBool>,
    resources: Arc<Mutex<ResourceStore>>,
    next_resource: Arc<AtomicU64>,
    cache_budget_bytes: u64,
    context: ClientContext,
    structured_output: bool,
    loop_guard: Arc<Mutex<LoopGuard<CallToolResult>>>,
    tool_router: ToolRouter<Self>,
    /// Code ranges (symbol bodies) already returned in this MCP session,
    /// keyed by root+path+range, with the index generation they were read at
    /// and the ordinal of the call that sent them.
    sent_ranges: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    /// One-line digest of each whole `search_code` result, keyed like the
    /// query entries of `sent_ranges`, so a repeat query gets the gist back.
    search_digests: Arc<Mutex<HashMap<String, String>>>,
    /// Index generation each `find_symbol` query (offset and budget aside) was
    /// last served at, so a later page can say the index changed in between.
    symbol_page_generations: Arc<Mutex<HashMap<String, u64>>>,
    /// Same for `resolve_context` pages.
    context_page_generations: Arc<Mutex<HashMap<String, u64>>>,
    call_seq: Arc<AtomicU64>,
    /// Fixed legends and footers already shown this MCP session (each is
    /// printed once, like the syntax-error footnote).
    shown_once: Arc<Mutex<std::collections::HashSet<&'static str>>>,
    /// Shared index across sessions (`shared_index`): `Some` on the stdio
    /// server when sharing is on; `None` in leader-side session views.
    shared: Option<Arc<crate::shared_index::Shared>>,
    /// Per-session coverage-note memory for leader-side session views
    /// (the root session's own slot otherwise).
    coverage_note_override: Option<Arc<Mutex<Option<String>>>>,
    /// Last unambiguous symbol → path[:line] from find_symbol / goto_definition
    /// in this MCP process, so find_references can inherit a missing path.
    recent_symbols: Arc<crate::recent_symbol::RecentSymbols>,
}

impl AstrolabeServer {
    pub fn new(root: PathBuf) -> Self {
        Self::with_context(root, ClientContext::default_builtin())
    }

    /// Native facade: [`Self::new`] with `structuredContent` always on, since
    /// an in-process host reads pagination and counters from it rather than
    /// parsing the rendered text.
    #[cfg(feature = "native")]
    pub fn new_native(root: PathBuf) -> Self {
        Self::new_native_configured(root, true)
    }

    /// Native facade with the watcher policy the host wants. `watch = false`
    /// keeps the query-time freshness barrier but stops OS event watching, which
    /// is what short-lived hosts and tests need.
    #[cfg(feature = "native")]
    pub fn new_native_configured(root: PathBuf, watch: bool) -> Self {
        Self::new_native_with_lsp(root, watch, true)
    }

    /// Native facade with an explicit language-server policy. `lsp = false`
    /// keeps the precise tools index-only: when the index cannot decide they
    /// return `confidence: unknown` instead of starting Astrolabe's own
    /// server, so the host can answer from the LSP pool it already runs.
    #[cfg(feature = "native")]
    pub fn new_native_with_lsp(root: PathBuf, watch: bool, lsp: bool) -> Self {
        Self::new_native_with_options(root, watch, lsp, true)
    }

    /// Native facade with explicit watcher, language-server and persistence
    /// policy. `persist = false` never takes the indexer lock and never
    /// creates `<root>/.astrolabe/`: the index lives in memory only.
    #[cfg(feature = "native")]
    pub fn new_native_with_options(root: PathBuf, watch: bool, lsp: bool, persist: bool) -> Self {
        Self::new_native_full(root, watch, lsp, persist, true, Vec::new())
    }

    /// [`Self::new_native_with_options`] plus the `.gitignore` policy and the
    /// host's deny/exclude globs. `respect_gitignore = false` indexes and
    /// searches ignored files, so the tool sees the same set a host `grep`
    /// configured to ignore nothing does. `extra_excludes` are applied as
    /// scan/watcher/incremental excludes: matching files are never parsed
    /// into the index (they stay invisible to search and symbols), which is
    /// the build-time half of the host's `DenyReadGlobs` policy — queries
    /// filter a second time on the way out.
    #[cfg(feature = "native")]
    pub fn new_native_full(
        root: PathBuf,
        watch: bool,
        lsp: bool,
        persist: bool,
        respect_gitignore: bool,
        extra_excludes: Vec<String>,
    ) -> Self {
        let mut knobs = McpKnobs::from_env();
        if !watch {
            knobs.watch_mode = None;
        }
        knobs.lsp_enabled = lsp;
        knobs.persist_enabled = persist && knobs.persist_enabled;
        knobs.respect_gitignore = respect_gitignore;
        knobs.extra_excludes = extra_excludes;
        let mut server = Self::assemble(
            root,
            cache_budget_from_env(),
            ClientContext::default_builtin(),
            knobs,
            None,
        );
        server.structured_output = true;
        server
    }

    pub fn with_context(root: PathBuf, context: ClientContext) -> Self {
        Self::new_with_cache_budget_and_context(root, cache_budget_from_env(), context)
    }

    #[allow(dead_code)]
    pub(crate) fn new_with_cache_budget(root: PathBuf, budget_bytes: u64) -> Self {
        Self::new_with_cache_budget_and_context(
            root,
            budget_bytes,
            ClientContext::default_builtin(),
        )
    }

    pub(crate) fn new_with_cache_budget_and_context(
        root: PathBuf,
        budget_bytes: u64,
        context: ClientContext,
    ) -> Self {
        Self::assemble(root, budget_bytes, context, McpKnobs::from_env(), None)
    }

    /// Multi-project dispatch mode: do not index the parent; children activate
    /// on the first routed tool call.
    pub fn with_multi_project(
        root: PathBuf,
        children: Vec<PathBuf>,
        context: ClientContext,
    ) -> Self {
        let budget = cache_budget_from_env();
        let knobs = McpKnobs::from_env();
        let children: Vec<PathBuf> = children
            .into_iter()
            .map(|c| std::fs::canonicalize(&c).unwrap_or(c))
            .collect();
        tracing::info!(
            root = %root.display(),
            children = children.len(),
            "starting in multi-project dispatch mode"
        );
        for child in &children {
            tracing::info!(child = %child.display(), "multi-project child");
        }
        let dispatch = DispatchState::new(children, knobs.clone(), budget);
        Self::assemble(root, budget, context, knobs, Some(dispatch))
    }

    #[cfg(test)]
    pub(crate) fn with_multi_project_knobs(
        root: PathBuf,
        children: Vec<PathBuf>,
        knobs: McpKnobs,
        budget_bytes: u64,
    ) -> Self {
        let children: Vec<PathBuf> = children
            .into_iter()
            .map(|c| std::fs::canonicalize(&c).unwrap_or(c))
            .collect();
        let dispatch = DispatchState::new(children, knobs.clone(), budget_bytes);
        Self::assemble(
            root,
            budget_bytes,
            ClientContext::default_builtin(),
            knobs,
            Some(dispatch),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_knobs(root: PathBuf, knobs: McpKnobs, budget_bytes: u64) -> Self {
        Self::assemble(
            root,
            budget_bytes,
            ClientContext::default_builtin(),
            knobs,
            None,
        )
    }

    fn assemble(
        root: PathBuf,
        budget_bytes: u64,
        context: ClientContext,
        knobs: McpKnobs,
        dispatch: Option<Arc<DispatchState>>,
    ) -> Self {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let structured_output = structured_output_for(&context);
        tracing::info!(
            root = %root.display(),
            context = %context.name,
            structured_output,
            cache_budget_bytes = budget_bytes,
            "configured bounded file cache"
        );
        let loop_guard_config = knobs.loop_guard;
        let primary = RootSession::new(root.clone(), knobs, budget_bytes);
        Self {
            root: primary.root.clone(),
            cache_budget_bytes: budget_bytes,
            primary,
            dispatch,
            dynamic_roots: Arc::new(Mutex::new(None)),
            ticker_stop: Arc::new(AtomicBool::new(false)),
            resources: Arc::new(Mutex::new(ResourceStore::new())),
            next_resource: Arc::new(AtomicU64::new(1)),
            context,
            structured_output,
            loop_guard: Arc::new(Mutex::new(LoopGuard::new(loop_guard_config))),
            tool_router: Self::tool_router(),
            sent_ranges: Arc::new(Mutex::new(HashMap::new())),
            search_digests: Arc::new(Mutex::new(HashMap::new())),
            symbol_page_generations: Arc::new(Mutex::new(HashMap::new())),
            context_page_generations: Arc::new(Mutex::new(HashMap::new())),
            call_seq: Arc::new(AtomicU64::new(0)),
            shown_once: Arc::new(Mutex::new(std::collections::HashSet::new())),
            shared: None,
            coverage_note_override: None,
            recent_symbols: Arc::new(crate::recent_symbol::RecentSymbols::new()),
        }
    }

    /// The many-hits next-step footer, once per session.
    fn many_hits_once(&self, total: usize) -> String {
        if total > GROUP_FULL_HITS && self.once("search_code.many_hits") {
            many_hits_hint(total)
        } else {
            String::new()
        }
    }

    /// `true` the first time `key` is asked for in this MCP session: fixed
    /// legends and footers are printed once, not on every result.
    pub(crate) fn once(&self, key: &'static str) -> bool {
        self.shown_once
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key)
    }

    pub fn start_indexing(&self) {
        if let Some(dispatch) = &self.dispatch {
            start_idle_ticker(Arc::clone(dispatch), Arc::clone(&self.ticker_stop));
            return;
        }
        start_root_indexing(Arc::clone(&self.primary));
    }

    /// Stop watchers and the idle-evict ticker. Tests call this to avoid
    /// leaving pollers running.
    pub fn stop_watching(&self) {
        self.ticker_stop.store(true, Ordering::SeqCst);
        self.primary.teardown();
        if let Some(dispatch) = &self.dispatch {
            teardown_all(dispatch);
        }
        if let Some(dispatch) = self
            .dynamic_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            teardown_all(dispatch);
        }
    }

    /// Server shutdown: close every session — which cancels in-flight cold
    /// builds and background rebuilds, stops watchers and releases indexer
    /// locks — then wait up to `timeout` for the cancelled builds to reach
    /// their safe point (no store writes after cancel). Returns `true` when
    /// every build stopped in time.
    pub fn shutdown_indexing(&self, timeout: std::time::Duration) -> bool {
        let mut sessions = vec![Arc::clone(&self.primary)];
        if let Some(dispatch) = &self.dispatch {
            sessions.extend(
                dispatch
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .cloned(),
            );
        }
        if let Some(dispatch) = self
            .dynamic_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            sessions.extend(
                dispatch
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .cloned(),
            );
        }
        self.stop_watching();
        let deadline = std::time::Instant::now() + timeout;
        sessions.iter().all(|session| {
            session.wait_builds_idle(deadline.saturating_duration_since(std::time::Instant::now()))
        })
    }

    /// Native facade: readiness of the primary index.
    /// `0` = building, `1` = ready, `2` = failed (with message).
    #[cfg(feature = "native")]
    pub(crate) fn native_primary_state(&self) -> (u8, Option<String>) {
        match &*self.primary.state.read().unwrap_or_else(|e| e.into_inner()) {
            IndexState::Building => (0, None),
            IndexState::Ready(_) => (1, None),
            IndexState::Failed(err) => (2, Some(err.clone())),
        }
    }

    /// Native facade: counters of the built index, `None` while building.
    #[cfg(feature = "native")]
    pub(crate) fn native_primary_stats(&self) -> Option<Value> {
        match &*self.primary.state.read().unwrap_or_else(|e| e.into_inner()) {
            IndexState::Ready(index) => Some(json!({
                "generation": index.generation(),
                "files": index.graph().files.len(),
                "symbols": index.graph().symbols.len(),
                "scanned": index.report().files_scanned,
                "parsed": index.report().files_parsed,
                "parse_failures": index.report().parse_failures.len(),
            })),
            _ => None,
        }
    }

    /// Native facade: `true` when this process owns the write side of
    /// `<repo>/.astrolabe`, `false` when it reads the leader's store.
    #[cfg(feature = "native")]
    pub(crate) fn native_leader_held(&self) -> bool {
        self.primary
            .indexer_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|lock| lock.held())
    }

    /// Native facade: the canonical root of the primary session.
    #[cfg(feature = "native")]
    pub(crate) fn native_primary_root(&self) -> &Path {
        &self.primary.root
    }

    /// Native facade: the built index, `None` while building.
    #[cfg(feature = "native")]
    pub(crate) fn native_index_snapshot(&self) -> Option<Arc<RepoIndex>> {
        match &*self.primary.state.read().unwrap_or_else(|e| e.into_inner()) {
            IndexState::Ready(index) => Some(Arc::clone(index)),
            _ => None,
        }
    }

    /// Native facade: apply edits the host already made to disk, synchronously,
    /// instead of waiting for the watcher. A host that owns the edit tools calls
    /// this right after a successful write so the next query sees the new text.
    ///
    /// Paths are repo-relative (forward slashes). A path the index does not know
    /// is classified as newly added, so a just-created file becomes searchable.
    /// Returns `true` when every change was applied to the live index.
    #[cfg(feature = "native")]
    pub fn native_note_edited(&self, paths: &[String]) -> bool {
        if paths.is_empty() {
            return true;
        }
        let known: std::collections::HashSet<String> =
            match &*self.primary.state.read().unwrap_or_else(|e| e.into_inner()) {
                IndexState::Ready(index) => index
                    .graph()
                    .files
                    .iter()
                    .map(|file| file.path.as_str().to_string())
                    .collect(),
                _ => std::collections::HashSet::new(),
            };
        let mut changes = astrolabe_core::ChangeSet::default();
        for path in paths {
            let rel = astrolabe_core::RelPath::new(path);
            if known.contains(rel.as_str()) {
                changes.modified.push(rel);
            } else {
                changes.added.push(rel);
            }
        }
        crate::session::note_edited(&self.primary, &changes)
    }

    fn session_for(&self, hint: &str) -> Result<Arc<RootSession>, CallToolResult> {
        if hint.is_empty() {
            return Ok(Arc::clone(&self.primary));
        }
        let hint_path = Path::new(hint);
        if hint_path == self.primary.root.as_path() || self.primary.root.to_string_lossy() == hint {
            return Ok(Arc::clone(&self.primary));
        }
        if let Some(dispatch) = &self.dispatch {
            return match session::route_session(dispatch, hint) {
                Ok(s) => Ok(s),
                Err(msg) => Err(self.finish(text_error(msg))),
            };
        }
        if let Some(dispatch) = self
            .dynamic_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return match session::route_session(dispatch, hint) {
                Ok(s) => Ok(s),
                Err(msg) => Err(self.finish(text_error(msg))),
            };
        }
        Ok(Arc::clone(&self.primary))
    }

    /// Resolve optional per-call `index_root`, then fall back to multi-project
    /// `root` / path / task routing. `index_root` wins and keeps `path` /
    /// `path_filter` relative to that directory.
    fn resolve_call_root(
        &self,
        index_root: Option<&str>,
        root: Option<&str>,
        path: Option<&str>,
        text: Option<&str>,
    ) -> Result<RouteOutcome, CallToolResult> {
        let override_raw = index_root.map(str::trim).filter(|s| !s.is_empty());
        if let Some(raw) = override_raw {
            let resolved = match resolve_call_index_root(raw) {
                Ok(p) => p,
                Err(msg) => {
                    return Err(self.finish(text_error(format!("{msg}\nconfidence: unknown"))))
                }
            };
            let session = self.ensure_index_root_session(&resolved)?;
            let keep_path = path.map(str::to_string).filter(|p| !p.is_empty());
            return Ok(RouteOutcome {
                hint: session.root.to_string_lossy().into_owned(),
                path: keep_path,
                note: None,
            });
        }
        self.resolve_route(root, path, text)
    }

    /// Open or reuse a [`RootSession`] for an absolute index root.
    fn ensure_index_root_session(
        &self,
        resolved: &Path,
    ) -> Result<Arc<RootSession>, CallToolResult> {
        if resolved == self.primary.root.as_path() {
            return Ok(Arc::clone(&self.primary));
        }
        if let Some(dispatch) = &self.dispatch {
            return Ok(activate_absolute_root(dispatch, resolved));
        }
        let mut slot = self.dynamic_roots.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let dispatch = DispatchState::new(
                Vec::new(),
                self.primary.knobs.clone(),
                self.cache_budget_bytes,
            );
            start_idle_ticker(Arc::clone(&dispatch), Arc::clone(&self.ticker_stop));
            *slot = Some(dispatch);
        }
        let dispatch = Arc::clone(slot.as_ref().expect("just inserted"));
        drop(slot);
        Ok(activate_absolute_root(&dispatch, resolved))
    }

    /// Remember a single unambiguous symbol hit for later find_references.
    fn remember_symbol_anchor(
        &self,
        root: &Path,
        symbol: &str,
        path: &str,
        line_0based: Option<u32>,
    ) {
        self.recent_symbols
            .remember(root, symbol, path, line_0based);
    }

    /// When find_symbol / path-query yields exactly one exact-name hit, remember it.
    fn remember_if_unique_symbol_hit(
        &self,
        root: &Path,
        typed: &str,
        index: &crate::index::RepoIndex,
        hits: &[(String, usize)],
    ) {
        let typed = typed.trim();
        if typed.is_empty() || hits.is_empty() {
            return;
        }
        let symbols = &index.graph().symbols;
        let exact: Vec<&astrolabe_core::CodeSymbol> = hits
            .iter()
            .map(|(_, idx)| &symbols[*idx])
            .filter(|s| s.name == typed || s.name.eq_ignore_ascii_case(typed))
            .collect();
        if exact.len() != 1 {
            return;
        }
        let s = exact[0];
        let Some(file) = index.file(s.file) else {
            return;
        };
        // Index lines are 1-based; find_references line is 0-based.
        let line = s.start_line.saturating_sub(1);
        self.remember_symbol_anchor(root, &s.name, file.path.as_str(), Some(line));
    }

    /// Resolve which sub-repo a call targets. Priority: explicit `root` >
    /// `path` (absolute under a child, or starting with the child's name,
    /// rewritten to a child-relative path) > task-text mention (only when
    /// `text` is given; noted in the result) > the only resident child.
    /// Single-repo servers ignore all of it. The returned `hint` is the
    /// child's absolute path, which downstream routing matches exactly.
    fn resolve_route(
        &self,
        root: Option<&str>,
        path: Option<&str>,
        text: Option<&str>,
    ) -> Result<RouteOutcome, CallToolResult> {
        let keep_path = path.map(str::to_string);
        let Some(dispatch) = &self.dispatch else {
            return Ok(RouteOutcome {
                hint: String::new(),
                path: keep_path,
                note: None,
            });
        };
        let root = root.map(str::trim).filter(|r| !r.is_empty());
        let disp = dispatch
            .dispatcher
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let outcome = if let Some(r) = root {
            match disp.route_explicit(r) {
                Some(child) => {
                    let rel = path.map(|p| relativize_to_child(child, p));
                    Some((child.to_path_buf(), rel.or(keep_path.clone()), None))
                }
                None => {
                    drop(disp);
                    return Err(self.finish(text_error(format!(
                        "root `{r}` matches no sub-repo.\n{}",
                        session::unroutable_message(&dispatch.children)
                    ))));
                }
            }
        } else if let Some((child, rest)) = path.and_then(|p| disp.route_path(p)) {
            Some((child.to_path_buf(), Some(rest), None))
        } else if let Some(child) = text.and_then(|t| disp.route(t)) {
            let note = format!(
                "Routing note: guessed sub-repo {} from names in the task text; pass root to pick a sub-repo explicitly.",
                child.display()
            );
            Some((child.to_path_buf(), keep_path.clone(), Some(note)))
        } else {
            disp.sole_resident()
                .map(|child| (child.to_path_buf(), keep_path.clone(), None))
        };
        drop(disp);
        match outcome {
            Some((child, path, note)) => Ok(RouteOutcome {
                hint: child.to_string_lossy().into_owned(),
                path: path.filter(|p| !p.is_empty()),
                note,
            }),
            None => Err(self.finish(text_error(session::unroutable_message(&dispatch.children)))),
        }
    }

    fn with_route_note(result: CallToolResult, note: &Option<String>) -> CallToolResult {
        match note {
            Some(note) => append_stale_note(result, note),
            None => result,
        }
    }

    /// The routed session, when its index is still in the cold build.
    fn building_session(&self, hint: &str) -> Option<Arc<RootSession>> {
        let session = self.session_for(hint).ok()?;
        let building = matches!(
            &*session.state.read().unwrap_or_else(|e| e.into_inner()),
            IndexState::Building
        );
        building.then_some(session)
    }

    /// `search_code` while the index is still building: a plain literal scan
    /// of the files on disk (`[text-only]`, no symbol context), so the first
    /// calls after startup get an answer instead of "try again later".
    fn search_while_building(
        &self,
        session: &RootSession,
        params: &SearchParams,
    ) -> CallToolResult {
        const MAX_SHOWN: usize = 60;
        const MAX_SECONDS: u64 = 20;
        let mut params = params.clone();
        let regex_downgraded = apply_code_fragment_literal(&mut params);
        let sensitive = search_shape::literal_case_sensitive(
            &params.query,
            params.regex,
            params.case_sensitive,
        );
        let matcher = match build_search_matcher(&params.query, params.regex, Some(sensitive)) {
            Ok(matcher) => matcher,
            Err(error) => return text_error(invalid_regex_message(error)),
        };
        let exclude_set = match build_exclude_set(params.exclude.as_deref()) {
            Ok(set) => set,
            Err(message) => return text_error(message),
        };
        let started = std::time::Instant::now();
        let scanned = astrolabe_core::scan::scan(
            &session.root,
            &crate::session::scan_options(&session.knobs),
        );
        let mut total = 0usize;
        let mut files_hit = 0usize;
        let mut lines: Vec<String> = Vec::new();
        let mut complete = true;
        for rel in &scanned.files {
            let path = rel.as_str();
            if params
                .path_filter
                .as_deref()
                .is_some_and(|f| !path.contains(f))
                || exclude_set.as_ref().is_some_and(|set| set.is_match(path))
            {
                continue;
            }
            if cancel::is_cancelled() || started.elapsed().as_secs() >= MAX_SECONDS {
                complete = false;
                break;
            }
            let Ok(text) = std::fs::read_to_string(session.root.join(path)) else {
                continue;
            };
            let mut file_hit = false;
            for (idx, line) in text.lines().enumerate() {
                if !matcher.is_match(line) {
                    continue;
                }
                file_hit = true;
                total += 1;
                if total > params.offset && lines.len() < MAX_SHOWN && !params.count_only {
                    lines.push(format!(
                        "{path}:{}: {}",
                        idx + 1,
                        clip_match_line(line, &params.query, Some(&matcher))
                    ));
                }
            }
            if file_hit {
                files_hit += 1;
            }
        }
        let mut body = String::from(
            "confidence: exact (on-disk source literal scan; index still building in background; no symbol/role tags, [text-only])\n",
        );
        body.push_str(&render_search_mode_header(
            params.regex,
            &params.query,
            regex_downgraded,
        ));
        if params.aggregate.is_some() {
            body.push_str("aggregate grouping needs a ready index; totals only here.\n");
        }
        for line in &lines {
            body.push_str(line);
            body.push('\n');
        }
        let shown = lines.len();
        if complete {
            let hw = if total == 1 { "hit" } else { "hits" };
            let fw = if files_hit == 1 { "file" } else { "files" };
            body.push_str(&format!(
                "{total} {hw} / {files_hit} {fw} (all scanned), showing {shown}{}. After the index is ready the same query will include symbol tags.\n",
                if total > params.offset + shown && !params.count_only {
                    "; page the rest with offset"
                } else {
                    ""
                }
            ));
        } else {
            let hw = if total == 1 { "hit" } else { "hits" };
            let fw = if files_hit == 1 { "file" } else { "files" };
            body.push_str(&format!(
                "Stopped after scanning some files (timeout or cancel): at least {total} {hw} / {files_hit} {fw}; incomplete. Retry after the index is ready.\n"
            ));
        }
        self.finish_at(
            &session.root,
            CallToolResult::success(vec![ContentBlock::text(body)]),
        )
    }

    /// Index access without a write barrier and without the per-query
    /// coverage note: for `get_languages`, whose own body already carries the
    /// full coverage block.
    fn with_index_without_coverage_note(
        &self,
        hint: &str,
        f: impl FnOnce(&RootSession, &RepoIndex) -> CallToolResult,
    ) -> CallToolResult {
        self.with_index_barrier(hint, false, false, f)
    }

    fn with_fresh_index(
        &self,
        hint: &str,
        f: impl FnOnce(&RootSession, &RepoIndex) -> CallToolResult,
    ) -> CallToolResult {
        self.with_index_barrier(hint, true, true, f)
    }

    fn with_index_barrier(
        &self,
        hint: &str,
        barrier: bool,
        coverage_note: bool,
        f: impl FnOnce(&RootSession, &RepoIndex) -> CallToolResult,
    ) -> CallToolResult {
        if cancel::is_cancelled() {
            return text_error("Request cancelled. confidence: unknown");
        }
        let session = match self.session_for(hint) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let effect = if barrier {
            run_barrier(&session)
        } else {
            BarrierEffect::None
        };
        // Clone the `Arc` and release the read guard before running the tool:
        // a long scan must not keep a queued watcher write lock (and every
        // reader behind it) waiting. The query runs on its own snapshot.
        enum Snap {
            Building,
            Failed(String),
            Ready(Arc<RepoIndex>),
        }
        let snap = match &*session.state.read().unwrap_or_else(|e| e.into_inner()) {
            IndexState::Building => Snap::Building,
            IndexState::Failed(error) => Snap::Failed(error.clone()),
            IndexState::Ready(index) => Snap::Ready(Arc::clone(index)),
        };
        let result = match snap {
            Snap::Building => self.finish_at(
                &session.root,
                text_error("Index is building in the background; MCP handshake is done — retry shortly. confidence: unknown"),
            ),
            Snap::Failed(error) => self.finish_at(
                &session.root,
                text_error(format!("Index build failed: {error}\nconfidence: unknown")),
            ),
            Snap::Ready(index) => {
                let result = f(&session, &index);
                if !coverage_note || result.is_error == Some(true) {
                    result
                } else {
                    let text = result
                        .content
                        .first()
                        .and_then(|block| block.as_text())
                        .map(|t| t.text.as_str())
                        .unwrap_or_default();
                    match index.query_coverage_note(text, self.coverage_slot(&session)) {
                        Some(note) => append_stale_note(result, &note),
                        None => result,
                    }
                }
            }
        };
        match effect {
            BarrierEffect::None => result,
            BarrierEffect::StaleNote(note) => append_stale_note(result, &note),
        }
    }

    fn with_precise(
        &self,
        hint: &str,
        barrier: bool,
        f: impl FnOnce(&RootSession) -> CallToolResult,
    ) -> CallToolResult {
        let session = match self.session_for(hint) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let effect = if barrier {
            run_barrier(&session)
        } else {
            BarrierEffect::None
        };
        let result = self.finish_at(&session.root, f(&session));
        match effect {
            BarrierEffect::None => result,
            BarrierEffect::StaleNote(note) => append_stale_note(result, &note),
        }
    }

    /// The coordinate legend of find_references / goto_definition is the
    /// same sentence every time (seven times in one A/B run); keep the
    /// first one in the session and drop the rest.
    fn coord_note_once(&self, mut result: CallToolResult) -> CallToolResult {
        let notes = [
            crate::precise_tools::REF_COORD_NOTE,
            crate::precise_tools::COORD_NOTE,
        ];
        let Some(text) = result
            .content
            .first()
            .and_then(|block| block.as_text())
            .map(|t| t.text.clone())
        else {
            return result;
        };
        let Some(note) = notes.into_iter().find(|n| text.contains(n)) else {
            return result;
        };
        if !self.once("precise.coord_note") {
            result.content[0] = ContentBlock::text(text.replacen(note, "", 1));
        }
        result
    }

    /// Catalog for `tools/list`: drop `excluded_tools`, optionally sanitize
    /// schemas for OpenAI/Codex, then sort by name.
    fn listed_tools(&self) -> Vec<rmcp::model::Tool> {
        let mut tools = self.tool_router.list_all();
        tools.retain(|tool| !self.context.excludes(tool.name.as_ref()));
        for tool in &mut tools {
            let mut schema = Value::Object((*tool.input_schema).clone());
            slim_schema(&mut schema);
            if !full_schema_requested() {
                hide_unadvertised_params(&mut schema, self.dispatch.is_some());
            }
            if let Value::Object(slim) = schema {
                tool.input_schema = Arc::new(slim);
            }
            // Every served tool only reads the repository (dry_run_rename
            // previews, never writes). Claude Code runs MCP calls from one
            // turn concurrently only when `readOnlyHint` is true
            // (`isConcurrencySafe`); without it they queue serially. A tool
            // that writes must not get this hint.
            tool.annotations = Some(rmcp::model::ToolAnnotations::new().read_only(true));
        }
        if self.context.openai_tool_compatible() {
            for tool in &mut tools {
                let schema = Value::Object((*tool.input_schema).clone());
                if let Value::Object(sanitized) = sanitize_for_openai_tools(schema) {
                    tool.input_schema = Arc::new(sanitized);
                }
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Generation of an already-routed session's Ready index; no barrier.
    fn stamp_of(session: &RootSession) -> Option<u64> {
        let state = session.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            IndexState::Ready(index) => Some(index.generation()),
            _ => None,
        }
    }

    /// Run a search tool through the per-connection [`LoopGuard`].
    ///
    /// Only successful executions are counted; the key includes every
    /// argument and the index generation, so edits reset the count. A blocked
    /// call returns the cached previous result (index unchanged) with a
    /// `blocked: true` marker.
    fn guarded(
        &self,
        tool: &str,
        hint: &str,
        signature: &str,
        run: impl FnOnce() -> CallToolResult,
    ) -> CallToolResult {
        // Route once; the barrier runs once here. The tool body's own
        // `with_fresh_index` runs it again, and the post-run stamp reuses the
        // routed session instead of routing and waiting a third time.
        let Ok(session) = self.session_for(hint) else {
            return run();
        };
        let _ = run_barrier(&session);
        let Some(stamp) = Self::stamp_of(&session) else {
            return run();
        };
        let verdict = self
            .loop_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(tool, signature, stamp);
        let note = match verdict {
            Verdict::Block { note, cached } => {
                tracing::warn!(tool, "loop guard blocked repeated query");
                return mark_blocked(cached, &note);
            }
            Verdict::Run => None,
            Verdict::RunWithWarning(note) => Some(note),
        };
        let mut result = run();
        if result.is_error != Some(true) {
            // Re-read the stamp: the run itself may have absorbed edits.
            if let Some(after) = Self::stamp_of(&session) {
                self.loop_guard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record(tool, signature, after, result.clone());
            }
        }
        if let Some(note) = note {
            if let Some(first) = result.content.first_mut() {
                if let Some(t) = first.as_text() {
                    *first = ContentBlock::text(format!("{}\n\n{note}", t.text));
                }
            }
        }
        result
    }

    fn finish(&self, result: CallToolResult) -> CallToolResult {
        self.finish_at(&self.root, result)
    }

    fn finish_at(&self, root: &Path, mut result: CallToolResult) -> CallToolResult {
        let root = root.display().to_string();
        let mut first_text = None;
        let mut content = Vec::with_capacity(result.content.len());
        for (index, block) in result.content.into_iter().enumerate() {
            if index == 0 {
                if let Some(existing) = block.as_text() {
                    let text = if existing.text.starts_with("index_root:") {
                        existing.text.clone()
                    } else {
                        format!("index_root: {root}\n{}", existing.text)
                    };
                    first_text = Some(text.clone());
                    content.push(ContentBlock::text(text));
                    continue;
                }
            }
            content.push(block);
        }
        result.content = content;

        if self.structured_output {
            let text = first_text.unwrap_or_default();
            let mut structured = result
                .structured_content
                .take()
                .unwrap_or_else(|| json!({}));
            if let Some(obj) = structured.as_object_mut() {
                obj.insert("root".into(), json!(root));
                obj.insert("text".into(), json!(text));
            }
            result.structured_content = Some(structured);
        } else {
            result.structured_content = None;
        }
        result
    }

    /// 动态卸载阈值：基线与 caller budget（钳制后）的较大者。caller 明确
    /// 给了更大的 budget 时，正文允许留在线内而不是立刻被卸载。
    fn resource_threshold_for(&self, caller_budget: Option<usize>) -> usize {
        resource_threshold_base_from_env().max(caller_budget.map(clamp_budget).unwrap_or(0))
    }

    #[cfg(test)]
    fn result(
        &self,
        title: &str,
        body: String,
        structured: Value,
        budget: Option<usize>,
    ) -> CallToolResult {
        self.result_on(&self.root, title, body, structured, budget)
    }

    fn result_on(
        &self,
        root: &Path,
        title: &str,
        body: String,
        structured: Value,
        budget: Option<usize>,
    ) -> CallToolResult {
        let body = if body.trim().is_empty() {
            "No matches. confidence: unknown".to_string()
        } else {
            body
        };
        // 双保险：估算器有偏差，判定线额外加 SLACK，避免在阈值边界处
        // 把刚好等于 budget 的正文误卸载。
        let threshold = self.resource_threshold_for(budget);
        if estimate_tokens(&body) > threshold.saturating_add(RESOURCE_SLACK_TOKENS) {
            let id = self.next_resource.fetch_add(1, Ordering::Relaxed);
            let mut store = self.resources.lock().unwrap_or_else(|e| e.into_inner());
            let uri = format!("astrolabe://results/{}-{id}", store.nonce);
            store.insert(uri.clone(), body.clone());
            drop(store);
            let resource = Resource::new(&uri, title)
                .with_description("Astrolabe large tool result; fetch full text via resources/read")
                .with_mime_type("text/plain")
                .with_size(body.len() as u64);
            let preview_text = resource_preview(&body);
            let mut result = CallToolResult::success(vec![
                ContentBlock::text(format!(
                    "{title} result is large (~{} tokens); spilled to resource: {uri}\n\n[Match preview]:\n{preview_text}\n\nFetch the full content via resources/read.",
                    estimate_tokens(&body)
                )),
                ContentBlock::resource_link(resource),
            ]);
            result.structured_content = Some(structured);
            self.finish_at(root, result)
        } else {
            let mut result = CallToolResult::success(vec![ContentBlock::text(body)]);
            result.structured_content = Some(structured);
            self.finish_at(root, result)
        }
    }
}

/// Drop schemars boilerplate that carries no information for a client:
/// `$schema`, `"default": null`, `format: uint*` + `minimum: 0` (the integer
/// type is enough), and the `["x","null"]` union of `Option<T>` fields, which
/// becomes plain `"x"`: optional is already expressed by absence from
/// `required`, and deserialization still accepts an explicit `null`.
/// Parameters still accepted by every tool but left out of `tools/list`.
/// None was passed once in 66 A/B runs (306 Astrolabe calls), yet together
/// they were 3.4 KB of the 7.3 KB catalog, resent on every API turn. Tool
/// descriptions, results and the L2 manual still name the useful ones
/// (`regex`, `exclude`, `depth`, `kind`, `dry_run_rename`, `offset` from a
/// result's next_offset). `ASTROLABE_FULL_SCHEMA=1` advertises them all.
const UNADVERTISED_PARAMS: &[&str] = &[
    "budget_tokens",
    "offset",
    "page",
    "depth",
    "kind",
    "substring",
    "dedup",
    "context_lines",
    "count_only",
    "exclude",
    "group",
    "regex",
    "dry_run_rename",
    "character",
    "severity",
    "lsp",
];

fn full_schema_requested() -> bool {
    matches!(
        std::env::var("ASTROLABE_FULL_SCHEMA")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Drop [`UNADVERTISED_PARAMS`] (and `root` outside multi-root sessions)
/// from a tool's advertised input schema. Deserialization is untouched, so
/// callers that pass them keep working.
fn hide_unadvertised_params(schema: &mut Value, multi_root: bool) {
    let hidden =
        |name: &str| UNADVERTISED_PARAMS.contains(&name) || (!multi_root && name == "root");
    if let Some(Value::Object(props)) = schema.get_mut("properties") {
        props.retain(|name, _| !hidden(name));
    }
    if let Some(Value::Array(required)) = schema.get_mut("required") {
        required.retain(|name| !name.as_str().is_some_and(hidden));
    }
}

fn slim_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            if map.get("default").is_some_and(Value::is_null) {
                map.remove("default");
            }
            if let Some(Value::Array(types)) = map.get("type") {
                if let [a, b] = types.as_slice() {
                    let null = Value::String("null".into());
                    let kept = if *b == null {
                        Some(a.clone())
                    } else if *a == null {
                        Some(b.clone())
                    } else {
                        None
                    };
                    if let Some(kept) = kept {
                        map.insert("type".into(), kept);
                    }
                }
            }
            let unsigned = map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|f| f.starts_with("uint"));
            if unsigned {
                map.remove("format");
                if map.get("minimum").and_then(Value::as_u64) == Some(0) {
                    map.remove("minimum");
                }
            }
            for child in map.values_mut() {
                slim_schema(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                slim_schema(child);
            }
        }
        _ => {}
    }
}

/// Cooperative cancellation for tool calls running on a blocking thread.
///
/// `call_tool` sets a per-thread flag; long loops may poll [`is_cancelled`]
/// and bail out early. Tool bodies are synchronous and cannot be preempted.
pub(crate) mod cancel {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    thread_local! {
        static FLAG: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    }

    pub(crate) fn scope<T>(flag: Arc<AtomicBool>, f: impl FnOnce() -> T) -> T {
        FLAG.with(|cell| *cell.borrow_mut() = Some(flag));
        let out = f();
        FLAG.with(|cell| *cell.borrow_mut() = None);
        out
    }

    /// The current thread's flag, for worker threads that cannot see the
    /// thread-local.
    pub(crate) fn current() -> Option<Arc<AtomicBool>> {
        FLAG.with(|cell| cell.borrow().clone())
    }

    pub(crate) fn is_cancelled() -> bool {
        FLAG.with(|cell| {
            cell.borrow()
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
        })
    }
}

/// Per-call wall-clock cap. `ASTROLABE_TOOL_TIMEOUT_SECS=0` disables it.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 180;

fn tool_timeout_from_env() -> Option<std::time::Duration> {
    let secs = std::env::var("ASTROLABE_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// Stand-in server used when startup fails (bad root, oversized directory,
/// bad flag): it still answers the MCP handshake and `tools/list`, and every
/// `tools/call` returns the failure as an `isError` result, so the client UI
/// shows the reason instead of a bare "connection closed".
#[derive(Clone)]
pub struct StartupFailedServer {
    message: String,
    inner: AstrolabeServer,
}

impl StartupFailedServer {
    pub fn new(message: impl Into<String>, context: ClientContext) -> Self {
        let message = message.into();
        // Only supplies the catalog and protocol negotiation; never indexed.
        let inner = AstrolabeServer::with_context(std::env::temp_dir(), context);
        Self { message, inner }
    }

    fn failure_result(&self) -> CallToolResult {
        text_error(format!(
            "Astrolabe failed to start; no index built:\n{}\nconfidence: unknown",
            self.message
        ))
    }
}

impl ServerHandler for StartupFailedServer {
    fn get_info(&self) -> ServerConfig {
        self.inner.get_info()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        self.inner.supported_protocol_versions()
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        self.inner.initialize(request, context).await
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.inner.list_tools(request, context).await
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(self.failure_result().into())
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        Err(custom_request_error(&request.method))
    }
}

/// JSON-RPC error for a request that reached the custom-request fallback:
/// a standard MCP method with malformed `params` is `-32602 Invalid params`;
/// anything else is `-32601 Method not found`.
fn custom_request_error(method: &str) -> ErrorData {
    const STANDARD: &[&str] = &[
        "initialize",
        "tools/call",
        "tools/list",
        "resources/list",
        "resources/read",
        "resources/templates/list",
        "prompts/list",
        "prompts/get",
        "completion/complete",
        "logging/setLevel",
    ];
    if STANDARD.contains(&method) {
        ErrorData::invalid_params(
            format!("Invalid params for `{method}`: params do not match the method's schema (e.g. `arguments` must be an object)"),
            None,
        )
    } else {
        ErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
            None,
        )
    }
}

/// Put `note` at the start of the first text block (after any leading blank).
fn prepend_tool_note(result: CallToolResult, note: &str) -> CallToolResult {
    let mut result = result;
    if let Some(first) = result.content.first_mut() {
        if let Some(existing) = first.as_text() {
            let new_text = format!("{note}{}", existing.text);
            result.content[0] = ContentBlock::text(new_text);
        }
    }
    result
}

/// Where a call goes after routing (see [`AstrolabeServer::resolve_route`]).
struct RouteOutcome {
    hint: String,
    path: Option<String>,
    note: Option<String>,
}

/// An absolute `path` under `child` becomes child-relative; anything else is
/// returned unchanged.
fn relativize_to_child(child: &Path, path: &str) -> String {
    let p = Path::new(path.trim());
    if p.is_absolute() {
        if let Ok(rel) = p.strip_prefix(child) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string()
}

fn top_unindexed_exts(
    counts: &std::collections::BTreeMap<String, u32>,
    limit: usize,
) -> Vec<(&str, u32)> {
    let mut list: Vec<(&str, u32)> = counts.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    list.truncate(limit);
    list
}

fn format_search_code_unindexed_hint(
    counts: &std::collections::BTreeMap<String, u32>,
    path_filter: Option<&str>,
) -> Option<String> {
    let max_count = counts.values().copied().max().unwrap_or(0);
    if max_count < 3 {
        return None;
    }
    let top = top_unindexed_exts(counts, 3);
    if top.is_empty() {
        return None;
    }
    let ext_summary = top
        .iter()
        .map(|(ext, count)| format!("{count} .{ext} files"))
        .collect::<Vec<_>>()
        .join(", ");
    let msg = match path_filter {
        Some(filter) => format!(
            "Note: within path_filter '{filter}', {ext_summary} belong to unsupported languages and are not in the code index — searched as plain text too, also no matches; they have no symbol/reference info.\n"
        ),
        None => format!(
            "Note: the repo has {ext_summary} in unsupported languages, not in the code index — searched as plain text too, also no matches; they have no symbol/reference info.\n"
        ),
    };
    Some(msg)
}

fn format_find_symbol_unindexed_hint(
    counts: &std::collections::BTreeMap<String, u32>,
) -> Option<String> {
    let max_count = counts.values().copied().max().unwrap_or(0);
    if max_count < 3 {
        return None;
    }
    let top = top_unindexed_exts(counts, 3);
    if top.is_empty() {
        return None;
    }
    let ext_summary = top
        .iter()
        .map(|(ext, count)| format!("{count} .{ext}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "(repo also has {ext_summary} etc. unsupported-language sources outside the symbol index; symbol search does not cover them — use Grep/Read)\n"
    ))
}

#[tool_router]
impl AstrolabeServer {
    #[tool(description = "When the task is unclear: task-ranked related files and symbols.")]
    pub(crate) fn resolve_context(
        &self,
        Parameters(params): Parameters<ResolveContextParams>,
    ) -> CallToolResult {
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            params.path_filter.as_deref(),
            Some(&params.task_description),
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        let mut params = params;
        params.path_filter = route.path.clone();
        let result = self.resolve_context_inner(params, &route.hint);
        Self::with_route_note(result, &route.note)
    }

    fn resolve_context_inner(&self, params: ResolveContextParams, hint: &str) -> CallToolResult {
        self.with_fresh_index(hint, |session, index| {
            let graph = index.graph();
            let kind = classify_task(&params.task_description);
            let compact_task = matches!(kind, TaskKind::Count | TaskKind::Refs);
            // Counting / caller tasks are answered by the follow-up tool, not
            // by a file map: keep the reply under ~3 KB unless the caller set
            // an explicit budget.
            let budget = if compact_task && params.budget_tokens == DEFAULT_BUDGET {
                COMPACT_CONTEXT_BUDGET
            } else {
                params.budget_tokens
            };

            // Repeat call with the same arguments on an unchanged index:
            // short pointer instead of resending the whole map.
            let dedup_on = session_dedup_enabled(params.dedup);
            let call = self.call_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let key = format!(
                "{}|resolve|{}|{}|{}|{}",
                session.root.display(),
                params.task_description,
                params.budget_tokens,
                params.offset,
                params.path_filter.as_deref().unwrap_or("")
            );
            let generation = index.generation();
            if dedup_on {
                let earlier = {
                    let mut map = self.sent_ranges.lock().unwrap_or_else(|e| e.into_inner());
                    match map.get(&key) {
                        Some((g, c)) if *g == generation => Some(*c),
                        _ => {
                            map.insert(key.clone(), (generation, call));
                            None
                        }
                    }
                };
                if let Some(c) = earlier {
                    let digest = self
                        .search_digests
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    let body = format!(
                        "Same resolve_context already returned on tool call #{c} this session (index unchanged); not resending full text.\n{digest}If you did not see that result (e.g. subtask or compressed context), pass dedup=false to refetch.\n{}",
                        next_step_hint(&params.task_description)
                    );
                    return self.result_on(
                        &session.root,
                        "resolve_context",
                        body,
                        json!({"deduplicated":true,"earlier_call":c,"budget_tokens":params.budget_tokens}),
                        Some(params.budget_tokens),
                    );
                }
            }

            let base = cached_centrality(graph);
            let (ranked, matched) = rank_and_match(graph, &params.task_description, &base);
            let index_line = format!(
                "index: scanned={}, parsed={}, parse_failures={}, unresolved_imports={}\n",
                index.report().files_scanned,
                index.report().files_parsed,
                index.report().parse_failures.len(),
                index.report().unresolved_imports.len()
            );
            let generation_note = self.context_page_drift_note(&params, generation);
            let page = crate::context_page::build_page(&crate::context_page::PageRequest {
                graph,
                ranked: &ranked,
                matched: &matched,
                offset: params.offset,
                budget,
                compact: compact_task,
                path_filter: params.path_filter.as_deref(),
                index_line: &index_line,
                generation_note: generation_note.as_deref(),
            });
            let mut body = page.body;
            let top_files = page.top_files;
            // A Go interface the task names: list its implementing types
            // (implicit satisfaction is not visible in the file map).
            if !page.unmatched {
                let words: std::collections::HashSet<&str> = params
                    .task_description
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .filter(|w| !w.is_empty())
                    .collect();
                let named = matched.symbols.iter().copied().filter(|&i| {
                    let s = &graph.symbols[i];
                    s.kind == astrolabe_core::SymbolKind::Interface
                        && words.contains(s.name.as_str())
                });
                body.push_str(&search_shape::implementors_block(index, named, budget / 4));
            }
            body.push_str(next_step_hint(&params.task_description));
            if dedup_on {
                let digest = if top_files.is_empty() {
                    "First-result digest: no related files matched.\n".to_string()
                } else {
                    format!("First-result digest: top related files {}\n", top_files.join(", "))
                };
                self.search_digests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, digest);
            }
            let conf_label = if page.unmatched { "unknown" } else { "scoped" };
            self.result_on(
                &session.root,
                "resolve_context",
                body,
                json!({"confidence":conf_label,"matched":!page.unmatched,"budget_tokens":budget,"offset":params.offset,"total":page.total}),
                Some(budget),
            )
        })
    }

    // ---- precise, language-server backed tools ---------------------------
    // These answer the questions the graph cannot: name-matched call edges
    // recall 66% on TypeScript and 18% on Python, so "where is this symbol
    // referenced" has to come from a real language server. Each forwards to
    // `precise_tools`, which reports an actionable install hint (and
    // `confidence: unknown`) when no server is available — never an empty
    // result that reads like "there are no references".
    #[tool(description = crate::precise_tools::FIND_REFERENCES_DESC)]
    pub(crate) fn find_references(
        &self,
        Parameters(mut params): Parameters<crate::precise_tools::FindReferencesParams>,
    ) -> CallToolResult {
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            params.path.as_deref(),
            None,
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        params.path = route.path;
        // Inherit path from this session's last unambiguous symbol hit when
        // the model omitted it (common after find_symbol / goto_definition).
        let mut inherit_note: Option<String> = None;
        if params.path.is_none() {
            if let Ok(session) = self.session_for(&route.hint) {
                if let Some(anchor) = self
                    .recent_symbols
                    .recall(&session.root, params.symbol.trim())
                {
                    let line_disp = anchor
                        .line
                        .map(|l| format!(":{}", l + 1))
                        .unwrap_or_default();
                    inherit_note = Some(format!(
                        "No path given; reusing this session's latest location for `{}` {}{}.\n",
                        params.symbol.trim(),
                        anchor.path,
                        line_disp
                    ));
                    if params.line.is_none() {
                        params.line = anchor.line;
                    }
                    params.path = Some(anchor.path);
                }
            }
        }
        self.with_precise(&route.hint, true, move |session| {
            let mut result = self.coord_note_once(crate::precise_tools::run_find_references(
                params,
                session.precise.as_ref(),
            ));
            if let Some(note) = inherit_note.as_deref() {
                result = prepend_tool_note(result, note);
            }
            result
        })
    }

    #[tool(description = crate::precise_tools::GOTO_DEFINITION_DESC)]
    pub(crate) fn goto_definition(
        &self,
        Parameters(mut params): Parameters<crate::precise_tools::GotoDefinitionParams>,
    ) -> CallToolResult {
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            Some(&params.path),
            None,
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        params.path = route.path.unwrap_or_default();
        self.with_precise(&route.hint, true, move |session| {
            // Use site is enough for a later find_references without path
            // (fix_path keeps a file that contains the symbol).
            if let Some(sym) = params
                .symbol
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                self.remember_symbol_anchor(&session.root, sym, &params.path, params.line);
            }
            self.coord_note_once(crate::precise_tools::run_goto_definition(
                params,
                session.precise.as_ref(),
            ))
        })
    }

    #[tool(description = crate::precise_tools::GET_DIAGNOSTICS_DESC)]
    pub(crate) fn get_diagnostics(
        &self,
        Parameters(mut params): Parameters<crate::precise_tools::GetDiagnosticsParams>,
    ) -> CallToolResult {
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            Some(&params.path),
            None,
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        params.path = route.path.unwrap_or_default();
        let toolchain = params.toolchain;
        self.with_precise(&route.hint, false, move |session| {
            crate::precise_tools::run_get_diagnostics_with(
                params,
                session.precise.as_ref(),
                toolchain,
            )
        })
    }

    #[tool(
        description = "Locate symbols by name or hierarchical path; returns signatures, path:line anchors, and definition bodies (include_body defaults true; indexed languages). Put the symbol name (or Class/method) in query; use path_filter for file scope. Outline: query='*' + path_filter. Do not put file paths in query."
    )]
    pub(crate) fn find_symbol(
        &self,
        Parameters(mut params): Parameters<QueryParams>,
    ) -> CallToolResult {
        let signature = format!("{params:?}");
        // The query is a symbol name, never a routing hint.
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            params.path_filter.as_deref(),
            None,
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        params.path_filter = route.path.clone();
        params.root = (!route.hint.is_empty()).then(|| route.hint.clone());
        let result = self.guarded("find_symbol", &route.hint, &signature, || {
            self.find_symbol_inner(params)
        });
        Self::with_route_note(result, &route.note)
    }

    /// Pages of one `find_symbol` query are cut from a ranking recomputed on
    /// every call, so an index replacement between two pages shifts the page
    /// boundaries (items skipped or repeated). Remember the generation each
    /// query was last served at; on `offset > 0` with a different generation,
    /// return a note telling the caller to restart from `offset=0`.
    fn symbol_page_drift_note(&self, params: &QueryParams, generation: u64) -> Option<String> {
        let key = format!(
            "{}|{}|{:?}|{:?}|{:?}|{}",
            params.root.as_deref().unwrap_or(""),
            params.query,
            params.kind,
            params.substring,
            params.path_filter,
            params.depth
        );
        let mut map = self.symbol_page_generations.lock().ok()?;
        if map.len() > 256 {
            map.clear();
        }
        let previous = map.insert(key, generation);
        (params.offset > 0 && previous.is_some_and(|g| g != generation)).then(|| {
            "Note: the index was updated while paging (files changed and reindexed); this page's boundary may misalign with the previous page (missed or duplicate items); for a complete result re-query with offset=0.\n".to_string()
        })
    }

    /// `resolve_context` pages are cut from a ranking recomputed per call; see
    /// `symbol_page_drift_note`.
    fn context_page_drift_note(
        &self,
        params: &ResolveContextParams,
        generation: u64,
    ) -> Option<String> {
        let key = format!(
            "{}|{}|{}",
            params.root.as_deref().unwrap_or(""),
            params.task_description,
            params.path_filter.as_deref().unwrap_or("")
        );
        let mut map = self.context_page_generations.lock().ok()?;
        if map.len() > 256 {
            map.clear();
        }
        let previous = map.insert(key, generation);
        (params.offset > 0 && previous.is_some_and(|g| g != generation)).then(|| {
            "Note: the index was updated while paging; this page's boundary may misalign with the previous page (missed or duplicate items); for a complete result re-query with offset=0.\n".to_string()
        })
    }

    fn find_symbol_inner(&self, params: QueryParams) -> CallToolResult {
        if params.query.trim().is_empty() {
            return text_error(
                "find_symbol needs a non-empty query (symbol name, substring, or 'Class/method' path); a blank query does not return all symbols.".to_string(),
            );
        }
        if looks_like_filesystem_path(&params.query) {
            return text_error(filesystem_path_as_query_error(&params.query));
        }
        self.with_fresh_index(params.root.as_deref().unwrap_or(""), |session, index| {
            if let Some(message) = path_filter_error(index, params.path_filter.as_deref()) {
                return self.finish_at(&session.root, text_error(message));
            }
            let kind = match params.kind.as_deref() {
                None => None,
                Some(name) => match kind_from_name(name) {
                    Ok(kind) => Some(kind),
                    Err(message) => return self.finish_at(&session.root, text_error(message)),
                },
            };
            // `*` + path_filter: file outline (file paths in query are rejected above).
            if let Some(target) =
                symbol_outline::outline_target(index, &params.query, params.path_filter.as_deref())
            {
                return match target {
                    Ok(target) => {
                        self.find_symbol_outline(&session.root, index, &params, kind, target)
                    }
                    Err(message) => self.finish_at(&session.root, text_error(message)),
                };
            }
            let normalized = normalize_symbol_query(&params.query);
            // "Class/method"（及 Type.method / A::b / pkg.Func）路径查询走层级
            // 匹配器（Serena NamePathMatcher 对应物）。
            // A symbol literally named `a::b` / `a.b` (shell `bats::on_failure`,
            // Ruby `self.x`) wins over the hierarchical path reading.
            let literal = params.query.trim();
            let literal_hit = normalized.query != literal
                && !literal.contains('/')
                && index.names().has_exact(&index.graph().symbols, literal);
            if normalized.query.contains('/') && !literal_hit {
                let mut params = params.clone();
                params.query = normalized.query;
                return self.find_symbol_by_path(
                    &session.root,
                    index,
                    &params,
                    normalized.ignored.as_deref(),
                );
            }
            let needle = if literal_hit {
                literal.to_lowercase()
            } else {
                normalized.query.to_lowercase()
            };
            // 命中对先收集 (锚点行, symbol 下标)，与展示列表同一排序源——
            // include_body 只对"用户实际看到的" kept 前几个附体（审查 major：
            // 原实现按 graph 构建序取前 5，可能对应被 budget 裁掉的符号）。
            // 排序：精确名 > 前缀 > 子串，同档再按锚点行——避免真正的定义
            // 因按路径排序排在后面而被 budget 截掉。
            let exact_only = params.substring == Some(false);
            // The query as typed (case kept): an exact-case name ranks first.
            let typed = if literal_hit {
                literal
            } else {
                normalized.query.as_str()
            };
            // 排序键：(档, 大小写不同, 类型优先, 测试路径靠后, exported 优先,
            // path, start_line)；末位 idx 保证全序。
            type RankKey<'a> = (u8, bool, u8, bool, bool, &'a str, u32);
            let names = index.names();
            let symbols = &index.graph().symbols;
            let mut ranked: Vec<(RankKey, usize)> = Vec::new();
            let mut test_path_cache: HashMap<FileId, bool> = HashMap::new();
            let mut consider = |idx: usize, tier: u8| {
                let symbol = &symbols[idx];
                if kind.is_some_and(|k| symbol.kind != k) {
                    return;
                }
                if let Some(file) = index.file(symbol.file) {
                    if params
                        .path_filter
                        .as_deref()
                        .is_some_and(|f| !file.path.as_str().contains(f))
                    {
                        return;
                    }
                    let is_test = *test_path_cache
                        .entry(symbol.file)
                        .or_insert_with(|| is_test_path(file.path.as_str()));
                    let key = (
                        tier,
                        !symbol.name.contains(typed),
                        search_shape::kind_rank(symbol.kind),
                        is_test,
                        !symbol.exported,
                        file.path.as_str(),
                        symbol.start_line,
                    );
                    ranked.push((key, idx));
                }
            };
            if exact_only {
                // Exact names only: O(1) map lookup instead of a full scan.
                for &idx in names.by_lower(&needle) {
                    consider(idx as usize, 0);
                }
            } else {
                for (idx, lower) in names.lower().iter().enumerate() {
                    if let Some(tier) = name_match_tier(lower, &needle) {
                        consider(idx, tier);
                    }
                }
            }
            ranked.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
            // An exact name exists: list it, count the prefix/substring rest
            // in one line (`ld` → 1 row, not 26) unless substring=true.
            let exact_count = ranked.iter().take_while(|(k, _)| k.0 == 0).count();
            let folded: Vec<usize> =
                if params.substring.is_none() && exact_count > 0 && exact_count < ranked.len() {
                    ranked
                        .split_off(exact_count)
                        .into_iter()
                        .map(|(_, i)| i)
                        .collect()
                } else {
                    Vec::new()
                };
            let hits: Vec<(String, usize)> = ranked
                .into_iter()
                .map(|(_, idx)| {
                    let symbol = &symbols[idx];
                    let path = &index
                        .file(symbol.file)
                        .expect("ranked symbol has a file")
                        .path;
                    (symbol_line(symbol, path), idx)
                })
                .collect();
            // One exact hit → later find_references can omit path.
            self.remember_if_unique_symbol_hit(&session.root, typed, index, &hits);
            let total = hits.len();
            let offset = params.offset.min(total);
            let mut body = if self.once("find_symbol.confidence") {
                "confidence: syntactic (symbol-name match)\n".to_string()
            } else {
                "confidence: syntactic\n".to_string()
            };
            let generation = index.generation();
            if let Some(note) = self.symbol_page_drift_note(&params, generation) {
                body.push_str(&note);
            }
            if let Some(ignored) = &normalized.ignored {
                body.push_str(&format!("Note: ignored keywords in the query: {ignored}\n"));
            }
            let folded_note = search_shape::folded_note(symbols, &folded, typed);
            // Go interfaces among the hits: their implementing types (implicit
            // satisfaction is invisible to a name search). Budget is reserved
            // up front so a truncated page still carries the line.
            let implementors = search_shape::implementors_block(
                index,
                hits[offset..].iter().map(|(_, idx)| *idx),
                params.budget_tokens / 3,
            );
            // 头部与状态行也计入预算：按"最长的截断态状态行"预留。
            let reserve = estimate_tokens(&body)
                + estimate_tokens(&folded_note)
                + estimate_tokens(&implementors)
                + estimate_tokens(&render_search_page_status(
                    total,
                    offset,
                    total - offset,
                    1,
                    params.budget_tokens,
                    params.offset,
                ));
            let limit = params.budget_tokens.saturating_sub(reserve);
            let (kept, omitted) = if params.depth > 0 {
                symbol_outline::paged_lines_with_children(index, &hits, offset, params.depth, limit)
            } else {
                paged_symbol_lines(index, &hits, offset, limit)
            };
            let kept_count = kept.len();
            let truncated = omitted > 0;
            // 一条都放不下时 next_offset 不能等于当前 offset（自动翻页会死循环）。
            let next_offset = (truncated && kept_count > 0).then_some(offset + kept_count);
            let narrow = truncated
                .then(|| search_shape::suggest_symbol_filter(index, &hits[offset + kept_count..]))
                .flatten();
            body.push_str(&render_search_page_status_narrow(
                total,
                offset,
                kept_count,
                omitted,
                params.budget_tokens,
                params.offset,
                narrow.as_deref(),
            ));
            for line in &kept {
                body.push_str(line);
                body.push('\n');
            }
            body.push_str(&implementors);
            body.push_str(&folded_note);
            // 零命中时显式声明（对齐 find_references 空结果约定），并跳过
            // include_body 附加，避免误导性的"无可提取的符号体"。
            if hits.is_empty() {
                body.push_str("(no matching symbols)\n");
                if let Some(filter) = params.path_filter.as_deref() {
                    body.push_str(&search_shape::symbol_hits_outside_filter(
                        index, &needle, kind, filter,
                    ));
                }
                let left = params.budget_tokens.saturating_sub(estimate_tokens(&body));
                match symbol_outline::word_fallback_block(
                    index,
                    &normalized.query,
                    kind,
                    params.path_filter.as_deref(),
                    left,
                ) {
                    Some(block) => body.push_str(&block),
                    None => body.push_str(&symbol_suggestions(index, &needle)),
                }
                let unindexed = index.unindexed_code_exts(None);
                if let Some(hint) = format_find_symbol_unindexed_hint(&unindexed) {
                    body.push_str(&hint);
                }
            }
            if params.include_body && !hits.is_empty() {
                self.attach_include_bodies(
                    &session.root,
                    index,
                    &mut body,
                    &hits[offset..],
                    kept_count,
                    params.budget_tokens,
                );
            }
            self.result_on(
                &session.root,
                "find_symbol",
                body,
                json!({
                    "confidence": "syntactic",
                    "generation": generation,
                    "matches": total, "total": total, "offset": offset, "next_offset": next_offset,
                    "shown": kept_count,
                    "omitted": omitted,
                    "truncated": truncated,
                    "budget_tokens": params.budget_tokens
                }),
                Some(params.budget_tokens),
            )
        })
    }

    #[tool(
        description = "Exhaustive code & text search: literal, case-sensitive only if query has uppercase (regex=true for regex); hits come with context, wide hits grouped per file; other text files tagged [text-only]. count: aggregate=top|dir|file gives matching lines / files per dir + total; add skip_comments, exclude=[\"_test.go\",\"vendor\"]."
    )]
    pub(crate) fn search_code(
        &self,
        Parameters(mut params): Parameters<SearchParams>,
    ) -> CallToolResult {
        let signature = format!("{params:?}");
        // The query is a search string, never a routing hint.
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            params.path_filter.as_deref(),
            None,
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        params.path_filter = route.path.clone();
        params.root = (!route.hint.is_empty()).then(|| route.hint.clone());
        if let Some(building) = self.building_session(&route.hint) {
            let result = self.search_while_building(&building, &params);
            return Self::with_route_note(result, &route.note);
        }
        let result = self.guarded("search_code", &route.hint, &signature, || {
            self.search_code_inner(params)
        });
        Self::with_route_note(result, &route.note)
    }

    fn search_code_inner(&self, mut params: SearchParams) -> CallToolResult {
        let regex_downgraded = apply_code_fragment_literal(&mut params);
        self.with_fresh_index(
            params.root.as_deref().unwrap_or(""),
            |session, index| {
                // A filter that only hits non-code text files (docs/, *.toml …)
                // is valid here: those files are searched as [text-only].
                // Computed once per call; the path_filter view, the text-only
                // scan and `data_hits_outside` all derive from it.
                let all_text_paths = index.text_only_paths(None);
                let filter_text_paths: Vec<&RelPath> = all_text_paths
                    .iter()
                    .copied()
                    .filter(|p| {
                        params
                            .path_filter
                            .as_deref()
                            .is_none_or(|f| p.as_str().contains(f))
                    })
                    .collect();
                if filter_text_paths.is_empty() {
                    if let Some(message) = path_filter_error(index, params.path_filter.as_deref()) {
                        return text_error(message);
                    }
                }
                // Smart case: a literal with an uppercase letter matches its
                // case exactly; case-only variants are counted in a footnote.
                let sensitive = search_shape::literal_case_sensitive(
                    &params.query,
                    params.regex,
                    params.case_sensitive,
                );
                let variant_matcher = if search_shape::smart_case_applied(
                    &params.query,
                    params.regex,
                    params.case_sensitive,
                ) {
                    build_search_matcher(&params.query, false, Some(false)).ok()
                } else {
                    None
                };
                let matcher = match build_search_matcher(
                    &params.query,
                    params.regex,
                    Some(sensitive),
                ) {
                    Ok(matcher) => matcher,
                    Err(error) => return text_error(invalid_regex_message(error)),
                };
                // Whole-buffer candidate lines (a superset of the lines the
                // per-line matchers accept; see `line_scan`). With smart case
                // the case-insensitive variant covers both matchers.
                let line_scanner = if variant_matcher.is_some() {
                    crate::line_scan::LineScanner::new(&regex::escape(&params.query), true)
                } else if params.regex {
                    crate::line_scan::LineScanner::new(&params.query, !sensitive)
                } else {
                    crate::line_scan::LineScanner::new(&regex::escape(&params.query), !sensitive)
                };
                if let Some(mode) = params.aggregate.as_deref() {
                    if mode != "top" && mode != "dir" && mode != "file" {
                        return text_error(format!(
                            "aggregate must be top, dir, or file; got `{mode}`."
                        ));
                    }
                }
                if params.query.is_empty() {
                    return text_error(
                        "query cannot be empty (empty string matches every line). Provide a literal to search; to match anything pass regex=true with an explicit pattern.",
                    );
                }
                // The text-only path uses grep-regex, which rejects patterns
                // that can match a line terminator. Surface that instead of
                // silently reporting "no match".
                let text_matcher = match astrolabe_core::text_search::build_matcher(
                    &params.query,
                    params.regex,
                    sensitive,
                ) {
                    Ok(m) => m,
                    Err(error) => {
                        return text_error(format!(
                            "Query cannot match per line: {error}. search_code searches line by line and does not support patterns with newlines (\\n, literal newlines) or cross-line matches; split into single-line queries."
                        ))
                    }
                };
                let exclude_set = match build_exclude_set(params.exclude.as_deref()) {
                    Ok(set) => set,
                    Err(message) => return text_error(message),
                };
                let excluded = |path: &str| exclude_set.as_ref().is_some_and(|set| set.is_match(path));
                let include_set = match build_include_set(params.glob.as_deref()) {
                    Ok(set) => set,
                    Err(message) => return text_error(message),
                };
                let included =
                    |path: &str| include_set.as_ref().is_none_or(|set| set.is_match(path));
                let conf = if self.once("search_code.confidence") {
                    "confidence: exact (on-disk source literal match)\n"
                } else {
                    "confidence: exact\n"
                };
                let mut dedup_key: Option<String> = None;
                // Ordinal taken by this search call (set when session dedup is on).
                let mut search_call: Option<u64> = None;
                let counting = params.count_only || params.aggregate.is_some();
                // count_only/aggregate replies are short tables: always resend
                // them in full (a pointer would hide the table the model needs).
                if session_dedup_enabled(params.dedup) && !counting {
                    let mut probe = params.clone();
                    probe.dedup = None;
                    let key = format!("{}|search|{probe:?}", session.root.display());
                    let generation = index.generation();
                    // A real, monotonically increasing tool-call ordinal.
                    let call = self.call_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    search_call = Some(call);
                    dedup_key = Some(key.clone());
                    let earlier = {
                        let mut map = self.sent_ranges.lock().unwrap_or_else(|e| e.into_inner());
                        match map.get(&key) {
                            Some((g, c)) if *g == generation => Some(*c),
                            _ => {
                                map.insert(key.clone(), (generation, call));
                                None
                            }
                        }
                    };
                    if let Some(c) = earlier {
                        let digest = self
                            .search_digests
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&key)
                            .cloned()
                            .map(|d| format!("First-result digest: {d}\n"))
                            .unwrap_or_default();
                        let body = format!(
                            "{conf}{}Same query already returned on tool call #{c} this session (files unchanged); not resending full text.\n{digest}If you did not see that result (e.g. subtask or compressed/lost context), pass dedup=false to refetch.\n",
                            render_search_mode_header(params.regex, &params.query, regex_downgraded)
                        );
                        return self.result_on(
                            &session.root,
                            "search_code",
                            body,
                            json!({"confidence":"exact","deduplicated":true,"earlier_call":c,"budget_tokens":params.budget_tokens}),
                            Some(params.budget_tokens),
                        );
                    }
                }
                let mut matches: Vec<SearchHit> = Vec::new();
                let mut variants: Vec<SearchHit> = Vec::new();
                let mut hits = 0u64;
                let mut misses = 0u64;
                let mut skipped = 0usize;
                let strings_are_code = params.query.contains(['"', '\'', '`']);
                // skip_comments on a type without a lexer cannot filter anything;
                // collect those types so the summary says so.
                let mut unlexed: BTreeSet<String> = BTreeSet::new();
                struct FileScan {
                    hits: Vec<SearchHit>,
                    cache_hit: bool,
                    skipped: usize,
                    unlexed: Option<String>,
                    /// Lines matching only when case is ignored (smart case).
                    variants: Vec<SearchHit>,
                }
                // Per-file scan: pure (no shared mutable state), so files are
                // scanned in parallel; results come back in graph order, which
                // keeps the output identical to a sequential scan.
                let cancel_flag = cancel::current();
                let scan_file = |path: &str| -> Option<FileScan> {
                    if cancel_flag
                        .as_ref()
                        .is_some_and(|f| f.load(Ordering::Relaxed))
                    {
                        return None;
                    }
                    let (lines, cache_hit) = index.cached_lines(&session.file_cache, path)?;
                    let mut out = FileScan {
                        hits: Vec::new(),
                        cache_hit,
                        skipped: 0,
                        unlexed: None,
                        variants: Vec::new(),
                    };
                    // Lazy: the mask is only computed once a line matches.
                    let mut mask: Option<Option<Vec<Vec<bool>>>> = None;
                    for line_no in line_scanner.candidates(&lines) {
                        let line = &lines[line_no];
                        if !matcher.is_match(line) {
                            if variant_matcher.as_ref().is_some_and(|v| v.is_match(line)) {
                                let text = if out.variants.is_empty() {
                                    line.trim().to_string()
                                } else {
                                    String::new()
                                };
                                out.variants.push(SearchHit {
                                    path: path.to_string(),
                                    line: line_no + 1,
                                    text,
                                    text_only: false,
                                });
                            }
                            continue;
                        }
                        if params.skip_comments {
                            let mask = mask.get_or_insert_with(|| {
                                crate::code_mask::code_mask(path, &lines, strings_are_code)
                            });
                            if let Some(mask) = mask {
                                let real = matcher
                                    .find_iter(line)
                                    .any(|m| mask[line_no].get(m.start()).copied().unwrap_or(true));
                                if !real {
                                    out.skipped += 1;
                                    continue;
                                }
                            } else {
                                out.unlexed = Some(crate::code_mask::type_label(path));
                            }
                        }
                        // P0:超长行先做单行安全切片(Unicode 边界安全)，
                        // 单条 match 不会把 budget 一口气吃光，也不会在
                        // 大小写折叠差异上 panic.
                        out.hits.push(SearchHit {
                            path: path.to_string(),
                            line: line_no + 1,
                            text: if counting {
                                String::new()
                            } else {
                                clip_match_line(line, &params.query, Some(&matcher))
                            },
                            text_only: false,
                        });
                    }
                    Some(out)
                };
                let candidates: Vec<&str> = index
                    .graph()
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .filter(|path| {
                        params
                            .path_filter
                            .as_ref()
                            .is_none_or(|filter| path.contains(filter.as_str()))
                            && !excluded(path)
                            && included(path)
                    })
                    .collect();
                {
                    use rayon::prelude::*;
                    let scanned: Vec<Option<FileScan>> =
                        candidates.par_iter().map(|path| scan_file(path)).collect();
                    for scan in scanned.into_iter().flatten() {
                        variants.extend(scan.variants);
                        if scan.cache_hit {
                            hits += 1;
                        } else {
                            misses += 1;
                        }
                        skipped += scan.skipped;
                        unlexed.extend(scan.unlexed);
                        matches.extend(scan.hits);
                    }
                }
                // 非索引文本文件(md/sql/yaml/json/xml/gradle/kt/模板…)也搜，
                // 排在代码命中之后并标注 [text-only]:它们没有符号/引用信息.
                // ripgrep 引擎(grep-searcher)并行搜索，按输入顺序回收结果.
                let code_matches = matches.len();
                let text_paths: Vec<&RelPath> = filter_text_paths
                    .iter()
                    .copied()
                    .filter(|p| !excluded(p.as_str()) && included(p.as_str()))
                    .collect();
                {
                    let rels: Vec<&str> = text_paths.iter().map(|p| p.as_str()).collect();
                    let found = astrolabe_core::text_search::search_files_opts(
                        &session.root,
                        &rels,
                        &text_matcher,
                        TEXT_SEARCH_MAX_FILE_BYTES,
                        !counting,
                    );
                    for (idx, file_hits) in found {
                        let mut file_hits = file_hits;
                        if params.skip_comments {
                            if is_doc_path(rels[idx]) {
                                skipped += file_hits.len();
                                continue;
                            }
                            if crate::code_mask::has_lexer(rels[idx]) {
                                let text = std::fs::read(session.root.join(rels[idx]))
                                    .map(|raw| String::from_utf8_lossy(&raw).into_owned())
                                    .unwrap_or_default();
                                let lines: Vec<String> =
                                    text.lines().map(str::to_string).collect();
                                if let Some(mask) = crate::code_mask::code_mask(
                                    rels[idx],
                                    &lines,
                                    strings_are_code,
                                ) {
                                    let before = file_hits.len();
                                    file_hits.retain(|hit| {
                                        let l = (hit.line as usize).saturating_sub(1);
                                        match (lines.get(l), mask.get(l)) {
                                            (Some(line), Some(m)) => {
                                                matcher.find_iter(line).any(|f| {
                                                    m.get(f.start()).copied().unwrap_or(true)
                                                })
                                            }
                                            _ => true,
                                        }
                                    });
                                    skipped += before - file_hits.len();
                                }
                            } else if !file_hits.is_empty() {
                                unlexed.insert(crate::code_mask::type_label(rels[idx]));
                            }
                        }
                        for hit in file_hits {
                            matches.push(SearchHit {
                                path: rels[idx].to_string(),
                                line: hit.line as usize,
                                text: if counting {
                                    String::new()
                                } else {
                                    clip_match_line(&hit.text, &params.query, Some(&matcher))
                                },
                                text_only: true,
                            });
                        }
                    }
                }
                let text_matches = matches.len() - code_matches;
                let variant_note = search_shape::case_variant_note(&variants);
                let skip_note = if params.skip_comments {
                    let mut note = format!("skip_comments: skipped {skipped} hits in comments/strings/docs; they are not listed below.\n");
                    if !unlexed.is_empty() {
                        let kinds: Vec<&str> = unlexed.iter().map(String::as_str).collect();
                        note.push_str(&format!(
                            "Note: {}-type comments not skipped (no lexer); hits may still include comment/string occurrences.\n",
                            kinds.join(", ")
                        ));
                    }
                    note
                } else {
                    String::new()
                };
                let data_note = if params.regex {
                    String::new()
                } else {
                    params
                        .path_filter
                        .as_deref()
                        .map(|filter| data_hits_outside(
                            &all_text_paths,
                            &session.root,
                            filter,
                            &params.query,
                            sensitive,
                            &excluded,
                        ))
                        .unwrap_or_default()
                };
                session.cache_hits.fetch_add(hits, Ordering::Relaxed);
                session.cache_misses.fetch_add(misses, Ordering::Relaxed);
                tracing::debug!(
                    hits,
                    misses,
                    weighted_size = session.file_cache.weighted_size(),
                    budget_bytes = session.cache_budget_bytes,
                    "search_code file cache"
                );
                // 头部(confidence/mode/截断状态)约 35 tokens 的固定开销先
                // 扣除，保证整个 body 不会在阈值边界处溢出.
                let clamped_budget = clamp_budget(params.budget_tokens);
                let remaining = clamped_budget.saturating_sub(SEARCH_HEADER_TOKENS);
                let total = matches.len();
                let offset = params.offset.min(total);
                if let Some(key) = &dedup_key {
                    if total > 0 {
                        let digest = search_digest(index, &matches);
                        self.search_digests
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key.clone(), digest);
                    }
                }
                if counting {
                    if search_call.is_none() {
                        self.call_seq.fetch_add(1, Ordering::Relaxed);
                    }
                    let body = format!(
                        "{conf}{}{}",
                        render_search_mode_header(params.regex, &params.query, regex_downgraded),
                        render_search_counts(&matches, params.aggregate.as_deref(), params.offset),
                    ) + &variant_note
                        + &skip_note;
                    return self.result_on(
                        &session.root,
                        "search_code",
                        body,
                        json!({
                            "confidence": "exact",
                            "counts_only": true,
                            "total": total,
                            "files": matches.iter().map(|m| m.path.as_str()).collect::<BTreeSet<_>>().len(),
                            "complete": true,
                            "budget_tokens": params.budget_tokens,
                        }),
                        Some(params.budget_tokens),
                    );
                }
                let auto_group = params.group.is_none()
                    && params.context_lines.is_none()
                    && !params.page
                    && params.offset == 0
                    && total > smart_body_max_hits()
                    && total <= broad_hits()
                    && matches
                        .iter()
                        .map(|m| m.path.as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        > 3;
                // Grouped output is re-ordered per file, so it is not a prefix of
                // the hit list and cannot be paged by `offset`; with offset > 0
                // fall through to ordinary pagination.
                if ((params.group == Some(true) && params.offset == 0) || auto_group) && total > 0 {
                    let (grouped, shown, folded_tests) = render_grouped_hits(
                        index,
                        &session.file_cache,
                        &matches,
                        clamped_budget.saturating_sub(SEARCH_HEADER_TOKENS),
                        self.once("search_code.group_legend"),
                    );
                    let mut body = format!(
                        "{conf}{}",
                        render_search_mode_header(params.regex, &params.query, regex_downgraded)
                    );
                    body.push_str(&format!(
                        "grouped=true total={total} shown={shown}\n"
                    ));
                    body.push_str(&grouped);
                    body.push_str(&grouped_closure_line(&matches, shown, folded_tests));
                    body.push_str(&variant_note);
                    body.push_str(&self.many_hits_once(total));
                    body.push_str(&skip_note);
                    body.push_str(&data_note);
                    return self.result_on(
                        &session.root,
                        "search_code",
                        body,
                        json!({
                            "confidence": "exact",
                            "grouped": true,
                            "total": total,
                            "shown": shown,
                            "budget_tokens": params.budget_tokens,
                        }),
                        Some(params.budget_tokens),
                    );
                }
                let broad = total > broad_hits()
                    && params.offset == 0
                    && !params.page
                    && params.context_lines.is_none();
                if broad {
                    let mut body = format!(
                        "{conf}{}",
                        render_search_mode_header(params.regex, &params.query, regex_downgraded)
                    );
                    let ranked = rank_broad_hits(index, &matches);
                    let preview_budget = clamped_budget.saturating_sub(SEARCH_HEADER_TOKENS) / 2;
                    let mut preview = String::new();
                    let mut shown_preview = 0;
                    for hit in ranked.iter().take(BROAD_PREVIEW_HITS) {
                        let line = format!("@{}:{} {}\n", hit.path, hit.line, hit.text);
                        if estimate_tokens(&preview) + estimate_tokens(&line) > preview_budget {
                            break;
                        }
                        preview.push_str(&line);
                        shown_preview += 1;
                    }
                    body.push_str(&format!(
                        "shown={shown_preview} omitted={} truncated=true total={total} offset=0 summary=true\n",
                        total - shown_preview
                    ));
                    body.push_str(&render_search_summary(&matches, params.path_filter.as_deref()));
                    if shown_preview > 0 {
                        body.push_str(&format!(
                            "Top {shown_preview} hits (def lines first, test files later; not the full set):\n{preview}"
                        ));
                    }
                    body.push_str(&closure_line(&matches, shown_preview, 0, true));
                    body.push_str(&variant_note);
                    body.push_str(&self.many_hits_once(total));
                    body.push_str(&skip_note);
                    body.push_str(&data_note);
                    return self.result_on(
                        &session.root,
                        "search_code",
                        body,
                        json!({
                            "confidence": "exact",
                            "summary": true,
                            "preview_shown": shown_preview,
                            "total": total,
                            "truncated": true,
                            "next_offset": 0,
                            "budget_tokens": params.budget_tokens,
                        }),
                        Some(params.budget_tokens),
                    );
                }
                let context = search_context_lines(params.context_lines, total);
                let mut file_lines = |hit: &SearchHit| -> Option<Arc<Vec<String>>> {
                    if hit.text_only {
                        let text =
                            std::fs::read_to_string(session.root.join(&hit.path)).ok()?;
                        Some(Arc::new(text.lines().map(str::to_string).collect()))
                    } else {
                        index
                            .cached_lines(&session.file_cache, &hit.path)
                            .map(|(lines, _)| lines)
                    }
                };
                let smart = params.context_lines.is_none()
                    && offset == 0
                    && total > 0
                    && total <= smart_body_max_hits();
                let dedup = session_dedup_enabled(params.dedup);
                let generation = index.generation();
                let call = search_call
                    .unwrap_or_else(|| self.call_seq.fetch_add(1, Ordering::Relaxed) + 1);
                let root_key = session.root.display().to_string();
                let mut pending: Vec<(String, u32, u32)> = Vec::new();
                let mut seen = |path: &str, start: u32, end: u32| -> Option<u64> {
                    let key = format!("{root_key}|{path}:{start}-{end}");
                    let earlier = self.lookup_sent(&key, dedup, generation);
                    if earlier.is_none() {
                        pending.push((path.to_string(), start, end));
                    }
                    earlier
                };
                let enclosing = |hit: &SearchHit| enclosing_symbol(index, &hit.path, hit.line);
                let smart_rendered = if smart {
                    render_symbol_hits(
                        &matches,
                        remaining,
                        context,
                        &enclosing,
                        &mut file_lines,
                        &mut seen,
                    )
                } else {
                    None
                };
                // Only ranges whose body actually shipped count as sent (a
                // render that overflowed the budget falls back to line context).
                if smart_rendered.is_some() {
                    for (path, start, end) in &pending {
                        self.commit_sent(
                            &session.root,
                            path,
                            (*start, *end),
                            generation,
                            call,
                        );
                    }
                }
                let bodies = smart_rendered.as_ref().is_some_and(|(_, used)| *used);
                let (rendered, shown) = match smart_rendered {
                    Some((text, _)) => (text, total),
                    None => render_search_hits(&matches[offset..], context, remaining, &mut file_lines),
                };
                let omitted = total - offset - shown;
                let truncated = omitted > 0;
                let next_offset = truncated.then_some(offset + shown);
                let mode = if params.regex { "regex" } else { "literal" };
                let mut body = format!(
                    "{conf}{}",
                    render_search_mode_header(params.regex, &params.query, regex_downgraded)
                );
                body.push_str(&render_search_page_status(
                    total,
                    offset,
                    shown,
                    omitted,
                    clamped_budget,
                    params.offset,
                ));
                if bodies && self.once("search_code.bodies_legend") {
                    body.push_str("Context: whole enclosing function/type given (`── kind name · path:start-end ──`, `  line| source`, hit lines as `@path:line`); no need to Read again.\n");
                } else if !bodies
                    && context > 0
                    && shown > 0
                    && self.once("search_code.context_legend")
                {
                    body.push_str(&format!(
                        "Context: {context} lines around each hit (`  line| source`); overlapping windows in the same file are merged.\n"
                    ));
                }
                body.push_str(&rendered);
                if !matches.is_empty() {
                    body.push_str(&closure_line(&matches, shown, offset, false));
                    body.push_str(&self.many_hits_once(total));
                    body.push_str(&variant_note);
                }
                body.push_str(&skip_note);
                    body.push_str(&data_note);
                // 零命中时显式声明，区分"正常无匹配"与"服务故障/吞内容"
                // (对齐 find_references 的 `未找到对 … 的引用.` 空结果约定).
                let mut unindexed_hint = false;
                if matches.is_empty() {
                    body.push_str("(no matching code lines)\n");
                    body.push_str(&variant_note);
                    let near = if params.regex {
                        Vec::new()
                    } else {
                        closest_symbols(index, &params.query, params.path_filter.as_deref(), 8)
                    };
                    if !near.is_empty() {
                        body.push_str("Closest symbol names (case/prefix/near spellings):\n");
                        for line in &near {
                            body.push_str(&format!("  {line}\n"));
                        }
                    }
                    if let Some(filter) = params.path_filter.as_deref() {
                        // Say what the filter removed instead of "too narrow":
                        // the model re-ran every such query without it.
                        let outside: Vec<SearchHit> = index
                            .graph()
                            .files
                            .iter()
                            .map(|file| file.path.as_str())
                            .filter(|path| !path.contains(filter) && !excluded(path))
                            .filter_map(&scan_file)
                            .flat_map(|scan| scan.hits)
                            .collect();
                        body.push_str(&search_shape::search_hits_outside_filter(filter, &outside));
                    } else if near.is_empty() && variants.is_empty() {
                        body.push_str("Hint: try a shorter literal, or use find_symbol by name.\n");
                    }
                    let counts = index.unindexed_code_exts(params.path_filter.as_deref());
                    if let Some(hint) =
                        format_search_code_unindexed_hint(&counts, params.path_filter.as_deref())
                    {
                        body.push_str(&hint);
                        unindexed_hint = true;
                    }
                }
                self.result_on(
                    &session.root,
                    "search_code",
                    body,
                    json!({
                        "confidence": "exact",
                        "mode": mode,
                        "matches": matches.len(),
                        "total": total,
                        "offset": offset,
                        "next_offset": next_offset,
                        "shown": shown,
                        "omitted": omitted,
                        "truncated": truncated,
                        "budget_tokens": params.budget_tokens,
                        "unindexed_hint": unindexed_hint,
                        "text_only_matches": text_matches,
                        "context_lines": context,
                        "bodies": bodies,
                        "text_only_files_searched": text_paths.len()
                    }),
                    Some(params.budget_tokens),
                )
            },
        )
    }

    /// Earlier call ordinal if this exact range was already returned this
    /// session at the same index generation (and dedup is on).
    fn lookup_sent(&self, key: &str, dedup: bool, generation: u64) -> Option<u64> {
        let map = self.sent_ranges.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            Some((g, c)) if dedup && *g == generation => Some(*c),
            _ => None,
        }
    }

    /// Remember a range as fully returned (dedup table + opt-in served log).
    fn commit_sent(&self, root: &Path, path: &str, range: (u32, u32), generation: u64, call: u64) {
        let key = format!("{}|{path}:{}-{}", root.display(), range.0, range.1);
        self.sent_ranges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (generation, call));
    }

    /// include_body:对展示列表(kept)前 5 个符号附源码体，头带
    /// path:line 锚点区分同名符号；附加体的 token 计入 budget.
    /// 预算不够时按行优雅截断而非整块丢弃:首个符号强制保留最小
    /// 展示空间，杜绝"找到符号却零字节 + 无可提取"的自相矛盾输出.
    #[allow(clippy::too_many_lines)]
    fn attach_include_bodies(
        &self,
        root: &Path,
        index: &RepoIndex,
        body: &mut String,
        hits: &[(String, usize)],
        kept_count: usize,
        budget_tokens: usize,
    ) {
        let mut attached = 0usize;
        let mut failed = 0usize;
        let mut spent = estimate_tokens(body);
        let dedup = session_dedup_enabled(None);
        let generation = index.generation();
        let call = self.call_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let root_key = root.display().to_string();
        for (line, idx) in hits.iter().take(kept_count).take(5) {
            let symbol = &index.graph().symbols[*idx];
            let Some(file) = index.file(symbol.file) else {
                failed += 1;
                continue;
            };
            let key = format!(
                "{root_key}|{}:{}-{}",
                file.path.as_str(),
                symbol.start_line,
                symbol.end_line
            );
            let earlier = self.lookup_sent(&key, dedup, generation);
            if let Some(c) = earlier {
                body.push_str(&format!(
                    "\n----- body @ {line} -----\n(Call #{c} this session already returned this definition body; files unchanged — not resending.)\n"
                ));
                attached += 1;
                continue;
            }
            match astrolabe_core::body::symbol_body(&root.join(file.path.as_str()), symbol) {
                Ok(text) if !text.is_empty() => {
                    let section = format!("\n----- body @ {line} -----\n{text}");
                    let section_tokens = estimate_tokens(&section);
                    if spent + section_tokens > budget_tokens {
                        // 整体放不下:不丢弃，按剩余预算逐行截断.
                        let mut available_tokens = budget_tokens.saturating_sub(spent);
                        if attached == 0 {
                            // 首个符号即使预算紧迫也强制保留展示空间.
                            available_tokens = available_tokens.max(250);
                        }
                        if available_tokens < 150 {
                            // 前面的符号已展示过, 剩余空间过小:省略收尾.
                            body.push_str("\n(include_body: remaining symbol bodies omitted due to budget_tokens)\n");
                            break;
                        }
                        let header = format!("\n----- body @ {line} -----\n");
                        // 截断说明约需 40 tokens，先从预算里扣除.
                        let body_budget = available_tokens
                            .saturating_sub(40)
                            .saturating_sub(estimate_tokens(&header));
                        let mut shown_lines = 0usize;
                        let mut partial = String::new();
                        for src_line in text.lines() {
                            let candidate = format!("{partial}{src_line}\n");
                            if estimate_tokens(&candidate) > body_budget
                                // 首个符号至少给出一行，避免零字节输出.
                                && !(attached == 0 && shown_lines == 0)
                            {
                                break;
                            }
                            partial = candidate;
                            shown_lines += 1;
                        }
                        if shown_lines > 0 {
                            let total_lines = symbol.end_line.saturating_sub(symbol.start_line) + 1;
                            let note = format!(
                                "\n... [symbol has {total_lines} lines total; budget_tokens truncated to first {shown_lines}; use Read(offset, limit) for inner ranges] ...\n"
                            );
                            let truncated = format!("{header}{partial}{note}");
                            body.push_str(&truncated);
                            spent += estimate_tokens(&truncated);
                            attached += 1;
                        }
                        body.push_str("\n(include_body: remaining symbol bodies omitted due to budget_tokens)\n");
                        break;
                    }
                    spent += section_tokens;
                    body.push_str(&section);
                    attached += 1;
                    self.commit_sent(
                        root,
                        file.path.as_str(),
                        (symbol.start_line, symbol.end_line),
                        generation,
                        call,
                    );
                }
                _ => {
                    failed += 1;
                }
            }
        }
        // 只有真正一个符号体都没附加(含截断附加)时才提示无可提取.
        let _ = spent;
        if attached == 0 && failed == 0 {
            body.push_str("\n(include_body: no extractable symbol body)\n");
        }
        if failed > 0 {
            body.push_str(&format!(
                "\n(include_body: {failed} symbol bodies failed to read; fell back to anchors)\n"
            ));
        }
    }

    /// "Class/method" 路径查询分支:core::name_path 层级匹配 + 渲染.
    /// 与名称匹配分支一致地支持 include_body(kept → top5 → symbol_body/truncate).
    fn find_symbol_by_path(
        &self,
        root: &Path,
        index: &RepoIndex,
        params: &QueryParams,
        ignored: Option<&str>,
    ) -> CallToolResult {
        let kind = match params.kind.as_deref() {
            None => None,
            Some(name) => match kind_from_name(name) {
                Ok(kind) => Some(kind),
                Err(message) => return self.finish_at(root, text_error(message)),
            },
        };
        let symbols = &index.graph().symbols;
        let mut matches = astrolabe_core::name_path::match_name_path_with(
            symbols,
            &index.names().name_path,
            &params.query,
            params.substring.unwrap_or(false),
            astrolabe_core::name_path::KindFilter(kind),
            params.depth,
        );
        // 行范围嵌套推不出的层级(Go 方法挂在接收者类型上, pkg.Func 的包是
        // 目录):最后一段精确名 + 父段出现在签名词或路径段中.
        let mut receiver_fallback = false;
        if matches.is_empty() && params.depth == 0 {
            let segments: Vec<&str> = params
                .query
                .split('/')
                .filter(|seg| !seg.is_empty())
                .collect();
            if let [.., parent, last] = segments.as_slice() {
                let mut impl_sources = std::collections::HashMap::new();
                // Equal ignoring ASCII case implies equal lowercase names, so
                // the name table's group is a superset (in symbol order).
                let candidates = index.names().by_lower(&last.to_lowercase());
                for symbol in candidates.iter().map(|&i| &symbols[i as usize]) {
                    if !symbol.name.eq_ignore_ascii_case(last)
                        || kind.is_some_and(|k| symbol.kind != k)
                    {
                        continue;
                    }
                    let Some(file) = index.file(symbol.file) else {
                        continue;
                    };
                    if signature_mentions(&symbol.signature, parent)
                        || path_has_component(file.path.as_str(), parent)
                        || enclosing_impl_mentions(
                            &mut impl_sources,
                            root,
                            file.path.as_str(),
                            symbol.start_line,
                            parent,
                        )
                    {
                        matches.push(astrolabe_core::name_path::NamePathMatch {
                            symbol: symbol.id,
                            depth: 0,
                        });
                    }
                }
                receiver_fallback = !matches.is_empty();
            }
        }
        // 命中对先收集 (锚点行, symbol 下标)，与名称分支同一 include_body 排序源.
        let mut hits: Vec<(String, usize)> = Vec::new();
        for m in &matches {
            if let Some((idx, symbol)) = index
                .names()
                .by_id
                .get(&m.symbol)
                .map(|&idx| (idx, &symbols[idx]))
            {
                if let Some(file) = index.file(symbol.file) {
                    if params
                        .path_filter
                        .as_deref()
                        .is_some_and(|f| !file.path.as_str().contains(f))
                    {
                        continue;
                    }
                    let depth_note = if m.depth > 0 {
                        format!(" (nesting depth d{})", m.depth)
                    } else {
                        String::new()
                    };
                    hits.push((
                        format!("{}{depth_note}", symbol_line(symbol, &file.path)),
                        idx,
                    ));
                }
            }
        }
        // Exact-case last segment first (`Command.Execute`: Execute before
        // execute); stable, so the matcher's order holds otherwise.
        let last_segment = params
            .query
            .rsplit('/')
            .find(|seg| !seg.is_empty())
            .unwrap_or_default();
        hits.sort_by_key(|(_, idx)| symbols[*idx].name != last_segment);
        // Path query that resolves to one leaf: same inherit as name search.
        self.remember_if_unique_symbol_hit(root, last_segment, index, &hits);
        let total = hits.len();
        let offset = params.offset.min(total);
        let mut body = if self.once("find_symbol.path_confidence") {
            "confidence: syntactic (hierarchical path match: nesting inferred from line ranges; syntax containers like impl form intermediate layers)\n"
                .to_string()
        } else {
            "confidence: syntactic\n".to_string()
        };
        let generation = index.generation();
        if let Some(note) = self.symbol_page_drift_note(params, generation) {
            body.push_str(&note);
        }
        if let Some(ignored) = ignored {
            body.push_str(&format!("Note: ignored keywords in the query: {ignored}\n"));
        }
        if receiver_fallback {
            body.push_str("Note: hierarchy inferred from receiver type/signature/path segments (not line-range nesting)\n");
        }
        let implementors = search_shape::implementors_block(
            index,
            hits[offset..].iter().map(|(_, idx)| *idx),
            params.budget_tokens / 3,
        );
        let reserve = estimate_tokens(&body)
            + estimate_tokens(&implementors)
            + estimate_tokens(&render_search_page_status(
                total,
                offset,
                total - offset,
                1,
                params.budget_tokens,
                params.offset,
            ));
        let (kept, omitted) = paged_symbol_lines(
            index,
            &hits,
            offset,
            params.budget_tokens.saturating_sub(reserve),
        );
        let kept_count = kept.len();
        let truncated = omitted > 0;
        // 一条都放不下时 next_offset 不能等于当前 offset(自动翻页会死循环).
        let next_offset = (truncated && kept_count > 0).then_some(offset + kept_count);
        let narrow = truncated
            .then(|| search_shape::suggest_symbol_filter(index, &hits[offset + kept_count..]))
            .flatten();
        body.push_str(&render_search_page_status_narrow(
            total,
            offset,
            kept_count,
            omitted,
            params.budget_tokens,
            params.offset,
            narrow.as_deref(),
        ));
        for line in &kept {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str(&implementors);
        // 零命中时显式声明，避免只剩 confidence 元信息的歧义输出.
        if hits.is_empty() {
            body.push_str("(no matching symbols)\n");
            let last = params
                .query
                .rsplit('/')
                .find(|seg| !seg.is_empty())
                .unwrap_or_default()
                .to_lowercase();
            let left = params.budget_tokens.saturating_sub(estimate_tokens(&body));
            match symbol_outline::word_fallback_block(
                index,
                &params.query.replace('/', " "),
                kind,
                params.path_filter.as_deref(),
                left,
            ) {
                Some(block) => body.push_str(&block),
                None => body.push_str(&symbol_suggestions(index, &last)),
            }
            let unindexed = index.unindexed_code_exts(None);
            if let Some(hint) = format_find_symbol_unindexed_hint(&unindexed) {
                body.push_str(&hint);
            }
        }
        if params.include_body && !hits.is_empty() {
            self.attach_include_bodies(
                root,
                index,
                &mut body,
                &hits[offset..],
                kept_count,
                params.budget_tokens,
            );
        }
        self.result_on(
            root,
            "find_symbol",
            body,
            json!({
                "confidence": "syntactic",
                "generation": generation,
                "matches": total, "total": total, "offset": offset, "next_offset": next_offset,
                "shown": kept_count,
                "omitted": omitted,
                "truncated": truncated,
                "path_query": true,
                "budget_tokens": params.budget_tokens
            }),
            Some(params.budget_tokens),
        )
    }

    #[tool(
        description = "Language distribution, code size, LSP readiness (Ready/needs_install/AST-only), index gaps, and files with syntax errors."
    )]
    pub(crate) fn get_languages(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        let route = match self.resolve_call_root(
            params.index_root.as_deref(),
            params.root.as_deref(),
            None,
            None,
        ) {
            Ok(route) => route,
            Err(err) => return err,
        };
        let result = self.get_languages_inner(params, &route.hint);
        Self::with_route_note(result, &route.note)
    }

    fn get_languages_inner(&self, params: BudgetOnly, hint: &str) -> CallToolResult {
        self.with_index_without_coverage_note(hint, |session, index| {
            let mut totals: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
            for file in &index.graph().files {
                if let Some(language) = file.language {
                    let total = totals.entry(language.name()).or_default();
                    total.0 += 1;
                    total.1 += u64::from(file.loc);
                }
            }
            // Host pool (`lsp_fallback=grok`) wins when the tool layer handed
            // us a non-empty registration list; PATH probe stays for the
            // astrolabe fallback and for languages the host does not cover.
            let host_by_lang: BTreeMap<&str, &HostLspServer> = params
                .host_lsp
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .flat_map(|s| s.languages.iter().map(move |lang| (lang.as_str(), s)))
                .collect();
            let host_section = params.host_lsp.as_ref().map(|servers| {
                let mut lines: Vec<String> = servers
                    .iter()
                    .map(|s| {
                        let state = if s.running { "running" } else { "configured" };
                        let langs = if s.languages.is_empty() {
                            "(no extension map)".to_string()
                        } else {
                            s.languages.join(", ")
                        };
                        format!("  {} [{}] {} — {langs}", s.name, s.command, state)
                    })
                    .collect();
                lines.sort();
                format!(
                    "Host LSP servers (from lsp.json / plugins):\n{}\n",
                    lines.join("\n")
                )
            });
            let installer = crate::ensure_ls::default_installer();
            let status_by_name: std::collections::BTreeMap<&str, _> = installer
                .probed_statuses()
                .into_iter()
                .map(|(lang, status)| (lang.name(), status))
                .collect();
            let mut rows: Vec<_> = totals
                .into_iter()
                .map(|(language, (files, loc))| {
                    let suffix = match host_by_lang.get(language) {
                        Some(host) if host.running => {
                            format!(" — Ready (host: {})", host.name)
                        }
                        Some(host) => {
                            format!(
                                " — configured (host: {}; not running yet)",
                                host.name
                            )
                        }
                        None if params.host_lsp.is_some() => {
                            match status_by_name.get(language) {
                                Some(s) => format!("{} (astrolabe PATH probe)", s.summary_suffix()),
                                None => {
                                    " — AST-only (no host LSP; language server probe not wired)"
                                        .to_string()
                                }
                            }
                        }
                        None => status_by_name
                            .get(language)
                            .map(|s| s.summary_suffix())
                            .unwrap_or_else(|| {
                                " — AST-only (language server probe not wired)".to_string()
                            }),
                    };
                    format!("{language}: {files} files, {loc} LOC{suffix}")
                })
                .collect();
            rows.sort_by(|a, b| b.cmp(a));
            let (kept, omitted) =
                truncate_ranked(&rows, params.budget_tokens, |row| format!("{row}\n"));
            let cache_bytes = session.file_cache.weighted_size();
            let cache_hits = session.cache_hits.load(Ordering::Relaxed);
            let cache_misses = session.cache_misses.load(Ordering::Relaxed);
            tracing::debug!(
                weighted_size = cache_bytes,
                budget_bytes = session.cache_budget_bytes,
                cache_hits,
                cache_misses,
                "file cache occupancy"
            );
            let status_legend = if params.host_lsp.is_some() {
                "LSP status: Ready = host language server running; \
                 configured = registered in lsp.json but not running yet; \
                 (astrolabe PATH probe) = not covered by the host pool; \
                 AST-only = parse/index only (server not wired).\n\
                 If a precise tool reports Unavailable, start the host language server or install the named server locally.\n"
            } else {
                "LSP status: Ready = server on PATH; needs_install = language server missing; \
                 AST-only = parse/index only (server not wired).\n\
                 If a precise tool reports Unavailable, install the named language server locally.\n"
            };
            let mut body = format!(
                "supported languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart, csharp, visualbasic, kotlin, shell, ruby, lua\n\
                 confidence: exact (from scanned file metadata + LSP discovery)\n\
                 file_cache: {cache_bytes}/{} bytes, hits={cache_hits}, misses={cache_misses}\n\
                 {status_legend}",
                session.cache_budget_bytes
            );
            body.push_str(
                "index-first precise answers (no language server): \
                 goto_definition = go; find_references = go (any name), \
                 typescript/tsx/python (unique top-level names); other languages need the host server.\n",
            );
            if let Some(section) = host_section {
                body.push_str(&section);
            }
            for row in kept {
                body.push_str(row);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} languages omitted due to budget_tokens.\n"));
            }
            body.push_str(&index.coverage_detail());
            let unindexed = index.unindexed_code_exts(None);
            let excluded = index.excluded_source_paths();
            if !excluded.is_empty() {
                body.push_str(&format!(
                    "build-output-excluded source files: {} (excluded by build-output rules; invisible to search_code/find_symbol — use Grep/Read)\n",
                    excluded.len()
                ));
            }
            if unindexed.is_empty() {
                body.push_str("unsupported-language source files: none\n");
            } else {
                body.push_str(&format!(
                    "unsupported-language source files by type: {} (outside the symbol/import index; invisible to search_code/find_symbol — use Grep/Read)\n",
                    crate::coverage::unsupported_by_type(&unindexed)
                ));
            }
            self.result_on(
                &session.root,
                "get_languages",
                body,
                json!({
                    "confidence":"exact",
                    "budget_tokens":params.budget_tokens,
                    "file_cache_bytes": cache_bytes,
                    "file_cache_budget_bytes": self.cache_budget_bytes,
                    "file_cache_hits": cache_hits,
                    "file_cache_misses": cache_misses,
                }),
                Some(params.budget_tokens),
            )
        })
    }
}

#[tool_handler]
impl ServerHandler for AstrolabeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
        .with_server_info(Implementation::new("astrolabe", env!("CARGO_PKG_VERSION")))
        .with_instructions(crate::instructions::connection_instructions(
            &self.context.name,
            &self.root,
        ))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2026_07_28))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // `serve_directly` skips rmcp's handshake wrapper, which would otherwise
        // overwrite peer_info with the *negotiated* version. Do that here so a
        // client that still sends `initialize` (legacy 2025-11-25 etc.) is
        // recorded as speaking the version we actually agreed, not the one it
        // asked for.
        let result = self.negotiate_initialize(&request)?;
        let mut peer_info = request;
        peer_info.protocol_version = result.protocol_version.clone();
        context.peer.set_peer_info(peer_info);
        Ok(result)
    }

    /// Run the (synchronous) tool body on a blocking thread so a heavy query
    /// never occupies an async worker: `ping`, `tools/list` and cancellation
    /// stay responsive on 1-2 core hosts. A per-call timeout and the request's
    /// cancellation token end the *wait*; the body observes the same flag via
    /// [`cancel::is_cancelled`] at its checkpoints.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.shared.is_some() {
            let ct = context.ct.clone();
            let forwarded = self
                .forward_call(
                    &request.name,
                    request.arguments.clone(),
                    Box::new(move || ct.is_cancelled()),
                )
                .await;
            if let Some(done) = forwarded {
                return done;
            }
        }
        let ct = context.ct.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = Arc::clone(&flag);
        let server = self.clone();
        let tool_name = request.name.to_string();
        let runtime = tokio::runtime::Handle::current();
        let work = tokio::task::spawn_blocking({
            let tool_name = tool_name.clone();
            move || {
                cancel::scope(worker_flag, || {
                    // Stale clients still call explain_symbol; alias without listing it.
                    if tool_name == crate::compat::EXPLAIN_SYMBOL {
                        return Ok(server.explain_symbol_compat(request.arguments).into());
                    }
                    runtime.block_on(async {
                        let tcc = ToolCallContext::new(&server, request, context);
                        server.tool_router.call(tcc).await
                    })
                })
            }
        });
        let timeout = tool_timeout_from_env();
        let deadline = async {
            match timeout {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            joined = work => match joined {
                Ok(result) => result,
                Err(error) => {
                    tracing::error!(tool = %tool_name, %error, "tool body panicked");
                    Ok(self
                        .finish(text_error(format!("{tool_name} internal error (logged). confidence: unknown")))
                        .into())
                }
            },
            () = ct.cancelled() => {
                flag.store(true, Ordering::Relaxed);
                Err(ErrorData::internal_error("request cancelled", None))
            }
            () = deadline => {
                flag.store(true, Ordering::Relaxed);
                let secs = timeout.map(|d| d.as_secs()).unwrap_or_default();
                Ok(self
                    .finish(text_error(format!(
                        "{tool_name} did not finish within {secs}s; gave up waiting. Narrow the scope (path_filter / aggregate / count_only) and retry;\
                         adjust ASTROLABE_TOOL_TIMEOUT_SECS if needed. confidence: unknown"
                    )))
                    .into())
            }
        }
    }

    /// rmcp routes a request whose `params` do not fit the typed schema to
    /// this hook. For a standard method that is a client error (-32602), not
    /// "method not found".
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        Err(custom_request_error(&request.method))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.listed_tools();
        Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(TOOL_CATALOG_TTL_MS)
            .with_cache_scope(CacheScope::Public))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if self.shared.is_some() {
            let ct = context.ct.clone();
            let forwarded = self
                .forward_read(&request.uri, Box::new(move || ct.is_cancelled()))
                .await;
            if let Some(done) = forwarded {
                return done;
            }
        }
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        let Some(text) = resources.get(&request.uri) else {
            return Err(ErrorData::resource_not_found(
                format!(
                    "resource not found: {} (expired, evicted, or from an earlier server process; re-run the query)",
                    request.uri
                ),
                None,
            ));
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]).into())
    }
}

/// P0 单行安全切片:超长匹配行只展示命中点附近的窗口.
///
/// 严禁用 `to_lowercase().find(..)` 拿字节偏移切原串——`İ`/`ẞ` 等字符的
/// 大小写折叠会改变字节长度， lowered 串上的偏移在原串上漂移，切片直接
/// panic.这里统一经 `regex::Match`(regex 保证字节偏移落在字符边界上)
/// 定位命中；字面量搜索用 `regex::escape(needle)` 构建 case-insensitive
/// regex 在原串上查找.找不到 Match(极端 Unicode 折叠差异，或命中只落
/// 在被 trim 掉的空白里)时退回行首固定窗口兜底，绝不 unwrap/panic.
/// Size cap for text-only search, same as the scanner's default admission
/// cap (files that grew past it after the scan are skipped).
const TEXT_SEARCH_MAX_FILE_BYTES: u64 = 1_500_000;

/// Compile the single matcher used by `search_code`.
///
/// Literal queries are escaped. Default case rules (`case_sensitive: None`):
/// literal is case-insensitive, regex is case-sensitive. `search_code`
/// passes the smart-case decision explicitly (see `search_shape`).
fn build_search_matcher(
    query: &str,
    regex: bool,
    case_sensitive: Option<bool>,
) -> Result<regex::Regex, regex::Error> {
    let pattern = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let sensitive = case_sensitive.unwrap_or(regex);
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!sensitive)
        .build()
}

fn clip_match_line(line: &str, needle: &str, regex: Option<&regex::Regex>) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= MAX_LINE_COLUMNS {
        return trimmed.to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let total = chars.len();

    let found = match regex {
        Some(regex) => regex.find(trimmed),
        None => regex::RegexBuilder::new(&regex::escape(needle))
            .case_insensitive(true)
            .build()
            .ok()
            .and_then(|literal| literal.find(trimmed)),
    };
    let Some(found) = found else {
        // 安全兜底:行首 250 字符窗口 + 省略号.
        let head: String = chars.into_iter().take(MAX_LINE_COLUMNS).collect();
        return format!("{head}…");
    };

    // Match 的字节偏移保证在字符边界上；换算成字符下标再开窗口.
    let start = trimmed[..found.start()].chars().count();
    let span = found.as_str().chars().count();
    let end = start + span;

    let mut out = String::new();
    let window_start = start.saturating_sub(CONTEXT_PADDING);
    if window_start > 0 {
        out.push('…');
    }
    out.extend(chars[window_start..start].iter());
    if span > MAX_MATCH_SPAN_CHARS {
        // 匹配段封顶:`.*` 命中上万字符时只展示前 40 字符.
        out.extend(chars[start..start + MAX_MATCH_SPAN_CHARS].iter());
        out.push_str(&format!("…[match segment {span} chars; excess omitted]"));
    } else {
        out.extend(chars[start..end].iter());
    }
    let after_end = (end + CONTEXT_PADDING).min(total);
    out.extend(chars[end..after_end].iter());
    if after_end < total {
        out.push('…');
    }
    out
}

/// P1 前瞻摘要:资源卸载时从正文提取前 5 行, 总量约 200 tokens 以内的
/// 预览，塞进首个 TextContent——客户端不读 resource 也能判断"这次卸载
/// 的内容是不是我要的"。首行单行超预算时按字符截断，保证 content[0]
/// 始终有可读文本。
fn resource_preview(body: &str) -> String {
    const PREVIEW_LINES: usize = 5;
    const PREVIEW_MAX_TOKENS: usize = 200;
    let mut preview = String::new();
    let mut cut = false;
    for (index, line) in body.lines().enumerate() {
        if index >= PREVIEW_LINES {
            cut = true;
            break;
        }
        let candidate = format!("{preview}{line}\n");
        if estimate_tokens(&candidate) > PREVIEW_MAX_TOKENS {
            if index == 0 {
                // 单行就超预算：按字符截到 200 tokens（约 800 字符）内。
                let clipped: String = line.chars().take(PREVIEW_MAX_TOKENS * 4).collect();
                preview.push_str(&clipped);
                preview.push_str("…\n");
            }
            cut = true;
            break;
        }
        preview = candidate;
    }
    if cut {
        preview.push_str("…(preview truncated; see resource for full content)\n");
    }
    preview.trim_end().to_string()
}

/// find_symbol 查询归一化结果。
struct NormalizedQuery {
    query: String,
    /// 被剥掉的语言关键字（如 `struct`），用于在结果中提示。
    ignored: Option<String>,
}

const QUERY_KEYWORDS: &[&str] = &[
    "struct",
    "class",
    "func",
    "fn",
    "def",
    "function",
    "method",
    "interface",
    "enum",
    "trait",
    "type",
    "impl",
    "record",
];

/// 剥掉 `Proc struct` / `func Foo` 中的关键字，并把 `::`、`.` 视作层级
/// 分隔符（`Type.method`、`A::b`、`pkg.Func` → `Type/method`）。
fn normalize_symbol_query(raw: &str) -> NormalizedQuery {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut ignored = Vec::new();
    let mut kept = Vec::new();
    if tokens.len() > 1 {
        for token in &tokens {
            if QUERY_KEYWORDS.contains(&token.to_ascii_lowercase().as_str()) {
                ignored.push(*token);
            } else {
                kept.push(*token);
            }
        }
        if kept.is_empty() {
            kept = tokens.clone();
            ignored.clear();
        }
    } else {
        kept = tokens.clone();
    }
    let mut query = kept.join(" ");
    if !query.contains('/') && (query.contains("::") || query.contains('.')) {
        let leading = query.starts_with("::");
        let replaced: Vec<&str> = query
            .split("::")
            .flat_map(|part| part.split('.'))
            .filter(|seg| !seg.is_empty())
            .collect();
        if replaced.len() > 1 {
            query = format!("{}{}", if leading { "/" } else { "" }, replaced.join("/"));
        }
    }
    NormalizedQuery {
        query,
        ignored: (!ignored.is_empty()).then(|| format!("`{}`", ignored.join(" "))),
    }
}

/// SymbolKind 的展示名（与 `kind` 过滤参数同一套词）。
fn kind_display_name(kind: astrolabe_core::SymbolKind) -> &'static str {
    use astrolabe_core::SymbolKind as K;
    match kind {
        K::Function => "function",
        K::Method => "method",
        K::Class => "class",
        K::Interface => "interface",
        K::Struct => "struct",
        K::Enum => "enum",
        K::Trait => "trait",
        K::Type => "type",
        K::Const => "const",
        K::Module => "module",
        K::Field => "field",
        K::Variable => "variable",
    }
}

/// 给 find_symbol 的每个命中行补 `[kind in 容器]`，让重名符号（Rust 的
/// 70 个 `new`）可分辨。控制体积：签名已以 kind 词开头（`struct Foo`）时省去
/// kind；容器名最多 24 字符；无容器的顶层符号只标 kind。
fn decorated_symbol_lines(index: &RepoIndex, hits: &[(String, usize)]) -> Vec<String> {
    let symbols = &index.graph().symbols;
    let targets: Vec<usize> = hits.iter().map(|(_, idx)| *idx).collect();
    let containers = astrolabe_core::name_path::innermost_containers_with(
        symbols,
        &index.names().name_path,
        &targets,
    );
    hits.iter()
        .zip(containers)
        .map(|((line, idx), container)| {
            let symbol = &symbols[*idx];
            let kind = kind_display_name(symbol.kind);
            let sig = symbol.signature.trim_start().to_ascii_lowercase();
            let kind_obvious = sig
                .strip_prefix(kind)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace));
            let container = container.map(|c| {
                let name = &symbols[c].name;
                if name.chars().count() > 24 {
                    format!("{}…", name.chars().take(23).collect::<String>())
                } else {
                    name.clone()
                }
            });
            match (kind_obvious, container) {
                (true, None) => line.clone(),
                (true, Some(c)) => format!("{line}  [in {c}]"),
                (false, None) => format!("{line}  [{kind}]"),
                (false, Some(c)) => format!("{line}  [{kind} in {c}]"),
            }
        })
        .collect()
}

/// The page of decorated lines starting at `offset` that fits `limit` tokens,
/// and how many hits remain after it. Decoration (container lookup, kind tag)
/// is the costly part and only the shown page needs it, so hits are decorated
/// in growing windows until the budget cuts the list or the hits run out.
fn paged_symbol_lines(
    index: &RepoIndex,
    hits: &[(String, usize)],
    offset: usize,
    limit: usize,
) -> (Vec<String>, usize) {
    let rest = &hits[offset.min(hits.len())..];
    let mut window = 256usize;
    loop {
        let end = window.min(rest.len());
        let lines = decorated_symbol_lines(index, &rest[..end]);
        let (kept, omitted_in_window) = truncate_ranked(&lines, limit, |line| format!("{line}\n"));
        let kept_len = kept.len();
        if omitted_in_window > 0 || end == rest.len() {
            let kept: Vec<String> = lines.into_iter().take(kept_len).collect();
            return (kept, rest.len() - kept_len);
        }
        window = window.saturating_mul(4);
    }
}

/// 名称匹配档位：0 精确、1 前缀、2 子串（`lower`、`needle` 均已小写）。
fn name_match_tier(lower: &str, needle: &str) -> Option<u8> {
    if lower == needle {
        Some(0)
    } else if lower.starts_with(needle) {
        Some(1)
    } else if lower.contains(needle) {
        Some(2)
    } else {
        None
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// 签名中是否以完整标识符出现 `word`（Go 接收者 `(c *Command)` 等）。
fn signature_mentions(signature: &str, word: &str) -> bool {
    signature
        .split(|c: char| !is_ident_char(c))
        .any(|token| token == word)
}

/// 路径中是否有目录段或文件名主干等于 `component`（pkg.Func 的 pkg）。
fn path_has_component(path: &str, component: &str) -> bool {
    path.split('/')
        .any(|seg| seg == component || seg.split('.').next().is_some_and(|stem| stem == component))
}

/// Rust `impl Type { fn m() }` / `impl Trait for Type`：向上找最近的
/// 顶格 `impl` 行，判断它是否提到父段名（impl 块本身不是索引符号）。
fn enclosing_impl_mentions(
    cache: &mut std::collections::HashMap<String, Option<String>>,
    root: &Path,
    path: &str,
    start_line: u32,
    parent: &str,
) -> bool {
    if !path.ends_with(".rs") {
        return false;
    }
    let source = cache
        .entry(path.to_string())
        .or_insert_with(|| std::fs::read_to_string(root.join(path)).ok());
    let Some(source) = source else {
        return false;
    };
    let before = usize::try_from(start_line.saturating_sub(1)).unwrap_or(0);
    source
        .lines()
        .take(before)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .find(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("impl") && trimmed.len() == line.len()
        })
        .is_some_and(|line| signature_mentions(line, parent))
}

fn edit_distance(a: &str, b: &str) -> usize {
    // Called once per symbol name on zero-hit queries: one reused DP row,
    // on the stack for short names, and bytes instead of chars when both
    // sides are ASCII. Same Levenshtein distance (over chars) as before.
    if a.is_ascii() && b.is_ascii() {
        levenshtein(a.as_bytes(), b.as_bytes())
    } else {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        levenshtein(&a, &b)
    }
}

fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut stack = [0usize; 65];
    let mut heap = Vec::new();
    let row: &mut [usize] = if b.len() < stack.len() {
        &mut stack[..=b.len()]
    } else {
        heap.resize(b.len() + 1, 0);
        &mut heap
    };
    for (j, cell) in row.iter_mut().enumerate() {
        *cell = j;
    }
    for (i, ca) in a.iter().enumerate() {
        // `diag` is the previous row's value at j; `row[j]` becomes the current row's.
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diag + usize::from(ca != cb))
                .min(above + 1)
                .min(row[j] + 1);
            diag = above;
        }
    }
    row[b.len()]
}

/// 零命中时给出相近符号名（编辑距离 ≤2 或共享 4 字符前缀），最多 5 个。
fn symbol_suggestions(index: &RepoIndex, needle: &str) -> String {
    if needle.is_empty() {
        return String::new();
    }
    let prefix: String = needle.chars().take(4).collect();
    let prefix_ok = prefix.chars().count() >= 4;
    let needle_chars = needle.chars().count();
    let mut scored: BTreeMap<(usize, String), ()> = BTreeMap::new();
    let symbols = &index.graph().symbols;
    let names = index.names();
    let lower_names = names.lower();
    // One candidate per distinct lowercase name, named after its first
    // occurrence (as the old first-seen scan did).
    for group in names.name_groups() {
        let first = group[0] as usize;
        let lower = &lower_names[first];
        let distance = if prefix_ok && lower.starts_with(&prefix) {
            edit_distance(lower, needle)
        } else {
            // Edit distance is at least the length difference: skip the
            // quadratic DP for names that cannot be within 2.
            let len = if lower.is_ascii() {
                lower.len()
            } else {
                lower.chars().count()
            };
            if len.abs_diff(needle_chars) > 2 {
                continue;
            }
            let distance = edit_distance(lower, needle);
            if distance > 2 {
                continue;
            }
            distance
        };
        scored.insert((distance, symbols[first].name.clone()), ());
    }
    if scored.is_empty() {
        return "Suggestion: check the name spelling; for qualified names try Type/method, Type.method, or A::b; or use search_code for full-text search.\n".to_string();
    }
    let names: Vec<String> = scored.into_keys().take(5).map(|(_, name)| name).collect();
    format!(
        "Similar symbol names: {} (retry with these names, or use search_code)\n",
        names.join(", ")
    )
}

/// path_filter 不匹配任何已索引路径（也不匹配未入索引的代码文件）时返回
/// 显式错误，列出最接近的已索引路径，而不是静默返回 0 条。
fn path_filter_error(index: &RepoIndex, filter: Option<&str>) -> Option<String> {
    let filter = filter.filter(|f| !f.is_empty())?;
    let files = &index.graph().files;
    if files.iter().any(|file| file.path.as_str().contains(filter)) {
        return None;
    }
    if !index.unindexed_code_exts(Some(filter)).is_empty() {
        return None;
    }
    let cleaned = filter.trim_start_matches("./").trim_start_matches('/');
    let segments: Vec<String> = cleaned
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut candidates: BTreeMap<String, ()> = BTreeMap::new();
    for file in files {
        let path = file.path.as_str();
        candidates.insert(path.to_string(), ());
        let mut dir = path;
        while let Some((parent, _)) = dir.rsplit_once('/') {
            candidates.insert(format!("{parent}/"), ());
            dir = parent;
        }
    }
    let mut scored: Vec<(usize, usize, usize, String)> = candidates
        .into_keys()
        .map(|candidate| {
            let lower = candidate.to_lowercase();
            let hits = segments
                .iter()
                .filter(|seg| lower.contains(seg.as_str()))
                .count();
            let tail = lower
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            let distance = segments
                .last()
                .map_or(usize::MAX, |seg| edit_distance(&tail, seg));
            (usize::MAX - hits, distance, candidate.len(), candidate)
        })
        .collect();
    scored.sort();
    let closest: Vec<String> = scored.into_iter().take(5).map(|entry| entry.3).collect();
    let mut message = format!(
        "path_filter '{filter}' matches no indexed path (case-sensitive repo-relative path substring, not a glob; no absolute path or ./ prefix)."
    );
    if !closest.is_empty() {
        message.push_str(&format!("Closest indexed paths: {}.", closest.join(", ")));
        message.push_str(&format!(
            "Next: use one of the paths above as path_filter (e.g. \"{}\"), or drop path_filter and retry.",
            closest[0]
        ));
    } else {
        message.push_str("Next: drop path_filter and retry.");
    }
    Some(message)
}

/// Prepend the loop-guard note to a cached result and flag it `blocked: true`.
fn mark_blocked(mut result: CallToolResult, note: &str) -> CallToolResult {
    match result.content.first_mut() {
        Some(first) if first.as_text().is_some() => {
            let text = first.as_text().map(|t| t.text.clone()).unwrap_or_default();
            *first = ContentBlock::text(format!("{note}\n\n--- Previous result ---\n{text}"));
        }
        _ => result
            .content
            .insert(0, ContentBlock::text(note.to_string())),
    }
    if let Some(Value::Object(obj)) = result.structured_content.as_mut() {
        obj.insert("blocked".into(), json!(true));
    }
    result
}

pub(crate) fn text_error(message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    CallToolResult::error(vec![ContentBlock::text(if message.trim().is_empty() {
        "unknown error".to_string()
    } else {
        message
    })])
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::{
        graph::CodeGraph, CodeFile, CodeSymbol, Language, RelPath, SymbolId, SymbolKind,
    };
    use std::collections::HashSet;
    use std::time::Duration;

    /// Pre-change `symbol_suggestions` (first-seen scan, full edit distance
    /// for every distinct name), kept as the oracle for the pruned version.
    fn reference_symbol_suggestions(index: &RepoIndex, needle: &str) -> String {
        if needle.is_empty() {
            return String::new();
        }
        let prefix: String = needle.chars().take(4).collect();
        let mut scored: BTreeMap<(usize, String), ()> = BTreeMap::new();
        let mut seen = std::collections::HashSet::new();
        for symbol in &index.graph().symbols {
            let lower = symbol.name.to_lowercase();
            if !seen.insert(lower.clone()) {
                continue;
            }
            let distance = edit_distance(&lower, needle);
            if distance <= 2 || (prefix.chars().count() >= 4 && lower.starts_with(&prefix)) {
                scored.insert((distance, symbol.name.clone()), ());
            }
        }
        if scored.is_empty() {
            return "Suggestion: check the name spelling; for qualified names try Type/method, Type.method, or A::b; or use search_code for full-text search.\n".to_string();
        }
        let names: Vec<String> = scored.into_keys().take(5).map(|(_, name)| name).collect();
        format!(
            "Similar symbol names: {} (retry with these names, or use search_code)\n",
            names.join(", ")
        )
    }

    /// Pre-change `closest_symbols` (path filter after scoring, every name
    /// tokenised), kept as the oracle for the pruned version.
    fn reference_closest_symbols(
        index: &RepoIndex,
        query: &str,
        path_filter: Option<&str>,
        limit: usize,
    ) -> Vec<String> {
        let q = query.trim().to_lowercase();
        if q.len() < 3 || q.len() > 80 {
            return Vec::new();
        }
        let q_tokens = name_tokens(query);
        let mut scored: Vec<(u32, String)> = Vec::new();
        for sym in &index.graph().symbols {
            let name = sym.name.to_lowercase();
            let score = if name == q {
                0
            } else if name.starts_with(&q) || q.starts_with(&name) && name.len() >= 4 {
                1
            } else if name.contains(&q) || q.contains(&name) && name.len() >= 4 {
                2
            } else if edit_distance(&name, &q) <= (q.len() / 5).clamp(1, 2) {
                3
            } else {
                let shared = name_tokens(&sym.name)
                    .iter()
                    .filter(|t| t.len() >= 3 && q_tokens.contains(t))
                    .count();
                if shared >= 1 && shared >= q_tokens.len().min(2) {
                    4
                } else {
                    continue;
                }
            };
            let Some(file) = index.file(sym.file) else {
                continue;
            };
            if let Some(f) = path_filter {
                if !file.path.as_str().contains(f) {
                    continue;
                }
            }
            let line = symbol_line(sym, &file.path);
            let line = match line.rsplit_once("  @") {
                Some((sig, loc)) => format!("{sig}  {loc}"),
                None => line,
            };
            scored.push((score, line));
        }
        scored.sort();
        scored.dedup_by(|a, b| a.1 == b.1);
        scored.into_iter().take(limit).map(|(_, l)| l).collect()
    }

    fn suggestion_fixture() -> RepoIndex {
        let names = [
            "Serialize",
            "serialize",
            "SERIALIZE",
            "Serializer",
            "SerializeObject",
            "deserialize",
            "JsonSerializer",
            "parse_json",
            "ParseJson",
            "handle",
            "Handler",
            "handle_request",
            "HTTPServer",
            "http_server",
            "Ärger",
            "ärgern",
            "İd",
            "straße",
            "STRASSE",
            "x",
            "ab",
            "abc",
            "abcd",
            "serialise",
            "seriali",
            "Config",
            "ConfigBuilder",
            "configure",
            "load_config_file",
            "ConfigFileLoader",
            "über_config",
            "日本語名",
            "名前",
        ];
        let mut files = Vec::new();
        let mut symbols = Vec::new();
        for f in 0..3u32 {
            files.push(CodeFile {
                id: FileId(f),
                path: RelPath::new(format!("dir{f}/m{f}.rs")),
                language: Some(Language::Rust),
                loc: 10,
                sha: String::new(),
            });
            for (i, name) in names.iter().enumerate() {
                if (i as u32 + f).is_multiple_of(3) && f > 0 {
                    continue;
                }
                symbols.push(CodeSymbol {
                    id: SymbolId(symbols.len() as u32),
                    file: FileId(f),
                    name: (*name).to_string(),
                    kind: SymbolKind::Function,
                    signature: format!("fn {name}()"),
                    start_line: i as u32 + 1,
                    end_line: i as u32 + 1,
                    exported: true,
                });
            }
        }
        // A symbol whose file is not in the graph is skipped by both.
        symbols.push(CodeSymbol {
            id: SymbolId(symbols.len() as u32),
            file: FileId(99),
            name: "Serialize".into(),
            kind: SymbolKind::Function,
            signature: "fn Serialize()".into(),
            start_line: 1,
            end_line: 1,
            exported: true,
        });
        let graph = CodeGraph {
            files,
            symbols,
            edges: vec![],
        };
        RepoIndex::from_graph(PathBuf::from("."), graph, Default::default())
    }

    /// The pruned zero-hit suggestions (distinct-name groups, length bound,
    /// token pre-check, path filter first) must equal the old full scan.
    #[test]
    fn zero_hit_suggestions_match_reference_scan() {
        let index = suggestion_fixture();
        let needles = [
            "serialize",
            "serialzer",
            "serial",
            "seri",
            "ser",
            "jsonserializer",
            "handel",
            "httpserver",
            "ärger",
            "arger",
            "i̇d",
            "strasse",
            "straße",
            "x",
            "ab",
            "abcde",
            "zzz_nope",
            "configfile",
            "config_loader",
            "load_config",
            "über",
            "日本語",
            "名前x",
            "",
            "serializeobjectinternalx",
            "parsejson",
            "parse_json_x",
        ];
        for needle in needles {
            assert_eq!(
                symbol_suggestions(&index, needle),
                reference_symbol_suggestions(&index, needle),
                "needle {needle:?}"
            );
            for filter in [None, Some("dir1/"), Some("dir2"), Some("nomatch")] {
                for raw in [needle.to_string(), needle.to_uppercase()] {
                    assert_eq!(
                        closest_symbols(&index, &raw, filter, 8),
                        reference_closest_symbols(&index, &raw, filter, 8),
                        "query {raw:?} filter {filter:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn index_read_lock_is_released_while_a_tool_runs() {
        let server = ready_server();
        let session = Arc::clone(&server.primary);
        let replacement = match &*session.state.read().unwrap() {
            IndexState::Ready(i) => Arc::clone(i),
            _ => panic!("ready"),
        };
        let result = server.with_index_without_coverage_note("", |_, _| {
            // A watcher publishing a new index must not wait for this tool.
            let s2 = Arc::clone(&session);
            let handle = std::thread::spawn(move || {
                *s2.state.write().unwrap() = IndexState::Ready(replacement);
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "writer blocked by running tool"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            handle.join().unwrap();
            text_error("done")
        });
        assert!(fs_text(&result).contains("done"));
    }

    fn ready_server() -> AstrolabeServer {
        let mut server =
            AstrolabeServer::new_with_cache_budget(PathBuf::from("."), DEFAULT_MEMORY_BUDGET_BYTES);
        let graph = CodeGraph {
            files: vec![CodeFile {
                id: FileId(0),
                path: RelPath::new("src/lib.rs"),
                language: Some(Language::Rust),
                loc: 10,
                sha: String::new(),
            }],
            symbols: vec![CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: "alpha".into(),
                kind: SymbolKind::Function,
                signature: "pub fn alpha()".into(),
                start_line: 3,
                end_line: 3,
                exported: true,
            }],
            edges: vec![],
        };
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(
            RepoIndex::from_graph(PathBuf::from("."), graph, Default::default()),
        ));
        // Tests must not inherit ASTROLABE_STRUCTURED from the environment.
        server.structured_output = false;
        server
    }

    #[test]
    fn looks_like_regex_query_catches_alternation_and_skips_dotted_names() {
        assert!(looks_like_regex_query(
            "scanForBrandHint|RewriteStreamingBody"
        ));
        assert!(looks_like_regex_query("foo.*bar"));
        assert!(looks_like_regex_query("\\bHermes\\b"));
        assert!(!looks_like_regex_query("foo.bar"));
        assert!(!looks_like_regex_query("RewriteClaudeBody"));
        assert!(!looks_like_regex_query("$"));
    }

    fn assert_first_text(result: CallToolResult) {
        let text = result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|content| content.text.trim())
            .unwrap_or_default();
        assert!(!text.is_empty(), "content[0].text must always be non-empty");
        assert!(
            text.starts_with("index_root:"),
            "tool text must declare the absolute index_root: {text}"
        );
    }

    #[test]
    fn tool_schemas_include_budget_and_tool_count_is_bounded() {
        let server = ready_server();
        let tools = server.tool_router.list_all();
        // Compare against the constant rather than a literal so adding a tool
        // fails in exactly one place.
        assert_eq!(tools.len(), crate::TOOL_COUNT);
        let names: HashSet<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        let catalog: HashSet<String> = crate::CATALOG_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        assert_eq!(names, catalog);
        for tool in tools {
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            assert!(
                schema.pointer("/properties/budget_tokens").is_some(),
                "{} lacks budget_tokens",
                tool.name
            );
        }
    }

    #[test]
    fn excluded_tools_are_omitted_from_catalog() {
        let mut ctx = crate::ClientContext::default_builtin();
        ctx.excluded_tools = vec!["get_languages".into(), "find_symbol".into()];
        let server = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            ctx,
        );
        let names: Vec<String> = server
            .listed_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(
            !names.iter().any(|name| name == "get_languages"),
            "{names:?}"
        );
        assert!(!names.iter().any(|name| name == "find_symbol"), "{names:?}");
        assert!(
            names.iter().any(|name| name == "resolve_context"),
            "{names:?}"
        );
        assert_eq!(names.len(), crate::TOOL_COUNT - 2);
    }

    #[test]
    fn codex_list_tools_sanitizes_integer_and_null_unions() {
        let ctx = crate::ClientContext::builtin("codex").unwrap();
        assert!(ctx.openai_tool_compatible());
        let server = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            ctx,
        );
        let tools = server.listed_tools();
        assert!(!tools.is_empty());
        for tool in &tools {
            let schema = Value::Object((*tool.input_schema).clone());
            let schema_str = schema.to_string();
            assert!(
                !schema_str.contains("\"integer\""),
                "{} still has integer: {schema_str}",
                tool.name
            );
            if let Some(line) = schema.pointer("/properties/line") {
                assert_eq!(line.get("type"), Some(&json!("number")), "{}", tool.name);
                assert_eq!(line.get("multipleOf"), Some(&json!(1)), "{}", tool.name);
            }
            if let Some(Value::Object(props)) = schema.get("properties") {
                for (prop_name, prop) in props {
                    assert!(
                        prop.get("type").is_some(),
                        "{} property `{prop_name}` missing type after sanitize: {prop}",
                        tool.name
                    );
                    if let Some(Value::Array(types)) = prop.get("type") {
                        assert!(
                            types.iter().all(|t| t.as_str() != Some("null")),
                            "{} property `{prop_name}` still has null in type: {prop}",
                            tool.name
                        );
                    }
                }
            }
        }

        // Default context must leave schemars integer alone.
        let default_server = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            crate::ClientContext::default_builtin(),
        );
        let raw = Value::Object((*default_server.listed_tools()[0].input_schema).clone());
        let line = raw
            .pointer("/properties/line")
            .expect("find_references line");
        assert_eq!(
            line.get("type"),
            Some(&json!("integer")),
            "non-codex contexts must not sanitize"
        );
    }

    #[test]
    fn structured_output_for_respects_context_then_env_override() {
        let claude = crate::ClientContext::builtin("claude-code").unwrap();
        let cursor = crate::ClientContext::builtin("cursor").unwrap();
        assert!(!claude.structured_or_auto_off());
        assert!(cursor.structured_or_auto_off());
        match structured_output_override() {
            Some(forced) => {
                assert_eq!(structured_output_for(&claude), forced);
                assert_eq!(structured_output_for(&cursor), forced);
            }
            None => {
                assert!(!structured_output_for(&claude));
                assert!(structured_output_for(&cursor));
            }
        }
    }

    #[test]
    fn every_tool_returns_nonempty_first_text_block() {
        let server = ready_server();
        assert_first_text(server.resolve_context(Parameters(ResolveContextParams {
            root: None,
            index_root: None,
            task_description: "alpha".into(),
            budget_tokens: 100,
            dedup: None,
            offset: 0,
            path_filter: None,
        })));
        assert_first_text(server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 100,
            offset: 0,
        })));
        assert_first_text(server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "definitely absent".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 100,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        })));
        assert_first_text(server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 100,
            host_lsp: None,
        })));
    }

    #[test]
    fn budget_parameter_reaches_core_truncation() {
        let server = ready_server();
        let result = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 0,
            offset: 0,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("truncated=true"), "{text}");
        assert!(text.contains("Not the full set"), "{text}");
        assert!(
            text.contains("budget_tokens=0") && text.contains("omitted"),
            "{text}"
        );
        assert!(
            result.structured_content.is_none(),
            "Claude Code replaces text with structuredContent; default is text-only"
        );
    }

    #[test]
    fn structured_output_opt_in_carries_root_and_full_text() {
        let mut server = ready_server();
        server.structured_output = true;
        let result = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 100,
            host_lsp: None,
        }));
        let text = result.content[0].as_text().unwrap().text.clone();
        let structured = result.structured_content.expect("opt-in structured");
        assert_eq!(structured["root"], json!(server.root.display().to_string()));
        assert_eq!(structured["text"], json!(text));
        assert_eq!(
            structured["file_cache_budget_bytes"],
            json!(DEFAULT_MEMORY_BUDGET_BYTES)
        );
        assert!(text.starts_with("index_root:"), "{text}");
    }

    #[test]
    fn large_results_keep_text_first_then_resource_link() {
        let server = ready_server();
        // SLACK 双保险：必须超过 基线+SLACK 才卸载，恰好超基线 1 token
        // 仍保持内联（见 dynamic_threshold_boundary_keeps_exact_budget_inline）。
        let result = server.result(
            "large",
            "x".repeat((DEFAULT_RESOURCE_THRESHOLD_TOKENS + RESOURCE_SLACK_TOKENS + 1) * 4),
            json!({}),
            None,
        );
        assert!(result.content[0].as_text().is_some());
        assert!(result.content[1].as_resource_link().is_some());
    }

    #[test]
    fn dynamic_threshold_boundary_keeps_exact_budget_inline() {
        let server = ready_server();
        // caller budget=6000 且正文恰好 6000 tokens：<= threshold+SLACK，
        // 必须保持内联，不被卸载（kept 恰好等于 budget 的边界）。
        let inline = server.result("boundary", "x".repeat(6_000 * 4), json!({}), Some(6_000));
        assert_eq!(
            inline.content.len(),
            1,
            "exact-budget body must stay inline"
        );
        assert!(inline.content[0].as_text().is_some());

        // 超过 budget + SLACK 后才卸载为资源。
        let offloaded = server.result(
            "boundary",
            "x".repeat((6_000 + RESOURCE_SLACK_TOKENS + 1) * 4),
            json!({}),
            Some(6_000),
        );
        assert!(offloaded.content[0].as_text().is_some());
        assert!(offloaded.content[1].as_resource_link().is_some());
    }

    #[test]
    fn budget_clamp_hard_cap_and_floor() {
        // 硬上限：异常大的 budget 被压回 max。
        assert_eq!(clamp_budget_within(100_000, 32_000), 32_000);
        assert_eq!(clamp_budget_within(32_000, 32_000), 32_000);
        // 下限：0/1 钳到 1，预算永远非零正数。
        assert_eq!(clamp_budget_within(0, 32_000), 1);
        assert_eq!(clamp_budget_within(1, 32_000), 1);
        // 常规值原样通过。
        assert_eq!(clamp_budget_within(5_000, 32_000), 5_000);
        assert_eq!(clamp_budget_within(9, 8), 8);
        // env 解析：未设/空白数字/格式错误回退默认。
        assert_eq!(parse_env_usize(None, "X", 4_000), 4_000);
        assert_eq!(parse_env_usize(Some(" 123 "), "X", 4_000), 123);
        assert_eq!(parse_env_usize(Some("nope"), "X", 4_000), 4_000);
        assert_eq!(parse_env_usize(Some("0"), "X", 4_000), 0);
    }

    #[test]
    fn resource_threshold_takes_max_of_base_and_clamped_budget() {
        let server = ready_server();
        let base = resource_threshold_base_from_env();
        // 无 budget / 预算低于基线：阈值就是基线。
        assert_eq!(server.resource_threshold_for(None), base);
        assert_eq!(server.resource_threshold_for(Some(0)), base);
        assert_eq!(server.resource_threshold_for(Some(100)), base.max(100));
        // 预算高于基线：阈值抬到钳制后的 budget。
        assert_eq!(
            server.resource_threshold_for(Some(6_000)),
            base.max(clamp_budget(6_000))
        );
        // 异常大的 budget 被硬上限压住，阈值不会无限抬升。
        assert_eq!(clamp_budget(usize::MAX), max_budget_tokens_from_env());
        assert_eq!(
            server.resource_threshold_for(Some(usize::MAX)),
            base.max(max_budget_tokens_from_env())
        );
    }

    #[test]
    fn offloaded_result_carries_lookahead_preview() {
        let server = ready_server();
        let mut body = String::new();
        for i in 0..30 {
            body.push_str(&format!("line-{i:02} {}\n", "z".repeat(660)));
        }
        // 30 行 × ~168 tokens ≈ 5040 tokens > 4000 + SLACK → 卸载。
        let result = server.result("search_code", body.clone(), json!({}), Some(2_500));
        assert!(result.content[0].as_text().is_some());
        assert!(result.content[1].as_resource_link().is_some());
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("spilled to resource"), "{text}");
        assert!(text.contains("[Match preview]"), "{text}");
        assert!(text.contains("line-00"), "{text}");
        assert!(text.contains("resources/read"), "{text}");
        // 摘要封顶 ~200 tokens：第 2 行就放不下，不能把整份 body 塞进摘要。
        assert!(!text.contains("line-01 "), "{text}");
        assert!(!text.contains("line-05"), "{text}");
        // 资源里保存的是完整 body。
        let stored = server.resources.lock().unwrap();
        assert!(
            stored.values().any(|v| v == &body),
            "offloaded resource must hold the full body"
        );
    }

    #[test]
    fn clip_match_line_short_lines_are_returned_trimmed() {
        assert_eq!(clip_match_line("  let x = 1;  ", "x", None), "let x = 1;");
        // 正好 250 字符：边界内原样返回。
        let exact: String = "a".repeat(MAX_LINE_COLUMNS);
        assert_eq!(clip_match_line(&exact, "a", None), exact);
        // 超一个字符才进入切片路径。
        let over = format!("{exact}b");
        let clipped = clip_match_line(&over, "a", None);
        assert!(clipped.contains('…'), "{clipped}");
        assert!(clipped.chars().count() <= MAX_LINE_COLUMNS + 1);
    }

    #[test]
    fn clip_match_line_long_ascii_centers_on_needle() {
        let line = format!("padding {} NEEDLE {}", "z".repeat(400), "y".repeat(400));
        let clipped = clip_match_line(&line, "needle", None);
        assert!(clipped.contains("NEEDLE"), "{clipped}");
        assert!(
            clipped.starts_with('…') && clipped.ends_with('…'),
            "{clipped}"
        );
        assert!(clipped.chars().count() < 2 * MAX_LINE_COLUMNS, "{clipped}");
    }

    #[test]
    fn clip_match_line_match_at_start_and_end_avoids_false_ellipses() {
        // 行首命中：无前导省略号（窗口起点就是 0）。
        let line = format!("NEEDLE{}", "z".repeat(400));
        let clipped = clip_match_line(&line, "needle", None);
        assert!(clipped.starts_with("NEEDLE"), "{clipped}");
        assert!(!clipped.starts_with('…'), "{clipped}");
        assert!(clipped.ends_with('…'), "{clipped}");

        // 行尾命中：无尾随省略号（窗口终点贴齐行尾）。
        let line = format!("{}NEEDLE", "z".repeat(400));
        let clipped = clip_match_line(&line, "needle", None);
        assert!(clipped.ends_with("NEEDLE"), "{clipped}");
        assert!(clipped.starts_with('…'), "{clipped}");
        assert!(!clipped.ends_with("NEEDLE…"), "{clipped}");
    }

    #[test]
    fn clip_match_line_unicode_case_folding_never_panics() {
        // ẞ (U+1E9E) 与 ß (U+00DF) 是 simple-case-folding 同组：regex 能在
        // 原串上定位命中（naive 的 to_lowercase 字节偏移在这里会漂移 panic）。
        let line = format!("{}ẞ{}", "x".repeat(300), "y".repeat(300));
        let clipped = clip_match_line(&line, "ß", None);
        assert!(clipped.contains('ẞ'), "{clipped}");

        // İ (U+0130) 的完整小写是两个码点（i + U+0307），simple folding 不同
        // 组：regex 找不到 Match，必须走行首安全窗口兜底而不是 panic。
        let line = format!("{}İ tail", "İ".repeat(300));
        let clipped = clip_match_line(&line, "i\u{307}", None);
        assert!(clipped.chars().count() <= MAX_LINE_COLUMNS + 1, "{clipped}");
        assert!(clipped.ends_with('…'), "{clipped}");
    }

    #[test]
    fn clip_match_line_caps_huge_match_spans() {
        // 纯 CJK 超长单行、命中整行（字面量 needle 覆盖全行）：匹配段封顶。
        let line = "中".repeat(600);
        let clipped = clip_match_line(&line, &line, None);
        assert!(clipped.contains("match segment"), "{clipped}");
        assert!(clipped.contains('…'), "{clipped}");
        assert!(clipped.chars().count() < MAX_LINE_COLUMNS, "{clipped}");
        assert!(
            !clipped.contains(&"中".repeat(MAX_MATCH_SPAN_CHARS + 1)),
            "{clipped}"
        );

        // regex `.*` 类命中横跨整行：同样封顶。
        let regex = regex::Regex::new("a.*b").unwrap();
        let line = format!("a{}b", "m".repeat(500));
        let clipped = clip_match_line(&line, "", Some(&regex));
        assert!(clipped.contains("match segment"), "{clipped}");
        assert!(clipped.chars().count() < MAX_LINE_COLUMNS, "{clipped}");
    }

    #[test]
    fn search_code_clips_overlong_matching_lines() {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("long.py"),
            format!("marker = '{}'\n", "needle".repeat(300)),
        )
        .unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;

        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "marker".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2_500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = result.content[0].as_text().unwrap().text.clone();
        let rendered = text
            .lines()
            .find(|line| line.contains("@long.py:1"))
            .expect("match line must exist");
        assert!(rendered.contains("marker"), "{rendered}");
        assert!(rendered.contains('…'), "{rendered}");
        assert!(
            rendered.chars().count() < MAX_LINE_COLUMNS + 100,
            "{rendered}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_zero_shown_diagnoses_single_item_overflow() {
        let dir = unique_temp_dir();
        let mut source = String::from("needle = 0\n");
        for i in 1..10 {
            source.push_str(&format!("needle = {i}\n"));
        }
        std::fs::write(dir.join("hits.py"), source).unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;

        // budget=1：扣掉头部 35 tokens 后剩余 0，首条匹配单项就超预算。
        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "needle".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("shown=0"), "{text}");
        assert!(text.contains("truncated=true"), "{text}");
        assert!(
            text.contains("first result alone exceeds budget_tokens=1 and was omitted"),
            "{text}"
        );
        assert!(
            text.to_ascii_lowercase().contains("raise budget_tokens"),
            "{text}"
        );
        // 一条都没展示时，正文绝不能出现匹配行内容本身。
        assert!(!text.contains("@hits.py:1 "), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unique_temp_dir() -> PathBuf {
        // A timestamp alone is not unique: macOS resolves `SystemTime::now`
        // to roughly a microsecond, so two tests starting in the same instant
        // get the same path, and whichever finishes first deletes the other's
        // files mid-run. The counter makes collisions impossible.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-mcp-{}-{}-{}-{}",
            "server",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn ready_server_with_disk() -> (AstrolabeServer, PathBuf) {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("hello.py"),
            "def greet():\n    return 'cache-me'\n",
        )
        .unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        (server, dir)
    }

    fn smart_server(files: &[(&str, String)]) -> (AstrolabeServer, PathBuf) {
        let dir = unique_temp_dir();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        (server, dir)
    }

    fn search(query: &str) -> SearchParams {
        SearchParams {
            root: None,
            index_root: None,
            query: query.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: None,
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }
    }

    /// A result as a later call in the same session renders it: fixed
    /// legends, footers and confidence parentheticals are sent once.
    fn once_free(text: &str) -> String {
        text.replace(
            "confidence: exact (on-disk source literal match)",
            "confidence: exact",
        )
        .replace(
            "confidence: syntactic (symbol-name match)",
            "confidence: syntactic",
        )
        .lines()
        .filter(|l| {
            !l.starts_with("Next: to count")
                && !l.starts_with("Hit-line tags:")
                && !l.starts_with("Context:")
        })
        .map(|l| format!("{l}\n"))
        .collect()
    }

    fn text_of(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    const CALC_PY: &str = "def compute(a, b):\n    total = a + b\n    total *= 2\n    marker_x = total - 1\n    return marker_x\n\n\ndef caller():\n    return compute(1, 2)\n";

    #[test]
    fn explain_symbol_compat_aliases_to_find_symbol() {
        let (server, dir) = smart_server(&[(
            "calc.py",
            "def compute(a, b):\n    return a + b\n\ndef caller():\n    return compute(1, 2)\n"
                .into(),
        )]);
        let args = json!({"name": "compute"})
            .as_object()
            .cloned()
            .expect("object");
        let via_dispatch = server
            .dispatch_tool(crate::compat::EXPLAIN_SYMBOL, Some(args.clone()))
            .expect("dispatch");
        assert_ne!(
            via_dispatch.is_error,
            Some(true),
            "{}",
            text_of(&via_dispatch)
        );
        let out = text_of(&via_dispatch);
        assert!(out.starts_with("index_root:"), "{out}");
        assert!(out.contains("[compat] explain_symbol"), "{out}");
        assert!(out.contains("find_symbol"), "{out}");
        assert!(out.contains("compute"), "{out}");
        assert!(
            out.contains("def compute") || out.contains("function compute"),
            "expected body via include_body: {out}"
        );
        let sc = via_dispatch.structured_content.expect("structured");
        assert_eq!(sc["compat_alias"], "find_symbol");
        assert_eq!(sc["compat_from"], "explain_symbol");

        // Catalog stays at 7 — alias is call-time only.
        let listed: Vec<_> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(listed.len(), crate::TOOL_COUNT);
        assert!(!listed.iter().any(|n| n == "explain_symbol"), "{listed:?}");

        let missing = server
            .dispatch_tool(crate::compat::EXPLAIN_SYMBOL, Some(Default::default()))
            .expect("dispatch empty");
        assert_eq!(missing.is_error, Some(true));
        let err = text_of(&missing);
        assert!(err.contains("find_symbol"), "{err}");
        assert!(err.contains("find_references"), "{err}");

        let via_query = server
            .dispatch_tool(
                crate::compat::EXPLAIN_SYMBOL,
                Some(json!({"query": "compute"}).as_object().cloned().unwrap()),
            )
            .expect("dispatch query");
        assert!(
            text_of(&via_query).contains("compute"),
            "{}",
            text_of(&via_query)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_few_hits_expand_to_enclosing_symbol_once() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let text = text_of(&server.search_code(Parameters(search("marker_x"))));
        // Two hits in one function share a single body with both anchors.
        assert_eq!(
            text.matches("── function compute · calc.py:1-5 ──").count(),
            1,
            "{text}"
        );
        assert!(text.contains("  1| def compute(a, b):"), "{text}");
        assert!(text.contains("@calc.py:4 marker_x = total - 1"), "{text}");
        assert!(text.contains("@calc.py:5 return marker_x"), "{text}");
        assert!(
            text.contains("whole enclosing function/type given"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_session_dedup_references_resent_bodies() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let _ = server.search_code(Parameters(search("marker_x")));
        let again = text_of(&server.search_code(Parameters(search("total *= 2"))));
        assert!(!again.contains("  1| def compute"), "{again}");
        assert!(again.contains("@calc.py:3 total *= 2"), "{again}");
        assert!(
            again.contains("calc.py:1-5") && again.contains("already returned on tool call #1"),
            "{again}"
        );
        let mut opt_out = search("total = a");
        opt_out.dedup = Some(false);
        let full = text_of(&server.search_code(Parameters(opt_out)));
        assert!(full.contains("  1| def compute(a, b):"), "{full}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_broad_query_returns_counts_and_paging_still_works() {
        let many: String = (0..70).map(|i| format!("NEEDLE_{i} = {i}\n")).collect();
        let few: String = (0..5).map(|i| format!("NEEDLE_b{i} = {i}\n")).collect();
        let (server, dir) = smart_server(&[("pkg/a/one.py", many), ("pkg/b/two.py", few)]);
        let text = text_of(&server.search_code(Parameters(search("NEEDLE_"))));
        assert!(
            text.contains("Wide query: 75 hits across 2 files"),
            "{text}"
        );
        assert!(text.contains("     70  pkg/a/one.py"), "{text}");
        assert!(text.contains("Suggestion: path_filter=\"pkg/a\""), "{text}");
        assert_eq!(text.matches("\n@pkg/").count(), 12, "{text}");
        let mut paged = search("NEEDLE_");
        paged.page = true;
        let page = text_of(&server.search_code(Parameters(paged)));
        assert!(page.contains("@pkg/a/one.py:1 NEEDLE_0 = 0"), "{page}");
        let mut second = search("NEEDLE_");
        second.offset = 70;
        let second = text_of(&server.search_code(Parameters(second)));
        assert!(second.contains("@pkg/b/two.py:1 NEEDLE_b0 = 0"), "{second}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_group_true_truncated_does_not_suggest_unusable_offset_and_offset_pages() {
        let a: String = (0..40).map(|i| format!("NEEDLE_a{i}\n")).collect();
        let b: String = (0..40).map(|i| format!("NEEDLE_b{i}\n")).collect();
        let (server, dir) = smart_server(&[("pkg/a.py", a), ("pkg/b.py", b)]);
        let mut grouped = search("NEEDLE_");
        grouped.group = Some(true);
        grouped.budget_tokens = 300;
        let text = text_of(&server.search_code(Parameters(grouped.clone())));
        assert!(text.contains("incomplete"), "{text}");
        assert!(text.contains("cannot page via offset"), "{text}");
        assert!(
            !text.contains("offset=") || text.contains("group=false then offset="),
            "{text}"
        );
        // offset > 0 with group=true falls back to ordinary paging.
        grouped.offset = 40;
        grouped.budget_tokens = 2500;
        let page = text_of(&server.search_code(Parameters(grouped)));
        assert!(!page.contains("grouped=true"), "{page}");
        assert!(page.contains("NEEDLE_b0"), "{page}");
        assert!(!page.contains("NEEDLE_a0\n"), "{page}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_repeated_count_only_and_aggregate_resend_table_and_advance_call_seq() {
        let (server, dir) = smart_server(&[("pkg/a.py", "x = foo()\nfoo(foo())\n".into())]);
        let mut p = search("foo(");
        p.aggregate = Some("file".into());
        let first = text_of(&server.search_code(Parameters(p.clone())));
        let second = text_of(&server.search_code(Parameters(p.clone())));
        assert!(second.contains("pkg/a.py"), "{second}");
        assert!(!second.contains("Same query already"), "{second}");
        assert_eq!(once_free(&first), second);
        let mut c = search("foo(");
        c.count_only = true;
        let _ = server.search_code(Parameters(c.clone()));
        let again = text_of(&server.search_code(Parameters(c)));
        assert!(again.contains("total 2 hit lines"), "{again}");
        // Wording is honest: lines, not occurrences (foo( occurs 3 times).
        assert!(again.contains("not occurrences"), "{again}");
        assert_eq!(server.call_seq.load(Ordering::Relaxed), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_text_only_gbk_and_utf16_bom_files_are_searchable() {
        let (server, dir) = smart_server(&[("src/a.py", "x = 1\n".into())]);
        // GBK: "# 你好，这是一段中文说明" + newline + marker.
        let mut gbk = b"# \xC4\xE3\xBA\xC3\xA3\xAC\xD5\xE2\xCA\xC7\xD2\xBB\xB6\xCE\xD6\xD0\xCE\xC4\xCB\xB5\xC3\xF7\n".to_vec();
        gbk.extend_from_slice(b"SysLogGbk\n");
        std::fs::write(dir.join("notes.txt"), gbk).unwrap();
        let mut u16b = vec![0xFF, 0xFE];
        for u in "line\nSysLogU16 here\n".encode_utf16() {
            u16b.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(dir.join("res.sql"), u16b).unwrap();
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        let gbk_hit = text_of(&server.search_code(Parameters(search("你好"))));
        assert!(gbk_hit.contains("notes.txt"), "{gbk_hit}");
        let u16_hit = text_of(&server.search_code(Parameters(search("SysLogU16"))));
        assert!(u16_hit.contains("res.sql"), "{u16_hit}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_newline_regex_and_empty_query_are_explicit_errors() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let mut multi = search("foo\\nbar");
        multi.regex = true;
        let r = server.search_code(Parameters(multi));
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).contains("line by line"), "{}", text_of(&r));
        let empty = server.search_code(Parameters(search("")));
        assert_eq!(empty.is_error, Some(true));
        assert!(
            text_of(&empty).contains("cannot be empty"),
            "{}",
            text_of(&empty)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn looks_like_code_fragment_detects_signatures_not_regex() {
        assert!(looks_like_code_fragment("fn process(data: &str)"));
        assert!(looks_like_code_fragment("compute(a, b)"));
        assert!(looks_like_code_fragment("Type::new(cfg)"));
        assert!(looks_like_code_fragment("obj.method(")); // unclosed call
        assert!(looks_like_code_fragment("(x) => x + 1"));
        assert!(!looks_like_code_fragment("foo|bar"));
        assert!(!looks_like_code_fragment(r"foo\(.*\)"));
        assert!(!looks_like_code_fragment(r"^\s*fn\s+"));
        assert!(!looks_like_code_fragment("[a-z]+"));
        assert!(!looks_like_code_fragment("(foo|bar)"));
        assert!(!looks_like_code_fragment("plain_name"));
    }

    #[test]
    fn search_code_downgrades_signature_query_from_regex_to_literal() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        // As regex, `compute(a, b)` is a capture group and would match `computea, b`
        // (no parens). As literal it matches the call site in CALC_PY.
        let mut p = search("compute(a, b)");
        p.regex = true;
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("ignored regex=true"), "{text}");
        assert!(text.contains("compute(a, b)"), "{text}");
        assert!(text.contains("calc.py"), "{text}");
        assert!(!text.contains("(no matching code lines)"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_downgrades_unclosed_call_instead_of_invalid_regex() {
        let (server, dir) =
            smart_server(&[("calc.py", "def helper(x):\n    return helper(x\n".into())]);
        let mut p = search("helper(x");
        p.regex = true;
        let r = server.search_code(Parameters(p));
        assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
        let text = text_of(&r);
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("ignored regex=true"), "{text}");
        assert!(text.contains("helper(x"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_invalid_regex_suggests_literal() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        // Character class left open — not a code-fragment shape, so still an error.
        let mut p = search("[unclosed");
        p.regex = true;
        let r = server.search_code(Parameters(p));
        assert_eq!(r.is_error, Some(true));
        let text = text_of(&r);
        assert!(text.contains("Invalid regex"), "{text}");
        assert!(text.contains("regex=false"), "{text}");
        assert!(text.contains("literal"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_keeps_intentional_regex_with_groups() {
        let (server, dir) = smart_server(&[("calc.py", "alpha_hit\nbeta_hit\n".into())]);
        let mut p = search("alpha_hit|beta_hit");
        p.regex = true;
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(text.contains("mode: regex"), "{text}");
        assert!(!text.contains("ignored regex=true"), "{text}");
        assert!(text.contains("alpha_hit"), "{text}");
        assert!(text.contains("beta_hit"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hit_role_matches_words_not_substrings() {
        assert_eq!(hit_role(false, "return thread"), "use");
        assert_eq!(hit_role(false, "x = spread(a)"), "use");
        assert_eq!(hit_role(false, "download(url)"), "use");
        assert_eq!(hit_role(false, "x = read_file(p)"), "load");
        assert_eq!(hit_role(false, "cfg.loadConfig()"), "load");
        assert_eq!(hit_role(false, "os.Getenv(\"A\")"), "load");
        assert_eq!(hit_role(false, "budget(3)"), "use");
        assert_eq!(hit_role(true, "event.error(x)"), "ref");
        assert_eq!(hit_role(true, "assert_eq!(a, b)"), "assert");
    }

    #[test]
    fn search_code_data_hits_outside_respects_exclude() {
        let (server, dir) = smart_server(&[
            ("src/a.py", "x = MARKER_Q\n".into()),
            ("testdata/d.json", "{\"k\": \"MARKER_Q\"}\n".into()),
        ]);
        let mut p = search("MARKER_Q");
        p.path_filter = Some("src/".into());
        let with = text_of(&server.search_code(Parameters(p.clone())));
        assert!(with.contains("testdata/d.json"), "{with}");
        let mut q = p.clone();
        q.dedup = Some(false);
        q.exclude = Some(vec!["testdata".into()]);
        let without = text_of(&server.search_code(Parameters(q)));
        assert!(!without.contains("testdata/d.json"), "{without}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn find(query: &str) -> QueryParams {
        QueryParams {
            root: None,
            index_root: None,
            query: query.into(),
            include_body: true,
            substring: Some(false),
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 2500,
            offset: 0,
        }
    }

    #[test]
    fn find_references_inherits_path_from_unique_find_symbol() {
        let (server, dir) = smart_server(&[(
            "svc/channel.go",
            "package svc\n\nfunc ShouldDisableChannel() bool { return false }\n".into(),
        )]);
        // Unique exact hit → remembered for this session.
        let sym = text_of(&server.find_symbol(Parameters(find("ShouldDisableChannel"))));
        assert!(
            sym.contains("svc/channel.go"),
            "find_symbol should hit the definition: {sym}"
        );
        let refs = text_of(&server.find_references(Parameters(
            crate::precise_tools::FindReferencesParams {
                root: None,
                index_root: None,
                symbol: "ShouldDisableChannel".into(),
                path: None,
                line: None,
                character: None,
                budget_tokens: 2500,
                offset: None,
                dry_run_rename: None,
            },
        )));
        assert!(
            refs.contains("reusing this session's latest") && refs.contains("svc/channel.go"),
            "expected session inherit note: {refs}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_references_does_not_inherit_when_find_symbol_is_ambiguous() {
        let (server, dir) = smart_server(&[
            ("a/x.go", "package a\n\nfunc Dup() {}\n".into()),
            ("b/y.go", "package b\n\nfunc Dup() {}\n".into()),
        ]);
        let sym = text_of(&server.find_symbol(Parameters(find("Dup"))));
        assert!(sym.contains("a/x.go") && sym.contains("b/y.go"), "{sym}");
        // Two exact hits → nothing remembered; missing path stays Ambiguous.
        let refs = text_of(&server.find_references(Parameters(
            crate::precise_tools::FindReferencesParams {
                root: None,
                index_root: None,
                symbol: "Dup".into(),
                path: None,
                line: None,
                character: None,
                budget_tokens: 2500,
                offset: None,
                dry_run_rename: None,
            },
        )));
        assert!(
            !refs.contains("reusing this session's latest"),
            "must not invent a path from an ambiguous find_symbol: {refs}"
        );
        assert!(
            refs.contains("a/x.go") && refs.contains("b/y.go"),
            "expected ranked candidates: {refs}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_references_inherits_after_path_filter_narrows_to_one() {
        let (server, dir) = smart_server(&[
            ("a/x.go", "package a\n\nfunc Init() {}\n".into()),
            ("b/y.go", "package b\n\nfunc Init() {}\n".into()),
        ]);
        let mut q = find("Init");
        q.path_filter = Some("a/".into());
        let sym = text_of(&server.find_symbol(Parameters(q)));
        assert!(sym.contains("a/x.go"), "{sym}");
        assert!(!sym.contains("b/y.go"), "{sym}");
        let refs = text_of(&server.find_references(Parameters(
            crate::precise_tools::FindReferencesParams {
                root: None,
                index_root: None,
                symbol: "Init".into(),
                path: None,
                line: None,
                character: None,
                budget_tokens: 2500,
                offset: None,
                dry_run_rename: None,
            },
        )));
        assert!(
            refs.contains("reusing this session's latest") && refs.contains("a/x.go"),
            "path_filter-narrowed find_symbol should be inherited: {refs}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_bodies_and_repeat_search_are_deduplicated() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let first = text_of(&server.find_symbol(Parameters(find("compute"))));
        assert!(first.contains("total *= 2"), "{first}");
        let again = text_of(&server.find_symbol(Parameters(find("compute"))));
        assert!(
            (again.contains("already returned") && again.contains("definition body"))
                || again.contains("already returned on tool call"),
            "{again}"
        );
        assert!(!again.contains("total *= 2"), "{again}");
        // A search whose body find_symbol already sent references it.
        let hit = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(hit.contains("already returned on tool call #"), "{hit}");
        // The identical query again is not resent at all.
        let repeat = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(
            repeat.contains("Same query already returned on tool call #"),
            "{repeat}"
        );
        let mut fresh = search("marker_x");
        fresh.dedup = Some(false);
        let full = text_of(&server.search_code(Parameters(fresh)));
        assert!(full.contains("@calc.py:4 marker_x = total - 1"), "{full}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_lists_data_fixture_hits_outside_path_filter() {
        let (server, dir) = smart_server(&[
            (
                "core/frame.rs",
                "pub fn code() -> &'static str {\n    \"E_SHORT\"\n}\n".into(),
            ),
            (
                "proto/vectors/frames.json",
                "[{\"expect_error\": \"E_SHORT\"},\n {\"expect_error\": \"E_SHORT\"}]\n".into(),
            ),
            ("docs/notes.md", "E_SHORT is mentioned in prose\n".into()),
        ]);
        let mut q = search("E_SHORT");
        q.path_filter = Some("core".into());
        let text = text_of(&server.search_code(Parameters(q)));
        assert!(text.contains("@core/frame.rs:2"), "{text}");
        assert!(
            text.contains("Outside path_filter, data/fixture files have 2 more"),
            "{text}"
        );
        assert!(
            text.contains("proto/vectors/frames.json: lines 1,2"),
            "{text}"
        );
        assert!(!text.contains("docs/notes.md"), "{text}");
        let mut rx = search("E_SHORT");
        rx.path_filter = Some("core".into());
        rx.regex = true;
        let regex = text_of(&server.search_code(Parameters(rx)));
        assert!(!regex.contains("data/fixture"), "{regex}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_pointer_carries_signature_and_match_line() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let _ = server.search_code(Parameters(search("marker_x")));
        let again = text_of(&server.search_code(Parameters(search("total *= 2"))));
        assert!(again.contains("@calc.py:3 total *= 2"), "{again}");
        assert!(again.contains("signature: def compute(a, b):"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broad_query_previews_ranked_hits_definitions_first_tests_last() {
        // 70 plain hits in a test file come first alphabetically; the single
        // definition and an ordinary use must outrank them.
        let tests: String = (0..70).map(|i| format!("x{i} = widget_{i}\n")).collect();
        let (server, dir) = smart_server(&[
            ("a_tests/t.py", tests),
            (
                "src/z_def.py",
                "def widget_main():\n    return 1\n\n\ndef other():\n    return widget_main()\n"
                    .into(),
            ),
        ]);
        let text = text_of(&server.search_code(Parameters(search("widget_"))));
        assert!(text.contains("shown=12 omitted=60"), "{text}");
        assert!(text.contains("Wide query: 72 hits"), "{text}");
        let def = text.find("@src/z_def.py:1 def widget_main").expect(&text);
        let call = text.find("@src/z_def.py:6").expect(&text);
        let test = text.find("@a_tests/t.py:1 ").expect(&text);
        assert!(def < call && call < test, "{text}");
        assert!(text.matches("@a_tests/t.py:").count() == 10, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_hit_search_suggests_close_symbols_and_looser_filter() {
        let (server, dir) = smart_server(&[("pkg/sub/calc.py", CALC_PY.into())]);
        let mut p = search("computee");
        p.path_filter = Some("pkg/sub".into());
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(text.contains("(no matching code lines)"), "{text}");
        assert!(text.contains("Closest symbol names"), "{text}");
        assert!(text.contains("def compute(a, b):"), "{text}");
        assert!(text.contains("this literal is absent repo-wide"), "{text}");
        assert!(text.contains("still nothing without path_filter"), "{text}");
        assert!(!text.contains("too narrow"), "{text}");
        // Nothing close: no suggestion block, still the generic hint.
        let none = text_of(&server.search_code(Parameters(search("zzzqqq"))));
        assert!(!none.contains("Closest symbol names"), "{none}");
        assert!(none.contains("find_symbol"), "{none}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_helpers_split_and_measure() {
        assert_eq!(name_tokens("fooBar_baz"), ["foo", "bar", "baz"]);
        assert_eq!(edit_distance("compute", "computee"), 1);
        // Reference: the original allocating DP over chars.
        fn reference(a: &str, b: &str) -> usize {
            let b: Vec<char> = b.chars().collect();
            let mut prev: Vec<usize> = (0..=b.len()).collect();
            for (i, ca) in a.chars().enumerate() {
                let mut cur = vec![i + 1; b.len() + 1];
                for (j, cb) in b.iter().enumerate() {
                    let cost = usize::from(ca != *cb);
                    cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
                }
                prev = cur;
            }
            prev[b.len()]
        }
        let long = "x".repeat(70) + "abc";
        let words = [
            "",
            "a",
            "ab",
            "ba",
            "kitten",
            "sitting",
            "flaw",
            "lawn",
            "café",
            "cafe",
            "日本語",
            "日本",
            "zzqqxx_nomatch",
            "jsonserializer",
            &long,
            "xabcx",
        ];
        for a in words {
            for b in words {
                assert_eq!(edit_distance(a, b), reference(a, b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn group_query_splits_code_and_tests_with_roles() {
        let (server, dir) = smart_server(&[
            (
                "cfg/load.go",
                "package cfg\n\nfunc LoadKey() string {\n\treturn os.Getenv(\"MAGIC_KEY\")\n}\n\nconst keyName = \"MAGIC_KEY\"\n".into(),
            ),
            (
                "cfg/load_test.go",
                "package cfg\n\nfunc TestLoadKey(t *testing.T) {\n\tassert.Equal(t, \"MAGIC_KEY\", keyName)\n}\n\nfunc helperOnly() string {\n\treturn \"MAGIC_KEY\"\n}\n".into(),
            ),
        ]);
        let mut p = search("MAGIC_KEY");
        p.group = Some(true);
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(text.contains("grouped=true total=4"), "{text}");
        let prod = text.find("## prod (2 hits / 1 file)").expect(&text);
        let tests = text.find("## test (2 hits / 1 file)").expect(&text);
        assert!(prod < tests, "{text}");
        assert!(text.contains("[load] in LoadKey"), "{text}");
        assert!(text.contains("[def]"), "{text}");
        assert!(text.contains("[assert] in TestLoadKey"), "{text}");
        assert!(text.contains("[ref] in helperOnly"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hit_roles_are_text_heuristics() {
        assert_eq!(hit_role(true, "require.Equal(t, a, b)"), "assert");
        assert_eq!(hit_role(true, "x := \"lit\""), "ref");
        assert_eq!(hit_role(false, "v := os.Getenv(\"K\")"), "load");
        assert_eq!(hit_role(false, "const K = \"v\""), "def");
        assert_eq!(hit_role(false, "log(\"v\")"), "use");
    }

    #[test]
    fn hit_roles_use_lexer_for_trailing_comments_and_raw_strings() {
        let go: Vec<String> = [
            "package p",
            "func f() {",
            "\ts := `",
            "\tx := 1",
            "\t`",
            "\treturn s // a := b",
            "}",
        ]
        .iter()
        .map(|l| l.to_string())
        .collect();
        let v = role_views("a.go", &go, &[3, 5]).unwrap();
        // inside a Go raw string: not a definition
        assert_eq!(hit_role_view(false, &go[3], &v[&3]), "string");
        // trailing comment words do not leak into the label
        assert_eq!(hit_role_view(false, &go[5], &v[&5]), "use");
        let py: Vec<String> = [
            "def g(thread):",
            "    return thread  # read SysLog(x)",
            "    # SysLog(x)",
            "    x = 1  # note",
        ]
        .iter()
        .map(|l| l.to_string())
        .collect();
        let v = role_views("b.py", &py, &[1, 2]).unwrap();
        assert_eq!(hit_role_view(false, &py[1], &v[&1]), "use");
        assert_eq!(hit_role_view(false, &py[2], &v[&2]), "comment");
        // no lexer: falls back to text heuristic
        assert!(role_views("a.txt", &py, &[0]).is_none());
        // string literal keys still see strings
        let rs: Vec<String> = vec!["let k = \"v\"; // read".into()];
        let v = role_views("a.rs", &rs, &[0]).unwrap();
        assert_eq!(hit_role_view(false, &rs[0], &v[&0]), "def");
    }

    #[test]
    fn hit_roles_tag_comments_and_definitions() {
        assert_eq!(hit_role(false, "// see MAGIC_KEY here"), "comment");
        assert_eq!(hit_role(false, "    # MAGIC_KEY note"), "comment");
        assert_eq!(hit_role(false, " * MAGIC_KEY javadoc"), "comment");
        assert_eq!(hit_role(true, "// want MAGIC_KEY"), "comment");
        assert_eq!(hit_role(false, "pub fn magic_key() -> u8 {"), "def");
        assert_eq!(hit_role(false, "func MagicKey() string {"), "def");
        assert_eq!(hit_role(false, "def magic_key(self):"), "def");
        assert_eq!(hit_role(false, "log(\"MAGIC_KEY\")"), "use");
        // `#define` / `#[attr]` are not comments.
        assert_eq!(hit_role(false, "#define MAGIC_KEY 1"), "def");
        assert_ne!(hit_role(false, "#[cfg(MAGIC_KEY)]"), "comment");
    }

    #[test]
    fn grouped_output_has_no_recheck_wording_and_closes_with_summary() {
        let rows: String = (0..9)
            .map(|i| format!("func f{i}() {{ use(\"CLOSE_KEY\") }}\n"))
            .collect();
        let (server, dir) = smart_server(&[
            ("a/one.go", format!("package a\n{rows}")),
            (
                "b/two.go",
                "package b\n// CLOSE_KEY doc\nconst k = \"CLOSE_KEY\"\n".into(),
            ),
        ]);
        let mut p = search("CLOSE_KEY");
        p.group = Some(true);
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(!text.contains("needs verification"), "{text}");
        assert!(!text.contains("text heuristic"), "{text}");
        assert!(text.contains("[comment]"), "{text}");
        assert!(text.contains("[def]"), "{text}");
        // 11 hits <= GROUP_FULL_HITS: all 9 rows of one.go are listed.
        assert_eq!(text.matches("  L").count(), 11, "{text}");
        assert!(!text.contains("not listed"), "{text}");
        assert!(
            text.contains("Summary: 11 lines / 2 files, fully listed."),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grouped_output_over_cap_says_incomplete_per_file() {
        let rows: String = (0..50)
            .map(|i| format!("func f{i}() {{ use(\"WIDE_KEY\") }}\n"))
            .collect();
        let (server, dir) = smart_server(&[("a/one.go", format!("package a\n{rows}"))]);
        let mut p = search("WIDE_KEY");
        p.group = Some(true);
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(
            text.contains("this file has 44 more hits not listed"),
            "{text}"
        );
        assert!(
            text.contains("incomplete: narrow with path_filter=\"a/one.go\""),
            "{text}"
        );
        assert!(!text.contains("Fully listed"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broad_summary_points_to_page_true_when_incomplete() {
        let rows: String = (0..70)
            .map(|i| format!("func f{i}() {{ use(\"BROAD_KEY\") }}\n"))
            .collect();
        let (server, dir) = smart_server(&[("a/one.go", format!("package a\n{rows}"))]);
        let text = text_of(&server.search_code(Parameters(search("BROAD_KEY"))));
        assert!(
            text.contains(
                "incomplete: narrow with path_filter=\"a/one.go\", or page=true then offset="
            ),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flat_results_end_with_completeness_line() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let text = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(
            text.contains("Summary: 2 lines / 1 file, fully listed."),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn counting_fixture() -> (AstrolabeServer, PathBuf) {
        smart_server(&[
            (
                "svc/a.go",
                "package svc\nfunc A() { SysLog(1)\n SysLog(2) }\n".into(),
            ),
            ("svc/b.go", "package svc\nfunc B() { SysLog(3) }\n".into()),
            ("web/c.go", "package web\nfunc C() { SysLog(4) }\n".into()),
            (
                "vendor/x/d.go",
                "package x\nfunc D() { SysLog(5) }\n".into(),
            ),
            (
                "svc/a_test.go",
                "package svc\nfunc TestA() { SysLog(6) }\n".into(),
            ),
        ])
    }

    #[test]
    fn aggregate_dir_counts_hits_per_directory_with_total() {
        let (server, dir) = counting_fixture();
        let mut p = search("SysLog(");
        p.aggregate = Some("dir".into());
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(
            text.contains("total 6 hit lines / 5 files / 3 dirs / 3 top-level dirs"),
            "{text}"
        );
        assert!(text.contains("per directory: N lines / M files"), "{text}");
        assert!(text.contains("    4 lines /    3 files  svc/"), "{text}");
        assert!(text.contains("    1 lines /    1 files  web/"), "{text}");
        assert!(
            text.contains("    1 lines /    1 files  vendor/x/"),
            "{text}"
        );
        assert!(text.contains("    6 lines /    5 files  total"), "{text}");
        assert!(!text.contains("@svc/a.go"), "no hit lines: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_file_and_count_only_and_validation() {
        let (server, dir) = counting_fixture();
        let mut p = search("SysLog(");
        p.aggregate = Some("file".into());
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(
            text.contains("    2 lines /    1 files  svc/a.go"),
            "{text}"
        );
        assert!(
            text.contains("    1 lines /    1 files  web/c.go"),
            "{text}"
        );
        let mut c = search("SysLog(");
        c.count_only = true;
        let text = text_of(&server.search_code(Parameters(c)));
        assert!(text.contains("total 6 hit lines / 5 files"), "{text}");
        assert!(!text.contains("total\n"), "count_only has no rows: {text}");
        let mut bad = search("SysLog(");
        bad.aggregate = Some("package".into());
        let text = text_of(&server.search_code(Parameters(bad)));
        assert!(
            text.contains("aggregate must be top, dir, or file"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_file_over_row_cap_pages_with_offset_and_says_so() {
        let files: Vec<(String, String)> = (0..95)
            .map(|i| {
                (
                    format!("pkg/f{i:03}.go"),
                    format!("package pkg\nfunc F{i}() {{ SysLog({i}) }}\n"),
                )
            })
            .collect();
        let refs: Vec<(&str, String)> =
            files.iter().map(|(p, c)| (p.as_str(), c.clone())).collect();
        let (server, dir) = smart_server(&refs);
        let mut p = search("SysLog(");
        p.aggregate = Some("file".into());
        let first = text_of(&server.search_code(Parameters(p.clone())));
        assert!(first.contains("total 95 hit lines / 95 files"), "{first}");
        assert!(first.contains("15 more items"), "{first}");
        assert!(first.contains("offset=80"), "{first}");
        assert!(first.contains("pkg/f000.go"), "{first}");
        assert!(!first.contains("pkg/f094.go"), "{first}");
        p.offset = 80;
        let second = text_of(&server.search_code(Parameters(p)));
        assert!(second.contains("pkg/f094.go"), "{second}");
        assert!(!second.contains("pkg/f000.go"), "{second}");
        assert!(second.contains("from item 81"), "{second}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_code_scan_keeps_graph_order_and_is_repeatable() {
        let files: Vec<(String, String)> = (0..60)
            .map(|i| {
                (
                    format!("d{}/f{i:02}.go", i % 7),
                    format!("package p\nfunc F{i}() {{ SysLog({i}) }}\n// SysLog(c{i})\n"),
                )
            })
            .collect();
        let refs: Vec<(&str, String)> =
            files.iter().map(|(p, c)| (p.as_str(), c.clone())).collect();
        let (server, dir) = smart_server(&refs);
        let mut p = search("SysLog(");
        p.dedup = Some(false);
        p.budget_tokens = 20000;
        let first = text_of(&server.search_code(Parameters(p.clone())));
        // The loop guard blocks the 4th identical call, so stop at 3.
        let first = once_free(&first);
        for _ in 0..2 {
            assert_eq!(text_of(&server.search_code(Parameters(p.clone()))), first);
        }
        let mut c = p.clone();
        c.skip_comments = true;
        let skipped = text_of(&server.search_code(Parameters(c)));
        assert!(
            skipped.contains("skipped 60 hits in comments/strings/docs"),
            "{skipped}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn nested_fixture() -> (AstrolabeServer, PathBuf) {
        smart_server(&[
            (
                "svc/a.go",
                "package svc\n// SysLog(0) note\nfunc A() { SysLog(1)\n s := \"SysLog(2)\"\n /* SysLog(3)\n SysLog(4) */ SysLog(5) }\n".into(),
            ),
            ("svc/sub/b.go", "package sub\nfunc B() { SysLog(6) }\n".into()),
            ("svc/b_test.go", "package svc\nfunc TestB() { SysLog(7) }\n".into()),
            ("web/c.go", "package web\nfunc C() { SysLog(8) }\n".into()),
            ("main.go", "package main\nfunc main() { SysLog(9) }\n".into()),
            ("docs/guide.md", "call SysLog( like this\n".into()),
        ])
    }

    #[test]
    fn aggregate_top_reports_hits_and_files_per_top_dir() {
        let (server, dir) = nested_fixture();
        let mut p = search("SysLog(");
        p.aggregate = Some("top".into());
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(
            text.contains("per top-level dir: N lines / M files"),
            "{text}"
        );
        assert!(text.contains("    7 lines /    3 files  svc/"), "{text}");
        assert!(text.contains("    1 lines /    1 files  web/"), "{text}");
        assert!(text.contains("    1 lines /    1 files  ./"), "{text}");
        assert!(text.contains("   10 lines /    6 files  total"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skip_comments_drops_comment_string_and_doc_hits_and_combines() {
        let (server, dir) = nested_fixture();
        let mut p = search("SysLog(");
        p.aggregate = Some("top".into());
        p.skip_comments = true;
        p.exclude = Some(vec!["_test.go".into()]);
        let text = text_of(&server.search_code(Parameters(p)));
        // a.go keeps the SysLog(1) line and the `*/ SysLog(5)` line.
        assert!(text.contains("    3 lines /    2 files  svc/"), "{text}");
        assert!(text.contains("    5 lines /    4 files  total"), "{text}");
        assert!(!text.contains("docs/"), "{text}");
        assert!(
            text.contains("skip_comments: skipped 4 hits in comments/strings/docs"),
            "{text}"
        );
        // Without the flag the same query counts them.
        let mut q = search("SysLog(");
        q.aggregate = Some("file".into());
        let text = text_of(&server.search_code(Parameters(q)));
        assert!(
            text.contains("    5 lines /    1 files  svc/a.go"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skip_comments_applies_to_text_only_files_and_keeps_build_txt() {
        let (server, dir) = smart_server(&[
            (
                "main.go",
                "package main\nfunc main() { SysLog(1) }\n".into(),
            ),
            (
                "cfg/app.yaml",
                "# SysLog(yaml comment)\nk: SysLog(2)\n".into(),
            ),
            (
                "cfg/q.sql",
                "-- SysLog(sql comment)\nselect SysLog(3);\n".into(),
            ),
            (
                "cfg/k.kt",
                "// SysLog(kt comment)\nfun f() { SysLog(4) }\n".into(),
            ),
            (
                "cfg/CMakeLists.txt",
                "add_executable(SysLog( main.c)\n".into(),
            ),
            ("notes/README.txt", "call SysLog( here\n".into()),
            ("cfg/a.hs", "-- SysLog(hs comment)\nx = SysLog(5)\n".into()),
        ]);
        let mut p = search("SysLog(");
        p.skip_comments = true;
        p.dedup = Some(false);
        let text = text_of(&server.search_code(Parameters(p)));
        for kept in [
            "SysLog(1)",
            "k: SysLog(2)",
            "select SysLog(3)",
            "SysLog(4)",
            "add_executable(SysLog(",
        ] {
            assert!(text.contains(kept), "{kept} missing: {text}");
        }
        for dropped in [
            "yaml comment",
            "sql comment",
            "kt comment",
            "call SysLog( here",
        ] {
            assert!(!text.contains(dropped), "{dropped} kept: {text}");
        }
        // No lexer for .hs: hits stay, and the summary says so.
        assert!(text.contains("hs comment"), "{text}");
        assert!(
            text.contains("comments not skipped") && text.contains(".hs"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_doc_path_txt_only_for_readme_like_names() {
        assert!(is_doc_path("README.txt"));
        assert!(is_doc_path("a/NOTES.txt"));
        assert!(is_doc_path("docs/x.yaml"));
        assert!(is_doc_path("a/b.md"));
        assert!(!is_doc_path("cfg/CMakeLists.txt"));
        assert!(!is_doc_path("requirements.txt"));
    }

    #[test]
    fn bare_exclude_name_is_whole_path_segment_for_dirs_and_files_alike() {
        let set = build_exclude_set(Some(&["test".into()])).unwrap().unwrap();
        assert!(set.is_match("test/a.rs"));
        assert!(set.is_match("src/test/a.rs"));
        assert!(set.is_match("src/test"));
        assert!(!set.is_match("src/contest/a.rs"));
        assert!(!set.is_match("src/test.rs"));
        assert!(!set.is_match("src/tests/a.rs"));
    }

    #[test]
    fn exclude_accepts_bare_suffix_glob_and_directory_names() {
        let set = build_exclude_set(Some(&[
            "_test.go".into(),
            "vendor".into(),
            "*.min.js".into(),
        ]))
        .unwrap()
        .unwrap();
        assert!(set.is_match("svc/a_test.go"));
        assert!(set.is_match("a_test.go"));
        assert!(set.is_match("x/vendor/y.go"));
        assert!(set.is_match("web/app.min.js"));
        assert!(!set.is_match("svc/a.go"));
    }

    #[test]
    fn many_hits_get_next_step_hint_and_docs_are_listed_apart() {
        let rows: String = (0..45)
            .map(|i| format!("func f{i}() {{ HINTKEY({i}) }}\n"))
            .collect();
        let (server, dir) = smart_server(&[
            ("a/one.go", format!("package a\n{rows}")),
            ("b/two.go", "package b\nfunc g() { HINTKEY(1) }\n".into()),
            ("c/three.go", "package c\nfunc h() { HINTKEY(2) }\n".into()),
            ("d/four.go", "package d\nfunc i() { HINTKEY(3) }\n".into()),
            ("docs/x.md", "HINTKEY docs\n".into()),
        ]);
        let mut p = search("HINTKEY");
        p.group = Some(true);
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(
            text.contains("Next: to count") && text.contains("per directory"),
            "{text}"
        );
        assert!(text.contains("aggregate=top|dir|file"), "{text}");
        assert!(text.contains("## doc (1 hit / 1 file)"), "{text}");
        assert!(text.contains("doc 1"), "{text}");
        assert!(text.contains("[doc]"), "{text}");
        let prod = text.find("## prod").expect(&text);
        assert!(prod < text.find("## doc").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_globs_skip_paths_in_lists_and_counts() {
        let (server, dir) = counting_fixture();
        let mut p = search("SysLog(");
        p.aggregate = Some("dir".into());
        p.exclude = Some(vec!["vendor".into(), "*_test.go".into()]);
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(
            text.contains("total 4 hit lines / 3 files / 2 dirs / 2 top-level dirs"),
            "{text}"
        );
        assert!(!text.contains("vendor"), "{text}");
        let mut q = search("SysLog(");
        q.exclude = Some(vec!["**/web/**".into(), "svc/b.go".into()]);
        let text = text_of(&server.search_code(Parameters(q)));
        assert!(!text.contains("web/c.go"), "{text}");
        assert!(!text.contains("svc/b.go"), "{text}");
        assert!(text.contains("svc/a.go"), "{text}");
        let mut bad = search("SysLog(");
        bad.exclude = Some(vec!["[".into()]);
        let text = text_of(&server.search_code(Parameters(bad)));
        assert!(text.contains("glob"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeat_query_pointer_returns_first_result_digest() {
        let (server, dir) = counting_fixture();
        let mut p = search("SysLog(");
        p.group = Some(false);
        let _first = server.search_code(Parameters(p.clone()));
        let again = text_of(&server.search_code(Parameters(p)));
        assert!(again.contains("not resending full text"), "{again}");
        assert!(
            again.contains("First-result digest: 6 hit lines / 5 files"),
            "{again}"
        );
        assert!(again.contains("svc/a.go×2"), "{again}");
        assert!(again.contains("first svc/a.go:2"), "{again}");
        assert!(again.contains("dedup=false"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_context_next_step_hint_matches_task() {
        assert!(next_step_hint("rename Foo to Bar everywhere").contains("dry_run_rename"));
        assert!(next_step_hint("count calls per directory").contains("aggregate=top|dir|file"));
        assert!(next_step_hint("who calls parse").contains("find_references"));
        assert!(next_step_hint("add a flag").contains("find_symbol"));
        for t in ["rename x", "how many hits", "callers of x", "other"] {
            let h = next_step_hint(t);
            assert!(h.starts_with("Next:") && h.ends_with('\n') && h.lines().count() == 1);
        }
    }

    #[test]
    fn resolve_context_output_ends_with_next_step_line() {
        let server = ready_server();
        let result = server.resolve_context(Parameters(ResolveContextParams {
            root: None,
            index_root: None,
            task_description: "rename alpha".into(),
            budget_tokens: 2000,
            dedup: None,
            offset: 0,
            path_filter: None,
        }));
        let text = text_of(&result);
        let last = text.trim_end().lines().last().unwrap_or_default();
        assert!(last.starts_with("Next:"), "{text}");
    }

    fn rc(task: &str, budget: usize, dedup: Option<bool>) -> ResolveContextParams {
        ResolveContextParams {
            root: None,
            index_root: None,
            task_description: task.into(),
            budget_tokens: budget,
            dedup,
            offset: 0,
            path_filter: None,
        }
    }

    fn many_file_server() -> (AstrolabeServer, PathBuf) {
        let mut files: Vec<(String, String)> = (0..40)
            .map(|i| {
                (
                    format!("pkg/mod{i}.py"),
                    format!(
                        "def filler_{i}():\n    return {i}\n\n\ndef other_{i}():\n    return 1\n"
                    ),
                )
            })
            .collect();
        files.push((
            "pkg/loop_guard.py".into(),
            "def other_a():\n    return 1\n\n\ndef repeat_budget_check():\n    return 2\n".into(),
        ));
        let refs: Vec<(&str, String)> =
            files.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
        smart_server(&refs)
    }

    #[test]
    fn resolve_context_pages_via_offset_and_warns_when_index_changed() {
        let files: Vec<(String, String)> = (0..80)
            .map(|i| {
                (
                    format!("pkg{}/mod{i}.py", i % 4),
                    format!("def widget_handler_{i}():\n    return {i}\n"),
                )
            })
            .collect();
        let refs: Vec<(&str, String)> =
            files.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
        let (server, dir) = smart_server(&refs);
        let task = "fix the widget handler";
        let page = |offset: usize| {
            let mut p = rc(task, 2500, Some(false));
            p.offset = offset;
            text_of(&server.resolve_context(Parameters(p)))
        };
        let first = page(0);
        assert!(first.contains("Matched 80 files"), "{first}");
        assert!(first.contains("(offset=25)"), "{first}");
        assert!(!first.contains("while paging"), "{first}");
        let second = page(25);
        assert!(second.contains("this page items 26–50"), "{second}");
        assert!(!second.contains("while paging"), "{second}");
        let p1: BTreeSet<&str> = first.lines().filter(|l| l.starts_with('@')).collect();
        assert!(second
            .lines()
            .filter(|l| l.starts_with('@'))
            .all(|l| !p1.contains(l)));
        *server.primary.state.write().unwrap() =
            IndexState::Ready(Arc::new(crate::index::build(&dir).expect("index")));
        let drifted = page(50);
        assert!(drifted.contains("while paging"), "{drifted}");
        assert!(!page(50).contains("while paging"));
        // path_filter narrows instead of paging.
        let mut p = rc(task, 2500, Some(false));
        p.path_filter = Some("pkg1/".into());
        let narrowed = text_of(&server.resolve_context(Parameters(p)));
        assert!(narrowed.contains("Matched 20 files"), "{narrowed}");
        assert!(narrowed
            .lines()
            .filter(|l| l.starts_with('@'))
            .all(|l| l.starts_with("@pkg1/")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn go_iface_repo() -> Vec<(String, String)> {
        let mut files: Vec<(String, String)> = vec![
            ("go.mod".into(), "module example.com/m\n".into()),
            (
                "oauth/provider.go".into(),
                "package oauth\n\ntype Provider interface {\n\tGetName() string\n\tLogin(code string) error\n}\n".into(),
            ),
            (
                "oauth/github.go".into(),
                "package oauth\n\ntype GitHubProvider struct{}\n\nfunc (p *GitHubProvider) GetName() string { return \"gh\" }\nfunc (p *GitHubProvider) Login(code string) error { return nil }\n".into(),
            ),
            (
                "oauth/types.go".into(),
                "package oauth\n\ntype Token struct {\n\tValue string\n}\n".into(),
            ),
        ];
        // Enough noise that page 1 is ranked, not the whole repo.
        for i in 0..70 {
            files.push((
                format!("svc{}/f{i}.go", i % 7),
                format!(
                    "package svc{}\n\nfunc ProviderInterfaceHelper{i}() int {{ return {i} }}\n",
                    i % 7
                ),
            ));
        }
        files
    }

    #[test]
    fn resolve_context_lists_go_implementors_and_package_after_the_interface() {
        let files = go_iface_repo();
        let refs: Vec<(&str, String)> =
            files.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
        let (server, dir) = smart_server(&refs);
        let text = text_of(&server.resolve_context(Parameters(rc(
            "which types implement the `Provider` interface in oauth/provider.go?",
            2500,
            Some(false),
        ))));
        let order: Vec<&str> = text.lines().filter(|l| l.starts_with('@')).collect();
        let pos = |p: &str| order.iter().position(|l| l.starts_with(p));
        assert_eq!(pos("@oauth/provider.go"), Some(0), "{text}");
        assert_eq!(pos("@oauth/github.go"), Some(1), "{text}");
        assert_eq!(pos("@oauth/types.go"), Some(2), "{text}");
        assert!(text.contains("GitHubProvider struct"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_lists_go_implementors_of_a_shown_interface() {
        let files = go_iface_repo();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (server, dir) = server_on_files(&refs);
        let text = fs_text(&server.find_symbol(Parameters(fs_query("Provider"))));
        assert!(
            text.contains("Go implicit implementors")
                && text.contains("GitHubProvider @oauth/github.go:3"),
            "{text}"
        );
        // A struct hit lists nothing extra.
        let text = fs_text(&server.find_symbol(Parameters(fs_query("Token"))));
        assert!(!text.contains("Go implicit implementors"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_context_ranks_symbols_and_files_by_task_words() {
        let (server, dir) = many_file_server();
        let text = text_of(&server.resolve_context(Parameters(rc(
            "where is repeat_budget_check in the loop guard",
            2500,
            None,
        ))));
        let first_file = text
            .lines()
            .find(|l| l.starts_with('@'))
            .unwrap_or_default();
        assert!(first_file.contains("loop_guard.py"), "{text}");
        assert!(text.contains("repeat_budget_check"), "{text}");
        // The unrelated first symbol of the file is not listed.
        assert!(!text.contains("other_a"), "{text}");
        assert!(!text.contains("filler_"), "{text}");
        assert!(text.contains("confidence: Scoped"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_context_without_match_says_so_and_stays_small() {
        let (server, dir) = many_file_server();
        let text = text_of(&server.resolve_context(Parameters(rc(
            "zzzz qqqq nonexistent thing",
            2500,
            None,
        ))));
        assert!(text.contains("No task-related"), "{text}");
        assert!(!text.contains("confidence: Scoped"), "{text}");
        assert!(
            text.matches("\n@").count() + usize::from(text.starts_with('@')) <= 6,
            "{text}"
        );
        assert!(text.len() < 3_000, "{} bytes", text.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_context_counting_task_is_short() {
        let (server, dir) = many_file_server();
        let text = text_of(&server.resolve_context(Parameters(rc(
            "how many callers of filler_3 per directory",
            2500,
            None,
        ))));
        assert!(text.len() < 3_000, "{} bytes\n{text}", text.len());
        assert!(text.contains("find_references"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_context_repeat_returns_pointer_with_top_files_and_dedup_hint() {
        let (server, dir) = many_file_server();
        let task = "where is repeat_budget_check in the loop guard";
        let first = text_of(&server.resolve_context(Parameters(rc(task, 2500, None))));
        let again = text_of(&server.resolve_context(Parameters(rc(task, 2500, None))));
        assert!(again.contains("not resending full text"), "{again}");
        assert!(again.contains("pkg/loop_guard.py"), "{again}");
        assert!(again.contains("dedup=false"), "{again}");
        let full = text_of(&server.resolve_context(Parameters(rc(task, 2500, Some(false)))));
        assert_eq!(
            full.contains("repeat_budget_check"),
            first.contains("repeat_budget_check")
        );
        assert!(!full.contains("not resending full text"), "{full}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_step_hint_uses_word_boundaries_and_caller_priority() {
        assert!(next_step_hint("account balance count").contains("find_symbol"));
        assert!(next_step_hint("the country discount").contains("find_symbol"));
        assert!(next_step_hint("how many callers of resolve_context").contains("find_references"));
        assert!(next_step_hint("how many calls per directory").contains("aggregate"));
        assert!(next_step_hint("count the usages of foo").contains("find_references"));
        assert!(next_step_hint("count TODO comments").contains("aggregate"));
        assert!(next_step_hint("rename account to user").contains("dry_run_rename"));
        assert!(next_step_hint("count occurrences of foo").contains("aggregate"));
        assert!(next_step_hint("who calls parse").contains("find_references"));
    }

    #[test]
    fn search_dedup_pointer_has_real_call_number_and_sample_hits() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let first = server.call_seq.load(Ordering::Relaxed);
        let _ = server.search_code(Parameters(search("marker_x")));
        let second_seq_before = server.call_seq.load(Ordering::Relaxed);
        assert_eq!(second_seq_before, first + 1, "search must take an ordinal");
        let again = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(
            again.contains(&format!("tool call #{}", first + 1)),
            "{again}"
        );
        assert!(again.contains("first hits: @calc.py:"), "{again}");
        assert!(again.contains("compressed"), "{again}");
        assert!(again.contains("dedup=false"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tools_list_stays_compact() {
        let server = ready_server();
        let tools = server.listed_tools();
        assert_eq!(tools.len(), 7);
        let bytes = serde_json::to_vec(&tools).unwrap().len();
        assert!(bytes < 8_000, "tools/list grew to {bytes} bytes");
    }

    #[test]
    fn dedup_marks_only_bodies_that_actually_shipped() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        // Budget too small for the body: falls back to line context.
        let mut tiny = search("marker_x");
        tiny.budget_tokens = 1;
        let first = text_of(&server.search_code(Parameters(tiny)));
        assert!(!first.contains("── function compute"), "{first}");
        // The body never shipped, so it must not be replaced by a pointer.
        let second = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(
            second.contains("── function compute · calc.py:1-5 ──"),
            "{second}"
        );
        assert!(!second.contains("already this session"), "{second}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_auto_groups_when_hits_span_more_than_three_files() {
        let files: Vec<(String, String)> = (0..4)
            .map(|i| (format!("pkg/f{i}.py"), format!("def f{i}():\n    return 'AUTO_KEY'\n\n\ndef g{i}():\n    return 'AUTO_KEY'\n")))
            .collect();
        let refs: Vec<(&str, String)> =
            files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
        let (server, dir) = smart_server(&refs);
        let text = text_of(&server.search_code(Parameters(search("AUTO_KEY"))));
        assert!(text.contains("grouped=true total=8"), "{text}");
        // Explicit false forces the plain list.
        let mut plain = search("AUTO_KEY");
        plain.group = Some(false);
        let plain = text_of(&server.search_code(Parameters(plain)));
        assert!(!plain.contains("grouped=true"), "{plain}");
        // Few files: no auto grouping.
        let (server2, dir2) = smart_server(&refs[..3]);
        let three = text_of(&server2.search_code(Parameters(search("AUTO_KEY"))));
        assert!(!three.contains("grouped=true"), "{three}");
        // Few hits (<=5) keep enclosing bodies even across many files.
        let (server3, dir3) = smart_server(&refs);
        let few = text_of(&server3.search_code(Parameters(search("def f"))));
        assert!(!few.contains("grouped=true"), "{few}");
        for d in [dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn tool_descriptions_carry_query_and_paging_guidance() {
        let full = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            crate::ClientContext::default_builtin(),
        );
        let tools = full.listed_tools();
        let desc = |n: &str| {
            tools
                .iter()
                .find(|t| t.name == n)
                .unwrap()
                .description
                .as_deref()
                .unwrap()
                .to_string()
        };
        let search_code = desc("search_code");
        assert!(
            search_code.contains("Exhaustive code & text search"),
            "{search_code}"
        );
        assert!(
            search_code.contains("aggregate=top|dir|file"),
            "{search_code}"
        );
        // Paging was never used in 66 A/B runs; descriptions no longer teach it.
        assert!(!search_code.contains("offset=next_offset"), "{search_code}");
        let find_symbol = desc("find_symbol");
        assert!(find_symbol.contains("include_body"), "{find_symbol}");
        assert!(!find_symbol.contains("offset=next_offset"), "{find_symbol}");
        let find_references = desc("find_references");
        // One source of truth: index-first, language server as fallback.
        assert!(
            !find_references.contains("LSP-resolved"),
            "{find_references}"
        );
        assert!(find_references.contains("index first"), "{find_references}");
        assert!(find_references.contains("rename"), "{find_references}");
        assert!(
            find_references.contains("dry_run_rename"),
            "{find_references}"
        );
        let goto_definition = desc("goto_definition");
        assert!(goto_definition.contains("definition"), "{goto_definition}");
    }

    #[test]
    fn cache_budget_env_parsing() {
        assert_eq!(parse_cache_budget_mb(None), DEFAULT_MEMORY_BUDGET_BYTES);
        assert_eq!(parse_cache_budget_mb(Some("8")), 8 * 1024 * 1024);
        assert_eq!(parse_cache_budget_mb(Some("256")), 256 * 1024 * 1024);
        assert_eq!(parse_cache_budget_mb(Some(" 4 ")), 4 * 1024 * 1024);
        assert_eq!(
            parse_cache_budget_mb(Some("nope")),
            DEFAULT_MEMORY_BUDGET_BYTES
        );
        assert_eq!(parse_cache_budget_mb(Some("0")), 0);
    }

    #[test]
    fn get_languages_does_not_repeat_the_per_query_coverage_note() {
        let (server, dir) = smart_server(&[
            ("ok.py", "def fine():\n    return 1\n".to_string()),
            ("bad.py", "def broken(:\n    pass\n".to_string()),
        ]);
        let note = {
            let state = server.primary.state.read().unwrap();
            let IndexState::Ready(index) = &*state else {
                panic!("index not ready")
            };
            index.coverage_note().expect("partial parse yields a note")
        };
        let languages = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 1000,
            host_lsp: None,
        }));
        let text = languages.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("syntax/compile errors: 1"), "{text}");
        assert!(
            !text.contains(&note),
            "note duplicated in get_languages: {text}"
        );
        let search = server.search_code(Parameters(search("fine")));
        let search_text = search.content[0].as_text().unwrap().text.as_str();
        assert!(
            search_text.contains(&note),
            "other tools keep the note: {search_text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_languages_reports_file_cache_occupancy() {
        let server = ready_server();
        let result = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 100,
            host_lsp: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("file_cache:"),
            "get_languages should expose cache occupancy: {text}"
        );
        assert!(
            text.contains(&format!("/{} bytes", DEFAULT_MEMORY_BUDGET_BYTES)),
            "cache budget should appear in text: {text}"
        );
        assert!(result.structured_content.is_none());
    }

    /// Native-only host LSP list overrides the PATH-probe column for covered
    /// languages and appends the registration inventory.
    #[test]
    fn get_languages_host_lsp_overrides_the_path_probe_column() {
        let server = ready_server();
        let result = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 2000,
            host_lsp: Some(vec![
                HostLspServer {
                    name: "rust-analyzer".into(),
                    command: "rust-analyzer".into(),
                    languages: vec!["rust".into()],
                    running: true,
                },
                HostLspServer {
                    name: "pyright".into(),
                    command: "pyright-langserver".into(),
                    languages: vec!["python".into()],
                    running: false,
                },
            ]),
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("Host LSP servers"),
            "host inventory section: {text}"
        );
        assert!(
            text.contains("rust-analyzer [rust-analyzer] running"),
            "{text}"
        );
        assert!(
            text.contains("pyright [pyright-langserver] configured"),
            "{text}"
        );
        assert!(text.contains("Ready (host: rust-analyzer)"), "{text}");
        assert!(
            text.contains("LSP status: Ready = host language server running"),
            "legend follows the host pool: {text}"
        );
    }

    /// Empty host list keeps the PATH-probe column (astrolabe fallback).
    #[test]
    fn get_languages_without_host_lsp_keeps_the_path_probe() {
        let server = ready_server();
        let result = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 2000,
            host_lsp: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            !text.contains("Host LSP servers"),
            "no host section without host_lsp: {text}"
        );
        assert!(
            text.contains("LSP status: Ready = server on PATH"),
            "path-probe legend: {text}"
        );
    }

    /// The "N 个文件有语法错误" footnote cost 7.2 KB over 87 of 223 results in
    /// the paging A/B: it is shown once per session (and when it changes),
    /// on empty results, and as a named line when a hit is in an error file.
    #[test]
    fn coverage_note_only_when_relevant() {
        let (server, dir) = smart_server(&[
            ("ok.py", "def fine():\n    return 1\n".to_string()),
            (
                "bad.py",
                "def kept_marker():\n    return 2\n\ndef broken(:\n    pass\n".to_string(),
            ),
        ]);
        let note = {
            let state = server.primary.state.read().unwrap();
            let IndexState::Ready(index) = &*state else {
                panic!("index not ready")
            };
            index.coverage_note().expect("partial parse yields a note")
        };
        let first = text_of(&server.search_code(Parameters(search("fine"))));
        assert!(first.contains(&note), "first result carries it: {first}");
        let second = text_of(&server.search_code(Parameters(search("return 1"))));
        assert!(second.contains("ok.py"), "{second}");
        assert!(!second.contains(&note), "not repeated: {second}");
        assert!(!second.contains("have syntax errors"), "{second}");
        let empty = text_of(&server.search_code(Parameters(search("absent_zz_token"))));
        assert!(empty.contains("(no matching code lines)"), "{empty}");
        assert!(empty.contains(&note), "empty result keeps it: {empty}");
        let hit = text_of(&server.search_code(Parameters(search("kept_marker"))));
        assert!(
            hit.contains("hit file bad.py has syntax errors"),
            "error-file hit is named: {hit}"
        );
        assert!(!hit.contains(&note), "{hit}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_uses_file_cache_and_records_hits() {
        let (server, dir) = ready_server_with_disk();
        let params = SearchParams {
            root: None,
            index_root: None,
            query: "cache-me".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: Some(false),
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        };

        let first = server.search_code(Parameters(params.clone()));
        let first_text = first.content[0].as_text().unwrap().text.as_str();
        assert!(first_text.contains("cache-me"), "{first_text}");
        assert!(server.primary.cache_misses.load(Ordering::Relaxed) >= 1);
        assert_eq!(server.primary.cache_hits.load(Ordering::Relaxed), 0);

        let second = server.search_code(Parameters(params));
        let second_text = second.content[0].as_text().unwrap().text.as_str();
        assert!(second_text.contains("cache-me"), "{second_text}");
        assert!(
            server.primary.cache_hits.load(Ordering::Relaxed) >= 1,
            "second search of an unchanged file must hit"
        );

        let languages = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 200,
            host_lsp: None,
        }));
        let languages_text = languages.content[0].as_text().unwrap().text.as_str();
        assert!(languages_text.contains("hits="), "{languages_text}");
        assert!(
            languages_text.contains(&format!("/{} bytes", 8 * 1024 * 1024)),
            "{languages_text}"
        );
        assert!(languages.structured_content.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_file_cache_stays_within_byte_budget() {
        let dir = unique_temp_dir();
        let payload = format!("payload = '{}'\n", "y".repeat(2_000));
        for index in 0..80 {
            std::fs::write(dir.join(format!("f{index}.py")), &payload).unwrap();
        }
        const BUDGET: u64 = 32 * 1024;
        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), BUDGET);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        let _ = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "payload".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 200,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));

        let weighted = server.primary.file_cache.weighted_size();
        assert!(
            weighted > 0,
            "enough inserts should flush moka maintenance so occupancy is visible"
        );
        assert!(
            weighted <= BUDGET,
            "file cache occupancy {weighted} exceeded budget {BUDGET}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_sees_edits_after_mtime_change() {
        let (server, dir) = ready_server_with_disk();
        let _ = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "cache-me".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            dir.join("hello.py"),
            "def greet():\n    return 'fresh-content'\n",
        )
        .unwrap();

        let after = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "fresh-content".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = after.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("fresh-content"), "{text}");
        assert!(!text.contains("cache-me"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn loop_guard_server(repeat_max: usize) -> (AstrolabeServer, PathBuf) {
        let (server, dir) = ready_server_with_disk();
        *server.loop_guard.lock().unwrap() = LoopGuard::new(crate::loop_guard::LoopGuardConfig {
            repeat_max,
            ..crate::loop_guard::LoopGuardConfig::default()
        });
        (server, dir)
    }

    fn guarded_search(server: &AstrolabeServer, q: &str, budget: usize) -> String {
        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: q.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: budget,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        result.content[0].as_text().unwrap().text.clone()
    }

    /// Timing probe (run with --ignored --nocapture): 3000 files, ~150k symbols.
    #[test]
    #[ignore]
    fn bench_find_symbol_large_graph() {
        let mut files = Vec::new();
        let mut symbols = Vec::new();
        for f in 0..3000u32 {
            files.push(CodeFile {
                id: FileId(f),
                path: RelPath::new(format!("pkg{}/mod{}.rs", f % 50, f)),
                language: Some(Language::Rust),
                loc: 1000,
                sha: String::new(),
            });
            let base = symbols.len() as u32;
            symbols.push(CodeSymbol {
                id: SymbolId(base),
                file: FileId(f),
                name: format!("Service{f}"),
                kind: SymbolKind::Class,
                signature: format!("pub struct Service{f}"),
                start_line: 1,
                end_line: 400,
                exported: true,
            });
            for m in 0..49u32 {
                symbols.push(CodeSymbol {
                    id: SymbolId(base + 1 + m),
                    file: FileId(f),
                    name: if m % 7 == 0 {
                        "handle".into()
                    } else {
                        format!("method_{m}_Helper")
                    },
                    kind: SymbolKind::Method,
                    signature: format!("fn method_{m}()"),
                    start_line: 2 + m * 8,
                    end_line: 8 + m * 8,
                    exported: m % 2 == 0,
                });
            }
        }
        let n = symbols.len();
        let graph = CodeGraph {
            files,
            symbols,
            edges: vec![],
        };
        let mut server =
            AstrolabeServer::new_with_cache_budget(PathBuf::from("."), DEFAULT_MEMORY_BUDGET_BYTES);
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(
            RepoIndex::from_graph(PathBuf::from("."), graph, Default::default()),
        ));
        server.structured_output = false;
        let run = |q: &str, substring: Option<bool>| {
            // Warm-up call: builds the per-snapshot lookup tables (once per index).
            let t0 = std::time::Instant::now();
            let _ = server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: None,
                query: q.into(),
                include_body: false,
                substring,
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 2000,
                offset: 0,
            }));
            eprintln!("BENCH first call {q:?}: {:?}", t0.elapsed());
            let t = std::time::Instant::now();
            let mut last = 0;
            for _ in 0..5 {
                let r = server.find_symbol(Parameters(QueryParams {
                    root: None,
                    index_root: None,
                    query: q.into(),
                    include_body: false,
                    substring,
                    path_filter: None,
                    kind: None,
                    depth: 0,
                    budget_tokens: 2000,
                    offset: 0,
                }));
                last = r.content[0].as_text().unwrap().text.len();
            }
            eprintln!(
                "BENCH {n} symbols query={q:?} substring={substring:?}: {:?}/call (out {last}B)",
                t.elapsed() / 5
            );
        };
        run("handle", Some(false));
        run("helper", Some(true));
        run("Service1500", None);
        run("Service1500/handle", None);
        run("Service/handle", Some(true));
    }

    #[test]
    fn find_symbol_pages_concatenate_to_full_match_set() {
        let dir = unique_temp_dir();
        let mut expected = std::collections::BTreeSet::new();
        for f in 0..4 {
            let mut src = String::new();
            for i in 0..40 {
                src.push_str(&format!("def pager_{f}_{i}():\n    pass\n"));
                expected.insert(format!("pager_{f}_{i}"));
            }
            std::fs::write(dir.join(format!("m{f}.py")), src).unwrap();
        }
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 << 20);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        let page = |offset: usize, path: bool| {
            let r = server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: None,
                query: if path {
                    "/pager".into()
                } else {
                    "pager".into()
                },
                include_body: false,
                substring: Some(true),
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 300,
                offset,
            }));
            r.content[0].as_text().unwrap().text.clone()
        };
        for path in [false, true] {
            let mut got = Vec::new();
            let mut offset = 0;
            let mut pages = 0;
            loop {
                let text = page(offset, path);
                pages += 1;
                assert!(text.contains("total=160"), "{text}");
                assert!(!text.contains("blocked: true"), "{text}");
                for name in text
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|w| w.starts_with("pager_"))
                {
                    got.push(name.to_string());
                }
                let Some(next) = text
                    .split("next_offset=")
                    .nth(1)
                    .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|n| n.parse::<usize>().ok())
                else {
                    assert!(text.contains("truncated=false"), "{text}");
                    break;
                };
                assert!(next > offset, "{text}");
                offset = next;
                assert!(pages < 100);
            }
            assert!(pages > 2, "fixture should need several pages");
            let unique: std::collections::BTreeSet<_> = got.iter().cloned().collect();
            assert_eq!(unique, expected, "path={path}");
        }
        let past = page(10_000, false);
        assert!(past.contains("beyond total"), "{past}");
    }

    #[test]
    fn search_code_pages_concatenate_to_full_match_set() {
        let dir = unique_temp_dir();
        let mut expected = std::collections::BTreeSet::new();
        for f in 0..4 {
            let mut src = String::new();
            for i in 0..40 {
                src.push_str(&format!("needle_{f}_{i} = {i}\n"));
                expected.insert(format!("@m{f}.py:{}", i + 1));
            }
            std::fs::write(dir.join(format!("m{f}.py")), src).unwrap();
        }
        let mut notes = String::new();
        for i in 0..30 {
            notes.push_str(&format!("needle note {i}\n"));
            expected.insert(format!("@notes.md:{}", i + 1));
        }
        std::fs::write(dir.join("notes.md"), notes).unwrap();
        let (mut server, _) = (
            AstrolabeServer::new_with_cache_budget(dir.clone(), 8 << 20),
            (),
        );
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        let page = |offset: usize| {
            let r = server.search_code(Parameters(SearchParams {
                root: None,
                index_root: None,
                context_lines: Some(0),
                page: false,
                group: None,
                dedup: None,
                aggregate: None,
                exclude: None,
                count_only: false,
                skip_comments: false,
                query: "needle".into(),
                regex: false,
                path_filter: None,
                case_sensitive: None,
                budget_tokens: 300,
                offset,
                glob: None,
            }));
            r.content[0].as_text().unwrap().text.clone()
        };
        let mut got = Vec::new();
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let text = page(offset);
            pages += 1;
            assert!(text.contains("total=190"), "{text}");
            assert!(
                !text.contains("blocked: true"),
                "paging must not trip loop guard: {text}"
            );
            for line in text.lines().filter(|l| l.starts_with('@')) {
                got.push(line.split(' ').next().unwrap().to_string());
            }
            let Some(next) = text
                .split("next_offset=")
                .nth(1)
                .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                assert!(text.contains("truncated=false"), "{text}");
                break;
            };
            assert!(next > offset, "{text}");
            offset = next;
            assert!(pages < 100);
        }
        assert!(pages > 2, "fixture should need several pages");
        let unique: std::collections::BTreeSet<_> = got.iter().cloned().collect();
        assert_eq!(unique.len(), got.len(), "pages overlap");
        assert_eq!(unique, expected);
        // code hits come before [text-only] hits across pages
        let first_text = got.iter().position(|l| l.starts_with("@notes.md")).unwrap();
        assert!(got[first_text..].iter().all(|l| l.starts_with("@notes.md")));
        // deterministic ordering
        assert_eq!(page(0), page(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_offset_is_part_of_signature() {
        let (server, dir) = loop_guard_server(1);
        let at = |offset: usize| {
            let r = server.search_code(Parameters(SearchParams {
                root: None,
                index_root: None,
                context_lines: Some(0),
                page: false,
                group: None,
                dedup: None,
                aggregate: None,
                exclude: None,
                count_only: false,
                skip_comments: false,
                query: "needle-a".into(),
                regex: false,
                path_filter: None,
                case_sensitive: None,
                budget_tokens: 1000,
                offset,
                glob: None,
            }));
            r.content[0].as_text().unwrap().text.clone()
        };
        for offset in 0..4 {
            assert!(!at(offset).contains("blocked: true"));
        }
        assert!(at(0).contains("blocked: true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_loop_guard_short_circuits_repeated_query() {
        let (server, dir) = loop_guard_server(2);
        let call = |q: &str| guarded_search(&server, q, 1000);
        assert!(call("needle-a").contains("confidence: exact"));
        assert!(call("needle-b").contains("confidence: exact"));
        let first = call("needle-a");
        assert!(first.contains("confidence: exact"));
        let blocked = call("needle-a");
        assert!(blocked.contains("loop guard"), "{blocked}");
        assert!(blocked.contains("blocked: true"), "{blocked}");
        assert!(!blocked.contains("same result as before"), "{blocked}");
        // the cached previous result is returned, not a bare note
        assert!(blocked.contains(&first), "{blocked}");
        assert!(call("needle-b").contains("confidence: exact"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_budget_tokens_is_part_of_signature() {
        let (server, dir) = loop_guard_server(1);
        assert!(!guarded_search(&server, "needle-a", 1000).contains("blocked: true"));
        // "truncated, raise budget_tokens" retry must execute
        assert!(!guarded_search(&server, "needle-a", 4000).contains("blocked: true"));
        assert!(guarded_search(&server, "needle-a", 4000).contains("blocked: true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_does_not_count_index_building_responses() {
        let (server, dir) = loop_guard_server(1);
        let ready = std::mem::replace(
            &mut *server.primary.state.write().unwrap(),
            IndexState::Building,
        );
        for _ in 0..5 {
            let text = guarded_search(&server, "needle-a", 1000);
            assert!(!text.contains("blocked: true"), "{text}");
        }
        *server.primary.state.write().unwrap() = ready;
        let text = guarded_search(&server, "needle-a", 1000);
        assert!(text.contains("confidence: exact"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_does_not_count_error_responses() {
        let (server, dir) = loop_guard_server(1);
        let bad = |s: &AstrolabeServer| {
            s.search_code(Parameters(SearchParams {
                root: None,
                index_root: None,
                query: "(".into(),
                regex: true,
                path_filter: None,
                case_sensitive: None,
                budget_tokens: 1000,
                offset: 0,
                context_lines: Some(0),
                page: false,
                group: None,
                dedup: None,
                aggregate: None,
                exclude: None,
                count_only: false,
                skip_comments: false,
                glob: None,
            }))
        };
        for _ in 0..3 {
            let r = bad(&server);
            let text = r.content[0].as_text().unwrap().text.clone();
            assert!(!text.contains("blocked: true"), "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_reexecutes_after_index_changes() {
        // edit-then-requery: a new index generation means a fresh count and
        // fresh results, never the stale cached text.
        let (server, dir) = loop_guard_server(1);
        assert!(!guarded_search(&server, "needle-a", 1000).contains("blocked: true"));
        assert!(guarded_search(&server, "needle-a", 1000).contains("blocked: true"));
        std::fs::write(
            dir.join("hello.py"),
            "def greet():\n    return 'needle-a'\n",
        )
        .unwrap();
        let rebuilt = Arc::new(crate::index::build(&dir).expect("index"));
        *server.primary.state.write().unwrap() = IndexState::Ready(rebuilt);
        let text = guarded_search(&server, "needle-a", 1000);
        assert!(!text.contains("blocked: true"), "{text}");
        assert!(text.contains("hello.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_marks_structured_blocked() {
        let blocked = mark_blocked(
            {
                let mut r = CallToolResult::success(vec![ContentBlock::text("prev")]);
                r.structured_content = Some(json!({"text": "prev"}));
                r
            },
            "blocked: true\nnote",
        );
        assert_eq!(blocked.structured_content.unwrap()["blocked"], json!(true));
        let text = blocked.content[0].as_text().unwrap().text.clone();
        assert!(
            text.starts_with("blocked: true") && text.ends_with("prev"),
            "{text}"
        );
    }

    fn hit(path: &str, line: usize) -> SearchHit {
        SearchHit {
            path: path.into(),
            line,
            text: format!("match{line}"),
            text_only: false,
        }
    }

    fn numbered(n: usize) -> Arc<Vec<String>> {
        Arc::new((1..=n).map(|i| format!("line{i}")).collect())
    }

    #[test]
    fn search_context_defaults_adapt_to_match_count() {
        assert_eq!(search_context_lines(None, 1), 3);
        assert_eq!(search_context_lines(None, 5), 3);
        assert_eq!(search_context_lines(None, 6), 1);
        assert_eq!(search_context_lines(None, 20), 1);
        assert_eq!(search_context_lines(None, 21), 0);
        assert_eq!(search_context_lines(Some(0), 1), 0);
        assert_eq!(search_context_lines(Some(99), 1), MAX_SEARCH_CONTEXT);
    }

    #[test]
    fn search_context_windows_merge_and_anchor_every_match() {
        let lines = numbered(30);
        let hits = vec![
            hit("a.rs", 5),
            hit("a.rs", 7),
            hit("a.rs", 20),
            hit("b.rs", 1),
        ];
        let (out, shown) = render_search_hits(&hits, 2, 10_000, &mut |_| Some(Arc::clone(&lines)));
        assert_eq!(shown, 4);
        let expected = "  3| line3\n  4| line4\n@a.rs:5 match5\n  6| line6\n@a.rs:7 match7\n  8| line8\n  9| line9\n  …\n  18| line18\n  19| line19\n@a.rs:20 match20\n  21| line21\n  22| line22\n@b.rs:1 match1\n  2| line2\n  3| line3\n";
        assert_eq!(out, expected);
        // Each source line at most once per file window.
        assert_eq!(out.matches("  6| ").count(), 1);
    }

    #[test]
    fn search_context_zero_is_bare_anchor_lines() {
        let hits = vec![hit("a.rs", 5), hit("a.rs", 7)];
        let (out, shown) = render_search_hits(&hits, 0, 10_000, &mut |_| panic!("not read"));
        assert_eq!(shown, 2);
        assert_eq!(out, "@a.rs:5 match5\n@a.rs:7 match7\n");
    }

    #[test]
    fn search_context_respects_budget_and_falls_back_to_bare_lines() {
        let lines = numbered(400);
        let hits: Vec<_> = (1..=40).map(|i| hit("a.rs", i * 10)).collect();
        let (out, shown) = render_search_hits(&hits, 3, 120, &mut |_| Some(Arc::clone(&lines)));
        assert!(shown > 0 && shown < 40, "{shown}");
        assert!(estimate_tokens(&out) <= 120, "{}", estimate_tokens(&out));
        assert_eq!(
            out.lines().filter(|l| l.starts_with("@a.rs:")).count(),
            shown
        );
    }

    #[test]
    fn search_code_shows_context_by_default_for_narrow_queries() {
        let (server, dir) = ready_server_with_disk();
        let params = |context_lines| SearchParams {
            root: None,
            index_root: None,
            query: "cache-me".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines,
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        };
        let result = server.search_code(Parameters(params(None)));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("  1| def greet():\n@hello.py:2 return 'cache-me'"),
            "{text}"
        );
        assert!(text.contains("Context: 3 lines around each hit"), "{text}");
        let bare = server.search_code(Parameters(params(Some(0))));
        let bare = bare.content[0].as_text().unwrap().text.as_str();
        assert!(!bare.contains("1| def greet"), "{bare}");
        assert!(bare.contains("@hello.py:2 return 'cache-me'"), "{bare}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_empty_result_states_no_match_explicitly() {
        let (server, dir) = ready_server_with_disk();
        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "totally-absent-needle".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        // 零命中必须显式声明，避免"只剩元信息、疑似服务故障"的歧义输出。
        assert!(text.contains("(no matching code lines)"), "{text}");
        // confidence 元信息保留，供宿主客户端区分索引根上下文。
        assert!(text.contains("confidence: exact"), "{text}");
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("shown=0 omitted=0 truncated=false"), "{text}");
        // Empty results must not claim truncation.
        assert!(!text.contains("omitted due to"), "{text}");
        assert!(!text.contains("Not the full set"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_warns_when_regex_metacharacters_are_literal() {
        let (server, dir) = ready_server_with_disk();
        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "scanForBrandHint|RewriteStreamingBody".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("regex=false"), "{text}");
        assert!(
            text.contains("scanForBrandHint|RewriteStreamingBody"),
            "{text}"
        );
        assert!(text.contains("(no matching code lines)"), "{text}");
        assert!(!text.contains("truncated=true"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_truncation_is_declared_in_text() {
        let dir = unique_temp_dir();
        let mut source = String::from("needle = 0\n");
        for i in 1..40 {
            source.push_str(&format!("needle = {i}\n"));
        }
        std::fs::write(dir.join("hits.py"), source).unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "needle".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 20,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("truncated=true"), "{text}");
        assert!(text.contains("Not the full set"), "{text}");
        assert!(
            text.to_ascii_lowercase().contains("raise budget_tokens"),
            "{text}"
        );
        assert!(!text.contains("regex=false; searched literally"), "{text}");
        let structured = result.structured_content.expect("opt-in structured");
        assert_eq!(structured["truncated"], json!(true));
        assert_eq!(structured["mode"], json!("literal"));
        assert!(structured["omitted"].as_u64().unwrap() > 0, "{structured}");
        assert_eq!(structured["matches"], json!(40));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_schema_describes_literal_default_and_regex_flag() {
        let server = ready_server();
        let tools = server.listed_tools();
        let search = tools
            .iter()
            .find(|tool| tool.name == "search_code")
            .expect("search_code in catalog");
        let schema = Value::Object((*search.input_schema).clone());
        let query = schema.pointer("/properties/query/description").unwrap();
        let path_filter = schema
            .pointer("/properties/path_filter/description")
            .unwrap();
        let tool_desc = search.description.as_deref().unwrap_or("");
        // `regex` is accepted but not advertised; query and tool descriptions
        // still name it.
        assert!(schema.pointer("/properties/regex").is_none());
        assert!(query.as_str().unwrap().contains("regex=true"), "{query}");
        assert!(
            path_filter.as_str().unwrap().contains("case-sensitive"),
            "{path_filter}"
        );
        assert!(schema.pointer("/properties/budget_tokens").is_none());
        assert!(schema.pointer("/properties/offset").is_none());
        assert!(tool_desc.contains("regex=true"), "{tool_desc}");
        assert!(!tool_desc.contains("offset=next_offset"), "{tool_desc}");
    }

    fn server_on_files(files: &[(&str, &str)]) -> (AstrolabeServer, PathBuf) {
        let dir = unique_temp_dir();
        for (path, body) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        (server, dir)
    }

    fn fs_query(query: &str) -> QueryParams {
        QueryParams {
            root: None,
            index_root: None,
            query: query.into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 2500,
            offset: 0,
        }
    }

    fn fs_text(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn find_symbol_ranks_exact_name_before_substring_and_survives_truncation() {
        let mut many = String::new();
        for i in 0..60 {
            many.push_str(&format!("def handle_widget_{i}():\n    pass\n"));
        }
        let (server, dir) = server_on_files(&[
            ("aaa/noise.py", &many),
            ("zzz/real.py", "def widget():\n    pass\n"),
        ]);
        let mut params = fs_query("widget");
        params.budget_tokens = 120;
        let text = fs_text(&server.find_symbol(Parameters(params)));
        let first = text.lines().find(|l| l.contains('@')).unwrap_or_default();
        assert!(first.contains("zzz/real.py"), "{text}");

        // 编辑文件后重建索引再查：新增的精确同名定义同样排在最前。
        std::fs::write(
            dir.join("aaa/exact.py"),
            "def handle_widget_5x():\n    pass\nclass Gadget:\n    pass\n",
        )
        .unwrap();
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        let mut params = fs_query("gadget");
        params.substring = Some(false);
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("aaa/exact.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_blank_query_errors_and_kind_variable_is_accepted() {
        let (server, dir) = server_on_files(&[("a.py", "def f():\n    pass\n")]);
        let text = fs_text(&server.find_symbol(Parameters(fs_query("   "))));
        assert!(text.contains("non-empty"), "{text}");
        assert!(!text.contains("shown="), "{text}");
        let mut params = fs_query("f");
        params.kind = Some("variable".into());
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(!text.contains("Unknown kind"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn looks_like_filesystem_path_detects_code_paths_not_type_methods() {
        assert!(looks_like_filesystem_path("pkg/cloud.py"));
        assert!(looks_like_filesystem_path("./src/lib.rs"));
        assert!(looks_like_filesystem_path("cloud.py"));
        assert!(looks_like_filesystem_path("/Users/x/proj/a.go"));
        assert!(looks_like_filesystem_path("/abs/pkg/cloud.py"));
        assert!(looks_like_filesystem_path("/cloud.py"));
        assert!(looks_like_filesystem_path("C:/work/a.ts"));
        assert!(looks_like_filesystem_path("pkg/"));
        assert!(looks_like_filesystem_path(r"src\main.rs"));
        assert!(looks_like_filesystem_path("~/proj/a.rs"));
        assert!(!looks_like_filesystem_path("/pager"));
        assert!(!looks_like_filesystem_path("/CloudClient"));
        assert!(!looks_like_filesystem_path("CloudClient"));
        assert!(!looks_like_filesystem_path("CloudClient/update"));
        assert!(!looks_like_filesystem_path("pkg.Func"));
        assert!(!looks_like_filesystem_path("a::b"));
        assert!(!looks_like_filesystem_path("*"));
        assert!(!looks_like_filesystem_path(""));
    }

    #[test]
    fn query_params_reject_unknown_fields_and_default_include_body() {
        let ok: QueryParams = serde_json::from_value(serde_json::json!({
            "query": "Foo"
        }))
        .expect("query alone");
        assert!(ok.include_body, "include_body should default to true");
        assert_eq!(ok.query, "Foo");

        let err = serde_json::from_value::<QueryParams>(serde_json::json!({
            "name": "Foo"
        }))
        .expect_err("name is not a valid field");
        let msg = err.to_string();
        assert!(
            msg.contains("name") && (msg.contains("query") || msg.contains("expected")),
            "{msg}"
        );

        let err = serde_json::from_value::<QueryParams>(serde_json::json!({
            "query": "Foo",
            "symbol": "Foo"
        }))
        .expect_err("symbol is not a valid field");
        let msg = err.to_string();
        assert!(msg.contains("symbol"), "{msg}");

        let off: QueryParams = serde_json::from_value(serde_json::json!({
            "query": "Foo",
            "include_body": false
        }))
        .unwrap();
        assert!(!off.include_body);
    }

    #[test]
    fn find_symbol_defaults_to_include_body_so_models_need_less_read() {
        let (server, dir) = server_on_files(&[(
            "pkg/cloud.py",
            "class CloudClient:\n    def update(self, item):\n        return item\n",
        )]);
        // Mimic an MCP call that omits include_body (serde default = true).
        let params: QueryParams = serde_json::from_value(serde_json::json!({
            "query": "CloudClient"
        }))
        .unwrap();
        assert!(params.include_body);
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("----- body @"), "{text}");
        assert!(text.contains("class CloudClient"), "{text}");
        assert!(text.contains("def update"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_orders_by_path_line_not_signature_and_tags_container() {
        let (server, dir) = server_on_files(&[
            (
                "pkg/b.py",
                "class Beta:\n    def run(self):\n        pass\n\ndef run():\n    pass\n",
            ),
            (
                "pkg/a.py",
                "class Alpha:\n    def run(self):\n        pass\n",
            ),
            ("tests/test_x.py", "def run():\n    pass\n"),
        ]);
        let mut params = fs_query("run");
        params.substring = Some(false);
        let text = fs_text(&server.find_symbol(Parameters(params)));
        let anchors: Vec<&str> = text.lines().filter(|l| l.contains("@")).collect();
        assert!(
            anchors[0].contains("pkg/a.py") && anchors[0].contains("[method in Alpha]"),
            "{text}"
        );
        assert!(
            anchors[1].contains("pkg/b.py:2") && anchors[1].contains("[method in Beta]"),
            "{text}"
        );
        assert!(
            anchors[2].contains("pkg/b.py:5") && anchors[2].contains("[function]"),
            "{text}"
        );
        assert!(anchors[3].contains("tests/test_x.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_zero_budget_has_no_stuck_next_offset() {
        let (server, dir) = server_on_files(&[(
            "a.py",
            "def widget():\n    pass\ndef widget2():\n    pass\n",
        )]);
        let mut params = fs_query("widget");
        params.budget_tokens = 0;
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("shown=0"), "{text}");
        assert!(!text.contains("next_offset="), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_later_page_warns_when_index_generation_changed() {
        let src: String = (0..30)
            .map(|i| format!("def drift_{i}():\n    pass\n"))
            .collect();
        let (server, dir) = server_on_files(&[("a.py", src.as_str())]);
        let page = |offset: usize| {
            let mut params = fs_query("drift");
            params.budget_tokens = 120;
            params.offset = offset;
            fs_text(&server.find_symbol(Parameters(params)))
        };
        let first = page(0);
        assert!(first.contains("truncated=true"), "{first}");
        assert!(!first.contains("while paging"), "{first}");
        // Same generation: no warning on the next page.
        assert!(!page(5).contains("while paging"));
        // Replace the index (what an edit + reindex does): a later page warns.
        *server.primary.state.write().unwrap() =
            IndexState::Ready(Arc::new(crate::index::build(&dir).expect("index")));
        let drifted = page(10);
        assert!(drifted.contains("while paging"), "{drifted}");
        // The warning is stated once; the generation is now the current one.
        assert!(!page(15).contains("while paging"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_exact_only_kind_and_path_filter_apply_to_name_queries() {
        let (server, dir) = server_on_files(&[
            (
                "src/a.py",
                "class Item:\n    pass\ndef item():\n    pass\ndef items_all():\n    pass\n",
            ),
            ("lib/b.py", "def item():\n    pass\n"),
        ]);
        let mut params = fs_query("item");
        params.substring = Some(false);
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(!text.contains("items_all"), "{text}");

        let mut params = fs_query("item");
        params.kind = Some("class".into());
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("class Item"), "{text}");
        assert!(!text.contains("def item"), "{text}");

        let mut params = fs_query("item");
        params.path_filter = Some("lib/".into());
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("lib/b.py"), "{text}");
        assert!(!text.contains("src/a.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_accepts_dot_and_colon_paths_and_go_receivers() {
        let (server, dir) = server_on_files(&[
            (
                "cmd/command.go",
                "package cmd\n\ntype Command struct {\n\tName string\n}\n\nfunc (c *Command) ExecuteC() error {\n\treturn nil\n}\n",
            ),
            (
                "src/lib.rs",
                "pub struct Pool;\nimpl Pool {\n    pub fn spawn(&self) {}\n}\n",
            ),
        ]);
        for query in ["Command.ExecuteC", "Command/ExecuteC", "cmd.ExecuteC"] {
            let text = fs_text(&server.find_symbol(Parameters(fs_query(query))));
            assert!(text.contains("cmd/command.go:7"), "{query}: {text}");
        }
        let text = fs_text(&server.find_symbol(Parameters(fs_query("Pool::spawn"))));
        assert!(text.contains("src/lib.rs:3"), "{text}");
        let text = fs_text(&server.find_symbol(Parameters(fs_query("Command struct"))));
        assert!(text.contains("cmd/command.go:3"), "{text}");
        assert!(text.contains("ignored keywords in the query"), "{text}");
        let text = fs_text(&server.find_symbol(Parameters(fs_query("ExecuteX"))));
        assert!(
            text.contains("Similar symbol names") && text.contains("ExecuteC"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_prefers_literal_names_containing_separators() {
        let (server, dir) = server_on_files(&[(
            "lib/hooks.sh",
            "#!/bin/bash\nbats::on_failure() {\n  :\n}\n",
        )]);
        let text = fs_text(&server.find_symbol(Parameters(fs_query("bats::on_failure"))));
        assert!(text.contains("lib/hooks.sh:2"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unmatched_path_filter_is_an_explicit_error_with_closest_paths() {
        let (server, dir) =
            server_on_files(&[("relay/channel/openai.py", "def send():\n    pass\n")]);
        let mut params = fs_query("send");
        params.path_filter = Some("relay/chanel".into());
        let result = server.find_symbol(Parameters(params));
        assert_eq!(result.is_error, Some(true));
        let text = fs_text(&result);
        assert!(text.contains("relay/channel"), "{text}");
        assert!(text.contains("Next:"), "{text}");
        assert!(
            text.contains("drop path_filter") || text.contains("use one of the paths above"),
            "{text}"
        );

        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "send".into(),
            regex: false,
            path_filter: Some("./relay/channel".into()),
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        assert_eq!(result.is_error, Some(true));
        assert!(
            fs_text(&result).contains("relay/channel/"),
            "{}",
            fs_text(&result)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_symbol_query_handles_separators_and_keywords() {
        assert_eq!(normalize_symbol_query("Type.method").query, "Type/method");
        assert_eq!(normalize_symbol_query("A::b::c").query, "A/b/c");
        assert_eq!(normalize_symbol_query("::A::b").query, "/A/b");
        let n = normalize_symbol_query("Proc struct");
        assert_eq!(n.query, "Proc");
        assert!(n.ignored.is_some());
        assert_eq!(normalize_symbol_query("struct").query, "struct");
    }

    #[test]
    fn find_symbol_empty_result_states_no_match_explicitly() {
        let (server, dir) = ready_server_with_disk();
        // 名称匹配分支：include_body=true 时零命中也不得出现
        // 误导性的"无可提取的符号体"。
        let named = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "NoSuchSymbolAnywhere".into(),
            include_body: true,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 1000,
            offset: 0,
        }));
        let named_text = named.content[0].as_text().unwrap().text.as_str();
        assert!(named_text.contains("(no matching symbols)"), "{named_text}");
        assert!(
            !named_text.contains("no extractable symbol body"),
            "{named_text}"
        );

        // 层级路径分支（query 含 '/'）同样需要显式零命中声明。
        let by_path = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "NoSuch/Path".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 1000,
            offset: 0,
        }));
        let path_text = by_path.content[0].as_text().unwrap().text.as_str();
        assert!(path_text.contains("(no matching symbols)"), "{path_text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_large_body_is_gracefully_truncated() {
        let dir = unique_temp_dir();
        // 62 行的函数体：全文远超 300 token 预算，必然触发截断路径。
        let mut source = String::from("fn spacious() {\n");
        for i in 0..60 {
            source.push_str(&format!("    let v{i} = {i};\n"));
        }
        source.push_str("}\n");
        std::fs::write(dir.join("big.rs"), source).unwrap();

        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;

        let result = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "spacious".into(),
            include_body: true,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 300,
            offset: 0,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();

        // 不是整块丢弃：符号的前几行代码确实被展示。
        assert!(text.contains("let v0 = 0;"), "{text}");
        assert!(text.contains("let v1 = 1;"), "{text}");
        // 截断说明带总行数/已展示行数与 Read 指引。
        let note = regex::Regex::new(
            r"\[symbol has \d+ lines total; budget_tokens truncated to first \d+; use Read\(offset, limit\) for inner ranges\]",
        )
        .unwrap();
        assert!(note.is_match(text), "truncation note missing: {text}");
        // 截断真的发生了：尾部行不在输出里。
        assert!(!text.contains("let v59 = 59;"), "{text}");
        // 已成功截断附加符号体，绝不能输出自相矛盾的"无可提取"。
        assert!(
            !text.contains("(include_body: no extractable symbol body)"),
            "{text}"
        );
        assert!(!text.contains("failed to read symbol body"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn first_text(result: CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|c| c.text.clone())
            .unwrap_or_default()
    }

    fn wait_ready(server: &AstrolabeServer, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            match &*server.primary.state.read().unwrap() {
                IndexState::Ready(_) => return,
                IndexState::Failed(e) => panic!("index failed: {e}"),
                IndexState::Building => {}
            }
            if start.elapsed() > timeout {
                panic!("timeout waiting for Ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_session_ready(session: &crate::session::RootSession, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            match &*session.state.read().unwrap() {
                IndexState::Ready(_) => return,
                IndexState::Failed(e) => panic!("index failed: {e}"),
                IndexState::Building => {}
            }
            if start.elapsed() > timeout {
                panic!("timeout waiting for child Ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn git_marker(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    fn test_knobs() -> crate::knobs::McpKnobs {
        crate::knobs::McpKnobs {
            safety_interval: Duration::ZERO,
            reindex_throttle: Duration::from_secs(60),
            watch_mode: Some(crate::knobs::WatchMode::Events),
            // Polling loops in these tests legitimately repeat one query.
            loop_guard: crate::loop_guard::LoopGuardConfig {
                repeat_max: 0,
                rate_max: 0,
                ..Default::default()
            },
            ..crate::knobs::McpKnobs::default()
        }
    }

    #[test]
    fn cold_build_edit_is_visible_at_ready() {
        let dir = unique_temp_dir();
        for i in 0..40 {
            std::fs::write(
                dir.join(format!("f{i}.py")),
                format!("def f{i}():\n    return {i}\n"),
            )
            .unwrap();
        }
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();

        // The watch handle is published before the baseline scan finishes.
        // Writing in that gap used to be sealed into the snapshot (no event)
        // while the cold walk missed the file, so Ready never contained it.
        // Wait until the baseline is sealed and the build is still running.
        let mut wrote = false;
        let write_deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < write_deadline {
            let seeded = server
                .primary
                .freshness
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|log| log.is_seeded());
            let building = matches!(*server.primary.state.read().unwrap(), IndexState::Building);
            if seeded && building {
                std::fs::write(
                    dir.join("during.py"),
                    "def during_build_xyz():\n    return 1\n",
                )
                .unwrap();
                wrote = true;
                break;
            }
            if matches!(*server.primary.state.read().unwrap(), IndexState::Ready(_)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            wrote,
            "never observed Building after watcher baseline; cold build was too fast"
        );
        wait_ready(&server, Duration::from_secs(30));
        // Notify delivery and the 500ms debounce can land the changeset just
        // after the Ready publish. The edit must still show up (pending drain
        // or the query barrier), not be dropped. Poll instead of asserting
        // on the first query, which raced that handoff on CI.
        let visible_deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut text = String::new();
        while std::time::Instant::now() < visible_deadline {
            text = first_text(server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: None,
                query: "during_build_xyz".into(),
                include_body: false,
                substring: None,
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 500,
                offset: 0,
            })));
            if text.contains("during_build_xyz") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            text.contains("during_build_xyz"),
            "Ready graph must include the in-flight edit, got {text}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_fresh_answers_without_staleness_note() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        })));
        assert!(text.contains("alpha"), "{text}");
        assert!(
            !text.contains("confidence: unknown") || text.contains("syntactic"),
            "{text}"
        );
        assert!(
            !text.contains("Index may be stale"),
            "Fresh must not append staleness: {text}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_repair_inlines_suspects_into_graph() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));

        std::fs::write(dir.join("a.py"), "def alpha_v2():\n    return 2\n").unwrap();
        std::thread::sleep(Duration::from_millis(120));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "alpha_v2".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        })));
        assert!(
            text.contains("alpha_v2"),
            "inline Repair must update the graph, got {text}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_degrade_appends_staleness_and_rebuilds() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut knobs = test_knobs();
        knobs.barrier_max = 0;
        let mut server = AstrolabeServer::new_with_knobs(dir.clone(), knobs, 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));

        std::fs::write(dir.join("a.py"), "def alpha_v3():\n    return 3\n").unwrap();
        std::thread::sleep(Duration::from_millis(120));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        })));
        assert!(
            text.contains("Index may be stale") && text.contains("confidence: unknown"),
            "Degrade must append staleness_note, got {text}"
        );
        let start = std::time::Instant::now();
        loop {
            let probe = first_text(server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: None,
                query: "alpha_v3".into(),
                include_body: false,
                substring: None,
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 500,
                offset: 0,
            })));
            if probe.contains("alpha_v3") {
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("background rebuild did not land alpha_v3: {probe}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_repair_empty_requests_verification() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));
        let log = server
            .primary
            .freshness
            .lock()
            .unwrap()
            .clone()
            .expect("watcher must publish a freshness log");
        // Age the watermark past FRESH_WINDOW with no suspects. Waiting for
        // the condition (instead of a fixed sleep) matters on loaded CI
        // runners: the watcher thread can be starved past `wait_ready`, so
        // the baseline check that pushes the watermark may land seconds
        // late and a fixed sleep leaves the watermark "fresh" at query
        // time — the barrier then answers Fresh and never requests a
        // verification. atime noise is one-shot (relatime), so once the
        // watermark has aged past the window no further bump races the query.
        let aged = astrolabe_core::freshness::FRESH_WINDOW + Duration::from_millis(200);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while log.watermark().elapsed() < aged {
            if std::time::Instant::now() > deadline {
                panic!("watermark never aged past FRESH_WINDOW");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let before = log.watermark();
        let _ = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        }));
        let start = std::time::Instant::now();
        while log.watermark() <= before {
            // Generous bound: on 2-vCPU CI runners the watch thread can be
            // starved by the parallel test suite for seconds before it
            // honors the verification request (STOP_SLICE is 50ms of idle).
            if start.elapsed() > Duration::from_secs(10) {
                panic!("request_verification did not advance the watermark");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_routes_hint_and_stores_in_child() {
        let parent = unique_temp_dir();
        let alpha = parent.join("alpha");
        let beta = parent.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git_marker(&alpha);
        git_marker(&beta);
        std::fs::write(alpha.join("a.py"), "def alpha_unique():\n    return 1\n").unwrap();
        std::fs::write(beta.join("b.py"), "def beta_unique():\n    return 2\n").unwrap();

        let mut knobs = test_knobs();
        knobs.watch_mode = None;
        knobs.resident_roots = 2;
        let mut server = AstrolabeServer::with_multi_project_knobs(
            parent.clone(),
            vec![alpha.clone(), beta.clone()],
            knobs,
            8 * 1024 * 1024,
        );
        server.structured_output = false;
        server.start_indexing();

        // Child name is a word-boundary hint; "alpha" also substring-matches
        // the symbol `alpha_unique` without taking the path-query branch.
        let priming = server.find_symbol(Parameters(QueryParams {
            root: Some("alpha".into()),
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        }));
        let priming_text = first_text(priming);
        if priming_text.contains("Index is building in the background") {
            let session = {
                let map = server.dispatch.as_ref().unwrap().sessions.lock().unwrap();
                map.get(&alpha.canonicalize().unwrap_or(alpha.clone()))
                    .cloned()
                    .expect("alpha session after route")
            };
            wait_session_ready(&session, Duration::from_secs(20));
        }
        let session = {
            let map = server.dispatch.as_ref().unwrap().sessions.lock().unwrap();
            map.get(&alpha.canonicalize().unwrap_or(alpha.clone()))
                .cloned()
                .expect("alpha session")
        };
        wait_session_ready(&session, Duration::from_secs(20));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            root: Some("alpha".into()),
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        })));
        assert!(
            text.starts_with(&format!("index_root: {}", session.root.display())),
            "dispatch must declare the child root, got {text}"
        );
        assert!(text.contains("alpha_unique"), "{text}");
        assert!(
            astrolabe_core::index::store_path(&session.root)
                .parent()
                .unwrap()
                .starts_with(&session.root),
            "store must live under the child"
        );
        assert!(
            !parent.join(".astrolabe").join("index.redb").exists(),
            "parent must not grow an index store"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn dispatch_evict_releases_child_lock() {
        let parent = unique_temp_dir();
        let alpha = parent.join("alpha");
        let beta = parent.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git_marker(&alpha);
        git_marker(&beta);
        std::fs::write(alpha.join("a.py"), "def alpha_fn():\n    return 1\n").unwrap();
        std::fs::write(beta.join("b.py"), "def beta_fn():\n    return 2\n").unwrap();

        let mut knobs = test_knobs();
        knobs.watch_mode = None;
        knobs.resident_roots = 1;
        let mut server = AstrolabeServer::with_multi_project_knobs(
            parent.clone(),
            vec![alpha.clone(), beta.clone()],
            knobs,
            8 * 1024 * 1024,
        );
        server.structured_output = false;
        server.start_indexing();

        let _ = server.find_symbol(Parameters(QueryParams {
            root: Some("alpha".into()),
            index_root: None,
            query: "alpha".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 200,
            offset: 0,
        }));
        let alpha_key = alpha.canonicalize().unwrap_or(alpha.clone());
        let session = server
            .dispatch
            .as_ref()
            .unwrap()
            .sessions
            .lock()
            .unwrap()
            .get(&alpha_key)
            .cloned()
            .expect("alpha");
        wait_session_ready(&session, Duration::from_secs(20));
        assert!(
            astrolabe_core::elect::IndexerLock::try_acquire(&alpha_key)
                .unwrap()
                .is_none(),
            "resident child must hold the indexer lock"
        );

        let _ = server.find_symbol(Parameters(QueryParams {
            root: Some("beta".into()),
            index_root: None,
            query: "beta".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 200,
            offset: 0,
        }));
        let start = std::time::Instant::now();
        loop {
            if astrolabe_core::elect::IndexerLock::try_acquire(&alpha_key)
                .unwrap()
                .is_some()
            {
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("evicted child did not release IndexerLock");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn dispatch_ambiguous_hint_lists_children() {
        let parent = unique_temp_dir();
        let alpha = parent.join("alpha");
        let beta = parent.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git_marker(&alpha);
        git_marker(&beta);
        std::fs::write(alpha.join("a.py"), "def a():\n    return 1\n").unwrap();
        std::fs::write(beta.join("b.py"), "def b():\n    return 2\n").unwrap();
        let mut knobs = test_knobs();
        knobs.watch_mode = None;
        let mut server = AstrolabeServer::with_multi_project_knobs(
            parent.clone(),
            vec![alpha.clone(), beta.clone()],
            knobs,
            8 * 1024 * 1024,
        );
        server.structured_output = false;
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "compare alpha and beta".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
        })));
        assert!(text.contains("alpha") && text.contains("beta"), "{text}");
        assert!(text.contains("confidence: unknown"), "{text}");
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn unindexed_languages_end_to_end_hints_and_dart_contrast() {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("main.py"),
            "def main():\n    print('hello from python')\n",
        )
        .unwrap();

        // 3 个 .scala 文件 (未支持语言)
        std::fs::write(dir.join("A.scala"), "class A { val x = 1 }\n").unwrap();
        std::fs::write(dir.join("B.scala"), "class B { val y = 2 }\n").unwrap();
        std::fs::write(dir.join("C.scala"), "class C { val z = 3 }\n").unwrap();

        // 1 个 .dart 文件 (支持语言对照)
        std::fs::write(
            dir.join("service.dart"),
            "void runService() {\n  print('dart service running');\n}\n",
        )
        .unwrap();

        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        // 1. search_code 零命中且无 path_filter，含 .scala×3 (N>=3) 时提示尚未支持语言且带计数
        let search_miss = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "nonexistent_code_pattern".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let search_text = search_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            search_text.contains("(no matching code lines)"),
            "{search_text}"
        );
        assert!(
            search_text.contains("unsupported languages"),
            "{search_text}"
        );
        assert!(search_text.contains("3 .scala files"), "{search_text}");
        assert!(
            search_text.contains("not in the code index"),
            "{search_text}"
        );
        assert!(
            search_text.contains("searched as plain text too, also no matches"),
            "{search_text}"
        );
        assert!(
            search_text.contains("Note: the repo has 3 .scala files in unsupported languages"),
            "{search_text}"
        );
        let structured = search_miss.structured_content.expect("structured");
        assert_eq!(structured["unindexed_hint"], json!(true));

        // 1b. 带 path_filter 的 search_code 零命中：filter 范围内 N>=3
        let search_filter_hit_hint = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "nonexistent".into(),
            regex: false,
            path_filter: Some(".scala".into()),
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let filter_hint_text = search_filter_hit_hint.content[0]
            .as_text()
            .unwrap()
            .text
            .as_str();
        assert!(
            filter_hint_text.contains(
                "Note: within path_filter '.scala', 3 .scala files belong to unsupported languages"
            ),
            "{filter_hint_text}"
        );
        let filter_hint_structured = search_filter_hit_hint
            .structured_content
            .expect("structured");
        assert_eq!(filter_hint_structured["unindexed_hint"], json!(true));

        // 1c. 带 path_filter 的 search_code 零命中：filter 范围内 N<3 (仅 A.scala 匹配，计 1)
        let search_filter_miss = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "nonexistent".into(),
            regex: false,
            path_filter: Some("A.scala".into()),
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let filter_miss_text = search_filter_miss.content[0]
            .as_text()
            .unwrap()
            .text
            .as_str();
        assert!(
            filter_miss_text.contains("(no matching code lines)"),
            "{filter_miss_text}"
        );
        assert!(
            !filter_miss_text.contains("unsupported languages"),
            "{filter_miss_text}"
        );
        let filter_structured = search_filter_miss.structured_content.expect("structured");
        assert_eq!(filter_structured["unindexed_hint"], json!(false));

        // 2. find_symbol 零命中提示
        let symbol_miss = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "NonExistentSymbol".into(),
            budget_tokens: 2500,
            offset: 0,
            depth: 0,
            substring: None,
            path_filter: None,
            kind: None,
            include_body: false,
        }));
        let symbol_text = symbol_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            symbol_text.contains("(no matching symbols)"),
            "{symbol_text}"
        );
        assert!(
            symbol_text.contains("(repo also has 3 .scala etc. unsupported-language sources outside the symbol index; symbol search does not cover them — use Grep/Read)"),
            "{symbol_text}"
        );

        // 4. get_languages 首行 supported、末尾 unindexed 行
        let langs_res = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 2500,
            host_lsp: None,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        let first_body_line = langs_text
            .lines()
            .find(|l| !l.starts_with("index_root:"))
            .expect("first line after index_root");
        assert_eq!(
            first_body_line,
            "supported languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart, csharp, visualbasic, kotlin, shell, ruby, lua"
        );
        assert!(
            langs_text.contains("unsupported-language source files by type: .scala×3"),
            "{langs_text}"
        );
        assert!(
            langs_text
                .contains("outside the symbol/import index; invisible to search_code/find_symbol"),
            "{langs_text}"
        );

        // 5. 对照验证：.dart 是支持语言
        // 5a. .dart 决不出现在 unindexed 统计中
        assert!(!langs_text.contains(".dart×"), "{langs_text}");
        // 5b. .dart 源码能被 search_code 命中
        let search_dart = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "dart service running".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let dart_text = search_dart.content[0].as_text().unwrap().text.as_str();
        assert!(dart_text.contains("@service.dart:2"), "{dart_text}");
        assert!(
            !dart_text.contains("(no matching code lines)"),
            "{dart_text}"
        );
        let dart_structured = search_dart.structured_content.expect("structured");
        assert_eq!(dart_structured["unindexed_hint"], json!(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn search_text(
        server: &AstrolabeServer,
        query: &str,
        regex: bool,
        cs: Option<bool>,
    ) -> (String, Value) {
        let result = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: query.into(),
            regex,
            path_filter: None,
            case_sensitive: cs,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let text = result.content[0].as_text().unwrap().text.clone();
        (text, result.structured_content.expect("structured"))
    }

    #[test]
    fn search_code_covers_non_indexed_text_files() {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("main.py"),
            "def load_user():\n    return 'user_table'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("README.md"),
            "Call load_user to read user_table.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("schema.sql"),
            "CREATE TABLE user_table (id int);\n",
        )
        .unwrap();
        std::fs::write(dir.join("config.yaml"), "table: user_table\n").unwrap();
        std::fs::write(dir.join("App.scala"), "val t = \"user_table\"\n").unwrap();
        std::fs::write(dir.join("package-lock.json"), "{\"user_table\": 1}\n").unwrap();

        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        let (text, structured) = search_text(&server, "user_table", false, None);
        assert!(text.contains("@main.py:2 "), "{text}");
        for anchor in [
            "@README.md:1 [text-only]",
            "@schema.sql:1 [text-only]",
            "@config.yaml:1 [text-only]",
            "@App.scala:1 [text-only]",
        ] {
            assert!(text.contains(anchor), "missing {anchor}: {text}");
        }
        assert!(
            !text.contains("package-lock.json"),
            "lock files skipped: {text}"
        );
        // Code hits rank before text-only hits.
        assert!(
            text.find("@main.py").unwrap() < text.find("@README.md").unwrap(),
            "{text}"
        );
        assert_eq!(structured["text_only_matches"], json!(4));

        // Edit a text file, then query: the new content is visible.
        std::fs::write(dir.join("README.md"), "Renamed to account_table.\n").unwrap();
        let (text, _) = search_text(&server, "account_table", false, None);
        assert!(text.contains("@README.md:1 [text-only]"), "{text}");
        let (text, _) = search_text(&server, "read user_table", false, None);
        assert!(!text.contains("README.md"), "stale text hit: {text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_text_only_covers_common_config_types() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "x = 1\n").unwrap();
        let files = [
            "Cargo.toml",
            "pyproject.toml",
            "config.toml",
            "setup.cfg",
            "app.ini",
            "nginx.conf",
            "app.properties",
            ".env",
            ".env.local",
            "ci.yml",
            "build.gradle.kts",
            "Dockerfile",
            "Makefile",
            "run.sh",
            "api.proto",
            "schema.graphql",
            "data.csv",
        ];
        for name in files {
            std::fs::write(dir.join(name), "marker_value_42\n").unwrap();
        }
        std::fs::write(dir.join("Cargo.lock"), "marker_value_42\n").unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;
        let (text, _) = search_text(&server, "marker_value_42", false, None);
        for name in files {
            let anchor = format!("@{name}:1");
            assert!(text.contains(&anchor), "missing {anchor}: {text}");
        }
        assert!(text.contains("@Cargo.toml:1 [text-only]"), "{text}");
        assert!(
            !text.contains("Cargo.lock"),
            "Cargo.lock must be skipped: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_case_rules_and_override() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def LoadUser():\n    pass\n").unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        // Literal default: case-insensitive.
        assert!(search_text(&server, "loaduser", false, None)
            .0
            .contains("@main.py:1"));
        // Literal forced case-sensitive.
        assert!(!search_text(&server, "loaduser", false, Some(true))
            .0
            .contains("@main.py:1"));
        // Regex default: case-sensitive.
        assert!(!search_text(&server, "load.ser", true, None)
            .0
            .contains("@main.py:1"));
        // Regex forced case-insensitive.
        assert!(search_text(&server, "load.ser", true, Some(false))
            .0
            .contains("@main.py:1"));
        // Literal special characters are escaped, not treated as regex.
        assert!(!search_text(&server, "Load.ser", false, None)
            .0
            .contains("@main.py:1"));
        // Smart case: an uppercase letter makes a literal case-sensitive,
        // and the case-only variant is counted in a footnote.
        let (text, _) = search_text(&server, "LOADUSER", false, None);
        assert!(!text.contains("@main.py:1"), "{text}");
        assert!(
            text.contains("differ") && text.contains("only by case"),
            "{text}"
        );
        assert!(text.contains("case_sensitive=false"), "{text}");
        assert!(search_text(&server, "LoadUser", false, None)
            .0
            .contains("@main.py:1"));
        assert!(search_text(&server, "LOADUSER", false, Some(false))
            .0
            .contains("@main.py:1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_languages_threshold_under_three_does_not_hint() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def main(): pass\n").unwrap();
        // 仅 2 个 .scala 文件 (N = 2 < 3)
        std::fs::write(dir.join("A.scala"), "class A\n").unwrap();
        std::fs::write(dir.join("B.scala"), "class B\n").unwrap();

        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        // search_code 零命中：N<3 不应触发提示
        let search_miss = server.search_code(Parameters(SearchParams {
            root: None,
            index_root: None,
            query: "nonexistent".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }));
        let search_text = search_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            search_text.contains("(no matching code lines)"),
            "{search_text}"
        );
        assert!(
            !search_text.contains("unsupported languages"),
            "{search_text}"
        );

        // find_symbol 零命中：N<3 不应触发提示
        let symbol_miss = server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: None,
            query: "NonExistent".into(),
            budget_tokens: 2500,
            offset: 0,
            depth: 0,
            substring: None,
            path_filter: None,
            kind: None,
            include_body: false,
        }));
        let symbol_text = symbol_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            symbol_text.contains("(no matching symbols)"),
            "{symbol_text}"
        );
        assert!(
            !symbol_text.contains("unsupported language"),
            "{symbol_text}"
        );

        // get_languages: >=1 就显示统计，这里有 2 个 .scala 仍会显示
        let langs_res = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 2500,
            host_lsp: None,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            langs_text.contains("unsupported-language source files by type: .scala×2"),
            "{langs_text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_languages_none_when_all_supported() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def main(): pass\n").unwrap();
        std::fs::write(dir.join("app.dart"), "void main() {}\n").unwrap();

        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        let langs_res = server.get_languages(Parameters(BudgetOnly {
            root: None,
            index_root: None,
            budget_tokens: 2500,
            host_lsp: None,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            langs_text.contains("unsupported-language source files: none"),
            "{langs_text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_descriptions_declare_indexed_languages_boundary() {
        let server = ready_server();
        let tools = server.listed_tools();
        let find_symbol = tools.iter().find(|t| t.name == "find_symbol").unwrap();
        let desc = find_symbol.description.as_deref().unwrap_or("");
        assert!(
            desc.contains("indexed languages"),
            "find_symbol description missing indexed languages notice: {desc}"
        );
        // search_code also covers non-indexed text files and says so.
        let tool = tools.iter().find(|t| t.name == "search_code").unwrap();
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(desc.contains("[text-only]"), "{desc}");
        assert!(
            desc.contains("case-sensitive only if query has uppercase"),
            "{desc}"
        );
    }
}

/// Multi-root routing, schema size, resource LRU and building-time search.
#[cfg(test)]
mod mcp_routing_tests {
    use super::*;
    use std::time::Duration;

    fn tmp(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-route-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn multi_server(names: &[&str]) -> (AstrolabeServer, PathBuf) {
        let parent = tmp("multi");
        let mut children = Vec::new();
        for name in names {
            let dir = parent.join(name);
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("src").join("lib.rs"),
                format!("pub fn handler_{name}() -> u8 {{\n    1 // shared\n}}\n"),
            )
            .unwrap();
            children.push(dir);
        }
        let knobs = crate::knobs::McpKnobs {
            watch_mode: None,
            resident_roots: 3,
            loop_guard: crate::loop_guard::LoopGuardConfig {
                repeat_max: 0,
                rate_max: 0,
                ..Default::default()
            },
            ..crate::knobs::McpKnobs::default()
        };
        let mut server =
            AstrolabeServer::with_multi_project_knobs(parent.clone(), children, knobs, 8 << 20);
        server.structured_output = false;
        server.start_indexing();
        (server, parent)
    }

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    fn sym(query: &str, root: Option<&str>, path_filter: Option<&str>) -> QueryParams {
        QueryParams {
            root: root.map(str::to_string),
            index_root: None,
            query: query.into(),
            include_body: false,
            substring: None,
            kind: None,
            path_filter: path_filter.map(str::to_string),
            depth: 0,
            budget_tokens: 800,
            offset: 0,
        }
    }

    /// Retry while the routed child is still doing its cold build.
    fn until_ready(mut call: impl FnMut() -> CallToolResult) -> String {
        let start = std::time::Instant::now();
        loop {
            let out = text(&call());
            if !out.contains("Index is building in the background")
                || start.elapsed() > Duration::from_secs(30)
            {
                return out;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn explicit_root_selects_sub_repo_for_every_tool() {
        let (server, parent) = multi_server(&["api", "web", "docs", "core"]);
        let found =
            until_ready(|| server.find_symbol(Parameters(sym("handler_web", Some("web"), None))));
        assert!(found.contains("handler_web"), "{found}");
        assert!(
            found.starts_with(&format!("index_root: {}", parent.join("web").display())),
            "{found}"
        );
        // The query string is a symbol name: it is never reinterpreted as a
        // routing hint, so naming another repo in it cannot misroute.
        let miss = until_ready(|| {
            server.find_symbol(Parameters(sym("api handler_api", Some("api"), None)))
        });
        assert!(
            miss.contains("index_root:") && miss.contains("api"),
            "{miss}"
        );
        assert!(!miss.contains("cannot route"), "{miss}");
        // Case-insensitive name and absolute path both work.
        let by_abs = until_ready(|| {
            server.find_symbol(Parameters(sym(
                "handler_core",
                Some(parent.join("core").to_str().unwrap()),
                None,
            )))
        });
        assert!(by_abs.contains("handler_core"), "{by_abs}");
        let langs = until_ready(|| {
            server.get_languages(Parameters(BudgetOnly {
                root: Some("DOCS".into()),
                index_root: None,
                budget_tokens: 200,
                host_lsp: None,
            }))
        });
        assert!(
            langs.starts_with(&format!("index_root: {}", parent.join("docs").display())),
            "{langs}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn path_filter_routes_and_is_rewritten_relative_to_the_sub_repo() {
        let (server, parent) = multi_server(&["api", "web"]);
        let out = until_ready(|| {
            server.search_code(Parameters(SearchParams {
                root: None,
                index_root: None,
                path_filter: Some("api/src".into()),
                glob: None,
                ..search_params("shared")
            }))
        });
        assert!(
            out.starts_with(&format!("index_root: {}", parent.join("api").display())),
            "{out}"
        );
        assert!(!out.contains("matches no"), "{out}");
        assert!(out.contains("lib.rs"), "{out}");
        // Bare sub-repo name routes and drops the (now empty) filter.
        let bare = until_ready(|| {
            server.search_code(Parameters(SearchParams {
                root: None,
                index_root: None,
                path_filter: Some("web/".into()),
                glob: None,
                ..search_params("shared")
            }))
        });
        assert!(bare.contains("web") && bare.contains("lib.rs"), "{bare}");
        assert!(!bare.contains("matches no"), "{bare}");
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    fn search_params(query: &str) -> SearchParams {
        SearchParams {
            root: None,
            index_root: None,
            query: query.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1500,
            offset: 0,
            context_lines: None,
            page: false,
            group: None,
            dedup: None,
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }
    }

    #[test]
    fn unroutable_call_names_the_root_parameter_and_unknown_root_is_rejected() {
        let (server, parent) = multi_server(&["api", "web"]);
        let none = text(&server.find_symbol(Parameters(sym("handler_web", None, None))));
        assert!(
            none.contains("cannot route") && none.contains("root"),
            "{none}"
        );
        assert!(none.contains("e.g. root="), "{none}");
        let bad = text(&server.find_symbol(Parameters(sym("x", Some("nope"), None))));
        assert!(bad.contains("`nope` matches no"), "{bad}");
        assert!(bad.contains("api") && bad.contains("web"), "{bad}");
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn sole_resident_sub_repo_serves_calls_without_root() {
        let (server, parent) = multi_server(&["api", "web"]);
        let first =
            until_ready(|| server.find_symbol(Parameters(sym("handler_api", Some("api"), None))));
        assert!(first.contains("handler_api"), "{first}");
        let second = until_ready(|| server.find_symbol(Parameters(sym("handler_api", None, None))));
        assert!(second.contains("handler_api"), "{second}");
        assert!(!second.contains("cannot route"), "{second}");
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn task_text_routing_is_guessed_noted_and_ignores_generic_words() {
        let (server, parent) = multi_server(&["api", "web", "billing"]);
        // "web" is an everyday word: prose alone must not pick a repo.
        let prose = text(&server.resolve_context(Parameters(ResolveContextParams {
            root: None,
            index_root: None,
            task_description: "set up a web server".into(),
            budget_tokens: 400,
            dedup: None,
            offset: 0,
            path_filter: None,
        })));
        assert!(prose.contains("cannot route"), "{prose}");
        // A distinctive name routes, and the result says it was a guess.
        let guessed = until_ready(|| {
            server.resolve_context(Parameters(ResolveContextParams {
                root: None,
                index_root: None,
                task_description: "fix the invoice rounding in billing".into(),
                budget_tokens: 400,
                dedup: None,
                offset: 0,
                path_filter: None,
            }))
        });
        assert!(
            guessed.contains("Routing note:") && guessed.contains("from names in the task text"),
            "{guessed}"
        );
        assert!(guessed.contains("billing"), "{guessed}");
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn absolute_path_routes_precise_tools_to_the_owning_sub_repo() {
        let (server, parent) = multi_server(&["api", "web"]);
        let file = parent.join("web").join("src").join("lib.rs");
        let out = text(&server.get_diagnostics(Parameters(
            crate::precise_tools::GetDiagnosticsParams {
                root: None,
                index_root: None,
                path: file.to_string_lossy().into_owned(),
                severity: None,
                budget_tokens: 300,
                lsp: None,
                toolchain: true,
            },
        )));
        assert!(
            out.starts_with(&format!("index_root: {}", parent.join("web").display())),
            "{out}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn per_call_index_root_overrides_startup_root() {
        // Startup indexes repo A; calls with index_root=B must hit B only.
        let parent = tmp("idxroot");
        let a = parent.join("repo_a");
        let b = parent.join("repo_b");
        for (dir, name) in [(&a, "alpha_only"), (&b, "beta_only")] {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            std::fs::write(
                dir.join("src").join("lib.rs"),
                format!("pub fn {name}() -> u8 {{ 1 }}\n"),
            )
            .unwrap();
        }
        let knobs = crate::knobs::McpKnobs {
            watch_mode: None,
            loop_guard: crate::loop_guard::LoopGuardConfig {
                repeat_max: 0,
                rate_max: 0,
                ..Default::default()
            },
            ..crate::knobs::McpKnobs::default()
        };
        let mut server = AstrolabeServer::new_with_knobs(a.clone(), knobs, 8 << 20);
        server.structured_output = false;
        server.start_indexing();

        let on_a = until_ready(|| {
            server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: None,
                query: "alpha_only".into(),
                include_body: false,
                substring: None,
                kind: None,
                path_filter: None,
                depth: 0,
                budget_tokens: 800,
                offset: 0,
            }))
        });
        assert!(on_a.contains("alpha_only"), "{on_a}");
        assert!(
            on_a.starts_with(&format!(
                "index_root: {}",
                a.canonicalize().unwrap().display()
            )),
            "{on_a}"
        );

        let on_b = until_ready(|| {
            server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: Some(b.to_string_lossy().into_owned()),
                query: "beta_only".into(),
                include_body: false,
                substring: None,
                kind: None,
                path_filter: None,
                depth: 0,
                budget_tokens: 800,
                offset: 0,
            }))
        });
        assert!(on_b.contains("beta_only"), "{on_b}");
        assert!(
            on_b.starts_with(&format!(
                "index_root: {}",
                b.canonicalize().unwrap().display()
            )),
            "{on_b}"
        );
        assert!(!on_b.contains("alpha_only"), "{on_b}");

        // Relative index_root resolves against process cwd.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&parent).unwrap();
        let rel = until_ready(|| {
            server.search_code(Parameters(SearchParams {
                root: None,
                index_root: Some("repo_b".into()),
                query: "beta_only".into(),
                ..search_params("beta_only")
            }))
        });
        std::env::set_current_dir(&prev).unwrap();
        assert!(
            rel.starts_with(&format!(
                "index_root: {}",
                b.canonicalize().unwrap().display()
            )),
            "{rel}"
        );
        assert!(rel.contains("beta_only"), "{rel}");

        // Nonexistent path errors in English without indexing.
        let bad = text(&server.find_symbol(Parameters(QueryParams {
            root: None,
            index_root: Some("/no/such/astrolabe/index_root_xyz".into()),
            query: "x".into(),
            include_body: false,
            substring: None,
            kind: None,
            path_filter: None,
            depth: 0,
            budget_tokens: 200,
            offset: 0,
        })));
        assert!(bad.contains("not an existing path"), "{bad}");
        assert!(bad.contains("confidence: unknown"), "{bad}");

        // Two roots do not mix: A-only symbol under index_root=B has no hit.
        let miss = until_ready(|| {
            server.find_symbol(Parameters(QueryParams {
                root: None,
                index_root: Some(b.to_string_lossy().into_owned()),
                query: "alpha_only".into(),
                include_body: false,
                substring: None,
                kind: None,
                path_filter: None,
                depth: 0,
                budget_tokens: 800,
                offset: 0,
            }))
        });
        assert!(
            miss.starts_with(&format!(
                "index_root: {}",
                b.canonicalize().unwrap().display()
            )),
            "{miss}"
        );
        assert!(
            !miss.lines().any(|l| l.contains("alpha_only") && (l.contains(".rs:") || l.contains("function"))),
            "mixed hit from repo A under index_root=B: {miss}"
        );

        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn schemas_are_slim_and_catalog_stays_small() {
        let server = AstrolabeServer::new(tmp("schema"));
        let tools = server.listed_tools();
        assert_eq!(tools.len(), 7);
        for tool in &tools {
            let json = serde_json::to_string(&*tool.input_schema).unwrap();
            assert!(!json.contains("$schema"), "{}: {json}", tool.name);
            assert!(!json.contains("\"default\":null"), "{}: {json}", tool.name);
            assert!(!json.contains("\"format\":\"uint"), "{}: {json}", tool.name);
            assert!(!json.contains("\"minimum\":0"), "{}: {json}", tool.name);
            // `root` only matters in multi-repo workspaces: hidden here.
            assert!(
                tool.input_schema
                    .get("properties")
                    .and_then(|p| p.get("root"))
                    .is_none(),
                "{} advertises `root` in a single-repo session",
                tool.name
            );
            // Per-call override must always be advertised (single- and multi-repo).
            assert!(
                tool.input_schema
                    .get("properties")
                    .and_then(|p| p.get("index_root"))
                    .is_some(),
                "{} must advertise optional index_root",
                tool.name
            );
            let required = tool
                .input_schema
                .get("required")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            assert!(!required.iter().any(|r| r == "root"), "{}", tool.name);
        }
        // Compact JSON of the whole `tools` array. v0.13.0 shipped 7,535 B,
        // v0.15.0 7,3xx B; hiding never-used parameters halves it.
        let bytes = serde_json::to_vec(&tools).unwrap().len();
        assert!(
            bytes <= TOOLS_LIST_MAX_BYTES,
            "tools/list is {bytes} B, limit {TOOLS_LIST_MAX_BYTES} B (v0.15.0: ~7,300 B)"
        );
    }

    /// Upper bound for the compact `tools` array. Raise only with a reason.
    const TOOLS_LIST_MAX_BYTES: usize = 5_200;

    /// Snapshot of the advertised parameters. Everything else is still
    /// accepted (see `unadvertised_params_are_still_accepted`); add a name
    /// here only when A/B runs show models need it.
    #[test]
    fn advertised_parameters_snapshot() {
        let server = AstrolabeServer::new(tmp("snapshot"));
        let props = |tool: &rmcp::model::Tool| -> Vec<String> {
            let mut names: Vec<String> = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        };
        let required = |tool: &rmcp::model::Tool| -> Vec<String> {
            tool.input_schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let got: Vec<(String, Vec<String>, Vec<String>)> = server
            .listed_tools()
            .iter()
            .map(|t| (t.name.to_string(), props(t), required(t)))
            .collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let want = vec![
            (
                "find_references".to_string(),
                s(&["index_root", "line", "path", "symbol"]),
                s(&["symbol"]),
            ),
            (
                "find_symbol".to_string(),
                s(&["include_body", "index_root", "path_filter", "query"]),
                s(&["query"]),
            ),
            (
                "get_diagnostics".to_string(),
                s(&["index_root", "path"]),
                s(&["path"]),
            ),
            ("get_languages".to_string(), s(&["index_root"]), s(&[])),
            (
                "goto_definition".to_string(),
                s(&["index_root", "line", "path", "symbol"]),
                s(&["path"]),
            ),
            (
                "resolve_context".to_string(),
                s(&["index_root", "path_filter", "task_description"]),
                s(&["task_description"]),
            ),
            (
                "search_code".to_string(),
                s(&[
                    "aggregate",
                    "case_sensitive",
                    "index_root",
                    "path_filter",
                    "query",
                    "skip_comments",
                ]),
                s(&["query"]),
            ),
        ];
        assert_eq!(got, want);
        // Multi-repo sessions advertise `root` on every tool.
        let (multi, _parent) = multi_server(&["alpha", "beta"]);
        for tool in multi.listed_tools() {
            assert!(props(&tool).iter().any(|p| p == "root"), "{}", tool.name);
            assert!(
                props(&tool).iter().any(|p| p == "index_root"),
                "{}",
                tool.name
            );
            assert!(
                !required(&tool).iter().any(|p| p == "root"),
                "{}",
                tool.name
            );
            assert!(
                !required(&tool).iter().any(|p| p == "index_root"),
                "{}",
                tool.name
            );
        }
        multi.stop_watching();
    }

    #[test]
    fn unadvertised_params_are_still_accepted() {
        let q: QueryParams = serde_json::from_value(json!({
            "query": "x", "root": "r", "kind": "function", "depth": 1,
            "substring": true, "budget_tokens": 900, "offset": 3
        }))
        .unwrap();
        assert_eq!((q.depth, q.budget_tokens, q.offset), (1, 900, 3));
        let sp: SearchParams = serde_json::from_value(json!({
            "query": "x", "regex": true, "budget_tokens": 900, "offset": 2,
            "context_lines": 1, "page": true, "group": false, "dedup": false,
            "exclude": ["vendor"], "count_only": true
        }))
        .unwrap();
        assert!(sp.regex && sp.page && sp.count_only);
        let r: crate::precise_tools::FindReferencesParams = serde_json::from_value(json!({
            "symbol": "X", "character": 2, "budget_tokens": 10, "offset": 1,
            "dry_run_rename": "Y"
        }))
        .unwrap();
        assert!(r.path.is_none(), "path is optional");
        assert_eq!(r.dry_run_rename.as_deref(), Some("Y"));
        let d: crate::precise_tools::GetDiagnosticsParams =
            serde_json::from_value(json!({ "path": "a.go", "severity": "error" })).unwrap();
        assert_eq!(d.severity.as_deref(), Some("error"));
        let c: ResolveContextParams = serde_json::from_value(json!({
            "task_description": "t", "dedup": false, "offset": 4, "budget_tokens": 5
        }))
        .unwrap();
        assert_eq!(c.offset, 4);
        let _: BudgetOnly =
            serde_json::from_value(json!({ "root": "r", "budget_tokens": 5 })).unwrap();
    }

    /// Claude Code treats an MCP tool as concurrency-safe only when
    /// `annotations.readOnlyHint` is true; without it, several Astrolabe calls
    /// in one turn run one after another. The bytes are paid for by dropping
    /// the `["x","null"]` unions of optional parameters.
    #[test]
    fn every_tool_is_read_only_and_optional_params_have_plain_types() {
        let server = AstrolabeServer::new(tmp("annotations"));
        let tools = server.listed_tools();
        assert_eq!(tools.len(), crate::TOOL_COUNT);
        for tool in &tools {
            let json = serde_json::to_value(tool).unwrap();
            assert_eq!(
                json.get("annotations"),
                Some(&json!({ "readOnlyHint": true })),
                "{}",
                tool.name
            );
            let schema = serde_json::to_string(&*tool.input_schema).unwrap();
            assert!(!schema.contains("\"null\""), "{}: {schema}", tool.name);
        }
        let find_symbol = tools.iter().find(|t| t.name == "find_symbol").unwrap();
        let schema = Value::Object((*find_symbol.input_schema).clone());
        let path_filter = schema.pointer("/properties/path_filter/type");
        assert_eq!(path_filter, Some(&json!("string")));
        // Explicit nulls from clients that still send them deserialize as None.
        let params: QueryParams =
            serde_json::from_value(json!({ "query": "x", "root": null, "kind": null })).unwrap();
        assert!(params.root.is_none() && params.kind.is_none());
        let params: SearchParams = serde_json::from_value(
            json!({ "query": "x", "exclude": null, "case_sensitive": null }),
        )
        .unwrap();
        assert!(params.exclude.is_none() && params.case_sensitive.is_none());
    }

    #[test]
    fn slim_schema_collapses_only_two_member_null_unions() {
        let mut v = json!({
            "a": { "type": ["integer", "null"] },
            "b": { "type": ["null", "boolean"] },
            "c": { "type": ["string", "integer"] },
            "d": { "type": "string" },
        });
        slim_schema(&mut v);
        assert_eq!(
            v,
            json!({
                "a": { "type": "integer" },
                "b": { "type": "boolean" },
                "c": { "type": ["string", "integer"] },
                "d": { "type": "string" },
            })
        );
    }

    #[test]
    fn resource_store_is_lru_bounded_and_uris_carry_a_process_nonce() {
        let mut store = ResourceStore::with_limits(3, 1_000);
        for i in 0..5 {
            store.insert(format!("u{i}"), "x".repeat(10));
        }
        assert_eq!(store.len(), 3, "entry cap");
        assert!(store.get("u0").is_none() && store.get("u1").is_none());
        assert!(store.get("u4").is_some());
        // Touching u2 makes u3 the next victim.
        assert!(store.get("u2").is_some());
        store.insert("u5".into(), "y".into());
        assert!(store.get("u3").is_none(), "LRU victim");
        assert!(store.get("u2").is_some());
        // Byte cap evicts old entries but always keeps the newest.
        let mut bytes = ResourceStore::with_limits(100, 25);
        bytes.insert("a".into(), "x".repeat(10));
        bytes.insert("b".into(), "x".repeat(10));
        bytes.insert("c".into(), "x".repeat(10));
        assert!(bytes.get("a").is_none() && bytes.get("c").is_some());
        bytes.insert("huge".into(), "x".repeat(100));
        assert_eq!(bytes.len(), 1, "an oversized entry stays alone");
        // A new process gets a different nonce window; same-process stores
        // built at different instants differ too.
        let a = ResourceStore::new();
        std::thread::sleep(Duration::from_millis(2));
        let b = ResourceStore::new();
        assert_ne!(a.nonce, b.nonce);
        let server = AstrolabeServer::new(tmp("res"));
        let big = "z".repeat(40_000);
        let result = server.result("t", big, json!({}), Some(100));
        let link = text(&result);
        assert!(link.contains("astrolabe://results/"), "{link}");
        let uri_tail = link
            .split("astrolabe://results/")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap();
        assert!(
            uri_tail.contains('-'),
            "uri carries nonce-counter: {uri_tail}"
        );
    }

    #[test]
    fn search_code_answers_from_disk_while_the_index_is_building() {
        let dir = tmp("building");
        std::fs::write(dir.join("notes.md"), "needle_text appears here\nnothing\n").unwrap();
        std::fs::write(dir.join("a.py"), "def f():\n    return 'needle_text'\n").unwrap();
        // Never call start_indexing: the session stays in Building.
        let mut server = AstrolabeServer::new(dir.clone());
        server.structured_output = false;
        let out = text(&server.search_code(Parameters(search_params("needle_text"))));
        assert!(!out.contains("retry shortly"), "{out}");
        assert!(
            out.contains("on-disk source literal scan") && out.contains("[text-only]"),
            "{out}"
        );
        assert!(
            out.contains("notes.md:1:") && out.contains("a.py:2:"),
            "{out}"
        );
        assert!(out.contains("2 hits / 2 files"), "{out}");
        // path_filter and count_only apply.
        let counted = text(&server.search_code(Parameters(SearchParams {
            index_root: None,
            path_filter: Some("a.py".into()),
            count_only: true,
            glob: None,
            ..search_params("needle_text")
        })));
        assert!(counted.contains("1 hit / 1 file"), "{counted}");
        assert!(!counted.contains("a.py:2:"), "{counted}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_params_on_standard_methods_are_invalid_params() {
        let bad = custom_request_error("tools/call");
        assert_eq!(bad.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let unknown = custom_request_error("frobnicate/now");
        assert_eq!(unknown.code, rmcp::model::ErrorCode::METHOD_NOT_FOUND);
        assert!(unknown.message.contains("frobnicate/now"));
    }

    #[test]
    fn cancel_flag_is_visible_only_inside_its_scope() {
        assert!(!cancel::is_cancelled());
        let flag = Arc::new(AtomicBool::new(false));
        cancel::scope(Arc::clone(&flag), || {
            assert!(!cancel::is_cancelled());
            flag.store(true, Ordering::Relaxed);
            assert!(cancel::is_cancelled());
        });
        assert!(!cancel::is_cancelled());
    }
}
