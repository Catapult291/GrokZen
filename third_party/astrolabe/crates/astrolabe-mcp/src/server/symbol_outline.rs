//! `find_symbol` file outlines, member expansion for name queries, and the
//! sub-word relevance fallback for zero-hit queries.
//!
//! - `query=<file path>` (or `query="*"` with a `path_filter`) lists the
//!   file's symbols as an indented outline with line ranges, so an agent can
//!   see a file's shape without reading it whole. `depth` controls how many
//!   member levels are shown; fields/variables below the top level are
//!   counted rather than listed. Over budget it degrades to depth 0, then
//!   pages (with per-kind counts).
//! - A plain name query with `depth>0` lists each shown match's members
//!   under it.
//! - A zero-hit query of two or more sub-words is ranked against the
//!   per-snapshot [`crate::word_index::WordIndex`].

use std::collections::HashMap;

use astrolabe_core::{CodeSymbol, FileId, SymbolKind};

use super::{
    decorated_symbol_lines, estimate_tokens, json, paged_symbol_lines, render_search_page_status,
    symbol_line, truncate_ranked, AstrolabeServer, CallToolResult, Path, QueryParams, RepoIndex,
};
use crate::word_index::{nest, words_of};

/// Single-file outlines with at most this many top-level symbols show one
/// member level even at `depth=0` (one call instead of two).
const AUTO_EXPAND_TOP_LEVEL: usize = 5;
/// Members listed under one name-query match before `…(+N more)`.
const CHILDREN_PER_MATCH: usize = 30;
/// Fallback candidates considered / shown.
const FALLBACK_CANDIDATES: usize = 30;
const FALLBACK_SHOWN: usize = 10;
const MAX_SIGNATURE_CHARS: usize = 100;

/// What an outline query covers.
#[derive(Debug, PartialEq)]
pub(super) enum OutlineTarget {
    /// One file; `others` are other files the query's path suffix matched.
    File { id: FileId, others: Vec<String> },
    /// Several files (a directory, or a `path_filter` matching many), by path.
    Files(Vec<FileId>),
}

/// Recognise an outline request: `query="*"` (needs `path_filter`), or a
/// query that is an indexed file's repo-relative path (or a path suffix of
/// one, or a directory holding indexed files). `None` = an ordinary symbol
/// query; `Some(Err)` = an outline request that cannot be served.
pub(super) fn outline_target(
    index: &RepoIndex,
    query: &str,
    path_filter: Option<&str>,
) -> Option<Result<OutlineTarget, String>> {
    let files = &index.graph().files;
    let q = query.trim();
    if q == "*" {
        let Some(filter) = path_filter.filter(|f| !f.is_empty()) else {
            return Some(Err(
                "query=\"*\" 需配合 path_filter（文件或目录）才列出符号大纲；不支持列出全库符号。"
                    .to_string(),
            ));
        };
        let mut matched: Vec<(&str, FileId)> = files
            .iter()
            .filter(|f| f.path.as_str().contains(filter))
            .map(|f| (f.path.as_str(), f.id))
            .collect();
        matched.sort();
        matched.dedup_by_key(|(p, _)| *p);
        return Some(match matched.as_slice() {
            [] => Err(format!(
                "path_filter `{filter}` 只匹配到未入符号索引的文件，无符号大纲可列；请用 Read 或 search_code。"
            )),
            [(_, id)] => Ok(OutlineTarget::File {
                id: *id,
                others: Vec::new(),
            }),
            many => Ok(OutlineTarget::Files(many.iter().map(|(_, id)| *id).collect())),
        });
    }
    let q = q.trim_start_matches("./");
    if q.is_empty()
        || q.contains(char::is_whitespace)
        || !(q.contains('/') || q.contains('.'))
        // A symbol literally named like the query wins (`a.b` in Ruby …).
        || index.names().has_exact(&index.graph().symbols, q)
    {
        return None;
    }
    if let Some(file) = files.iter().find(|f| f.path.as_str() == q) {
        return Some(Ok(OutlineTarget::File {
            id: file.id,
            others: Vec::new(),
        }));
    }
    let dir = q.trim_end_matches('/');
    let suffix = format!("/{dir}");
    let mut by_suffix: Vec<(&str, FileId)> = files
        .iter()
        .filter(|f| f.path.as_str().ends_with(&suffix))
        .map(|f| (f.path.as_str(), f.id))
        .collect();
    if !by_suffix.is_empty() && !q.ends_with('/') {
        by_suffix.sort_by_key(|(p, _)| (p.len(), *p));
        let (_, id) = by_suffix[0];
        let others = by_suffix[1..].iter().map(|(p, _)| p.to_string()).collect();
        return Some(Ok(OutlineTarget::File { id, others }));
    }
    // A directory: only when written with a `/` (so `Type/method` path
    // queries are not taken for directories unless such a directory exists).
    if q.contains('/') {
        let prefix = format!("{dir}/");
        let mut under: Vec<(&str, FileId)> = files
            .iter()
            .filter(|f| f.path.as_str().starts_with(&prefix))
            .map(|f| (f.path.as_str(), f.id))
            .collect();
        under.sort();
        if !under.is_empty() {
            return Some(Ok(OutlineTarget::Files(
                under.into_iter().map(|(_, id)| id).collect(),
            )));
        }
    }
    None
}

