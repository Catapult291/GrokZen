//! Syntax-error diagnostics for coverage reporting (`get_languages`).
//!
//! Two jobs, both best effort and only for files that already parsed with
//! errors:
//! * **location** — the first error *site*: the innermost ERROR / MISSING
//!   node, skipping comments and the well-formed subtrees tree-sitter wraps
//!   into a recovering ERROR node. A file-spanning ERROR node therefore no
//!   longer reports "line 1: unexpected `// Copyright`".
//! * **kind** — in C / C++ / Objective-C, errors next to preprocessor
//!   directives or macro-looking code (ALL_CAPS names, names `#define`d in
//!   the file, statement macros like `foreach(x) {`, attribute macros before
//!   a type, types passed as macro arguments) are labelled
//!   [`SyntaxErrorKind::Preprocessor`]: without a preprocessor and build
//!   configuration no grammar can parse them, and tree-sitter's recovery
//!   keeps the symbol names (see `PARSE_ERRORS_CLASSIFY.md`: error files
//!   match clean files on recall against ctags). Everything else is
//!   [`SyntaxErrorKind::Syntax`].
//!
//! Re-parses with the raw grammar. The pre-pass retries of C# and TypeScript
//! are kept only when they parse clean, so for a file that is still partial
//! the raw tree is the one that was indexed. C / C++ may keep a still-partial
//! retry that recovers more symbols (`cfamily.rs`, recorded as
//! `ParsedFile::c_family_rewrite`); [`diagnose_indexed_syntax`] then
//! diagnoses that rewritten text, which has the same bytes per line, so an
//! error the rewrite already fixed is never reported. Snippets, tokens and
//! the macro test still quote the original source.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::types::Language;

/// Error sites examined for the kind decision.
const MAX_SITES: usize = 16;
/// Byte cap of the quoted line snippet.
const SNIPPET_MAX: usize = 60;
/// Byte cap of the quoted unexpected token.
const TOKEN_MAX: usize = 30;
/// Lines of an ERROR node, from its first site, scanned for macro use.
const SPAN_LINES: usize = 6;

/// Version of what [`diagnose_indexed_syntax`] reports for given bytes.
/// Bump it whenever a change here can alter a diagnosis (line, text or kind):
/// cached diagnoses (the MCP server's `syntax-diagnoses.json` sidecar) are
/// keyed on it and discarded on mismatch.
pub const DIAG_FORMAT_VERSION: u32 = 1;

/// Why a file that is in the index has syntax errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SyntaxErrorKind {
    /// C-family errors attributable to macros or preprocessor directives;
    /// symbol names are normally recovered.
    Preprocessor,
    /// Any other syntax error; symbols in the broken region may be missing.
    Syntax,
}

/// First error location plus the kind of the file's errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxDiagnosis {
    /// 1-based line of the first error site.
    pub line: u32,
    /// Short description with a snippet of that line.
    pub what: String,
    pub kind: SyntaxErrorKind,
}

struct Site {
    row: usize,
    /// Last line of the enclosing ERROR node (capped), scanned for macros.
    end_row: usize,
    /// `Some(kind)` for a MISSING node.
    missing: Option<String>,
    /// Text of the unexpected token (first line, capped).
    token: String,
}

/// Diagnose `source` with the raw grammar; `None` when the raw parse is
/// clean or the language has no plain grammar (Vue).
pub fn diagnose_syntax(lang: Language, source: &str) -> Option<SyntaxDiagnosis> {
    diagnose_indexed_syntax(lang, source, false)
}

