//! `find_references` output extras (batch 7, stream R).
//!
//! Measured on the batch 6 runs: after a reference list the model Read the
//! reported function range in 102 / 183 Reads, and re-ran `git grep` on a
//! complete answer in 30 / 45 greps. This module adds, compactly:
//!
//! * [`context_blocks`]: for a short list (≤ [`CONTEXT_MAX_HITS`] non-test
//!   code hits), the enclosing function inline when it is short, else a
//!   ±[`CONTEXT_RADIUS`] window, merged per file and capped in bytes.
//!   **Off by default since batch 8** (opt in with
//!   `ASTROLABE_REFS_CONTEXT_HITS`): on the batch 7 C runs (33) the model
//!   Read 0 ranges that were fully shown inline and still Read 26 times
//!   around hits whose context it had just been given (21 behind a ±4
//!   window in a long function, 5 behind a whole short function), so the
//!   ~960 B/run of context (≈10 KB/run once re-sent on later turns) saved
//!   no Reads; see `/workspace/audit/b8cx/REPORT.md`;
//! * [`caller_summary`]: one line naming the non-test callers when test hits
//!   outnumber them;
//! * [`completion_line`]: an explicit "complete, no grep needed" line, only
//!   for answers that are complete by construction.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use astrolabe_core::lsp::{queries::AMBIGUOUS_DEFINITIONS_MARK, Location};
use astrolabe_core::types::Confidence;

use crate::refs_graph::{HitInfo, HitKind};

/// When enabled, context is shown for lists with at most this many non-test
/// code hits. Disabled by default; `ASTROLABE_REFS_CONTEXT_HITS=on` (or
/// `1`/`true`) enables it with this threshold, a number `N > 1` enables it
/// with threshold `N`, and `0`/`off`/unset keeps it off.
pub(crate) const CONTEXT_MAX_HITS: usize = 8;
/// A function this short (lines) is inlined whole instead of a window.
pub(crate) const CONTEXT_WHOLE_FN_LINES: u32 = 40;
/// Lines above / below a hit when the function is longer.
pub(crate) const CONTEXT_RADIUS: u32 = 4;
/// Byte cap for all context of one result.
pub(crate) const CONTEXT_MAX_BYTES: usize = 4_800;
/// Columns kept per context line.
const CONTEXT_COLUMNS: usize = 120;
/// Callers named in the summary line.
const SUMMARY_MAX_CALLERS: usize = 12;

/// Marker of the index (graph) answer in the backend note; see
/// `refs_graph::graph_references`.
const GRAPH_ANSWER_MARK: &str = "索引作答";
/// Note fragments that mean the answer may be incomplete or imprecise.
const INCOMPLETE_MARKS: &[&str] = &[
    "still indexing",
    "incomplete",
    "未核",
    "不完整",
    "可能不全",
    "文本回退",
    "fallback",
];

/// Threshold from an `ASTROLABE_REFS_CONTEXT_HITS` value; 0 = off.
fn parse_context_hits(v: Option<&str>) -> usize {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return 0;
    };
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => CONTEXT_MAX_HITS,
        "off" | "false" | "no" | "0" => 0,
        n => n.parse().unwrap_or(0),
    }
}

