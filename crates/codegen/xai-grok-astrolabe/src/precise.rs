//! Host-language-server results, rendered under the model contract.
//!
//! The engine answers from the index whenever it can. When it cannot, the
//! tool layer asks the host's LSP manager and hands the locations or
//! diagnostics here: paths become model-visible (`C`-relative or `D`-absolute),
//! coordinates become 1-based, denied files are dropped, and the reply keeps
//! the same header/pagination shape as the index tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::paths::{DenyPolicy, PathMap, normalize, relative_posix};
use crate::render::finalize;
use crate::types::{Anchor, EngineMeta, EngineOutput, LspDiagnostic, LspLocation, LspSeverity};

/// Above this many references, the first page is a per-file summary.
const WIDE_REFERENCES: usize = 150;
/// Characters of source shown per hit.
const MAX_LINE_CHARS: usize = 180;
/// Byte reserve for the trailing status line inside a budgeted page.
const STATUS_RESERVE: usize = 160;

/// UTF-16 column of `name` on the 0-based `line`, or the first identifier's
/// column when the name is not on that line.
pub fn snap_character(path: &Path, line: u32, name: Option<&str>) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.lines().nth(line as usize)?;
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(byte) = text.find(name) {
            return Some(text[..byte].encode_utf16().count() as u32);
        }
    }
    let byte = text
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric() || *c == '_')
        .map(|(i, _)| i)?;
    Some(text[..byte].encode_utf16().count() as u32)
}

