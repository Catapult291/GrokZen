//! Output shaping for `find_symbol` / `search_code` (batch 6, from the S6
//! session audits): type-first ranking, folding substring noise behind an
//! exact name, Go implementor lines, smart-case literal search, path_filter
//! zero-hit diagnosis, and ready-made `path_filter` values in truncation hints.

use std::collections::{BTreeMap, HashSet};

use astrolabe_core::{CodeSymbol, SymbolKind};

use super::{estimate_tokens, name_match_tier, RepoIndex, SearchHit};

/// Same-tier order for `find_symbol`: types/interfaces/classes first, then
/// callables, then fields/variables/constants.
pub(super) fn kind_rank(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Interface
        | SymbolKind::Class
        | SymbolKind::Struct
        | SymbolKind::Trait
        | SymbolKind::Enum
        | SymbolKind::Type => 0,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Module => 1,
        _ => 2,
    }
}

/// One line for prefix/substring matches folded behind an exact name.
pub(super) fn folded_note(symbols: &[CodeSymbol], folded: &[usize], typed: &str) -> String {
    if folded.is_empty() {
        return String::new();
    }
    let mut seen = HashSet::new();
    let sample: Vec<&str> = folded
        .iter()
        .map(|&i| symbols[i].name.as_str())
        .filter(|n| seen.insert(*n))
        .take(3)
        .collect();
    format!(
        "另有 {} 个名字含 \"{typed}\" 的符号未列出（如 {}）；要看全部传 substring=true。\n",
        folded.len(),
        sample.join("、")
    )
}

/// Implementor lines for the first Go interfaces among `hits` (symbol
/// indices, ranked), at most `GO_IMPLEMENTOR_IFACES` interfaces and `budget`
/// tokens. Empty when none has implementors.
pub(super) fn implementors_block(
    index: &RepoIndex,
    hits: impl Iterator<Item = usize>,
    budget: usize,
) -> String {
    let graph = index.graph();
    let symbols = &graph.symbols;
    let mut order: Vec<u32> = Vec::new();
    for idx in hits {
        let s = &symbols[idx];
        if s.kind == SymbolKind::Interface && !order.contains(&s.id.0) {
            order.push(s.id.0);
            if order.len() >= super::GO_IMPLEMENTOR_IFACES {
                break;
            }
        }
    }
    if order.is_empty() {
        return String::new();
    }
    let set: HashSet<u32> = order.iter().copied().collect();
    let by = astrolabe_core::go_iface::implementors_of(graph, &set);
    let mut out = String::new();
    for iface in &order {
        let Some(impls) = by.get(iface) else { continue };
        let line = astrolabe_core::go_iface::render_implementors(
            graph,
            *iface,
            impls,
            super::GO_IMPLEMENTORS_LISTED,
        );
        if estimate_tokens(&out) + estimate_tokens(&line) <= budget {
            out.push_str(&line);
        }
    }
    out
}

/// `find_symbol` zero hits under a `path_filter`: how many name matches the
/// filter removed and where, or that the whole index has none.
pub(super) fn symbol_hits_outside_filter(
    index: &RepoIndex,
    needle: &str,
    kind: Option<SymbolKind>,
    filter: &str,
) -> String {
    let symbols = &index.graph().symbols;
    let mut outside: Vec<(u8, String)> = Vec::new();
    for (idx, lower) in index.names().lower().iter().enumerate() {
        let Some(tier) = name_match_tier(lower, needle) else {
            continue;
        };
        let s = &symbols[idx];
        if kind.is_some_and(|k| s.kind != k) {
            continue;
        }
        if let Some(file) = index.file(s.file) {
            outside.push((tier, format!("{}:{}", file.path.as_str(), s.start_line)));
        }
    }
    let mut note = if outside.is_empty() {
        format!("path_filter=\"{filter}\" 之外也没有：全仓库都没有这个名字。\n")
    } else {
        outside.sort();
        let sample: Vec<&str> = outside.iter().take(3).map(|(_, l)| l.as_str()).collect();
        format!(
            "path_filter=\"{filter}\" 内没有；过滤范围外有 {} 处：{}{}\n",
            outside.len(),
            sample.join("、"),
            if outside.len() > 3 { " …" } else { "" }
        )
    };
    if index.file_id(filter).is_some() {
        note.push_str(&format!(
            "要这个文件的大纲：query=\"{filter}\"（文件路径作为 query）。\n"
        ));
    }
    note
}