/// Fields and variables: listed at the top level, only counted below it.
fn low_level(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Field | SymbolKind::Variable)
}

/// Kinds whose members an outline lists (a function's nested closures and
/// locals are implementation detail, not shape).
fn expandable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Module
            | SymbolKind::Type
    )
}

/// One file's nesting: outline order, parent and children of each symbol.
struct FileTree {
    order: Vec<usize>,
    parent: HashMap<usize, usize>,
    children: HashMap<usize, Vec<usize>>,
}

impl FileTree {
    fn new(index: &RepoIndex, file: FileId) -> Self {
        let symbols = &index.graph().symbols;
        let members = index.names().name_path.file_symbols(file);
        let (order, parent) = nest(symbols, members);
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for &idx in &order {
            if let Some(&p) = parent.get(&idx) {
                children.entry(p).or_default().push(idx);
            }
        }
        FileTree {
            order,
            parent,
            children,
        }
    }

    fn depth_of(&self, mut idx: usize) -> usize {
        let mut depth = 0;
        while let Some(&p) = self.parent.get(&idx) {
            depth += 1;
            idx = p;
        }
        depth
    }

    fn kids(&self, idx: usize) -> &[usize] {
        self.children.get(&idx).map_or(&[], Vec::as_slice)
    }
}

/// `sig  L12-40`, signature collapsed to one line and capped.
fn outline_text(symbol: &CodeSymbol) -> String {
    let raw = if symbol.signature.trim().is_empty() {
        symbol.name.as_str()
    } else {
        symbol.signature.as_str()
    };
    let mut sig = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if sig.chars().count() > MAX_SIGNATURE_CHARS {
        sig = sig
            .chars()
            .take(MAX_SIGNATURE_CHARS - 1)
            .collect::<String>()
            + "…";
    }
    if symbol.end_line > symbol.start_line {
        format!("{sig}  L{}-{}", symbol.start_line, symbol.end_line)
    } else {
        format!("{sig}  L{}", symbol.start_line)
    }
}

/// ` (+3 members, 2 fields)` for direct children not listed.
fn hidden_note(symbols: &[CodeSymbol], hidden: impl Iterator<Item = usize>) -> String {
    let (mut members, mut fields) = (0usize, 0usize);
    for idx in hidden {
        if low_level(symbols[idx].kind) {
            fields += 1;
        } else {
            members += 1;
        }
    }
    match (members, fields) {
        (0, 0) => String::new(),
        (m, 0) => format!(" (+{m} members)"),
        (0, f) => format!(" (+{f} fields)"),
        (m, f) => format!(" (+{m} members, {f} fields)"),
    }
}

