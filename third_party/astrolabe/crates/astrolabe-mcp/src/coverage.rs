//! Index-coverage reporting for `get_languages` and the per-query note.
//!
//! Two different things are kept apart:
//! * **hard gaps** — supported-language files that are NOT in the index (over
//!   the size cap, unreadable, parser timeout / no tree);
//! * **partial parses** — files that ARE in the index but have syntax/compile
//!   errors. Two kinds: C-family errors attributable to macros / preprocessor
//!   directives (symbols mostly intact: tree-sitter's recovery keeps the
//!   names), and other syntax errors (symbols in the broken region may be
//!   missing). See `astrolabe_core::parse::diagnose_indexed_syntax`.
//!
//! Unsupported files are only ever summarised by extension. When more than
//! [`INLINE_LIMIT`] files have problems the details go to a Markdown report
//! next to the index cache (never inside the tracked tree), and the tool
//! output only gives the count and the absolute path.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use astrolabe_core::parse::{diagnose_indexed_syntax, SyntaxDiagnosis, SyntaxErrorKind};
use astrolabe_core::{Language, RelPath};
use rayon::prelude::*;

use crate::index::RepoIndex;

/// At most this many problem files are listed inline.
pub(crate) const INLINE_LIMIT: usize = 10;
/// Unsupported file types shown by name before folding into "other".
pub(crate) const TOP_TYPES: usize = 8;
/// Entries per section in the Markdown report.
const REPORT_MAX_ENTRIES: usize = 1000;
/// Hard byte cap of the Markdown report.
const REPORT_MAX_BYTES: usize = 192 * 1024;
const REPORT_FILE: &str = "coverage-report.md";

/// Partial-parse files diagnosed (re-parsed) per snapshot; beyond this the
/// rest are counted as unclassified so a huge broken tree stays cheap.
const DIAG_MAX: usize = 2000;

/// Label of an ordinary syntax error.
pub(crate) const PARTIAL_LABEL: &str =
    "has syntax errors, index may be incomplete (symbols may be missing)";
/// Label of a C-family error attributable to macros / preprocessor.
pub(crate) const PREPROC_LABEL: &str = "macro/preprocessor syntax errors, symbols mostly intact";

/// One file that is in the index but parsed with errors.
#[derive(Clone, Debug)]
pub(crate) struct PartialEntry {
    pub path: RelPath,
    pub language: Option<Language>,
    /// `None` when not diagnosed (over [`DIAG_MAX`], unreadable, clean raw
    /// re-parse).
    pub diag: Option<SyntaxDiagnosis>,
}

impl PartialEntry {
    pub(crate) fn kind(&self) -> SyntaxErrorKind {
        self.diag
            .as_ref()
            .map_or(SyntaxErrorKind::Syntax, |d| d.kind)
    }

    fn label(&self) -> &'static str {
        match self.kind() {
            SyntaxErrorKind::Preprocessor => PREPROC_LABEL,
            SyntaxErrorKind::Syntax => PARTIAL_LABEL,
        }
    }
}

/// Body text of a zero-hit result (find_symbol, search_code,
/// resolve_context): the coverage note is always relevant there.
const EMPTY_RESULT_MARKERS: &[&str] = &[
    "(未找到匹配的符号)",
    "(未找到匹配的代码行)",
    "没有匹配结果",
    "未匹配到与任务相关的符号或文件",
];
/// Error files named by the per-result note before it switches to a count.
const CITED_NAMES_MAX: usize = 3;

/// Short reason bucket for a hard gap.
fn gap_kind(reason: &str) -> &'static str {
    if reason.starts_with("too large") {
        "too large (size cap)"
    } else if reason.starts_with("read:") {
        "read failure"
    } else if reason.contains("budget") || reason.contains("tree") {
        "parser timeout"
    } else {
        "parse error"
    }
}

impl RepoIndex {
    /// Supported-language files that are not in the index. Sorted, unique.
    pub(crate) fn hard_gaps(&self) -> Vec<(RelPath, String)> {
        let report = self.report();
        let mut gaps: Vec<(RelPath, String)> = report
            .parse_failures
            .iter()
            .cloned()
            .chain(
                report
                    .too_large
                    .iter()
                    .map(|path| (path.clone(), "too large (size cap)".to_string())),
            )
            .collect();
        gaps.sort();
        gaps.dedup_by(|a, b| a.0 == b.0);
        gaps
    }

