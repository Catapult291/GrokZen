//! Index-side helpers for `find_references` (batch 6, stream R).
//!
//! The syntax index is the primary source; the language server is the
//! memory-heavy fallback. This module holds everything `find_references`
//! can answer without a language server:
//!
//! * [`snap_position`]: a `line`/`character` that does not sit on the named
//!   `symbol` is moved onto it (same line, then ±1/±2 lines). The symbol wins.
//! * [`fix_path`]: a missing / non-existent / wrong `path` is replaced by the
//!   file of the symbol's only declaration in the index.
//! * [`graph_references`]: when the index holds exactly one declaration of
//!   the name, its code occurrences in the declaring package / file and in
//!   the files that import it are the answer, with no language server.
//! * [`annotate`]: per hit, the source line, the enclosing function (with
//!   its line range), whether it is the declaration, comment / string
//!   classification and test-file status, so a hit list needs no Reads.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use astrolabe_core::cache::FileCache;
use astrolabe_core::lsp::{Location, Position, Precise, Range};
use astrolabe_core::types::{
    CodeSymbol, Confidence, EdgeKind, FileId, Language, RelPath, SymbolKind,
};

use crate::index::RepoIndex;

/// Lines searched above and below a caller-given line when snapping.
const SNAP_RADIUS: u32 = 2;
/// Cap on the occurrences a graph answer may list before it defers to the
/// language server (a name used this widely is rarely unambiguous).
const MAX_GRAPH_HITS: usize = 2_000;
/// Source line text kept per hit.
pub(crate) const HIT_TEXT_CHARS: usize = 120;

/// Languages whose `find_references` is answered from the index first for
/// every symbol kind: Go.
///
/// Chosen by measurement against the language servers
/// (`docs/find-references-graph-first.md`): on cobra / aiproxy / aicon the
/// Go answer recalled every gopls hit (precision ≥ 0.996).
///
/// TypeScript/TSX and Python are index-first only for top-level names
/// with exactly one definition in the repo (batch 7, measured against
/// LanguageService / pyright baselines): see
/// [`crate::refs_graph_ts::ts_graph_first`] and `py_refs::graph_first`.
/// Their methods and fields, names with several definitions, and
/// JavaScript keep the language server first.
pub(crate) fn graph_first_language(language: Language) -> bool {
    matches!(language, Language::Go)
}

thread_local! {
    /// Session file cache for the reads of one `find_references` call (see
    /// [`with_file_cache`]); `None` reads from disk.
    static FILE_CACHE: std::cell::RefCell<Option<Arc<FileCache>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with [`read_lines`] served from `cache` (the session's
/// search_code cache, keyed by repo-relative path, mtime/len-checked), so
/// fix_path / snap / graph / annotate stop re-reading the same files.
pub(crate) fn with_file_cache<R>(cache: Option<&Arc<FileCache>>, f: impl FnOnce() -> R) -> R {
    let prev = FILE_CACHE.with(|c| c.replace(cache.cloned()));
    let out = f();
    FILE_CACHE.with(|c| *c.borrow_mut() = prev);
    out
}

pub(crate) fn read_lines(root: &Path, path: &str) -> Option<Arc<Vec<String>>> {
    if let Some(cache) = FILE_CACHE.with(|c| c.borrow().clone()) {
        return crate::index::load_source_lines(&cache, root, path).map(|(lines, _)| lines);
    }
    let source = astrolabe_core::text::read_source(&root.join(path)).ok()?;
    Some(Arc::new(source.lines().map(str::to_owned).collect()))
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Byte offsets of whole-word occurrences of `name` in `line`.
pub(crate) fn word_offsets(line: &str, name: &str) -> Vec<usize> {
    if name.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = line[from..].find(name) {
        let start = from + i;
        let end = start + name.len();
        let before = line[..start].chars().next_back();
        let after = line[end..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            out.push(start);
        }
        from = start + name.len().max(1);
        while from < line.len() && !line.is_char_boundary(from) {
            from += 1;
        }
        if from >= line.len() {
            break;
        }
    }
    out
}

fn utf16_col(line: &str, byte: usize) -> u32 {
    line[..byte.min(line.len())].encode_utf16().count() as u32
}

fn byte_of_utf16(line: &str, col: u32) -> usize {
    let mut units = 0u32;
    for (i, ch) in line.char_indices() {
        if units >= col {
            return i;
        }
        units += ch.len_utf16() as u32;
    }
    line.len()
}

/// What a hit sits in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HitKind {
    #[default]
    Code,
    Comment,
    Str,
}

/// Comment / string classification of `(line, byte)` hits in one file.
/// `Code` everywhere when the file type has no lexer.
fn classify(path: &str, lines: &[String], hits: &[(usize, usize)]) -> Vec<HitKind> {
    let all_masked = crate::code_mask::code_mask(path, lines, false);
    let comments = crate::code_mask::code_mask(path, lines, true);
    let python = path.ends_with(".py") || path.ends_with(".pyi");
    hits.iter()
        .map(|&(line, byte)| {
            let at = |m: &Option<Vec<Vec<bool>>>| {
                m.as_ref()
                    .and_then(|m| m.get(line))
                    .and_then(|l| l.get(byte))
                    .copied()
                    .unwrap_or(true)
            };
            if !at(&comments) {
                HitKind::Comment
            } else if !at(&all_masked) {
                // Python forward references (`x: "Foo"`) are code.
                if python && crate::py_refs::type_string_in(lines, line, byte) {
                    HitKind::Code
                } else {
                    HitKind::Str
                }
            } else {
                HitKind::Code
            }
        })
        .collect()
}

// ---------------------------------------------------------------- snapping

/// Outcome of [`snap_position`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snap {
    /// Position to query, or `None` to query by name.
    pub position: Option<Position>,
    /// Correction to report, when the position was changed.
    pub note: Option<String>,
}

/// Trust `symbol` over a `line`/`character` that disagrees with it: move to
/// the nearest code occurrence of `symbol` on that line, then ±1 and ±2
/// lines. With no occurrence nearby, drop the position (query by name).
pub(crate) fn snap_position(root: &Path, path: &str, symbol: &str, pos: Position) -> Snap {
    let symbol = symbol.trim();
    let keep = Snap {
        position: Some(pos),
        note: None,
    };
    if symbol.is_empty() {
        return keep;
    }
    let Some(lines) = read_lines(root, path) else {
        return keep;
    };
    let at = astrolabe_core::lsp::queries::identifier_at(root, &RelPath::new(path), pos);
    if at.as_deref() == Some(symbol) {
        return keep;
    }
    let mut order: Vec<i64> = vec![0];
    for d in 1..=i64::from(SNAP_RADIUS) {
        order.push(-d);
        order.push(d);
    }
    for d in order {
        let line_no = i64::from(pos.line) + d;
        let Some(line) = usize::try_from(line_no).ok().and_then(|l| lines.get(l)) else {
            continue;
        };
        let offsets = word_offsets(line, symbol);
        if offsets.is_empty() {
            continue;
        }
        let wanted = byte_of_utf16(line, pos.character);
        let hits: Vec<(usize, usize)> = offsets.iter().map(|&b| (line_no as usize, b)).collect();
        let kinds = classify(path, &lines, &hits);
        let best = offsets
            .iter()
            .zip(kinds.iter())
            .filter(|(_, k)| **k == HitKind::Code)
            .map(|(b, _)| *b)
            .min_by_key(|b| b.abs_diff(wanted))
            .or_else(|| offsets.iter().copied().min_by_key(|b| b.abs_diff(wanted)));
        let Some(byte) = best else { continue };
        let snapped = Position {
            line: line_no as u32,
            character: utf16_col(line, byte),
        };
        let was = at
            .map(|w| format!("是 `{w}`"))
            .unwrap_or_else(|| "不在标识符上".to_string());
        return Snap {
            position: Some(snapped),
            note: Some(format!(
                "坐标已校正：line={} character={} 处{was}，已按 symbol 改为 `{symbol}` 所在的 line={} character={}（0 起）。",
                pos.line, pos.character, snapped.line, snapped.character
            )),
        };
    }
    Snap {
        position: None,
        note: Some(format!(
            "line={} 附近 ±{SNAP_RADIUS} 行没有 `{symbol}`，已忽略 line/character，按名字查询。",
            pos.line
        )),
    }
}

// ----------------------------------------------------------- path fixing

/// Outcome of [`fix_path`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathFix {
    /// The given path is usable as is.
    Keep,
    /// Use this path instead; `note` says why.
    Use { path: String, note: String },
    /// No usable path and several candidates: answer Unknown with `note`.
    Ambiguous { note: String },
}

/// Same-family declarations of exactly `name`, deduplicated per line.
fn declarations<'a>(
    index: &'a RepoIndex,
    name: &str,
    family: Option<Language>,
) -> Vec<&'a CodeSymbol> {
    let graph = index.graph();
    let mut seen = HashSet::new();
    let mut out: Vec<&CodeSymbol> = index
        .names()
        .by_lower(&name.to_lowercase())
        .iter()
        .map(|&i| &graph.symbols[i as usize])
        .filter(|s| s.name == name)
        .filter(|s| {
            family.is_none_or(|lang| {
                index
                    .file(s.file)
                    .and_then(|f| f.language)
                    .is_some_and(|l| same_family(l, lang))
            })
        })
        .filter(|s| seen.insert((s.file, s.start_line)))
        .collect();
    out.sort_by_key(|s| (s.file, s.start_line));
    out
}

/// Languages whose declarations can answer for each other.
pub(crate) fn same_family(a: Language, b: Language) -> bool {
    use Language::*;
    let group = |l: Language| match l {
        Kotlin | Java => 1,
        TypeScript | Tsx | JavaScript | Vue => 2,
        C | Cpp | ObjC | ObjCpp => 3,
        CSharp | VisualBasic => 4,
        _ => 0,
    };
    a == b || (group(a) != 0 && group(a) == group(b))
}

fn contains_word(root: &Path, path: &str, name: &str) -> bool {
    read_lines(root, path)
        .is_some_and(|lines| lines.iter().any(|l| !word_offsets(l, name).is_empty()))
}

/// Replace a missing, non-existent, or symbol-free `path` with the file of
/// the symbol's only declaration (one file) in the index.
pub(crate) fn fix_path(
    index: &RepoIndex,
    root: &Path,
    symbol: &str,
    given: Option<&str>,
) -> PathFix {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return PathFix::Keep;
    }
    let given = given.map(str::trim).filter(|p| !p.is_empty());
    let exists = given.is_some_and(|p| root.join(p).is_file());
    if exists && contains_word(root, given.unwrap_or_default(), symbol) {
        return PathFix::Keep;
    }
    let hint_lang = given.and_then(|p| Language::from_path(&RelPath::new(p)));
    let mut decls = declarations(index, symbol, hint_lang);
    if decls.is_empty() && hint_lang.is_some() {
        decls = declarations(index, symbol, None);
    }
    let files: BTreeSet<FileId> = decls.iter().map(|s| s.file).collect();
    let why = match given {
        None => "未给 path".to_string(),
        Some(p) if !exists => format!("path `{p}` 不存在"),
        Some(p) => format!("`{p}` 里没有 `{symbol}`"),
    };
    match files.len() {
        0 => PathFix::Keep,
        1 => {
            let s = decls[0];
            let Some(file) = index.file(s.file) else {
                return PathFix::Keep;
            };
            PathFix::Use {
                path: file.path.as_str().to_string(),
                note: format!(
                    "{why}；已按索引改用 `{symbol}` 唯一定义所在文件 {}（定义 @{}:{}）。",
                    file.path.as_str(),
                    file.path.as_str(),
                    s.start_line
                ),
            }
        }
        n if !exists => {
            let list: Vec<String> = decls
                .iter()
                .take(8)
                .filter_map(|s| {
                    index
                        .file(s.file)
                        .map(|f| format!("{}:{}", f.path.as_str(), s.start_line))
                })
                .collect();
            let more = if decls.len() > 8 {
                format!(" …共 {} 处", decls.len())
            } else {
                String::new()
            };
            PathFix::Ambiguous {
                note: format!(
                    "{why}，而索引里 `{symbol}` 定义在 {n} 个文件：{}{more}。请把其中一个作为 path 再查。",
                    list.join("、")
                ),
            }
        }
        _ => PathFix::Keep,
    }
}

// --------------------------------------------------------- graph-first

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Import path of the Go package in `def_dir`: the `module` line of the
/// nearest enclosing `go.mod` plus the rest of the directory.
fn go_import_path(root: &Path, def_dir: &str) -> Option<String> {
    let mut dir = def_dir;
    loop {
        let go_mod = if dir.is_empty() {
            root.join("go.mod")
        } else {
            root.join(dir).join("go.mod")
        };
        if let Ok(text) = std::fs::read_to_string(go_mod) {
            let module = text.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("module")
                    .map(|m| m.trim().trim_matches('"').to_string())
                    .filter(|m| !m.is_empty())
            })?;
            let rest = def_dir[dir.len()..].trim_start_matches('/');
            return Some(if rest.is_empty() {
                module
            } else {
                format!("{module}/{rest}")
            });
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir_of(dir);
    }
}