/// Outline lines of one file: symbols down to `depth` member levels (kind
/// filter: every symbol of that kind, flat). Each line is indented two
/// spaces per level, plus `indent`.
fn file_outline(
    index: &RepoIndex,
    tree: &FileTree,
    depth: usize,
    kind: Option<SymbolKind>,
    indent: usize,
) -> Vec<String> {
    let symbols = &index.graph().symbols;
    let mut lines = Vec::new();
    if let Some(k) = kind {
        for &idx in tree.order.iter().filter(|&&i| symbols[i].kind == k) {
            lines.push(format!(
                "{}{}",
                " ".repeat(indent),
                outline_text(&symbols[idx])
            ));
        }
        return lines;
    }
    // Pre-order walk: a member is listed when its parent was listed and is
    // expandable, it is within `depth`, and it is not a field/variable.
    let mut listed: std::collections::HashSet<usize> = Default::default();
    let member_listed = |idx: usize, d: usize| d <= depth && !low_level(symbols[idx].kind);
    for &idx in &tree.order {
        let d = tree.depth_of(idx);
        let ok = match tree.parent.get(&idx) {
            None => true,
            Some(p) => listed.contains(p) && expandable(symbols[*p].kind) && member_listed(idx, d),
        };
        if !ok {
            continue;
        }
        listed.insert(idx);
        let note = if expandable(symbols[idx].kind) {
            hidden_note(
                symbols,
                tree.kids(idx)
                    .iter()
                    .copied()
                    .filter(|&c| !member_listed(c, d + 1)),
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "{}{}{note}",
            " ".repeat(indent + 2 * d),
            outline_text(&symbols[idx])
        ));
    }
    lines
}

