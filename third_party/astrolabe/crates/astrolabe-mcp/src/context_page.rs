//! `resolve_context` as pages. Nothing is dropped silently: files that match
//! the task are ordered by relevance (files defining an identifier the task
//! spells out first), cut into pages, and every page says how many matched, which
//! range it shows and where the rest lives so the caller can choose between
//! narrowing (`path_filter`, a more specific identifier) and `offset`.

use std::collections::{BTreeMap, HashMap};

use astrolabe_core::{
    budget::{estimate_tokens, TokenBudget},
    graph::{Centrality, CodeGraph, TaskMatch},
    render::{confidence_note, symbol_line},
    CodeFile, Confidence, FileId,
};

/// Repos with at most this many indexed files are "small": page 1 lists all of
/// them (matched files first), so a thin match list cannot hide a needed file.
const SMALL_REPO_FILES: usize = 60;
const COMPACT_FILES_PER_PAGE: usize = 10;
/// Symbol lines are only added for the first files of a page.
const SYMBOL_FILES: usize = 8;
const SYMBOLS_PER_FILE: usize = 4;
const COMPACT_SYMBOLS: usize = 6;
const NO_MATCH_FILES: usize = 5;
/// Leads the one-line list of small-repo files that did not match the task.
pub(crate) const PADDING_PREFIX: &str = "未匹配任务词的其余文件（小仓库，仅列路径）：";

pub(crate) struct PageRequest<'a> {
    pub graph: &'a CodeGraph,
    pub ranked: &'a Centrality,
    pub matched: &'a TaskMatch,
    pub offset: usize,
    pub budget: usize,
    pub compact: bool,
    pub path_filter: Option<&'a str>,
    pub index_line: &'a str,
    pub generation_note: Option<&'a str>,
}

pub(crate) struct Page {
    pub body: String,
    /// First files of the page, for the repeat-call digest.
    pub top_files: Vec<String>,
    pub total: usize,
    pub unmatched: bool,
}

fn score(ranked: &Centrality, id: FileId) -> f64 {
    ranked.by_file.get(&id).copied().unwrap_or_default()
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(d, _)| d)
}

