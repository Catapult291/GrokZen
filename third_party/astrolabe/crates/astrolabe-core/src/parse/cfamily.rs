//! C / C++ retry rewrites for constructs tree-sitter-c / tree-sitter-cpp
//! cannot parse without a preprocessor.
//!
//! `parse/mod.rs` calls [`retry`] only when the first parse of a C or C++
//! file has syntax errors. Every rewrite is **length-preserving** and keeps
//! every `\n`, so byte offsets, lines and columns of the retry point at the
//! original text. All rewrites look at the current file only (no
//! cross-file macro table), so the parse result still depends on one file.
//!
//! The rewrites:
//!
//! 1. Comments inside a multi-line `#define` are blanked
//!    (tree-sitter-c issue #319: `#define B { \ /* c */ \ 0 \ }` turns the
//!    define and the following declaration into an ERROR node).
//! 2. `#if __has_include(<x>)` / `FMT_HAS_INCLUDE("x")`: the header operand
//!    is not a preprocessor expression for the grammars, so the whole
//!    conditional (and the symbols inside it) is lost. The operand becomes
//!    an identifier of the same length (`(<version>)` → `(_version_)`).
//! 3. `#ifdef __cplusplus` blocks that hold only `extern "C" {` or its
//!    closing `}`: the braces straddle conditional blocks, which neither
//!    grammar accepts. C blanks the whole block (`__cplusplus` is undefined
//!    in C); C++ blanks only the directive lines and keeps the linkage
//!    specification.
//! 4. A small built-in table of well-known library macros used as syntax
//!    (nlohmann/json, {fmt}, doctest), rewritten to equivalent valid code of
//!    the same or shorter length (padded with spaces); C++-only entries
//!    (`namespace`, `constexpr`, ...) are skipped for C. In C++ also
//!    doctest/Catch2 `TEST_CASE("x") {` (arguments blanked, the shape
//!    GoogleTest's `TEST(a, b) {` already parses as) and
//!    `SECTION("x") {` / `SUBCASE("x") {` (→ `if ("x") {`).
//!
//! [`retry`] keeps the rewritten parse only when it is clean, or when it
//! keeps every symbol (kind and name) of the first parse and has at least as
//! many symbols. Clean first parses never reach this module.

use std::collections::HashSet;

use super::{ParseError, ParsedFile};
use crate::types::Language;

/// Re-parse `source` after the rewrites when they change anything and the
/// result is better than `first`. `None` keeps the first parse.
pub(super) fn retry(
    lang: Language,
    source: &str,
    first: &ParsedFile,
    parse: impl FnOnce(&str) -> Result<ParsedFile, ParseError>,
) -> Option<ParsedFile> {
    kept_rewrite(lang, source, first, parse).map(|(_, parsed)| parsed)
}

/// [`retry`] that also returns the rewritten text the kept parse came from
/// (same byte length and lines as `source`), so diagnostics can look at the
/// tree that was actually indexed.
pub(super) fn kept_rewrite(
    lang: Language,
    source: &str,
    first: &ParsedFile,
    parse: impl FnOnce(&str) -> Result<ParsedFile, ParseError>,
) -> Option<(String, ParsedFile)> {
    let fixed = rewrite(lang, source)?;
    let second = parse(&fixed).ok()?;
    better(first, &second).then_some((fixed, second))
}

fn better(first: &ParsedFile, second: &ParsedFile) -> bool {
    if !second.partial {
        return true;
    }
    if second.symbols.len() < first.symbols.len() {
        return false;
    }
    let kept: HashSet<(_, &str)> = second
        .symbols
        .iter()
        .map(|s| (s.kind, s.name.as_str()))
        .collect();
    first
        .symbols
        .iter()
        .all(|s| kept.contains(&(s.kind, s.name.as_str())))
}

