//! Precise symbol queries on top of [`LanguageServer`].
//!
//! Call-graph edges in this project are name-matched. Measured against a
//! language server they recall 66% of TypeScript references and 18% of Python
//! ones, so "where is this symbol referenced" must not be answered from the
//! graph. This module is the wrapper callers actually use: it locates a
//! symbol, talks to a server, and never presents a failure as an empty hit
//! list.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::types::{Confidence, Language, RelPath};

use super::anchor::{self, Anchor, MAX_DEFINITION_ANCHORS};
use super::{LanguageServer, Location, LspError, Position, Precise};

/// How a caller identifies the symbol to query.
///
/// Language-server requests are position-based. When the caller only has a
/// name, [`anchor::locate`] parses the file and anchors on the name token of
/// every declaration of it (comments, strings and docstrings are skipped);
/// with no declaration in the file it uses the first code occurrence and
/// follows `definition` from there. Several definitions are all queried and
/// merged. Callers who already know the cursor should pass [`SymbolAt::Position`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolAt<'a> {
    /// Zero-based line and UTF-16 column, matching the LSP position encoding.
    Position(Position),
    /// Declarations of this name in the file (else its first code use).
    Name(&'a str),
}

/// Marker put in a result note when a by-name query matched several
/// declarations and their answers were merged. Callers (the MCP rename
/// preview) refuse to present such a list as one symbol's exact checklist.
pub const AMBIGUOUS_DEFINITIONS_MARK: &str = "[ambiguous-definitions]";

const TEXT_SEARCH_HINT: &str =
    "search_code can fall back to a text search but will have false positives";

/// Every reference to the symbol at `target`, including its declaration.
///
/// Hits are converted from `file://` URIs to repo-relative [`RelPath`]s,
/// locations outside the repository are dropped (and named in `note` when
/// that happens), then the list is de-duplicated and sorted. The sort puts
/// the queried file first and then orders by path and position, so a caller
/// that truncates to the first *N* items still keeps the most local hits and
/// a deterministic remainder.
///
/// A missing, crashed, or timed-out server returns [`Confidence::Unknown`]
/// with a non-empty `note`. An empty `value` with [`Confidence::Exact`] and
/// no note means the server ran and found nothing — not that we skipped the
/// check.
pub fn find_references(
    server: Option<&dyn LanguageServer>,
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
) -> Precise<Vec<Location>> {
    let Some(server) = server else {
        return unavailable_without_server(path);
    };
    let sites = match resolve_query_site(server, repo_root, path, target) {
        Ok(sites) => sites,
        // The name→declaration hop can time out too: answer by text search.
        Err(SiteError::Lsp(err)) => return references_failed(repo_root, path, target, &err),
        Err(SiteError::Failed(failed)) => return failed,
    };
    let mut hits = Vec::new();
    let mut errors = Vec::new();
    let mut attempted = 0;
    for (query_path, query_pos) in &sites.sites {
        attempted += 1;
        match server.references(query_path, *query_pos) {
            Ok(found) => hits.extend(found),
            Err(err) => {
                let timed_out = matches!(err, LspError::Timeout(_));
                errors.push(err);
                // One timeout is a verdict on the server; do not stack
                // another 30s per remaining anchor.
                if timed_out {
                    break;
                }
            }
        }
    }
    if errors.len() == attempted {
        return references_failed(repo_root, path, target, &errors[0]);
    }
    let mut result = guard_cold_index(finalize_hits(hits, repo_root, path), server);
    let all_external = result.value.is_empty() && result.note.is_some();
    let mut notes: Vec<String> = result.note.take().into_iter().collect();
    cap_confidence(&mut result.confidence, sites.cap);
    if let Some(note) = sites.note {
        notes.push(note);
    }
    if !errors.is_empty() {
        result.confidence = Confidence::Unknown;
        notes.push(format!(
            "{} of {} anchor queries failed ({}); results may be incomplete",
            errors.len(),
            sites.sites.len(),
            errors[0]
        ));
    }
    if sites.in_comment {
        result.confidence = Confidence::Unknown;
        notes.push(
            "the given position is inside a comment or string, not on a code identifier; \
             pass the line/character of the symbol's name token"
                .to_string(),
        );
    }
    if result.value.is_empty() && !all_external && result.confidence == Confidence::Exact {
        // `includeDeclaration` is set, so a correctly anchored symbol always
        // yields at least its own declaration. Empty means a bad anchor.
        result.confidence = Confidence::Unknown;
        notes.push(format!(
            "language server returned no references (not even the declaration) at {}; \
             the anchor is probably not on the symbol. Retry with line/character of the \
             declaration, or use search_code. {TEXT_SEARCH_HINT}",
            describe_sites(&sites.sites)
        ));
    }
    result.note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    result
}