#[cfg(test)]
thread_local! {
    /// Test override of the threshold (env vars are process-global).
    static TEST_CONTEXT_HITS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn context_max_hits() -> usize {
    #[cfg(test)]
    if let Some(n) = TEST_CONTEXT_HITS.with(|c| c.get()) {
        return n;
    }
    parse_context_hits(std::env::var("ASTROLABE_REFS_CONTEXT_HITS").ok().as_deref())
}

/// Context per hit index (`locations` index) and the hit indices whose own
/// line is shown inside some block (their one-line text is redundant).
#[derive(Debug, Default)]
pub(crate) struct Context {
    pub blocks: HashMap<usize, String>,
    pub covered: HashSet<usize>,
}

/// Build context blocks for the hits in display `order`. `lines_of` returns
/// a file's lines (cached). Empty when the list is not short.
pub(crate) fn context_blocks(
    locations: &[Location],
    infos: &[HitInfo],
    order: &[usize],
    lines_of: &dyn Fn(&str) -> Option<Arc<Vec<String>>>,
) -> Context {
    let eligible: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| {
            let h = &infos[i];
            !h.test && !h.declaration && h.kind == HitKind::Code
        })
        .collect();
    if eligible.is_empty() || eligible.len() > context_max_hits() {
        return Context::default();
    }
    // Per file: (1-based inclusive range, owning hit) in display order.
    let mut by_file: BTreeMap<&str, Vec<(u32, u32, usize)>> = BTreeMap::new();
    let mut file_order: Vec<&str> = Vec::new();
    for &i in &eligible {
        let path = locations[i].path.as_str();
        let line1 = locations[i].range.start.line + 1;
        let (lo, hi) = match &infos[i].enclosing {
            Some((_, s, e)) if e >= s && e - s < CONTEXT_WHOLE_FN_LINES => (*s, *e),
            Some((_, s, e)) if e >= s => (
                line1.saturating_sub(CONTEXT_RADIUS).max(*s),
                (line1 + CONTEXT_RADIUS).min(*e),
            ),
            _ => (
                line1.saturating_sub(CONTEXT_RADIUS).max(1),
                line1 + CONTEXT_RADIUS,
            ),
        };
        if !file_order.contains(&path) {
            file_order.push(path);
        }
        by_file.entry(path).or_default().push((lo, hi, i));
    }
    let mut out = Context::default();
    let mut bytes = 0usize;
    'files: for path in file_order {
        let Some(lines) = lines_of(path) else {
            continue;
        };
        let mut ranges = by_file.remove(path).unwrap_or_default();
        // Owner = the hit shown first; merge overlapping / adjacent ranges.
        let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(k, &i)| (i, k)).collect();
        ranges.sort_by_key(|r| (r.0, r.1));
        let mut merged: Vec<(u32, u32, usize)> = Vec::new();
        for r in ranges {
            match merged.last_mut() {
                Some(m) if r.0 <= m.1 + 1 => {
                    m.1 = m.1.max(r.1);
                    if rank[&r.2] < rank[&m.2] {
                        m.2 = r.2;
                    }
                }
                _ => merged.push(r),
            }
        }
        let hit_lines: HashMap<u32, usize> = eligible
            .iter()
            .filter(|&&i| locations[i].path.as_str() == path)
            .map(|&i| (locations[i].range.start.line + 1, i))
            .collect();
        for (lo, hi, owner) in merged {
            let hi = hi.min(lines.len() as u32);
            if lo > hi {
                continue;
            }
            let mut block = String::new();
            for n in lo..=hi {
                let text = lines[(n - 1) as usize].trim_end();
                let clipped: String = text.chars().take(CONTEXT_COLUMNS).collect();
                let more = if clipped.len() < text.len() {
                    "…"
                } else {
                    ""
                };
                let mark = if hit_lines.contains_key(&n) { '>' } else { '|' };
                block.push_str(&format!("    {n}{mark} {clipped}{more}\n"));
            }
            if bytes + block.len() > CONTEXT_MAX_BYTES {
                break 'files;
            }
            bytes += block.len();
            for (&n, &i) in &hit_lines {
                if (lo..=hi).contains(&n) {
                    out.covered.insert(i);
                }
            }
            out.blocks.insert(owner, block);
        }
    }
    out
}