/// Diagnose the text whose parse is in the index: the `cfamily` rewrite of
/// `source` when `c_family_rewrite` (C / C++ only; see
/// `ParsedFile::c_family_rewrite`), else the raw source. Lines, snippets and
/// tokens refer to `source` (the rewrite keeps every byte offset).
pub fn diagnose_indexed_syntax(
    lang: Language,
    source: &str,
    c_family_rewrite: bool,
) -> Option<SyntaxDiagnosis> {
    if super::needs_eof_newline(lang, source) {
        return diagnose_indexed_syntax(lang, &format!("{source}\n"), c_family_rewrite);
    }
    let fixed = (c_family_rewrite && matches!(lang, Language::C | Language::Cpp))
        .then(|| super::cfamily::rewrite(lang, source))
        .flatten()
        .filter(|fixed| fixed.len() == source.len());
    let mut parser = super::configured_parser(lang).ok()?;
    let tree = parser.parse(fixed.as_deref().unwrap_or(source), None)?;
    let root = tree.root_node();
    if !root.has_error() {
        return None;
    }
    let sites = collect_sites(root, source);
    let lines: Vec<&str> = source.split('\n').collect();
    let Some(first) = sites.first() else {
        // `has_error` without a visible ERROR / MISSING node: report the
        // deepest node that still carries the flag.
        let mut node = root;
        'down: loop {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.has_error() {
                    node = child;
                    continue 'down;
                }
            }
            break;
        }
        let row = node.start_position().row;
        let snippet = snippet(lines.get(row).copied().unwrap_or(""));
        return Some(SyntaxDiagnosis {
            line: row as u32 + 1,
            what: if node == root || snippet.is_empty() {
                "syntax errors (location not determined)".to_owned()
            } else {
                format!("syntax error in `{snippet}`")
            },
            kind: SyntaxErrorKind::Syntax,
        });
    };
    let snippet = snippet(lines.get(first.row).copied().unwrap_or(""));
    let what = match &first.missing {
        Some(kind) if snippet.is_empty() => format!("missing `{kind}`"),
        Some(kind) => format!("missing `{kind}` in `{snippet}`"),
        None if first.token.is_empty() && snippet.is_empty() => "unexpected syntax".to_owned(),
        None if first.token.is_empty() || first.token == snippet => {
            format!("unexpected `{snippet}`")
        }
        None if snippet.is_empty() => format!("unexpected `{}`", first.token),
        None => format!("unexpected `{}` in `{snippet}`", first.token),
    };
    let kind = if is_c_family(lang) && macro_attributable(&sites, &lines, source) {
        SyntaxErrorKind::Preprocessor
    } else {
        SyntaxErrorKind::Syntax
    };
    Some(SyntaxDiagnosis {
        line: first.row as u32 + 1,
        what,
        kind,
    })
}

fn is_c_family(lang: Language) -> bool {
    matches!(
        lang,
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp
    )
}

fn is_comment(node: Node<'_>) -> bool {
    node.kind().contains("comment")
}

/// Pre-order walk; each ERROR node yields one site (its innermost first
/// error) and is not descended further; MISSING nodes are sites themselves.
fn collect_sites(root: Node<'_>, source: &str) -> Vec<Site> {
    let mut sites = Vec::new();
    let mut cursor = root.walk();
    'walk: loop {
        let node = cursor.node();
        let mut descend = true;
        if node.is_missing() {
            sites.push(missing_site(node));
            descend = false;
        } else if node.is_error() {
            let mut site = locate_in_error(node, source);
            site.end_row = site
                .end_row
                .max(node.end_position().row)
                .min(site.row + SPAN_LINES);
            sites.push(site);
            descend = false;
        }
        if sites.len() >= MAX_SITES {
            break;
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    sites
}

/// A MISSING node is zero-width and may sit after trailing comments; report
/// the line of the preceding code token, where the `;` / `)` is missing.
fn missing_site(node: Node<'_>) -> Site {
    let mut row = node.start_position().row;
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if is_comment(p) {
            prev = p.prev_sibling();
            continue;
        }
        row = row.min(p.end_position().row);
        break;
    }
    Site {
        row,
        end_row: row,
        missing: Some(node.kind().to_owned()),
        token: String::new(),
    }
}

fn token_site(node: Node<'_>, source: &str) -> Site {
    let text = source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    Site {
        row: node.start_position().row,
        end_row: node.start_position().row,
        missing: None,
        token: cap(text, TOKEN_MAX),
    }
}