/// Local names of the Go imports in `lines` that are not the package at
/// `def_dir` (`fmt`, `exec`, an alias…): `name.X` through one of them is
/// another package's `X`, not ours.
fn go_foreign_imports(
    lines: &[String],
    def_import: Option<&str>,
    def_dir: &str,
) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_block = false;
    // Imports follow the `package` clause; a package doc comment above it
    // may hold example code (`var png []byte`, `import …`).
    let start = lines
        .iter()
        .position(|l| l.starts_with("package "))
        .map_or(0, |i| i + 1);
    for line in &lines[start..] {
        let t = line.trim();
        let spec = if in_block {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            t
        } else if let Some(rest) = t.strip_prefix("import") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('(') {
                in_block = true;
                inner.trim()
            } else {
                rest
            }
        } else if t.starts_with("func ") || t.starts_with("type ") || t.starts_with("var ") {
            break;
        } else {
            continue;
        };
        let Some(q) = spec.find('"') else { continue };
        let Some(end) = spec[q + 1..].find('"') else {
            continue;
        };
        let path = &spec[q + 1..q + 1 + end];
        let alias = spec[..q].trim();
        // Unknown module layout: a path ending in the package directory's
        // name is kept as ours (recall over precision).
        let ours = def_import.is_some_and(|d| path == d)
            || (def_import.is_none()
                && !def_dir.is_empty()
                && path.rsplit('/').next() == def_dir.rsplit('/').next());
        if ours || alias == "_" || alias == "." {
            continue;
        }
        let local = if alias.is_empty() {
            let last = path.rsplit('/').next().unwrap_or(path);
            let last = if last.len() > 1
                && last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit())
            {
                path.rsplit('/').nth(1).unwrap_or(last)
            } else {
                last
            };
            let last = last.split('.').next().unwrap_or(last);
            last.trim_start_matches("go-").replace('-', "_")
        } else {
            alias.to_string()
        };
        out.insert(local);
    }
    out
}

/// Identifier right before `.` at `byte` (`fmt` in `fmt.Println`).
fn selector_base(line: &str, byte: usize) -> Option<&str> {
    let before = line[..byte].strip_suffix('.')?;
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map(|(i, _)| i)?;
    Some(&before[start..])
}

/// All Go imports in `lines`: `(local name, import path)`. `_` and `.`
/// imports are left out (no selector reaches them).
fn go_imports(lines: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    // Imports follow the `package` clause; a package doc comment above it
    // may hold example code (`var png []byte`, `import …`).
    let start = lines
        .iter()
        .position(|l| l.starts_with("package "))
        .map_or(0, |i| i + 1);
    for line in &lines[start..] {
        let t = line.trim();
        let spec = if in_block {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            t
        } else if let Some(rest) = t.strip_prefix("import") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('(') {
                in_block = true;
                inner.trim()
            } else {
                rest
            }
        } else if t.starts_with("func ") || t.starts_with("type ") || t.starts_with("var ") {
            break;
        } else {
            continue;
        };
        let Some(q) = spec.find('"') else { continue };
        let Some(end) = spec[q + 1..].find('"') else {
            continue;
        };
        let path = &spec[q + 1..q + 1 + end];
        let alias = spec[..q].trim();
        if alias == "_" || alias == "." {
            continue;
        }
        let local = if alias.is_empty() {
            go_default_local(path)
        } else {
            alias.to_string()
        };
        out.push((local, path.to_string()));
    }
    out
}

/// Local name of an un-aliased import (`yaml` for `gopkg.in/yaml.v3`).
fn go_default_local(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last =
        if last.len() > 1 && last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit())
        {
            path.rsplit('/').nth(1).unwrap_or(last)
        } else {
            last
        };
    let last = last.split('.').next().unwrap_or(last);
    last.trim_start_matches("go-").replace('-', "_")
}

/// The import path is the package at `def_dir` (same rule as
/// [`go_foreign_imports`]).
fn go_import_is_def(path: &str, def_import: Option<&str>, def_dir: &str) -> bool {
    def_import.is_some_and(|d| path == d)
        || (def_import.is_none()
            && !def_dir.is_empty()
            && path.rsplit('/').next() == def_dir.rsplit('/').next())
}

/// `module` of the nearest `go.mod` enclosing `dir`.
fn go_module(root: &Path, dir: &str) -> Option<String> {
    let mut d = dir;
    loop {
        let go_mod = if d.is_empty() {
            root.join("go.mod")
        } else {
            root.join(d).join("go.mod")
        };
        if let Ok(text) = std::fs::read_to_string(go_mod) {
            return text.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("module")
                    .map(|m| m.trim().trim_matches('"').to_string())
                    .filter(|m| !m.is_empty())
            });
        }
        if d.is_empty() {
            return None;
        }
        d = dir_of(d);
    }
}

// ------------------------------------------------ Go receiver resolution

/// Byte index of the bracket closing the one at `open`.
fn close_of(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split at `sep` outside brackets.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut from = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&s[from..i]);
                from = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[from..]);
    out
}

fn is_ident_str(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// A Go type expression as `(package qualifier, type name)`: `*pkg.T[X]`
/// is `(Some(pkg), T)`. `None` for slices, maps, funcs, literals.
fn go_type_name(t: &str) -> Option<(Option<String>, String)> {
    let t = t
        .trim()
        .trim_start_matches("...")
        .trim_start_matches(['*', '&'])
        .trim();
    let head = t
        .split(|c: char| matches!(c, '[' | '{' | '(' | ')' | ',' | ';') || c.is_whitespace())
        .next()?;
    let (pkg, name) = match head.split_once('.') {
        Some((p, n)) => (Some(p), n),
        None => (None, head),
    };
    if !is_ident_str(name) || pkg.is_some_and(|p| !is_ident_str(p)) {
        return None;
    }
    if pkg.is_none() && matches!(name, "map" | "chan" | "func" | "struct" | "interface") {
        return None;
    }
    Some((pkg.map(str::to_string), name.to_string()))
}

/// Receiver type of a Go method signature (`Command` for
/// `func (c *Command) Execute() error`). `None` for interface methods.
fn go_receiver_type(signature: &str) -> Option<String> {
    let s = signature.trim().strip_prefix("func")?.trim_start();
    if !s.starts_with('(') {
        return None;
    }
    let inner = &s[1..close_of(s, 0)?];
    let ty = inner
        .trim()
        .rsplit_once(|c: char| c.is_whitespace())
        .map_or(inner, |(_, t)| t);
    go_type_name(ty).map(|(_, n)| n)
}

/// One `func` on a line: receiver list, parameter list, result text.
struct FuncParts<'a> {
    receiver: Option<&'a str>,
    params: &'a str,
    results: &'a str,
}

/// Every `func` (declarations and literals) on `text`.
fn go_func_parts(text: &str) -> Vec<FuncParts<'_>> {
    let mut out = Vec::new();
    for off in word_offsets(text, "func") {
        let mut s = &text[off + 4..];
        let lead = s.len() - s.trim_start().len();
        s = &s[lead..];
        let mut receiver = None;
        if s.starts_with('(') {
            let Some(close) = close_of(s, 0) else {
                continue;
            };
            let after = s[close + 1..].trim_start();
            let name_len = after.find(|c: char| !is_ident(c)).unwrap_or(after.len());
            if name_len > 0 && after[name_len..].trim_start().starts_with(['(', '[']) {
                receiver = Some(&s[1..close]);
                s = &after[name_len..];
            }
        } else {
            let name_len = s.find(|c: char| !is_ident(c)).unwrap_or(s.len());
            s = &s[name_len..];
        }
        s = s.trim_start();
        if s.starts_with('[') {
            let Some(close) = close_of(s, 0) else {
                continue;
            };
            s = s[close + 1..].trim_start();
        }
        if !s.starts_with('(') {
            continue;
        }
        let Some(close) = close_of(s, 0) else {
            continue;
        };
        let params = &s[1..close];
        let rest = &s[close + 1..];
        let results = rest.split('{').next().unwrap_or(rest).trim();
        out.push(FuncParts {
            receiver,
            params,
            results,
        });
    }
    out
}

/// Type text of parameter `x` in a Go parameter list (`a, x *T, n int`).
fn go_param_type<'a>(params: &'a str, x: &str) -> Option<&'a str> {
    let mut pending: Vec<&str> = Vec::new();
    for seg in split_top(params, ',') {
        let seg = seg.trim();
        match seg.split_once(|c: char| c.is_whitespace()) {
            Some((name, ty)) if is_ident_str(name) => {
                if name == x || pending.contains(&x) {
                    return Some(ty.trim());
                }
                pending.clear();
            }
            _ => pending.push(seg),
        }
    }
    None
}

/// `idx`-th result type of a Go result list (`*T`, `(T, error)`,
/// `(c *T, err error)`).
fn go_result_type(results: &str, idx: usize) -> Option<&str> {
    let r = results.trim();
    if let Some(inner) = r.strip_prefix('(') {
        let inner = &inner[..close_of(r, 0)? - 1];
        let seg = split_top(inner, ',').into_iter().nth(idx)?.trim();
        Some(match seg.split_once(|c: char| c.is_whitespace()) {
            Some((name, ty)) if is_ident_str(name) && !ty.trim().is_empty() => ty.trim(),
            _ => seg,
        })
    } else {
        (idx == 0 && !r.is_empty()).then_some(r)
    }
}

/// How the name `x` is bound before `(line, byte)`.
enum Binding {
    /// Declared type text (parameter, `var x T`).
    Type(String),
    /// Initializer expression; the variable is its `idx`-th value.
    Expr(String, usize),
    /// Bound, but not resolvable (`range`, type switch…).
    Opaque,
}

/// Nearest binding of `x` above `(line, byte)`, up to and including the
/// enclosing top-level `func` line.
fn go_local_binding(lines: &[String], line: usize, byte: usize, x: &str) -> Option<Binding> {
    let last = line.min(lines.len().checked_sub(1)?);
    for i in (0..=last).rev() {
        let full = &lines[i];
        let text = if i == line {
            &full[..byte.min(full.len())]
        } else {
            full.as_str()
        };
        if let Some(b) = binding_on_line(text, x) {
            return Some(b);
        }
        if full.starts_with("func ") {
            break;
        }
    }
    None
}

fn binding_on_line(text: &str, x: &str) -> Option<Binding> {
    if word_offsets(text, x).is_empty() {
        return None;
    }
    // Parameters and receivers of a func on this line.
    for parts in go_func_parts(text) {
        if let Some(t) = parts.receiver.and_then(|r| go_param_type(r, x)) {
            return Some(Binding::Type(t.to_string()));
        }
        if let Some(t) = go_param_type(parts.params, x) {
            return Some(Binding::Type(t.to_string()));
        }
    }
    // `a, x := expr` (also inside `if` / `for` / `switch` headers).
    for (pos, _) in text.match_indices(":=") {
        let lhs = &text[..pos];
        let lhs = lhs.rsplit([';', '(', '{']).next().unwrap_or(lhs).trim();
        let lhs = ["if ", "for ", "switch ", "case "]
            .iter()
            .find_map(|k| lhs.strip_prefix(k))
            .unwrap_or(lhs);
        let names: Vec<&str> = lhs.split(',').map(str::trim).collect();
        if !names.iter().all(|n| is_ident_str(n)) {
            continue;
        }
        let Some(idx) = names.iter().position(|n| *n == x) else {
            continue;
        };
        let rhs = text[pos + 2..].trim();
        let rhs = split_top(rhs, ';')[0].trim();
        if rhs.starts_with("range ") || rhs.contains(".(type)") {
            return Some(Binding::Opaque);
        }
        return Some(Binding::Expr(rhs.to_string(), idx));
    }
    // `var x T`, `var x = expr`, `var x, y T`.
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("var ") {
        return var_spec_binding(rest, x);
    }
    None
}

/// `x T`, `x = expr`, `x, y T = a, b` (a var spec without `var`).
fn var_spec_binding(spec: &str, x: &str) -> Option<Binding> {
    let (left, init) = match split_top(spec, '=').as_slice() {
        [l, r, ..] => (*l, Some(*r)),
        [l] => (*l, None),
        _ => return None,
    };
    // Names are a comma list; the type follows the last name.
    let mut names = Vec::new();
    let mut rest = left.trim();
    loop {
        let n = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        names.push(&rest[..n]);
        rest = rest[n..].trim_start();
        if let Some(r) = rest.strip_prefix(',') {
            rest = r.trim_start();
        } else {
            break;
        }
    }
    let ty = rest.trim();
    let idx = names.iter().position(|n| *n == x)?;
    if !ty.is_empty() {
        return Some(Binding::Type(ty.to_string()));
    }
    let init = init?.trim();
    Some(Binding::Expr(init.to_string(), idx))
}

/// A resolved receiver type, relative to the declaring package.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GoTy {
    /// Named type `name` of package directory `dir`.
    Named { dir: String, name: String },
    /// Type of a package outside the repo (stdlib, third party).
    External,
    /// Built-in non-interface type: has no methods.
    Builtin,
}

/// Verdict on one `x.Method` hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recv {
    Ours,
    Other,
    Unknown,
}

const GO_BUILTINS: &[&str] = &[
    "bool",
    "string",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "byte",
    "rune",
    "float32",
    "float64",
    "complex64",
    "complex128",
];

/// Shared state for resolving receivers of one method's hits.
struct GoRecv<'a> {
    index: &'a RepoIndex,
    root: &'a Path,
    def_dir: &'a str,
    def_import: Option<&'a str>,
    module: Option<String>,
    /// Directories holding Go files.
    go_dirs: HashSet<String>,
    /// Receiver type of the method being searched.
    recv: &'a str,
    method: &'a str,
    files: HashMap<String, Option<Arc<Vec<String>>>>,
}

impl<'a> GoRecv<'a> {
    fn lines(&mut self, path: &str) -> Option<Arc<Vec<String>>> {
        self.files
            .entry(path.to_string())
            .or_insert_with(|| read_lines(self.root, path))
            .clone()
    }