/// One line naming the non-test callers when tests outnumber them, e.g.
/// `非测试调用方（3 处 / 2 个函数）：Relay@controller/relay.go×2(L271,L410)；…`.
pub(crate) fn caller_summary(locations: &[Location], infos: &[HitInfo]) -> Option<String> {
    let code = |h: &HitInfo| !h.declaration && h.kind == HitKind::Code;
    let tests = infos.iter().filter(|h| h.test && code(h)).count();
    let prod: Vec<usize> = (0..infos.len())
        .filter(|&i| !infos[i].test && code(&infos[i]))
        .collect();
    if tests == 0 || tests <= prod.len() {
        return None;
    }
    if prod.is_empty() {
        return Some(format!(
            "非测试调用方：0 处（{tests} 处代码引用全在测试里）。\n"
        ));
    }
    let mut callers: Vec<(String, Vec<u32>)> = Vec::new();
    for &i in &prod {
        let path = locations[i].path.as_str();
        let name = infos[i]
            .enclosing
            .as_ref()
            .map_or("<顶层>", |(n, _, _)| n.as_str());
        let key = format!("{name}@{path}");
        let line1 = locations[i].range.start.line + 1;
        match callers.iter_mut().find(|(k, _)| *k == key) {
            Some((_, ls)) => ls.push(line1),
            None => callers.push((key, vec![line1])),
        }
    }
    let n_callers = callers.len();
    let parts: Vec<String> = callers
        .iter()
        .take(SUMMARY_MAX_CALLERS)
        .map(|(k, ls)| {
            let ls: Vec<String> = ls.iter().map(|l| format!("L{l}")).collect();
            if ls.len() > 1 {
                format!("{k}×{}({})", ls.len(), ls.join(","))
            } else {
                format!("{k}({})", ls[0])
            }
        })
        .collect();
    let mut out = format!(
        "非测试调用方（{} 处 / {n_callers} 个函数；测试 {tests} 处）：{}",
        prod.len(),
        parts.join("；")
    );
    if n_callers > SUMMARY_MAX_CALLERS {
        out.push_str(&format!("；…另 {} 个", n_callers - SUMMARY_MAX_CALLERS));
    }
    out.push_str("。\n");
    Some(out)
}

/// Where a complete answer came from, or `None` when it may be incomplete.
pub(crate) fn complete_source(confidence: Confidence, note: Option<&str>) -> Option<&'static str> {
    let note = note.unwrap_or("");
    if note.contains(AMBIGUOUS_DEFINITIONS_MARK)
        || INCOMPLETE_MARKS.iter().any(|m| note.contains(m))
    {
        return None;
    }
    match confidence {
        Confidence::Exact => Some("语言服务器解析"),
        Confidence::Scoped if note.contains(GRAPH_ANSWER_MARK) => {
            Some(if note.contains("定义文件") {
                // TS/JS/Python: the declaring file, its importers and the
                // re-export chain.
                "索引：定义文件 + 全部导入方及转导出链"
            } else {
                "索引：定义所在包 + 全部导入方"
            })
        }
        _ => None,
    }
}