/// All files the task selects, most relevant first: exact identifier / path
/// files, then by task-personalised PageRank, then id (stable).
pub(crate) fn ordered_files(req: &PageRequest) -> (Vec<FileId>, usize, bool) {
    let g = req.graph;
    let keep = |f: &CodeFile| {
        req.path_filter
            .filter(|p| !p.is_empty())
            .is_none_or(|p| f.path.as_str().contains(p))
    };
    let by_id: HashMap<FileId, &CodeFile> = g.files.iter().map(|f| (f.id, f)).collect();
    let mut relevant: Vec<FileId> = req
        .matched
        .files
        .iter()
        .copied()
        .chain(req.matched.symbols.iter().map(|&i| g.symbols[i].file))
        .chain(req.matched.caller_files.iter().copied())
        .chain(req.matched.package_files.iter().copied())
        .filter(|id| by_id.get(id).is_some_and(|f| keep(f)))
        .collect();
    relevant.sort_unstable();
    relevant.dedup();
    let sort_by_score = |ids: &mut Vec<FileId>| {
        ids.sort_by(|a, b| {
            let tier = |id: &FileId| {
                if req.matched.exact_files.contains(id) {
                    0
                } else if req.matched.caller_files.contains(id) {
                    1
                } else if req.matched.package_files.contains(id) {
                    2
                } else {
                    3
                }
            };
            tier(a)
                .cmp(&tier(b))
                .then_with(|| {
                    score(req.ranked, *b)
                        .partial_cmp(&score(req.ranked, *a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.cmp(b))
        });
    };
    sort_by_score(&mut relevant);
    let matched_count = relevant.len();
    if matched_count == 0 {
        let mut all: Vec<FileId> = g.files.iter().filter(|f| keep(f)).map(|f| f.id).collect();
        sort_by_score(&mut all);
        let total = all.len();
        return (all, total, true);
    }
    // Small repo: page 1 lists every file (matched first), the whole map is
    // ~1-2 KB and a thin match list must not hide a needed file.
    if g.files.len() <= SMALL_REPO_FILES {
        let have: std::collections::HashSet<FileId> = relevant.iter().copied().collect();
        let mut rest: Vec<FileId> = g
            .files
            .iter()
            .filter(|f| keep(f) && !have.contains(&f.id))
            .map(|f| f.id)
            .collect();
        sort_by_score(&mut rest);
        relevant.extend(rest);
    }
    (relevant, matched_count, false)
}

pub(crate) fn build_page(req: &PageRequest) -> Page {
    let g = req.graph;
    let (list, matched_count, unmatched) = ordered_files(req);
    let by_id: HashMap<FileId, &CodeFile> = g.files.iter().map(|f| (f.id, f)).collect();
    let page_size = if unmatched {
        NO_MATCH_FILES
    } else if list.len() > matched_count {
        // Small repo (padded list): everything on one page.
        list.len()
    } else if req.compact {
        COMPACT_FILES_PER_PAGE
    } else {
        // 100 tokens of budget per file line: default 2500 -> 25 files.
        (req.budget / 100).clamp(COMPACT_FILES_PER_PAGE, 80)
    };
    let total_listed = list.len();
    let offset = req.offset.min(total_listed);
    let end = (offset + page_size).min(total_listed);
    let page = &list[offset..end];

    let confidence = if unmatched {
        Confidence::Unknown
    } else {
        Confidence::Scoped
    };
    let mut body = format!(
        "confidence: {:?} {}\n",
        confidence,
        confidence_note(confidence)
    );
    if let Some(note) = req.generation_note {
        body.push_str(note);
    }
    if req.offset > total_listed {
        body.push_str(&format!(
            "offset={} 超出总数 {total_listed}，本页为空；offset=0 重新开始。\n",
            req.offset
        ));
    }
    let scope = req
        .path_filter
        .filter(|p| !p.is_empty())
        .map(|p| format!("（path_filter={p}）"))
        .unwrap_or_default();
    if !unmatched {
        body.push_str(&format!(
            "匹配 {matched_count} 个文件{scope}，本页第 {}–{}，按相关度降序（任务点名的标识符/路径所在文件在前）。\n",
            if page.is_empty() { 0 } else { offset + 1 },
            end
        ));
    }
    let mut top_files = Vec::new();
    // Small-repo padding (files that did not match the task): one line of
    // bare paths, test files only counted. 12 of 182 padded files were used
    // in the S6 sessions, but the floor keeps recall when task words miss.
    let mut padding: Vec<&str> = Vec::new();
    let mut padded_tests = 0usize;
    for (k, id) in page.iter().enumerate() {
        let Some(file) = by_id.get(id) else { continue };
        if !unmatched && offset + k >= matched_count {
            if crate::explain::is_test_caller(file.path.as_str(), "") {
                padded_tests += 1;
            } else {
                padding.push(file.path.as_str());
            }
            continue;
        }
        body.push_str(&format!("@{}:1 ({} LOC)\n", file.path, file.loc));
        if top_files.len() < 5 {
            top_files.push(file.path.to_string());
        }
    }
    if !padding.is_empty() || padded_tests > 0 {
        let tests = if padded_tests > 0 {
            format!("；另有测试文件 {padded_tests} 个")
        } else {
            String::new()
        };
        body.push_str(&format!("{PADDING_PREFIX}{}{tests}\n", padding.join("  ")));
    }
    if unmatched {
        body.push_str(&format!(
            "未匹配到与任务相关的符号或文件{scope}：任务词没有命中任何文件名或符号名。上面 {} 个只是仓库里依赖关系最中心的文件，与任务无关，不要据此判断相关性。\n建议：字面量/文案用 search_code，已知名字用 find_symbol。\n",
            page.len()
        ));
    }
    body.push_str(req.index_line);

    // Symbols, only for files on this page and only after the file lines
    // have been paid for.
    if !unmatched && !page.is_empty() {
        let mut by_file: HashMap<FileId, Vec<usize>> = HashMap::new();
        let on_page: std::collections::HashSet<FileId> = page.iter().copied().collect();
        for (idx, symbol) in g.symbols.iter().enumerate() {
            if on_page.contains(&symbol.file) {
                by_file.entry(symbol.file).or_default().push(idx);
            }
        }
        let mut remaining = TokenBudget::new(req.budget.saturating_sub(estimate_tokens(&body)));
        let symbol_cap = if req.compact {
            COMPACT_SYMBOLS
        } else {
            usize::MAX
        };
        let mut shown = 0;
        let mut omitted = 0;
        // Padding files (small-repo floor) did not match: no symbols.
        let matched_on_page = matched_count.saturating_sub(offset).min(page.len());
        for id in page[..matched_on_page].iter().take(SYMBOL_FILES) {
            let Some(file) = by_id.get(id) else { continue };
            let all = by_file.get(id).map(Vec::as_slice).unwrap_or_default();
            let mut picked: Vec<usize> = all
                .iter()
                .copied()
                .filter(|i| {
                    req.matched.symbols.contains(i) || req.matched.caller_symbols.contains(i)
                })
                .collect();
            if picked.is_empty() {
                // Matched by file name only (or padding): a short skeleton.
                picked = all.iter().copied().take(2).collect();
            }
            for idx in picked.into_iter().take(SYMBOLS_PER_FILE) {
                let line = format!("{}\n", symbol_line(&g.symbols[idx], &file.path));
                if shown < symbol_cap && remaining.try_add(&line) {
                    body.push_str(&line);
                    shown += 1;
                } else {
                    omitted += 1;
                }
            }
        }
        if omitted > 0 {
            body.push_str(&format!(
                "{omitted} 个符号因 budget_tokens 被省略（用 find_symbol 取指定符号）。\n"
            ));
        }
    }

    // Decision line: what is left and where. Unmatched centrality filler is
    // not worth paging through.
    if !unmatched {
        body.push_str(&decision_line(&list, end, page_size, &by_id));
    }
    Page {
        body,
        top_files,
        total: total_listed,
        unmatched,
    }
}

fn decision_line(
    list: &[FileId],
    end: usize,
    page_size: usize,
    by_id: &HashMap<FileId, &CodeFile>,
) -> String {
    if end >= list.len() {
        return "已到最后一页。\n".to_string();
    }
    let rest = list.len() - end;
    let mut dirs: BTreeMap<&str, usize> = BTreeMap::new();
    for id in list[end..].iter().take(page_size) {
        if let Some(f) = by_id.get(id) {
            *dirs.entry(dir_of(f.path.as_str())).or_default() += 1;
        }
    }
    let mut dirs: Vec<(&str, usize)> = dirs.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let dir_text = dirs
        .iter()
        .take(3)
        .map(|(d, n)| format!("{d}/ ×{n}"))
        .collect::<Vec<_>>()
        .join("、");
    format!(
        "还有 {rest} 个文件未显示；下一页（offset={end}）主要在 {dir_text}。选择：缩小范围——path_filter=目录 或在任务里写具体标识符；或翻页——offset={end}。\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::{
        graph::{compute_centrality, rank_and_match},
        CodeSymbol, Language, RelPath, SymbolId, SymbolKind,
    };

    /// `n` files `dirK/fileI.go`, each defining `widgetHandlerI` (all match
    /// "widget handler"), plus `extra` unrelated files.
    fn repo(n: usize, extra: usize) -> CodeGraph {
        let mut g = CodeGraph::default();
        for i in 0..n + extra {
            g.files.push(CodeFile {
                id: FileId(i as u32),
                path: RelPath::new(format!("dir{}/file{i}.go", i % 3)),
                language: Some(Language::Go),
                loc: 10,
                sha: String::new(),
            });
            let name = if i < n {
                format!("widget_handler_{i}")
            } else {
                format!("zzz{i}")
            };
            g.symbols.push(CodeSymbol {
                id: SymbolId(i as u32),
                file: FileId(i as u32),
                name,
                kind: SymbolKind::Function,
                signature: String::new(),
                start_line: 1,
                end_line: 2,
                exported: true,
            });
        }
        g
    }

    fn page(g: &CodeGraph, task: &str, offset: usize, filter: Option<&str>) -> Page {
        let base = compute_centrality(g);
        let (ranked, matched) = rank_and_match(g, task, &base);
        build_page(&PageRequest {
            graph: g,
            ranked: &ranked,
            matched: &matched,
            offset,
            budget: 2_500,
            compact: false,
            path_filter: filter,
            index_line: "index: test\n",
            generation_note: None,
        })
    }

    fn files(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|l| l.strip_prefix('@'))
            .map(|l| l.split(':').next().unwrap_or_default().to_string())
            .collect()
    }

    /// Bare paths on the small-repo padding line.
    fn padded(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|l| l.strip_prefix(PADDING_PREFIX))
            .flat_map(|l| l.split('；').next().unwrap_or_default().split("  "))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn next_offset(body: &str) -> Option<usize> {
        let at = body.find("（offset=")?;
        body[at + "（offset=".len()..]
            .split('）')
            .next()?
            .parse()
            .ok()
    }

    #[test]
    fn pages_cover_every_matched_file_once_in_a_stable_order() {
        let g = repo(70, 10);
        let task = "fix the widget handler";
        let mut seen = Vec::new();
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let p = page(&g, task, offset, None);
            assert_eq!(p.total, 70, "{}", p.body);
            assert!(p.body.contains("匹配 70 个文件"), "{}", p.body);
            assert!(
                p.body.contains(&format!("本页第 {}–", offset + 1)),
                "{}",
                p.body
            );
            // Same request, same page.
            assert_eq!(p.body, page(&g, task, offset, None).body);
            let got = files(&p.body);
            assert!(!got.is_empty());
            seen.extend(got);
            pages += 1;
            match next_offset(&p.body) {
                Some(next) => {
                    assert!(p.body.contains("还有 "), "{}", p.body);
                    assert!(p.body.contains("path_filter"), "{}", p.body);
                    assert!(p.body.contains("dir"), "next-page dirs: {}", p.body);
                    assert_eq!(next, seen.len());
                    offset = next;
                }
                None => {
                    assert!(p.body.contains("已到最后一页"), "{}", p.body);
                    break;
                }
            }
        }
        assert!(pages >= 3, "{pages}");
        let unique: std::collections::BTreeSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "duplicates across pages");
        assert_eq!(seen.len(), 70, "unmatched files are not paged in");
        // Past the end: empty page that says so.
        let p = page(&g, task, 500, None);
        assert!(files(&p.body).is_empty());
        assert!(p.body.contains("超出总数"), "{}", p.body);
    }

    #[test]
    fn path_filter_narrows_the_matched_set() {
        let g = repo(70, 10);
        let p = page(&g, "fix the widget handler", 0, Some("dir1/"));
        assert!(files(&p.body).iter().all(|f| f.starts_with("dir1/")));
        assert!(p.body.contains("path_filter=dir1/"), "{}", p.body);
        assert!(p.total < 70 && p.total > 0);
    }

    #[test]
    fn small_repo_page_one_lists_every_file_matched_first() {
        // 3 matching files in a 20-file repo: page 1 still shows all 20,
        // matched first, padding marked and without symbol lines.
        let g = repo(3, 17);
        let p = page(&g, "fix the widget handler", 0, None);
        let got = files(&p.body);
        assert_eq!(got.len(), 3, "matched files as anchors: {}", p.body);
        assert_eq!(
            padded(&p.body).len(),
            17,
            "padding as bare paths: {}",
            p.body
        );
        let boundary = p.body.find(PADDING_PREFIX).unwrap();
        for i in 0..3 {
            let at = p.body.find(&format!("@dir{}/file{i}.go", i % 3)).unwrap();
            assert!(at < boundary, "matched file{i} after padding");
        }
        assert!(!p.body.contains("zzz"), "padding files get no symbols");
        assert!(p.body.contains("已到最后一页"), "{}", p.body);
        // Files come before symbols in the budget.
        let last_file = p.body.rfind("\n@").unwrap();
        let first_sym = p.body.find("widget_handler_").unwrap();
        assert!(first_sym > last_file, "{}", p.body);
    }

    #[test]
    fn exact_identifier_file_leads_page_one() {
        let mut g = repo(70, 0);
        g.symbols[42].name = "processWidgetHandlerError".into();
        let p = page(&g, "where is processWidgetHandlerError called", 0, None);
        assert_eq!(files(&p.body)[0], "dir0/file42.go", "{}", p.body);
    }

    #[test]
    fn no_match_says_so_and_offers_no_paging() {
        let g = repo(70, 10);
        let p = page(&g, "qqqq zzzzzz", 0, None);
        assert!(p.unmatched);
        assert!(p.body.contains("未匹配到"), "{}", p.body);
        assert!(files(&p.body).len() <= NO_MATCH_FILES);
        assert!(next_offset(&p.body).is_none(), "{}", p.body);
    }

    /// Recall gate on two fixture repos shaped like the RECALL_CHECK.md
    /// failures: page 1 must carry the files the agent needed.
    fn indexed(files: &[(String, String)]) -> (crate::index::RepoIndex, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-recall-{}-{}",
            std::process::id(),
            files.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        (crate::index::build(&dir).expect("index"), dir)
    }

    fn page_one(index: &crate::index::RepoIndex, task: &str, compact: bool) -> Vec<String> {
        let g = index.graph();
        let (ranked, matched) = rank_and_match(g, task, &compute_centrality(g));
        let p = build_page(&PageRequest {
            graph: g,
            ranked: &ranked,
            matched: &matched,
            offset: 0,
            budget: if compact { 700 } else { 2_500 },
            compact,
            path_filter: None,
            index_line: "",
            generation_note: None,
        });
        let mut got = files(&p.body);
        got.extend(padded(&p.body));
        got
    }

    #[test]
    fn recall_large_repo_exact_identifier_and_its_callers_on_page_one() {
        // aiproxy H1 in miniature: 120 files full of `channel`/`error` names,
        // the identifier defined once and called from two files.
        let mut files: Vec<(String, String)> = (0..120)
            .map(|i| {
                (
                    format!("pkg{}/chan{i}.go", i % 6),
                    format!(
                        "package pkg\n\nfunc ChannelError{i}() {{}}\nfunc processChannel{i}() {{}}\nfunc errorChannel{i}() {{}}\nfunc channelProcessError{i}x() {{}}\n"
                    ),
                )
            })
            .collect();
        files.push((
            "controller/relay.go".into(),
            "package controller\n\nfunc processChannelError(c int) {}\n".into(),
        ));
        files.push((
            "controller/channel-test.go".into(),
            "package controller\n\nfunc testChannel() {\n\tprocessChannelError(1)\n}\n".into(),
        ));
        files.push((
            "service/retry.go".into(),
            "package service\n\nfunc retryLoop() {\n\tprocessChannelError(2)\n}\n".into(),
        ));
        let (index, dir) = indexed(&files);
        for (task, compact) in [
            ("find definition and call sites of processChannelError", true),
            (
                "In this Go repository: find every non-test call site of processChannelError and the enclosing function",
                true,
            ),
            ("how does processChannelError disable a channel", false),
        ] {
            let got = page_one(&index, task, compact);
            assert_eq!(got.first().map(String::as_str), Some("controller/relay.go"), "{task}: {got:?}");
            for need in ["controller/channel-test.go", "service/retry.go"] {
                assert!(got.iter().any(|f| f == need), "{task}: {need} not on page 1: {got:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recall_small_repo_page_one_keeps_files_without_task_words() {
        // cobra N6 in miniature: the needed `args.go` / `cobra.go` share no
        // sub-word with the task; a 36-file repo lists them anyway.
        let mut files: Vec<(String, String)> = (0..33)
            .map(|i| {
                (
                    format!("doc{i}_test.go"),
                    format!("package cobra\n\nfunc TestExactArgsWithInvalidCount{i}() {{}}\n"),
                )
            })
            .collect();
        files.push((
            "args.go".into(),
            "package cobra\n\nfunc ExactArgs(n int) {}\n".into(),
        ));
        files.push((
            "cobra.go".into(),
            "package cobra\n\nfunc OnInitialize() {}\n".into(),
        ));
        files.push((
            "command.go".into(),
            "package cobra\n\ntype Command struct{}\nfunc (c *Command) ValidateArgs() {}\n".into(),
        ));
        let (index, dir) = indexed(&files);
        let got = page_one(
            &index,
            "how many positional args validators count invalid",
            true,
        );
        for need in ["args.go", "cobra.go", "command.go"] {
            assert!(got.iter().any(|f| f == need), "{need}: {got:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
