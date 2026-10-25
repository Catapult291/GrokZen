//! Shell / Bash (`.sh`, `.bash`, shebang) extraction hooks (lang-batch-8).
//!
//! Owned by the shell worker; see `LANG-PLAN.md`. `parse/mod.rs` calls these
//! hooks for [`Language::Shell`] only, so all language-specific post-processing
//! lives here and never in shared code.
//!
//! Shell has no namespaces, so names stay unqualified. The only refinement is
//! the kind of file-scope variables: `readonly X=1` and `declare -r X=1`
//! (`typeset -r`, combined flags like `-gr`) are constants.

use crate::types::{Language, SymbolKind};

#[allow(dead_code)]
const LANG: Language = Language::Shell;

/// `\\. FILE` (nvm's alias-proof `source`) at the start of a line that
/// follows another statement is mis-parsed by tree-sitter-bash 0.25: the
/// external scanner reads `\\` + newline context as a continuation and glues
/// `\\.` onto the previous command as plain arguments, so the `source` is
/// lost. Rewrite a line-leading `\\.` followed by whitespace to ` .` — same
/// byte length, so every line / column the extractor reports is unchanged.
/// Returns `None` when there is nothing to rewrite.
pub(super) fn normalize_backslash_dot(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    let mut line_start = 0usize;
    while line_start < bytes.len() {
        let mut i = line_start;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'\\')
            && bytes.get(i + 1) == Some(&b'.')
            && matches!(bytes.get(i + 2), Some(b' ' | b'\t'))
        {
            out.get_or_insert_with(|| bytes.to_vec())[i] = b' ';
        }
        line_start = match bytes[i..].iter().position(|&b| b == b'\n') {
            Some(n) => i + n + 1,
            None => bytes.len(),
        };
    }
    // Only an ASCII byte was replaced by another, so UTF-8 stays valid.
    out.map(|v| String::from_utf8(v).expect("ascii-for-ascii rewrite"))
}

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    if kind == SymbolKind::Variable && is_readonly(definition, source) {
        return (name.to_owned(), SymbolKind::Const);
    }
    (name.to_owned(), kind)
}

/// `true` when the assignment sits in `readonly …` or `declare/typeset -…r…`.
fn is_readonly(assignment: tree_sitter::Node<'_>, source: &str) -> bool {
    let Some(decl) = assignment.parent() else {
        return false;
    };
    if decl.kind() != "declaration_command" {
        return false;
    }
    let bytes = source.as_bytes();
    let mut cursor = decl.walk();
    let mut keyword = "";
    for child in decl.children(&mut cursor) {
        let text = child.utf8_text(bytes).unwrap_or("");
        if !child.is_named() && keyword.is_empty() {
            keyword = text;
            if keyword == "readonly" {
                return true;
            }
            continue;
        }
        if child.kind() == "word"
            && matches!(keyword, "declare" | "typeset")
            && text.starts_with('-')
            && text[1..].contains('r')
        {
            return true;
        }
    }
    false
}

/// Whether a definition is visible outside its file. Sourcing a shell file
/// shares every function and file-scope variable, so everything is.
pub(super) fn exported(_name: &str, _node: tree_sitter::Node<'_>, _source: &str) -> bool {
    true
}

// ------------------------------------------------------------- pre-pass

/// Sentinel function name a `.bats` `@test "…" {` header is rewritten to.
const BATS_SENTINEL: &str = "_BT_";

/// Shell entry point used by `ParserPool::parse`: zsh sanitising and `.bats`
/// test blocks are rewritten (line- and length-preserving) before the bash
/// grammar sees the text; results are mapped back to the original source.
pub(super) fn parse_with(
    path: &str,
    source: &str,
    mut raw: impl FnMut(&str) -> Result<super::ParsedFile, super::ParseError>,
) -> Result<super::ParsedFile, super::ParseError> {
    let pre = preprocess(path, source);
    if pre.text == source {
        let mut parsed = raw(source)?;
        recover_headers(&pre, source, &mut parsed, &mut raw);
        recover_file_scope(&pre, source, &mut parsed, &mut raw);
        return Ok(parsed);
    }
    let mut parsed = raw(&pre.text)?;
    if pre.zsh {
        // Import specifiers keep their original spelling (`${0:A:h}/x.zsh`).
        if let Ok(orig) = raw(source) {
            parsed.imports = orig.imports;
        }
    }
    let lines: Vec<&str> = source.lines().collect();
    let extra: Vec<crate::types::CodeSymbol> = Vec::new();
    for symbol in &mut parsed.symbols {
        let row = symbol.start_line;
        if symbol.name == BATS_SENTINEL {
            if let Some((_, name)) = pre.tests.iter().find(|(l, _)| *l == row) {
                symbol.name = name.clone();
                symbol.kind = SymbolKind::Function;
            }
        }
        if let Some(orig) = lines.get((row as usize).wrapping_sub(1)) {
            let rewritten = pre.text.lines().nth(row as usize - 1).unwrap_or("");
            if rewritten != *orig {
                symbol.signature = super::signature_from_row(orig.trim_start(), 0);
            }
        }
    }
    parsed.symbols.retain(|s| s.name != BATS_SENTINEL);
    parsed.symbols.extend(extra);
    recover_headers(&pre, source, &mut parsed, &mut raw);
    recover_file_scope(&pre, source, &mut parsed, &mut raw);
    expand_multi_names(&pre, &mut parsed);
    parsed.symbols.sort_by_key(|s| (s.start_line, s.end_line));
    Ok(parsed)
}

/// Column-0 function header (`name() …`, `function name …`) on this line.
fn header_name(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("function ") {
        let name = rest.trim_start().split([' ', '\t', '(', '{']).next()?;
        return (!name.is_empty() && !name.starts_with('$')).then_some(name);
    }
    let (name, rest) = line.split_once('(')?;
    let name = name.trim_end();
    let ok = !name.is_empty()
        && !name.starts_with(['$', '-', '#'])
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.' | b'@' | b'+')
        });
    (ok && rest.trim_start().starts_with(')')).then_some(name)
}