/// All rewrites; `None` when none applies.
pub(super) fn rewrite(lang: Language, source: &str) -> Option<String> {
    let cpp = match lang {
        Language::C => false,
        Language::Cpp => true,
        _ => return None,
    };
    let mut buf = source.as_bytes().to_vec();
    let mask = classify(&buf);
    let mut changed = blank_define_comments(&mut buf, &mask);
    changed |= rewrite_has_include(&mut buf, &mask);
    changed |= blank_extern_c_guards(&mut buf, &mask, cpp);
    changed |= rewrite_known_macros(&mut buf, &mask, cpp);
    if !changed {
        return None;
    }
    // Only ASCII bytes replaced whole characters, so this cannot fail.
    String::from_utf8(buf).ok()
}

/// Per-byte classification of the source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Code,
    Comment,
    Literal,
    /// Preprocessor directive text (outside its comments).
    Directive,
    /// Comment inside a preprocessor directive.
    DirectiveComment,
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True when the newline at `i` is escaped by a preceding `\` (allowing `\r`).
fn escaped_newline(b: &[u8], i: usize) -> bool {
    let mut j = i;
    if j > 0 && b[j - 1] == b'\r' {
        j -= 1;
    }
    j > 0 && b[j - 1] == b'\\'
}

fn classify(b: &[u8]) -> Vec<Class> {
    let mut mask = vec![Class::Code; b.len()];
    let mut i = 0;
    let mut line_start = true;
    let mut in_directive = false;
    while i < b.len() {
        let c = b[i];
        let (code, comment) = if in_directive {
            (Class::Directive, Class::DirectiveComment)
        } else {
            (Class::Code, Class::Comment)
        };
        if c == b'\n' {
            mask[i] = code;
            if in_directive && !escaped_newline(b, i) {
                in_directive = false;
            }
            line_start = true;
            i += 1;
            continue;
        }
        if line_start && (c == b' ' || c == b'\t' || c == b'\r' || c == 0x0c) {
            mask[i] = code;
            i += 1;
            continue;
        }
        if line_start && c == b'#' && !in_directive {
            in_directive = true;
            line_start = false;
            mask[i] = Class::Directive;
            i += 1;
            continue;
        }
        line_start = false;
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            // Line comment; a trailing `\` continues it on the next line.
            let mut j = i;
            while j < b.len() {
                if b[j] == b'\n' && !escaped_newline(b, j) {
                    break;
                }
                mask[j] = comment;
                j += 1;
            }
            i = j;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = find(b, i + 2, b"*/").map_or(b.len(), |e| e + 2);
            for m in &mut mask[i..end] {
                *m = comment;
            }
            i = end;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let prev_ident = i > 0 && is_ident(b[i - 1]);
            if c == b'\'' && prev_ident && number_before(b, i) {
                // C++14 digit separator (`1'000`).
                mask[i] = code;
                i += 1;
                continue;
            }
            if c == b'"' && prev_ident && raw_prefix(b, i) {
                if let Some(end) = raw_string_end(b, i) {
                    let lit = if in_directive {
                        Class::Directive
                    } else {
                        Class::Literal
                    };
                    for m in &mut mask[i..end] {
                        *m = lit;
                    }
                    i = end;
                    continue;
                }
            }
            let mut j = i + 1;
            while j < b.len() && b[j] != c && b[j] != b'\n' {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            // An unterminated literal stops before the newline.
            let end = if j < b.len() && b[j] == c { j + 1 } else { j };
            let lit = if in_directive {
                Class::Directive
            } else {
                Class::Literal
            };
            for m in &mut mask[i..end] {
                *m = lit;
            }
            i = end;
            continue;
        }
        mask[i] = code;
        i += 1;
    }
    mask
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| p + from)
}

/// The identifier-ish token ending right before `i` starts with a digit.
fn number_before(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && (is_ident(b[j - 1]) || b[j - 1] == b'\'' || b[j - 1] == b'.') {
        j -= 1;
    }
    b[j].is_ascii_digit()
}

/// `R"`, `u8R"`, `LR"`, `uR"`, `UR"` immediately before the quote at `i`.
fn raw_prefix(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && is_ident(b[j - 1]) {
        j -= 1;
    }
    matches!(&b[j..i], b"R" | b"u8R" | b"LR" | b"uR" | b"UR")
}