    /// `(qualifier, name)` as written in a file of package `dir` with
    /// `imports`, as a [`GoTy`].
    fn ty_in(
        &self,
        dir: &str,
        imports: &[(String, String)],
        parsed: (Option<String>, String),
    ) -> Option<GoTy> {
        let (pkg, name) = parsed;
        match pkg {
            None if GO_BUILTINS.contains(&name.as_str()) => Some(GoTy::Builtin),
            None => Some(GoTy::Named {
                dir: dir.to_string(),
                name,
            }),
            Some(p) => {
                let (_, path) = imports.iter().find(|(l, _)| *l == p)?;
                if go_import_is_def(path, self.def_import, self.def_dir) {
                    return Some(GoTy::Named {
                        dir: self.def_dir.to_string(),
                        name,
                    });
                }
                // In-repo packages may embed our type: not resolved further.
                (!self.in_repo(path)).then_some(GoTy::External)
            }
        }
    }

    /// The import path names a package of this repo (any module of it,
    /// vendored copies included): the path is under the declaring module,
    /// or its last two segments (one, for one-segment paths) are a Go
    /// directory of the repo.
    fn in_repo(&self, path: &str) -> bool {
        if self
            .module
            .as_deref()
            .is_some_and(|m| path == m || path.starts_with(&format!("{m}/")))
        {
            return true;
        }
        let segs: Vec<&str> = path.split('/').collect();
        let min = segs.len().min(2);
        (min..=segs.len()).any(|n| {
            let suffix = segs[segs.len() - n..].join("/");
            self.go_dirs.contains(&suffix)
                || self
                    .go_dirs
                    .iter()
                    .any(|d| d.ends_with(&format!("/{suffix}")))
        })
    }

    fn verdict(&self, ty: Option<GoTy>) -> Recv {
        match ty {
            Some(GoTy::External | GoTy::Builtin) => Recv::Other,
            Some(GoTy::Named { dir, name }) => {
                if dir == self.def_dir && name == self.recv {
                    Recv::Ours
                } else if self.has_own_method(&dir, &name) {
                    // Its own method of that name shadows any embedded one.
                    Recv::Other
                } else {
                    // Could embed our type, or be an interface.
                    Recv::Unknown
                }
            }
            None => Recv::Unknown,
        }
    }

    fn has_own_method(&self, dir: &str, ty: &str) -> bool {
        declarations(self.index, self.method, Some(Language::Go))
            .iter()
            .any(|s| {
                s.kind == SymbolKind::Method
                    && go_receiver_type(&s.signature).as_deref() == Some(ty)
                    && self
                        .index
                        .file(s.file)
                        .is_some_and(|f| dir_of(f.path.as_str()) == dir)
            })
    }

    /// Type of the receiver `base` of the hit at `(line, byte)` of `path`.
    fn base_type(&mut self, path: &str, line: usize, byte: usize, base: &str) -> Option<GoTy> {
        let lines = self.lines(path)?;
        let dir = dir_of(path).to_string();
        let imports = go_imports(&lines);
        let base_start = byte.checked_sub(base.len() + 1)?;
        let before = &lines[line][..base_start];
        if before.ends_with('.') {
            return self.field_type(base);
        }
        if base == self.recv && dir == self.def_dir {
            // Method expression `Command.Execute`.
            return Some(GoTy::Named {
                dir,
                name: base.to_string(),
            });
        }
        self.var_type(&lines, &dir, &imports, line, base_start, base, 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn var_type(
        &mut self,
        lines: &[String],
        dir: &str,
        imports: &[(String, String)],
        line: usize,
        byte: usize,
        x: &str,
        depth: u32,
    ) -> Option<GoTy> {
        if depth > 3 {
            return None;
        }
        let binding = go_local_binding(lines, line, byte, x).or_else(|| self.package_var(dir, x));
        match binding? {
            Binding::Opaque => None,
            Binding::Type(t) => self.ty_in(dir, imports, go_type_name(&t)?),
            Binding::Expr(e, idx) => self.expr_type(lines, dir, imports, line, &e, idx, depth),
        }
    }

    /// Package-level `var x …` of package `dir`, from the index.
    fn package_var(&mut self, dir: &str, x: &str) -> Option<Binding> {
        let decl = declarations(self.index, x, Some(Language::Go))
            .into_iter()
            .find(|s| {
                s.kind == SymbolKind::Variable
                    && self
                        .index
                        .file(s.file)
                        .is_some_and(|f| dir_of(f.path.as_str()) == dir)
            })?;
        let sig = decl.signature.trim();
        let spec = sig.strip_prefix("var ").unwrap_or(sig);
        match var_spec_binding(spec, x)? {
            // The signature stops at `{`: `x = &T{…}` reads `x = &T`.
            Binding::Expr(e, idx) if e.starts_with('&') && !e.contains('(') => {
                Some(Binding::Type(e.to_string()).then_idx(idx))
            }
            Binding::Expr(e, _) if !e.contains('(') => Some(Binding::Opaque),
            b => Some(b),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn expr_type(
        &mut self,
        lines: &[String],
        dir: &str,
        imports: &[(String, String)],
        line: usize,
        expr: &str,
        idx: usize,
        depth: u32,
    ) -> Option<GoTy> {
        let e = expr.trim();
        let e = e.strip_prefix('&').unwrap_or(e).trim();
        if let Some(inner) = e.strip_prefix("new(") {
            return (idx == 0)
                .then(|| self.ty_in(dir, imports, go_type_name(inner)?))
                .flatten();
        }
        if is_ident_str(e) {
            // `x := y`: y's type.
            return (idx == 0)
                .then(|| self.var_type(lines, dir, imports, line, 0, e, depth + 1))
                .flatten();
        }
        // `x := a.b.field`: the field's type (when not `pkg.Var`).
        let parts: Vec<&str> = e.split('.').collect();
        if parts.len() >= 2 && parts.iter().all(|p| is_ident_str(p)) {
            if idx != 0 || imports.iter().any(|(l, _)| l == parts[0]) {
                return None;
            }
            return self.field_type(parts[parts.len() - 1]);
        }
        let open = e.find(['{', '('])?;
        let head = e[..open].trim();
        if e[open..].starts_with('{') {
            if idx != 0 {
                return None;
            }
            return self.ty_in(dir, imports, go_type_name(head)?);
        }
        // A call: `F(…)`, `pkg.F(…)`, `T(x)` conversion.
        if head == "func" {
            return None;
        }
        match head.split_once('.') {
            Some((p, f)) if is_ident_str(p) && is_ident_str(f) => {
                if let Some((_, path)) = imports.iter().find(|(l, _)| l == p) {
                    if go_import_is_def(path, self.def_import, self.def_dir) {
                        let def_dir = self.def_dir.to_string();
                        return self.func_result(&def_dir, f, idx);
                    }
                    return (!self.in_repo(path)).then_some(GoTy::External);
                }
                // `v.M(…)`: M's result, when v's type and M are known.
                if idx > 0 || depth >= 3 {
                    return None;
                }
                match self.var_type(lines, dir, imports, line, 0, p, depth + 1)? {
                    GoTy::Named {
                        dir: tdir,
                        name: ty,
                    } => self.method_result(&tdir, &ty, f),
                    _ => None,
                }
            }
            None if is_ident_str(head) => {
                // Local func value `f := func() *T {…}` first, then the package.
                if let Some(Binding::Expr(rhs, 0)) = go_local_binding(lines, line, 0, head) {
                    if let Some(parts) = go_func_parts(&rhs).into_iter().next() {
                        let t = go_result_type(parts.results, idx)?;
                        return self.ty_in(dir, imports, go_type_name(t)?);
                    }
                }
                if let Some(t) = self.func_result(dir, head, idx) {
                    return Some(t);
                }
                // Conversion to a named type: `T(x)`.
                let ty = self.ty_in(dir, imports, (None, head.to_string()))?;
                let is_type = declarations(self.index, head, Some(Language::Go))
                    .iter()
                    .any(|s| {
                        matches!(
                            s.kind,
                            SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Type
                        ) && self
                            .index
                            .file(s.file)
                            .is_some_and(|f| dir_of(f.path.as_str()) == dir)
                    });
                (is_type && idx == 0).then_some(ty)
            }
            _ => None,
        }
    }

    /// `idx`-th result type of function `f` of package `dir`.
    fn func_result(&mut self, dir: &str, f: &str, idx: usize) -> Option<GoTy> {
        let decls = declarations(self.index, f, Some(Language::Go));
        let decl = decls.iter().find(|s| {
            s.kind == SymbolKind::Function
                && self
                    .index
                    .file(s.file)
                    .is_some_and(|fl| dir_of(fl.path.as_str()) == dir)
        })?;
        let path = self.index.file(decl.file)?.path.as_str().to_string();
        let parts = go_func_parts(&decl.signature).into_iter().next()?;
        let t = go_result_type(parts.results, idx)?.to_string();
        let lines = self.lines(&path)?;
        let imports = go_imports(&lines);
        self.ty_in(dir, &imports, go_type_name(&t)?)
    }

    /// First result type of method `m` of type `ty` in package `dir`.
    fn method_result(&mut self, dir: &str, ty: &str, m: &str) -> Option<GoTy> {
        let decls = declarations(self.index, m, Some(Language::Go));
        let decl = decls.iter().find(|s| {
            s.kind == SymbolKind::Method
                && go_receiver_type(&s.signature).as_deref() == Some(ty)
                && self
                    .index
                    .file(s.file)
                    .is_some_and(|fl| dir_of(fl.path.as_str()) == dir)
        })?;
        let path = self.index.file(decl.file)?.path.as_str().to_string();
        let parts = go_func_parts(&decl.signature).into_iter().next()?;
        let t = go_result_type(parts.results, 0)?.to_string();
        let lines = self.lines(&path)?;
        let imports = go_imports(&lines);
        self.ty_in(dir, &imports, go_type_name(&t)?)
    }

    /// Type of struct field `name`, when every Go field of that name in
    /// the repo agrees.
    fn field_type(&mut self, name: &str) -> Option<GoTy> {
        let fields: Vec<(String, String)> = declarations(self.index, name, Some(Language::Go))
            .iter()
            .filter(|s| s.kind == SymbolKind::Field)
            .filter_map(|s| {
                let path = self.index.file(s.file)?.path.as_str().to_string();
                Some((path, s.signature.clone()))
            })
            .collect();
        let mut agreed: Option<GoTy> = None;
        for (path, sig) in fields {
            let rest = sig.trim().strip_prefix(name)?.trim();
            let lines = self.lines(&path)?;
            let imports = go_imports(&lines);
            let ty = self.ty_in(dir_of(&path), &imports, go_type_name(rest)?)?;
            match &agreed {
                None => agreed = Some(ty),
                Some(a) if *a == ty => {}
                Some(_) => return None,
            }
        }
        agreed
    }
}

impl Binding {
    fn then_idx(self, idx: usize) -> Binding {
        match self {
            Binding::Type(t) if idx == 0 => Binding::Type(t),
            Binding::Type(_) => Binding::Opaque,
            b => b,
        }
    }
}

/// How [`choose_declaration`] picked among several declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    /// The only declaration (or the only non-field one).
    Only,
    /// By the file / package / imports of `path`, or the declaration line.
    Path,
    /// By the occurrence at the caller's line (a call site): its package
    /// qualifier, receiver type or package.
    CallSite,
}

/// Pick the declaration `find_references` is about when the name has
/// several: the one declared at `line` of `origin`, the one the use of the
/// name at `line` resolves to (Go: package qualifier, receiver type, or
/// the package of a bare name), the one whose range holds `line`, the only
/// one in `origin`, the only one in `origin`'s Go package, or the only one
/// in a package `origin` imports. `None` when that still leaves a choice.
fn choose_declaration<'a>(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    decls: &[&'a CodeSymbol],
    origin: &RelPath,
    line: Option<u32>,
    go: bool,
) -> Option<(&'a CodeSymbol, Choice)> {
    if let [only] = decls {
        return Some((only, Choice::Only));
    }
    // Other languages keep the old rule: one declaration or the server.
    if !go {
        return None;
    }
    let path_of = |s: &CodeSymbol| index.file(s.file).map(|f| f.path.as_str().to_string());
    // Struct fields lose to a type / func of the same name.
    let pick = |c: Vec<&'a CodeSymbol>| -> Option<&'a CodeSymbol> {
        if let [one] = c.as_slice() {
            return Some(one);
        }
        let non_field: Vec<_> = c.iter().filter(|s| s.kind != SymbolKind::Field).collect();
        match non_field.as_slice() {
            [one] => Some(**one),
            _ => None,
        }
    };
    let in_file: Vec<&CodeSymbol> = decls
        .iter()
        .copied()
        .filter(|s| path_of(s).as_deref() == Some(origin.as_str()))
        .collect();
    if let Some(line) = line.map(|l| l + 1) {
        // The declaration line itself.
        if let Some(d) = in_file.iter().copied().find(|s| s.start_line == line) {
            return Some((d, Choice::Path));
        }
        // A use of the name on that line (a call site): its package
        // qualifier, receiver type or package decides (batch 8, H6).
        match go_call_site(index, root, name, decls, origin, line as usize - 1) {
            Site::Pick(d) => return Some((d, Choice::CallSite)),
            Site::Block => return None,
            Site::Undecided => {}
        }
        let at = in_file
            .iter()
            .copied()
            .filter(|s| s.start_line <= line && line <= s.end_line.max(s.start_line))
            .min_by_key(|s| s.end_line.saturating_sub(s.start_line));
        if let Some(at) = at {
            return Some((at, Choice::Path));
        }
    }
    let pick = |c: Vec<&'a CodeSymbol>| pick(c).map(|d| (d, Choice::Path));
    if !in_file.is_empty() {
        return pick(in_file);
    }
    let origin_dir = dir_of(origin.as_str());
    let in_pkg: Vec<&CodeSymbol> = decls
        .iter()
        .copied()
        .filter(|s| path_of(s).is_some_and(|p| dir_of(&p) == origin_dir))
        .collect();
    if !in_pkg.is_empty() {
        return pick(in_pkg);
    }
    // A caller file: the declaration in a package it imports.
    let graph = index.graph();
    let origin_id = graph.files.iter().find(|f| f.path == *origin)?.id.0;
    let imported_dirs: HashSet<String> = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import && e.from == origin_id)
        .filter_map(|e| index.file(FileId(e.to)))
        .map(|f| dir_of(f.path.as_str()).to_string())
        .collect();
    let imported: Vec<&CodeSymbol> = decls
        .iter()
        .copied()
        .filter(|s| path_of(s).is_some_and(|p| imported_dirs.contains(dir_of(&p))))
        .collect();
    if imported.is_empty() {
        return None;
    }
    pick(imported)
}