/// zsh `function a b c {`: bash only saw `a`; add `b`, `c` on their own
/// lines (continuation lines keep each name's row) with `a`'s body span.
fn expand_multi_names(pre: &Pre, parsed: &mut super::ParsedFile) {
    let mut extra = Vec::new();
    for (line, names) in &pre.multi_names {
        let Some((first, _)) = names.first() else {
            continue;
        };
        let Some(base) = parsed
            .symbols
            .iter()
            .find(|s| s.start_line == *line && &s.name == first)
        else {
            continue;
        };
        for (name, row) in &names[1..] {
            let mut clone = base.clone();
            clone.name = name.clone();
            clone.start_line = *row;
            clone.end_line = clone.end_line.max(*row);
            extra.push(clone);
        }
    }
    parsed.symbols.extend(extra);
    parsed.symbols.sort_by_key(|s| (s.start_line, s.end_line));
}

/// File-scope assignments in the gaps between functions that an earlier
/// grammar error hid (`gallois.zsh-theme`): re-parse each such gap alone.
fn recover_file_scope(
    pre: &Pre,
    source: &str,
    parsed: &mut super::ParsedFile,
    raw: &mut impl FnMut(&str) -> Result<super::ParsedFile, super::ParseError>,
) {
    let lines: Vec<&str> = pre.text.split('\n').collect();
    let code = code_line_starts(&pre.text);
    let mut covered = vec![false; lines.len()];
    let mut have = vec![false; lines.len()];
    for s in &parsed.symbols {
        let a = (s.start_line as usize).saturating_sub(1);
        if let Some(h) = have.get_mut(a) {
            *h = true;
        }
        if s.kind == SymbolKind::Function {
            let b = (s.end_line as usize).min(lines.len());
            for c in covered.iter_mut().take(b).skip(a) {
                *c = true;
            }
        }
    }
    let wanted =
        |r: usize| !covered[r] && !have[r] && code.get(r) == Some(&true) && looks_assign(lines[r]);
    let orig: Vec<&str> = source.lines().collect();
    let mut r = 0;
    while r < lines.len() {
        if covered[r] {
            r += 1;
            continue;
        }
        let start = r;
        while r < lines.len() && !covered[r] {
            r += 1;
        }
        if !(start..r).any(wanted) {
            continue;
        }
        let mut chunk = "\n".repeat(start);
        chunk.push_str(&lines[start..r].join("\n"));
        let Ok(part) = raw(&chunk) else { continue };
        for mut symbol in part.symbols {
            let row = (symbol.start_line as usize).wrapping_sub(1);
            if symbol.kind == SymbolKind::Function || row < start || row >= r || have[row] {
                continue;
            }
            if let Some(o) = orig.get(row) {
                symbol.signature = super::signature_from_row(o.trim_start(), 0);
            }
            parsed.symbols.push(symbol);
        }
    }
    parsed.symbols.sort_by_key(|s| (s.start_line, s.end_line));
}