/// `search_code` zero hits under a `path_filter`: the hits outside it (count
/// and first locations, `file:line,line`), or that the repo has none.
pub(super) fn search_hits_outside_filter(filter: &str, outside: &[SearchHit]) -> String {
    if outside.is_empty() {
        return format!("path_filter=\"{filter}\" 之外也没有：全仓库都没有这个字面量。\n");
    }
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();
    for h in outside {
        let e = by_file.entry(h.path.as_str()).or_default();
        if e.is_empty() {
            order.push(h.path.as_str());
        }
        e.push(h.line);
    }
    let shown: Vec<String> = order
        .iter()
        .take(3)
        .map(|p| {
            let lines: Vec<String> = by_file[p].iter().take(4).map(|l| l.to_string()).collect();
            format!("{p}:{}", lines.join(","))
        })
        .collect();
    format!(
        "path_filter=\"{filter}\" 内没有；过滤范围外有 {} 处 / {} 个文件：{}{}\n",
        outside.len(),
        by_file.len(),
        shown.join("；"),
        if order.len() > 3 { " …" } else { "" }
    )
}

/// Smart case for literal search: case-sensitive when the query has an
/// uppercase letter, unless the caller set `case_sensitive` or `regex`.
pub(super) fn literal_case_sensitive(query: &str, regex: bool, explicit: Option<bool>) -> bool {
    explicit.unwrap_or(regex || query.chars().any(char::is_uppercase))
}

/// `true` when smart case (not the caller) made this literal search
/// case-sensitive, so case-only variants are worth counting.
pub(super) fn smart_case_applied(query: &str, regex: bool, explicit: Option<bool>) -> bool {
    explicit.is_none() && !regex && query.chars().any(char::is_uppercase)
}

/// Footnote for case-only variants hidden by smart case.
pub(super) fn case_variant_note(variants: &[SearchHit]) -> String {
    if variants.is_empty() {
        return String::new();
    }
    let sample = &variants[0];
    let text: String = sample.text.trim().chars().take(60).collect();
    format!(
        "另有 {} 行仅大小写不同（如 {}:{} {text}）；含大写的字面量默认区分大小写，传 case_sensitive=false 一并列出。\n",
        variants.len(),
        sample.path,
        sample.line
    )
}

/// The narrowest useful `path_filter` for hits a page did not show: the file
/// holding most of them when it has at least half, else their most common
/// top-level directory. `None` when nothing is left.
pub(super) fn suggest_path_filter<'a>(rest: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dirs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut total = 0usize;
    for path in rest {
        total += 1;
        *files.entry(path).or_default() += 1;
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        *dirs.entry(dir).or_default() += 1;
    }
    let (file, n) = files
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))?;
    if *n * 2 >= total || files.len() == 1 {
        return Some(file.to_string());
    }
    let (dir, dn) = dirs.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))?;
    if dir.is_empty() || *dn * 2 < total {
        return Some(file.to_string());
    }
    Some(format!("{dir}/"))
}