/// The bare Go name at `(line, byte)` is a function-local binding: inside
/// one of `funcs` (1-based line ranges), it is bound on its own line (`x :=`
/// left side, a parameter) or above it in the function.
fn go_shadowed(
    lines: &[String],
    line: usize,
    byte: usize,
    name: &str,
    funcs: &[(u32, u32)],
) -> bool {
    let line1 = line as u32 + 1;
    if !funcs.iter().any(|&(s, e)| s <= line1 && line1 <= e) {
        return false;
    }
    binding_on_line(&lines[line], name).is_some()
        || go_local_binding(lines, line, byte, name).is_some()
}

/// What the Go use(s) of a name on the caller's line say.
enum Site<'a> {
    /// They all refer to this declaration.
    Pick(&'a CodeSymbol),
    /// They refer to different declarations, or to a method the index cannot
    /// pick: the file / package of `path` must not decide either.
    Block,
    /// No code use, or none tells (a foreign package, a shadowing local).
    Undecided,
}

/// The declaration the Go use(s) of `name` on 0-based `line` of `origin`
/// refer to: `pkg.Name` → the top-level one in the package `pkg` imports;
/// `x.Name` → the method whose receiver type `x` has (or the only method of
/// that name that `x` may have); a bare `Name` → the top-level one of
/// `origin`'s package (unless a local binding shadows it).
fn go_call_site<'a>(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    decls: &[&'a CodeSymbol],
    origin: &RelPath,
    line: usize,
) -> Site<'a> {
    let Some(lines) = read_lines(root, origin.as_str()) else {
        return Site::Undecided;
    };
    let Some(text) = lines.get(line) else {
        return Site::Undecided;
    };
    let offsets = word_offsets(text, name);
    if offsets.is_empty() {
        return Site::Undecided;
    }
    let at: Vec<(usize, usize)> = offsets.iter().map(|&b| (line, b)).collect();
    let kinds = classify(origin.as_str(), &lines, &at);
    let imports = go_imports(&lines);
    let origin_dir = dir_of(origin.as_str());
    let path_of = |s: &CodeSymbol| index.file(s.file).map(|f| f.path.as_str().to_string());
    let top = |s: &&&CodeSymbol| !matches!(s.kind, SymbolKind::Method | SymbolKind::Field);
    let one = |c: Vec<&'a CodeSymbol>| match c.as_slice() {
        [d] => Site::Pick(d),
        _ => Site::Undecided,
    };
    let methods: Vec<&'a CodeSymbol> = decls
        .iter()
        .copied()
        .filter(|s| s.kind == SymbolKind::Method)
        .collect();
    // An interface method may be what any `x.Name` calls.
    let interface = methods
        .iter()
        .any(|s| go_receiver_type(&s.signature).is_none());
    // A member access whose receiver the index cannot type: the only
    // method of that name, if there is one.
    let only_method = || match methods.as_slice() {
        [m] if !interface => Site::Pick(m),
        _ => Site::Block,
    };
    let mut chosen: Option<&'a CodeSymbol> = None;
    for (byte, kind) in offsets.into_iter().zip(kinds) {
        if kind != HitKind::Code {
            continue;
        }
        let site = match selector_base(text, byte) {
            Some(base) => {
                let chained = text[..byte - base.len() - 1].ends_with('.');
                let pkg = (!chained)
                    .then(|| imports.iter().find(|(l, _)| l == base))
                    .flatten();
                if let Some((_, ipath)) = pkg {
                    // `pkg.Name`: the package's own top-level declaration.
                    one(decls
                        .iter()
                        .filter(top)
                        .copied()
                        .filter(|s| {
                            path_of(s).is_some_and(|p| {
                                let d = dir_of(&p);
                                go_import_is_def(ipath, go_import_path(root, d).as_deref(), d)
                            })
                        })
                        .collect())
                } else if interface {
                    Site::Block
                } else {
                    // `x.Name`: the receiver type decides among methods.
                    let (mut ours, mut unknown) = (Vec::new(), Vec::new());
                    for &s in &methods {
                        let (Some(recv), Some(def_path)) =
                            (go_receiver_type(&s.signature), path_of(s))
                        else {
                            continue;
                        };
                        let def_dir = dir_of(&def_path);
                        let def_import = go_import_path(root, def_dir);
                        let mut r =
                            go_recv(index, root, def_dir, def_import.as_deref(), &recv, name);
                        let ty = r.base_type(origin.as_str(), line, byte, base);
                        match r.verdict(ty) {
                            Recv::Ours => ours.push(s),
                            Recv::Other => {}
                            Recv::Unknown => unknown.push(s),
                        }
                    }
                    match (ours.as_slice(), unknown.as_slice()) {
                        ([one], _) => Site::Pick(one),
                        ([], [one]) => Site::Pick(one),
                        // Another type's method (stdlib, third party).
                        ([], []) => Site::Undecided,
                        _ => Site::Block,
                    }
                }
            }
            // `f().Name`, `xs[i].Name`: a method / field, receiver untyped.
            None if text[..byte].ends_with('.') => only_method(),
            None => {
                if go_local_binding(&lines, line, byte, name).is_some() {
                    Site::Undecided
                } else {
                    one(decls
                        .iter()
                        .filter(top)
                        .copied()
                        .filter(|s| path_of(s).is_some_and(|p| dir_of(&p) == origin_dir))
                        .collect())
                }
            }
        };
        match site {
            Site::Block => return Site::Block,
            Site::Undecided => {}
            Site::Pick(d) => match chosen {
                Some(c) if !std::ptr::eq(c, d) => return Site::Block,
                _ => chosen = Some(d),
            },
        }
    }
    chosen.map_or(Site::Undecided, Site::Pick)
}

/// Receiver resolver for the method `name` of `recv` declared in `def_dir`.
fn go_recv<'a>(
    index: &'a RepoIndex,
    root: &'a Path,
    def_dir: &'a str,
    def_import: Option<&'a str>,
    recv: &'a str,
    name: &'a str,
) -> GoRecv<'a> {
    GoRecv {
        index,
        root,
        def_dir,
        def_import,
        module: go_module(root, def_dir),
        go_dirs: index
            .graph()
            .files
            .iter()
            .filter(|f| f.language == Some(Language::Go))
            .map(|f| dir_of(f.path.as_str()).to_string())
            .collect(),
        recv,
        method: name,
        files: HashMap::new(),
    }
}

/// `name` at `byte` is a composite-literal key (`{Name: v`, `, Name: v`,
/// or a line of its own `Name: v,`), not `case Name:` or a label.
fn go_literal_key(line: &str, byte: usize, name: &str) -> bool {
    let after = line[byte + name.len()..].trim_start();
    if !after.starts_with(':') || after.starts_with(":=") {
        return false;
    }
    let before = line[..byte].trim_end();
    if before.is_empty() {
        // `Name: v,` alone on a line: a key, unless it is a label (a label
        // is followed by nothing or a statement, not a value and a comma).
        return after.trim_end().ends_with(',') || after.trim_end().ends_with('{');
    }
    before.ends_with('{') || before.ends_with(',')
}

/// Max `path:line` entries listed per category in the note.
const NOTE_LIST: usize = 8;

fn list_hits(hits: &[(String, usize)]) -> String {
    let mut s: Vec<String> = hits
        .iter()
        .take(NOTE_LIST)
        .map(|(p, l)| format!("{p}:{}", l + 1))
        .collect();
    if hits.len() > NOTE_LIST {
        s.push(format!("…共 {} 处", hits.len()));
    }
    s.join("、")
}

/// The index's answer to "references of `name`", when it can tell which
/// declaration is meant: the only same-family declaration of `name`, or,
/// with several, the one `origin` (and `line`, 0-based) points at (see
/// [`choose_declaration`]). `None` otherwise (the caller then asks the
/// language server).
///
/// Go: occurrences in the declaring package (its directory) and, for an
/// exported name, in files whose import edges reach that package. A Go
/// method's `x.Name` hits are checked against the receiver type (`x`'s
/// parameter / `var` / `:=` / field type from the source and the index):
/// other types' same-named methods are dropped, unresolvable receivers are
/// kept and listed as unconfirmed. Top-level names drop `x.Name` member
/// accesses. Other languages: the declaring file and the files importing
/// it. Comments and strings are excluded; hits come back sorted by path and
/// position.
pub(crate) fn graph_references(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    origin: &RelPath,
) -> Option<Precise<Vec<Location>>> {
    graph_references_at(index, root, name, origin, None)
}

/// [`graph_references`] with the caller's 0-based `line` in `origin`, which
/// picks among several declarations in that file.
pub(crate) fn graph_references_at(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    origin: &RelPath,
    line: Option<u32>,
) -> Option<Precise<Vec<Location>>> {
    let language = Language::from_path(origin)?;
    let go = language == Language::Go;
    let decls = declarations(index, name, Some(language));
    match choose_declaration(index, root, name, &decls, origin, line, go) {
        Some((decl, how)) => {
            decl_refs(index, root, name, language, &decls, decl, how).map(|r| r.answer)
        }
        // Go: several declarations and nothing picks one. When their
        // references do not overlap, one grouped answer instead of gopls.
        None if go => grouped_references(index, root, name, language, &decls),
        None => None,
    }
}

/// One declaration's index answer, plus the hits whose receiver could not
/// be typed (path, 0-based line).
struct DeclRefs {
    answer: Precise<Vec<Location>>,
    unconfirmed: Vec<(String, usize)>,
}