/// The innermost first error inside `err`. A nested ERROR / MISSING node is
/// preferred: an outer recovering ERROR (e.g. a whole include-guard block
/// that could not be closed) exists because of it. Without one, the first
/// stray token is reported, skipping comments, well-formed subtrees and
/// preprocessor-directive lines (`#ifndef GUARD`).
fn locate_in_error(err: Node<'_>, source: &str) -> Site {
    if let Some(site) = first_error_below(err, source) {
        return site;
    }
    let mut cursor = err.walk();
    let mut first_code: Option<Node<'_>> = None;
    for child in err.children(&mut cursor) {
        if is_comment(child) {
            continue;
        }
        first_code.get_or_insert(child);
        if child.child_count() == 0 && !on_directive_line(child, source) {
            return token_site(child, source);
        }
    }
    // Every child is well formed (e.g. a missing `#endif` at EOF): report the
    // first code child rather than a leading comment.
    match first_code {
        Some(node) => {
            let mut leaf = node;
            while let Some(c) = leaf.child(0) {
                leaf = c;
            }
            token_site(leaf, source)
        }
        None => token_site(err, source),
    }
}

fn on_directive_line(node: Node<'_>, source: &str) -> bool {
    let start = node.start_byte().min(source.len());
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    source[line_start..].trim_start().starts_with('#')
}

fn first_error_below(node: Node<'_>, source: &str) -> Option<Site> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_missing() {
            return Some(missing_site(child));
        }
        if child.is_error() {
            return Some(locate_in_error(child, source));
        }
        if child.has_error() {
            if let Some(site) = first_error_below(child, source) {
                return Some(site);
            }
        }
    }
    None
}

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn snippet(line: &str) -> String {
    cap(
        &line.trim().replace('\t', " ").replace('`', "'"),
        SNIPPET_MAX,
    )
}

/// Keywords (types excluded, see [`PRIMITIVE_TYPES`]).
const C_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "continue",
    "default",
    "do",
    "else",
    "extern",
    "for",
    "goto",
    "if",
    "inline",
    "register",
    "restrict",
    "return",
    "sizeof",
    "static",
    "switch",
    "typedef",
    "volatile",
    "while",
    "class",
    "namespace",
    "template",
    "typename",
    "public",
    "private",
    "protected",
    "virtual",
    "operator",
    "new",
    "delete",
    "using",
    "friend",
    "explicit",
    "constexpr",
    "noexcept",
    "decltype",
    "catch",
    "try",
    "throw",
    "mutable",
    "override",
    "final",
    "_Atomic",
    "__attribute__",
    "defined",
    "alignas",
    "alignof",
    "static_assert",
    "co_return",
    "co_await",
    "co_yield",
    "this",
    "true",
    "false",
    "nullptr",
];

const PRIMITIVE_TYPES: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "signed", "unsigned", "bool",
    "_Bool", "struct", "union", "enum", "const", "auto",
];

/// Value-like ALL_CAPS names that are ordinary in valid code.
const CAPS_STOP: &[&str] = &["NULL", "FILE", "EOF", "TRUE", "FALSE", "BOOL", "DIR"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok<'a> {
    Id(&'a str),
    Str,
    P(char),
}

/// Tokens of one line: identifiers, string / char literals, punctuation.
/// Comments are dropped; numbers count as punctuation-free gaps (`P('0')`).
fn tokenize(line: &str) -> Vec<Tok<'_>> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if b[i..].starts_with(b"//") {
            break;
        } else if b[i..].starts_with(b"/*") {
            match line[i + 2..].find("*/") {
                Some(p) => i += 2 + p + 2,
                None => break,
            }
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            out.push(Tok::Str);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Id(&line[start..i]));
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            out.push(Tok::P('0'));
        } else {
            let ch = line[i..].chars().next().unwrap_or(' ');
            out.push(Tok::P(ch));
            i += ch.len_utf8();
        }
    }
    out
}

fn is_caps_macro(word: &str) -> bool {
    let letters = word.bytes().filter(|b| b.is_ascii_uppercase()).count();
    word.bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && letters >= 2
        && (word.len() >= 3 || word.contains('_'))
        && !CAPS_STOP.contains(&word)
}

fn is_type_word(word: &str) -> bool {
    PRIMITIVE_TYPES.contains(&word) || (word.ends_with("_t") && word.len() > 2)
}