/// Closing line of a complete, unpaged reference list.
pub(crate) fn completion_line(total: usize, files: usize, source: &str) -> String {
    format!("已全列：共 {total} 处 / {files} 个文件，覆盖全仓（{source}），无需再 grep 复核。\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precise_tools::{run_find_references, FindReferencesParams, PreciseCapability};
    use astrolabe_core::lsp::{Diagnostic, Position, Precise, Range};
    use astrolabe_core::types::RelPath;
    use rmcp::model::ContentBlock;

    fn loc(path: &str, line0: u32) -> Location {
        let p = Position {
            line: line0,
            character: 4,
        };
        Location {
            path: RelPath::new(path),
            range: Range { start: p, end: p },
        }
    }

    fn hit(func: Option<(&str, u32, u32)>, test: bool) -> HitInfo {
        HitInfo {
            text: Some("callee(x)".into()),
            enclosing: func.map(|(n, a, b)| (n.to_string(), a, b)),
            declaration: false,
            kind: HitKind::Code,
            test,
        }
    }

    #[derive(Debug)]
    struct Fake {
        refs: Vec<Location>,
        infos: Vec<HitInfo>,
        confidence: Confidence,
        note: Option<String>,
    }

    impl PreciseCapability for Fake {
        fn references(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<Position>,
            _: bool,
        ) -> Precise<Vec<Location>> {
            Precise {
                value: self.refs.clone(),
                confidence: self.confidence,
                note: self.note.clone(),
            }
        }
        fn annotate(&self, _: &str, _: &[Location]) -> Vec<HitInfo> {
            self.infos.clone()
        }
        fn source_lines(&self, _path: &str) -> Option<Arc<Vec<String>>> {
            Some(Arc::new((1..=200).map(|n| format!("line {n}")).collect()))
        }
        fn definition(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<u32>,
            _: Option<u32>,
        ) -> Precise<Vec<Location>> {
            Precise::exact(Vec::new())
        }
        fn diagnostics(&self, _: &str) -> Precise<Vec<Diagnostic>> {
            Precise::exact(Vec::new())
        }
    }

    /// Run `f` with inline context enabled at `n` (default: off).
    fn with_context<T>(n: usize, f: impl FnOnce() -> T) -> T {
        TEST_CONTEXT_HITS.with(|c| c.set(Some(n)));
        let out = f();
        TEST_CONTEXT_HITS.with(|c| c.set(None));
        out
    }

    fn run(fake: Fake, budget: usize) -> (String, serde_json::Value) {
        let r = run_find_references(
            FindReferencesParams {
                root: None,
                symbol: "callee".into(),
                path: Some("a.go".into()),
                line: None,
                character: None,
                budget_tokens: budget,
                offset: None,
                dry_run_rename: None,
            },
            &fake,
        );
        let text = r
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|c| c.text.clone())
            .unwrap_or_default();
        (text, r.structured_content.unwrap_or_default())
    }

    /// Two production callers (one short function, one long) and three
    /// test callers, as in H1.
    fn h1_like(confidence: Confidence, note: Option<&str>) -> Fake {
        Fake {
            refs: vec![
                loc("relay.go", 11),
                loc("relay.go", 99),
                loc("relay_test.go", 9),
                loc("relay_test.go", 19),
                loc("relay_test.go", 29),
            ],
            infos: vec![
                hit(Some(("Short", 10, 14)), false),
                hit(Some(("Long", 50, 150)), false),
                hit(Some(("TestA", 8, 12)), true),
                hit(Some(("TestB", 18, 22)), true),
                hit(Some(("TestC", 28, 32)), true),
            ],
            confidence,
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn short_lists_inline_caller_context_and_say_complete() {
        let (text, json) = with_context(CONTEXT_MAX_HITS, || {
            run(h1_like(Confidence::Exact, None), 2_500)
        });
        // Short function inlined whole, hit line marked.
        assert!(
            text.contains("    10| line 10\n    11| line 11\n    12> line 12\n"),
            "{text}"
        );
        assert!(text.contains("    14| line 14"), "{text}");
        // Long function: ±4 window only.
        assert!(text.contains("    96| line 96\n"), "{text}");
        assert!(text.contains("    100> line 100\n"), "{text}");
        assert!(text.contains("    104| line 104"), "{text}");
        assert!(!text.contains("    95| "), "{text}");
        assert!(!text.contains("    105| "), "{text}");
        // Covered hits drop their one-line text; test hits keep theirs.
        assert!(text.contains("  L12:5 in Short[10-14]\n"), "{text}");
        assert!(text.contains("in TestA[8-12]: callee(x)"), "{text}");
        assert!(
            !text.contains("    9> "),
            "test hits get no context: {text}"
        );
        assert!(
            text.contains(
                "非测试调用方（2 处 / 2 个函数；测试 3 处）：Short@relay.go(L12)；Long@relay.go(L100)。"
            ),
            "{text}"
        );
        assert!(
            text.trim_end().ends_with(
                "已全列：共 5 处 / 2 个文件，覆盖全仓（语言服务器解析），无需再 grep 复核。"
            ),
            "{text}"
        );
        assert_eq!(json["complete"], true);
        assert_eq!(json["context_blocks"], 2);
    }

    #[test]
    fn completion_line_only_for_complete_answers() {
        let graph = "索引作答（未启动语言服务器）：`callee` 全仓唯一定义 @relay.go:3；以下是包 ./ 内及导入该包的文件中的代码出现。";
        let (text, _) = run(h1_like(Confidence::Scoped, Some(graph)), 2_500);
        assert!(
            text.contains("已全列：共 5 处 / 2 个文件，覆盖全仓（索引：定义所在包 + 全部导入方）"),
            "{text}"
        );
        for (c, note) in [
            (
                Confidence::Syntactic,
                Some("language server still indexing, results may be incomplete"),
            ),
            (Confidence::Scoped, Some("[多处定义] merged")),
            (Confidence::Scoped, None),
            (Confidence::Exact, Some("language server still indexing")),
        ] {
            let (text, json) = run(h1_like(c, note), 2_500);
            assert!(!text.contains("已全列"), "{c:?} {note:?}: {text}");
            assert_eq!(json["complete"], false);
        }
        // TS index answer: complete, labelled with the re-export chain;
        // a truncated re-export walk is not complete.
        let ts = "索引作答（未启动语言服务器）：`f` 全仓唯一定义 @a.ts:1；以下是定义文件、导入它的文件中的代码出现。";
        let (text, _) = run(h1_like(Confidence::Scoped, Some(ts)), 2_500);
        assert!(
            text.contains("（索引：定义文件 + 全部导入方及转导出链）"),
            "{text}"
        );
        let cut = format!("{ts}转导出链超过跟踪上限，结果可能不全。");
        let (text, _) = run(h1_like(Confidence::Scoped, Some(&cut)), 2_500);
        assert!(!text.contains("已全列"), "{text}");
        let receiver = format!("{graph}\n[接收者未核] 2 处");
        let (text, _) = run(h1_like(Confidence::Scoped, Some(&receiver)), 2_500);
        assert!(!text.contains("已全列"), "{text}");
    }

    #[test]
    fn paged_or_long_lists_get_no_context_or_completion() {
        // Budget too small for the context: plain list; paged: no "complete".
        let (text, json) = with_context(CONTEXT_MAX_HITS, || {
            run(h1_like(Confidence::Exact, None), 60)
        });
        assert!(!text.contains("> line"), "{text}");
        assert!(!text.contains("已全列"), "{text}");
        assert_eq!(json["complete"], false);
        // More than CONTEXT_MAX_HITS production hits: no context.
        let n = CONTEXT_MAX_HITS + 1;
        let fake = Fake {
            refs: (0..n as u32).map(|k| loc("a.go", 10 * k + 1)).collect(),
            infos: (0..n as u32)
                .map(|k| hit(Some(("F", 10 * k + 1, 10 * k + 5)), false))
                .collect(),
            confidence: Confidence::Exact,
            note: None,
        };
        let (text, json) = with_context(CONTEXT_MAX_HITS, || run(fake, 2_500));
        assert!(!text.contains("> line"), "{text}");
        assert_eq!(json["context_blocks"], 0);
        assert!(!text.contains("非测试调用方"), "{text}");
        assert!(text.contains("已全列：共 9 处"), "{text}");
    }

    #[test]
    fn caller_summary_when_tests_dominate() {
        let locs = vec![
            loc("a.go", 4),
            loc("a.go", 6),
            loc("a_test.go", 1),
            loc("a_test.go", 2),
            loc("a_test.go", 3),
        ];
        let infos = vec![
            hit(Some(("Run", 1, 9)), false),
            hit(Some(("Run", 1, 9)), false),
            hit(None, true),
            hit(None, true),
            hit(None, true),
        ];
        assert_eq!(
            caller_summary(&locs, &infos).unwrap(),
            "非测试调用方（2 处 / 1 个函数；测试 3 处）：Run@a.go×2(L5,L7)。\n"
        );
        let only_tests: Vec<HitInfo> = (0..3).map(|_| hit(None, true)).collect();
        assert_eq!(
            caller_summary(&locs[2..], &only_tests).unwrap(),
            "非测试调用方：0 处（3 处代码引用全在测试里）。\n"
        );
        // Production hits not outnumbered: no line.
        assert!(caller_summary(&locs[..3], &infos[..3]).is_none());
    }

    #[test]
    fn overlapping_windows_merge_and_respect_the_byte_cap() {
        let locs = vec![loc("a.go", 99), loc("a.go", 102)];
        let infos = vec![
            hit(Some(("Long", 1, 190)), false),
            hit(Some(("Long", 1, 190)), false),
        ];
        let lines = |_: &str| Some(Arc::new((1..=200).map(|n| format!("line {n}")).collect()));
        let ctx = with_context(CONTEXT_MAX_HITS, || {
            context_blocks(&locs, &infos, &[0, 1], &lines)
        });
        assert_eq!(ctx.blocks.len(), 1);
        let block = &ctx.blocks[&0];
        assert!(
            block.starts_with("    96| ") && block.trim_end().ends_with("107| line 107"),
            "{block}"
        );
        assert!(
            block.contains("    100> ") && block.contains("    103> "),
            "{block}"
        );
        assert_eq!(ctx.covered.len(), 2);
        let wide = |_: &str| Some(Arc::new((1..=200).map(|_| "x".repeat(150)).collect()));
        let far: Vec<Location> = [10, 40, 70, 100, 130]
            .iter()
            .map(|&l| loc("a.go", l))
            .collect();
        let far_infos: Vec<HitInfo> = (0..5).map(|_| hit(Some(("Long", 1, 190)), false)).collect();
        let ctx = with_context(CONTEXT_MAX_HITS, || {
            context_blocks(&far, &far_infos, &[0, 1, 2, 3, 4], &wide)
        });
        // 9 lines × ~126 B per window: the 5th window is over the cap.
        assert_eq!(ctx.blocks.len(), 4, "byte cap");
        assert!(!ctx.covered.contains(&4));
    }

    #[test]
    fn context_is_off_by_default_but_summary_and_completion_stay() {
        // Batch 8: inline context saved no Reads in the C runs; default off.
        let (text, json) = run(h1_like(Confidence::Exact, None), 2_500);
        assert!(!text.contains("> line"), "{text}");
        assert!(!text.contains("| line"), "{text}");
        assert_eq!(json["context_blocks"], 0);
        // Hits keep their one-line text and enclosing function.
        assert!(text.contains("in Short[10-14]: callee(x)"), "{text}");
        assert!(text.contains("in Long[50-150]: callee(x)"), "{text}");
        assert!(
            text.contains("非测试调用方（2 处 / 2 个函数；测试 3 处）"),
            "{text}"
        );
        assert!(text.trim_end().ends_with("无需再 grep 复核。"), "{text}");
        assert_eq!(json["complete"], true);
    }

    #[test]
    fn context_switch_values() {
        assert_eq!(parse_context_hits(None), 0);
        assert_eq!(parse_context_hits(Some("")), 0);
        for v in ["0", "off", "OFF", "false", "no", "garbage"] {
            assert_eq!(parse_context_hits(Some(v)), 0, "{v}");
        }
        for v in ["on", "1", "true", " yes "] {
            assert_eq!(parse_context_hits(Some(v)), CONTEXT_MAX_HITS, "{v}");
        }
        assert_eq!(parse_context_hits(Some("3")), 3);
        assert_eq!(parse_context_hits(Some("12")), 12);
    }
}