/// References of `decl` (one of `decls`, all named `name`) from the index.
fn decl_refs(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    language: Language,
    decls: &[&CodeSymbol],
    decl: &CodeSymbol,
    how: Choice,
) -> Option<DeclRefs> {
    let go = language == Language::Go;
    let graph = index.graph();
    let def_file = index.file(decl.file)?;
    let def_path = def_file.path.as_str();
    let def_dir = dir_of(def_path);
    let exported = name.chars().next().is_some_and(char::is_uppercase);
    // Files whose imports reach the declaration (Go: any file of the package).
    let targets: HashSet<u32> = graph
        .files
        .iter()
        .filter(|f| {
            if go {
                dir_of(f.path.as_str()) == def_dir && f.language == Some(Language::Go)
            } else {
                f.id == decl.file
            }
        })
        .map(|f| f.id.0)
        .collect();
    let importers: HashSet<u32> = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import && targets.contains(&e.to))
        .map(|e| e.from)
        .collect();
    // TS/JS: also barrel re-exports and renamed bindings (refs_graph_ts).
    let ts = (!go && crate::refs_graph_ts::is_ts_family(language))
        .then(|| crate::refs_graph_ts::ts_scope(index, root, decl.file, decl.start_line, name));
    // Python: also `__init__` / `__all__` re-exports and `as` aliases (py_refs).
    let py = (language == Language::Python)
        .then(|| crate::py_refs::python_scope(index, root, decl.file, name));
    // Python methods / attributes: receivers checked against the class.
    let py_member = py
        .as_ref()
        .filter(|_| decl.kind != SymbolKind::Class)
        .and_then(|(s, _)| crate::py_refs::PyMember::new(index, root, decl, name, s));
    let mut files: Vec<&RelPath> = graph
        .files
        .iter()
        .filter(|f| f.language.is_some_and(|l| same_family(l, language)))
        .filter(|f| {
            if go {
                dir_of(f.path.as_str()) == def_dir || (exported && importers.contains(&f.id.0))
            } else {
                f.id == decl.file
                    || importers.contains(&f.id.0)
                    || ts.as_ref().is_some_and(|t| t.contains(f.id.0))
                    || py.as_ref().is_some_and(|(s, _)| s.contains_key(&f.id.0))
                    || py_member
                        .as_ref()
                        .is_some_and(|m| m.extra_files.contains(&f.id.0))
            }
        })
        .map(|f| &f.path)
        .collect();
    files.sort();
    let def_import = go.then(|| go_import_path(root, def_dir)).flatten();
    // Go methods: hits are checked against the receiver type.
    let method_recv = (go && decl.kind == SymbolKind::Method)
        .then(|| go_receiver_type(&decl.signature))
        .flatten();
    let interface_method = go && decl.kind == SymbolKind::Method && method_recv.is_none();
    // Top-level Go names are reached bare or as `pkg.Name`, never `x.Name`.
    let top_level = go && !matches!(decl.kind, SymbolKind::Method | SymbolKind::Field);
    let mut resolver = method_recv
        .as_deref()
        .map(|recv| go_recv(index, root, def_dir, def_import.as_deref(), recv, name));
    // Same-named declarations other than `decl`, by (path, 1-based line).
    let others: HashMap<(String, u32), &CodeSymbol> = decls
        .iter()
        .filter(|s| !std::ptr::eq(**s, decl))
        .filter_map(|s| {
            Some((
                (index.file(s.file)?.path.as_str().to_string(), s.start_line),
                *s,
            ))
        })
        .collect();
    let name_units = name.encode_utf16().count() as u32;
    let mut hits = Vec::new();
    let mut other_type: Vec<(String, usize)> = Vec::new();
    let mut unconfirmed: Vec<(String, usize)> = Vec::new();
    let mut members = 0usize;
    for path in files {
        let Some(lines) = read_lines(root, path.as_str()) else {
            continue;
        };
        // The name, plus (TS/JS) the local aliases it has in this file.
        let words: Vec<&str> = match (&ts, index.file_id(path.as_str())) {
            (Some(t), Some(id)) => t.words(id.0, name),
            (None, Some(id)) => match py.as_ref().and_then(|(s, _)| s.get(&id.0)) {
                Some(w) => w.iter().map(String::as_str).collect(),
                None => vec![name],
            },
            _ => vec![name],
        };
        let mut alias_units: HashMap<(usize, usize), u32> = HashMap::new();
        let mut found: Vec<(usize, usize)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            for w in &words {
                for b in word_offsets(line, w) {
                    found.push((i, b));
                    if *w != name {
                        alias_units.insert((i, b), w.encode_utf16().count() as u32);
                    }
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        if found.is_empty() {
            continue;
        }
        let kinds = classify(path.as_str(), &lines, &found);
        let dot_import = go
            && lines
                .iter()
                .any(|l| l.trim_start().starts_with(". \"") || l.contains("import . \""));
        let (foreign, ours_locals) = if go {
            let imports = go_imports(&lines);
            let ours: HashSet<String> = imports
                .iter()
                .filter(|(_, p)| go_import_is_def(p, def_import.as_deref(), def_dir))
                .map(|(l, _)| l.clone())
                .collect();
            (
                go_foreign_imports(&lines, def_import.as_deref(), def_dir),
                ours,
            )
        } else {
            (HashSet::new(), HashSet::new())
        };
        let mut first_on_line: HashSet<usize> = HashSet::new();
        // Go: (start, end) lines of this file's functions, on demand.
        let mut funcs: Option<Vec<(u32, u32)>> = None;
        for ((line, byte), kind) in found.into_iter().zip(kinds) {
            let first = first_on_line.insert(line);
            if kind != HitKind::Code {
                continue;
            }
            let base = selector_base(&lines[line], byte);
            if base.is_some_and(|b| foreign.contains(b)) {
                continue;
            }
            let is_decl =
                first && def_path == path.as_str() && decl.start_line as usize == line + 1;
            let other_decl = (go && first)
                .then(|| others.get(&(path.as_str().to_string(), line as u32 + 1)))
                .flatten();
            if let Some(o) = other_decl {
                // Another declaration's own name: not a reference to ours,
                // except an embedded field (`*Command` in a struct).
                let embedded =
                    o.kind == SymbolKind::Field && o.signature.split_whitespace().count() == 1;
                if !embedded {
                    continue;
                }
            }
            // TS/JS object-literal key / type member `name: …` (after `{`,
            // `,` or at line start): a property, not the declaration
            // (`{ smootherstep: smootherstep }`, `interface I { name: T }`).
            if ts.is_some()
                && !is_decl
                && base.is_none()
                && crate::refs_graph_ts::is_object_key(&lines[line], byte)
            {
                members += 1;
                continue;
            }
            // TS/JS top-level declarations: `x.name` is ours only through a
            // module object (`ns.name`, `require(...).name` bound to a local).
            // A declaration made by property assignment (`app.x = function
            // x(`) is reached as `obj.x`; a bare `x` in its file is not it.
            if let Some(t) = &ts {
                let member_kind = matches!(decl.kind, SymbolKind::Method | SymbolKind::Field);
                let reached = match base {
                    Some(b) => {
                        member_kind
                            || index.file_id(path.as_str()).is_some_and(|f| {
                                t.member_reaches(f.0, b, path.as_str() == def_path)
                            })
                    }
                    None => member_kind || t.bare_reaches(path.as_str() == def_path),
                };
                if !reached {
                    members += 1;
                    continue;
                }
            }
            if go && !is_decl && base.is_none() && go_literal_key(&lines[line], byte, name) {
                // `Name: v` in a composite literal is a field key.
                members += 1;
                continue;
            }
            if top_level {
                let reached = match base {
                    Some(b) => ours_locals.contains(b),
                    // Another package names it `pkg.Name` (unless dot-imported).
                    None => dir_of(path.as_str()) == def_dir || dot_import,
                };
                if !reached {
                    members += 1;
                    continue;
                }
                // A function-local binding of the same name shadows it
                // (`verifier := …`, a parameter, `var item T`).
                if base.is_none() && !is_decl {
                    let funcs = funcs.get_or_insert_with(|| {
                        index
                            .file_id(path.as_str())
                            .map(|id| {
                                index
                                    .file_symbols(id)
                                    .filter(|s| {
                                        matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
                                    })
                                    .map(|s| (s.start_line, s.end_line))
                                    .collect()
                            })
                            .unwrap_or_default()
                    });
                    if go_shadowed(&lines, line, byte, name, funcs) {
                        members += 1;
                        continue;
                    }
                }
            }
            if let (Some(m), Some(id)) = (py_member.as_ref(), index.file_id(path.as_str())) {
                if !is_decl {
                    use crate::py_refs::PyVerdict;
                    match m.verdict(id.0, &lines, line, byte) {
                        PyVerdict::Ours => {}
                        PyVerdict::Other => {
                            other_type.push((path.as_str().to_string(), line));
                            continue;
                        }
                        PyVerdict::Bare => {
                            members += 1;
                            continue;
                        }
                        PyVerdict::Unknown => unconfirmed.push((path.as_str().to_string(), line)),
                    }
                }
            }
            if let Some(r) = resolver.as_mut() {
                if !is_decl {
                    // `pkg.Name` through the declaring package's import is
                    // its top-level `Name`, never a method value.
                    if base.is_some_and(|b| ours_locals.contains(b))
                        && !lines[line][..byte - base.map_or(0, str::len) - 1].ends_with('.')
                    {
                        members += 1;
                        continue;
                    }
                    let verdict = match base {
                        Some(b) => {
                            let ty = r.base_type(path.as_str(), line, byte, b);
                            r.verdict(ty)
                        }
                        // `f().M`, `xs[i].M`, `(*T).M`: not resolved.
                        None if lines[line][..byte].ends_with('.') => Recv::Unknown,
                        // A bare name is never a method reference: a local
                        // variable / parameter / label of the same name.
                        None => {
                            members += 1;
                            continue;
                        }
                    };
                    match verdict {
                        Recv::Ours => {}
                        Recv::Other => {
                            other_type.push((path.as_str().to_string(), line));
                            continue;
                        }
                        Recv::Unknown => unconfirmed.push((path.as_str().to_string(), line)),
                    }
                }
            }
            let character = utf16_col(&lines[line], byte);
            let start = Position {
                line: line as u32,
                character,
            };
            let end = Position {
                line: line as u32,
                character: character
                    + alias_units
                        .get(&(line, byte))
                        .copied()
                        .unwrap_or(name_units),
            };
            hits.push(Location {
                path: path.clone(),
                range: Range { start, end },
            });
        }
        if hits.len() > MAX_GRAPH_HITS {
            return None;
        }
    }
    if hits.is_empty() {
        return None;
    }
    let pkg = if def_dir.is_empty() { "." } else { def_dir };
    let scope = if go {
        if exported {
            format!("包 {pkg}/ 内及导入该包的文件")
        } else {
            format!("包 {pkg}/ 内（未导出名）")
        }
    } else if let Some(t) = ts
        .as_ref()
        .filter(|t| t.barrels > 0 || !t.aliases().is_empty())
    {
        let mut s = "定义文件、导入它的文件".to_string();
        if t.barrels > 0 {
            s += &format!("，以及经 {} 个转导出（barrel）文件到达的文件", t.barrels);
        }
        let aliases = t.aliases();
        if !aliases.is_empty() {
            let shown: Vec<String> = aliases.iter().take(5).map(|a| format!("`{a}`")).collect();
            s += &format!("（含改名绑定 {}）", shown.join("、"));
        }
        s
    } else if let Some((scope, via)) = py.as_ref() {
        let mut s = "定义文件及导入它的文件".to_string();
        if !via.is_empty() {
            let shown: Vec<&str> = via.iter().take(3).map(String::as_str).collect();
            s += &format!(
                "，以及经 {} 个转导出模块（{}）到达的文件",
                via.len(),
                shown.join("、")
            );
        }
        let aliases: BTreeSet<&str> = scope
            .values()
            .flatten()
            .map(String::as_str)
            .filter(|a| *a != name)
            .collect();
        if !aliases.is_empty() {
            let shown: Vec<String> = aliases.iter().take(5).map(|a| format!("`{a}`")).collect();
            s += &format!("（含 as 改名 {}）", shown.join("、"));
        }
        if via.truncated {
            s += "（转导出链超过跟踪上限，结果可能不全）";
        }
        s
    } else {
        "定义文件及导入它的文件".to_string()
    };
    let at = format!("@{def_path}:{}", decl.start_line);
    let what = if let Some(recv) = &method_recv {
        format!("`{name}` 是 `{recv}` 的方法，定义 {at}")
    } else if interface_method {
        format!("`{name}` 是接口方法，定义 {at}")
    } else if decls.len() == 1 {
        format!("`{name}` 全仓唯一定义 {at}")
    } else {
        let rest: Vec<&&CodeSymbol> = decls.iter().filter(|s| !std::ptr::eq(**s, decl)).collect();
        let fields = rest.iter().filter(|s| s.kind == SymbolKind::Field).count();
        let mut elsewhere: Vec<String> = rest
            .iter()
            .filter(|s| s.kind != SymbolKind::Field)
            .filter_map(|s| {
                index
                    .file(s.file)
                    .map(|f| format!("{}:{}", f.path.as_str(), s.start_line))
            })
            .take(3)
            .collect();
        let shown_more = rest.len() - fields > 3;
        if shown_more {
            elsewhere.push("…".to_string());
        }
        let fields = if fields > 0 {
            format!("，其中结构体字段 {fields} 处")
        } else {
            String::new()
        };
        let listed = if elsewhere.is_empty() {
            String::new()
        } else {
            format!("：{}", elsewhere.join("、"))
        };
        let by = if how == Choice::CallSite {
            "按 path 所给行的用法（包限定名 / 接收者类型 / 所在包）"
        } else {
            "按 path"
        };
        format!(
            "`{name}` 另有 {} 处同名定义{fields}{listed}；已{by}选定 {at}",
            rest.len()
        )
    };
    let mut note = format!(
        "索引作答（未启动语言服务器）：{what}；以下是{scope}中的代码出现，已排除注释和字符串"
    );
    if method_recv.is_some() {
        note.push_str("，已按接收者类型核对");
    }
    if py.is_some() {
        note.push_str("（字符串形式的类型注解 `x: \"Foo\"` 按代码计）");
    }
    if let Some(m) = &py_member {
        note.push_str(&format!(
            "，`x.{name}` 已按接收者类核对（定义类 `{}` 及其子类；另查了 {} 个未导入定义文件但出现 `.{name}` 的文件）",
            m.class,
            m.extra_files.len()
        ));
    }
    note.push('。');
    if interface_method {
        note.push_str("调用经接口分派，未按接收者类型核对。");
    }
    if !other_type.is_empty() {
        note.push_str(&format!(
            "已排除 {} 处其他类型的同名方法调用：{}。",
            other_type.len(),
            list_hits(&other_type)
        ));
    }
    if members > 0 {
        note.push_str(&format!(
            "已排除 {members} 处同名的其他标识符（字段/方法访问 `x.{name}`、字面量键 `{name}:`、他包未经导入名的裸名、方法同名的局部变量）。"
        ));
    }
    if ts.as_ref().is_some_and(|t| t.truncated) {
        note.push_str("转导出链超过跟踪上限，结果可能不全。");
    }
    if !unconfirmed.is_empty() {
        note.push_str(&format!(
            "其中 {} 处接收者类型无法从索引判定（未核实，可能是其他类型的同名方法）：{}。",
            unconfirmed.len(),
            list_hits(&unconfirmed)
        ));
    }
    Some(DeclRefs {
        answer: Precise {
            value: hits,
            confidence: Confidence::Scoped,
            note: Some(note),
        },
        unconfirmed,
    })
}

/// `find_references` with no usable `path` and a Go name declared in
/// several files (batch 8, E1: `ViaQuietDropped` in two packages): the
/// grouped per-definition answer of [`grouped_references`], when every
/// declaration is Go and their references do not overlap. `None` keeps the
/// "pass one of these as path" answer.
pub(crate) fn grouped_references_without_path(
    index: &RepoIndex,
    root: &Path,
    name: &str,
) -> Option<Precise<Vec<Location>>> {
    let name = name.trim();
    let decls = declarations(index, name, Some(Language::Go));
    let all = declarations(index, name, None);
    if decls.len() < 2 || decls.len() > MAX_GROUPED_WITHOUT_PATH || decls.len() != all.len() {
        return None;
    }
    grouped_references(index, root, name, Language::Go, &decls)
}

/// Most declarations grouped when no `path` was given at all.
const MAX_GROUPED_WITHOUT_PATH: usize = 3;
/// Each declaration's index answer, or `None` when one has none or a hit
/// is claimed by two declarations (undecidable from the index).
fn go_groups<'a>(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    language: Language,
    decls: &[&'a CodeSymbol],
) -> Option<Vec<(&'a CodeSymbol, DeclRefs)>> {
    let mut groups: Vec<(&CodeSymbol, DeclRefs)> = Vec::new();
    for &d in decls {
        groups.push((
            d,
            decl_refs(index, root, name, language, decls, d, Choice::Path)?,
        ));
    }
    let mut owner: HashSet<(String, u32, u32)> = HashSet::new();
    for (_, g) in &groups {
        for loc in &g.answer.value {
            let key = (
                loc.path.as_str().to_string(),
                loc.range.start.line,
                loc.range.start.character,
            );
            if !owner.insert(key) {
                return None;
            }
        }
    }
    Some(groups)
}

/// Test harness view of [`go_groups`]: `(decl path, 1-based decl line,
/// hits)` per declaration of `name`, when they group.
#[cfg(test)]
pub(crate) fn go_groups_for_test(
    index: &RepoIndex,
    root: &Path,
    name: &str,
) -> Option<Vec<(String, u32, Vec<Location>)>> {
    let decls = declarations(index, name, Some(Language::Go));
    if decls.len() < 2 || decls.iter().any(|s| s.kind == SymbolKind::Field) {
        return None;
    }
    let groups = go_groups(index, root, name, Language::Go, &decls)?;
    groups
        .into_iter()
        .map(|(d, g)| {
            Some((
                index.file(d.file)?.path.as_str().to_string(),
                d.start_line,
                g.answer.value,
            ))
        })
        .collect()
}

/// Most same-named Go declarations a grouped answer covers.
const MAX_GROUPED_DECLS: usize = 6;
/// Most hits a non-largest group lists by `path:line` in the note.
const MAX_GROUP_LISTED: usize = 40;

/// Go, several declarations of `name` and nothing in the request picks
/// one (H6: `MarkFlagRequired` is both `(*Command).MarkFlagRequired` and a
/// package function, declared in the very file passed as `path`): each
/// declaration's index answer, merged into one list with a note that says
/// which hit belongs to which declaration. Only when the groups are
/// disjoint (no hit claimed by two declarations): the package qualifier /
/// receiver type / package of every hit separates them. `None` (language
/// server) otherwise, or with a struct field among the declarations
/// (`x.Name` field accesses are not attributed), or with too many.
fn grouped_references(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    language: Language,
    decls: &[&CodeSymbol],
) -> Option<Precise<Vec<Location>>> {
    if decls.len() < 2
        || decls.len() > MAX_GROUPED_DECLS
        || decls.iter().any(|s| s.kind == SymbolKind::Field)
    {
        return None;
    }
    let groups = go_groups(index, root, name, language, decls)?;
    let total: usize = groups.iter().map(|(_, g)| g.answer.value.len()).sum();
    if total > MAX_GRAPH_HITS {
        return None;
    }
    // The largest group is described as "the rest"; the others are listed.
    let largest = groups
        .iter()
        .enumerate()
        .max_by_key(|(i, (_, g))| (g.answer.value.len(), std::cmp::Reverse(*i)))
        .map(|(i, _)| i)?;
    if groups
        .iter()
        .enumerate()
        .any(|(i, (_, g))| i != largest && g.answer.value.len() > MAX_GROUP_LISTED)
    {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut unconfirmed: Vec<(String, usize)> = Vec::new();
    for (i, (d, g)) in groups.iter().enumerate() {
        let path = index.file(d.file)?.path.as_str();
        let kind = match d.kind {
            SymbolKind::Method => match go_receiver_type(&d.signature) {
                Some(r) => format!("`{r}` 的方法，已按接收者类型核对"),
                None => "接口方法".to_string(),
            },
            SymbolKind::Function => "包级函数，只计裸名和 `包名.` 限定的用法".to_string(),
            _ => "包级声明，只计裸名和 `包名.` 限定的用法".to_string(),
        };
        let sig: String = d.signature.trim().chars().take(100).collect();
        let n = g.answer.value.len();
        let list = if i == largest {
            format!("其余 {n} 处都属于它")
        } else {
            let hits: Vec<String> = g
                .answer
                .value
                .iter()
                .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
                .collect();
            format!("{n} 处：{}", hits.join("、"))
        };
        lines.push(format!(
            "  [定义{}] `{sig}` @{path}:{}（{kind}）—— {list}",
            i + 1,
            d.start_line
        ));
        unconfirmed.extend(g.unconfirmed.iter().cloned());
    }
    let mut note = format!(
        "索引作答（未启动语言服务器）：`{name}` 有 {} 处同名定义，path 选不出其中一处；各定义的引用互不重叠，已按定义分组（下方清单是各组合并，每处只属于一个定义，含各自的 [声明]）：\n{}\n以下是各定义所在包内及导入该包的文件中的代码出现，已排除注释和字符串。只要其中一个定义时，把 path 设为它的声明文件并传它的 line。",
        groups.len(),
        lines.join("\n")
    );
    if !unconfirmed.is_empty() {
        unconfirmed.sort();
        note.push_str(&format!(
            "其中 {} 处接收者类型无法从索引判定（未核实，可能是其他类型的同名方法）：{}。",
            unconfirmed.len(),
            list_hits(&unconfirmed)
        ));
    }
    let mut value: Vec<Location> = groups
        .into_iter()
        .flat_map(|(_, g)| g.answer.value)
        .collect();
    value.sort_by(|a, b| {
        (a.path.as_str(), a.range.start.line, a.range.start.character).cmp(&(
            b.path.as_str(),
            b.range.start.line,
            b.range.start.character,
        ))
    });
    Some(Precise {
        value,
        confidence: Confidence::Scoped,
        note: Some(note),
    })
}

// ------------------------------------------------------------ annotate

/// Self-contained description of one hit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HitInfo {
    /// Source line, trimmed and cut to [`HIT_TEXT_CHARS`] chars.
    pub text: Option<String>,
    /// Enclosing function / type: name, first and last line (1-based).
    pub enclosing: Option<(String, u32, u32)>,
    /// The hit is the symbol's own declaration.
    pub declaration: bool,
    pub kind: HitKind,
    /// In a test file or test function.
    pub test: bool,
}

fn enclosing_symbol<'a>(symbols: &[&'a CodeSymbol], line1: u32) -> Option<&'a CodeSymbol> {
    let pick = |callable: bool| {
        symbols
            .iter()
            .copied()
            .filter(|s| s.start_line <= line1 && line1 <= s.end_line)
            .filter(|s| s.end_line >= s.start_line + 2 || s.start_line == line1)
            .filter(|s| !callable || matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .filter(|s| {
                !matches!(
                    s.kind,
                    SymbolKind::Field | SymbolKind::Variable | SymbolKind::Const
                ) || s.end_line > s.start_line + 2
            })
            .min_by_key(|s| s.end_line - s.start_line)
    };
    pick(true).or_else(|| pick(false))
}