fn is_keyword(word: &str) -> bool {
    C_KEYWORDS.contains(&word)
}

fn is_conditional_directive(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix('#') else {
        return false;
    };
    let rest = rest.trim_start();
    ["if", "elif", "else", "endif", "define", "undef", "pragma"]
        .iter()
        .any(|d| rest.starts_with(d))
}

/// Names `#define`d in this file.
fn file_macros(source: &str) -> HashSet<&str> {
    let mut set = HashSet::new();
    for line in source.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('#') {
            if let Some(def) = rest.trim_start().strip_prefix("define") {
                if let Some(Tok::Id(name)) = tokenize(def).first() {
                    set.insert(*name);
                }
            }
        }
    }
    set
}

/// True when the file's errors are attributable to macros: the first
/// non-cascade error site is macro-looking and so are at least three
/// quarters of all non-cascade sites. Cascade sites (a lone closing `}` /
/// `)` / `;` line, a comment line) follow an earlier error and say nothing
/// about the cause, so they are not counted either way.
fn macro_attributable(sites: &[Site], lines: &[&str], source: &str) -> bool {
    let macros = file_macros(source);
    let mut counted = 0usize;
    let mut hits = 0usize;
    for site in sites {
        let line = lines.get(site.row).copied().unwrap_or("").trim();
        if is_cascade_line(line) {
            continue;
        }
        let hit = site_is_macro(site, lines, &macros);
        if counted == 0 && !hit {
            return false;
        }
        counted += 1;
        hits += usize::from(hit);
    }
    counted > 0 && hits * 4 >= counted * 3
}

fn is_cascade_line(line: &str) -> bool {
    line.is_empty()
        || line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.starts_with('}')
        || line.chars().all(|c| matches!(c, ')' | ';' | '}' | ' '))
}

/// A line that is only a macro use with no terminator:
/// `NAMESPACE_BEGIN`, `TEST_END`, `SUPPRESS_WARNING("-Wx")`.
fn is_solo_macro_line(toks: &[Tok<'_>], macro_ish: &dyn Fn(&str) -> bool) -> bool {
    match toks {
        [Tok::Id(w)] => macro_ish(w),
        [Tok::Id(w), Tok::P('('), .., Tok::P(')')] => macro_ish(w),
        _ => false,
    }
}

/// Macro-looking code on one line (see the module docs).
fn line_has_macro_use(line: &str, macro_ish: &dyn Fn(&str) -> bool) -> bool {
    let toks = tokenize(line);
    if is_solo_macro_line(&toks, macro_ish) {
        return true;
    }
    let ident_like = |t: Option<&Tok<'_>>| match t {
        Some(Tok::Id(x)) => !is_keyword(x),
        _ => false,
    };
    for (i, t) in toks.iter().enumerate() {
        let Tok::Id(w) = t else { continue };
        if !macro_ish(w) {
            continue;
        }
        let prev = i.checked_sub(1).and_then(|j| toks.get(j));
        let next = toks.get(i + 1);
        // `LUAI_FUNC int f`, `x JEMALLOC_ATTR(...)`, `T *JET_MUTABLE x`,
        // `"%" PRIx64`, `"a" LUA_QL("b")`.
        if ident_like(next)
            || ident_like(prev)
            || matches!(next, Some(Tok::Str))
            || matches!(prev, Some(Tok::Str))
            || (prev == Some(&Tok::P('*'))
                && ident_like(i.checked_sub(2).and_then(|j| toks.get(j))))
        {
            return true;
        }
    }
    false
}