fn describe_sites(sites: &[(RelPath, Position)]) -> String {
    sites
        .iter()
        .map(|(p, pos)| format!("{}:{}:{}", p.as_str(), pos.line + 1, pos.character + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Swift / Objective-C answer of last resort for [`find_references`].
///
/// sourcekit-lsp and clangd answer `[]` (or nothing) whenever they have no
/// index for the file: no SwiftPM package or build, no compile flags, code
/// inside an `#if` that is inactive on this host (`canImport(Security)` on
/// Linux), or the server is not installed. Rather than an empty Unknown that
/// sends an agent off to grep by hand, return the tree-sitter code
/// occurrences of the name, labelled [`Confidence::Syntactic`] with a note
/// that says why. Any other result (including a non-empty or `Exact` one) is
/// returned unchanged, and other languages are never touched.
pub fn apple_references_fallback(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    result: Precise<Vec<Location>>,
) -> Precise<Vec<Location>> {
    apple_references_fallback_in(repo_root, path, target, result, None)
}

/// [`apple_references_fallback`] over a known file list (the admitted paths
/// of a fresh index) instead of a fresh directory walk.
pub fn apple_references_fallback_in(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    result: Precise<Vec<Location>>,
    files: Option<&[RelPath]>,
) -> Precise<Vec<Location>> {
    if !result.value.is_empty() || result.confidence != Confidence::Unknown {
        return result;
    }
    if !is_apple_source(repo_root, path) {
        return result;
    }
    let name = match target {
        SymbolAt::Name(name) if !name.is_empty() => name.to_string(),
        SymbolAt::Name(_) => return result,
        SymbolAt::Position(pos) => match super::fallback::identifier_at(repo_root, path, pos) {
            Some(name) => name,
            None => return result,
        },
    };
    let reason = result
        .note
        .as_deref()
        .and_then(|n| n.split(['\n', ';']).next())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("language server returned no references")
        .to_string();
    super::fallback::syntactic_references(repo_root, path, &name, &reason, files)
}

fn is_apple_source(repo_root: &Path, path: &RelPath) -> bool {
    match Language::from_path(path) {
        Some(Language::Swift | Language::ObjC | Language::ObjCpp) => true,
        Some(Language::C) => {
            crate::text::read_source(&repo_root.join(path.as_str())).is_ok_and(|src| {
                matches!(
                    Language::from_path_and_source(path, &src),
                    Some(Language::ObjC | Language::ObjCpp)
                )
            })
        }
        _ => false,
    }
}

/// Positions a references query should be issued at, plus anything the
/// caller must be told about how they were chosen.
struct QuerySites {
    sites: Vec<(RelPath, Position)>,
    note: Option<String>,
    /// A caller-given position that sits inside a comment / string.
    in_comment: bool,
    /// Highest confidence the merged answer may claim: several declarations
    /// of one name were merged (`Scoped`) or some were dropped (`Syntactic`).
    cap: Option<Confidence>,
}

/// Why no references query site could be produced.
enum SiteError {
    /// The language server itself failed (e.g. the name→declaration hop);
    /// kept structured so timeouts can fall back to text search.
    Lsp(LspError),
    /// Already-rendered Unknown result (name not found, only in comments…).
    Failed(Precise<Vec<Location>>),
}

/// Answer for a references query whose LSP request failed: text search on
/// timeout (when the name is known), otherwise an Unknown degrade.
fn references_failed(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    err: &LspError,
) -> Precise<Vec<Location>> {
    if matches!(err, LspError::Timeout(_)) {
        if let Some(fallback) = timeout_fallback(repo_root, path, target, &err.to_string(), None) {
            return fallback;
        }
    }
    degrade(err)
}

/// Text-search answer for a timed-out references query, when we can tell
/// which name was asked about.
fn timeout_fallback(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    reason: &str,
    files: Option<&[RelPath]>,
) -> Option<Precise<Vec<Location>>> {
    let name = match target {
        SymbolAt::Name(name) if !name.is_empty() => name.to_string(),
        SymbolAt::Name(_) => return None,
        SymbolAt::Position(pos) => super::fallback::identifier_at(repo_root, path, pos)?,
    };
    let reason = reason.split(';').next().unwrap_or(reason);
    Some(super::fallback::text_references(
        repo_root, path, &name, reason, files,
    ))
}

/// Whole-word text matches for the symbol at `target`, labelled syntactic,
/// for callers that need an answer while the server cannot give one yet
/// (still importing / indexing). `None` when no identifier can be read.
pub fn text_fallback_references(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    reason: &str,
) -> Option<Precise<Vec<Location>>> {
    timeout_fallback(repo_root, path, target, reason, None)
}

/// [`text_fallback_references`] over a known file list (the admitted paths
/// of a fresh index) instead of a fresh directory walk.
pub fn text_fallback_references_in(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    reason: &str,
    files: Option<&[RelPath]>,
) -> Option<Precise<Vec<Location>>> {
    timeout_fallback(repo_root, path, target, reason, files)
}

/// Identifier under `position` in `path` (UTF-16 column), if any.
pub fn identifier_at(repo_root: &Path, path: &RelPath, position: Position) -> Option<String> {
    super::fallback::identifier_at(repo_root, path, position)
}

/// Refuse to call an empty result authoritative while the server is still
/// indexing.
///
/// A cold `rust-analyzer` answers `textDocument/references` with `[]` rather
/// than an error, so reporting it as `exact` would tell an agent "this symbol
/// has no references" — and an agent that believes that will happily delete
/// live code. A non-empty result is kept as-is: those hits are real, they may
/// merely be incomplete.
fn guard_cold_index<T>(result: Precise<Vec<T>>, server: &dyn LanguageServer) -> Precise<Vec<T>> {
    if !server.is_busy() {
        return result;
    }
    if !result.value.is_empty() {
        // The hits are real but a half-built index can miss some; never
        // let a caller treat this list as the complete set.
        return downgrade_incomplete(result);
    }
    const COLD: &str = "语言服务器仍在建立索引，返回的空结果不可信。\
         请在索引完成后重试。";
    // Preserve finalize_hits notes (e.g. all-external definitions) rather than
    // wholesale-replacing them when the empty result is also untrustworthy.
    let note = match result.note {
        Some(existing) if !existing.is_empty() => format!("{existing}; {COLD}"),
        _ => COLD.to_string(),
    };
    Precise::unknown(result.value, note)
}

/// Definitions of the symbol at `target`.
///
/// Same URI conversion, out-of-repo handling, de-duplication, and sort as
/// [`find_references`]. Out-of-repo definitions (stdlib, vendored deps) are
/// not stuffed into [`RelPath`]; they are described in `note` instead.
///
/// Accepts [`SymbolAt`] so MCP callers can pass either a cursor or a name
/// (first identifier occurrence in `path`). Empty results from a still-busy
/// server are downgraded via [`guard_cold_index`] — same rule as references.
pub fn goto_definition(
    server: Option<&dyn LanguageServer>,
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
) -> Precise<Vec<Location>> {
    let Some(server) = server else {
        return unavailable_definition_without_server(path);
    };
    let resolved = match resolve_definition_positions(repo_root, path, target) {
        Ok(resolved) => resolved,
        Err(failed) => return failed,
    };
    let mut hits = Vec::new();
    let mut errors = Vec::new();
    for position in &resolved.positions {
        match server.definition(path, *position) {
            Ok(found) => hits.extend(found),
            Err(err) => errors.push(err),
        }
    }
    if errors.len() == resolved.positions.len() {
        return degrade_definition(&errors[0]);
    }
    let mut result = guard_cold_index(finalize_hits(hits, repo_root, path), server);
    cap_confidence(&mut result.confidence, resolved.cap);
    let mut notes: Vec<String> = result.note.take().into_iter().collect();
    if let Some(note) = resolved.note {
        notes.push(note);
    }
    if !errors.is_empty() {
        result.confidence = Confidence::Unknown;
        notes.push(format!(
            "{} of {} definition queries failed ({}); results may be incomplete",
            errors.len(),
            resolved.positions.len(),
            errors[0]
        ));
    }
    result.note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    result
}

struct DefinitionPositions {
    positions: Vec<Position>,
    note: Option<String>,
    cap: Option<Confidence>,
}

/// Locate the cursor(s) for a definition query without anchoring through a
/// prior `definition` round-trip (unlike [`resolve_named_site`] for references).
/// A name with several declarations yields one position per declaration (at
/// most [`MAX_DEFINITION_ANCHORS`]) and a note saying so.
fn resolve_definition_positions(
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
) -> Result<DefinitionPositions, Precise<Vec<Location>>> {
    let single = |position| DefinitionPositions {
        positions: vec![position],
        note: None,
        cap: None,
    };
    match target {
        SymbolAt::Position(position) => Ok(single(position)),
        SymbolAt::Name(name) => match locate_named(repo_root, path, name, "definitions") {
            Ok(Anchor::Definitions(mut defs)) => {
                let total = defs.len();
                if total > MAX_DEFINITION_ANCHORS {
                    defs.truncate(MAX_DEFINITION_ANCHORS);
                    Ok(DefinitionPositions {
                        note: Some(format!(
                            "{AMBIGUOUS_DEFINITIONS_MARK} `{name}` has {total} declarations in {}; \
                             definitions of the first {MAX_DEFINITION_ANCHORS} only (incomplete). \
                             Pass line/character to target one",
                            path.as_str()
                        )),
                        positions: defs,
                        cap: Some(Confidence::Syntactic),
                    })
                } else if total > 1 {
                    Ok(DefinitionPositions {
                        note: Some(format!(
                            "{AMBIGUOUS_DEFINITIONS_MARK} `{name}` has {total} declarations in {}; \
                             definitions of all of them are listed. Pass line/character to target one",
                            path.as_str()
                        )),
                        positions: defs,
                        cap: Some(Confidence::Scoped),
                    })
                } else {
                    Ok(single(defs[0]))
                }
            }
            Ok(Anchor::Usage(position)) => Ok(single(position)),
            Ok(_) => unreachable!("mapped to Err"),
            Err(reason) => Err(Precise::unknown(Vec::new(), reason)),
        },
    }
}

/// `textDocument/hover` 的方法名。传输层实现该往返（`LspSession`）时应复用
/// 这个常量与 [`hover_params`]，保证线上请求形态与此处的解析永远一致。
pub const HOVER_METHOD: &str = "textDocument/hover";

/// hover 不可用时的降级提示。与引用/定义查询的 `TEXT_SEARCH_HINT` 不同：
/// 文本搜索帮不上 hover，调用方合理的降级是只用符号名做锚点。
const HOVER_FALLBACK_HINT: &str = "callers can fall back to a plain symbol-name anchor";

/// textDocument/hover：取光标处符号的 docstring / 类型 / 签名信息。
///
/// 这是 Serena `include_info` / `request_hover` 的对应物。返回渲染后的
/// Markdown 文本（`Hover.contents` 各段拼接，见 [`parse_hover`]）；无信息
/// 时 `value` 为 `None`，调用方降级为符号名锚点即可。
///
/// 语义级置信度：结论来自语言服务器的语义分析，区别于调用图那种按名字
/// 匹配的句法信息。服务器缺失、超时或崩溃时返回 [`Confidence::Unknown`]
/// 并附说明，绝不把失败伪装成“没有信息”；就绪服务器的空回答保持
/// [`Confidence::Exact`]——但仍在索引的服务器会用空 hover 冒充它，见
/// `guard_cold_hover`（与 [`find_references`] 的 `guard_cold_index` 同一条
/// 规则）。
///
/// 命名定位走 [`resolve_hover_site`]（非 references 的 `resolve_query_site`）：
/// 先取首个标识符；定义唯一且服务器非 busy 时锚定到声明处。定义锚定失败
/// 时回退到首标识符再发 hover，避免超时叠加上且不把 references 降级文案
/// 渗进 hover。
///
/// `issue` 是到传输层的缝：它替本函数发出一次 hover 往返（方法名
/// [`HOVER_METHOD`]、params 用 [`hover_params`] 构造），返回原始 `result`。
/// 主线在 `LanguageServer` 上补齐该往返后，以
/// `|server, path, position| server.hover(&path, position)` 接入。
pub fn hover<I>(
    server: Option<&dyn LanguageServer>,
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
    issue: I,
) -> Precise<Option<String>>
where
    I: FnOnce(&dyn LanguageServer, RelPath, Position) -> Result<Value, LspError>,
{
    let Some(server) = server else {
        return unavailable_hover_without_server(path);
    };
    let (query_path, query_pos) = match resolve_hover_site(server, repo_root, path, target) {
        Ok(site) => site,
        Err(failed) => return failed,
    };
    match issue(server, query_path, query_pos) {
        Ok(result) => guard_cold_hover(Precise::exact(parse_hover(&result)), server),
        Err(err) => degrade_hover(&err),
    }
}

/// Named/position site for hover. Unlike [`resolve_named_site`] (references):
/// - failure notes are hover-specific (never `TEXT_SEARCH_HINT` / search_code);
/// - definition-anchor `Err` falls back to the first identifier, then hover;
/// - a unique in-repo definition is ignored while the server is busy/cold so
///   a skewed cold-index hit is not treated as an Exact hover anchor.
fn resolve_hover_site(
    server: &dyn LanguageServer,
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
) -> Result<(RelPath, Position), Precise<Option<String>>> {
    match target {
        SymbolAt::Position(position) => Ok((path.clone(), position)),
        SymbolAt::Name(name) => resolve_named_hover_site(server, repo_root, path, name),
    }
}

fn resolve_named_hover_site(
    server: &dyn LanguageServer,
    repo_root: &Path,
    path: &RelPath,
    name: &str,
) -> Result<(RelPath, Position), Precise<Option<String>>> {
    if name.is_empty() {
        return Err(hover_locate_failed(
            "empty symbol name; cannot provide precise hover information",
        ));
    }

    let position = match locate_named(repo_root, path, name, "hover information") {
        // A declaration in this file is already the docstring site.
        Ok(Anchor::Definitions(defs)) => return Ok((path.clone(), defs[0])),
        Ok(Anchor::Usage(position)) => position,
        Ok(_) => unreachable!("mapped to Err"),
        Err(reason) => return Err(hover_locate_failed(reason)),
    };

    match server.definition(path, position) {
        // Definition round-trip failed: still hover at the first identifier.
        Err(_) => Ok((path.clone(), position)),
        Ok(defs) => {
            let mut in_repo = classify_all(defs, repo_root).in_repo;
            in_repo.sort();
            // Unique declaration is the docstring site — but not while cold:
            // a busy server's single hit can be skewed; stay on the occurrence.
            if in_repo.len() == 1 && !server.is_busy() {
                let loc = &in_repo[0];
                Ok((loc.path.clone(), loc.range.start))
            } else {
                Ok((path.clone(), position))
            }
        }
    }
}

/// Site-failure degrade for hover: hover-specific reason + [`HOVER_FALLBACK_HINT`]
/// only — never references / `search_code` / [`TEXT_SEARCH_HINT`].
fn hover_locate_failed(reason: impl AsRef<str>) -> Precise<Option<String>> {
    let reason = sanitize_hover_site_reason(reason.as_ref());
    Precise::unknown(None, format!("{reason}. {HOVER_FALLBACK_HINT}"))
}

/// Strip any references-oriented trails that may have leaked into a site note.
fn sanitize_hover_site_reason(reason: &str) -> String {
    let mut r = reason.replace(TEXT_SEARCH_HINT, "");
    r = r.replace(
        "cannot provide precise references",
        "cannot provide precise hover information",
    );
    // Collapse leftover punctuation/whitespace from stripping the hint.
    while r.contains("  ") {
        r = r.replace("  ", " ");
    }
    r = r.replace(" .", ".");
    r = r.replace(";;", ";");
    r.trim()
        .trim_end_matches(|c: char| c == '.' || c == ';' || c.is_whitespace())
        .to_string()
}

/// 定位失败（名字找不到 / 文件读不出）映射到 hover 的返回形态。
/// 只保留 hover 说明 + [`HOVER_FALLBACK_HINT`]，不含 references/`search_code`。
#[cfg(test)]
fn hover_site_failed(failed: Precise<Vec<Location>>) -> Precise<Option<String>> {
    let reason = failed
        .note
        .unwrap_or_else(|| "could not locate the symbol to hover".into());
    hover_locate_failed(reason)
}

/// `guard_cold_index` 的 `Option<String>` 版：就绪服务器的“无信息”是权威
/// 答案，但仍在索引的服务器会用空 hover 冒充它。有文本时原样保留——那
/// 些内容是真实的，至多不全。
/// Note attached to non-empty results from a server that is still indexing.
pub const STILL_INDEXING_NOTE: &str = "language server still indexing, results may be incomplete";

fn downgrade_incomplete<T>(mut result: Precise<T>) -> Precise<T> {
    result.note = Some(match result.note.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}; {STILL_INDEXING_NOTE}"),
        _ => STILL_INDEXING_NOTE.to_string(),
    });
    if result.confidence == Confidence::Exact {
        result.confidence = Confidence::Syntactic;
    }
    result
}

fn guard_cold_hover(
    result: Precise<Option<String>>,
    server: &dyn LanguageServer,
) -> Precise<Option<String>> {
    if !server.is_busy() {
        return result;
    }
    if result.value.is_some() {
        return downgrade_incomplete(result);
    }
    Precise::unknown(
        result.value,
        "语言服务器仍在建立索引，返回的空结果不可信。\
         请在索引完成后重试。",
    )
}

fn degrade_hover(err: &LspError) -> Precise<Option<String>> {
    Precise::unknown(
        None,
        format!("{err}; cannot provide precise hover information. {HOVER_FALLBACK_HINT}"),
    )
}

fn unavailable_hover_without_server(path: &RelPath) -> Precise<Option<String>> {
    let label = Language::from_path(path)
        .map(server_label)
        .unwrap_or("language server");
    Precise::unknown(
        None,
        format!("{label} is not installed; cannot provide precise hover information. {HOVER_FALLBACK_HINT}"),
    )
}

/// 构造 `textDocument/hover` 的 params。行/列 0-based、列为 UTF-16，与
/// [`find_references`] 的坐标习惯一致；`uri` 由传输层按 workspace 根生成。
pub fn hover_params(uri: &str, position: Position) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": position.line, "character": position.character },
    })
}