/// Describe `locations` (any source: graph, LSP or text fallback) for
/// rendering. Works without an index (no enclosing / declaration info).
pub(crate) fn annotate(
    index: Option<&RepoIndex>,
    root: &Path,
    symbol: &str,
    locations: &[Location],
) -> Vec<HitInfo> {
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, l) in locations.iter().enumerate() {
        by_file.entry(l.path.as_str()).or_default().push(i);
    }
    let mut out = vec![HitInfo::default(); locations.len()];
    for (path, idxs) in by_file {
        let lines = read_lines(root, path).unwrap_or_default();
        let file_symbols: Vec<&CodeSymbol> = index
            .and_then(|ix| {
                ix.file_id(path)
                    .filter(|id| ix.file(*id).is_some_and(|f| f.path.as_str() == path))
                    .map(|id| (ix, id))
            })
            .map(|(ix, id)| ix.graph().symbols.iter().filter(|s| s.file == id).collect())
            .unwrap_or_default();
        let positions: Vec<(usize, usize)> = idxs
            .iter()
            .map(|&i| {
                let p = locations[i].range.start;
                let line = p.line as usize;
                let byte = lines.get(line).map_or(0, |l| byte_of_utf16(l, p.character));
                (line, byte)
            })
            .collect();
        let kinds = if lines.is_empty() {
            vec![HitKind::Code; idxs.len()]
        } else {
            classify(path, &lines, &positions)
        };
        let file_test = crate::explain::is_test_caller(path, "");
        for ((&i, &(line, byte)), kind) in idxs.iter().zip(positions.iter()).zip(kinds) {
            let line1 = line as u32 + 1;
            let text = lines.get(line).map(|l| {
                let t = l.trim();
                let mut s: String = t.chars().take(HIT_TEXT_CHARS).collect();
                if t.chars().count() > HIT_TEXT_CHARS {
                    s.push('…');
                }
                s
            });
            let enclosing = enclosing_symbol(&file_symbols, line1);
            let declaration = kind == HitKind::Code
                && file_symbols.iter().any(|s| {
                    s.name == symbol
                        && s.start_line <= line1
                        && line1
                            <= s.start_line
                                .saturating_add(3)
                                .min(s.end_line.max(s.start_line))
                        && first_occurrence(&lines, symbol, s.start_line, line1)
                            == Some((line, byte))
                });
            let test = file_test
                || enclosing.is_some_and(|s| crate::explain::is_test_caller(path, &s.name));
            out[i] = HitInfo {
                text,
                enclosing: enclosing.map(|s| (s.name.clone(), s.start_line, s.end_line)),
                declaration,
                kind,
                test,
            };
        }
    }
    out
}