/// `static redisAtomic run_cb g_cb`: three plain identifiers in a row
/// (only `*` between them, none a keyword or primitive type) is never valid
/// C/C++, so one of them is a macro. Also an attribute macro before a type
/// in declaration position: `static redisAtomic size_t x`.
fn has_adjacent_identifier_macro(toks: &[Tok<'_>]) -> bool {
    let plain = |t: &Tok<'_>| match t {
        Tok::Id(x) => !is_keyword(x) && !PRIMITIVE_TYPES.contains(x),
        _ => false,
    };
    let words: Vec<&Tok<'_>> = toks.iter().filter(|t| **t != Tok::P('*')).collect();
    for w in words.windows(3) {
        if w.iter().all(|t| plain(t)) {
            return true;
        }
    }
    for (i, pair) in toks.windows(2).enumerate() {
        if let [Tok::Id(x), Tok::Id(y)] = pair {
            // Only in declaration position: line start or after a storage /
            // qualifier keyword.
            let before = i.checked_sub(1).and_then(|j| toks.get(j));
            let decl_pos = matches!(
                before,
                None | Some(Tok::Id(
                    "static" | "extern" | "inline" | "const" | "volatile"
                ))
            );
            if decl_pos && !is_keyword(x) && !is_type_word(x) && is_type_word(y) {
                return true;
            }
        }
    }
    false
}