/// Column-0 `NAME=…` / `export|declare|typeset|readonly …=…` line.
fn looks_assign(l: &str) -> bool {
    let w = l.split(['=', ' ']).next().unwrap_or("");
    l.contains('=')
        && (matches!(w, "export" | "declare" | "typeset" | "readonly")
            || (!w.is_empty() && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')))
}

/// Per line: does it start outside any string / heredoc body? Headers found
/// inside a multi-line `'…'` (embedded awk, `eval '…'`) are not shell.
fn code_line_starts(text: &str) -> Vec<bool> {
    let b = text.as_bytes();
    let mut starts = vec![true];
    let (mut sq, mut dq, mut comment) = (false, false, false);
    // `$'…'` (ANSI-C quoting: `\'` is an escaped quote) and `((…))` / `$((…))`
    // arithmetic, where `<<` is a left shift and not a heredoc.
    let mut ansi = false;
    let mut arith = 0u32;
    let mut heredoc: Option<Vec<u8>> = None;
    let mut pending: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            comment = false;
            if heredoc.is_none() {
                heredoc = pending.take();
            }
            let line_end = b[i + 1..]
                .iter()
                .position(|&x| x == b'\n')
                .map_or(b.len(), |p| i + 1 + p);
            if let Some(d) = &heredoc {
                starts.push(false);
                if b[i + 1..line_end].trim_ascii() == d.as_slice() {
                    heredoc = None;
                }
                i += 1;
                if heredoc.is_some() {
                    i = line_end;
                }
                continue;
            }
            starts.push(!sq && !dq);
            i += 1;
            continue;
        }
        if heredoc.is_some() || comment {
            i += 1;
            continue;
        }
        if sq {
            if ansi && c == b'\\' && b.get(i + 1).is_some_and(|&n| n != b'\n') {
                i += 1;
            } else {
                sq = c != b'\'';
                ansi &= sq;
            }
        } else if c == b'\\' {
            i += 1;
        } else if dq {
            dq = c != b'"';
        } else {
            match c {
                b'\'' => {
                    sq = true;
                    ansi = i > 0 && b[i - 1] == b'$';
                }
                b'"' => dq = true,
                b'#' if is_word_start(b, i) => comment = true,
                b'(' if b.get(i + 1) == Some(&b'(') => {
                    arith += 1;
                    i += 2;
                    continue;
                }
                b')' if arith > 0 && b.get(i + 1) == Some(&b')') => {
                    arith -= 1;
                    i += 2;
                    continue;
                }
                b'<' if arith > 0 && b[i..].starts_with(b"<<") => {
                    i += 2;
                    continue;
                }
                b'<' if b[i..].starts_with(b"<<") && !b[i..].starts_with(b"<<<") => {
                    let mut j = i + 2;
                    while j < b.len() && matches!(b[j], b'-' | b'~' | b' ' | b'\t' | b'\'' | b'"') {
                        j += 1;
                    }
                    let s = j;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    if j > s {
                        pending = Some(b[s..j].to_vec());
                    }
                    i = j;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    starts
}

/// Error recovery: a grammar error early in a file (zsh-only or exotic bash
/// syntax) can swallow every later definition. For each column-0 header with
/// no symbol, re-parse just that top-level chunk (up to the next header),
/// with everything else blanked to newlines so line numbers stay put.
fn recover_headers(
    pre: &Pre,
    source: &str,
    parsed: &mut super::ParsedFile,
    raw: &mut impl FnMut(&str) -> Result<super::ParsedFile, super::ParseError>,
) {
    let lines: Vec<&str> = pre.text.split('\n').collect();
    let code = code_line_starts(&pre.text);
    let headers: Vec<usize> = (0..lines.len())
        .filter(|&r| code.get(r).copied().unwrap_or(false) && header_name(lines[r]).is_some())
        .collect();
    let have: std::collections::HashSet<u32> =
        parsed.symbols.iter().map(|s| s.start_line).collect();
    let mut seen: std::collections::HashSet<(u32, String)> = parsed
        .symbols
        .iter()
        .map(|s| (s.start_line, s.name.clone()))
        .collect();
    let orig: Vec<&str> = source.lines().collect();
    for (k, &row) in headers.iter().enumerate() {
        if have.contains(&(row as u32 + 1)) {
            continue;
        }
        let end = headers.get(k + 1).copied().unwrap_or(lines.len());
        let mut chunk = "\n".repeat(row);
        chunk.push_str(&lines[row..end].join("\n"));
        let Ok(mut part) = raw(&chunk) else { continue };
        if !part
            .symbols
            .iter()
            .any(|s| s.start_line as usize == row + 1)
        {
            // The body itself does not parse: keep the header with an empty
            // body so the name is still indexed, spanning the whole chunk.
            let name = header_name(lines[row]).unwrap_or_default();
            let stub = format!("{}{name}() {{ :; }}\n", "\n".repeat(row));
            if let Ok(stub) = raw(&stub) {
                let last = (row..end)
                    .rev()
                    .find(|&r| !lines[r].trim().is_empty())
                    .unwrap_or(row);
                for mut symbol in stub.symbols {
                    if symbol.start_line as usize == row + 1 {
                        symbol.end_line = last as u32 + 1;
                        part.symbols.push(symbol);
                    }
                }
            }
        }
        for mut symbol in part.symbols {
            let r = symbol.start_line as usize;
            if r <= row
                || r > end.max(row + 1)
                || seen.contains(&(symbol.start_line, symbol.name.clone()))
            {
                continue;
            }
            if symbol.name == BATS_SENTINEL {
                match pre.tests.iter().find(|(l, _)| *l == symbol.start_line) {
                    Some((_, name)) => symbol.name = name.clone(),
                    None => continue,
                }
            }
            if let Some(o) = orig.get(r - 1) {
                symbol.signature = super::signature_from_row(o.trim_start(), 0);
            }
            seen.insert((symbol.start_line, symbol.name.clone()));
            parsed.symbols.push(symbol);
        }
    }
    parsed.symbols.sort_by_key(|s| (s.start_line, s.end_line));
}

pub(super) struct Pre {
    pub text: String,
    pub zsh: bool,
    /// `(line, test name)` for each `.bats` `@test`.
    pub tests: Vec<(u32, String)>,
    /// `(line, names)` for zsh `function a b c {` (bash keeps only `a`).
    pub multi_names: Vec<(u32, Vec<(String, u32)>)>,
}

/// Whether the text is zsh rather than bash/sh: `.zsh` / `.zsh-theme`, a zsh
/// shebang, or zsh-only syntax markers (oh-my-zsh ships zsh as `.sh`).
pub(crate) fn is_zsh(path: &str, source: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".zsh") || lower.ends_with(".zsh-theme") || lower.ends_with(".zshrc") {
        return true;
    }
    if Language::shebang_interpreter(source) == Some("zsh") {
        return true;
    }
    const MARKERS: [&str; 8] = [
        "${(",
        "${\"",
        "${${",
        "${+",
        "$+commands",
        "$+functions",
        ":a:h",
        ":A:h",
    ];
    const KEYWORDS: [&str; 4] = ["setopt ", "zstyle ", "emulate ", "autoload -U"];
    MARKERS.iter().any(|m| source.contains(m))
        || source
            .lines()
            .any(|l| KEYWORDS.iter().any(|k| l.trim_start().starts_with(k)))
}

pub(super) fn preprocess(path: &str, source: &str) -> Pre {
    let zsh = is_zsh(path, source);
    let bats = path.to_ascii_lowercase().ends_with(".bats")
        || matches!(
            Language::shebang_interpreter(source),
            Some("bats" | "zunit")
        );
    let mut out = source.as_bytes().to_vec();
    let mut tests = Vec::new();
    let mut multi_names = Vec::new();
    if bats {
        bats_tests(&mut out, &mut tests);
    }
    if zsh {
        sanitize_zsh(&mut out, &mut multi_names);
    }
    // Only ASCII bytes are ever written over whole ASCII-or-not runs that we
    // fully replace, so the buffer stays valid UTF-8; fall back defensively.
    let text = String::from_utf8(out).unwrap_or_else(|_| source.to_owned());
    Pre {
        text,
        zsh,
        tests,
        multi_names,
    }
}

/// `@test "name" {` → `_BT_()        {` (same length, same line).
fn bats_tests(out: &mut [u8], tests: &mut Vec<(u32, String)>) {
    let mut line_no = 0u32;
    let mut start = 0usize;
    let n = out.len();
    while start < n {
        line_no += 1;
        let end = out[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(n, |p| start + p);
        let line = &out[start..end];
        let indent = line
            .iter()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
        let rest = &line[indent..];
        if rest.first() == Some(&b'@') && !rest.starts_with(b"@test") {
            // zunit `@setup {` / `@teardown {` → brace group.
            let w = rest[1..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric())
                .count();
            let tail = rest[1 + w..].trim_ascii_start();
            if w > 0 && tail.starts_with(b"{") {
                let at = start + indent;
                out[at..at + 1 + w].fill(b' ');
            }
        } else if let Some(after) = rest.strip_prefix(b"@test") {
            let ws = after
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            let q = after.get(ws).copied();
            if ws > 0 && matches!(q, Some(b'"' | b'\'')) {
                let body = &after[ws + 1..];
                let mut j = 0;
                while j < body.len() && body[j] != q.unwrap() {
                    if body[j] == b'\\' && q == Some(b'"') {
                        j += 1;
                    }
                    j += 1;
                }
                if j < body.len() {
                    let name = String::from_utf8_lossy(&body[..j]).into_owned();
                    let head_len = 5 + ws + 1 + j + 1;
                    let at = start + indent;
                    let repl = format!("{BATS_SENTINEL}()");
                    if head_len >= repl.len() {
                        out[at..at + head_len].fill(b' ');
                        out[at..at + repl.len()].copy_from_slice(repl.as_bytes());
                        tests.push((line_no, name));
                    }
                }
            }
        }
        start = end + 1;
    }
}

/// Heredoc bodies that tree-sitter-bash 0.25 ends too early: its scanner
/// closes the heredoc at the first body line that merely *starts* with the
/// delimiter (`BLOCKER (W6-A22):` inside `<<'BLOCKER' … BLOCKER`), and the
/// rest of the body is then parsed as commands. Same-length rewrite: a
/// quoted heredoc (`<<'D'`, `<<"D"`, `<<\D`) does not expand, so its body is
/// blanked to spaces; in an unquoted body only the first byte of each such
/// look-alike line becomes `_`, keeping its expansions. Newlines stay, so
/// lines are unchanged. `None` when no heredoc needs it. The caller uses it
/// only for a retry after a partial parse.
pub(super) fn neutralize_heredoc_bodies(source: &str) -> Option<String> {
    if !source.contains("<<") {
        return None;
    }
    let b = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    let (mut sq, mut dq, mut comment) = (false, false, false);
    let mut ansi = false;
    let mut arith = 0u32;
    // (delimiter, quoted) queued on the current line.
    let mut pending: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            comment = false;
            i += 1;
            for (delim, quoted) in std::mem::take(&mut pending) {
                let mut needed = false;
                let body_start = i;
                let mut body_end = None;
                while i < b.len() {
                    let end = b[i..]
                        .iter()
                        .position(|&x| x == b'\n')
                        .map_or(b.len(), |p| i + p);
                    let line = &b[i..end];
                    if line.trim_ascii() == delim.as_slice() {
                        body_end = Some(i);
                        i = (end + 1).min(b.len());
                        break;
                    }
                    let lead = line
                        .iter()
                        .take_while(|&&x| x == b' ' || x == b'\t')
                        .count();
                    if line[lead..].starts_with(&delim) {
                        needed = true;
                        if !quoted {
                            out.get_or_insert_with(|| b.to_vec())[i + lead] = b'_';
                        }
                    }
                    i = (end + 1).min(b.len());
                }
                if needed && quoted {
                    let buf = out.get_or_insert_with(|| b.to_vec());
                    for x in &mut buf[body_start..body_end.unwrap_or(b.len())] {
                        if *x != b'\n' {
                            *x = b' ';
                        }
                    }
                }
            }
            continue;
        }
        if comment {
            i += 1;
            continue;
        }
        if sq {
            if ansi && c == b'\\' && b.get(i + 1).is_some_and(|&n| n != b'\n') {
                i += 1;
            } else {
                sq = c != b'\'';
                ansi &= sq;
            }
        } else if c == b'\\' {
            i += 1;
        } else if dq {
            dq = c != b'"';
        } else {
            match c {
                b'\'' => {
                    sq = true;
                    ansi = i > 0 && b[i - 1] == b'$';
                }
                b'"' => dq = true,
                b'#' if is_word_start(b, i) => comment = true,
                b'(' if b.get(i + 1) == Some(&b'(') => {
                    arith += 1;
                    i += 2;
                    continue;
                }
                b')' if arith > 0 && b.get(i + 1) == Some(&b')') => {
                    arith -= 1;
                    i += 2;
                    continue;
                }
                b'<' if arith > 0 && b[i..].starts_with(b"<<") => {
                    i += 2;
                    continue;
                }
                b'<' if b[i..].starts_with(b"<<<") => {
                    i += 3;
                    continue;
                }
                b'<' if b[i..].starts_with(b"<<") => {
                    let mut j = i + 2;
                    if matches!(b.get(j), Some(b'-' | b'~')) {
                        j += 1;
                    }
                    while j < b.len() && matches!(b[j], b' ' | b'\t') {
                        j += 1;
                    }
                    let q = b
                        .get(j)
                        .copied()
                        .filter(|q| matches!(q, b'\'' | b'"' | b'\\'));
                    if q.is_some() {
                        j += 1;
                    }
                    let s = j;
                    while j < b.len()
                        && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'-'))
                    {
                        j += 1;
                    }
                    if j > s {
                        pending.push((b[s..j].to_vec(), q.is_some()));
                        if matches!(q, Some(b'\'' | b'"')) && b.get(j) == q.as_ref() {
                            j += 1;
                        }
                    }
                    i = j;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    out.and_then(|v| String::from_utf8(v).ok())
}

fn is_word_start(out: &[u8], i: usize) -> bool {
    i == 0
        || matches!(
            out[i - 1],
            b' ' | b'\t' | b'\n' | b';' | b'(' | b'|' | b'&' | b'{'
        )
}

fn blank(out: &mut [u8], from: usize, to: usize, with: u8) {
    for b in &mut out[from..to] {
        if *b != b'\n' {
            *b = with;
        }
    }
}

/// End (exclusive) of a balanced `${…}` starting at `i` (`out[i] == '$'`).
fn brace_end(out: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i + 1;
    while j < out.len() {
        match out[j] {
            b'\\' => j += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            b'\n' => return None,
            _ => {}
        }
        j += 1;
    }
    None
}

/// Length- and line-preserving rewrite of zsh-only syntax into bash the
/// tree-sitter-bash grammar accepts. Strings, comments and heredoc bodies are
/// skipped; expansions inside double quotes are rewritten.
fn sanitize_zsh(out: &mut [u8], multi_names: &mut Vec<(u32, Vec<(String, u32)>)>) {
    let n = out.len();
    let mut i = 0usize;
    let mut in_dq = false;
    let mut line_no = 1u32;
    let mut line_start = true;
    // (delimiter, quoted)
    let mut heredocs: Vec<(Vec<u8>, bool)> = Vec::new();
    while i < n {
        let c = out[i];
        if c == b'\n' {
            line_no += 1;
            i += 1;
            line_start = true;
            // Skip heredoc bodies queued on the line just ended.
            for (delim, quoted) in std::mem::take(&mut heredocs) {
                while i < n {
                    let end = out[i..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(n, |p| i + p);
                    let done = out[i..end].trim_ascii() == delim.as_slice();
                    if !quoted && !done {
                        // Unquoted bodies expand; the grammar parses `${…}`.
                        let mut k = i;
                        while k + 1 < end {
                            if out[k] == b'\\' {
                                k += 2;
                                continue;
                            }
                            if out[k] == b'$' && out[k + 1] == b'{' {
                                if let Some(e) = brace_end(out, k) {
                                    blank(out, k + 2, e - 1, b'_');
                                    k = e;
                                    continue;
                                }
                            }
                            k += 1;
                        }
                    }
                    i = (end + 1).min(n);
                    line_no += 1;
                    if done {
                        break;
                    }
                }
            }
            continue;
        }
        let at_line_start = line_start;
        if !matches!(c, b' ' | b'\t') {
            line_start = false;
        }
        if c == b'\\' {
            if out.get(i + 1) == Some(&b'\n') {
                line_no += 1;
            }
            i += 2;
            continue;
        }
        if in_dq {
            if c == b'"' {
                in_dq = false;
                i += 1;
                continue;
            }
        } else {
            match c {
                b'\'' => {
                    let ansi = i > 0 && out[i - 1] == b'$';
                    i += 1;
                    while i < n && out[i] != b'\'' {
                        if ansi && out[i] == b'\\' {
                            i += 1;
                        }
                        if i < n && out[i] == b'\n' {
                            line_no += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                b'"' => {
                    in_dq = true;
                    i += 1;
                    continue;
                }
                b'#' if is_word_start(out, i) => {
                    while i < n && out[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'<' if out[i..].starts_with(b"<<") && !out[i..].starts_with(b"<<<") => {
                    let mut j = i + 2;
                    let dash = out.get(j) == Some(&b'-');
                    if dash || out.get(j) == Some(&b'~') {
                        j += 1;
                    }
                    while j < n && matches!(out[j], b' ' | b'\t') {
                        j += 1;
                    }
                    let q = out.get(j).copied();
                    if matches!(q, Some(b'\'' | b'"')) {
                        j += 1;
                    }
                    let s = j;
                    while j < n && (out[j].is_ascii_alphanumeric() || out[j] == b'_') {
                        j += 1;
                    }
                    let _ = dash;
                    if j > s {
                        heredocs.push((out[s..j].to_vec(), matches!(q, Some(b'\'' | b'"'))));
                        if matches!(q, Some(b'\'' | b'"')) && out.get(j) == q.as_ref() {
                            j += 1;
                        }
                        i = j;
                        continue;
                    }
                }
                b'<' => {
                    // `<->` / `<1-10>` numeric globs.
                    let mut j = i + 1;
                    while j < n && out[j].is_ascii_digit() {
                        j += 1;
                    }
                    if out.get(j) == Some(&b'-') {
                        j += 1;
                        while j < n && out[j].is_ascii_digit() {
                            j += 1;
                        }
                        if out.get(j) == Some(&b'>') && (i == 0 || out[i - 1] != b'<') {
                            blank(out, i, j + 1, b'?');
                            i = j + 1;
                            continue;
                        }
                    }
                }
                b'0' if at_line_start && out.get(i + 1) == Some(&b'=') => {
                    out[i] = b':'; // `0="${…}"` (plugin standard) → `:=…` no-op word
                }
                b'}' if i > 1
                    && out[i - 1] == b' '
                    && !matches!(out[i - 2], b';' | b'{' | b'&' | b'\n' | b' ' | b'\t' | b'|')
                    && out
                        .get(i + 1)
                        .is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b';' | b')')) =>
                {
                    out[i - 1] = b';'; // zsh `{ cmd }` → `{ cmd;}`
                }
                b'>' if out.get(i + 1) == Some(&b'|') => {
                    out[i + 1] = b' '; // `>|` / `>>|` clobber
                }
                b'=' if out.get(i + 1) == Some(&b'(') && is_word_start(out, i) => {
                    out[i] = b'<'; // `=( cmd )` → `<( cmd )`
                }
                b'&' if matches!(out.get(i + 1), Some(b'!' | b'|'))
                    && out
                        .get(i + 2)
                        .is_none_or(|b| b.is_ascii_whitespace() || *b == b';') =>
                {
                    out[i + 1] = b' '; // `&!` / `&|` disown
                }
                b'(' if at_line_start && out[i..].starts_with(b"()") => {
                    // Anonymous function `() { … }` → brace group.
                    let mut j = i + 2;
                    while j < n && matches!(out[j], b' ' | b'\t') {
                        j += 1;
                    }
                    if out.get(j) == Some(&b'{') {
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        continue;
                    }
                }
                b'(' if i > 0 && glob_qualifier_prev(out[i - 1]) && glob_word_before(out, i) => {
                    // `*.zsh(-.N:r:gs:"$X"/::)` — loose qualifier after a glob word.
                    let mut j = i + 1;
                    let mut dq = false;
                    while j < n && out[j] != b'\n' && (dq || !matches!(out[j], b')' | b' ' | b'('))
                    {
                        if out[j] == b'"' {
                            dq = !dq;
                        }
                        j += 1;
                    }
                    if j > i + 1 && out.get(j) == Some(&b')') {
                        blank(out, i, j + 1, b' ');
                        i = j + 1;
                        continue;
                    }
                }
                b'(' if i > 0 && glob_qualifier_prev(out[i - 1]) => {
                    let mut j = i + 1;
                    while j < n
                        && (out[j].is_ascii_alphanumeric()
                            || matches!(
                                out[j],
                                b'.' | b':'
                                    | b'^'
                                    | b'-'
                                    | b'+'
                                    | b'='
                                    | b','
                                    | b'#'
                                    | b'/'
                                    | b'['
                                    | b']'
                                    | b'@'
                                    | b'*'
                                    | b'%'
                            ))
                    {
                        j += 1;
                    }
                    if j > i + 1
                        && out.get(j) == Some(&b')')
                        && out.get(j + 1).is_none_or(|b| {
                            b.is_ascii_whitespace() || matches!(b, b')' | b';' | b'"' | b'|' | b'}')
                        })
                    {
                        blank(out, i, j + 1, b' ');
                        i = j + 1;
                        continue;
                    }
                }
                b'f' if is_word_start(out, i) => {
                    if out[i..].starts_with(b"for ") {
                        rewrite_for(out, i + 4);
                    } else if out[i..].starts_with(b"function ") {
                        function_names(out, i + 9, line_no, multi_names);
                    }
                }
                _ => {}
            }
        }
        if c == b'$' {
            match out.get(i + 1) {
                Some(b'{') => {
                    if let Some(end) = brace_end(out, i) {
                        if end - i >= 3 {
                            blank(out, i + 2, end - 1, b'_');
                        }
                        i = end;
                        continue;
                    }
                }
                Some(b) if b.is_ascii_alphabetic() || *b == b'_' => {
                    let mut j = i + 1;
                    while j < n && (out[j].is_ascii_alphanumeric() || out[j] == b'_') {
                        j += 1;
                    }
                    if out.get(j) == Some(&b'[') && out.get(j + 1) == Some(&b'(') {
                        if let Some(p) = out[j..].iter().position(|&b| b == b']' || b == b'\n') {
                            if out[j + p] == b']' {
                                blank(out, j, j + p + 1, b'_');
                                i = j + p + 1;
                                continue;
                            }
                        }
                    }
                }
                Some(b'#')
                    if out
                        .get(i + 2)
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') =>
                {
                    // zsh `$#name` (length) → `$_____`.
                    let mut j = i + 2;
                    while j < n && (out[j].is_ascii_alphanumeric() || out[j] == b'_') {
                        j += 1;
                    }
                    blank(out, i + 1, j, b'_');
                    i = j;
                    continue;
                }
                Some(b'+')
                    if out
                        .get(i + 2)
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') =>
                {
                    let mut j = i + 2;
                    while j < n && (out[j].is_ascii_alphanumeric() || out[j] == b'_') {
                        j += 1;
                    }
                    if out.get(j) == Some(&b'[') {
                        if let Some(p) = out[j..].iter().position(|&b| b == b']' || b == b'\n') {
                            if out[j + p] == b']' {
                                j += p + 1;
                            }
                        }
                    }
                    blank(out, i + 1, j, b'_');
                    i = j;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
}

/// The word ending at `i` contains an unquoted-looking `*` / `?` glob.
fn glob_word_before(out: &[u8], i: usize) -> bool {
    let mut k = i;
    while k > 0 && !matches!(out[k - 1], b' ' | b'\t' | b'\n' | b'(' | b'=' | b';') {
        k -= 1;
        if matches!(out[k], b'*' | b'?') {
            return true;
        }
    }
    false
}

fn glob_qualifier_prev(b: u8) -> bool {
    matches!(b, b'*' | b'?' | b']' | b'/' | b'"' | b'}' | b'.') || b.is_ascii_alphanumeric()
}

/// zsh `for k v in …` / `for x ( list )` → bash `for k   in …` / `for x in list `.
fn rewrite_for(out: &mut [u8], mut j: usize) {
    let n = out.len();
    let mut idents = Vec::new();
    loop {
        while j < n && matches!(out[j], b' ' | b'\t') {
            j += 1;
        }
        let s = j;
        while j < n && (out[j].is_ascii_alphanumeric() || out[j] == b'_') {
            j += 1;
        }
        if j == s {
            break;
        }
        if &out[s..j] == b"in" {
            j = s;
            break;
        }
        idents.push((s, j));
    }
    if idents.is_empty() {
        return;
    }
    if out.get(j) == Some(&b'(') && out.get(j + 1) == Some(&b' ') && j > 0 && out[j - 1] == b' ' {
        // Short form: `( ` → `in`, matching `)` → ` `.
        let mut depth = 0;
        let mut k = j;
        while k < n {
            match out[k] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if k < n {
            out[j] = b'i';
            out[j + 1] = b'n';
            out[k] = b';';
        }
    } else if !out[j..].starts_with(b"in") {
        return;
    }
    for &(s, e) in &idents[1..] {
        blank(out, s, e, b' ');
    }
}

/// zsh `function a b c {` → bash `function a     {`; the extra names are
/// re-added after parsing.
fn function_names(
    out: &mut [u8],
    mut j: usize,
    line: u32,
    multi: &mut Vec<(u32, Vec<(String, u32)>)>,
) {
    let n = out.len();
    let mut names = Vec::new();
    loop {
        while j < n && matches!(out[j], b' ' | b'\t') {
            j += 1;
        }
        if out[j..].starts_with(b"\\\n") {
            // `function a \⏎ b {`: the `\` goes, the newline stays (bash
            // accepts a newline before the body).
            out[j] = b' ';
            j += 2;
            continue;
        }
        let s = j;
        while j < n
            && !out[j].is_ascii_whitespace()
            && !matches!(out[j], b'(' | b')' | b'{' | b'}' | b';' | b'|' | b'&')
        {
            j += 1;
        }
        if j == s {
            break;
        }
        names.push((s, j));
    }
    if names.len() < 2 {
        return;
    }
    let text =
        |(s, e): (usize, usize), out: &[u8]| String::from_utf8_lossy(&out[s..e]).into_owned();
    let start = names[0].0;
    let list: Vec<(String, u32)> = names
        .iter()
        .map(|&r| {
            let nl = out[start..r.0].iter().filter(|&&b| b == b'\n').count() as u32;
            (text(r, out), line + nl)
        })
        .collect();
    for &(s, e) in &names[1..] {
        blank(out, s, e, b' ');
    }
    multi.push((line, list));
}

#[cfg(test)]
mod tests {
    use super::super::ParserPool;
    use crate::types::{Language, RelPath, SymbolKind};

    const FIXTURE: &str = include_str!("../../tests/fixtures/shell/lib.sh");

    #[test]
    fn shell_full_declaration_coverage() {
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("lib.sh"), FIXTURE)
            .expect("parse");
        let mut got: Vec<(String, SymbolKind)> = parsed
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        use SymbolKind::*;
        let mut want: Vec<(String, SymbolKind)> = [
            ("APP_HOME", Variable),
            ("EXPORTED", Variable),
            ("MORE", Variable),
            ("VERSION", Const),
            ("LIMIT", Const),
            ("GLOBAL_RO", Const),
            ("ARR", Variable),
            ("TYPESET_VAR", Variable),
            ("posix_fn", Function),
            ("kw_fn", Function),
            ("kw_paren_fn", Function),
            ("inner_fn", Function),
            ("_private_helper", Function),
            ("in_if_fn", Function),
        ]
        .iter()
        .map(|(n, k)| (n.to_string(), *k))
        .collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, want);
        assert!(parsed.symbols.iter().all(|s| s.exported));

        let posix = parsed
            .symbols
            .iter()
            .find(|s| s.name == "posix_fn")
            .unwrap();
        assert!(posix.end_line > posix.start_line);
    }

    #[test]
    fn shell_imports_and_calls() {
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("lib.sh"), FIXTURE)
            .expect("parse");
        assert_eq!(
            parsed.imports,
            vec![
                "./common.sh",
                "helpers/util.bash",
                "${BASH_SOURCE%/*}/dyn.sh",
                "$(dirname \"$0\")/sub.sh",
            ]
        );
        let names: Vec<&str> = parsed.calls.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"posix_fn"));
        assert!(names.contains(&"echo"));
        assert!(!names.iter().any(|n| n.starts_with('$')));
    }

    fn names(lang_path: &str, src: &str) -> Vec<(String, u32)> {
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new(lang_path), src)
            .expect("parse");
        parsed
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.start_line))
            .collect()
    }

    #[test]
    fn zsh_extension_and_shebang_map_to_shell() {
        for p in ["a.zsh", "t.zsh-theme", "x.bats"] {
            assert_eq!(
                Language::from_path(&RelPath::new(p)),
                Some(Language::Shell),
                "{p}"
            );
        }
        assert_eq!(
            Language::from_shebang("#!/usr/bin/env zsh\n"),
            Some(Language::Shell)
        );
        assert_eq!(
            Language::from_shebang("#!/usr/bin/env bats\n"),
            Some(Language::Shell)
        );
    }

    #[test]
    fn zsh_only_syntax_does_not_swallow_definitions() {
        let src = r#"#!/usr/bin/env zsh
0="${${ZERO:-${0:#$ZSH_ARGZERO}}:-${(%):-%N}}"
remote=${"$(git config --local x.remote)":-origin}
(( $+commands[git] )) || return
files=( $dir/*.zsh(N) ${(f)"$(ls)"} )
() {
  local x=$1
} arg
for k v in ${(kv)map}; do echo $k; done
for f ( $files ); do echo $f; done
if [[ $x == <-> ]]; then echo n; fi
function cfap() { cf app $1 }
function multi_a multi_b {
  echo ${#multi}
}
cat <<EOF
Usage: ${(j: :)${(s.::.)0#_}}
EOF
function _omz::plugin { print -l ${(@f)"$(cat x)"}; }
after_all() {
  echo ok
}
"#;
        let got = names("lib/cli.zsh", src);
        for want in ["cfap", "multi_a", "multi_b", "_omz::plugin", "after_all"] {
            assert!(
                got.iter().any(|(n, _)| n == want),
                "{want} missing: {got:?}"
            );
        }
        let line = |n: &str| got.iter().find(|(m, _)| m == n).unwrap().1;
        assert_eq!(line("cfap"), 12);
        assert_eq!(line("multi_b"), 13);
        assert_eq!(line("after_all"), 20);
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("lib/cli.zsh"), src)
            .unwrap();
        let sig = &parsed
            .symbols
            .iter()
            .find(|s| s.name == "_omz::plugin")
            .unwrap()
            .signature;
        assert!(
            !sig.contains("___"),
            "signature shows sanitised text: {sig}"
        );
    }

    #[test]
    fn file_scope_vars_before_a_broken_first_function_survive() {
        let src = "#!/usr/bin/env zsh\n\nTHEMES_DIR=\"$ZSH/themes\"\nFAVLIST=\"${HOME}/.zsh_favlist\"\n\nfunction preview() {\n    NAME=`echo $THEME | sed s/\\.zsh-theme$//`\n    cols=$(tput cols)\n}\n";
        let got = names("tools/theme_chooser.sh", src);
        for (n, l) in [("THEMES_DIR", 3), ("FAVLIST", 4), ("preview", 6)] {
            assert!(got.contains(&(n.to_string(), l)), "{n} missing: {got:?}");
        }
        // Function-local assignments are not file-scope variables.
        assert!(
            !got.iter().any(|(n, _)| n == "cols" || n == "NAME"),
            "{got:?}"
        );
    }

    #[test]
    fn zsh_multi_name_functions_keep_each_names_line() {
        let src = "#!/usr/bin/env zsh\nls -l \\\n  x\nfunction a_info \\\n  b_info \\\n  c_info \\\n{\n  return 1\n}\nfunction mkcd takedir() {\n  mkdir -p $@ && cd ${@:$#}\n}\n";
        let got = names("lib/p.zsh", src);
        for (n, l) in [
            ("a_info", 4),
            ("b_info", 5),
            ("c_info", 6),
            ("mkcd", 10),
            ("takedir", 10),
        ] {
            assert!(
                got.contains(&(n.to_string(), l)),
                "{n}@{l} missing: {got:?}"
            );
        }
    }

    #[test]
    fn file_scope_vars_after_an_inline_function_survive() {
        let src = "(( $+functions[wip] )) || wip() {}\n\nPREFIX=\"%{$fg[cyan]%}[\"\nexport VCS_PROMPT=x\nf() {\n  echo ${(%):-%N}\n}\nPROMPT='$(f)'\n";
        let got = names("t.zsh-theme", src);
        for (n, l) in [("PREFIX", 3), ("VCS_PROMPT", 4), ("PROMPT", 8)] {
            assert!(got.contains(&(n.to_string(), l)), "{n} missing: {got:?}");
        }
    }

    #[test]
    fn bash_files_are_not_rewritten() {
        let src = "#!/bin/bash\nf() { echo \"${x:-y}\"; }\n";
        let pre = super::preprocess("a.sh", src);
        assert!(!pre.zsh);
        assert_eq!(pre.text, src);
    }

    #[test]
    fn recovery_ignores_headers_inside_strings() {
        let src = "#!/bin/bash\nf() {\n  awk '\nfunction alen(arr) { return 1 }\n' \"${x[@]\"\n}\ng() { :; }\n";
        let got = names("a.sh", src);
        assert!(!got.iter().any(|(n, _)| n == "alen"), "{got:?}");
        assert!(got.iter().any(|(n, _)| n == "g"), "{got:?}");
    }

    #[test]
    fn bats_tests_are_functions_named_after_the_test() {
        let src = "#!/usr/bin/env bats\nload test_helper\n\nsetup() {\n  x=1\n}\n\n@test \"adds two numbers\" {\n  run add 1 2\n  [ \"$output\" -eq 3 ]\n}\n\n@test 'single quoted' {\n  true\n}\n";
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("test/add.bats"), src)
            .unwrap();
        let t = parsed
            .symbols
            .iter()
            .find(|s| s.name == "adds two numbers")
            .expect("test");
        assert_eq!(
            (t.start_line, t.end_line, t.kind),
            (8, 11, SymbolKind::Function)
        );
        assert!(
            t.signature.starts_with("@test \"adds two numbers\""),
            "{}",
            t.signature
        );
        assert!(parsed
            .symbols
            .iter()
            .any(|s| s.name == "single quoted" && s.start_line == 13));
        assert!(parsed.symbols.iter().any(|s| s.name == "setup"));
        assert!(!parsed
            .symbols
            .iter()
            .any(|s| s.name == super::BATS_SENTINEL));
    }

    /// Corpus symbol dump for the recall bench (`/workspace/recall-bench/lang8`).
    /// `DUMP_DIR` root, `DUMP_LIST` repo-relative paths, `DUMP_OUT` TSV
    /// `file\tline\tname\tkind`. Files whose language is not detected as
    /// Shell are reported as `SKIP`.
    #[test]
    #[ignore = "corpus dump; run with -- --ignored"]
    fn zz_dump_shell_symbols() {
        use std::io::Write;
        let (Ok(dir), Ok(list), Ok(out)) = (
            std::env::var("DUMP_DIR"),
            std::env::var("DUMP_LIST"),
            std::env::var("DUMP_OUT"),
        ) else {
            return;
        };
        let pool = ParserPool::new();
        let mut w = std::fs::File::create(out).unwrap();
        let t0 = std::time::Instant::now();
        let (mut files, mut bytes, mut errs) = (0usize, 0usize, 0usize);
        for rel in std::fs::read_to_string(list).unwrap().lines() {
            let Ok(raw) = std::fs::read(format!("{dir}/{rel}")) else {
                continue;
            };
            let src = String::from_utf8_lossy(&raw);
            let path = RelPath::new(rel);
            if Language::from_path_and_source(&path, &src) != Some(Language::Shell) {
                println!("SKIP {rel}");
                continue;
            }
            files += 1;
            bytes += raw.len();
            let res = if std::env::var("DUMP_RAW").is_ok() {
                pool.parse_raw(Language::Shell, &path, &src)
            } else {
                pool.parse(Language::Shell, &path, &src)
            };
            if let Ok(pre_out) = std::env::var("DUMP_PRE") {
                let pre = super::preprocess(rel, &src);
                std::fs::write(format!("{pre_out}/{}", rel.replace('/', "__")), pre.text).unwrap();
            }
            match res {
                Ok(p) => {
                    for s in p.symbols {
                        writeln!(w, "{rel}\t{}\t{}\t{:?}", s.start_line, s.name, s.kind).unwrap();
                    }
                }
                Err(e) => {
                    errs += 1;
                    println!("ERR {rel} {e:?}");
                }
            }
        }
        println!(
            "DUMP files={files} bytes={bytes} errs={errs} ms={}",
            t0.elapsed().as_millis()
        );
    }
}

#[cfg(test)]
mod backslash_dot_tests {
    use super::normalize_backslash_dot;

    #[test]
    fn rewrites_only_line_leading_backslash_dot() {
        let src = ": nvm.sh\n\\. ../nvm.sh\n  \\.\t./a.sh\necho \\. x\n\\.x\n";
        let got = normalize_backslash_dot(src).expect("rewritten");
        assert_eq!(
            got,
            ": nvm.sh\n . ../nvm.sh\n   .\t./a.sh\necho \\. x\n\\.x\n"
        );
        assert_eq!(got.len(), src.len());
        assert_eq!(normalize_backslash_dot("source a.sh\n"), None);
    }
}

#[cfg(test)]
mod audit_fix_tests {
    use super::code_line_starts;

    #[test]
    fn shift_and_ansi_quote_do_not_hide_later_lines() {
        let a = code_line_starts("x=$((1<<bits))\nf() { :; }\ng() { :; }\n");
        assert!(a.iter().all(|&c| c), "{a:?}");
        let b = code_line_starts("(( y = 1<<n ))\nf() { :; }\n");
        assert!(b.iter().all(|&c| c), "{b:?}");
        let c = code_line_starts("x=$'it\\'s'\nf() { :; }\ng() { :; }\n");
        assert!(c.iter().all(|&c| c), "{c:?}");
        // real heredocs and multi-line quotes still hide their bodies
        let d = code_line_starts("cat <<EOF\nf() {\nEOF\nh() { :; }\n");
        assert_eq!(d, vec![true, false, false, true, true]);
        let e = code_line_starts("echo 'a\nf() { :; }\n'\n");
        assert!(!e[1], "{e:?}");
    }
}

#[cfg(test)]
mod heredoc_prefix_tests {
    use super::neutralize_heredoc_bodies as fix;
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath};

    #[test]
    fn blanks_quoted_body_and_marks_unquoted_lookalikes() {
        let src = "cat >&2 <<'BLOCKER'\nERROR: x\nBLOCKER (W6):\nBLOCKER\nf() { :; }\n";
        let out = fix(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert_eq!(
            out,
            "cat >&2 <<'BLOCKER'\n        \n             \nBLOCKER\nf() { :; }\n"
        );
        let src = "cat <<EOF\nhi $x\nEOFX $(y)\nEOF\n";
        assert_eq!(fix(src).unwrap(), "cat <<EOF\nhi $x\n_OFX $(y)\nEOF\n");
    }

    #[test]
    fn leaves_ordinary_heredocs_and_shifts_alone() {
        for src in [
            "cat <<'EOF'\nhello\nEOF\n",
            "cat <<-EOF\n\thi\n\tEOF\n",
            "x=$((1<<EOF))\nEOF2=1\n",
            "echo '<<EOF'\nEOFX\n",
            "cat <<< \"EOF\"\nEOFX=1\n",
        ] {
            assert_eq!(fix(src), None, "{src}");
        }
    }

    #[test]
    fn script_with_delimiter_prefixed_body_line_parses_clean() {
        let src = "#!/usr/bin/env bash\nset -euo pipefail\ninfo() { echo \"$*\"; }\nif true; then\n  :\nelse\n  cat >&2 <<'BLOCKER'\nERROR: Android NDK not found.\n\nBLOCKER (W6-A22 Android build):\n  - Install NDK via \"ndk;27\"\nBLOCKER\n  exit 2\nfi\nbuild() { cargo ndk build; }\n";
        let p = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("build_android.sh"), src)
            .unwrap();
        assert!(!p.partial);
        let names: Vec<_> = p.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"info") && names.contains(&"build"),
            "{names:?}"
        );
        let b = p.symbols.iter().find(|s| s.name == "build").unwrap();
        assert_eq!(b.start_line, 15);
    }
}