/// First whole-word occurrence of `name` from 1-based line `from` through
/// `to`, as `(0-based line, byte)`.
fn first_occurrence(lines: &[String], name: &str, from: u32, to: u32) -> Option<(usize, usize)> {
    let start = from.saturating_sub(1) as usize;
    let end = to.saturating_sub(1) as usize;
    (start..=end).find_map(|l| {
        lines
            .get(l)
            .and_then(|text| word_offsets(text, name).first().map(|&b| (l, b)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-refs-graph-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    fn index(dir: &Path) -> RepoIndex {
        crate::index::build_with_options(dir, false, 1 << 20).expect("index")
    }

    #[test]
    fn go_foreign_imports_and_selector_base() {
        let lines: Vec<String> = "package p\n\nimport \"fmt\"\nimport (\n\tex \"os/exec\"\n\t\"example.com/m/service\"\n\t\"gopkg.in/yaml.v3\"\n\t\"github.com/x/y/v2\"\n)\n\nfunc F() {}\n"
            .lines()
            .map(str::to_string)
            .collect();
        let f = go_foreign_imports(&lines, Some("example.com/m/service"), "service");
        let want: HashSet<String> = ["fmt", "ex", "yaml", "y"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(f, want);
        // Unknown module: same last segment counts as ours.
        let f = go_foreign_imports(&lines, None, "x/service");
        assert!(!f.contains("service") && f.contains("fmt"));
        // Example code in the package doc comment does not end the scan.
        let doc: Vec<String> = "/*\nUsage:\n\tvar png []byte\n\tfunc f() {}\n*/\npackage qrcode\n\nimport (\n\trs \"github.com/skip2/go-qrcode/reedsolomon\"\n)\n"
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            go_imports(&doc),
            vec![(
                "rs".to_string(),
                "github.com/skip2/go-qrcode/reedsolomon".to_string()
            )]
        );
        assert!(go_foreign_imports(&doc, None, "x").contains("rs"));
        assert_eq!(selector_base("\tfmt.Println(x)", 5), Some("fmt"));
        assert_eq!(selector_base("Println(x)", 0), None);
    }

    #[test]
    fn go_import_path_uses_the_nearest_go_mod() {
        let dir = repo(&[
            ("src/go.mod", "module aicon\n\ngo 1.22\n"),
            ("src/server/push/q.go", "package push\n"),
        ]);
        assert_eq!(
            go_import_path(&dir, "src/server/push").as_deref(),
            Some("aicon/server/push")
        );
        assert_eq!(go_import_path(&dir, "src").as_deref(), Some("aicon"));
        assert_eq!(go_import_path(&dir, "other"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn word_offsets_respect_identifier_boundaries() {
        assert_eq!(
            word_offsets("a.Execute() + execute + ExecuteC", "Execute"),
            vec![2]
        );
        assert_eq!(word_offsets("Execute(Execute)", "Execute"), vec![0, 8]);
        assert!(word_offsets("x", "").is_empty());
        assert_eq!(word_offsets("é Execute", "Execute"), vec![3]);
    }

    #[test]
    fn off_by_one_column_snaps_onto_the_symbol() {
        let dir = repo(&[(
            "command.go",
            "package cobra\n\nfunc (c *Command) Execute() error {\n\treturn nil\n}\n",
        )]);
        // 0-based char 17 is the space before `Execute` (18).
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 2,
                character: 17,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 18
            })
        );
        assert!(
            snap.note.as_deref().unwrap().contains("坐标已校正"),
            "{snap:?}"
        );
        // Pointing at `Command` on the same line: symbol wins.
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 2,
                character: 10,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 18
            })
        );
        assert!(
            snap.note.as_deref().unwrap().contains("`Command`"),
            "{snap:?}"
        );
        // Already right: untouched, no note.
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 2,
                character: 20,
            },
        );
        assert_eq!(
            snap,
            Snap {
                position: Some(Position {
                    line: 2,
                    character: 20
                }),
                note: None
            }
        );
        // Two lines off: still found.
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 4,
                character: 0,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 18
            })
        );
        // Nowhere near: by name.
        let snap = snap_position(
            &dir,
            "command.go",
            "Missing",
            Position {
                line: 2,
                character: 0,
            },
        );
        assert_eq!(snap.position, None);
        assert!(snap.note.unwrap().contains("按名字查询"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snap_prefers_code_over_comment_occurrence() {
        let dir = repo(&[(
            "a.go",
            "package a\n\nfunc F() { G() } // G here\n\nfunc G() {}\n",
        )]);
        let snap = snap_position(
            &dir,
            "a.go",
            "G",
            Position {
                line: 2,
                character: 25,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 11
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_wrong_path_is_inferred_from_a_unique_declaration() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("service/channel.go", "package service\n\nfunc ShouldDisableChannel() bool { return false }\n"),
            ("controller/relay.go", "package controller\n\nimport \"example.com/m/service\"\n\nfunc R() { service.ShouldDisableChannel() }\n"),
            ("a/x.go", "package a\n\nfunc Dup() {}\n"),
            ("b/y.go", "package b\n\nfunc Dup() {}\n"),
        ]);
        let ix = index(&dir);
        let fix = fix_path(&ix, &dir, "ShouldDisableChannel", None);
        assert!(
            matches!(&fix, PathFix::Use { path, .. } if path == "service/channel.go"),
            "{fix:?}"
        );
        let fix = fix_path(
            &ix,
            &dir,
            "ShouldDisableChannel",
            Some("relay/channel/disable.go"),
        );
        match fix {
            PathFix::Use { path, note } => {
                assert_eq!(path, "service/channel.go");
                assert!(note.contains("不存在"), "{note}");
            }
            other => panic!("{other:?}"),
        }
        // An existing file that uses the symbol is kept.
        assert_eq!(
            fix_path(
                &ix,
                &dir,
                "ShouldDisableChannel",
                Some("controller/relay.go")
            ),
            PathFix::Keep
        );
        // Existing file without the symbol: unique definition wins.
        assert!(matches!(
            fix_path(&ix, &dir, "ShouldDisableChannel", Some("a/x.go")),
            PathFix::Use { .. }
        ));
        // Ambiguous without a path: candidates listed.
        match fix_path(&ix, &dir, "Dup", None) {
            PathFix::Ambiguous { note } => assert!(
                note.contains("a/x.go:3") && note.contains("b/y.go:3"),
                "{note}"
            ),
            other => panic!("{other:?}"),
        }
        // Unknown names: keep (the server path reports it).
        assert_eq!(fix_path(&ix, &dir, "Nope", None), PathFix::Keep);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_graph_references_scope_to_package_and_importers() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("service/channel.go", "package service\n\n// ShouldDisableChannel decides.\nfunc ShouldDisableChannel() bool { return false }\n\nfunc other() { _ = ShouldDisableChannel() }\n"),
            ("controller/relay.go", "package controller\n\nimport \"example.com/m/service\"\n\nfunc R() {\n\tif service.ShouldDisableChannel() {\n\t\tprintln(\"ShouldDisableChannel\")\n\t}\n}\n"),
            // Same name, package that does not import service: not ours.
            ("controller/log.go", "package controller\n\nimport (\n\tsvc \"example.com/m/service\"\n\tstrs \"example.com/ext/strs\"\n)\n\nfunc L() { strs.ShouldDisableChannel(); svc.ShouldDisableChannel() }\n"),
            ("other/z.go", "package other\n\nimport \"ext.com/x\"\n\nfunc Z(t *x.T) { t.ShouldDisableChannel() }\n"),
        ]);
        let ix = index(&dir);
        let r = graph_references(
            &ix,
            &dir,
            "ShouldDisableChannel",
            &RelPath::new("service/channel.go"),
        );
        let r = r.expect("unique declaration");
        let got: Vec<String> = r
            .value
            .iter()
            .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
            .collect();
        // `strs.ShouldDisableChannel` is another package's; `svc.` is ours.
        assert_eq!(
            got,
            vec![
                "controller/log.go:8",
                "controller/relay.go:6",
                "service/channel.go:4",
                "service/channel.go:6"
            ]
        );
        assert_eq!(r.confidence, Confidence::Scoped);
        assert!(r.note.unwrap().contains("未启动语言服务器"));
        // Two declarations: the path picks one; a path that reaches
        // neither defers to the language server.
        let dir2 = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("a/x.go", "package a\n\nfunc Dup() {}\n"),
            ("b/y.go", "package b\n\nfunc Dup() {}\n"),
            ("c/z.go", "package c\n\nfunc Z() {}\n"),
        ]);
        let ix2 = index(&dir2);
        let r = graph_references(&ix2, &dir2, "Dup", &RelPath::new("a/x.go")).expect("by path");
        assert_eq!(r.value.len(), 1);
        assert_eq!(r.value[0].path.as_str(), "a/x.go");
        // A path that reaches neither: grouped per declaration (batch 8).
        let r = graph_references(&ix2, &dir2, "Dup", &RelPath::new("c/z.go")).expect("grouped");
        assert_eq!(hit_lines(&r), vec!["a/x.go:3", "b/y.go:3"]);
        assert!(r.note.unwrap().contains("已按定义分组"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn annotate_marks_declaration_enclosing_comment_string_and_tests() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("p/a.go", "package p\n\n// Run starts.\nfunc Run() {\n}\n\nfunc caller() {\n\tRun()\n\ts := \"Run\"\n\t_ = s\n}\n"),
            ("p/a_test.go", "package p\n\nimport \"testing\"\n\nfunc TestRun(t *testing.T) {\n\tRun()\n}\n"),
        ]);
        let ix = index(&dir);
        let at = |path: &str, line: u32, character: u32| Location {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character },
                end: Position {
                    line,
                    character: character + 3,
                },
            },
        };
        let locs = vec![
            at("p/a.go", 2, 3),
            at("p/a.go", 3, 5),
            at("p/a.go", 7, 1),
            at("p/a.go", 8, 7),
            at("p/a_test.go", 5, 1),
        ];
        let info = annotate(Some(&ix), &dir, "Run", &locs);
        assert_eq!(info[0].kind, HitKind::Comment);
        assert!(info[1].declaration, "{:?}", info[1]);
        assert_eq!(info[1].text.as_deref(), Some("func Run() {"));
        assert!(!info[2].declaration);
        assert_eq!(info[2].enclosing, Some(("caller".to_string(), 7, 11)));
        assert_eq!(info[3].kind, HitKind::Str);
        assert!(info[4].test && !info[2].test);
        assert_eq!(
            info[4].enclosing.as_ref().map(|e| e.0.as_str()),
            Some("TestRun")
        );
        // Without an index: text and kinds still there.
        let bare = annotate(None, &dir, "Run", &locs);
        assert_eq!(bare[0].kind, HitKind::Comment);
        assert!(bare[2].enclosing.is_none() && bare[2].text.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Manual probe on a real tree: `ASTRO_PROBE=root|path|symbol[|line]`.
    #[test]
    #[ignore = "manual probe on a local corpus"]
    fn probe_graph_references() {
        let Ok(spec) = std::env::var("ASTRO_PROBE") else {
            return;
        };
        let parts: Vec<&str> = spec.split('|').collect();
        let root = Path::new(parts[0]);
        let ix = index(root);
        let line = parts.get(3).and_then(|l| l.parse().ok());
        let r = graph_references_at(&ix, root, parts[2], &RelPath::new(parts[1]), line);
        match r {
            None => println!("PROBE none"),
            Some(r) => {
                println!("PROBE note: {}", r.note.unwrap_or_default());
                for l in r.value {
                    println!("PROBE {}:{}", l.path.as_str(), l.range.start.line + 1);
                }
            }
        }
    }

    /// Manual sweep: every Go method of `ASTRO_SWEEP=root`, with its
    /// graph note (receiver verdicts) and hit count.
    #[test]
    #[ignore = "manual sweep on a local corpus"]
    fn sweep_go_methods() {
        let Ok(root) = std::env::var("ASTRO_SWEEP") else {
            return;
        };
        let root = Path::new(&root);
        let ix = index(root);
        let mut seen = HashSet::new();
        for s in ix
            .graph()
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
        {
            let Some(f) = ix.file(s.file) else { continue };
            if f.language != Some(Language::Go) || !seen.insert((s.file, s.name.clone())) {
                continue;
            }
            let r = graph_references_at(&ix, root, &s.name, &f.path, Some(s.start_line - 1));
            match r {
                None => println!("SWEEP\t{}\t{}\tnone", f.path.as_str(), s.name),
                Some(r) => println!(
                    "SWEEP\t{}\t{}\t{}\t{}",
                    f.path.as_str(),
                    s.name,
                    r.value.len(),
                    r.note.unwrap_or_default()
                ),
            }
        }
    }

    fn hit_lines(r: &Precise<Vec<Location>>) -> Vec<String> {
        r.value
            .iter()
            .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
            .collect()
    }

    /// Batch 6 N1: `(*Command).Execute` vs text/template's `t.Execute`,
    /// another local type's own `Execute`, and receivers the index cannot
    /// type.
    #[test]
    fn go_method_refs_are_filtered_by_receiver_type() {
        let dir = repo(&[
            ("go.mod", "module github.com/spf13/cobra\n"),
            (
                "command.go",
                "package cobra\n\ntype Command struct {\n\tparent *Command\n}\n\nfunc (c *Command) ExecuteContext() error {\n\treturn c.Execute()\n}\n\nfunc (c *Command) Execute() error {\n\treturn nil\n}\n\nfunc (c *Command) Root() *Command { return c }\n",
            ),
            (
                "cobra.go",
                "package cobra\n\nimport (\n\t\"io\"\n\t\"text/template\"\n)\n\nfunc tmpl(text string) func(io.Writer, any) error {\n\treturn func(w io.Writer, data any) error {\n\t\tt := template.New(\"top\")\n\t\treturn t.Execute(w, data)\n\t}\n}\n",
            ),
            (
                "runner.go",
                "package cobra\n\ntype runner struct{}\n\nfunc (r *runner) Execute() error { return nil }\n\nfunc useRunner() {\n\tr := &runner{}\n\t_ = r.Execute()\n}\n",
            ),
            (
                "command_test.go",
                "package cobra\n\nimport \"testing\"\n\nfunc TestRoot(t *testing.T) {\n\troot := &Command{}\n\t// root.Execute() in a comment\n\tif err := root.Execute(); err != nil {\n\t\tt.Fatal(err)\n\t}\n\tgetCmd := func() *Command {\n\t\treturn &Command{}\n\t}\n\tc := getCmd()\n\t_ = c.Execute()\n\tp := root.parent\n\t_ = p.Execute()\n\tvar v *Command\n\t_ = v.Execute()\n\tw := root.Root()\n\t_ = w.Execute()\n\tfor _, x := range all {\n\t\t_ = x.Execute()\n\t}\n}\n\nvar all []*Command\n\nfunc helper(Execute func() error) error {\n\treturn Execute()\n}\n",
            ),
        ]);
        let ix = index(&dir);
        let r = graph_references(&ix, &dir, "Execute", &RelPath::new("command.go"))
            .expect("chosen by path");
        assert_eq!(
            hit_lines(&r),
            vec![
                "command.go:8",
                "command.go:11",
                "command_test.go:8",
                "command_test.go:15",
                "command_test.go:17",
                "command_test.go:19",
                "command_test.go:21",
                "command_test.go:23",
            ]
        );
        let note = r.note.unwrap();
        assert!(note.contains("是 `Command` 的方法"), "{note}");
        assert!(!note.contains("全仓唯一"), "{note}");
        assert!(note.contains("已排除 2 处其他类型的同名方法调用"), "{note}");
        assert!(
            note.contains("cobra.go:11") && note.contains("runner.go:9"),
            "{note}"
        );
        // `helper`'s parameter named `Execute`: a bare name, not the method.
        assert!(note.contains("已排除 2 处同名的其他标识符"), "{note}");
        // A `range` variable is not typed: kept, flagged unconfirmed.
        assert!(note.contains("1 处接收者类型无法从索引判定"), "{note}");
        assert!(note.contains("command_test.go:23"), "{note}");
        // The other type's method, chosen by its file.
        let r = graph_references(&ix, &dir, "Execute", &RelPath::new("runner.go")).unwrap();
        assert_eq!(
            hit_lines(&r),
            vec!["command_test.go:23", "runner.go:5", "runner.go:9"]
        );
        assert!(r.note.unwrap().contains("是 `runner` 的方法"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two methods of one name in one file: the caller's line picks.
    #[test]
    fn go_same_file_methods_are_picked_by_line() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            (
                "p/a.go",
                "package p\n\ntype A struct{}\ntype B struct{}\n\nfunc (a A) Name() string { return \"a\" }\n\nfunc (b B) Name() string { return \"b\" }\n\nfunc use() {\n\tx := A{}\n\ty := B{}\n\t_ = x.Name() + y.Name()\n}\n",
            ),
        ]);
        let ix = index(&dir);
        let origin = RelPath::new("p/a.go");
        // No line: two candidates in the file; their hits separate by
        // receiver type → one grouped answer (batch 8), no language server.
        let r = graph_references(&ix, &dir, "Name", &origin).expect("grouped");
        assert_eq!(
            hit_lines(&r),
            vec!["p/a.go:6", "p/a.go:8", "p/a.go:13", "p/a.go:13"]
        );
        assert!(r.note.unwrap().contains("已按定义分组"));
        // A line whose two uses resolve to different methods picks
        // neither: still the grouped answer.
        let r = graph_references_at(&ix, &dir, "Name", &origin, Some(12)).expect("line 13");
        assert!(r.note.unwrap().contains("已按定义分组"));
        let r = graph_references_at(&ix, &dir, "Name", &origin, Some(7)).expect("line 8");
        assert_eq!(hit_lines(&r), vec!["p/a.go:8", "p/a.go:13"]);
        assert_eq!(r.value[1].range.start.character, 18);
        let r = graph_references_at(&ix, &dir, "Name", &origin, Some(5)).expect("line 6");
        assert_eq!(hit_lines(&r), vec!["p/a.go:6", "p/a.go:13"]);
        assert_eq!(r.value[1].range.start.character, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Batch 8 H6: cobra's `MarkFlagRequired` is a `*Command` method and a
    /// package function in the same file, and that file was passed as
    /// `path` with no line. gopls was started 3/3 runs.
    fn h6_repo() -> std::path::PathBuf {
        repo(&[
            ("go.mod", "module github.com/spf13/cobra\n"),
            (
                "shell_completions.go",
                "package cobra\n\nimport \"github.com/spf13/pflag\"\n\ntype Command struct{}\n\nfunc (c *Command) Flags() *pflag.FlagSet { return nil }\n\n// MarkFlagRequired marks.\nfunc (c *Command) MarkFlagRequired(name string) error {\n\treturn MarkFlagRequired(c.Flags(), name)\n}\n\n// MarkFlagRequired marks.\nfunc MarkFlagRequired(flags *pflag.FlagSet, name string) error {\n\treturn nil\n}\n",
            ),
            (
                "flag_groups.go",
                "package cobra\n\nfunc (c *Command) enforce() {\n\t_ = c.MarkFlagRequired(\"a\")\n}\n",
            ),
            (
                "command_test.go",
                "package cobra\n\nimport \"testing\"\n\nfunc TestReq(t *testing.T) {\n\tc := &Command{}\n\t_ = c.MarkFlagRequired(\"foo\")\n}\n",
            ),
            (
                "app/main.go",
                "package main\n\nimport (\n\t\"github.com/spf13/cobra\"\n\t\"github.com/spf13/pflag\"\n)\n\nfunc main() {\n\tcmd := &cobra.Command{}\n\t_ = cmd.MarkFlagRequired(\"x\")\n\tvar fs *pflag.FlagSet\n\t_ = cobra.MarkFlagRequired(fs, \"y\")\n}\n",
            ),
        ])
    }

    #[test]
    fn go_h6_same_file_method_and_function_are_grouped() {
        let dir = h6_repo();
        let ix = index(&dir);
        let origin = RelPath::new("shell_completions.go");
        let r = graph_references(&ix, &dir, "MarkFlagRequired", &origin).expect("grouped");
        assert_eq!(
            hit_lines(&r),
            vec![
                "app/main.go:10",
                "app/main.go:12",
                "command_test.go:7",
                "flag_groups.go:4",
                "shell_completions.go:10",
                "shell_completions.go:11",
                "shell_completions.go:15",
            ]
        );
        let note = r.note.clone().unwrap();
        assert!(
            note.contains("索引作答") && note.contains("已按定义分组"),
            "{note}"
        );
        assert!(
            note.contains("[定义1]") && note.contains("`Command` 的方法"),
            "{note}"
        );
        // The function's group is listed hit by hit, the method's is "the rest".
        assert!(
            note.contains("3 处：app/main.go:12、shell_completions.go:11、shell_completions.go:15"),
            "{note}"
        );
        assert!(note.contains("其余 4 处都属于它"), "{note}");
        // Complete by construction: the "已全列" line applies.
        assert!(
            crate::refs_render::complete_source(r.confidence, r.note.as_deref()).is_some(),
            "{note}"
        );
        // Call sites pick one declaration.
        let at = |origin: &str, line: u32| {
            let r = graph_references_at(
                &ix,
                &dir,
                "MarkFlagRequired",
                &RelPath::new(origin),
                Some(line),
            )
            .expect("picked");
            let note = r.note.clone().unwrap_or_default();
            assert!(!note.contains("已按定义分组"), "{note}");
            hit_lines(&r)
        };
        let function = vec![
            "app/main.go:12",
            "shell_completions.go:11",
            "shell_completions.go:15",
        ];
        let method = vec![
            "app/main.go:10",
            "command_test.go:7",
            "flag_groups.go:4",
            "shell_completions.go:10",
        ];
        // Bare call inside the method's own body: the package function.
        assert_eq!(at("shell_completions.go", 10), function);
        assert_eq!(at("app/main.go", 11), function, "cobra.MarkFlagRequired");
        assert_eq!(at("flag_groups.go", 3), method, "c.MarkFlagRequired");
        assert_eq!(at("app/main.go", 9), method, "cmd := &cobra.Command{{}}");
        // Declaration lines still pick their own declaration.
        assert_eq!(at("shell_completions.go", 14), function);
        assert_eq!(at("shell_completions.go", 9), method);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hit two declarations could both claim (an interface method and a
    /// concrete one, receiver not typable): the index cannot group it, the
    /// language server decides.
    #[test]
    fn go_overlapping_groups_defer_to_the_server() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            (
                "p/a.go",
                "package p\n\ntype Runner interface {\n\tRun() error\n}\n\ntype T struct{}\n\nfunc (t *T) Run() error { return nil }\n\nfunc use(rs []Runner) {\n\tfor _, r := range rs {\n\t\t_ = r.Run()\n\t}\n}\n",
            ),
            ("q/b.go", "package q\n\nfunc Z() {}\n"),
        ]);
        let ix = index(&dir);
        assert!(graph_references(&ix, &dir, "Run", &RelPath::new("q/b.go")).is_none());
        assert!(grouped_references_without_path(&ix, &dir, "Run").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Batch 8 E1 r2: no path, `ViaQuietDropped` declared in two packages.
    #[test]
    fn go_no_path_several_packages_is_grouped() {
        let dir = repo(&[
            ("src/go.mod", "module aicon\n"),
            (
                "src/server/srv/push/quiet.go",
                "package push\n\nconst (\n\tViaQuietDropped = \"quiet_dropped\"\n)\n\nfunc q() string { return ViaQuietDropped }\n",
            ),
            (
                "src/daemon/internal/signaling/wake.go",
                "package signaling\n\nconst ViaQuietDropped = \"dropped\"\n\nfunc W() string { return ViaQuietDropped }\n",
            ),
            (
                "src/server/api/h.go",
                "package api\n\nimport \"aicon/server/srv/push\"\n\nfunc H() string { return push.ViaQuietDropped }\n",
            ),
        ]);
        let ix = index(&dir);
        let r = grouped_references_without_path(&ix, &dir, "ViaQuietDropped").expect("grouped");
        assert_eq!(
            hit_lines(&r),
            vec![
                "src/daemon/internal/signaling/wake.go:3",
                "src/daemon/internal/signaling/wake.go:5",
                "src/server/api/h.go:5",
                "src/server/srv/push/quiet.go:4",
                "src/server/srv/push/quiet.go:7",
            ]
        );
        let note = r.note.unwrap();
        assert!(
            note.contains("有 2 处同名定义") && note.contains("[定义2]"),
            "{note}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Batch 6 E1 / N3: the name is declared in two packages and the path
    /// says which; gopls used to be started for these.
    #[test]
    fn go_duplicate_declarations_are_disambiguated_by_path() {
        let dir = repo(&[
            ("src/go.mod", "module aicon\n"),
            (
                "src/server/srv/push/quiet.go",
                "package push\n\nconst (\n\tViaQuietDropped = \"quiet_dropped\"\n)\n",
            ),
            (
                "src/server/srv/push/quiet_test.go",
                "package push_test\n\nimport (\n\t\"testing\"\n\n\t\"aicon/server/srv/push\"\n)\n\nfunc TestQ(t *testing.T) {\n\tif push.ViaQuietDropped != \"quiet_dropped\" {\n\t\tt.Fatal(push.ViaQuietDropped)\n\t}\n}\n",
            ),
            (
                "src/daemon/internal/signaling/wake.go",
                "package signaling\n\nconst ViaQuietDropped = \"dropped\"\n\nfunc W() string { return ViaQuietDropped }\n",
            ),
            (
                "src/server/api/h.go",
                "package api\n\nimport \"aicon/server/srv/push\"\n\nfunc H() string { return push.ViaQuietDropped }\n",
            ),
        ]);
        let ix = index(&dir);
        let r = graph_references(
            &ix,
            &dir,
            "ViaQuietDropped",
            &RelPath::new("src/server/srv/push/quiet.go"),
        )
        .expect("path picks push");
        assert_eq!(
            hit_lines(&r),
            vec![
                "src/server/api/h.go:5",
                "src/server/srv/push/quiet.go:4",
                "src/server/srv/push/quiet_test.go:10",
                "src/server/srv/push/quiet_test.go:11",
            ]
        );
        let note = r.note.unwrap();
        assert!(note.contains("另有 1 处同名定义"), "{note}");
        assert!(
            note.contains("src/daemon/internal/signaling/wake.go:3"),
            "{note}"
        );
        assert!(!note.contains("全仓唯一"), "{note}");
        // A caller file: the declaration in the package it imports.
        let r = graph_references(
            &ix,
            &dir,
            "ViaQuietDropped",
            &RelPath::new("src/server/api/h.go"),
        )
        .expect("imported package");
        assert!(hit_lines(&r).contains(&"src/server/srv/push/quiet.go:4".to_string()));
        // The other package's own.
        let r = graph_references(
            &ix,
            &dir,
            "ViaQuietDropped",
            &RelPath::new("src/daemon/internal/signaling/wake.go"),
        )
        .unwrap();
        assert_eq!(
            hit_lines(&r),
            vec![
                "src/daemon/internal/signaling/wake.go:3",
                "src/daemon/internal/signaling/wake.go:5"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N3 shape: an interface type also used as a struct field name and a
    /// composite-literal key; only the type's uses are references.
    #[test]
    fn go_type_refs_skip_same_named_fields_and_keys() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            (
                "oauth/provider.go",
                "package oauth\n\ntype Provider interface {\n\tName() string\n}\n\nvar providers = map[string]Provider{}\n",
            ),
            (
                "providerauth/types.go",
                "package providerauth\n\ntype Provider interface {\n\tID() string\n}\n",
            ),
            (
                "controller/oauth.go",
                "package controller\n\nimport \"example.com/m/oauth\"\n\ntype req struct {\n\tProvider string\n}\n\nfunc bind(p oauth.Provider, r req) req {\n\tr.Provider = p.Name()\n\treturn req{Provider: r.Provider}\n}\n",
            ),
        ]);
        let ix = index(&dir);
        let r = graph_references(&ix, &dir, "Provider", &RelPath::new("oauth/provider.go"))
            .expect("path picks oauth");
        assert_eq!(
            hit_lines(&r),
            vec![
                "controller/oauth.go:9",
                "oauth/provider.go:3",
                "oauth/provider.go:7"
            ]
        );
        let note = r.note.unwrap();
        assert!(note.contains("其中结构体字段 1 处"), "{note}");
        assert!(note.contains("已排除 3 处同名的其他标识符"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_signature_helpers() {
        assert_eq!(
            go_receiver_type("func (c *Command) Execute() error").as_deref(),
            Some("Command")
        );
        assert_eq!(
            go_receiver_type("func (s *Set[T]) Add(v T)").as_deref(),
            Some("Set")
        );
        assert_eq!(
            go_receiver_type("func (Command) X()").as_deref(),
            Some("Command")
        );
        assert_eq!(go_receiver_type("Execute() error"), None);
        assert_eq!(go_receiver_type("func Execute() error"), None);
        let parts = go_func_parts("\tRun: func(cmd *Command, args []string) {");
        assert_eq!(parts.len(), 1);
        assert_eq!(go_param_type(parts[0].params, "cmd"), Some("*Command"));
        assert_eq!(go_param_type(parts[0].params, "args"), Some("[]string"));
        assert_eq!(go_param_type("a, b *T, n int", "a"), Some("*T"));
        assert_eq!(go_param_type("a, b *T, n int", "n"), Some("int"));
        assert_eq!(go_param_type("int, string", "int"), None);
        let parts =
            go_func_parts("func (c *Command) Find(args []string) (*Command, []string, error) {");
        assert_eq!(parts[0].receiver, Some("c *Command"));
        assert_eq!(go_result_type(parts[0].results, 0), Some("*Command"));
        assert_eq!(go_result_type(parts[0].results, 2), Some("error"));
        assert_eq!(
            go_result_type("(c *Command, err error)", 0),
            Some("*Command")
        );
        assert_eq!(
            go_type_name("*template.Template"),
            Some((Some("template".into()), "Template".into()))
        );
        assert_eq!(go_type_name("[]string"), None);
        assert!(go_literal_key(
            "\treturn req{Provider: r.Provider}",
            12,
            "Provider"
        ));
        assert!(go_literal_key("\t\tProvider: name,", 2, "Provider"));
        assert!(!go_literal_key("\tcase Provider:", 6, "Provider"));
        assert!(!go_literal_key("\tx := Provider", 6, "Provider"));
    }
}