/// 解析 `textDocument/hover` 的 `result`，取出 `contents` 的文本。
///
/// 覆盖三种形态（`range` 一律忽略）：
///
/// - MarkupContent：`{ "contents": { "kind": "markdown", "value": "…" } }`
///   （`{ "language": "go", "value": "…" }` 的 MarkedString 对象同样只取
///   `value`）；
/// - 老客户端数组：`{ "contents": [ "…", { "value": "…" }, … ] }`，逐段
///   取出后以空行拼接成 Markdown；
/// - 裸字符串：`{ "contents": "…" }`。
///
/// `result` 为 `null`、`contents` 缺失、或取出后只剩空白 → `None`。
pub fn parse_hover(result: &Value) -> Option<String> {
    let contents = match result {
        Value::Null => return None,
        value => value.get("contents")?,
    };
    let parts: Vec<String> = match contents {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items.iter().filter_map(marked_string_text).collect(),
        single => marked_string_text(single).into_iter().collect(),
    };
    let joined = parts.join("\n\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// MarkedString / MarkupContent 单元素 → 文本。无法识别的形态返回 `None`，
/// 让数组里的其它段继续参与拼接。
fn marked_string_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(_) => value
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Convert an LSP path — `file://` URI, absolute path, or already-relative
/// path — into a repo-relative [`RelPath`].
///
/// Returns `None` when the target sits outside `repo_root`, percent-decoding
/// fails, or the path would escape the repository with `..`.
pub fn lsp_path_to_rel(path: &str, repo_root: &Path) -> Option<RelPath> {
    if looks_like_file_uri(path) {
        let abs = abs_path_from_lsp(path)?;
        return rel_under_root(&abs, repo_root);
    }

    let normalized = path.replace('\\', "/");
    let as_path = Path::new(&normalized);
    if as_path.is_absolute() {
        return rel_under_root(as_path, repo_root);
    }

    // `RelPath::new` strips a leading `/`, so a Unix absolute path that has
    // already been wrapped may arrive as `Users/…/repo/src/a.rs`. Recover it
    // when it still carries the repo root as a prefix.
    if let Some(rel) = strip_root_string(&normalized, repo_root) {
        return Some(rel);
    }

    if normalized.split('/').any(|seg| seg == "..") {
        return None;
    }

    Some(RelPath::new(normalized))
}

fn resolve_query_site(
    server: &dyn LanguageServer,
    repo_root: &Path,
    path: &RelPath,
    target: SymbolAt<'_>,
) -> Result<QuerySites, SiteError> {
    match target {
        SymbolAt::Position(position) => {
            let in_comment = crate::text::read_source(&repo_root.join(path.as_str()))
                .ok()
                .and_then(|src| anchor::position_in_comment_or_string(path, &src, position))
                .unwrap_or(false);
            Ok(QuerySites {
                sites: vec![(path.clone(), position)],
                note: None,
                in_comment,
                cap: None,
            })
        }
        SymbolAt::Name(name) => resolve_named_site(server, repo_root, path, name),
    }
}

/// Read `path` and pick an anchor for `name`, mapping failures to an Unknown
/// result whose note ends with `what` ("references", "definitions", …).
fn locate_named(
    repo_root: &Path,
    path: &RelPath,
    name: &str,
    what: &str,
) -> Result<Anchor, String> {
    if name.is_empty() {
        return Err(format!("empty symbol name; cannot provide precise {what}"));
    }
    let source = crate::text::read_source(&repo_root.join(path.as_str())).map_err(|err| {
        format!(
            "could not read {} to locate `{name}` ({err}); cannot provide precise {what}",
            path.as_str()
        )
    })?;
    match anchor::locate(path, &source, name) {
        Anchor::NotFound => Err(format!(
            "symbol `{name}` not found in {}; cannot provide precise {what}",
            path.as_str()
        )),
        Anchor::OnlyInCommentsOrStrings => Err(format!(
            "`{name}` only appears inside comments or strings in {}; refusing to anchor there. \
             Pass the file that declares or uses it, or line/character; cannot provide precise {what}",
            path.as_str()
        )),
        found => Ok(found),
    }
}

fn resolve_named_site(
    server: &dyn LanguageServer,
    repo_root: &Path,
    path: &RelPath,
    name: &str,
) -> Result<QuerySites, SiteError> {
    let anchor = locate_named(repo_root, path, name, "references").map_err(|reason| {
        SiteError::Failed(Precise::unknown(
            Vec::new(),
            format!("{reason}. {TEXT_SEARCH_HINT}"),
        ))
    })?;
    let sites: Vec<(RelPath, Position)> = match anchor {
        Anchor::Definitions(defs) => defs.into_iter().map(|p| (path.clone(), p)).collect(),
        Anchor::Usage(position) => match server.definition(path, position) {
            Err(err) => return Err(SiteError::Lsp(err)),
            Ok(defs) => {
                let mut in_repo = classify_all(defs, repo_root).in_repo;
                in_repo.sort();
                in_repo.dedup();
                if in_repo.is_empty() {
                    // Out-of-repo or unresolved: let the server pick from the use.
                    vec![(path.clone(), position)]
                } else {
                    in_repo
                        .into_iter()
                        .map(|loc| (loc.path, loc.range.start))
                        .collect()
                }
            }
        },
        Anchor::NotFound | Anchor::OnlyInCommentsOrStrings => unreachable!("mapped to Err"),
    };
    let total = sites.len();
    let mut note = None;
    let mut cap = None;
    let mut sites = sites;
    if total > MAX_DEFINITION_ANCHORS {
        sites.truncate(MAX_DEFINITION_ANCHORS);
        cap = Some(Confidence::Syntactic);
        note = Some(format!(
            "{AMBIGUOUS_DEFINITIONS_MARK} `{name}` has {total} definitions; merged results of the first {MAX_DEFINITION_ANCHORS} only (incomplete). \
             Pass line/character to target one"
        ));
    } else if total > 1 {
        cap = Some(Confidence::Scoped);
        note = Some(format!(
            "{AMBIGUOUS_DEFINITIONS_MARK} `{name}` has {total} definitions ({}); results of all of them were merged, \
             so this is not the list for one symbol. Pass line/character to target one",
            describe_sites(&sites)
        ));
    }
    Ok(QuerySites {
        sites,
        note,
        in_comment: false,
        cap,
    })
}

/// Lower `confidence` to at most `cap` (Exact > Scoped > Syntactic > Unknown).
fn cap_confidence(confidence: &mut Confidence, cap: Option<Confidence>) {
    let rank = |c: Confidence| match c {
        Confidence::Exact => 0,
        Confidence::Scoped => 1,
        Confidence::Syntactic => 2,
        Confidence::Unknown => 3,
    };
    if let Some(cap) = cap {
        if rank(*confidence) < rank(cap) {
            *confidence = cap;
        }
    }
}

fn finalize_hits(
    hits: Vec<Location>,
    repo_root: &Path,
    origin: &RelPath,
) -> Precise<Vec<Location>> {
    let classified = classify_all(hits, repo_root);
    let mut in_repo = classified.in_repo;
    sort_locations(&mut in_repo, origin);

    if !classified.external.is_empty() {
        tracing::debug!(
            dropped = classified.external.len(),
            "filtered language-server locations outside the repository"
        );
    }

    let note = external_note(&in_repo, &classified.external);
    Precise {
        value: in_repo,
        confidence: Confidence::Exact,
        note,
    }
}

struct Classified {
    in_repo: Vec<Location>,
    external: Vec<String>,
}

fn classify_all(hits: Vec<Location>, repo_root: &Path) -> Classified {
    let mut in_repo = Vec::new();
    let mut external = Vec::new();
    for hit in hits {
        match lsp_path_to_rel(hit.path.as_str(), repo_root) {
            Some(path) => in_repo.push(Location {
                path,
                range: hit.range,
            }),
            None => external.push(display_external(hit.path.as_str())),
        }
    }
    Classified { in_repo, external }
}

fn sort_locations(locs: &mut Vec<Location>, origin: &RelPath) {
    locs.sort_by(|a, b| {
        (a.path != *origin)
            .cmp(&(b.path != *origin))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.range.cmp(&b.range))
    });
    locs.dedup();
}

fn external_note(in_repo: &[Location], external: &[String]) -> Option<String> {
    if external.is_empty() {
        return None;
    }
    let shown = format_external_list(external);
    if in_repo.is_empty() {
        Some(format!(
            "{} 条结果在仓库外（标准库或依赖），全部已省略；all {} result(s) sit outside the repository (stdlib or dependencies) and were omitted: {shown}",
            external.len(),
            external.len()
        ))
    } else {
        Some(format!(
            "omitted {} reference(s) outside the repository (stdlib or dependencies): {shown}",
            external.len()
        ))
    }
}

fn format_external_list(external: &[String]) -> String {
    const SHOW: usize = 3;
    if external.len() <= SHOW {
        return external.join(", ");
    }
    format!(
        "{}, … ({} total)",
        external[..SHOW].join(", "),
        external.len()
    )
}

fn display_external(raw: &str) -> String {
    abs_path_from_lsp(raw)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| raw.to_string())
}