fn site_is_macro(site: &Site, lines: &[&str], macros: &HashSet<&str>) -> bool {
    if site.token.starts_with('#') || site.missing.as_deref().is_some_and(|k| k.starts_with('#')) {
        return true;
    }
    if lines.is_empty() {
        return false;
    }
    let last = lines.len() - 1;
    let row = site.row.min(last);
    let line = lines[row];
    let macro_ish = |w: &str| is_caps_macro(w) || macros.contains(w);
    // Directive on this line or inside the error span (a `#if` that cuts a
    // declaration), or inside a multi-line `#define` (continuation
    // backslash). A directive merely nearby is not enough.
    let span_end = site.end_row.min(last).max(row);
    if lines[row..=span_end]
        .iter()
        .any(|l| is_conditional_directive(l))
    {
        return true;
    }
    let end = site.end_row.max(row + 2).min(last);
    if row > 0 && lines[row - 1].trim_end().ends_with('\\') {
        return true;
    }
    if lines[row..=end]
        .iter()
        .any(|l| line_has_macro_use(l, &macro_ish))
    {
        return true;
    }
    // Unterminated macro line just above: `NAMESPACE_BEGIN` / `TEST_END`.
    if let Some(prev) = lines[..row].iter().rev().find(|l| !l.trim().is_empty()) {
        if is_solo_macro_line(&tokenize(prev), &macro_ish) {
            return true;
        }
    }
    let toks = tokenize(line);
    if has_adjacent_identifier_macro(&toks) {
        return true;
    }
    let t = line.trim();
    // Empty macro argument (`ph_gen(, a, b)`) is never valid C/C++.
    if toks
        .windows(2)
        .any(|w| matches!(w, [Tok::P('(' | ','), Tok::P(',')]))
    {
        return true;
    }
    // File-scope `gen(a, b)` with no `;`: a declaration-generating macro.
    if site.missing.as_deref() == Some(";")
        && !line.starts_with([' ', '\t'])
        && matches!(toks.as_slice(), [Tok::Id(f), Tok::P('('), .., Tok::P(')')] if !is_keyword(f))
    {
        return true;
    }
    // Statement macro inside a body: `    ql_foreach(x, &l, link) {`.
    if line.starts_with([' ', '\t'])
        && matches!(toks.as_slice(), [Tok::Id(f), Tok::P('('), .., Tok::P(')'), Tok::P('{')] if !is_keyword(f))
    {
        return true;
    }
    // Type passed as a macro argument next to a comma: `va_arg(ap, int *)`,
    // `cast(unsigned char, c)`, `M(T*, f)`, `CHECK_THROWS_AS(x, ns::error&)`.
    // A segment at line start may continue a multi-line macro call.
    let mut start = 0usize;
    let bytes: Vec<(usize, char)> = t.char_indices().collect();
    for (k, &(i, c)) in bytes.iter().enumerate() {
        let is_end = matches!(c, '(' | ',' | ')' | ';');
        if !is_end && k + 1 != bytes.len() {
            continue;
        }
        let stop = if is_end { i } else { t.len() };
        let arg = &t[start..stop];
        let before = t[..start].chars().last();
        let after = t[stop..].chars().next();
        let at_start = start == 0;
        start = i + c.len_utf8();
        if !at_start && before != Some(',') && after != Some(',') {
            continue;
        }
        if at_start && after != Some(',') && after != Some(')') {
            continue;
        }
        let a = arg.trim().trim_end_matches(['*', '&', ' ']);
        let words: Vec<&str> = tokenize(a)
            .into_iter()
            .filter_map(|t| match t {
                Tok::Id(w) => Some(w),
                _ => None,
            })
            .collect();
        let pointer_or_ref = arg.trim_end().ends_with(['*', '&']);
        let all_types = words.iter().all(|w| is_type_word(w));
        let type_path = a
            .chars()
            .all(|c| c.is_alphanumeric() || "_:<> ".contains(c));
        if !words.is_empty()
            && (all_types && (pointer_or_ref || words.len() > 1) || pointer_or_ref && type_path)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_file_error_skips_leading_comment() {
        // A missing `#endif` makes tree-sitter-cpp wrap everything into
        // one ERROR node starting at the licence comment.
        let src = "// Copyright 2020 Someone\n// licence text\n#include <x>\n#ifdef FOO\nint a;\n";
        let d = diagnose_syntax(Language::Cpp, src).unwrap();
        assert!(!d.what.contains("Copyright"), "{d:?}");
        assert!(d.line >= 3, "{d:?}");
    }

    #[test]
    fn reports_innermost_token_with_line_snippet() {
        let src = "def a():\n    pass\n\ndef b(:\n    return 1\n";
        let d = diagnose_syntax(Language::Python, src).unwrap();
        assert_eq!(d.line, 4, "{d:?}");
        assert!(d.what.contains("def b(:"), "{d:?}");
        assert_eq!(d.kind, SyntaxErrorKind::Syntax);
    }

    #[test]
    fn c_family_diagnoses_the_kept_rewrite_not_errors_it_fixed() {
        // Line 1-4: a comment in a multi-line `#define` (tree-sitter-c #319),
        // fixed by the cfamily rewrite. Line 6: a real error it cannot fix.
        // The retry is still partial but keeps every symbol, so it is the
        // indexed parse and the diagnosis must point at line 6.
        let src = "#define B { \\\n  /* c */ \\\n  0 \\\n}\nint ok(void);\nint bad( { ;\nint after(void);\n";
        for lang in [Language::C, Language::Cpp] {
            let raw = diagnose_syntax(lang, src).expect("raw partial");
            assert!(
                raw.line < 6,
                "{lang:?}: the raw tree errs before line 6 ({raw:?}), else the test proves nothing"
            );
            let path = crate::types::RelPath::new("a.h");
            let indexed = super::super::ParserPool::new()
                .parse(lang, &path, src)
                .unwrap();
            assert!(
                indexed.partial && indexed.c_family_rewrite,
                "{lang:?}: still-partial retry kept"
            );
            let d = diagnose_indexed_syntax(lang, src, true).expect("still partial");
            assert_eq!(d.line, 6, "{lang:?}: {d:?}");
            assert!(
                d.what.contains("int bad("),
                "original text is quoted: {d:?}"
            );
        }
    }

    #[test]
    fn c_family_clean_after_rewrite_has_no_diagnosis() {
        let src = "#define B { \\\n  /* c */ \\\n  0 \\\n}\nint ok(void);\n";
        assert!(diagnose_syntax(Language::C, src).is_some(), "raw text errs");
        assert!(diagnose_indexed_syntax(Language::C, src, true).is_none());
    }

    #[test]
    fn clean_source_has_no_diagnosis() {
        assert_eq!(
            diagnose_syntax(Language::C, "int main(void) { return 0; }\n"),
            None
        );
    }

    #[test]
    fn c_macro_errors_are_preprocessor_kind() {
        for src in [
            // attribute macro before a type
            "static redisAtomic size_t used = 0;\nint ok(void) { return 1; }\n",
            // statement macro
            "void f(void) {\n    ql_foreach(x, &list, link) {\n        use(x);\n    }\n}\n",
            // ALL_CAPS namespace macro (not in the cfamily macro table)
            "MYLIB_NAMESPACE_BEGIN\nclass basic_json {};\nMYLIB_NAMESPACE_END\n",
            // directive splitting a brace (not an `extern "C"` guard)
            "#ifdef USE_NS\nnamespace x {\n#endif\nint api(void);\n#ifdef USE_NS\n}\n#endif\n",
        ] {
            let lang = if src.contains("class") || src.contains("namespace") {
                Language::Cpp
            } else {
                Language::C
            };
            let d = diagnose_syntax(lang, src).expect("partial");
            assert_eq!(d.kind, SyntaxErrorKind::Preprocessor, "{src}\n{d:?}");
        }
        // Fixed by the cfamily rewrite (table macro, `extern "C"` guard): the
        // raw text is still classified, the indexed (rewritten) parse is clean.
        for (lang, src) in [
            (
                Language::Cpp,
                "NLOHMANN_JSON_NAMESPACE_BEGIN\nclass basic_json {};\nNLOHMANN_JSON_NAMESPACE_END\n",
            ),
            (
                Language::C,
                "#ifdef __cplusplus\nextern \"C\" {\n#endif\nint api(void);\n#ifdef __cplusplus\n}\n#endif\n",
            ),
        ] {
            let d = diagnose_syntax(lang, src).expect("raw partial");
            assert_eq!(d.kind, SyntaxErrorKind::Preprocessor, "{src}\n{d:?}");
            assert_eq!(diagnose_indexed_syntax(lang, src, true), None, "{src}");
        }
    }

    #[test]
    fn plain_c_syntax_error_is_syntax_kind() {
        let src = "int f(void) {\n    return 1 +;\n}\nint g(void { return 2; }\n";
        let d = diagnose_syntax(Language::C, src).unwrap();
        assert_eq!(d.kind, SyntaxErrorKind::Syntax, "{d:?}");
    }

    #[test]
    fn non_c_languages_are_never_preprocessor_kind() {
        let src = "public class X\n#if A\n : Base\n#endif\n{\n}\n";
        let d = diagnose_syntax(Language::CSharp, src).expect("partial");
        assert_eq!(d.kind, SyntaxErrorKind::Syntax, "{d:?}");
    }

    #[test]
    fn include_guard_block_reports_the_nested_error_not_the_guard() {
        // The unclosed body makes tree-sitter wrap the whole `#ifndef` guard
        // into one ERROR node; the real error is the nested one.
        let src = "/* licence */\n#ifndef CONN_H\n#define CONN_H\n#include <stdio.h>\n\nstatic inline int ok(void) {\n    return 1;\n}\n\nstatic inline int w(int c) {\n    return write(c) int\n}\n\n#endif\n";
        let d = diagnose_syntax(Language::C, src).unwrap();
        assert!((10..=11).contains(&d.line), "{d:?}");
        assert!(!d.what.contains("#ifndef"), "{d:?}");
        assert_eq!(d.kind, SyntaxErrorKind::Syntax, "{d:?}");
    }

    #[test]
    fn plain_errors_next_to_constants_or_directives_stay_syntax_kind() {
        for src in [
            // ALL_CAPS constants in an expression are not macro syntax.
            "int f(int m) {\n    if (m == REDIS_OK {\n        return C_ERR;\n    }\n    return C_OK;\n}\n",
            // A directive above an ordinary error is not the cause.
            "void g(int fd) {\n#ifdef HAVE_EVENT_FD\n    close(fd;\n#else\n    close(fd);\n#endif\n}\n",
        ] {
            let d = diagnose_syntax(Language::C, src).unwrap();
            assert_eq!(d.kind, SyntaxErrorKind::Syntax, "{src}\n{d:?}");
        }
    }

    #[test]
    fn missing_token_is_reported_on_the_code_line_not_a_trailing_comment() {
        let src = "int f(void) {\n    int a = 1\n    /* explain */\n    return a;\n}\n";
        let d = diagnose_syntax(Language::C, src).unwrap();
        assert_eq!(d.line, 2, "{d:?}");
        assert!(d.what.starts_with("missing `;`"), "{d:?}");
    }

    #[test]
    fn snippet_is_capped_on_char_boundary() {
        let long = "é".repeat(100);
        let s = snippet(&long);
        assert!(s.len() <= SNIPPET_MAX + 3, "{}", s.len());
    }
}