/// [`suggest_path_filter`] for `find_symbol` hits (`(line, symbol index)`).
pub(super) fn suggest_symbol_filter(index: &RepoIndex, rest: &[(String, usize)]) -> Option<String> {
    let symbols = &index.graph().symbols;
    let paths: Vec<&str> = rest
        .iter()
        .filter_map(|(_, idx)| index.file(symbols[*idx].file).map(|f| f.path.as_str()))
        .collect();
    suggest_path_filter(paths.into_iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, line: usize) -> SearchHit {
        SearchHit {
            path: path.into(),
            line,
            text: "x".into(),
            text_only: false,
        }
    }

    #[test]
    fn smart_case_only_for_uppercase_literals_without_override() {
        assert!(literal_case_sensitive("Execute", false, None));
        assert!(!literal_case_sensitive("execute", false, None));
        assert!(!literal_case_sensitive("Execute", false, Some(false)));
        assert!(literal_case_sensitive("x", true, None));
        assert!(smart_case_applied("Execute", false, None));
        assert!(!smart_case_applied("Execute", false, Some(true)));
        assert!(!smart_case_applied("Execute", true, None));
    }

    #[test]
    fn suggested_filter_is_the_dominant_file_or_directory() {
        let hits = [hit("a/x.go", 1), hit("a/x.go", 2), hit("b/y.go", 3)];
        assert_eq!(
            suggest_path_filter(hits.iter().map(|h| h.path.as_str())).as_deref(),
            Some("a/x.go")
        );
        let hits = [
            hit("a/x.go", 1),
            hit("a/y.go", 2),
            hit("a/z.go", 3),
            hit("b/q.go", 1),
        ];
        assert_eq!(
            suggest_path_filter(hits.iter().map(|h| h.path.as_str())).as_deref(),
            Some("a/")
        );
        assert_eq!(suggest_path_filter(std::iter::empty()), None);
    }

    #[test]
    fn outside_filter_note_counts_and_lists_locations() {
        let out = [
            hit("command.go", 789),
            hit("command.go", 862),
            hit("args.go", 3),
        ];
        let note = search_hits_outside_filter("_test.go", &out);
        assert!(note.contains("过滤范围外有 3 处 / 2 个文件"), "{note}");
        assert!(note.contains("command.go:789,862"), "{note}");
        assert!(!note.contains("过窄"), "{note}");
        let none = search_hits_outside_filter("_test.go", &[]);
        assert!(none.contains("全仓库都没有"), "{none}");
    }
}