/// `find_references` from host-server locations.
pub fn render_references(
    paths: &PathMap,
    deny: &DenyPolicy,
    symbol: &str,
    anchor: Option<&Anchor>,
    locations: &[LspLocation],
    offset: usize,
    dry_run: Option<&str>,
    budget_bytes: usize,
) -> EngineOutput {
    let mut sources = Sources::new(paths.root());
    let mut hidden = 0usize;
    let mut rows: Vec<Row> = Vec::new();
    for location in locations {
        let Some((rel, shown)) = locate(paths, &location.path) else {
            continue;
        };
        if deny.denies(&rel) {
            hidden += 1;
            continue;
        }
        let line1 = location.line + 1;
        let declaration = anchor.is_some_and(|a| a.engine_path == rel && a.line == location.line);
        let text = sources.line(&rel, location.line);
        rows.push(Row {
            rel,
            shown,
            line: line1,
            character: location.character + 1,
            declaration,
            test: is_test_path(&location.path),
            text,
        });
    }
    // Production hits first; server order is kept inside each group.
    rows.sort_by_key(|row| row.test);
    let dedup = deduplicated(&rows);
    let rows = dedup;

    let total = rows.len();
    if total == 0 {
        let mut text = format!("confidence: exact (lsp)\n未找到对 `{symbol}` 的引用。\n");
        text.push_str("The language server knows this file but reports no references; the name may be unused or generated.\n");
        return outcome(
            finalize(text, hidden, false, budget_bytes),
            EngineMeta {
                confidence: Some("exact".to_string()),
                total: Some(0),
                ..EngineMeta::default()
            },
        );
    }

    let files = unique_files(&rows);
    if let Some(new_name) = dry_run {
        let text = render_rename(paths, symbol, new_name, &rows, offset, budget_bytes);
        return outcome(
            finalize(text, hidden, false, budget_bytes),
            EngineMeta {
                confidence: Some("exact".to_string()),
                total: Some(total as u64),
                ..EngineMeta::default()
            },
        );
    }

    let mut header = String::from("confidence: exact (lsp)\n");
    header.push_str(&format!("{total} references / {} files\n", files.len()));

    if total > WIDE_REFERENCES && offset == 0 {
        header.push_str(&format!(
            "wide result: per-file counts first (showing {} of {total}); narrow with `path` or `line` for the full list.\n",
            WIDE_REFERENCES
        ));
        let mut counts: Vec<(String, usize)> = Vec::new();
        for row in &rows {
            match counts.iter_mut().find(|(path, _)| *path == row.shown) {
                Some((_, count)) => *count += 1,
                None => counts.push((row.shown.clone(), 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (path, count) in &counts {
            header.push_str(&format!("{path}  {count}\n"));
        }
        header.push_str(&format!(
            "\nfirst {} of {total}:\n",
            WIDE_REFERENCES.min(total)
        ));
        for row in rows.iter().take(12) {
            header.push_str(&row.render(true));
        }
        return outcome(
            finalize(header, hidden, false, budget_bytes),
            EngineMeta {
                confidence: Some("exact".to_string()),
                total: Some(total as u64),
                shown: Some(12),
                omitted: Some(total as u64 - 12),
                offset: Some(0),
                truncated: true,
                ..EngineMeta::default()
            },
        );
    }

    let start = offset.min(total);
    let mut text = header;
    let mut kept = 0usize;
    let room = budget_bytes.saturating_sub(text.len() + STATUS_RESERVE);
    let mut used = 0usize;
    for row in rows.iter().skip(start) {
        let line = row.render(true);
        if kept > 0 && used + line.len() > room.max(1) {
            break;
        }
        used += line.len();
        text.push_str(&line);
        kept += 1;
    }
    let next = start + kept;
    if next < total {
        text.push_str(&format!(
            "showing {kept} of {total} · offset={start} · next_offset={next}\n"
        ));
    } else {
        text.push_str(&format!("showing {kept} of {total} · offset={start}\n"));
    }
    outcome(
        finalize(text, hidden, false, budget_bytes),
        EngineMeta {
            confidence: Some("exact".to_string()),
            total: Some(total as u64),
            shown: Some(kept as u64),
            omitted: Some((total - next) as u64),
            offset: Some(start as u64),
            next_offset: (next < total).then_some(next as u64),
            ..EngineMeta::default()
        },
    )
}

/// `goto_definition` from host-server locations.
pub fn render_definitions(
    paths: &PathMap,
    deny: &DenyPolicy,
    motif: Option<&str>,
    locations: &[LspLocation],
    budget_bytes: usize,
) -> EngineOutput {
    let mut sources = Sources::new(paths.root());
    let mut hidden = 0usize;
    let mut rows: Vec<Row> = Vec::new();
    for location in locations {
        let Some((rel, shown)) = locate(paths, &location.path) else {
            continue;
        };
        if deny.denies(&rel) {
            hidden += 1;
            continue;
        }
        rows.push(Row {
            rel: rel.clone(),
            shown,
            line: location.line + 1,
            character: location.character + 1,
            declaration: true,
            test: is_test_path(&location.path),
            text: sources.line(&rel, location.line),
        });
    }
    let rows = deduplicated(&rows);
    let mut text = String::from("confidence: exact (lsp)\n");
    if rows.is_empty() {
        text.push_str("未找到定义。\n");
        text.push_str(
            "The language server resolved the file but returned no definition; \
             try find_symbol or search_code for a same-name declaration.\n",
        );
        return outcome(
            finalize(text, hidden, false, budget_bytes),
            EngineMeta {
                confidence: Some("exact".to_string()),
                total: Some(0),
                ..EngineMeta::default()
            },
        );
    }
    if let Some(motif) = motif.filter(|m| !m.trim().is_empty()) {
        text.push_str(&format!("definition of `{motif}`\n"));
    }
    if rows.len() == 1 {
        text.push_str(&format!("definition: {}", rows[0].render(true)));
    } else {
        text.push_str(&format!("{} definitions\n", rows.len()));
        for (index, row) in rows.iter().enumerate() {
            text.push_str(&format!("{}. {}", index + 1, row.render(false)));
        }
    }
    outcome(
        finalize(text, hidden, false, budget_bytes),
        EngineMeta {
            confidence: Some("exact".to_string()),
            total: Some(rows.len() as u64),
            shown: Some(rows.len() as u64),
            ..EngineMeta::default()
        },
    )
}

/// Ambiguous name from the index: list the candidate declarations.
pub fn render_candidates(
    paths: &PathMap,
    deny: &DenyPolicy,
    symbol: Option<&str>,
    candidates: &[Anchor],
) -> EngineOutput {
    let mut text = String::from("confidence: syntactic (index)\n");
    let name = symbol
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("symbol");
    text.push_str(&format!(
        "{} candidate definitions for `{name}` (add path+line to disambiguate):\n",
        candidates.len()
    ));
    let mut hidden = 0usize;
    for candidate in candidates {
        if deny.denies(&candidate.engine_path) {
            hidden += 1;
            continue;
        }
        text.push_str(&format!(
            "{}:{}\n",
            paths.display_path(&candidate.engine_path),
            candidate.line + 1
        ));
    }
    outcome(
        finalize(text, hidden, false, 0),
        EngineMeta {
            confidence: Some("syntactic".to_string()),
            total: Some(candidates.len() as u64),
            shown: Some(candidates.len() as u64),
            ..EngineMeta::default()
        },
    )
}

/// `get_diagnostics` from host-server diagnostics. `minimum` is the model's
/// `severity` filter (`error` …).
pub fn render_diagnostics(
    paths: &PathMap,
    deny: &DenyPolicy,
    path_input: &str,
    diagnostics: &[LspDiagnostic],
    minimum: Option<&str>,
    budget_bytes: usize,
) -> EngineOutput {
    let min_rank = severity_rank(minimum).unwrap_or(LspSeverity::Hint);
    let mut hidden = 0usize;
    let display = match paths.engine_rel(path_input) {
        Ok(rel) => {
            if deny.denies(&rel) {
                hidden += 1;
                String::new()
            } else {
                paths.display_path(&rel)
            }
        }
        Err(_) => path_input.to_string(),
    };
    if hidden > 0 {
        return outcome(
            finalize(
                "excluded by the workspace read policy (DenyReadGlobs)".to_string(),
                hidden,
                true,
                budget_bytes,
            ),
            EngineMeta {
                confidence: Some("exact".to_string()),
                is_error: true,
                ..EngineMeta::default()
            },
        );
    }
    let mut counts = [0usize; 3];
    let mut lines: Vec<String> = Vec::new();
    for diagnostic in diagnostics {
        if severity_rank(Some(diagnostic.severity.as_str())).unwrap_or(LspSeverity::Hint) > min_rank
        {
            continue;
        }
        match diagnostic.severity {
            LspSeverity::Error => counts[0] += 1,
            LspSeverity::Warning => counts[1] += 1,
            LspSeverity::Information | LspSeverity::Hint => counts[2] += 1,
        }
        let mut line = format!(
            "{display}:{} {}",
            diagnostic.line,
            diagnostic.severity.as_str()
        );
        if let Some(code) = diagnostic.code.as_deref().filter(|c| !c.is_empty()) {
            line.push_str(&format!("[{code}]"));
        }
        if let Some(source) = diagnostic.source.as_deref().filter(|s| !s.is_empty()) {
            line.push_str(&format!(" ({source})"));
        }
        line.push_str(": ");
        line.push_str(diagnostic.message.trim());
        lines.push(clip(&line));
    }
    let mut text = format!(
        "{} error / {} warning / {} info · source: lsp\n",
        counts[0], counts[1], counts[2]
    );
    let mut shown = 0usize;
    if lines.is_empty() {
        text.push_str("No diagnostics at or above the requested severity.\n");
    } else {
        let room = budget_bytes.saturating_sub(text.len() + STATUS_RESERVE);
        let mut used = 0usize;
        for line in &lines {
            if shown > 0 && used + line.len() > room.max(1) {
                break;
            }
            used += line.len();
            text.push_str(line);
            text.push('\n');
            shown += 1;
        }
        if shown < lines.len() {
            text.push_str(&format!(
                "showing {shown} of {} diagnostics.\n",
                lines.len()
            ));
        }
    }
    outcome(
        finalize(text, hidden, false, budget_bytes),
        EngineMeta {
            confidence: Some("exact".to_string()),
            total: Some(lines.len() as u64),
            shown: Some(shown as u64),
            omitted: Some((lines.len() - shown) as u64),
            ..EngineMeta::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct Row {
    rel: String,
    shown: String,
    line: u32,
    #[allow(dead_code)]
    character: u32,
    declaration: bool,
    test: bool,
    text: Option<String>,
}

impl Row {
    fn render(&self, with_text: bool) -> String {
        let mut out = format!("{}:{}", self.shown, self.line);
        if self.declaration {
            out.push_str(" [declaration]");
        }
        if with_text {
            if let Some(text) = self.text.as_deref() {
                out.push_str("  ");
                out.push_str(text);
            }
        }
        out.push('\n');
        out
    }
}

struct Sources {
    root: PathBuf,
    cache: HashMap<String, Vec<String>>,
}

impl Sources {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cache: HashMap::new(),
        }
    }

    fn line(&mut self, rel: &str, line0: u32) -> Option<String> {
        let lines = self.cache.entry(rel.to_string()).or_insert_with(|| {
            std::fs::read_to_string(self.root.join(rel))
                .map(|text| text.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
        lines.get(line0 as usize).map(|line| clip(line.trim()))
    }
}

fn outcome(text: String, meta: EngineMeta) -> EngineOutput {
    EngineOutput { text, meta }
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_LINE_CHARS).collect();
    out.push('…');
    out
}

fn locate(paths: &PathMap, path: &Path) -> Option<(String, String)> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        paths.root().join(path)
    };
    let abs = normalize(&abs);
    let rel = relative_posix(paths.root(), &abs)?;
    if rel == ".." || rel.starts_with("../") {
        return None;
    }
    let shown = paths.display_path(&rel);
    Some((rel, shown))
}

fn unique_files(rows: &[Row]) -> Vec<&str> {
    let mut files: Vec<&str> = Vec::new();
    for row in rows {
        if !files.contains(&row.shown.as_str()) {
            files.push(&row.shown);
        }
    }
    files
}

fn deduplicated(rows: &[Row]) -> Vec<Row> {
    // `Row` has no Clone on purpose; rebuild from references.
    let mut out: Vec<&Row> = Vec::new();
    for row in rows {
        if out
            .iter()
            .any(|seen| seen.rel == row.rel && seen.line == row.line)
        {
            continue;
        }
        out.push(row);
    }
    out.into_iter()
        .map(|row| Row {
            rel: row.rel.clone(),
            shown: row.shown.clone(),
            line: row.line,
            character: row.character,
            declaration: row.declaration,
            test: row.test,
            text: row.text.clone(),
        })
        .collect()
}

fn is_test_path(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.to_lowercase();
    text.contains("/tests/")
        || text.contains("/test/")
        || text.contains("/__tests__/")
        || text.contains("/spec/")
        || text.contains("_test.")
        || text.contains(".test.")
        || text.contains(".spec.")
}

fn severity_rank(raw: Option<&str>) -> Option<LspSeverity> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "error" => Some(LspSeverity::Error),
        "warning" | "warn" => Some(LspSeverity::Warning),
        "information" | "info" => Some(LspSeverity::Information),
        "hint" => Some(LspSeverity::Hint),
        _ => None,
    }
}

fn render_rename(
    paths: &PathMap,
    symbol: &str,
    new_name: &str,
    rows: &[Row],
    offset: usize,
    budget_bytes: usize,
) -> String {
    let mut files: Vec<(String, Vec<u32>)> = Vec::new();
    for row in rows {
        match files.iter_mut().find(|(path, _)| *path == row.shown) {
            Some((_, lines)) => lines.push(row.line),
            None => files.push((row.shown.clone(), vec![row.line])),
        }
    }
    let mut text = format!(
        "rename preview: `{symbol}` -> `{new_name}` (no files written)\nconfidence: exact (lsp)\n{} files / {} references\n",
        files.len(),
        rows.len()
    );
    let start = offset.min(files.len());
    let room = budget_bytes.saturating_sub(text.len() + STATUS_RESERVE);
    let mut used = 0usize;
    let mut kept = 0usize;
    for (path, lines) in files.iter().skip(start) {
        let line = format!(
            "{path}  {} reference(s): {}\n",
            lines.len(),
            lines
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        if kept > 0 && used + line.len() > room.max(1) {
            break;
        }
        used += line.len();
        text.push_str(&line);
        kept += 1;
    }
    let next = start + kept;
    if next < files.len() {
        text.push_str(&format!(
            "showing files {}-{} of {} · next_offset={next}\n",
            start + 1,
            next,
            files.len()
        ));
    }
    let _ = paths;
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DenyPolicy;

    fn map(root: &Path) -> PathMap {
        PathMap::new(root, root, None)
    }

    #[test]
    fn references_are_one_based_and_marked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "fn foo() {}\nfn bar() { foo(); }\n",
        )
        .unwrap();
        let paths = map(root);
        let deny = DenyPolicy::default();
        let anchor = Anchor {
            engine_path: "src/lib.rs".to_string(),
            absolute: root.join("src/lib.rs"),
            line: 0,
            character: 3,
        };
        let locations = vec![
            LspLocation {
                path: root.join("src/lib.rs"),
                line: 0,
                character: 3,
                end_line: 0,
                end_character: 6,
            },
            LspLocation {
                path: root.join("src/lib.rs"),
                line: 1,
                character: 11,
                end_line: 1,
                end_character: 14,
            },
        ];
        let out = render_references(
            &paths,
            &deny,
            "foo",
            Some(&anchor),
            &locations,
            0,
            None,
            40_000,
        );
        assert!(out.text.contains("2 references / 1 files"), "{}", out.text);
        assert!(
            out.text.contains("src/lib.rs:1 [declaration]"),
            "{}",
            out.text
        );
        assert!(out.text.contains("src/lib.rs:2"), "{}", out.text);
        assert_eq!(out.meta.total, Some(2));
        assert_eq!(out.meta.confidence.as_deref(), Some("exact"));
    }

    #[test]
    fn denied_reference_lines_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("secret")).unwrap();
        std::fs::write(root.join("secret/key.rs"), "fn foo() {}\n").unwrap();
        std::fs::write(root.join("ok.rs"), "fn foo() {}\n").unwrap();
        let paths = map(root);
        let deny = DenyPolicy::new(&["secret".to_string()]);
        let locations = vec![
            LspLocation {
                path: root.join("secret/key.rs"),
                line: 0,
                character: 3,
                end_line: 0,
                end_character: 6,
            },
            LspLocation {
                path: root.join("ok.rs"),
                line: 0,
                character: 3,
                end_line: 0,
                end_character: 6,
            },
        ];
        let out = render_references(&paths, &deny, "foo", None, &locations, 0, None, 40_000);
        assert!(!out.text.contains("secret/key.rs"), "{}", out.text);
        assert!(out.text.contains("hidden by the workspace read policy"));
        assert!(out.text.contains("ok.rs:1"));
    }

    #[test]
    fn rename_preview_is_read_only_and_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "fn foo() {}\nfoo();\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn foo() {}\n").unwrap();
        let paths = map(root);
        let deny = DenyPolicy::default();
        let locations = vec![
            LspLocation {
                path: root.join("a.rs"),
                line: 0,
                character: 3,
                end_line: 0,
                end_character: 6,
            },
            LspLocation {
                path: root.join("a.rs"),
                line: 1,
                character: 0,
                end_line: 1,
                end_character: 3,
            },
            LspLocation {
                path: root.join("b.rs"),
                line: 0,
                character: 3,
                end_line: 0,
                end_character: 6,
            },
        ];
        let out = render_references(
            &paths,
            &deny,
            "foo",
            None,
            &locations,
            0,
            Some("bar"),
            40_000,
        );
        assert!(out.text.contains("no files written"), "{}", out.text);
        assert!(
            out.text.contains("a.rs  2 reference(s): 1, 2"),
            "{}",
            out.text
        );
        assert!(out.text.contains("b.rs  1 reference(s): 1"), "{}", out.text);
    }

    #[test]
    fn diagnostics_honor_the_severity_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let paths = map(root);
        let deny = DenyPolicy::default();
        let diagnostics = vec![
            LspDiagnostic {
                line: 4,
                character: 1,
                severity: LspSeverity::Error,
                message: "mismatched types".to_string(),
                source: None,
                code: Some("E0308".to_string()),
            },
            LspDiagnostic {
                line: 9,
                character: 2,
                severity: LspSeverity::Warning,
                message: "unused".to_string(),
                source: None,
                code: None,
            },
        ];
        let out = render_diagnostics(&paths, &deny, "a.rs", &diagnostics, Some("error"), 40_000);
        assert_eq!(out.meta.total, Some(1));
        assert!(
            out.text.contains("1 error / 0 warning / 0 info"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("a.rs:4 error[E0308]: mismatched types"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("a.rs:9"), "{}", out.text);
    }

    #[test]
    fn candidate_list_is_syntactic_and_one_based() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let paths = map(root);
        let deny = DenyPolicy::default();
        let candidates = vec![
            Anchor {
                engine_path: "src/a.rs".to_string(),
                absolute: root.join("src/a.rs"),
                line: 11,
                character: 0,
            },
            Anchor {
                engine_path: "src/b.rs".to_string(),
                absolute: root.join("src/b.rs"),
                line: 3,
                character: 0,
            },
        ];
        let out = render_candidates(&paths, &deny, Some("foo"), &candidates);
        assert!(out.text.contains("2 candidate definitions"), "{}", out.text);
        assert!(out.text.contains("src/a.rs:12"), "{}", out.text);
        assert_eq!(out.meta.confidence.as_deref(), Some("syntactic"));
    }
}