fn degrade(err: &LspError) -> Precise<Vec<Location>> {
    Precise::unknown(
        Vec::new(),
        format!("{err}; cannot provide precise references. {TEXT_SEARCH_HINT}"),
    )
}

fn degrade_definition(err: &LspError) -> Precise<Vec<Location>> {
    Precise::unknown(
        Vec::new(),
        format!("{err}; cannot provide precise definitions. {TEXT_SEARCH_HINT}"),
    )
}

fn unavailable_without_server(path: &RelPath) -> Precise<Vec<Location>> {
    let label = Language::from_path(path)
        .map(server_label)
        .unwrap_or("language server");
    Precise::unknown(
        Vec::new(),
        format!("{label} is not installed; cannot provide precise references. {TEXT_SEARCH_HINT}"),
    )
}

fn unavailable_definition_without_server(path: &RelPath) -> Precise<Vec<Location>> {
    let label = Language::from_path(path)
        .map(server_label)
        .unwrap_or("language server");
    Precise::unknown(
        Vec::new(),
        format!("{label} is not installed; cannot provide precise definitions. {TEXT_SEARCH_HINT}"),
    )
}

fn server_label(language: Language) -> &'static str {
    match language {
        Language::Go => "gopls",
        Language::Rust => "rust-analyzer",
        Language::Python => "pyright",
        Language::Java => "jdtls",
        Language::TypeScript | Language::Tsx | Language::JavaScript => "typescript-language-server",
        Language::Swift | Language::ObjC | Language::ObjCpp => "sourcekit-lsp",
        Language::C | Language::Cpp => "clangd",
        Language::Php => "intelephense",
        Language::Vue => "vue-language-server",
        Language::Dart => "dart language-server",
        Language::CSharp | Language::VisualBasic => "Microsoft.CodeAnalysis.LanguageServer",
        Language::Kotlin => "kotlin-lsp",
        Language::Shell => "bash-language-server",
        Language::Ruby => "ruby-lsp",
        Language::Lua => "lua-language-server",
    }
}

fn looks_like_file_uri(path: &str) -> bool {
    path.len() >= 5 && path[..5].eq_ignore_ascii_case("file:")
}

fn abs_path_from_lsp(raw: &str) -> Option<PathBuf> {
    if looks_like_file_uri(raw) {
        return file_uri_to_path(raw);
    }
    let normalized = raw.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute() {
        return Some(lexical_normalize(path));
    }
    None
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = if looks_like_file_uri(uri) {
        &uri[5..]
    } else {
        return None;
    };
    let rest = rest.split_once('#').map(|(head, _)| head).unwrap_or(rest);
    let rest = rest.split_once('?').map(|(head, _)| head).unwrap_or(rest);
    let decoded = percent_decode(rest)?;

    if let Some(after_authority) = decoded.strip_prefix("//") {
        let slash = after_authority.find('/')?;
        let host = &after_authority[..slash];
        let path = &after_authority[slash..];
        if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
            Some(local_fs_path(path))
        } else {
            // UNC / remote host: never treat as in-repo.
            None
        }
    } else {
        Some(local_fs_path(&decoded))
    }
}

fn local_fs_path(path: &str) -> PathBuf {
    // `file:///C:/Users/…` yields `/C:/Users/…`. On Windows that leading slash
    // has to come off so `Path` sees a drive prefix.
    #[cfg(windows)]
    {
        let bytes = path.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
            return lexical_normalize(Path::new(&path[1..]));
        }
    }
    lexical_normalize(Path::new(path))
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let high = from_hex(bytes[i + 1])?;
            let low = from_hex(bytes[i + 2])?;
            out.push((high << 4) | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn rel_under_root(abs: &Path, repo_root: &Path) -> Option<RelPath> {
    let abs_lex = lexical_normalize(abs);
    let root_lex = lexical_normalize(repo_root);
    if let Some(rel) = strip_prefix_path(&abs_lex, &root_lex) {
        return path_to_rel(rel);
    }
    if let (Ok(abs_c), Ok(root_c)) = (abs.canonicalize(), repo_root.canonicalize()) {
        if let Some(rel) = strip_prefix_path(&abs_c, &root_c) {
            return path_to_rel(rel);
        }
    }
    None
}

fn strip_prefix_path<'a>(abs: &'a Path, root: &Path) -> Option<&'a Path> {
    let rel = abs.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    Some(rel)
}