/// `function 12, class 3, method 40` over the targeted files.
fn kind_counts(index: &RepoIndex, files: &[FileId]) -> String {
    let symbols = &index.graph().symbols;
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for &file in files {
        for &idx in index.names().name_path.file_symbols(file) {
            *counts
                .entry(super::kind_display_name(symbols[idx].kind))
                .or_default() += 1;
        }
    }
    let mut pairs: Vec<(&str, usize)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    pairs
        .iter()
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl AstrolabeServer {
    /// Render an outline for `target` (see the module docs).
    pub(super) fn find_symbol_outline(
        &self,
        root: &Path,
        index: &RepoIndex,
        params: &QueryParams,
        kind: Option<SymbolKind>,
        target: OutlineTarget,
    ) -> CallToolResult {
        let (files, others) = match target {
            OutlineTarget::File { id, others } => (vec![id], others),
            OutlineTarget::Files(ids) => (ids, Vec::new()),
        };
        let single = files.len() == 1;
        let trees: Vec<(FileId, FileTree)> = files
            .iter()
            .map(|&f| (f, FileTree::new(index, f)))
            .collect();
        let path_of = |file: FileId| {
            index
                .file(file)
                .map(|f| f.path.as_str().to_string())
                .unwrap_or_default()
        };
        // Lines for a given depth; multi-file outlines carry a path header
        // on each file's first line.
        let render = |depth: usize| -> Vec<String> {
            let mut out = Vec::new();
            for (file, tree) in &trees {
                let lines = file_outline(index, tree, depth, kind, if single { 0 } else { 2 });
                if lines.is_empty() {
                    continue;
                }
                for (i, line) in lines.into_iter().enumerate() {
                    if !single && i == 0 {
                        out.push(format!("{}\n{line}", path_of(*file)));
                    } else {
                        out.push(line);
                    }
                }
            }
            out
        };
        let requested = usize::from(params.depth);
        let top_level = trees
            .iter()
            .map(|(_, t)| {
                t.order
                    .iter()
                    .filter(|&&i| !t.parent.contains_key(&i))
                    .count()
            })
            .sum::<usize>();
        let auto = single && requested == 0 && top_level <= AUTO_EXPAND_TOP_LEVEL && kind.is_none();
        let wanted = if auto { 1 } else { requested };

        let mut head = "confidence: syntactic (文件符号大纲；行号来自语法索引)\n".to_string();
        let generation = index.generation();
        if let Some(note) = self.symbol_page_drift_note(params, generation) {
            head.push_str(&note);
        }
        let title = if single {
            format!("outline: {}", path_of(files[0]))
        } else {
            format!("outline: {} 个文件", files.len())
        };
        let limit_for = |head: &str| {
            params.budget_tokens.saturating_sub(
                estimate_tokens(head)
                    + estimate_tokens(&render_search_page_status(
                        usize::MAX / 4,
                        params.offset,
                        1,
                        1,
                        params.budget_tokens,
                        params.offset,
                    ))
                    + 40,
            )
        };
        // Depth ladder: wanted → 0, the first that fits whole (else 0, paged).
        let mut depth = wanted;
        let mut lines = render(depth);
        while depth > 0 {
            let cost: usize = lines.iter().map(|l| estimate_tokens(l) + 1).sum();
            if cost <= limit_for(&head) {
                break;
            }
            depth = if depth > 1 { 1 } else { 0 };
            lines = render(depth);
        }
        let degraded = depth < requested;
        let mut header = title;
        header.push_str(&format!("（depth={depth}"));
        if auto && depth == 1 {
            header.push_str("，顶层符号少，已自动展开一层");
        }
        header.push_str("）\n");
        head.push_str(&header);
        if degraded {
            head.push_str(&format!(
                "注意：depth={wanted} 超出预算，已退到 depth={depth}；展开单个类型可用 query=<类型名> depth=1。\n"
            ));
        }
        if !others.is_empty() {
            let listed: Vec<&str> = others.iter().take(3).map(String::as_str).collect();
            head.push_str(&format!(
                "注意：另有 {} 个文件路径以此结尾（{}{}），需要时请写完整路径。\n",
                others.len(),
                listed.join(", "),
                if others.len() > 3 { " …" } else { "" }
            ));
        }
        let total = lines.len();
        let offset = params.offset.min(total);
        // Go interfaces in the outlined files: their implementing types.
        let file_set: std::collections::HashSet<FileId> = files.iter().copied().collect();
        let implementors = super::search_shape::implementors_block(
            index,
            index
                .graph()
                .symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| file_set.contains(&s.file))
                .map(|(i, _)| i),
            params.budget_tokens / 3,
        );
        let limit = limit_for(&head).saturating_sub(estimate_tokens(&implementors));
        let (kept, omitted) = truncate_ranked(&lines[offset..], limit, |l| format!("{l}\n"));
        let kept_count = kept.len();
        let truncated = omitted > 0;
        let next_offset = (truncated && kept_count > 0).then_some(offset + kept_count);
        let mut body = head;
        body.push_str(&render_search_page_status(
            total,
            offset,
            kept_count,
            omitted,
            params.budget_tokens,
            params.offset,
        ));
        if truncated {
            body.push_str(&format!("按类型计数：{}\n", kind_counts(index, &files)));
        }
        // A page starting mid-file in a multi-file outline names its file.
        if !single && offset > 0 && kept_count > 0 && !kept[0].contains('\n') {
            if let Some(file) = file_of_line(&lines, offset) {
                body.push_str(&file);
                body.push('\n');
            }
        }
        for line in &kept {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str(&implementors);
        if total == 0 {
            body.push_str(if kind.is_some() {
                "(该文件没有此类型的符号)\n"
            } else {
                "(该文件没有已索引的符号)\n"
            });
        }
        self.result_on(
            root,
            "find_symbol",
            body,
            json!({
                "confidence": "syntactic",
                "generation": generation,
                "outline": true,
                "files": files.len(),
                "depth": depth,
                "matches": total, "total": total, "offset": offset, "next_offset": next_offset,
                "shown": kept_count,
                "omitted": omitted,
                "truncated": truncated,
                "budget_tokens": params.budget_tokens
            }),
            Some(params.budget_tokens),
        )
    }
}

/// The path header of the file that `lines[at]` belongs to.
fn file_of_line(lines: &[String], at: usize) -> Option<String> {
    lines[..=at.min(lines.len().saturating_sub(1))]
        .iter()
        .rev()
        .find_map(|l| l.split_once('\n').map(|(path, _)| path.to_string()))
}

/// Name-query page with members: each shown type's line is followed by its
/// members down to `depth` levels (functions are not expanded). Returns one
/// string per shown match (possibly multi-line) and how many matches remain.
pub(super) fn paged_lines_with_children(
    index: &RepoIndex,
    hits: &[(String, usize)],
    offset: usize,
    depth: u8,
    limit: usize,
) -> (Vec<String>, usize) {
    let symbols = &index.graph().symbols;
    let rest = &hits[offset.min(hits.len())..];
    let mut trees: HashMap<FileId, FileTree> = HashMap::new();
    let mut window = 64usize;
    loop {
        let end = window.min(rest.len());
        let lines = decorated_symbol_lines(index, &rest[..end]);
        let blocks: Vec<String> = lines
            .into_iter()
            .zip(&rest[..end])
            .map(|(line, (_, idx))| {
                let file = symbols[*idx].file;
                let tree = trees
                    .entry(file)
                    .or_insert_with(|| FileTree::new(index, file));
                let mut block = line;
                let mut members = Vec::new();
                if expandable(symbols[*idx].kind) {
                    block.push_str(&unlisted_fields_note(symbols, tree, *idx));
                    collect_members(symbols, tree, *idx, 1, usize::from(depth), &mut members);
                }
                let more = members.len().saturating_sub(CHILDREN_PER_MATCH);
                for (d, child, note) in members.into_iter().take(CHILDREN_PER_MATCH) {
                    block.push('\n');
                    block.push_str(&" ".repeat(2 * d));
                    block.push_str(&outline_text(&symbols[child]));
                    block.push_str(&note);
                }
                if more > 0 {
                    block.push_str(&format!("\n  …(+{more} more)"));
                }
                block
            })
            .collect();
        let (kept, omitted_in_window) = truncate_ranked(&blocks, limit, |b| format!("{b}\n"));
        let kept_len = kept.len();
        if kept_len == 0 && !rest.is_empty() {
            // Not even one match with its members fits: plain lines.
            return paged_symbol_lines(index, hits, offset, limit);
        }
        if omitted_in_window > 0 || end == rest.len() {
            let kept: Vec<String> = blocks.into_iter().take(kept_len).collect();
            return (kept, rest.len() - kept_len);
        }
        window = window.saturating_mul(4);
    }
}

/// A type whose members are all fields/variables (a Go struct, a record)
/// lists them; a type with methods lists the methods and counts the fields.
fn lists_fields(symbols: &[CodeSymbol], tree: &FileTree, idx: usize) -> bool {
    tree.kids(idx).iter().all(|&c| low_level(symbols[c].kind))
}

/// ` (+N fields)` when `idx`'s fields are counted rather than listed.
fn unlisted_fields_note(symbols: &[CodeSymbol], tree: &FileTree, idx: usize) -> String {
    if lists_fields(symbols, tree, idx) {
        return String::new();
    }
    hidden_note(
        symbols,
        tree.kids(idx)
            .iter()
            .copied()
            .filter(|&c| low_level(symbols[c].kind)),
    )
}

/// Members of `idx` down to `max` levels: (level, symbol, hidden note).
/// Only types are descended into (see [`lists_fields`] for fields).
fn collect_members(
    symbols: &[CodeSymbol],
    tree: &FileTree,
    idx: usize,
    level: usize,
    max: usize,
    out: &mut Vec<(usize, usize, String)>,
) {
    if level > max {
        return;
    }
    let fields_listed = lists_fields(symbols, tree, idx);
    for &child in tree.kids(idx) {
        if low_level(symbols[child].kind) && !fields_listed {
            continue;
        }
        let note = if !expandable(symbols[child].kind) {
            String::new()
        } else if level == max {
            hidden_note(symbols, tree.kids(child).iter().copied())
        } else {
            unlisted_fields_note(symbols, tree, child)
        };
        out.push((level, child, note));
        if expandable(symbols[child].kind) {
            collect_members(symbols, tree, child, level + 1, max, out);
        }
    }
}

/// Zero-hit fallback: symbols ranked by sub-word relevance to `query`
/// (two or more sub-words), under the same kind / path filters. `None` when
/// the query has fewer than two sub-words or nothing qualifies.
pub(super) fn word_fallback_block(
    index: &RepoIndex,
    query: &str,
    kind: Option<SymbolKind>,
    path_filter: Option<&str>,
    budget: usize,
) -> Option<String> {
    let words = words_of(query);
    if words.len() < 2 {
        return None;
    }
    let symbols = &index.graph().symbols;
    let tests: std::cell::RefCell<HashMap<FileId, bool>> = Default::default();
    let is_test = |file: FileId| {
        *tests.borrow_mut().entry(file).or_insert_with(|| {
            index
                .file(file)
                .is_some_and(|f| super::is_test_path(f.path.as_str()))
        })
    };
    let accept = |s: &CodeSymbol| {
        kind.is_none_or(|k| s.kind == k)
            && path_filter.is_none_or(|f| {
                index
                    .file(s.file)
                    .is_some_and(|file| file.path.as_str().contains(f))
            })
    };
    let ranked = index
        .words()
        .search(symbols, &words, &is_test, &accept, FALLBACK_CANDIDATES);
    if ranked.is_empty() {
        return None;
    }
    let hits: Vec<(String, usize)> = ranked
        .iter()
        .filter_map(|h| {
            let symbol = &symbols[h.pos];
            index
                .file(symbol.file)
                .map(|f| (symbol_line(symbol, &f.path), h.pos))
        })
        .take(FALLBACK_SHOWN)
        .collect();
    let lines = decorated_symbol_lines(index, &hits);
    let head = format!(
        "按子词（{}）相关度排序的近似候选，名字并不包含整串查询，请核验：\n",
        words.join(" ")
    );
    let (kept, _) = truncate_ranked(&lines, budget.saturating_sub(estimate_tokens(&head)), |l| {
        format!("{l}\n")
    });
    if kept.is_empty() {
        return None;
    }
    let mut block = head;
    for line in kept {
        block.push_str(line);
        block.push('\n');
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use rmcp::handler::server::wrapper::Parameters;

    use super::super::{AstrolabeServer, IndexState, QueryParams};

    const MODULE: &str = "\
class CloudClient:
    retries = 3

    def update(self, item):
        def helper():
            pass
        return helper

    def delete(self, item):
        pass


def make_client():
    return CloudClient()
";

    fn server_on(files: &[(&str, &str)]) -> (AstrolabeServer, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-outline-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
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

    fn query(q: &str) -> QueryParams {
        QueryParams {
            root: None,
            query: q.into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 2500,
            offset: 0,
        }
    }

    fn run(server: &AstrolabeServer, params: QueryParams) -> String {
        let result = server.find_symbol(Parameters(params));
        result.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn file_path_query_is_an_outline_with_ranges_and_auto_expanded_members() {
        let (server, dir) = server_on(&[("pkg/cloud.py", MODULE)]);
        let text = run(&server, query("pkg/cloud.py"));
        assert!(text.contains("outline: pkg/cloud.py"), "{text}");
        assert!(text.contains("已自动展开一层"), "{text}");
        assert!(text.contains("\nclass CloudClient:  L1-"), "{text}");
        assert!(text.contains("\n  def update(self, item):  L4-7"), "{text}");
        assert!(text.contains("\ndef make_client():  L13-14"), "{text}");
        // Fields are counted on the type's line; closures are not members.
        assert!(text.contains("(+1 fields)"), "{text}");
        assert!(!text.contains("helper"), "{text}");
        // `./` prefix and a bare suffix resolve to the same file.
        let suffix = run(&server, query("./cloud.py"));
        assert!(suffix.contains("outline: pkg/cloud.py"), "{suffix}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn star_with_path_filter_outlines_a_file_or_a_directory() {
        let (server, dir) = server_on(&[
            ("pkg/cloud.py", MODULE),
            ("pkg/local.py", "def save():\n    pass\n"),
            ("other/x.py", "def unrelated():\n    pass\n"),
        ]);
        let mut params = query("*");
        params.path_filter = Some("pkg/local.py".into());
        let one = run(&server, params);
        assert!(one.contains("outline: pkg/local.py"), "{one}");
        assert!(one.contains("def save():  L1-2"), "{one}");

        let mut params = query("*");
        params.path_filter = Some("pkg/".into());
        let many = run(&server, params);
        assert!(many.contains("outline: 2 个文件"), "{many}");
        assert!(
            many.contains("\npkg/cloud.py\n  class CloudClient:"),
            "{many}"
        );
        assert!(many.contains("\npkg/local.py\n  def save():"), "{many}");
        assert!(!many.contains("unrelated"), "{many}");
        // Multi-file outlines stay at depth 0; types show a member count.
        assert!(many.contains("(+2 members, 1 fields)"), "{many}");

        let bare = server.find_symbol(Parameters(query("*")));
        assert_eq!(bare.is_error, Some(true));
        let text = bare.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("path_filter"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ambiguous_suffix_names_the_other_files_and_hierarchical_queries_still_work() {
        let (server, dir) = server_on(&[
            ("a/cloud.py", MODULE),
            ("b/c/cloud.py", "def other():\n    pass\n"),
        ]);
        let text = run(&server, query("cloud.py"));
        assert!(text.contains("outline: a/cloud.py"), "{text}");
        assert!(
            text.contains("另有 1 个文件路径以此结尾（b/c/cloud.py）"),
            "{text}"
        );
        // `Type/method` is not a file or directory: hierarchical lookup.
        let path = run(&server, query("CloudClient/update"));
        assert!(
            path.contains("def update(self, item):  @a/cloud.py:4"),
            "{path}"
        );
        assert!(!path.contains("outline:"), "{path}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_budget_outline_degrades_to_depth_zero_then_pages() {
        let mut source = String::new();
        for i in 0..40 {
            source.push_str(&format!(
                "class Widget{i}:\n    def method_with_a_long_name_{i}(self, a, b, c):\n        pass\n\n"
            ));
        }
        let (server, dir) = server_on(&[("w.py", &source)]);
        let mut params = query("w.py");
        params.depth = 1;
        params.budget_tokens = 400;
        let text = run(&server, params);
        assert!(text.contains("depth=1 超出预算，已退到 depth=0"), "{text}");
        assert!(text.contains("next_offset="), "{text}");
        assert!(text.contains("按类型计数：class 40, method 40"), "{text}");
        assert!(!text.contains("method_with_a_long_name"), "{text}");
        let mut params = query("w.py");
        params.depth = 1;
        let full = run(&server, params);
        assert!(full.contains("  def method_with_a_long_name_39"), "{full}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_query_with_depth_lists_type_members_not_function_internals() {
        let (server, dir) = server_on(&[("pkg/cloud.py", MODULE)]);
        let mut params = query("CloudClient");
        params.depth = 1;
        let text = run(&server, params);
        // Methods are listed; the field is counted on the type's line.
        let head = "class CloudClient:  @pkg/cloud.py:1 (+1 fields)\n  def update";
        assert!(text.contains(head), "{text}");
        assert!(!text.contains("retries = 3"), "{text}");
        assert!(text.contains("\n  def update(self, item):  L4-7"), "{text}");
        assert!(!text.contains("helper"), "{text}");
        let mut params = query("update");
        params.depth = 1;
        let func = run(&server, params);
        assert!(!func.contains("helper"), "{func}");
        // A fields-only type lists its fields.
        let go_src = "package p\n\ntype Request struct {\n\tMethod string\n\tURL    string\n}\n";
        let (go, go_dir) = server_on(&[("req.go", go_src)]);
        let mut params = query("Request");
        params.depth = 1;
        let fields = run(&go, params);
        assert!(fields.contains("\n  Method string  L4"), "{fields}");
        let _ = std::fs::remove_dir_all(&go_dir);
        // depth=0 keeps the old one-line-per-match output.
        let plain = run(&server, query("CloudClient"));
        assert!(!plain.contains("def update"), "{plain}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_hit_multi_word_query_falls_back_to_sub_word_relevance() {
        let (server, dir) = server_on(&[
            ("pkg/cloud.py", MODULE),
            ("pkg/store.py", "def update_store():\n    pass\n"),
        ]);
        let text = run(&server, query("update cloud client"));
        assert!(text.contains("(未找到匹配的符号)"), "{text}");
        assert!(
            text.contains("按子词（client cloud update）相关度"),
            "{text}"
        );
        let first = text
            .lines()
            .find(|l| l.contains('@'))
            .unwrap_or_default()
            .to_string();
        assert!(
            first.contains("def update(self, item):  @pkg/cloud.py:4"),
            "{text}"
        );
        assert!(!text.contains("update_store"), "{text}");
        // Filters still apply: no class named like that → near-name hint.
        let mut params = query("update cloud client");
        params.kind = Some("function".into());
        params.path_filter = Some("store".into());
        let filtered = run(&server, params);
        assert!(!filtered.contains("按子词"), "{filtered}");
        // camelCase spelling of the same words.
        let camel = run(&server, query("clientCloudUpdate"));
        assert!(camel.contains("@pkg/cloud.py:4"), "{camel}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