/// End-to-end regressions for the batch-6 output changes (S6 audits).
#[cfg(test)]
mod behaviour {
    use super::super::*;
    use crate::session::IndexState;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn server_on(files: &[(&str, String)]) -> (AstrolabeServer, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-b6-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
            query: query.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: None,
            page: false,
            group: None,
            dedup: Some(false),
            aggregate: None,
            exclude: None,
            count_only: false,
            skip_comments: false,
            glob: None,
        }
    }

    fn find(query: &str) -> QueryParams {
        QueryParams {
            root: None,
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

    fn text(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    /// Go repo: an interface, two production implementors, a test double,
    /// and several struct fields / a lowercase variable named like it.
    fn provider_repo() -> Vec<(&'static str, String)> {
        let mut files = vec![
            ("go.mod", "module example.com/m\n".to_string()),
            (
                "oauth/provider.go",
                "package oauth\n\ntype Provider interface {\n\tGetName() string\n\tLogin(code string) error\n}\n"
                    .to_string(),
            ),
            (
                "oauth/github.go",
                "package oauth\n\ntype GitHubProvider struct{}\n\nfunc (p *GitHubProvider) GetName() string { return \"gh\" }\nfunc (p *GitHubProvider) Login(code string) error { return nil }\n"
                    .to_string(),
            ),
            (
                "oauth/discord.go",
                "package oauth\n\ntype DiscordProvider struct{}\n\nfunc (p *DiscordProvider) GetName() string { return \"d\" }\nfunc (p *DiscordProvider) Login(code string) error { return nil }\n"
                    .to_string(),
            ),
            (
                "oauth/flow_test.go",
                "package oauth\n\ntype fakeFlowProvider struct{}\n\nfunc (p *fakeFlowProvider) GetName() string { return \"t\" }\nfunc (p *fakeFlowProvider) Login(code string) error { return nil }\n"
                    .to_string(),
            ),
            (
                "model/a.go",
                "package model\n\nvar provider = 1\n\nfunc ProviderName() string { return \"\" }\n".to_string(),
            ),
        ];
        // Struct fields named exactly `Provider` in files that sort first.
        for d in ["aa", "ab", "ac", "ad"] {
            files.push((
                Box::leak(format!("{d}/rec.go").into_boxed_str()),
                format!("package {d}\n\ntype Rec struct {{\n\tProvider string\n\tOther int\n}}\n"),
            ));
        }
        files
    }

    #[test]
    fn find_symbol_ranks_types_first_exact_case_first_and_folds_substrings() {
        let (server, dir) = server_on(&provider_repo());
        let out = text(&server.find_symbol(Parameters(find("Provider"))));
        let rows: Vec<&str> = out.lines().filter(|l| l.contains("  @")).collect();
        assert!(rows[0].starts_with("Provider interface"), "{out}");
        let field = out.find("Provider string").expect(&out);
        let lower = out.find("provider = 1").expect(&out);
        assert!(field < lower, "exact case before case variant: {out}");
        assert!(
            !out.contains("func ProviderName"),
            "substring folded: {out}"
        );
        assert!(out.contains("另有 "), "{out}");
        assert!(
            out.contains("ProviderName"),
            "fold note names a sample: {out}"
        );
        assert!(out.contains("substring=true"), "{out}");
        assert!(!out.contains("请用锚点核验"), "{out}");
        // Implementors: rule + count, production first, test double tagged.
        assert!(out.contains("方法 2/2 全匹配"), "{out}");
        assert!(!out.contains("误报"), "{out}");
        let gh = out.find("GitHubProvider @oauth/github.go:3").expect(&out);
        let fake = out
            .find("fakeFlowProvider @oauth/flow_test.go:3 [测试]")
            .expect(&out);
        assert!(gh < fake, "{out}");
        // substring=true lists everything.
        let mut all = find("Provider");
        all.substring = Some(true);
        let out = text(&server.find_symbol(Parameters(all)));
        assert!(out.contains("func ProviderName"), "{out}");
        // The confidence parenthetical is sent once per session.
        assert!(!out.contains("(符号名称匹配)"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_keeps_implementors_on_a_truncated_page_and_names_a_filter() {
        let mut files = provider_repo();
        for i in 0..40 {
            files.push((
                Box::leak(format!("zz{i:02}/rec.go").into_boxed_str()),
                format!("package zz\n\ntype Rec{i} struct {{\n\tProvider string\n}}\n"),
            ));
        }
        let (server, dir) = server_on(&files);
        let mut p = find("Provider");
        p.budget_tokens = 300;
        let out = text(&server.find_symbol(Parameters(p)));
        assert!(out.contains("truncated=true"), "{out}");
        assert!(out.contains("Go 隐式实现方"), "{out}");
        assert!(out.contains("收窄：path_filter="), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_query_lists_exact_case_first() {
        let (server, dir) = server_on(&[(
            "cmd.go",
            "package c\n\ntype Command struct{}\n\nfunc (c *Command) execute() error { return nil }\n\nfunc (c *Command) Execute() error { return c.execute() }\n"
                .to_string(),
        )]);
        let out = text(&server.find_symbol(Parameters(find("Command.Execute"))));
        let upper = out.find("Execute() error").expect(&out);
        let lower = out.find("execute() error").expect(&out);
        assert!(upper < lower, "{out}");
        assert!(!out.contains("请核验"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outline_and_resolve_context_show_go_implementors() {
        let (server, dir) = server_on(&provider_repo());
        let out = text(&server.find_symbol(Parameters(find("oauth/provider.go"))));
        assert!(out.contains("outline:"), "{out}");
        assert!(
            out.contains("Go 隐式实现方") && out.contains("GitHubProvider"),
            "{out}"
        );
        let rc = ResolveContextParams {
            root: None,
            task_description: "which types implement the Provider interface?".into(),
            budget_tokens: 2500,
            dedup: Some(false),
            offset: 0,
            path_filter: None,
        };
        let out = text(&server.resolve_context(Parameters(rc)));
        assert!(out.contains("Go 隐式实现方"), "{out}");
        assert!(out.contains("DiscordProvider @oauth/discord.go:3"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_zero_hits_under_path_filter_reports_hits_outside() {
        let (server, dir) = server_on(&provider_repo());
        let mut p = find("GitHubProvider");
        p.path_filter = Some("_test.go".into());
        let out = text(&server.find_symbol(Parameters(p)));
        assert!(out.contains("过滤范围外有"), "{out}");
        assert!(out.contains("oauth/github.go:3"), "{out}");
        let mut p = find("controller");
        p.path_filter = Some("oauth/github.go".into());
        let out = text(&server.find_symbol(Parameters(p)));
        assert!(out.contains("全仓库都没有这个名字"), "{out}");
        assert!(out.contains("query=\"oauth/github.go\""), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_zero_hits_under_path_filter_lists_hits_outside() {
        let (server, dir) = server_on(&provider_repo());
        let mut p = search("GetName");
        p.path_filter = Some("model/".into());
        let out = text(&server.search_code(Parameters(p)));
        assert!(out.contains("(未找到匹配的代码行)"), "{out}");
        assert!(
            out.contains("path_filter=\"model/\" 内没有；过滤范围外有"),
            "{out}"
        );
        assert!(out.contains("oauth/discord.go:5"), "{out}");
        assert!(!out.contains("过窄"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The H3 duplicate: hits in a function too long to print whole used to
    /// render one by one, so overlapping windows printed lines twice.
    #[test]
    fn long_function_hits_merge_windows_and_state_the_range() {
        let mut body = String::from("def long_fn():\n");
        for i in 0..200 {
            if i == 100 || i == 103 {
                body.push_str(&format!("    x{i} = MERGE_ME\n"));
            } else {
                body.push_str(&format!("    y{i} = {i}\n"));
            }
        }
        let (server, dir) = server_on(&[("big.py", body)]);
        let out = text(&server.search_code(Parameters(search("MERGE_ME"))));
        for n in 99..=105 {
            let line = format!("  {n}| ");
            assert!(out.matches(&line).count() <= 1, "line {n} twice: {out}");
        }
        assert!(out.contains("@big.py:102"), "{out}");
        assert!(out.contains("@big.py:105"), "{out}");
        assert!(
            out.contains("long_fn · big.py:1-201"),
            "range stated: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grouped_rows_carry_function_ranges_cap_test_rows_and_legend_once() {
        let prod: String = (0..3)
            .map(|i| format!("func f{i}() {{\n\tuse(\"GRP_KEY\")\n}}\n\n"))
            .collect();
        let tests: String = (0..5)
            .map(|i| format!("func TestG{i}(t *testing.T) {{\n\tuse(\"GRP_KEY\")\n}}\n\n"))
            .collect();
        let (server, dir) = server_on(&[
            ("a/one.go", format!("package a\n\n{prod}")),
            ("a/two.go", format!("package a\n\n{prod}")),
            ("b/three.go", format!("package b\n\n{prod}")),
            ("b/x_test.go", format!("package b\n\n{tests}")),
        ]);
        let mut p = search("GRP_KEY");
        p.group = Some(true);
        let out = text(&server.search_code(Parameters(p.clone())));
        assert!(
            out.contains("L4 [定义] in f0@3-5:") || out.contains(" in f0@3-5:"),
            "{out}"
        );
        assert!(
            out.contains("命中行标注"),
            "first grouped result has the legend: {out}"
        );
        let listed = out.lines().filter(|l| l.contains("in TestG")).count();
        assert_eq!(listed, 2, "test rows capped at 2: {out}");
        assert!(
            out.contains("测试另有 3 处，在 TestG2、TestG3、TestG4"),
            "{out}"
        );
        assert!(
            out.contains("测试文件各列 2 处，其余 3 处已按文件计数"),
            "{out}"
        );
        let again = text(&server.search_code(Parameters(p)));
        assert!(!again.contains("命中行标注"), "legend once: {again}");
        assert!(again.contains("confidence: exact\n"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