fn strip_root_string(raw: &str, repo_root: &Path) -> Option<RelPath> {
    let raw = raw.trim_start_matches('/');
    let root = lexical_normalize(repo_root);
    let root = root.to_string_lossy().replace('\\', "/");
    let root = root.trim_start_matches('/');
    // A single-component root such as `/repo` is also a plausible first
    // path segment inside the workspace (`repo/src/a.rs`). Require at
    // least one extra component so we only recover paths that look like
    // a Unix absolute path after `RelPath::new` stripped its leading `/`.
    if root.is_empty() || !root.contains('/') {
        return None;
    }
    let rest = raw.strip_prefix(root)?;
    // Require a path-separator boundary so `/Users/me/proj` does not steal
    // `Users/me/projectile/src`.
    if rest.is_empty() || !rest.starts_with('/') {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() || rest.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(RelPath::new(rest))
}

fn path_to_rel(rel: &Path) -> Option<RelPath> {
    let s = rel.to_str()?.replace('\\', "/");
    if s.is_empty() || s.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(RelPath::new(s))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::lsp::{Diagnostic, Range, WorkspaceEdit};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn apple_fallback_replaces_only_empty_unknown_swift_and_objc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("A.swift"),
            "struct Widget {}\nlet w = Widget()\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("B.swift"),
            "// Widget\nfunc f(_ w: Widget) {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("a.go"), "type Widget struct{}\n").unwrap();
        let unknown = || Precise::unknown(Vec::new(), "language server returned no references; x");
        let swift = RelPath::new("A.swift");

        let r = apple_references_fallback(dir.path(), &swift, SymbolAt::Name("Widget"), unknown());
        assert_eq!(r.confidence, Confidence::Syntactic);
        assert_eq!(r.value.len(), 3, "{:?}", r.value);
        assert!(r
            .note
            .unwrap()
            .starts_with("language server returned no references."));

        // By position too.
        let pos = Position {
            line: 1,
            character: 8,
        };
        let r = apple_references_fallback(dir.path(), &swift, SymbolAt::Position(pos), unknown());
        assert_eq!(r.value.len(), 3);

        // Exact answers (even empty ones) and other languages are untouched.
        let exact = apple_references_fallback(
            dir.path(),
            &swift,
            SymbolAt::Name("Widget"),
            Precise::exact(Vec::new()),
        );
        assert_eq!(exact.confidence, Confidence::Exact);
        let go = apple_references_fallback(
            dir.path(),
            &RelPath::new("a.go"),
            SymbolAt::Name("Widget"),
            unknown(),
        );
        assert_eq!(go.confidence, Confidence::Unknown);
        assert!(go.value.is_empty());
    }

    #[derive(Clone)]
    enum Script {
        Ok(Vec<Location>),
        Unavailable(&'static str),
        Timeout,
        Crashed,
        Startup,
        Protocol,
    }

    impl Script {
        fn result(&self, language: Language) -> Result<Vec<Location>, LspError> {
            match self {
                Script::Ok(hits) => Ok(hits.clone()),
                Script::Unavailable(msg) => Err(LspError::Unavailable(language, (*msg).into())),
                Script::Timeout => Err(LspError::Timeout(Duration::from_secs(8))),
                Script::Crashed => Err(LspError::Crashed),
                Script::Startup => Err(LspError::Startup(language, "spawn failed".into())),
                Script::Protocol => Err(LspError::Protocol("broken response".into())),
            }
        }
    }

    struct FakeServer {
        language: Language,
        references: Script,
        definition: Script,
        ref_calls: Mutex<Vec<(RelPath, Position)>>,
        def_calls: Mutex<Vec<(RelPath, Position)>>,
        busy: bool,
    }

    impl FakeServer {
        fn still_indexing() -> Self {
            Self {
                busy: true,
                ..Self::refs(Vec::new())
            }
        }

        fn refs(hits: Vec<Location>) -> Self {
            Self {
                language: Language::Go,
                references: Script::Ok(hits),
                definition: Script::Ok(Vec::new()),
                ref_calls: Mutex::new(Vec::new()),
                def_calls: Mutex::new(Vec::new()),
                busy: false,
            }
        }

        fn failing(script: Script) -> Self {
            Self {
                language: Language::Go,
                references: script.clone(),
                definition: script,
                ref_calls: Mutex::new(Vec::new()),
                def_calls: Mutex::new(Vec::new()),
                busy: false,
            }
        }
    }

    impl LanguageServer for FakeServer {
        fn language(&self) -> Language {
            self.language
        }

        fn hover(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<serde_json::Value, LspError> {
            Ok(serde_json::Value::Null)
        }

        fn references(
            &self,
            path: &RelPath,
            position: Position,
        ) -> Result<Vec<Location>, LspError> {
            self.ref_calls
                .lock()
                .unwrap()
                .push((path.clone(), position));
            self.references.result(self.language)
        }

        fn definition(
            &self,
            path: &RelPath,
            position: Position,
        ) -> Result<Vec<Location>, LspError> {
            self.def_calls
                .lock()
                .unwrap()
                .push((path.clone(), position));
            self.definition.result(self.language)
        }

        fn diagnostics(&self, _path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
            Ok(Vec::new())
        }

        fn prepare_rename(
            &self,
            _path: &RelPath,
            _position: Position,
            _new_name: &str,
        ) -> Result<WorkspaceEdit, LspError> {
            Ok(WorkspaceEdit::default())
        }

        fn is_busy(&self) -> bool {
            self.busy
        }

        fn memory_bytes(&self) -> Option<u64> {
            None
        }

        fn shutdown(&self) -> Result<(), LspError> {
            Ok(())
        }
    }

    struct TempRepo {
        root: PathBuf,
    }

    impl TempRepo {
        fn new(files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "astrolabe-lsp-queries-{}-{}",
                std::process::id(),
                TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            for (rel, body) in files {
                let path = root.join(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(path, body).unwrap();
            }
            TempRepo { root }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn loc(path: &str, line: u32, character: u32) -> Location {
        Location {
            path: RelPath::new(path),
            range: Range {
                start: pos(line, character),
                end: pos(line, character + 1),
            },
        }
    }

    fn assert_unknown_empty(result: &Precise<Vec<Location>>) {
        assert!(result.value.is_empty());
        assert_eq!(result.confidence, Confidence::Unknown);
        let note = result.note.as_deref().unwrap_or("");
        assert!(!note.is_empty(), "Unknown results must explain themselves");
    }

    #[test]
    fn references_sort_dedup_and_prefer_origin_file() {
        let server = FakeServer::refs(vec![
            loc("pkg/z.go", 4, 2),
            loc("pkg/a.go", 9, 0),
            loc("pkg/a.go", 1, 5),
            loc("pkg/a.go", 1, 5),
            loc("pkg/z.go", 4, 2),
            loc("pkg/origin.go", 3, 1),
            loc("pkg/a.go", 1, 0),
        ]);
        let origin = RelPath::new("pkg/origin.go");
        let first = find_references(
            Some(&server),
            Path::new("/repo"),
            &origin,
            SymbolAt::Position(pos(3, 1)),
        );
        let second = find_references(
            Some(&server),
            Path::new("/repo"),
            &origin,
            SymbolAt::Position(pos(3, 1)),
        );

        assert_eq!(first.confidence, Confidence::Exact);
        assert!(first.note.is_none());
        assert_eq!(
            first.value,
            vec![
                loc("pkg/origin.go", 3, 1),
                loc("pkg/a.go", 1, 0),
                loc("pkg/a.go", 1, 5),
                loc("pkg/a.go", 9, 0),
                loc("pkg/z.go", 4, 2),
            ]
        );
        assert_eq!(first.value, second.value);
    }

    #[test]
    fn empty_references_are_unknown_not_exact() {
        let server = FakeServer::refs(Vec::new());
        let result = find_references(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("main.go"),
            SymbolAt::Position(pos(0, 0)),
        );
        assert!(result.value.is_empty());
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.note.is_some());
    }

    #[test]
    fn file_uri_percent_decodes_spaces_and_unicode() {
        let root = Path::new("/repo/my project");
        assert_eq!(
            lsp_path_to_rel("file:///repo/my%20project/src/a.rs", root)
                .unwrap()
                .as_str(),
            "src/a.rs"
        );
        assert_eq!(
            lsp_path_to_rel("file:///repo/my%20project/src/weird%20name.go", root)
                .unwrap()
                .as_str(),
            "src/weird name.go"
        );
        assert_eq!(
            lsp_path_to_rel("file:///repo/my%20project/src/%E6%B5%8B.go", root)
                .unwrap()
                .as_str(),
            "src/测.go"
        );
        assert_eq!(
            lsp_path_to_rel("FILE://localhost/repo/my%20project/lib.rs", root)
                .unwrap()
                .as_str(),
            "lib.rs"
        );
        // `+` is a plus in file URIs, not a space.
        assert_eq!(
            lsp_path_to_rel("file:///repo/my%20project/a+b.rs", root)
                .unwrap()
                .as_str(),
            "a+b.rs"
        );
    }

    #[test]
    fn references_convert_file_uris_end_to_end() {
        let server = FakeServer::refs(vec![
            loc("file:///repo/src/has%20space.go", 2, 0),
            loc("file:///repo/src/lib.go", 8, 4),
        ]);
        let result = find_references(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/lib.go"),
            SymbolAt::Position(pos(1, 0)),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value[0].path.as_str(), "src/lib.go");
        assert_eq!(result.value[1].path.as_str(), "src/has space.go");
    }

    #[test]
    fn out_of_repo_paths_are_omitted_and_named() {
        let server = FakeServer::refs(vec![
            loc("file:///repo/src/main.go", 1, 0),
            loc("file:///usr/lib/go/src/fmt/print.go", 40, 2),
            loc("file:///Users/me/go/pkg/mod/github.com/x@v1/y.go", 3, 0),
        ]);
        let result = find_references(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/main.go"),
            SymbolAt::Position(pos(1, 0)),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value, vec![loc("src/main.go", 1, 0)]);
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("outside the repository"));
        assert!(note.contains("/usr/lib/go/src/fmt/print.go"));
    }

    #[test]
    fn all_external_hits_stay_exact_with_an_explanatory_note() {
        let server = FakeServer::refs(vec![loc("file:///usr/lib/go/src/fmt/print.go", 1, 0)]);
        let result = find_references(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/main.go"),
            SymbolAt::Position(pos(1, 0)),
        );
        assert!(result.value.is_empty());
        assert_eq!(result.confidence, Confidence::Exact);
        assert!(result
            .note
            .as_deref()
            .unwrap()
            .contains("sit outside the repository"));
    }

    #[test]
    fn missing_server_is_unknown_with_a_non_empty_note() {
        let result = find_references(
            None,
            Path::new("/repo"),
            &RelPath::new("cmd/app.go"),
            SymbolAt::Position(pos(0, 0)),
        );
        assert_unknown_empty(&result);
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("gopls is not installed"));
        assert!(note.contains("search_code"));
        assert!(note.contains("false positives"));
    }

    #[test]
    fn unavailable_error_is_unknown_with_a_non_empty_note() {
        let server = FakeServer::failing(Script::Unavailable(
            "gopls not found on PATH; install with `go install golang.org/x/tools/gopls@latest`",
        ));
        let result = find_references(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("main.go"),
            SymbolAt::Position(pos(0, 0)),
        );
        assert_unknown_empty(&result);
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("gopls not found on PATH"));
        assert!(note.contains("search_code"));
        assert_ne!(result.confidence, Confidence::Exact);
    }

    #[test]
    fn timeout_and_crash_are_unknown_not_empty_exact() {
        for script in [
            Script::Timeout,
            Script::Crashed,
            Script::Startup,
            Script::Protocol,
        ] {
            let server = FakeServer::failing(script);
            let refs = find_references(
                Some(&server),
                Path::new("/repo"),
                &RelPath::new("main.go"),
                SymbolAt::Position(pos(0, 0)),
            );
            assert_unknown_empty(&refs);
            assert!(refs.note.as_deref().unwrap().contains("search_code"));

            let defs = goto_definition(
                Some(&server),
                Path::new("/repo"),
                &RelPath::new("main.go"),
                SymbolAt::Position(pos(0, 0)),
            );
            assert_unknown_empty(&defs);
            let def_note = defs.note.as_deref().unwrap();
            assert!(
                def_note.contains("definitions"),
                "goto_definition degrade must be definition-specific, got: {def_note}"
            );
            assert!(
                !def_note.contains("precise references"),
                "goto_definition must not reuse references degrade text, got: {def_note}"
            );
            assert!(def_note.contains("search_code"));
        }
    }

    #[test]
    fn goto_definition_normalizes_and_filters() {
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Ok(vec![
            loc("file:///repo/src/lib.go", 10, 4),
            loc("file:///repo/src/lib.go", 10, 4),
            loc("file:///usr/lib/go/src/builtin/builtin.go", 1, 0),
        ]);
        let result = goto_definition(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/main.go"),
            SymbolAt::Position(pos(3, 2)),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value, vec![loc("src/lib.go", 10, 4)]);
        assert!(result.note.as_deref().unwrap().contains("omitted 1"));
    }

    #[test]
    fn named_query_anchors_on_declaration_without_definition_roundtrip() {
        let repo = TempRepo::new(&[(
            "src/main.go",
            "package p\n\nfunc helper() {}\n\nfunc Run() { helper() }\n",
        )]);
        let mut server =
            FakeServer::refs(vec![loc("src/main.go", 2, 5), loc("src/main.go", 4, 13)]);
        server.definition = Script::Ok(vec![loc("src/main.go", 2, 5)]);

        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("src/main.go"),
            SymbolAt::Name("helper"),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value.len(), 2);

        // The declaration is in this file, so no definition round-trip.
        assert!(server.def_calls.lock().unwrap().is_empty());
        // `func helper` — after "func " the UTF-16 column of `helper` is 5.

        let ref_calls = server.ref_calls.lock().unwrap().clone();
        assert_eq!(ref_calls, vec![(RelPath::new("src/main.go"), pos(2, 5))]);
    }

    #[test]
    fn named_query_skips_partial_identifier_matches() {
        let repo = TempRepo::new(&[("pkg/util.go", "func foobar() { foo() }\n")]);
        let server = FakeServer::refs(vec![loc("pkg/util.go", 0, 16)]);
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("pkg/util.go"),
            SymbolAt::Name("foo"),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        let def_calls = server.def_calls.lock().unwrap().clone();
        assert_eq!(def_calls[0].1, pos(0, 16));
    }

    #[test]
    fn named_query_utf16_column_counts_cjk_and_emoji() {
        let repo = TempRepo::new(&[("src/lib.go", "你好 😀 foo\n")]);
        let server = FakeServer::refs(Vec::new());
        let _ = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("src/lib.go"),
            SymbolAt::Name("foo"),
        );
        let def_calls = server.def_calls.lock().unwrap().clone();
        // 你 好 space 😀(2 UTF-16) space foo → column 6.
        assert_eq!(def_calls[0].1, pos(0, 6));
    }

    #[test]
    fn missing_symbol_name_is_unknown() {
        let repo = TempRepo::new(&[("src/lib.go", "package p\n")]);
        let server = FakeServer::refs(Vec::new());
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("src/lib.go"),
            SymbolAt::Name("absent"),
        );
        assert_unknown_empty(&result);
        assert!(result.note.as_deref().unwrap().contains("`absent`"));
        assert!(server.ref_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn invalid_percent_encoding_is_treated_as_external() {
        assert!(lsp_path_to_rel("file:///repo/src/%zz.go", Path::new("/repo")).is_none());
        assert!(lsp_path_to_rel("file://host/share/x.go", Path::new("/repo")).is_none());
        assert!(lsp_path_to_rel("../outside.go", Path::new("/repo")).is_none());
    }

    #[test]
    fn already_relative_paths_pass_through() {
        assert_eq!(
            lsp_path_to_rel("src/lib.go", Path::new("/repo"))
                .unwrap()
                .as_str(),
            "src/lib.go"
        );
        assert_eq!(
            lsp_path_to_rel("/repo/src/lib.go", Path::new("/repo"))
                .unwrap()
                .as_str(),
            "src/lib.go"
        );
        // Nested folder that shares the root's last component must stay put.
        assert_eq!(
            lsp_path_to_rel("repo/src/a.rs", Path::new("/repo"))
                .unwrap()
                .as_str(),
            "repo/src/a.rs"
        );
        // Recover a Unix absolute path after RelPath::new stripped '/'.
        assert_eq!(
            lsp_path_to_rel("Users/me/proj/src/lib.go", Path::new("/Users/me/proj"))
                .unwrap()
                .as_str(),
            "src/lib.go"
        );
        assert_eq!(
            lsp_path_to_rel(
                "Users/me/projectile/src/lib.go",
                Path::new("/Users/me/proj")
            )
            .unwrap()
            .as_str(),
            "Users/me/projectile/src/lib.go"
        );
    }

    #[test]
    fn empty_result_from_a_still_indexing_server_is_not_authoritative() {
        let server = FakeServer::still_indexing();
        let root = std::env::temp_dir();
        let path = RelPath::new("main.go");

        let result = find_references(
            Some(&server),
            &root,
            &path,
            SymbolAt::Position(Position {
                line: 0,
                character: 0,
            }),
        );

        // A cold server returns `[]` rather than an error. Calling that
        // `exact` tells an agent the symbol is unused, and an agent that
        // believes it will delete live code.
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.value.is_empty());
        assert!(
            result.note.is_some(),
            "the caller has to be told why the empty answer is untrusted"
        );
    }

    #[test]
    fn empty_result_from_a_ready_server_stays_authoritative() {
        let server = FakeServer::refs(Vec::new());
        let root = std::env::temp_dir();
        let path = RelPath::new("main.go");

        let result = find_references(
            Some(&server),
            &root,
            &path,
            SymbolAt::Position(Position {
                line: 0,
                character: 0,
            }),
        );

        // `includeDeclaration` is on, so a dead symbol still returns its own
        // declaration. Fully empty means the anchor missed the symbol (e.g. a
        // doc comment) and must not be reported as an exact "no references".
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.value.is_empty());
        assert!(result.note.unwrap().contains("not even the declaration"));
    }

    #[test]
    fn empty_definition_from_a_still_indexing_server_is_not_authoritative() {
        let server = FakeServer::still_indexing();
        let root = std::env::temp_dir();
        let path = RelPath::new("main.go");

        let result = goto_definition(
            Some(&server),
            &root,
            &path,
            SymbolAt::Position(Position {
                line: 0,
                character: 0,
            }),
        );

        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.value.is_empty());
        assert!(
            result.note.is_some(),
            "the caller has to be told why the empty answer is untrusted"
        );
    }

    #[test]
    fn empty_definition_from_a_ready_server_stays_authoritative() {
        let server = FakeServer::refs(Vec::new());
        let root = std::env::temp_dir();
        let path = RelPath::new("main.go");

        let result = goto_definition(
            Some(&server),
            &root,
            &path,
            SymbolAt::Position(Position {
                line: 0,
                character: 0,
            }),
        );

        assert_eq!(result.confidence, Confidence::Exact);
        assert!(result.value.is_empty());
    }

    #[test]
    fn goto_definition_named_query_uses_first_identifier() {
        let repo = TempRepo::new(&[(
            "src/main.go",
            "package p\n\nfunc helper() {}\n\nfunc Run() { helper() }\n",
        )]);
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Ok(vec![loc("src/main.go", 2, 5)]);

        let result = goto_definition(
            Some(&server),
            &repo.root,
            &RelPath::new("src/main.go"),
            SymbolAt::Name("helper"),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value, vec![loc("src/main.go", 2, 5)]);

        let def_calls = server.def_calls.lock().unwrap().clone();
        assert_eq!(def_calls.len(), 1);
        assert_eq!(def_calls[0].1, pos(2, 5));
    }

    #[test]
    fn goto_definition_missing_symbol_name_is_unknown() {
        let repo = TempRepo::new(&[("src/lib.go", "package p\n")]);
        let server = FakeServer::refs(Vec::new());
        let result = goto_definition(
            Some(&server),
            &repo.root,
            &RelPath::new("src/lib.go"),
            SymbolAt::Name("absent"),
        );
        assert_unknown_empty(&result);
        assert!(result.note.as_deref().unwrap().contains("`absent`"));
        assert!(server.def_calls.lock().unwrap().is_empty());
    }

    fn issue_ok(
        result: Value,
        calls: &Mutex<Vec<(RelPath, Position)>>,
    ) -> impl FnOnce(&dyn LanguageServer, RelPath, Position) -> Result<Value, LspError> + '_ {
        move |_server, path, position| {
            calls.lock().unwrap().push((path, position));
            Ok(result)
        }
    }

    fn issue_err(
        err: LspError,
        calls: &Mutex<Vec<(RelPath, Position)>>,
    ) -> impl FnOnce(&dyn LanguageServer, RelPath, Position) -> Result<Value, LspError> + '_ {
        move |_server, path, position| {
            calls.lock().unwrap().push((path, position));
            Err(err)
        }
    }

    #[test]
    fn hover_params_shape_matches_the_wire_format() {
        assert_eq!(HOVER_METHOD, "textDocument/hover");
        assert_eq!(
            hover_params("file:///repo/src/lib.go", pos(2, 5)),
            json!({
                "textDocument": { "uri": "file:///repo/src/lib.go" },
                "position": { "line": 2, "character": 5 },
            })
        );
    }

    #[test]
    fn parse_hover_reads_markup_contents_and_ignores_range() {
        let result = json!({
            "contents": {
                "kind": "markdown",
                "value": "```go\nfunc helper() int\n```\nDoes things."
            },
            "range": {
                "start": { "line": 2, "character": 5 },
                "end": { "line": 2, "character": 11 }
            }
        });
        assert_eq!(
            parse_hover(&result).as_deref(),
            Some("```go\nfunc helper() int\n```\nDoes things.")
        );
    }

    #[test]
    fn parse_hover_joins_legacy_array_contents() {
        let result = json!({
            "contents": [
                { "language": "go", "value": "func helper() int" },
                "plain MarkedString",
                { "kind": "markdown", "value": "Doc text." },
                42
            ]
        });
        assert_eq!(
            parse_hover(&result).as_deref(),
            Some("func helper() int\n\nplain MarkedString\n\nDoc text.")
        );
    }

    #[test]
    fn parse_hover_accepts_bare_string_contents() {
        assert_eq!(
            parse_hover(&json!({ "contents": "just a signature" })).as_deref(),
            Some("just a signature")
        );
    }

    #[test]
    fn parse_hover_null_missing_or_blank_contents_are_none() {
        assert_eq!(parse_hover(&Value::Null), None);
        assert_eq!(parse_hover(&json!({ "contents": null })), None);
        assert_eq!(parse_hover(&json!({ "range": {} })), None);
        assert_eq!(parse_hover(&json!({ "contents": "" })), None);
        assert_eq!(parse_hover(&json!({ "contents": [] })), None);
        assert_eq!(parse_hover(&json!({ "contents": "  \n" })), None);
        assert_eq!(parse_hover(&json!({ "contents": 42 })), None);
    }

    #[test]
    fn hover_named_symbol_anchors_on_the_unique_definition() {
        let repo = TempRepo::new(&[(
            "src/main.go",
            "package p\n\nfunc helper() {}\n\nfunc Run() { helper() }\n",
        )]);
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Ok(vec![loc("src/main.go", 2, 5)]);
        let calls = Mutex::new(Vec::new());

        let result = hover(
            Some(&server),
            &repo.root,
            &RelPath::new("src/main.go"),
            SymbolAt::Name("helper"),
            issue_ok(
                json!({ "contents": { "kind": "markdown", "value": "helper does things" } }),
                &calls,
            ),
        );

        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value.as_deref(), Some("helper does things"));
        assert!(result.note.is_none());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(RelPath::new("src/main.go"), pos(2, 5))]
        );
    }

    #[test]
    fn hover_position_target_skips_the_definition_round_trip() {
        let server = FakeServer::refs(Vec::new());
        let calls = Mutex::new(Vec::new());

        let result = hover(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/main.go"),
            SymbolAt::Position(pos(3, 1)),
            issue_ok(json!({ "contents": "sig" }), &calls),
        );

        assert_eq!(result.value.as_deref(), Some("sig"));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(RelPath::new("src/main.go"), pos(3, 1))]
        );
        assert!(server.def_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn hover_null_from_a_ready_server_is_exact_none() {
        let server = FakeServer::refs(Vec::new());
        let calls = Mutex::new(Vec::new());

        let result = hover(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("main.go"),
            SymbolAt::Position(pos(0, 0)),
            issue_ok(Value::Null, &calls),
        );

        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value, None);
        assert!(result.note.is_none());
    }

    #[test]
    fn hover_null_from_a_still_indexing_server_is_not_authoritative() {
        let server = FakeServer::still_indexing();
        let calls = Mutex::new(Vec::new());

        let result = hover(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("main.go"),
            SymbolAt::Position(pos(0, 0)),
            issue_ok(Value::Null, &calls),
        );

        assert_eq!(result.confidence, Confidence::Unknown);
        assert_eq!(result.value, None);
        assert!(
            result.note.is_some(),
            "the caller has to be told why the empty answer is untrusted"
        );
    }

    #[test]
    fn hover_errors_degrade_to_unknown_with_a_fallback_hint() {
        for err in [
            LspError::Timeout(Duration::from_secs(8)),
            LspError::Crashed,
            LspError::Protocol("broken response".into()),
        ] {
            let server = FakeServer::refs(Vec::new());
            let calls = Mutex::new(Vec::new());

            let result = hover(
                Some(&server),
                Path::new("/repo"),
                &RelPath::new("main.go"),
                SymbolAt::Position(pos(0, 0)),
                issue_err(err, &calls),
            );

            assert_eq!(result.confidence, Confidence::Unknown);
            assert_eq!(result.value, None);
            let note = result.note.as_deref().unwrap();
            assert!(!note.is_empty());
            assert!(note.contains("symbol-name anchor"));
        }
    }

    #[test]
    fn hover_missing_server_is_unknown_and_never_issues() {
        let called = Mutex::new(false);

        let result = hover(
            None,
            Path::new("/repo"),
            &RelPath::new("cmd/app.go"),
            SymbolAt::Position(pos(0, 0)),
            |_server, _path, _position| {
                *called.lock().unwrap() = true;
                Ok(Value::Null)
            },
        );

        assert_eq!(result.confidence, Confidence::Unknown);
        assert_eq!(result.value, None);
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("gopls is not installed"));
        assert!(note.contains("symbol-name anchor"));
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn hover_missing_symbol_name_is_unknown_and_never_issues() {
        let repo = TempRepo::new(&[("src/lib.go", "package p\n")]);
        let server = FakeServer::refs(Vec::new());
        let called = Mutex::new(false);

        let result = hover(
            Some(&server),
            &repo.root,
            &RelPath::new("src/lib.go"),
            SymbolAt::Name("absent"),
            |_server, _path, _position| {
                *called.lock().unwrap() = true;
                Ok(json!({ "contents": "unreachable" }))
            },
        );

        assert_eq!(result.confidence, Confidence::Unknown);
        assert_eq!(result.value, None);
        assert!(result.note.as_deref().unwrap().contains("`absent`"));
        assert!(result
            .note
            .as_deref()
            .unwrap()
            .contains("symbol-name anchor"));
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn hover_site_failure_note_excludes_references_search_hint() {
        let repo = TempRepo::new(&[("src/lib.go", "package p\n")]);
        let server = FakeServer::refs(Vec::new());
        let result = hover(
            Some(&server),
            &repo.root,
            &RelPath::new("src/lib.go"),
            SymbolAt::Name("absent"),
            |_server, _path, _position| Ok(Value::Null),
        );
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("symbol-name anchor"));
        assert!(
            !note.contains("search_code"),
            "hover site-failure must not mention search_code, got: {note}"
        );
        assert!(
            !note.contains("false positives"),
            "hover site-failure must not include TEXT_SEARCH_HINT, got: {note}"
        );
        assert!(
            !note.contains("precise references"),
            "hover site-failure must not reuse references wording, got: {note}"
        );
        assert!(note.contains("hover information") || note.contains("`absent`"));
    }

    #[test]
    fn named_references_fall_back_to_text_search_when_definition_hop_times_out() {
        let repo = TempRepo::new(&[
            ("src/lib.go", "package p\n\nfunc helper() {}\n"),
            ("src/use.go", "package p\n\nfunc Run() { helper() }\n"),
        ]);
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Timeout;

        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("src/use.go"),
            SymbolAt::Name("helper"),
        );

        assert_ne!(result.confidence, Confidence::Exact);
        assert!(
            !result.value.is_empty(),
            "timeout on the name→declaration hop must fall back to text search"
        );
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("timed out"), "got: {note}");
    }

    #[test]
    fn multi_anchor_references_stop_after_first_timeout() {
        let repo = TempRepo::new(&[
            ("a.go", "package p\n\nfunc helper() {}\n"),
            ("b.go", "package p\n\nfunc helper() {}\n"),
        ]);
        let mut server = FakeServer::failing(Script::Timeout);
        server.definition = Script::Ok(vec![loc("a.go", 2, 5), loc("b.go", 2, 5)]);
        let _ = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Name("helper"),
        );
        assert_eq!(
            server.ref_calls.lock().unwrap().len(),
            1,
            "a timeout on the first anchor must not be repeated per anchor"
        );
    }

    #[test]
    fn named_references_fall_back_to_text_search_when_references_time_out() {
        let repo = TempRepo::new(&[
            ("src/lib.go", "package p\n\nfunc helper() {}\n"),
            ("src/use.go", "package p\n\nfunc Run() { helper() }\n"),
        ]);
        let server = FakeServer::failing(Script::Timeout);

        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("src/lib.go"),
            SymbolAt::Name("helper"),
        );

        assert_ne!(result.confidence, Confidence::Exact);
        assert!(!result.value.is_empty());
        assert!(result.note.as_deref().unwrap().contains("timed out"));
    }

    #[test]
    fn named_hover_falls_back_to_first_identifier_when_definition_errors() {
        let repo = TempRepo::new(&[(
            "src/main.go",
            "package p\n\nfunc helper() {}\n\nfunc Run() { helper() }\n",
        )]);
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Timeout;
        let calls = Mutex::new(Vec::new());

        let result = hover(
            Some(&server),
            &repo.root,
            &RelPath::new("src/main.go"),
            SymbolAt::Name("helper"),
            issue_ok(
                json!({ "contents": { "kind": "markdown", "value": "helper docs" } }),
                &calls,
            ),
        );

        assert_eq!(result.confidence, Confidence::Exact);
        assert_eq!(result.value.as_deref(), Some("helper docs"));
        // First identifier of helper is at func helper → (2, 5).
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(RelPath::new("src/main.go"), pos(2, 5))]
        );
        let note = result.note.as_deref().unwrap_or("");
        assert!(
            !note.contains("search_code"),
            "fallback hover must not carry references degrade, got: {note}"
        );
    }

    #[test]
    fn goto_definition_missing_server_uses_definition_wording() {
        let result = goto_definition(
            None,
            Path::new("/repo"),
            &RelPath::new("cmd/app.go"),
            SymbolAt::Position(pos(0, 0)),
        );
        assert_unknown_empty(&result);
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("gopls is not installed"));
        assert!(note.contains("definitions"));
        assert!(!note.contains("precise references"));
        assert!(note.contains("search_code"));
    }

    /// M3: a definition that lives in the stdlib/dependencies must come back
    /// with an explicit "outside the repository" note, not a bare empty Exact.
    #[test]
    fn goto_definition_into_stdlib_says_results_are_outside_repo() {
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Ok(vec![loc("file:///usr/lib/go/src/fmt/print.go", 10, 5)]);
        let result = goto_definition(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/main.go"),
            SymbolAt::Position(pos(1, 0)),
        );
        assert!(result.value.is_empty());
        let note = result.note.as_deref().expect("note required");
        assert!(note.contains("1 条结果在仓库外"), "{note}");
    }

    #[test]
    fn cold_guard_merges_external_note_instead_of_replacing() {
        let mut server = FakeServer::refs(vec![loc("file:///usr/lib/go/src/fmt/print.go", 1, 0)]);
        server.busy = true;
        let result = find_references(
            Some(&server),
            Path::new("/repo"),
            &RelPath::new("src/main.go"),
            SymbolAt::Position(pos(1, 0)),
        );
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.value.is_empty());
        let note = result.note.as_deref().unwrap();
        assert!(
            note.contains("outside the repository"),
            "external note must be preserved, got: {note}"
        );
        assert!(
            note.contains("索引") || note.contains("不可信"),
            "cold-index note must be merged in, got: {note}"
        );
    }

    #[test]
    fn named_hover_skips_unique_definition_anchor_while_server_busy() {
        let repo = TempRepo::new(&[("src/main.go", "package p\n\nfunc Run() { helper() }\n")]);
        // Unique def points at another file; while busy that anchor is untrusted.
        let mut server = FakeServer::refs(Vec::new());
        server.busy = true;
        server.definition = Script::Ok(vec![loc("src/other.go", 0, 5)]);
        let calls = Mutex::new(Vec::new());

        let result = hover(
            Some(&server),
            &repo.root,
            &RelPath::new("src/main.go"),
            SymbolAt::Name("helper"),
            issue_ok(
                json!({ "contents": { "kind": "markdown", "value": "maybe skewed" } }),
                &calls,
            ),
        );

        // Hover still returns text, but must have stayed on the first identifier
        // in the queried file — not the cold unique definition in other.go.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(RelPath::new("src/main.go"), pos(2, 13))]
        );
        // Non-empty hover from a busy server is kept (content is real); the
        // important guard is not anchoring on the cold definition above.
        assert_eq!(result.value.as_deref(), Some("maybe skewed"));
    }

    #[test]
    fn hover_site_failed_sanitizes_leaked_references_degrade() {
        let leaked = Precise::unknown(
            Vec::new(),
            format!(
                "language server timed out after 8s; cannot provide precise references. {TEXT_SEARCH_HINT}"
            ),
        );
        let result = hover_site_failed(leaked);
        assert_eq!(result.confidence, Confidence::Unknown);
        assert_eq!(result.value, None);
        let note = result.note.as_deref().unwrap();
        assert!(note.contains("symbol-name anchor"));
        assert!(note.contains("hover information"));
        assert!(!note.contains("search_code"));
        assert!(!note.contains("false positives"));
        assert!(!note.contains("precise references"));
    }

    #[test]
    fn named_query_skips_go_doc_comment_anchor() {
        // Regression: Go doc comments start with the item name, so the first
        // textual hit was inside `// Execute ...` and gopls errored.
        let repo = TempRepo::new(&[(
            "cmd.go",
            "package cobra\n\n// Execute runs the command.\nfunc Execute() error { return nil }\n",
        )]);
        let server = FakeServer::refs(vec![loc("cmd.go", 3, 5)]);
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("cmd.go"),
            SymbolAt::Name("Execute"),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        let ref_calls = server.ref_calls.lock().unwrap().clone();
        assert_eq!(ref_calls, vec![(RelPath::new("cmd.go"), pos(3, 5))]);
    }

    #[test]
    fn named_query_merges_all_definitions() {
        let repo = TempRepo::new(&[(
            "m.rs",
            "struct A;\nstruct B;\nimpl A { fn run(&self) {} }\nimpl B { fn run(&self) {} }\n",
        )]);
        let server = FakeServer::refs(vec![loc("m.rs", 2, 12)]);
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("m.rs"),
            SymbolAt::Name("run"),
        );
        let ref_calls = server.ref_calls.lock().unwrap().clone();
        assert_eq!(ref_calls.len(), 2);
        assert_eq!(result.value.len(), 1, "merged hits are de-duplicated");
        assert!(result.note.as_ref().unwrap().contains("2 definitions"));
        assert_eq!(
            result.confidence,
            Confidence::Scoped,
            "merged definitions must not claim Exact"
        );
        assert!(result
            .note
            .unwrap()
            .contains(super::AMBIGUOUS_DEFINITIONS_MARK));
    }

    #[test]
    fn more_than_max_definitions_is_syntactic_and_incomplete() {
        let mut src = String::new();
        for i in 0..(MAX_DEFINITION_ANCHORS + 2) {
            src.push_str(&format!("func (T{i}) Run() {{}}\n"));
        }
        let repo = TempRepo::new(&[("m.go", &format!("package m\n{src}"))]);
        let server = FakeServer::refs(vec![loc("m.go", 1, 10)]);
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("m.go"),
            SymbolAt::Name("Run"),
        );
        assert_eq!(
            server.ref_calls.lock().unwrap().len(),
            MAX_DEFINITION_ANCHORS
        );
        assert_eq!(result.confidence, Confidence::Syntactic);
        assert!(result.note.unwrap().contains("incomplete"));
    }

    #[test]
    fn goto_definition_by_name_queries_every_declaration() {
        let repo = TempRepo::new(&[(
            "a.go",
            "package a\nfunc (A) Run() {}\nfunc (B) Run() {}\nfunc Run() {}\n",
        )]);
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Ok(vec![loc("a.go", 1, 10)]);
        let result = goto_definition(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Name("Run"),
        );
        assert_eq!(server.def_calls.lock().unwrap().len(), 3);
        assert_eq!(result.confidence, Confidence::Scoped);
        let note = result.note.unwrap();
        assert!(note.contains("3 declarations"), "{note}");
        assert!(note.contains(super::AMBIGUOUS_DEFINITIONS_MARK), "{note}");
    }

    #[test]
    fn goto_definition_single_declaration_stays_exact() {
        let repo = TempRepo::new(&[("a.go", "package a\nfunc Only() {}\n")]);
        let mut server = FakeServer::refs(Vec::new());
        server.definition = Script::Ok(vec![loc("a.go", 1, 5)]);
        let result = goto_definition(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Name("Only"),
        );
        assert_eq!(result.confidence, Confidence::Exact);
        assert!(result.note.is_none());
    }

    #[test]
    fn name_only_in_comments_is_unknown_without_querying() {
        let repo = TempRepo::new(&[("a.go", "package a\n// Gone was removed.\n")]);
        let server = FakeServer::refs(vec![loc("a.go", 1, 3)]);
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Name("Gone"),
        );
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.note.unwrap().contains("comments or strings"));
        assert!(server.ref_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn position_inside_comment_is_not_exact() {
        let repo = TempRepo::new(&[("a.go", "package a\n// Run it.\nfunc Run() {}\n")]);
        let server = FakeServer::refs(vec![loc("a.go", 2, 5)]);
        let result = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Position(pos(1, 3)),
        );
        assert_eq!(result.confidence, Confidence::Unknown);
    }

    #[test]
    fn edit_file_then_named_query_reanchors_on_new_declaration() {
        let repo = TempRepo::new(&[("a.go", "package a\n// Run it.\nfunc old() {}\n")]);
        let server = FakeServer::refs(vec![loc("a.go", 3, 5)]);
        let first = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Name("Run"),
        );
        assert_eq!(first.confidence, Confidence::Unknown);
        std::fs::write(
            repo.root.join("a.go"),
            "package a\n// Run it.\n\nfunc Run() {}\n",
        )
        .unwrap();
        let second = find_references(
            Some(&server),
            &repo.root,
            &RelPath::new("a.go"),
            SymbolAt::Name("Run"),
        );
        assert_eq!(second.confidence, Confidence::Exact);
        let ref_calls = server.ref_calls.lock().unwrap().clone();
        assert_eq!(ref_calls, vec![(RelPath::new("a.go"), pos(3, 5))]);
    }
}