/// End (exclusive) of the raw string literal whose `"` is at `i`.
fn raw_string_end(b: &[u8], i: usize) -> Option<usize> {
    let open = i + 1 + b[i + 1..].iter().take(17).position(|&c| c == b'(')?;
    let delim = &b[i + 1..open];
    if delim
        .iter()
        .any(|&c| c == b' ' || c == b'\\' || c == b')' || c == b'\n' || c == b'"')
    {
        return None;
    }
    let mut close = Vec::with_capacity(delim.len() + 2);
    close.push(b')');
    close.extend_from_slice(delim);
    close.push(b'"');
    find(b, open + 1, &close).map(|e| e + close.len())
}

/// Replace `b[range]` with spaces, keeping newlines and the `\` of escaped
/// newlines. Returns whether anything changed.
fn blank(b: &mut [u8], from: usize, to: usize) -> bool {
    let mut changed = false;
    for i in from..to {
        let c = b[i];
        if c == b'\n' || c == b' ' || c == b'\r' {
            continue;
        }
        if c == b'\\' && (b.get(i + 1) == Some(&b'\n') || b.get(i + 1) == Some(&b'\r')) {
            continue;
        }
        b[i] = b' ';
        changed = true;
    }
    changed
}

/// Logical preprocessor directives: `(start, end)` byte ranges, `start` at
/// the `#`, `end` at the terminating newline (or EOF).
fn directives(b: &[u8], mask: &[Class]) -> Vec<(usize, usize)> {
    let in_dir = |i: usize| matches!(mask[i], Class::Directive | Class::DirectiveComment);
    let mut out = Vec::new();
    let mut i = 0;
    while i < mask.len() {
        if !in_dir(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < mask.len() && in_dir(i) {
            let end_of_line =
                b[i] == b'\n' && mask[i] == Class::Directive && !escaped_newline(b, i);
            i += 1;
            if end_of_line {
                break;
            }
        }
        out.push((start, i));
    }
    out
}

/// Directive keyword (`define`, `if`, ...) of the directive starting at `start`.
fn directive_keyword(b: &[u8], start: usize, end: usize) -> &[u8] {
    let mut i = start + 1;
    while i < end && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let k = i;
    while i < end && is_ident(b[i]) {
        i += 1;
    }
    &b[k..i]
}

/// Rewrite 1: blank comments inside `#define`s that span more than one line.
fn blank_define_comments(b: &mut [u8], mask: &[Class]) -> bool {
    let mut changed = false;
    for (start, end) in directives(b, mask) {
        if directive_keyword(b, start, end) != b"define" {
            continue;
        }
        // Spans more than one line: a newline before the terminating one.
        let body_end = if b[end - 1] == b'\n' { end - 1 } else { end };
        if !b[start..body_end].contains(&b'\n') {
            continue;
        }
        let mut i = start;
        while i < end {
            if mask[i] == Class::DirectiveComment {
                let s = i;
                while i < end && mask[i] == Class::DirectiveComment {
                    i += 1;
                }
                changed |= blank(b, s, i);
            } else {
                i += 1;
            }
        }
    }
    changed
}

/// Rewrite 2: `#if` / `#elif` operands `(<hdr>)` / `("hdr")` become `(_hdr_)`.
fn rewrite_has_include(b: &mut [u8], mask: &[Class]) -> bool {
    let mut changed = false;
    for (start, end) in directives(b, mask) {
        let kw = directive_keyword(b, start, end);
        if kw != b"if" && kw != b"elif" {
            continue;
        }
        let mut i = start;
        while i < end {
            if b[i] != b'(' || mask[i] != Class::Directive {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < end && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let close = match b.get(j) {
                Some(b'<') => b'>',
                Some(b'"') => b'"',
                _ => {
                    i += 1;
                    continue;
                }
            };
            let Some(len) = b[j + 1..end]
                .iter()
                .position(|&c| c == close || c == b'\n' || c == b')')
            else {
                i += 1;
                continue;
            };
            let k = j + 1 + len;
            if b[k] != close || b[k + 1..end].iter().find(|c| **c != b' ') != Some(&b')') {
                i += 1;
                continue;
            }
            for c in &mut b[j..=k] {
                if !is_ident(*c) {
                    *c = b'_';
                }
            }
            changed = true;
            i = k + 1;
        }
    }
    changed
}

/// Code of `b[from..to]` with comments removed and whitespace collapsed.
fn code_text(b: &[u8], mask: &[Class], from: usize, to: usize) -> String {
    let mut s = String::new();
    for i in from..to {
        if matches!(mask[i], Class::Comment | Class::DirectiveComment) {
            s.push(' ');
        } else {
            s.push(b[i] as char);
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Directive is `#ifdef __cplusplus`, `#if defined(__cplusplus)`,
/// `#if defined __cplusplus` or `#if __cplusplus`.
fn is_cplusplus_guard(text: &str) -> bool {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    matches!(
        t.as_str(),
        "#ifdef__cplusplus"
            | "#ifdefined(__cplusplus)"
            | "#ifdefined__cplusplus"
            | "#if__cplusplus"
    )
}

/// Rewrite 3: `#ifdef __cplusplus` / `extern "C" {` or `}` / `#endif`.
fn blank_extern_c_guards(b: &mut [u8], mask: &[Class], cpp: bool) -> bool {
    let dirs = directives(b, mask);
    let mut changed = false;
    for w in dirs.windows(2) {
        let (open_s, open_e) = w[0];
        let (end_s, end_e) = w[1];
        if !is_cplusplus_guard(&code_text(b, mask, open_s, open_e)) {
            continue;
        }
        if directive_keyword(b, end_s, end_e) != b"endif" {
            continue;
        }
        let body = code_text(b, mask, open_e, end_s);
        let body_ok = body == "}"
            || body
                .strip_prefix("extern \"C\"")
                .is_some_and(|r| r.trim() == "{");
        if !body_ok {
            continue;
        }
        changed |= blank(b, open_s, open_e);
        changed |= blank(b, end_s, end_e);
        if !cpp {
            changed |= blank(b, open_e, end_s);
        }
    }
    changed
}

/// How a known macro is rewritten.
enum Macro {
    /// Object-like: replaced by this text, padded with spaces.
    Object(&'static str),
    /// Function-like: the name and its balanced argument list are replaced
    /// by this text, padded with spaces (newlines kept).
    Call(&'static str),
}

/// One [`KNOWN_MACROS`] entry: `name` matches exactly, or as a prefix when
/// it ends with `*`; `cpp` entries are applied to C++ only.
struct Known {
    name: &'static str,
    cpp: bool,
    rewrite: Macro,
}

const fn obj(name: &'static str, text: &'static str, cpp: bool) -> Known {
    Known {
        name,
        cpp,
        rewrite: Macro::Object(text),
    }
}

const fn call(name: &'static str, text: &'static str, cpp: bool) -> Known {
    Known {
        name,
        cpp,
        rewrite: Macro::Call(text),
    }
}

/// Well-known library macros used as syntax. A replacement longer than the
/// macro name is never applied.
const KNOWN_MACROS: &[Known] = &[
    // nlohmann/json
    obj(
        "NLOHMANN_JSON_NAMESPACE_BEGIN",
        "namespace nlohmann {",
        true,
    ),
    obj("NLOHMANN_JSON_NAMESPACE_END", "}", true),
    obj(
        "NLOHMANN_BASIC_JSON_TPL_DECLARATION",
        "template<class>",
        true,
    ),
    obj("NLOHMANN_BASIC_JSON_TPL", "basic_json", true),
    obj("JSON_PRIVATE_UNLESS_TESTED", "private", true),
    // These expand to to_json/from_json definitions; used without `;`.
    call("NLOHMANN_DEFINE_TYPE_*", "", true),
    call("NLOHMANN_DEFINE_DERIVED_TYPE_*", "", true),
    call("NLOHMANN_JSON_SERIALIZE_ENUM*", "", true),
    obj("JSON_HEDLEY_WARN_UNUSED_RESULT", "", false),
    obj("JSON_HEDLEY_RETURNS_NON_NULL", "", false),
    obj("JSON_HEDLEY_NO_RETURN", "", false),
    obj("JSON_HEDLEY_CONST", "", false),
    obj("JSON_HEDLEY_PURE", "", false),
    obj("JSON_HEDLEY_DIAGNOSTIC_PUSH", "", false),
    obj("JSON_HEDLEY_DIAGNOSTIC_POP", "", false),
    obj("JSON_HEDLEY_DIAGNOSTIC_DISABLE_DEPRECATED", "", false),
    obj("JSON_HEDLEY_DIAGNOSTIC_DISABLE_UNKNOWN_PRAGMAS", "", false),
    obj(
        "JSON_HEDLEY_DIAGNOSTIC_DISABLE_UNKNOWN_CPP_ATTRIBUTES",
        "",
        false,
    ),
    obj("JSON_HEDLEY_DIAGNOSTIC_DISABLE_CAST_QUAL", "", false),
    call("JSON_HEDLEY_NON_NULL", "", false),
    call("JSON_HEDLEY_DEPRECATED", "", false),
    call("JSON_HEDLEY_DEPRECATED_FOR", "", false),
    call("JSON_HEDLEY_PRAGMA", "", false),
    // {fmt}
    obj("FMT_BEGIN_NAMESPACE", "namespace fmt {", true),
    obj("FMT_END_NAMESPACE", "}", true),
    obj("FMT_BEGIN_DETAIL_NAMESPACE", "namespace detail {", true),
    obj("FMT_END_DETAIL_NAMESPACE", "}", true),
    obj("FMT_BEGIN_EXPORT", "", true),
    obj("FMT_END_EXPORT", "", true),
    obj("FMT_MODULE_EXPORT_BEGIN", "", true),
    obj("FMT_MODULE_EXPORT_END", "", true),
    obj("FMT_EXPORT", "", true),
    obj("FMT_MODULE_EXPORT", "", true),
    obj("FMT_API", "", false),
    obj("FMT_CAPI", "", false),
    obj("FMT_CONSTEXPR20", "constexpr", true),
    obj("FMT_CONSTEXPR", "constexpr", true),
    obj("FMT_CONSTEVAL", "constexpr", true),
    obj("FMT_INLINE", "inline", false),
    obj("FMT_NODISCARD", "", false),
    obj("FMT_NORETURN", "", false),
    obj("FMT_DEPRECATED", "", false),
    call("FMT_VISIBILITY", "", false),
    call("FMT_PRAGMA_CLANG", "", false),
    call("FMT_PRAGMA_GCC", "", false),
    call("FMT_MSC_WARNING", "", false),
    // `template <typename T, FMT_ENABLE_IF(...)>`: an unnamed `int` parameter.
    call("FMT_ENABLE_IF", "int", true),
    // doctest warning-suppression statements (no trailing `;`).
    obj("DOCTEST_CLANG_SUPPRESS_WARNING_POP", "", true),
    obj("DOCTEST_GCC_SUPPRESS_WARNING_POP", "", true),
    obj("DOCTEST_MSVC_SUPPRESS_WARNING_POP", "", true),
    obj("DOCTEST_CLANG_SUPPRESS_WARNING_PUSH", "", true),
    obj("DOCTEST_GCC_SUPPRESS_WARNING_PUSH", "", true),
    obj("DOCTEST_MSVC_SUPPRESS_WARNING_PUSH", "", true),
    call("DOCTEST_CLANG_SUPPRESS_WARNING", "", true),
    call("DOCTEST_GCC_SUPPRESS_WARNING", "", true),
    call("DOCTEST_MSVC_SUPPRESS_WARNING", "", true),
    call("DOCTEST_CLANG_SUPPRESS_WARNING_WITH_PUSH", "", true),
    call("DOCTEST_GCC_SUPPRESS_WARNING_WITH_PUSH", "", true),
    call("DOCTEST_MSVC_SUPPRESS_WARNING_WITH_PUSH", "", true),
];

fn known(word: &[u8], cpp: bool) -> Option<&'static Macro> {
    KNOWN_MACROS
        .iter()
        .filter(|k| cpp || !k.cpp)
        .find(|k| match k.name.strip_suffix('*') {
            Some(prefix) => word.starts_with(prefix.as_bytes()),
            None => k.name.as_bytes() == word,
        })
        .map(|k| &k.rewrite)
}

/// End (exclusive) of the balanced `( ... )` starting at the first
/// non-space byte from `i`, over code bytes only.
fn balanced_args(b: &[u8], mask: &[Class], i: usize) -> Option<usize> {
    let mut j = i;
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    if b.get(j) != Some(&b'(') || mask[j] != Class::Code {
        return None;
    }
    let mut depth = 0usize;
    for (k, &c) in b.iter().enumerate().skip(j) {
        if mask[k] != Class::Code {
            continue;
        }
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
            b';' | b'{' | b'}' => return None,
            _ => {}
        }
    }
    None
}

/// Identifier start at `i` in code: `(start, end)` of the word.
fn word_at(b: &[u8], mask: &[Class], i: usize) -> Option<(usize, usize)> {
    if mask[i] != Class::Code || !is_ident(b[i]) || (i > 0 && is_ident(b[i - 1])) {
        return None;
    }
    if b[i].is_ascii_digit() {
        return None;
    }
    let mut j = i;
    while j < b.len() && is_ident(b[j]) {
        j += 1;
    }
    Some((i, j))
}

/// Rewrite 4: [`KNOWN_MACROS`] in code (never in directives, comments or
/// literals), then the test-framework macros (C++).
fn rewrite_known_macros(b: &mut [u8], mask: &[Class], cpp: bool) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < b.len() {
        let Some((s, j)) = word_at(b, mask, i) else {
            i += 1;
            continue;
        };
        let word = &b[s..j];
        let candidate = [&b"NLOHMANN_"[..], b"JSON_", b"FMT_", b"DOCTEST_"]
            .iter()
            .any(|p| word.starts_with(p));
        let Some(m) = candidate.then(|| known(word, cpp)).flatten() else {
            i = j;
            continue;
        };
        let (end, text) = match m {
            Macro::Object(t) => (j, *t),
            Macro::Call(t) => match balanced_args(b, mask, j) {
                Some(e) => (e, *t),
                None => {
                    i = j;
                    continue;
                }
            },
        };
        if text.len() > j - s {
            i = j;
            continue;
        }
        b[s..s + text.len()].copy_from_slice(text.as_bytes());
        blank(b, s + text.len(), end);
        changed = true;
        i = end;
    }
    if cpp {
        changed |= rewrite_test_macros(b, mask);
    }
    changed
}

/// First code byte at or after `i`, skipping whitespace and comments.
fn next_code(b: &[u8], mask: &[Class], mut i: usize) -> Option<usize> {
    while i < b.len() && (b[i].is_ascii_whitespace() || mask[i] == Class::Comment) {
        i += 1;
    }
    (i < b.len()).then_some(i)
}

/// Line-leading `TEST_CASE("name") {` (doctest / Catch2) becomes
/// `TEST_CASE(      ) {`, the shape tree-sitter-cpp already parses as a
/// function definition for GoogleTest's `TEST(a, b) {` (symbol `TEST_CASE`,
/// like `TEST`). `SECTION("x") {` / `SUBCASE("x") {` becomes `if ("x") {`.
/// Only when the call is on one line and directly followed by `{`.
fn rewrite_test_macros(b: &mut [u8], mask: &[Class]) -> bool {
    let mut changed = false;
    let mut i = 0;
    let mut line_start = true;
    while i < b.len() {
        if b[i] == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if b[i] == b' ' || b[i] == b'\t' {
            i += 1;
            continue;
        }
        let at_start = std::mem::replace(&mut line_start, false);
        let Some((s, j)) = at_start.then(|| word_at(b, mask, i)).flatten() else {
            i += 1;
            continue;
        };
        let word = &b[s..j];
        let test = word == b"TEST_CASE";
        let section = matches!(word, b"SECTION" | b"SUBCASE");
        if !test && !section {
            i = j;
            continue;
        }
        let Some(end) = balanced_args(b, mask, j) else {
            i = j;
            continue;
        };
        if b[s..end].contains(&b'\n') || next_code(b, mask, end).map(|k| b[k]) != Some(b'{') {
            i = j;
            continue;
        }
        if section {
            b[s..s + 2].copy_from_slice(b"if");
            blank(b, s + 2, j);
        } else {
            // Keep `TEST_CASE(` and `)`, blank the arguments.
            let open = j + b[j..end].iter().position(|&c| c == b'(').unwrap_or(0);
            blank(b, open + 1, end - 1);
        }
        changed = true;
        i = end;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParserPool;
    use crate::types::{RelPath, SymbolKind};

    fn parse(lang: Language, path: &str, src: &str) -> ParsedFile {
        ParserPool::new()
            .parse(lang, &RelPath::new(path), src)
            .expect("parse")
    }

    fn names(p: &ParsedFile) -> Vec<&str> {
        p.symbols.iter().map(|s| s.name.as_str()).collect()
    }

    /// Every rewrite keeps the byte length and every newline.
    fn assert_shape(lang: Language, src: &str) -> String {
        let out = rewrite(lang, src).expect("rewrite applies");
        assert_eq!(out.len(), src.len());
        let nl = |s: &str| s.match_indices('\n').map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(nl(&out), nl(src));
        out
    }

    #[test]
    fn define_comment_no_longer_swallows_next_declaration() {
        // tree-sitter-c #319.
        let src = "#define B { \\\n  /* c */ \\\n  0 \\\n}\nint ok(void);\n";
        let out = assert_shape(Language::C, src);
        assert!(!out.contains("/*"));
        let p = parse(Language::C, "a.c", src);
        assert!(!p.partial);
        assert!(names(&p).contains(&"ok"), "{:?}", names(&p));
        let p = parse(
            Language::Cpp,
            "a.hpp",
            "#define E \\\n  } /* c */ \\\n  }  // x\nint ok;\n",
        );
        assert!(!p.partial);
    }

    #[test]
    fn single_line_define_comment_and_string_are_untouched() {
        let src = "#define X 1 /* one */\n#define S \"/* not a comment */ \\\n  x\"\n";
        assert!(rewrite(Language::C, src).is_none_or(|o| o.contains("/* one */")));
    }

    #[test]
    fn has_include_block_keeps_its_symbols() {
        for (lang, path, src) in [
            (Language::Cpp, "a.hpp", "#if __has_include(<version>)\nint v(void) { return 0; }\n#endif\n"),
            (Language::C, "a.c", "#if __has_include(\"version.h\")\nint v(void) { return 0; }\n#endif\n"),
            (
                Language::Cpp,
                "b.hpp",
                "#if FMT_CPLUSPLUS > 201703L && FMT_HAS_INCLUDE(<sys/x.h>)\nint v(void) { return 0; }\n#elif __has_include( <y> )\nint w(void) { return 1; }\n#endif\n",
            ),
        ] {
            let out = assert_shape(lang, src);
            assert!(!out.contains('<') || out.contains("> 2017"), "{out}");
            let p = parse(lang, path, src);
            assert!(!p.partial, "{src}");
            assert!(names(&p).contains(&"v"), "{src}: {:?}", names(&p));
        }
        // `#include <x>` lines are not conditions and stay as they are.
        assert!(rewrite(Language::C, "#include <x.h>\nint v(void) { return 0; }\n").is_none());
    }

    #[test]
    fn extern_c_guards() {
        let src = "#ifndef H\n#define H\n#ifdef __cplusplus\nextern \"C\" {\n#endif\nint f(void);\n#if defined(__cplusplus)\n} /* extern \"C\" */\n#endif\n#endif /* H */\n";
        let c = assert_shape(Language::C, src);
        assert!(!c.contains("extern"), "{c}");
        assert!(c.contains("#endif /* H */"), "{c}");
        let cpp = assert_shape(Language::Cpp, src);
        assert!(
            cpp.contains("extern \"C\" {") && !cpp.contains("__cplusplus"),
            "{cpp}"
        );
        for (lang, path) in [(Language::C, "a.h"), (Language::Cpp, "a.hpp")] {
            let p = parse(lang, path, src);
            assert!(!p.partial, "{lang:?}");
            assert!(names(&p).contains(&"f"), "{lang:?}: {:?}", names(&p));
        }
        // A guard with more than the brace is left alone.
        let other = "#ifdef __cplusplus\nextern \"C\" {\nint g();\n#endif\n";
        assert!(rewrite(Language::C, other).is_none());
    }

    #[test]
    fn known_macros_namespace_and_specifiers() {
        let src = "NLOHMANN_JSON_NAMESPACE_BEGIN\nnamespace detail {\nint f();\n}\nNLOHMANN_JSON_NAMESPACE_END\n";
        let out = assert_shape(Language::Cpp, src);
        assert!(out.starts_with("namespace nlohmann {"));
        let p = parse(Language::Cpp, "a.hpp", src);
        assert!(!p.partial);
        assert!(p
            .symbols
            .iter()
            .any(|s| s.name == "nlohmann" && s.kind == SymbolKind::Module));
        assert!(names(&p).contains(&"f"));

        let fmt = "FMT_BEGIN_NAMESPACE\ntemplate <typename T, FMT_ENABLE_IF(std::is_integral<T>::value)>\nFMT_CONSTEXPR FMT_INLINE auto g(T x) -> int { return 0; }\nclass FMT_API buf {};\nFMT_END_NAMESPACE\n";
        assert_shape(Language::Cpp, fmt);
        let p = parse(Language::Cpp, "fmt.h", fmt);
        assert!(!p.partial);
        for n in ["fmt", "g", "buf"] {
            assert!(names(&p).contains(&n), "{n}: {:?}", names(&p));
        }
    }

    #[test]
    fn known_macros_skip_directives_comments_literals_and_cpp_only_entries_in_c() {
        let src = "#define FMT_API __attribute__((x))\n// FMT_API\nconst char* s = \"FMT_BEGIN_NAMESPACE\";\n";
        assert!(rewrite(Language::Cpp, src).is_none());
        // `namespace` must never be written into C; attribute-like erasures are.
        let c = "FMT_BEGIN_NAMESPACE\nFMT_CAPI int f(void);\n";
        let out = assert_shape(Language::C, c);
        assert!(out.starts_with("FMT_BEGIN_NAMESPACE\n") && !out.contains("FMT_CAPI"));
    }

    #[test]
    fn doctest_cases_and_sections_parse() {
        let src = "#include <doctest.h>\nstruct helper { int x; };\nTEST_CASE(\"algorithms \\\"q\\\" é\")\n{\n    SECTION(\"one\")\n    {\n        CHECK(run(1));\n    }\n}\nDOCTEST_CLANG_SUPPRESS_WARNING_PUSH\nint after(int a) { return a; }\n";
        let out = assert_shape(Language::Cpp, src);
        assert!(
            out.contains("TEST_CASE(") && out.contains("if     (\"one\")"),
            "{out}"
        );
        let p = parse(Language::Cpp, "t.cpp", src);
        assert!(!p.partial);
        for n in ["helper", "after", "TEST_CASE"] {
            assert!(names(&p).contains(&n), "{n}: {:?}", names(&p));
        }
        assert!(p.calls.iter().any(|c| c.0 == "run"));
    }

    #[test]
    fn clean_files_are_not_rewritten_and_partial_retry_keeps_symbols() {
        // Clean first pass: never reaches the rewrite (same result as raw).
        let clean = "namespace a { int f(); }\n";
        let p = parse(Language::Cpp, "c.hpp", clean);
        assert!(!p.partial);
        // A file that stays partial keeps every first-pass symbol.
        let src = "#ifdef __cplusplus\nextern \"C\" {\n#endif\nint keep(void);\nLUAI_FUNC int g() @@;\n#ifdef __cplusplus\n}\n#endif\n";
        let p = parse(Language::C, "p.c", src);
        assert!(names(&p).contains(&"keep"), "{:?}", names(&p));
    }

    #[test]
    fn digit_separators_and_raw_strings_do_not_open_literals() {
        let src = "int n = 1'000'000;\nconst char* r = R\"x(FMT_API \" ' )x\";\nFMT_API int f();\n";
        let out = assert_shape(Language::Cpp, src);
        assert!(out.contains("R\"x(FMT_API"), "{out}");
        assert!(!out.contains("FMT_API int"), "{out}");
    }
}