    /// Files in the index that have syntax errors. Sorted.
    pub(crate) fn partial_files(&self) -> Vec<RelPath> {
        let mut v: Vec<RelPath> = self
            .report()
            .partial_parses
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Very short note appended to every query result; no paths. `None` when
    /// coverage is complete.
    pub(crate) fn coverage_note(&self) -> Option<String> {
        let hard = self.hard_gaps().len();
        let partial = self.report().partial_parses.len();
        if hard == 0 && partial == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if hard > 0 {
            parts.push(format!("{hard} 个文件未进入索引"));
        }
        if partial > 0 {
            parts.push(format!("{partial} 个文件有语法错误"));
        }
        Some(format!(
            "注意：{}，结果可能遗漏；详见 get_languages。",
            parts.join("，")
        ))
    }

    /// Partial-parse files that `text` cites as a location (`@path:line`,
    /// `path:line: …`, or a bare path token). Sorted, unique.
    pub(crate) fn partial_files_cited(&self, text: &str) -> Vec<String> {
        let partial = &self.report().partial_parses;
        if partial.is_empty() {
            return Vec::new();
        }
        let set: std::collections::HashSet<&str> =
            partial.iter().map(|(p, _)| p.as_str()).collect();
        let mut out = std::collections::BTreeSet::new();
        let separators = |c: char| {
            c.is_whitespace() || matches!(c, '`' | '(' | ')' | '[' | ']' | ',' | '，' | '、')
        };
        for token in text.split(separators) {
            let token = token.trim_start_matches('@');
            let path = token.split_once(':').map_or(token, |(p, _)| p);
            if !path.is_empty() && set.contains(path) {
                out.insert(path.to_string());
            }
        }
        out.into_iter().collect()
    }

    /// Coverage footnote for one query result, or `None` when it would only
    /// repeat what this session was already told. The full count note is
    /// shown the first time (and again whenever it changes) and on empty
    /// results, where a gap may be the reason nothing matched; a result that
    /// cites a file with syntax errors names that file.
    pub(crate) fn query_coverage_note(
        &self,
        text: &str,
        shown: &std::sync::Mutex<Option<String>>,
    ) -> Option<String> {
        let note = self.coverage_note()?;
        let fresh = {
            let mut last = shown.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = last.as_deref() != Some(note.as_str());
            if fresh {
                *last = Some(note.clone());
            }
            fresh
        };
        let empty = EMPTY_RESULT_MARKERS.iter().any(|m| text.contains(m));
        let cited = self.partial_files_cited(text);
        let mut lines = Vec::new();
        if fresh || empty {
            lines.push(note);
        }
        if !cited.is_empty() {
            let names: Vec<&str> = cited
                .iter()
                .take(CITED_NAMES_MAX)
                .map(String::as_str)
                .collect();
            let more = cited.len().saturating_sub(CITED_NAMES_MAX);
            let tail = if more > 0 {
                format!(" 等 {} 个", cited.len())
            } else {
                String::new()
            };
            lines.push(format!(
                "注意：命中文件 {}{tail} 有语法错误，其符号可能不全；详见 get_languages。",
                names.join("、")
            ));
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// Diagnosis of every partial-parse file (first error, kind), computed
    /// once per snapshot in parallel. Sorted by path.
    ///
    /// Each diagnosis is a full re-parse, so results are kept in a small
    /// sidecar next to the index cache, keyed by path + content sha: a warm
    /// restart (or a watcher apply) only re-parses files whose bytes changed.
    pub(crate) fn partial_entries(&self) -> &[PartialEntry] {
        self.partial_diagnoses_cell().get_or_init(|| {
            let files: std::collections::HashMap<&RelPath, (Option<Language>, &str)> = self
                .graph()
                .files
                .iter()
                .map(|f| (&f.path, (f.language, f.sha.as_str())))
                .collect();
            // C / C++ files whose indexed parse is the macro rewrite are
            // diagnosed on that rewrite, not on errors it already fixed.
            let rewritten: std::collections::HashSet<&RelPath> = self
                .report()
                .partial_parses
                .iter()
                .filter(|(_, why)| why == astrolabe_core::PARTIAL_PARSE_REWRITE_REASON)
                .map(|(p, _)| p)
                .collect();
            let cache = DiagCache::load(&self.root);
            let paths = self.partial_files();
            let results: Vec<(PartialEntry, Option<CachedDiag>, bool)> = paths
                .par_iter()
                .enumerate()
                .map(|(i, path)| {
                    let (language, sha) = files.get(path).copied().unwrap_or((None, ""));
                    let is_rewritten = rewritten.contains(path);
                    let entry = |diag| PartialEntry {
                        path: path.clone(),
                        language,
                        diag,
                    };
                    let Some(lang) = language.filter(|_| i < DIAG_MAX) else {
                        return (entry(None), None, false);
                    };
                    if let Some(hit) = cache.get(path.as_str(), sha, is_rewritten) {
                        return (entry(hit.diagnosis()), Some(hit.clone()), false);
                    }
                    let Ok(source) =
                        astrolabe_core::text::read_source(&self.root.join(path.as_str()))
                    else {
                        return (entry(None), None, false);
                    };
                    let diag = diagnose_indexed_syntax(lang, &source, is_rewritten);
                    let stored = (!sha.is_empty())
                        .then(|| CachedDiag::new(sha, is_rewritten, diag.as_ref()));
                    (entry(diag), stored, true)
                })
                .collect();
            if results.iter().any(|(_, _, fresh)| *fresh) {
                let keep = results
                    .iter()
                    .filter_map(|(e, c, _)| Some((e.path.as_str().to_owned(), c.clone()?)))
                    .collect();
                DiagCache::store(&self.root, keep);
            }
            results.into_iter().map(|(e, _, _)| e).collect()
        })
    }

    /// `(macro/preprocessor, other)` counts of partial-parse files.
    fn partial_kind_counts(&self) -> (usize, usize) {
        let entries = self.partial_entries();
        let pre = entries
            .iter()
            .filter(|e| e.kind() == SyntaxErrorKind::Preprocessor)
            .count();
        (pre, entries.len() - pre)
    }

    /// The coverage block of `get_languages`.
    pub(crate) fn coverage_detail(&self) -> String {
        let hard = self.hard_gaps();
        let partial = self.partial_files();
        if hard.is_empty() && partial.is_empty() {
            return "index coverage: complete (every supported-language file is indexed, none has syntax errors)\n".to_string();
        }
        let mut out = String::new();
        let need_report = hard.len() > INLINE_LIMIT || partial.len() > INLINE_LIMIT;
        if !hard.is_empty() {
            let mut kinds: std::collections::BTreeMap<&str, usize> = Default::default();
            for (_, why) in &hard {
                *kinds.entry(gap_kind(why)).or_default() += 1;
            }
            let kinds = kinds
                .iter()
                .map(|(k, n)| format!("{k}: {n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "files NOT in the index: {} ({kinds})", hard.len());
            if hard.len() <= INLINE_LIMIT {
                for (path, why) in &hard {
                    let _ = writeln!(out, "  {path}: {why}");
                }
            }
        }
        if !partial.is_empty() {
            let (pre, other) = self.partial_kind_counts();
            let _ = writeln!(
                out,
                "files with syntax/compile errors: {} (indexed) — other syntax errors: {other} (symbols may be missing), macro/preprocessor: {pre} (symbols mostly intact)",
                partial.len()
            );
            if partial.len() <= INLINE_LIMIT {
                for entry in self.partial_entries() {
                    let label = entry.label();
                    match &entry.diag {
                        Some(d) => {
                            let _ = writeln!(
                                out,
                                "  {}: {label}; line {}: {}",
                                entry.path, d.line, d.what
                            );
                        }
                        None => {
                            let _ = writeln!(out, "  {}: {label}", entry.path);
                        }
                    }
                }
            }
        }
        if need_report {
            match self.coverage_report_path() {
                Some(path) => {
                    let _ = writeln!(
                        out,
                        "details for all affected files (path, language, kind, first error): Read {}",
                        path.display()
                    );
                }
                None => {
                    out.push_str("details report could not be written (cache dir not writable); use find_symbol/search_code on suspect files\n");
                }
            }
        }
        out
    }

    /// Absolute path of the Markdown report, written once per index snapshot.
    /// `None` when no writable location exists.
    pub(crate) fn coverage_report_path(&self) -> Option<PathBuf> {
        self.coverage_report_cell()
            .get_or_init(|| {
                let body = self.render_report();
                write_report(&self.root, &body)
            })
            .clone()
    }

    pub(crate) fn render_report(&self) -> String {
        let hard = self.hard_gaps();
        let partial = self.partial_files();
        let mut md = String::new();
        md.push_str("# Astrolabe index coverage report\n\n");
        let _ = writeln!(md, "- repository: `{}`", self.root.display());
        let _ = writeln!(md, "- files not in the index: {}", hard.len());
        let _ = writeln!(md, "- files with syntax/compile errors: {}", partial.len());
        if !partial.is_empty() {
            let (pre, other) = self.partial_kind_counts();
            let _ = writeln!(md, "  - other syntax errors: {other} ({PARTIAL_LABEL})");
            let _ = writeln!(md, "  - macro/preprocessor: {pre} ({PREPROC_LABEL})");
        }
        md.push('\n');
        md.push_str(
            "Files with syntax errors are still indexed. **other syntax errors**: symbols \
             inside the broken region may be missing; use `Read`/`search_code` on them when a \
             symbol seems absent. **macro/preprocessor** (C, C++, Objective-C): the grammar \
             cannot expand macros or pick `#if` branches without a build configuration, \
             but error recovery keeps the symbol names; symbol ranges or nesting near the \
             reported line may be off. Other syntax errors are listed first.\n\n",
        );
        let mut truncated = false;
        if !hard.is_empty() {
            md.push_str("## Not in the index\n\n| path | reason |\n|---|---|\n");
            for (path, why) in hard.iter().take(REPORT_MAX_ENTRIES) {
                if md.len() > REPORT_MAX_BYTES {
                    truncated = true;
                    break;
                }
                let _ = writeln!(
                    md,
                    "| {path} | {} |",
                    why.replace('|', "/").replace('\n', " ")
                );
            }
            if hard.len() > REPORT_MAX_ENTRIES {
                truncated = true;
            }
            md.push('\n');
        }
        if !partial.is_empty() {
            md.push_str(
                "## Syntax/compile errors (indexed)\n\n\
                 | path | language | kind | first error |\n|---|---|---|---|\n",
            );
            let mut entries: Vec<&PartialEntry> = self.partial_entries().iter().collect();
            // Other syntax errors first (stable: path order within a kind).
            entries.sort_by_key(|e| e.kind() == SyntaxErrorKind::Preprocessor);
            for entry in entries.iter().take(REPORT_MAX_ENTRIES) {
                if md.len() > REPORT_MAX_BYTES {
                    truncated = true;
                    break;
                }
                let lang = entry.language.map(|l| l.name()).unwrap_or("unknown");
                let kind = match entry.kind() {
                    SyntaxErrorKind::Preprocessor => "macro/preprocessor",
                    SyntaxErrorKind::Syntax => "syntax",
                };
                let err = match &entry.diag {
                    Some(d) => format!("line {}: {}", d.line, d.what.replace('|', "/")),
                    None => "syntax errors (location not determined)".to_string(),
                };
                let _ = writeln!(md, "| {} | {lang} | {kind} | {err} |", entry.path);
            }
            if partial.len() > REPORT_MAX_ENTRIES {
                truncated = true;
            }
            md.push('\n');
        }
        if truncated {
            md.push_str(
                "_Report truncated: only the first entries (other syntax errors first, then by path) are listed._\n",
            );
        }
        md
    }
}

/// Sidecar of syntax diagnoses next to the index cache.
const DIAG_CACHE_FILE: &str = "syntax-diagnoses.json";

/// One cached [`diagnose_indexed_syntax`] result.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct CachedDiag {
    sha: String,
    rewritten: bool,
    /// `None`: the re-parse was clean (no diagnosis).
    diag: Option<(u32, String, bool)>,
}

impl CachedDiag {
    fn new(sha: &str, rewritten: bool, diag: Option<&SyntaxDiagnosis>) -> Self {
        CachedDiag {
            sha: sha.to_owned(),
            rewritten,
            diag: diag.map(|d| {
                (
                    d.line,
                    d.what.clone(),
                    d.kind == SyntaxErrorKind::Preprocessor,
                )
            }),
        }
    }

    fn diagnosis(&self) -> Option<SyntaxDiagnosis> {
        self.diag
            .as_ref()
            .map(|(line, what, preprocessor)| SyntaxDiagnosis {
                line: *line,
                what: what.clone(),
                kind: if *preprocessor {
                    SyntaxErrorKind::Preprocessor
                } else {
                    SyntaxErrorKind::Syntax
                },
            })
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct DiagCache {
    /// Binary + diagnosis-format stamp ([`Self::format`]); any mismatch
    /// discards the whole file.
    format: String,
    entries: std::collections::BTreeMap<String, CachedDiag>,
}

impl DiagCache {
    /// Also stamped with the running executable's size and mtime, so any
    /// rebuild (a parser fix that forgot to bump a version) starts fresh.
    fn format() -> String {
        let exe = std::env::current_exe()
            .and_then(std::fs::metadata)
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{}-{mtime}", m.len())
            })
            .unwrap_or_default();
        format!(
            "v{}-x{}-d{}-e{exe}",
            env!("CARGO_PKG_VERSION"),
            astrolabe_core::store::EXTRACTOR_VERSION,
            astrolabe_core::parse::DIAG_FORMAT_VERSION
        )
    }

    fn path(root: &Path) -> PathBuf {
        root.join(".astrolabe").join(DIAG_CACHE_FILE)
    }

    /// The cache for this binary, or an empty one (missing, unreadable,
    /// corrupt or from another version).
    fn load(root: &Path) -> Self {
        std::fs::read(Self::path(root))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<DiagCache>(&bytes).ok())
            .filter(|cache| cache.format == Self::format())
            .unwrap_or_default()
    }

    fn get(&self, path: &str, sha: &str, rewritten: bool) -> Option<&CachedDiag> {
        if sha.is_empty() {
            return None;
        }
        self.entries
            .get(path)
            .filter(|c| c.sha == sha && c.rewritten == rewritten)
    }

    /// Replace the sidecar with `entries` (atomic rename). Best effort: only
    /// into an existing `.astrolabe` dir, never elsewhere.
    fn store(root: &Path, entries: std::collections::BTreeMap<String, CachedDiag>) {
        let dir = root.join(".astrolabe");
        if !dir.is_dir() {
            return;
        }
        // The index cache creates this too; never leave the sidecar visible
        // to git if the dir came from elsewhere.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(&ignore, "# Created by Astrolabe. Ignores itself.\n*\n");
        }
        let cache = DiagCache {
            format: Self::format(),
            entries,
        };
        let Ok(body) = serde_json::to_vec(&cache) else {
            return;
        };
        let dest = Self::path(root);
        let tmp = dir.join(format!("{DIAG_CACHE_FILE}.tmp{}", std::process::id()));
        if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &dest).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Write the report atomically (temp file + rename) into `<root>/.astrolabe/`
/// (the self-ignoring cache dir), falling back to the OS temp dir.
fn write_report(root: &Path, body: &str) -> Option<PathBuf> {
    let primary = root.join(".astrolabe");
    if let Some(path) = write_atomic(&primary, body) {
        return Some(path);
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    write_atomic(
        &std::env::temp_dir().join(format!("astrolabe-coverage-{h:016x}")),
        body,
    )
}

fn write_atomic(dir: &Path, body: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# Created by Astrolabe. Ignores itself.\n*\n");
    }
    let dest = dir.join(REPORT_FILE);
    let tmp = dir.join(format!("{REPORT_FILE}.tmp{}", std::process::id()));
    if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(dest.canonicalize().unwrap_or(dest))
}

/// Unsupported-file summary by type: top [`TOP_TYPES`] extensions, the rest
/// folded into one count. Size is bounded regardless of file count.
pub(crate) fn unsupported_by_type(counts: &std::collections::BTreeMap<String, u32>) -> String {
    let mut list: Vec<(&String, &u32)> = counts.iter().collect();
    list.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut items: Vec<String> = list
        .iter()
        .take(TOP_TYPES)
        .map(|(ext, n)| format!(".{ext}×{n}"))
        .collect();
    if list.len() > TOP_TYPES {
        let rest = &list[TOP_TYPES..];
        let files: u32 = rest.iter().map(|(_, n)| **n).sum();
        items.push(format!("other {} types×{files}", rest.len()));
    }
    items.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::graph::CodeGraph;
    use astrolabe_core::IndexReport;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-cov-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn broken(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(
                dir.join(format!("bad{i:02}.py")),
                format!("def keep{i}():\n    pass\n\ndef oops{i}(:\n"),
            )
            .unwrap();
        }
    }

    fn diag_summary(index: &RepoIndex) -> Vec<(String, Option<(u32, String)>)> {
        index
            .partial_entries()
            .iter()
            .map(|e| {
                (
                    e.path.as_str().to_owned(),
                    e.diag.as_ref().map(|d| (d.line, d.what.clone())),
                )
            })
            .collect()
    }

    #[test]
    fn query_note_names_cited_error_files_and_dedups_the_count() {
        let dir = temp_dir("cited");
        broken(&dir, 5);
        std::fs::write(dir.join("good.py"), "def fine():\n    pass\n").unwrap();
        let index = crate::index::build(&dir).unwrap();
        let shown = std::sync::Mutex::new(None);
        let full = index.coverage_note().unwrap();
        // First result of the session: the count note.
        let first = index
            .query_coverage_note("def fine()  @good.py:1", &shown)
            .unwrap();
        assert_eq!(first, full);
        // Same note again, no error file cited: nothing.
        assert!(index
            .query_coverage_note("def fine()  @good.py:1", &shown)
            .is_none());
        // `xbad00.py` / `bad00.pyc` are not `bad00.py`.
        assert!(index
            .query_coverage_note("@xbad00.py:1 bad00.pyc:3:", &shown)
            .is_none());
        let cited = index
            .query_coverage_note("@bad00.py:1\nbad01.py:2: x\n`bad02.py`, bad03.py:4", &shown)
            .unwrap();
        assert!(!cited.contains(&full), "{cited}");
        assert!(
            cited.contains("bad00.py、bad01.py、bad02.py 等 4 个"),
            "{cited}"
        );
        // Empty result: count note again.
        let empty = index
            .query_coverage_note("(未找到匹配的符号)", &shown)
            .unwrap();
        assert_eq!(empty, full);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Diagnoses are reused across snapshots/restarts only for unchanged
    /// bytes of the same binary; a fresh compute gives the same answer.
    #[test]
    fn syntax_diagnoses_are_cached_by_content_sha() {
        let dir = temp_dir("diagcache");
        broken(&dir, 3);
        let first = crate::index::build_with_options(&dir, true, 1 << 20).unwrap();
        let fresh = diag_summary(&first);
        assert_eq!(fresh.len(), 3);
        assert!(fresh.iter().all(|(_, d)| d.is_some()), "{fresh:?}");
        let path = DiagCache::path(&dir);
        let mut cache: DiagCache = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(cache.format, DiagCache::format());
        assert_eq!(cache.entries.len(), 3);
        // The sidecar sits in the self-ignoring cache dir, never in git status.
        let ignore = std::fs::read_to_string(path.parent().unwrap().join(".gitignore")).unwrap();
        assert!(ignore.lines().any(|l| l.trim() == "*"), "{ignore:?}");

        // Same bytes after a restart: the cached diagnosis is used as is.
        let again = crate::index::build_with_options(&dir, true, 1 << 20).unwrap();
        assert_eq!(diag_summary(&again), fresh);
        if let Some((_, what, _)) = &mut cache.entries.get_mut("bad00.py").unwrap().diag {
            *what = "FROM CACHE".into();
        }
        std::fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        let reused = crate::index::build_with_options(&dir, true, 1 << 20).unwrap();
        let got = diag_summary(&reused);
        assert_eq!(got[0].1.as_ref().unwrap().1, "FROM CACHE");
        assert_eq!(got[1..], fresh[1..]);

        // Changed bytes: re-diagnosed (the tampered entry for bad00 stays).
        std::fs::write(dir.join("bad01.py"), "x = 1\n\n\ndef late(:\n").unwrap();
        let edited = crate::index::build_with_options(&dir, true, 1 << 20).unwrap();
        let got = diag_summary(&edited);
        assert_eq!(got[0].1.as_ref().unwrap().1, "FROM CACHE");
        assert_eq!(got[1].1.as_ref().unwrap().0, 4, "{got:?}");

        // Another format stamp: the whole sidecar is ignored.
        let mut stale: DiagCache = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stale.format = "other-binary".into();
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let recomputed = crate::index::build_with_options(&dir, true, 1 << 20).unwrap();
        let got = diag_summary(&recomputed);
        assert_eq!(got[0], fresh[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_repo_reports_complete_and_no_note() {
        let dir = temp_dir("clean");
        std::fs::write(dir.join("a.py"), "def a():\n    pass\n").unwrap();
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        assert!(index.coverage_note().is_none());
        assert!(index.coverage_detail().contains("complete"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_parse_stays_indexed_and_is_listed_inline_up_to_ten() {
        let dir = temp_dir("inline");
        broken(&dir, 3);
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        // Not a hard gap, and its symbols are in the index.
        assert!(index.hard_gaps().is_empty());
        assert_eq!(index.partial_files().len(), 3);
        assert!(index.graph().symbols.iter().any(|s| s.name == "keep0"));
        let detail = index.coverage_detail();
        assert!(detail.contains("syntax/compile errors: 3"), "{detail}");
        assert!(detail.contains("bad00.py"), "{detail}");
        assert!(detail.contains(PARTIAL_LABEL), "{detail}");
        assert!(detail.contains("line 4"), "first error line: {detail}");
        assert!(
            !detail.contains("Read "),
            "no report below the limit: {detail}"
        );
        assert!(!detail.contains("NOT in the index"), "{detail}");
        let note = index.coverage_note().unwrap();
        assert!(note.contains("3 个文件有语法错误"), "{note}");
        assert!(!note.contains("bad00"), "no paths in the note: {note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn more_than_ten_problem_files_go_to_a_markdown_report() {
        let dir = temp_dir("report");
        broken(&dir, 12);
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        let detail = index.coverage_detail();
        assert!(detail.contains("syntax/compile errors: 12"), "{detail}");
        assert!(!detail.contains("bad00.py"), "no inline list: {detail}");
        let path = index.coverage_report_path().expect("report written");
        assert!(path.is_absolute());
        assert!(detail.contains(&path.display().to_string()), "{detail}");
        let md = std::fs::read_to_string(&path).unwrap();
        assert!(md.contains("bad00.py") && md.contains("bad11.py"), "{md}");
        assert!(md.contains("python"), "{md}");
        assert!(md.contains("line 4"), "{md}");
        // Deterministic, no leftover temp file, self-ignoring directory.
        assert_eq!(md, index.render_report());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(path.parent().unwrap().join(".gitignore").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn macro_errors_are_labelled_apart_from_other_syntax_errors() {
        let dir = temp_dir("kinds");
        // C: attribute macro the grammar cannot know.
        std::fs::write(
            dir.join("atomic.c"),
            "static redisAtomic size_t used = 0;\nint keep_c(void) { return 1; }\n",
        )
        .unwrap();
        // Python: a real syntax error.
        std::fs::write(
            dir.join("bad.py"),
            "def keep_py():\n    pass\n\ndef oops(:\n",
        )
        .unwrap();
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        assert_eq!(index.partial_files().len(), 2);
        let detail = index.coverage_detail();
        assert!(
            detail.contains("other syntax errors: 1") && detail.contains("macro/preprocessor: 1"),
            "{detail}"
        );
        let line_of = |name: &str| {
            detail
                .lines()
                .find(|l| l.contains(name))
                .unwrap_or_default()
                .to_string()
        };
        assert!(line_of("atomic.c").contains(PREPROC_LABEL), "{detail}");
        assert!(line_of("atomic.c").contains("line 1"), "{detail}");
        assert!(line_of("bad.py").contains(PARTIAL_LABEL), "{detail}");
        assert!(line_of("bad.py").contains("line 4"), "{detail}");
        assert!(
            line_of("bad.py").contains("def oops(:"),
            "snippet: {detail}"
        );
        // Symbols of both stay indexed.
        let names: Vec<&str> = index
            .graph()
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            names.contains(&"keep_c") && names.contains(&"keep_py"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_lists_kind_with_other_syntax_errors_first() {
        let dir = temp_dir("kindrep");
        for i in 0..8 {
            std::fs::write(
                dir.join(format!("a{i:02}.c")),
                format!(
                    "static redisAtomic size_t used{i} = 0;\nint keep{i}(void) {{ return 1; }}\n"
                ),
            )
            .unwrap();
        }
        broken(&dir, 4);
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        let detail = index.coverage_detail();
        assert!(detail.contains("syntax/compile errors: 12"), "{detail}");
        assert!(detail.contains("other syntax errors: 4"), "{detail}");
        assert!(detail.contains("macro/preprocessor: 8"), "{detail}");
        let md = index.render_report();
        assert!(
            md.contains("| path | language | kind | first error |"),
            "{md}"
        );
        let first_py = md.find("bad00.py").unwrap();
        let first_c = md.find("a00.c").unwrap();
        assert!(first_py < first_c, "other syntax errors first:\n{md}");
        assert!(
            md.contains("| a00.c | c | macro/preprocessor | line 1: "),
            "{md}"
        );
        assert!(
            md.contains("| bad00.py | python | syntax | line 4: "),
            "{md}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_is_bounded_with_truncation_note() {
        let report = IndexReport {
            partial_parses: (0..3000)
                .map(|i| (RelPath::new(format!("src/m{i:05}.py")), String::new()))
                .collect(),
            ..IndexReport::default()
        };
        let index = RepoIndex::from_graph(
            PathBuf::from("/nonexistent-root"),
            CodeGraph::default(),
            report,
        );
        let md = index.render_report();
        assert!(md.len() <= REPORT_MAX_BYTES + 512, "{}", md.len());
        assert!(
            md.contains("m00999") && !md.contains("m01000"),
            "cap entries"
        );
        assert!(md.contains("Report truncated"));
    }

    #[test]
    fn hard_gaps_are_counted_by_reason_and_not_partial() {
        let report = IndexReport {
            parse_failures: vec![
                (RelPath::new("a.py"), "parse exceeded its budget".into()),
                (RelPath::new("b.py"), "read: permission denied".into()),
            ],
            too_large: vec![RelPath::new("big.ts")],
            ..IndexReport::default()
        };
        let index = RepoIndex::from_graph(PathBuf::from("/r"), CodeGraph::default(), report);
        assert_eq!(index.hard_gaps().len(), 3);
        let detail = index.coverage_detail();
        assert!(detail.contains("files NOT in the index: 3"), "{detail}");
        assert!(detail.contains("too large (size cap): 1"), "{detail}");
        assert!(detail.contains("read failure: 1"), "{detail}");
        assert!(detail.contains("parser timeout: 1"), "{detail}");
        assert!(detail.contains("a.py"), "{detail}");
        let note = index.coverage_note().unwrap();
        assert!(
            note.contains("3 个文件未进入索引") && !note.contains("a.py"),
            "{note}"
        );
    }

    #[test]
    fn report_write_failure_is_graceful() {
        // `<root>/.astrolabe` is a regular file: primary location fails and the
        // temp-dir fallback (or None) is used without panicking.
        let dir = temp_dir("wfail");
        std::fs::write(dir.join(".astrolabe"), b"not a dir").unwrap();
        let path = write_report(&dir, "# x\n");
        if let Some(p) = &path {
            assert!(!p.starts_with(&dir));
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_block_size_does_not_grow_with_file_count() {
        let mk = |n: usize| {
            let report = IndexReport {
                partial_parses: (0..n)
                    .map(|i| {
                        (
                            RelPath::new(format!("src/deep/dir/m{i:06}.c")),
                            String::new(),
                        )
                    })
                    .collect(),
                parse_failures: (0..n)
                    .map(|i| {
                        (
                            RelPath::new(format!("gen/x{i:06}.c")),
                            "read: denied".into(),
                        )
                    })
                    .collect(),
                ..IndexReport::default()
            };
            let dir = temp_dir("size");
            let index = RepoIndex::from_graph(dir.clone(), CodeGraph::default(), report);
            let len = index.coverage_detail().len();
            let _ = std::fs::remove_dir_all(&dir);
            len
        };
        let small = mk(50);
        let large = mk(5000);
        assert!(large <= 600, "coverage block is {large} B");
        // Only the digits of the counts may differ.
        assert!(large <= small + 16, "{small} -> {large}");
    }

    #[test]
    fn unsupported_files_are_summarised_by_type_with_bounded_size() {
        let mut counts = std::collections::BTreeMap::new();
        for (i, ext) in [
            "scala", "ex", "erl", "zig", "nim", "hs", "ml", "clj", "jl", "r", "pl",
        ]
        .iter()
        .enumerate()
        {
            counts.insert((*ext).to_string(), 100 - i as u32);
        }
        let s = unsupported_by_type(&counts);
        assert!(s.starts_with(".scala×100, .ex×99"), "{s}");
        assert!(s.contains("other 3 types×"), "{s}");
        assert_eq!(s.matches('×').count(), TOP_TYPES + 1, "{s}");
        // 10x the files: identical size.
        let big = counts.iter().map(|(k, v)| (k.clone(), v * 10)).collect();
        assert!(unsupported_by_type(&big).len() <= s.len() + 24);
    }
}
