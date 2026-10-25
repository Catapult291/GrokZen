//! Objective-C / Objective-C++ (`.m`, `.mm`, ObjC `.h`) extraction hooks.
//!
//! `parse/mod.rs` calls these hooks for [`Language::ObjC`] /
//! [`Language::ObjCpp`] only, so all language-specific post-processing lives
//! here and never in shared code.
//!
//! Three pieces beyond the query files:
//!
//! - [`symbol_name`]: selectors (`initWithURL:options:`), categories
//!   (`NSString(AFExtras)`), declarator peeling for properties / ivars, and
//!   `const` file-scope variables.
//! - [`preprocess`]: a **length-preserving** rewrite of the Cocoa macros
//!   tree-sitter-objc cannot parse (`typedef NS_ENUM(T, N)`,
//!   `NS_ASSUME_NONNULL_BEGIN`, trailing `API_AVAILABLE(...)` style
//!   attributes, React Native `RCT_EXPORT_METHOD`, brace-splitting `#if`
//!   branches). Byte offsets and line numbers are unchanged, so symbol lines
//!   and signatures still point at the original text. Without it one
//!   `NS_ENUM` swallows the rest of the file into an ERROR node.
//! - [`parse_with`]: ObjC++ (`.mm`) runs the ObjC grammar *and* the C++
//!   grammar and unions the results, because tree-sitter-objc has no C++
//!   (namespaces, templates, `::`, lambdas) and ~25% of `.mm` bytes end up in
//!   ERROR nodes otherwise.

use std::collections::HashSet;

use super::{ParseError, ParsedFile};
use crate::types::{Language, SymbolKind};

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    match definition.kind() {
        "method_declaration" | "method_definition" => (method_selector(definition, source), kind),
        "class_interface" | "class_implementation" => {
            match definition
                .child_by_field_name("category")
                .and_then(|c| c.utf8_text(source.as_bytes()).ok())
            {
                Some(category) => (format!("{name}({category})"), kind),
                None => (name.to_owned(), kind),
            }
        }
        "property_declaration" | "instance_variable" => (declarator_identifier(name), kind),
        "type_definition" | "field_declaration" => {
            let want = if definition.kind() == "type_definition" {
                "type_identifier"
            } else {
                "field_identifier"
            };
            (peel_declarator(definition, name, want, source), kind)
        }
        "declaration" if kind == SymbolKind::Variable => {
            file_scope_declaration(definition, name, source)
        }
        _ => (name.to_owned(), kind),
    }
}

/// The `want` identifier inside the declarator whose text is `name`:
/// `(^Block)(id)` → `Block`, `* _Nullable (*fn)(int)` → `fn`.
fn peel_declarator(
    definition: tree_sitter::Node<'_>,
    name: &str,
    want: &str,
    source: &str,
) -> String {
    let mut cursor = definition.walk();
    let declarator = definition
        .children_by_field_name("declarator", &mut cursor)
        .find(|d| d.utf8_text(source.as_bytes()).map(str::trim) == Ok(name));
    if let Some(d) = declarator {
        let mut stack = vec![d];
        while let Some(n) = stack.pop() {
            if n.kind() == want {
                return n.utf8_text(source.as_bytes()).unwrap_or(name).to_owned();
            }
            if n.kind() == "parameter_list" {
                continue;
            }
            let mut c = n.walk();
            let kids: Vec<_> = n.named_children(&mut c).collect();
            stack.extend(kids.into_iter().rev());
        }
    }
    declarator_identifier(name)
}

/// File-scope `declaration` matched with its whole declarator as `@name`:
/// a prototype (`void f(int);`) is a Function, a variable whose type or
/// pointer is `const` is a Const, anything else a Variable.
fn file_scope_declaration(
    definition: tree_sitter::Node<'_>,
    name: &str,
    source: &str,
) -> (String, SymbolKind) {
    let mut cursor = definition.walk();
    let declarator = definition
        .children_by_field_name("declarator", &mut cursor)
        .find(|d| d.utf8_text(source.as_bytes()).map(str::trim) == Ok(name));
    let Some(mut node) = declarator else {
        return (declarator_identifier(name), SymbolKind::Variable);
    };
    let mut kind = SymbolKind::Variable;
    loop {
        let next = match node.kind() {
            "identifier" => break,
            "function_declarator" => {
                let inner = node.child_by_field_name("declarator");
                if inner.is_some_and(|i| i.kind() == "identifier") {
                    kind = SymbolKind::Function;
                }
                inner
            }
            "parenthesized_declarator" => node.named_child(0),
            _ => node.child_by_field_name("declarator").or_else(|| {
                node.named_children(&mut node.walk())
                    .find(|c| c.kind().ends_with("declarator") || c.kind() == "identifier")
            }),
        };
        match next {
            Some(n) => node = n,
            None => return (declarator_identifier(name), kind),
        }
    }
    let ident = node.utf8_text(source.as_bytes()).unwrap_or(name).to_owned();
    if kind == SymbolKind::Variable {
        let head = &source[definition.start_byte()..node.start_byte()];
        if head
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|t| t == "const")
        {
            kind = SymbolKind::Const;
        }
    }
    (ident, kind)
}

/// Whether a definition is visible outside its file: `static` functions and
/// file-scope variables are not; everything else (classes, methods,
/// properties, extern symbols) is.
pub(super) fn exported(_name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    if !matches!(node.kind(), "function_definition" | "declaration") {
        return true;
    }
    let mut cursor = node.walk();
    let is_static = node.children(&mut cursor).any(|child| {
        child.kind() == "storage_class_specifier"
            && child.utf8_text(source.as_bytes()) == Ok("static")
    });
    !is_static
}

/// Callee name for a call match. Message sends become the full selector
/// (`[d setObject:o forKey:k]` → `setObject:forKey:`) so they line up with
/// method symbol names; plain C calls keep the captured identifier.
#[allow(dead_code)] // wired by the lead in `extract_calls`.
pub(super) fn call_name(call: tree_sitter::Node<'_>, name: &str, source: &str) -> String {
    if call.kind() != "message_expression" {
        return name.to_owned();
    }
    let mut selector = String::new();
    let mut cursor = call.walk();
    let children: Vec<_> = call.children(&mut cursor).collect();
    for (i, child) in children.iter().enumerate() {
        if call.field_name_for_child(i as u32) != Some("method") {
            continue;
        }
        selector.push_str(child.utf8_text(source.as_bytes()).unwrap_or(""));
        if children.get(i + 1).is_some_and(|n| n.kind() == ":") {
            selector.push(':');
        }
    }
    if selector.is_empty() {
        name.to_owned()
    } else {
        selector
    }
}

/// `- (void)foo:(int)a bar:(int)b` → `foo:bar:`; `- (void)run` → `run`;
/// an unnamed keyword (`foo:(int)a :(int)b`) contributes a bare `:`.
fn method_selector(node: tree_sitter::Node<'_>, source: &str) -> String {
    let mut selector = String::new();
    let mut cursor = node.walk();
    let named: Vec<_> = node.named_children(&mut cursor).collect();
    for (i, child) in named.iter().enumerate() {
        match child.kind() {
            "identifier" => {
                selector.push_str(child.utf8_text(source.as_bytes()).unwrap_or(""));
                if named
                    .get(i + 1)
                    .is_some_and(|n| n.kind() == "method_parameter")
                {
                    selector.push(':');
                }
            }
            "method_parameter" if i > 0 && named[i - 1].kind() == "method_parameter" => {
                selector.push(':');
            }
            _ => {}
        }
    }
    selector
}

const DECLARATOR_NOISE: &[&str] = &[
    "const",
    "volatile",
    "_Nullable",
    "_Nonnull",
    "_Null_unspecified",
    "__nullable",
    "__nonnull",
    "__null_unspecified",
    "nullable",
    "nonnull",
    "__kindof",
    "__weak",
    "__strong",
    "__unsafe_unretained",
    "__autoreleasing",
    "__block",
    "_Atomic",
    "restrict",
];

/// Identifier of a struct declarator's text: `*session` → `session`,
/// `(^completion)(BOOL)` → `completion`, `(*fn)(int)` → `fn`,
/// `flag : 1` → `flag`, `buf[16]` → `buf`.
fn declarator_identifier(text: &str) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    // Function / block pointer: the identifier follows `^` or `*` inside
    // the first parenthesised group.
    if let Some(open) = text.find('(') {
        let inner = &text[open + 1..];
        if let Some(pos) = inner.find(['^', '*']) {
            if let Some(tok) = inner[pos + 1..]
                .split(|c: char| !is_ident(c))
                .find(|t| !t.is_empty() && !DECLARATOR_NOISE.contains(t))
            {
                return tok.to_owned();
            }
        }
    }
    let mut head = text.split('[').next().unwrap_or(text);
    // Bit-field width (`flag : 1`), but not a C++ scope (`std::string s`).
    if let Some((before, width)) = head.rsplit_once(':') {
        let width = width.trim();
        if !before.ends_with(':') && !width.is_empty() && width.bytes().all(|b| b.is_ascii_digit())
        {
            head = before;
        }
    }
    head.split(|c: char| !is_ident(c))
        .rfind(|t| !t.is_empty() && !DECLARATOR_NOISE.contains(t))
        .unwrap_or(text.trim())
        .to_owned()
}

// ------------------------------------------------------------------ ObjC++

/// Parse entry for ObjC (`lang == ObjC`) and ObjC++ (`lang == ObjCpp`).
///
/// `raw` runs one grammar + its queries on a source string (the plain
/// tree-sitter path in `ParserPool`, without this dispatch). ObjC source is
/// macro-normalised first. ObjC++ additionally runs the C++ grammar on the
/// original text and adds every C++ symbol / call / include the ObjC pass
/// did not already produce (same name and line), so ObjC declarations come
/// from the ObjC grammar and namespaces / classes / templates / lambdas
/// from the C++ one.
pub(super) fn parse_with(
    lang: Language,
    source: &str,
    mut raw: impl FnMut(Language, &str) -> Result<ParsedFile, ParseError>,
) -> Result<ParsedFile, ParseError> {
    let normalised = preprocess(source);
    let mut objc_source = normalised.unwrap_or_else(|| source.to_owned());
    if lang == Language::ObjCpp {
        objc_source = mask_cpp(&objc_source);
    }
    let mut parsed = raw(Language::ObjC, &objc_source)?;
    restore_signatures(&mut parsed.symbols, &objc_source, source);
    if let Some(heads) = alternate_method_heads(source) {
        // `#if SD_MAC - (void)a #else - (void)b #endif {…}`: preprocess keeps
        // the first head; the other platforms' selectors come from a
        // line-aligned stub `@interface` holding just those heads.
        if let Ok(alt) = raw(Language::ObjC, &heads) {
            let seen: HashSet<(String, u32)> = parsed
                .symbols
                .iter()
                .map(|s| (s.name.clone(), s.start_line))
                .collect();
            let original: Vec<&str> = source.lines().collect();
            for mut symbol in alt.symbols {
                if symbol.kind != SymbolKind::Method
                    || seen.contains(&(symbol.name.clone(), symbol.start_line))
                {
                    continue;
                }
                if let Some(line) = original.get(symbol.start_line as usize - 1) {
                    symbol.signature = super::signature_from_row(line, 0);
                }
                parsed.symbols.push(symbol);
            }
            parsed.symbols.sort_by_key(|s| s.start_line);
        }
    }
    if lang != Language::ObjCpp {
        return Ok(parsed);
    }
    // C++ the mask left behind can still make the ObjC grammar invent names
    // (`std__unique_ptr`); keep only names that occur in the original text.
    let lines: Vec<&str> = source.lines().collect();
    parsed.symbols.retain(|s| {
        let key = s.name.split([':', '(']).next().unwrap_or(&s.name);
        is_symbol_name(&s.name)
            && (s.start_line as usize..=s.end_line as usize)
                .filter_map(|l| lines.get(l.wrapping_sub(1)))
                .take(16)
                .any(|l| l.contains(key))
    });
    let cpp_source = mask_objc(source);
    let cpp = match raw(Language::Cpp, &cpp_source) {
        Ok(mut cpp) => {
            restore_signatures(&mut cpp.symbols, &cpp_source, source);
            cpp
        }
        // The ObjC pass already succeeded; a C++ timeout must not lose it.
        Err(_) => return Ok(parsed),
    };
    let seen: HashSet<(String, u32)> = parsed
        .symbols
        .iter()
        .map(|s| (s.name.clone(), s.start_line))
        .collect();
    let objc_rows = objc_construct_rows(source);
    for symbol in cpp.symbols {
        // C++ grammar misreads ObjC lines (`@interface`, `- (void)x`) as
        // declarations; only keep symbols that start outside ObjC constructs.
        if seen.contains(&(symbol.name.clone(), symbol.start_line))
            || objc_rows.contains(&symbol.start_line)
        {
            continue;
        }
        parsed.symbols.push(symbol);
    }
    let calls: HashSet<(String, u32)> = parsed.calls.iter().cloned().collect();
    parsed
        .calls
        .extend(cpp.calls.into_iter().filter(|c| !calls.contains(c)));
    for import in cpp.imports {
        if !parsed.imports.contains(&import) {
            parsed.imports.push(import);
        }
    }
    parsed.symbols.sort_by_key(|s| s.start_line);
    Ok(parsed)
}

/// Line-aligned stub for the non-first branches of per-platform method
/// heads sharing one body:
///
/// ```text
/// #if SD_MAC
/// - (void)viewDidMoveToWindow
/// #else                        → @interface _
/// - (void)didMoveToWindow      → - (void)didMoveToWindow
/// #endif                       → ;@end
/// {
/// ```
///
/// Every other line is empty, so symbol lines match the original. `None`
/// when the file has no such group.
fn alternate_method_heads(source: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let directive = |l: &str| {
        l.trim_start().strip_prefix('#').map(|d| {
            d.trim_start()
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or("")
                .to_owned()
        })
    };
    let is_head = |l: &str| {
        let t = l.trim_start();
        (t.starts_with('-') || t.starts_with('+')) && t[1..].trim_start().starts_with('(')
    };
    let mut out: Vec<String> = vec![String::new(); lines.len()];
    let mut found = false;
    let mut k = 0;
    while k < lines.len() {
        let is_alt = matches!(directive(lines[k]).as_deref(), Some("else" | "elif"));
        if !(is_alt && lines.get(k + 1).is_some_and(|l| is_head(l))) {
            k += 1;
            continue;
        }
        // Head lines up to the closing directive (multi-line selectors allowed).
        let mut j = k + 1;
        while j < lines.len() && directive(lines[j]).is_none() && !lines[j].contains(['{', ';']) {
            j += 1;
        }
        let closes = j < lines.len()
            && matches!(
                directive(lines[j]).as_deref(),
                Some("endif" | "else" | "elif")
            );
        let body_follows = closes && {
            // Through `#endif`, the next code line opens the shared body.
            let mut e = j;
            while e < lines.len() && directive(lines[e]).as_deref() != Some("endif") {
                e += 1;
            }
            lines[e + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.trim_start().starts_with('{'))
        };
        if closes && body_follows {
            out[k] = "@interface _".to_owned();
            for row in k + 1..j {
                out[row] = lines[row].to_owned();
            }
            out[j] = ";@end".to_owned();
            found = true;
        }
        k = j.max(k + 1);
    }
    found.then(|| out.join("\n"))
}

/// Signatures are cut from the parsed (normalised / masked) buffer; rebuild
/// them from the same byte range of the original text so users see
/// `std::vector<T>` rather than the mask (`std__vector __ __`). The rewrites
/// are length-preserving, so the column found in the rewritten line is valid
/// in the original line.
fn restore_signatures(symbols: &mut [crate::types::CodeSymbol], rewritten: &str, original: &str) {
    if rewritten == original {
        return;
    }
    let rewritten_lines: Vec<&str> = rewritten.lines().collect();
    let original_lines: Vec<&str> = original.lines().collect();
    for symbol in symbols {
        let row = (symbol.start_line as usize).wrapping_sub(1);
        let (Some(masked), Some(orig)) = (rewritten_lines.get(row), original_lines.get(row)) else {
            continue;
        };
        if masked == orig {
            continue;
        }
        let probe = symbol.signature.trim_end_matches('…');
        if probe.is_empty() {
            continue;
        }
        let Some(col) = masked.find(probe) else {
            continue;
        };
        let Some(tail) = orig.get(col..) else {
            continue;
        };
        symbol.signature = super::signature_from_row(tail, 0);
    }
}

/// Length-preserving C++ masking for the ObjC pass of an ObjC++ file. The
/// C++ pass extracts everything masked here.
///
/// - top-level `namespace … { … }`, `template …` declarations, `using …;`
///   and C++ `class` definitions are blanked;
/// - `a::b::c` becomes `a__b__c` and template arguments after a qualified
///   name (`std::shared_ptr<T>`) are blanked, so C++ types in method
///   signatures and ivars read as plain identifiers.
fn mask_cpp(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut depth = 0i32;
    let mut i = 0;
    let mut stmt_start = true;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'#' if stmt_start => {
                while i < bytes.len() && !(bytes[i] == b'\n' && bytes[i - 1] != b'\\') {
                    i += 1;
                }
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        if b == b'\n' && depth == 0 {
            // A statement starts after an ObjC container line (`@end`,
            // `@interface X : Y`), which ends without `;` / `}`.
            let line_start = source[..i].rfind('\n').map_or(0, |p| p + 1);
            if source[line_start..i].trim_start().starts_with('@') {
                stmt_start = true;
            }
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if depth == 0 && stmt_start && is_ident(b) {
            let word_end = i + bytes[i..].iter().take_while(|&&c| is_ident(c)).count();
            let word = &source[i..word_end];
            // A struct *definition* (`struct Key {…}`, `struct A : B {…}`)
            // may hold C++ members (`operator()`, methods, nested types);
            // the C++ pass extracts it, plain C structs included.
            let struct_def = word == "struct" && {
                let name = skip_ws(bytes, word_end);
                let name_end = name + bytes[name..].iter().take_while(|&&c| is_ident(c)).count();
                name_end > name && matches!(bytes.get(skip_ws(bytes, name_end)), Some(b'{' | b':'))
            };
            if struct_def || matches!(word, "namespace" | "template" | "using" | "class") {
                if let Some(end) = cpp_decl_end(bytes, word_end, word == "using") {
                    blank(&mut out, i, end);
                    i = end;
                    continue;
                }
            }
        }
        // C++ references read as pointers: `(const T &)x`, `const auto &x`,
        // `const T &name` (a spaced `a & b` bit-and is left alone).
        if b == b'&' && bytes.get(i + 1) != Some(&b'&') && i > 0 && bytes[i - 1] != b'&' {
            let next = bytes.get(i + 1).copied();
            let prev_tok = source[..i].trim_end();
            let spaced_before = bytes[i - 1] == b' ';
            let reference = next == Some(b')')
                || (spaced_before
                    && next.is_some_and(|c| is_ident(c) || c == b')')
                    && prev_tok
                        .bytes()
                        .last()
                        .is_some_and(|c| is_ident(c) || c == b'>'));
            if reference
                && !prev_tok.ends_with("return")
                && !prev_tok.ends_with(['=', '(', ',', '!'])
            {
                out[i] = b'*';
            }
        }
        let word_end = i + bytes[i..].iter().take_while(|&&c| is_ident(c)).count();
        // Skip words an earlier rewrite (range-for head) already replaced.
        if is_ident(b)
            && (i == 0 || !is_ident(bytes[i - 1]))
            && out[i..word_end] == bytes[i..word_end]
        {
            match &source[i..word_end] {
                // `auto x = …` → `id   x = …` (an ObjC object type reads fine).
                "auto" => {
                    out[i] = b'i';
                    out[i + 1] = b'd';
                    out[i + 2] = b' ';
                    out[i + 3] = b' ';
                }
                // `new T(…)` → `    T(…)`; `delete p;` → `       p;`.
                "new" | "delete"
                    if bytes.get(word_end) == Some(&b' ')
                        && source[..i]
                            .trim_end()
                            .ends_with(['=', '(', ',', ';', '{', '}', 'n']) =>
                {
                    blank(&mut out, i, word_end);
                }
                word => {
                    // Brace init `RawValue{…}` → `RawValue(…)`.
                    let keyword = matches!(
                        word,
                        "else"
                            | "do"
                            | "try"
                            | "struct"
                            | "union"
                            | "enum"
                            | "class"
                            | "namespace"
                            | "extern"
                    );
                    let line_start = source[..i].rfind('\n').map_or(0, |p| p + 1);
                    let objc_head = source[line_start..i].trim_start().starts_with('@');
                    if !keyword && !objc_head && bytes.get(word_end) == Some(&b'{') {
                        if let Some(close) = matching_brace(bytes, word_end) {
                            out[word_end] = b'(';
                            out[close] = b')';
                        }
                    }
                }
            }
        }
        // Lambda `[&](T x) {…}` / `[]() {…}` → block `^  (T x) {…}`.
        if b == b'[' && out[i] == b'[' {
            let prev = source[..i].trim_end();
            let expr_start =
                prev.ends_with(['=', '(', ',', '{', ';', ':']) || prev.ends_with("return");
            if expr_start {
                if let Some(rel) = bytes[i + 1..].iter().position(|&c| c == b']') {
                    let close = i + 1 + rel;
                    let capture = &source[i + 1..close];
                    let capture_like = capture
                        .bytes()
                        .all(|c| is_ident(c) || matches!(c, b'&' | b'=' | b',' | b' ' | b'*'))
                        && !capture.contains("  ");
                    let next = skip_ws(bytes, close + 1);
                    if capture_like && matches!(bytes.get(next), Some(b'(' | b'{')) {
                        blank(&mut out, i, close + 1);
                        out[i] = b'^';
                    }
                }
            }
        }
        // C++ range-for `for (auto x : xs)` → `for (;;          )`.
        if b == b'f'
            && source[i..].starts_with("for")
            && (i == 0 || !is_ident(bytes[i - 1]))
            && !bytes.get(i + 3).copied().is_some_and(is_ident)
        {
            let open = skip_ws(bytes, i + 3);
            if bytes.get(open) == Some(&b'(') {
                if let Some(close) = matching_paren(bytes, open) {
                    let head = &source[open + 1..close];
                    let single_colon = head.replace("::", "").matches(':').count() == 1;
                    if single_colon && !head.contains(';') && !head.contains('?') {
                        blank(&mut out, open + 1, close);
                        out[open + 1] = b';';
                        out[open + 2] = b';';
                    }
                }
            }
        }
        // Template arguments the ObjC grammar reads as comparisons:
        // `Element<T>()`, `make_shared<T>(…)`, `Pool<17> pool_;`.
        if b == b'<' && i > 0 && is_ident(bytes[i - 1]) && out[i] == b'<' {
            if let Some(close) = matching_angle(bytes, i) {
                let inner = source[i + 1..close].trim();
                let numeric = !inner.is_empty() && inner.bytes().all(|c| c.is_ascii_digit());
                if numeric || matches!(bytes.get(close + 1), Some(b'(' | b'{')) {
                    blank(&mut out, i, close + 1);
                }
            }
        }
        // Brace-init argument `f({a, b})` / `.children({x, y})` → `f((a, b))`
        // (a comma expression); GNU `({ stmt; })` keeps its braces.
        if b == b'{' && depth > 1 {
            let prev = source[..i].trim_end();
            if prev.ends_with(['(', ',']) {
                if let Some(close) = matching_block(bytes, i) {
                    if !has_top_level_semicolon(&bytes[i + 1..close]) {
                        out[i] = b'(';
                        out[close] = b')';
                    }
                }
            }
        }
        stmt_start = matches!(b, b';' | b'{' | b'}');
        if b == b':' && bytes.get(i + 1) == Some(&b':') {
            out[i] = b'_';
            out[i + 1] = b'_';
            // Template arguments of the qualified name.
            let mut j = i + 2;
            while j < bytes.len() && is_ident(bytes[j]) {
                j += 1;
            }
            if bytes.get(j) == Some(&b'<') {
                if let Some(close) = matching_angle(bytes, j) {
                    blank(&mut out, j, close + 1);
                }
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_owned())
}

/// Length-preserving ObjC masking for the C++ pass of an ObjC++ file:
/// `@interface` / `@implementation` / `@protocol` blocks through `@end`, and
/// `@class` / `@protocol` forward declarations and `@import`, are blanked.
/// The ObjC pass extracts all of them.
fn mask_objc(source: &str) -> String {
    let mut out = source.as_bytes().to_vec();
    let mut offset = 0;
    let mut block_start: Option<usize> = None;
    for line in source.split_inclusive('\n') {
        let t = line.trim_start();
        let word = t
            .split(|c: char| !(c.is_alphanumeric() || c == '@'))
            .next()
            .unwrap_or("");
        match (block_start, word) {
            (None, "@interface" | "@implementation") => block_start = Some(offset),
            (None, "@protocol") if !t.trim_end().ends_with(';') => block_start = Some(offset),
            (None, "@class" | "@protocol" | "@import") => {
                blank(&mut out, offset, offset + line.len())
            }
            (Some(start), "@end") => {
                blank(&mut out, start, offset + line.len());
                block_start = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    mask_objc_exprs(&mut out);
    String::from_utf8(out).unwrap_or_else(|_| source.to_owned())
}

/// ObjC expressions left in C++ code of an ObjC++ file (C++ class bodies,
/// free functions, lambdas) become a length-preserving `0`: message sends
/// `[obj sel:x]`, `@selector(…)` / `@{…}` / `@[…]` / `@(…)` literals and
/// blocks `^(T x) {…}`. `@"…"` strings drop the `@`, `@try` / `@catch (…)` /
/// `@autoreleasepool` heads are blanked, and ARC qualifiers (`__bridge`,
/// `__weak`, …) are blanked. Without this tree-sitter-cpp drops the whole
/// enclosing class or function into an ERROR node.
fn mask_objc_exprs(out: &mut [u8]) {
    let src = out.to_vec();
    let bytes = &src[..];
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let prev_sig = |i: usize| {
        bytes[..i]
            .iter()
            .rev()
            .find(|b| !b.is_ascii_whitespace())
            .copied()
    };
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'#' if bytes[..i]
                .iter()
                .rev()
                .take_while(|&&c| c != b'\n')
                .all(u8::is_ascii_whitespace) =>
            {
                while i < bytes.len() && !(bytes[i] == b'\n' && bytes[i - 1] != b'\\') {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        if b == b'@' {
            let next = bytes.get(i + 1).copied();
            match next {
                Some(b'"') => {
                    out[i] = b' ';
                    i += 1;
                    continue;
                }
                Some(b'{' | b'[' | b'(') => {
                    if let Some(close) = matching_any(bytes, i + 1) {
                        blank(out, i, close + 1);
                        out[i] = b'0';
                        i = close + 1;
                        continue;
                    }
                }
                Some(c) if is_ident(c) => {
                    let end = i + 1 + bytes[i + 1..].iter().take_while(|&&c| is_ident(c)).count();
                    let word = &bytes[i + 1..end];
                    let open = skip_ws(bytes, end);
                    let paren = (bytes.get(open) == Some(&b'('))
                        .then(|| matching_paren(bytes, open))
                        .flatten();
                    match word {
                        b"selector" | b"encode" | b"protocol" if paren.is_some() => {
                            let close = paren.unwrap_or(end);
                            blank(out, i, close + 1);
                            out[i] = b'0';
                            i = close + 1;
                            continue;
                        }
                        b"catch" | b"synchronized" if paren.is_some() => {
                            blank(out, i, paren.unwrap_or(end) + 1);
                            i = paren.unwrap_or(end) + 1;
                            continue;
                        }
                        b"try" | b"finally" | b"autoreleasepool" => blank(out, i, end),
                        b"YES" | b"NO" | b"true" | b"false" => out[i] = b' ',
                        _ if word.first().is_some_and(u8::is_ascii_digit) => out[i] = b' ',
                        _ => {}
                    }
                    i = end;
                    continue;
                }
                _ => {}
            }
        }
        if b == b'_' && bytes.get(i + 1) == Some(&b'_') && (i == 0 || !is_ident(bytes[i - 1])) {
            let end = i + bytes[i..].iter().take_while(|&&c| is_ident(c)).count();
            if matches!(
                &bytes[i..end],
                b"__bridge"
                    | b"__bridge_transfer"
                    | b"__bridge_retained"
                    | b"__weak"
                    | b"__strong"
                    | b"__block"
                    | b"__unsafe_unretained"
                    | b"__autoreleasing"
                    | b"__kindof"
            ) {
                blank(out, i, end);
            }
            i = end;
            continue;
        }
        // Block literal `^{…}` / `^(T x) {…}` / `^T(T x) {…}` in expression
        // position (not `a ^ b`).
        if b == b'^' && !prev_sig(i).is_some_and(|c| is_ident(c) || c == b')' || c == b']') {
            let mut j = i + 1;
            while j < bytes.len() && !matches!(bytes[j], b'{' | b';' | b'}') {
                j += 1;
            }
            if bytes.get(j) == Some(&b'{') {
                if let Some(close) = matching_block(bytes, j) {
                    blank(out, i, close + 1);
                    out[i] = b'0';
                    i = close + 1;
                    continue;
                }
            }
        }
        // Fast enumeration `for (T x in xs)` → C++ range-for `for (T x :  xs)`.
        if b == b'f'
            && bytes[i..].starts_with(b"for")
            && (i == 0 || !is_ident(bytes[i - 1]))
            && !bytes.get(i + 3).copied().is_some_and(is_ident)
        {
            let open = skip_ws(bytes, i + 3);
            if bytes.get(open) == Some(&b'(') {
                if let Some(close) = matching_paren(bytes, open) {
                    let head = &bytes[open + 1..close];
                    if let Some(k) = head.windows(4).position(|w| w == b" in ") {
                        if !head.contains(&b';') {
                            out[open + 1 + k + 1] = b':';
                            out[open + 1 + k + 2] = b' ';
                        }
                    }
                }
            }
        }
        // Message send `[recv sel…]` (not a subscript, lambda or attribute).
        if b == b'[' && !prev_sig(i).is_some_and(|c| is_ident(c) || matches!(c, b')' | b']' | b'['))
        {
            if let Some(close) = matching_any(bytes, i) {
                let after = bytes.get(skip_ws(bytes, close + 1)).copied();
                if !matches!(after, Some(b'(' | b'{' | b'['))
                    && message_send_shaped(&bytes[i + 1..close])
                {
                    blank(out, i, close + 1);
                    out[i] = b'0';
                    i = close + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
}

/// `recv sel`, `recv sel:arg`, `[nested] sel`: a receiver followed by
/// whitespace and a selector word at nesting depth 0.
fn message_send_shaped(inner: &[u8]) -> bool {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut depth = 0i32;
    let mut k = 0;
    let mut saw_receiver = false;
    while k < inner.len() {
        let c = inner[k];
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' | b'\'' => {
                k = skip_literal(inner, k);
                saw_receiver = true;
                continue;
            }
            b';' => return false,
            _ => {}
        }
        if depth == 0 && c.is_ascii_whitespace() && saw_receiver {
            let w = skip_ws(inner, k);
            let end = w + inner[w..].iter().take_while(|&&c| is_ident(c)).count();
            if end > w && (inner[w].is_ascii_alphabetic() || inner[w] == b'_') {
                let rest = skip_ws(inner, end);
                // `a sel` / `a sel:` but not `a + b` / `a ? b : c`.
                let prev = inner[..k].iter().rev().find(|b| !b.is_ascii_whitespace());
                let operand_end =
                    prev.is_some_and(|&p| is_ident(p) || matches!(p, b')' | b']' | b'"'));
                return operand_end && (rest == inner.len() || inner[rest] == b':');
            }
            return false;
        }
        if !c.is_ascii_whitespace() {
            saw_receiver = true;
        }
        k += 1;
    }
    false
}

/// Whether `inner` holds a `;` outside nested brackets and literals.
fn has_top_level_semicolon(inner: &[u8]) -> bool {
    let mut depth = 0i32;
    let mut i = 0;
    while i < inner.len() {
        match inner[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' | b'\'' => {
                i = skip_literal(inner, i);
                continue;
            }
            b';' if depth == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Matching close of `bytes[open]` (`(`, `[` or `{`), skipping literals.
fn matching_any(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `}` matching a block body `{` (statements allowed), skipping literals and
/// comments.
fn matching_block(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// End (exclusive) of a top-level C++ declaration starting after its
/// keyword: through the matching `}` (plus a trailing `;`), or the first `;`
/// at depth 0 when no block opens first.
fn cpp_decl_end(bytes: &[u8], from: usize, semicolon_only: bool) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'@' if depth == 0 => return None,
            b'{' if !semicolon_only => depth += 1,
            b'}' if !semicolon_only => {
                depth -= 1;
                if depth == 0 {
                    let after = skip_ws(bytes, i + 1);
                    return Some(if bytes.get(after) == Some(&b';') {
                        after + 1
                    } else {
                        i + 1
                    });
                }
            }
            b';' if depth == 0 => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// `}` matching `bytes[open] == b'{'` when the braces hold an expression
/// list (no `;`), i.e. a brace initialiser rather than a block.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b';' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// `<…>` matching `bytes[open] == b'<'` on one line, type-shaped content only.
fn matching_angle(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (k, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            b'\n' | b';' | b'{' | b'}' | b'=' | b'"' => return None,
            _ => {}
        }
    }
    None
}

/// Identifier, selector (`a:b:`) or category (`A(B)`) shaped.
fn is_symbol_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(':')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b':' | b'(' | b')'))
}

/// 1-based line numbers of lines that are ObjC-only syntax (`@...`, method
/// heads), where a C++ parse result is noise.
fn objc_construct_rows(source: &str) -> HashSet<u32> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let t = line.trim_start();
            t.starts_with('@')
                || ((t.starts_with('-') || t.starts_with('+'))
                    && t[1..].trim_start().starts_with('('))
        })
        .map(|(i, _)| i as u32 + 1)
        .collect()
}

/// Whether a `.h` header is Objective-C (for `Language::from_path_and_source`).
#[allow(dead_code)] // wired by the lead in `types.rs`.
pub fn header_looks_objc(source: &str) -> bool {
    source.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("@interface")
            || t.starts_with("@protocol")
            || t.starts_with("@end")
            || t.starts_with("@class ")
            || t.starts_with("#import")
            || t.starts_with("NS_ASSUME_NONNULL_BEGIN")
    })
}

/// Language of a `.h` that [`header_looks_objc`]: ObjC++ when it also uses
/// C++ (`namespace`, `template<`, `std::`, `a::b`), else ObjC.
#[allow(dead_code)] // wired by the lead in `types.rs`.
pub fn header_language(source: &str) -> Option<Language> {
    if !header_looks_objc(source) {
        return None;
    }
    let cpp = source.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') || t.starts_with("/*") {
            return false;
        }
        t.starts_with("namespace ")
            || t.starts_with("template <")
            || t.starts_with("template<")
            || t.starts_with("using ")
            || t.contains("std::")
            || (t.contains("::") && !t.contains("\""))
    });
    Some(if cpp {
        Language::ObjCpp
    } else {
        Language::ObjC
    })
}

// -------------------------------------------------------------- preprocess

const BLANK_WORDS: &[&str] = &[
    "NS_ASSUME_NONNULL_BEGIN",
    "NS_ASSUME_NONNULL_END",
    "NS_STRING_ENUM",
    "NS_EXTENSIBLE_STRING_ENUM",
    "NS_TYPED_ENUM",
    "NS_TYPED_EXTENSIBLE_ENUM",
    "CF_EXTERN_C_BEGIN",
    "CF_EXTERN_C_END",
    "CF_ASSUME_NONNULL_BEGIN",
    "CF_ASSUME_NONNULL_END",
];

const ENUM_MACROS: &[&str] = &[
    "NS_ENUM",
    "NS_OPTIONS",
    "NS_CLOSED_ENUM",
    "NS_ERROR_ENUM",
    "CF_ENUM",
    "CF_OPTIONS",
    "CF_CLOSED_ENUM",
];

/// Method-defining macros: `(macro, leading args before the selector)`.
const METHOD_MACROS: &[(&str, usize)] = &[
    ("RCT_EXPORT_METHOD", 0),
    ("RCT_REMAP_METHOD", 1),
    ("RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD", 0),
    ("RCT_EXPORT_SYNCHRONOUS_TYPED_METHOD", 1),
    ("RCT_EXPORT_CPP_METHOD", 0),
];

/// Length-preserving Cocoa macro normalisation. `None` when nothing changed.
pub(super) fn preprocess(source: &str) -> Option<String> {
    let mut out: Vec<u8> = source.as_bytes().to_vec();
    let mut changed = false;
    let bytes = source.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        // Skip comments and string / char literals untouched.
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            // Preprocessor directives (`#define NS_ENUM(...)`) stay verbatim.
            b'#' if source[..i]
                .rsplit('\n')
                .next()
                .is_some_and(|l| l.trim().is_empty()) =>
            {
                while i < bytes.len() && !(bytes[i] == b'\n' && bytes[i - 1] != b'\\') {
                    i += 1;
                }
                continue;
            }
            b'_' if source[i..].starts_with("__attribute__")
                && (i == 0 || !is_ident(bytes[i - 1])) =>
            {
                // `__attribute__((deprecated("…")))` right before `@interface`.
                let open = skip_ws(bytes, i + "__attribute__".len());
                if let Some(close) = (bytes.get(open) == Some(&b'('))
                    .then(|| matching_paren(bytes, open))
                    .flatten()
                {
                    let next = bytes.get(skip_attributes(bytes, close + 1));
                    let prev = source[..i].trim_end().bytes().last();
                    let trailing = matches!(next, Some(b';' | b'{'))
                        && prev.is_some_and(|b| is_ident(b) || b == b')' || b == b'*');
                    if next == Some(&b'@') || trailing {
                        blank(&mut out, i, close + 1);
                        changed = true;
                    }
                    i = close + 1;
                    continue;
                }
                i += 1;
                continue;
            }
            b'_' if source[i..].starts_with("__deprecated")
                || source[i..].starts_with("__unavailable") =>
            {
                let end = i + bytes[i..].iter().take_while(|&&c| is_ident(c)).count();
                let open = skip_ws(bytes, end);
                let stop = if bytes.get(open) == Some(&b'(') {
                    matching_paren(bytes, open).map_or(end, |c| c + 1)
                } else {
                    end
                };
                if matches!(bytes.get(skip_attributes(bytes, stop)), Some(b';' | b'{')) {
                    blank(&mut out, i, stop);
                    changed = true;
                }
                i = stop;
                continue;
            }
            _ => {}
        }
        if !(bytes[i].is_ascii_uppercase() && (i == 0 || !is_ident(bytes[i - 1]))) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < bytes.len() && is_ident(bytes[end]) {
            end += 1;
        }
        let word = &source[start..end];
        if !word.contains('_') || word.bytes().any(|b| b.is_ascii_lowercase()) {
            i = end;
            continue;
        }
        if BLANK_WORDS.contains(&word) {
            blank(&mut out, start, end);
            changed = true;
            i = end;
            continue;
        }
        let open = skip_ws(bytes, end);
        let close = (bytes.get(open) == Some(&b'('))
            .then(|| matching_paren(bytes, open))
            .flatten();
        if word == "NS_HEADER_AUDIT_BEGIN" || word == "NS_HEADER_AUDIT_END" {
            if let Some(close) = close {
                blank(&mut out, start, close + 1);
                changed = true;
                i = close + 1;
                continue;
            }
        }
        let Some(close) = close else {
            if bare_macro_word(source, start, end) {
                blank(&mut out, start, end);
                changed = true;
            }
            i = end;
            continue;
        };
        if ENUM_MACROS.contains(&word) {
            if rewrite_enum(source, &mut out, start, word, open, close) {
                changed = true;
            }
            i = close + 1;
            continue;
        }
        if let Some(&(_, skip)) = METHOD_MACROS.iter().find(|(m, _)| *m == word) {
            if rewrite_method_macro(bytes, &mut out, start, open, close, skip) {
                changed = true;
            }
            i = open + 1;
            continue;
        }
        let after = skip_ws(bytes, close + 1);
        let next = bytes.get(skip_attributes(bytes, close + 1)).copied();
        let line_start = source[..start].rfind('\n').map_or(0, |p| p + 1);
        let at_line_start = source[line_start..start].trim().is_empty();
        let prev = source[..start].trim_end();
        let prev_char = prev.bytes().last();
        let prev_word = prev
            .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("");
        let after_declarator = matches!(prev_char, Some(b')' | b']' | b'*'))
            || (prev_char.is_some_and(is_ident)
                && !matches!(
                    prev_word,
                    "return" | "else" | "case" | "do" | "goto" | "sizeof" | "typeof"
                ));
        if matches!(next, Some(b';' | b'{')) && after_declarator && !at_line_start {
            // Trailing attribute macro: `- (void)x API_AVAILABLE(ios(13));`
            blank(&mut out, start, close + 1);
            changed = true;
        } else if at_line_start && (next.is_none() || source[close + 1..after].contains('\n')) {
            // Statement-less declaration macro on its own line:
            // `RCT_EXPORT_MODULE()`, `RCT_EXPORT_VIEW_PROPERTY(a, T)`.
            blank(&mut out, start, close + 1);
            changed = true;
        } else if at_line_start
            && next == Some(b';')
            && word.starts_with("RCT_")
            && brace_depth(&bytes[..start]) == 0
        {
            // File-scope declaration macro ending in `;` (often inside an
            // `@implementation`): `RCT_ENUM_CONVERTER(T, (@{…}), d, v);`,
            // `RCT_MULTI_ENUM_CONVERTER(…);`. They expand to methods the
            // grammar cannot see; blanking keeps the rest of the block.
            let semi = skip_attributes(bytes, close + 1);
            blank(&mut out, start, semi + 1);
            changed = true;
        } else if at_line_start && next == Some(b'{') && word.starts_with("RCT_") {
            // `RCT_CUSTOM_VIEW_PROPERTY(name, T, V) { ... }`: method body
            // without a selector; keep the body, drop the head.
            blank(&mut out, start, close + 1);
            changed = true;
        }
        i = close + 1;
    }
    if blank_unbalanced_branches(source, &mut out) {
        changed = true;
    }
    changed.then(|| String::from_utf8(out).unwrap_or_else(|_| source.to_owned()))
}

/// A parenthesis-less attribute macro (`NS_DESIGNATED_INITIALIZER`,
/// `NS_REFINED_FOR_SWIFT`, `NS_SWIFT_NONISOLATED`): trailing a declarator
/// right before `;` / `{`, or alone on its line.
fn bare_macro_word(source: &str, start: usize, end: usize) -> bool {
    let line_start = source[..start].rfind('\n').map_or(0, |p| p + 1);
    let line_end = source[end..].find('\n').map_or(source.len(), |p| end + p);
    if source[line_start..start].trim().is_empty() && source[end..line_end].trim().is_empty() {
        return true;
    }
    let next = source
        .as_bytes()
        .get(skip_attributes(source.as_bytes(), end))
        .copied();
    let prev = source[..start].trim_end();
    let prev_word = prev
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    matches!(next, Some(b';' | b'{'))
        && (prev.ends_with([')', '*'])
            || (prev
                .bytes()
                .last()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !matches!(prev_word, "return" | "case" | "else" | "goto" | "sizeof")))
        && !source[line_start..start]
            .trim_start()
            .starts_with("typedef")
}

/// Position of the first token after `from` that is not whitespace or a
/// declaration attribute (`NS_SWIFT_NAME(x)`, `NS_DESIGNATED_INITIALIZER`,
/// `__deprecated_msg("…")`, `__attribute__((…))`), so chained trailing
/// attributes all see the `;` / `{` that ends the declaration.
fn skip_attributes(bytes: &[u8], from: usize) -> usize {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = skip_ws(bytes, from);
    loop {
        let end = i + bytes[i..].iter().take_while(|&&c| is_ident(c)).count();
        let word = &bytes[i..end];
        let macro_like = (word.first().is_some_and(u8::is_ascii_uppercase)
            && word.contains(&b'_')
            && !word.iter().any(u8::is_ascii_lowercase))
            || word.starts_with(b"__");
        if end == i || !macro_like {
            return i;
        }
        let open = skip_ws(bytes, end);
        i = if bytes.get(open) == Some(&b'(') {
            match matching_paren(bytes, open) {
                Some(close) => skip_ws(bytes, close + 1),
                None => return i,
            }
        } else {
            skip_ws(bytes, end)
        };
    }
}

fn skip_literal(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() && bytes[i] != quote && bytes[i] != b'\n' {
        if bytes[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    i + 1
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Index of the `)` matching `bytes[open] == b'('`, skipping literals.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            // Macro arguments may hold `@{…}` literals but never a `;`.
            b';' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// `text` without `//` line comments and `#pragma` lines (block comments
/// are kept; they never hold the `#` / brace tokens the caller checks).
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("#pragma"))
        .map(|l| l.find("//").map_or(l, |p| &l[..p]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `{` minus `}` outside comments and literals: brace depth at the end of
/// `bytes`. `@implementation` / `@interface` do not open a brace scope, so 0
/// means file scope (or directly inside an ObjC container).
fn brace_depth(bytes: &[u8]) -> i32 {
    let mut depth = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth
}

/// Overwrite `out[start..end]` with spaces, keeping newlines (line numbers).
fn blank(out: &mut [u8], start: usize, end: usize) {
    let end = end.min(out.len());
    for b in &mut out[start..end] {
        if *b != b'\n' && *b != b'\r' {
            *b = b' ';
        }
    }
}

/// Write `text` at `start` and pad with spaces up to `end` (exclusive).
fn overwrite(out: &mut [u8], start: usize, end: usize, text: &str) -> bool {
    if text.len() > end - start {
        return false;
    }
    blank(out, start, end);
    out[start..start + text.len()].copy_from_slice(text.as_bytes());
    true
}

/// `typedef NS_ENUM(NSInteger, Name)` → `enum Name : NSInteger` (the
/// `typedef` is dropped: there is no trailing declarator to bind).
fn rewrite_enum(
    source: &str,
    out: &mut [u8],
    start: usize,
    word: &str,
    open: usize,
    close: usize,
) -> bool {
    let args: Vec<&str> = source[open + 1..close].split(',').map(str::trim).collect();
    let [underlying, name] = args[..] else {
        return false;
    };
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok(name) {
        return false;
    }
    let before = source[..start].trim_end();
    let from = before
        .strip_suffix("typedef")
        .filter(|rest| {
            rest.is_empty() || rest.ends_with(|c: char| !c.is_alphanumeric() && c != '_')
        })
        .map_or(start, str::len);
    let text = if word == "NS_ERROR_ENUM" || !ok(underlying) {
        format!("enum {name}")
    } else {
        format!("enum {name} : {underlying}")
    };
    overwrite(out, from, close + 1, &text)
}

/// `RCT_EXPORT_METHOD(sel:(T)x)` → `- (void)sel:(T)x `;
/// `RCT_REMAP_METHOD(js, sel:(T)x)` → `- (void)    sel:(T)x `;
/// `RCT_EXPORT_SYNCHRONOUS_TYPED_METHOD(T *, sel)` → `- (T *)sel `.
fn rewrite_method_macro(
    bytes: &[u8],
    out: &mut [u8],
    start: usize,
    open: usize,
    close: usize,
    skip: usize,
) -> bool {
    // Commas at paren depth 1 split leading args from the selector.
    let mut commas = Vec::new();
    let mut depth = 0;
    for (k, &b) in bytes.iter().enumerate().take(close).skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 1 => commas.push(k),
            _ => {}
        }
    }
    let typed = bytes[start..open].starts_with(b"RCT_EXPORT_SYNCHRONOUS_TYPED_METHOD");
    if commas.len() < skip {
        return false;
    }
    if typed {
        // `MACRO(` → `-  (` …  `,` → `)`.
        let comma = commas[0];
        blank(out, start, open + 1);
        out[start] = b'-';
        out[open] = b'(';
        out[comma] = b')';
        blank(out, close, close + 1);
        return true;
    }
    let sel_start = if skip == 0 {
        open + 1
    } else {
        commas[skip - 1] + 1
    };
    if !overwrite(out, start, sel_start, "- (void)") {
        return false;
    }
    blank(out, close, close + 1);
    true
}

/// `#if A … { #else … { #endif` splits braces across branches, which wrecks
/// the tree. When every branch of a conditional changes the brace depth by
/// the same non-zero amount, keep the first branch and blank the others.
fn blank_unbalanced_branches(source: &str, out: &mut [u8]) -> bool {
    struct Group {
        branches: Vec<(usize, usize, i32)>, // (content start, content end, brace delta)
        cur_start: usize,
        delta: i32,
        /// Byte offset of the `#if` line.
        if_line: usize,
        /// The `#if` sits directly in an `@interface` / `@implementation` /
        /// `@protocol` body.
        in_container: bool,
    }
    let mut stack: Vec<Group> = Vec::new();
    let mut in_container = false;
    let mut changed = false;
    let mut offset = 0;
    // Open `<` of an `@interface … <` / `@protocol … <` protocol list.
    let mut in_protocol_list = false;
    // Blanked `#ifdef __cplusplus extern "C" {` guards awaiting their `}`.
    let mut open_extern_guards = 0usize;
    for line in source.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("@end") {
            in_container = false;
        } else if (t.starts_with("@interface")
            || t.starts_with("@implementation")
            || t.starts_with("@protocol"))
            && !t.trim_end().ends_with(';')
        {
            in_container = true;
        }
        if in_protocol_list {
            if t.starts_with('#') {
                // `#if !TARGET_OS_TV , UITextDropDelegate #endif`: keep the
                // protocols, drop the directive lines.
                blank(out, offset, offset + line.len());
                changed = true;
                offset += line.len();
                continue;
            }
            if t.contains('>') {
                in_protocol_list = false;
            }
        } else if (t.starts_with("@interface") || t.starts_with("@protocol"))
            && t.rfind('<').is_some_and(|lt| !t[lt..].contains('>'))
        {
            in_protocol_list = true;
        }
        let directive = t.strip_prefix('#').map(|d| {
            d.trim_start()
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or("")
        });
        match directive {
            Some("if" | "ifdef" | "ifndef") => stack.push(Group {
                branches: Vec::new(),
                cur_start: offset + line.len(),
                delta: 0,
                if_line: offset,
                in_container,
            }),
            Some("elif" | "else" | "elifdef" | "elifndef") => {
                if let Some(g) = stack.last_mut() {
                    g.branches.push((g.cur_start, offset, g.delta));
                    g.cur_start = offset + line.len();
                    g.delta = 0;
                }
            }
            Some("endif") => {
                if let Some(mut g) = stack.pop() {
                    g.branches.push((g.cur_start, offset, g.delta));
                    let first = g.branches[0].2;
                    // `#if SD_MAC - (void)a #else - (void)b #endif { body }`:
                    // per-platform method heads sharing one body.
                    let heads_only = g.branches.len() > 1
                        && g.branches.iter().all(|&(s, e, _)| {
                            let text = source[s..e].trim();
                            !text.is_empty()
                                && (text.starts_with('-') || text.starts_with('+'))
                                && !text.contains(['{', ';', '}', '#'])
                        });
                    // `#ifdef __cplusplus / extern "C" { / #endif` guards.
                    let guard_text = (g.branches.len() == 1)
                        .then(|| source[g.branches[0].0..g.branches[0].1].trim());
                    let extern_guard = match guard_text {
                        Some("extern \"C\" {") => {
                            open_extern_guards += 1;
                            true
                        }
                        Some("}") if open_extern_guards > 0 => {
                            open_extern_guards -= 1;
                            true
                        }
                        _ => false,
                    };
                    // `#ifndef X - (id)k {…} #else - (id)k {…} #endif` inside an
                    // `@implementation`: tree-sitter-objc cannot hold method
                    // definitions under a conditional. When every branch is
                    // whole method definitions, drop only the directive lines.
                    let whole_methods = g.branches.len() > 1
                        && g.branches.iter().all(|&(s, e, d)| {
                            let text = source[s..e].trim();
                            d == 0
                                && (text.is_empty()
                                    || ((text.starts_with('-') || text.starts_with('+'))
                                        && text.ends_with('}')
                                        && !text.contains('#')))
                        });
                    // `#ifdef X @property … / - (T)m; / - (T)m {…} #endif`
                    // directly in an ObjC container (any number of branches):
                    // the grammar cannot hold directives there, so drop only
                    // the directive lines and keep every branch's members.
                    let decl_only = g.in_container
                        && g.branches.iter().all(|&(s, e, d)| {
                            let text = strip_line_comments(&source[s..e]);
                            let text = text.trim();
                            d == 0
                                && (text.is_empty()
                                    || ((text.starts_with('-')
                                        || text.starts_with('+')
                                        || text.starts_with("@property")
                                        || text.starts_with("@dynamic")
                                        || text.starts_with("@synthesize"))
                                        && text.ends_with([';', '}'])
                                        && !text.contains('#')))
                        });
                    if whole_methods || decl_only {
                        let line_end = offset + line.len();
                        blank(out, g.if_line, g.branches[0].0);
                        for pair in g.branches.windows(2) {
                            blank(out, pair[0].1, pair[1].0);
                        }
                        blank(out, offset, line_end);
                        changed = true;
                    } else if extern_guard {
                        let (s, e, _) = g.branches[0];
                        blank(out, s, e);
                        changed = true;
                    } else if heads_only
                        || (g.branches.len() > 1
                            && first != 0
                            && g.branches.iter().all(|b| b.2 == first))
                    {
                        for &(s, e, _) in &g.branches[1..] {
                            blank(out, s, e);
                        }
                        changed = true;
                    }
                    // Nested deltas count toward the parent's branch once.
                    if let Some(parent) = stack.last_mut() {
                        parent.delta += first;
                    }
                }
            }
            _ => {
                if let Some(g) = stack.last_mut() {
                    if directive.is_none() {
                        g.delta += brace_delta(line);
                    }
                }
            }
        }
        offset += line.len();
    }
    changed
}

/// `{` minus `}` on a line, ignoring string literals and `//` comments.
fn brace_delta(line: &str) -> i32 {
    let bytes = line.as_bytes();
    let mut delta = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => break,
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::super::ParserPool;
    use super::*;
    use crate::types::RelPath;
    use std::fs;

    fn parse(lang: Language, src: &str) -> ParsedFile {
        let pool = ParserPool::new();
        parse_with(lang, src, |l, s| pool.parse(l, &RelPath::new("t.m"), s)).unwrap()
    }

    fn names(p: &ParsedFile) -> Vec<String> {
        p.symbols.iter().map(|s| s.name.clone()).collect()
    }

    fn has(p: &ParsedFile, name: &str, kind: SymbolKind) -> bool {
        p.symbols.iter().any(|s| s.name == name && s.kind == kind)
    }

    #[test]
    fn interface_protocol_category_and_members() {
        let src = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/objc/Sample.m"
        ))
        .unwrap();
        let p = parse(Language::ObjC, &src);
        let n = names(&p);
        for (name, kind) in [
            ("AFProto", SymbolKind::Interface),
            ("AFMgr", SymbolKind::Class),
            ("AFMgr(Cat)", SymbolKind::Class),
            ("foo:bar:", SymbolKind::Method),
            ("manager", SymbolKind::Method),
            ("catMethod", SymbolKind::Method),
            ("session", SymbolKind::Field),
            ("completion", SymbolKind::Field),
            ("count", SymbolKind::Field),
            ("_ivar", SymbolKind::Field),
            ("_buffer", SymbolKind::Field),
            ("AFStatus", SymbolKind::Enum),
            ("AFStatusA", SymbolKind::Const),
            ("AFOptionB", SymbolKind::Const),
            ("AFErrorTimeout", SymbolKind::Const),
            ("AFBlock", SymbolKind::Type),
            ("kKey", SymbolKind::Const),
            ("AFExt", SymbolKind::Const),
            ("gCounter", SymbolKind::Variable),
            ("helper", SymbolKind::Function),
            ("AFPublic", SymbolKind::Function),
            ("S", SymbolKind::Struct),
            ("a", SymbolKind::Field),
            ("AF_MAX", SymbolKind::Const),
            ("AF_MIN", SymbolKind::Function),
            ("availableMethod", SymbolKind::Method),
            ("initWithX:", SymbolKind::Method),
            ("afterIf", SymbolKind::Method),
            ("exported:resolver:", SymbolKind::Method),
            ("remapped:", SymbolKind::Method),
        ] {
            assert!(has(&p, name, kind), "missing {name} {kind:?} in {n:?}");
        }
        // Forward declarations and locals are not definitions.
        for absent in ["Fwd", "Foo", "local", "self"] {
            assert!(!n.iter().any(|x| x == absent), "{absent} in {n:?}");
        }
        // Visibility: static is file-local.
        let helper = p.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(!helper.exported);
        let public = p.symbols.iter().find(|s| s.name == "AFPublic").unwrap();
        assert!(public.exported);
        // Line numbers survive the length-preserving macro rewrite.
        let status = p.symbols.iter().find(|s| s.name == "AFStatusB").unwrap();
        let line = src.lines().position(|l| l.contains("AFStatusB")).unwrap() as u32 + 1;
        assert_eq!(status.start_line, line);
        assert!(p.imports.iter().any(|i| i == "Foundation/Foundation.h"));
        assert!(p.imports.iter().any(|i| i == "AFURLSessionManager.h"));
        assert!(p.imports.iter().any(|i| i == "UIKit"));
    }

    #[test]
    fn objcpp_unions_cpp_symbols() {
        let src = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/objc/Bridge.mm"
        ))
        .unwrap();
        let p = parse(Language::ObjCpp, &src);
        let n = names(&p);
        for name in [
            "RCTBridgeThing",
            "initWithRuntime:",
            "invoke",
            "Helper",
            "compute",
            "freeFunction",
            "Mode",
            "_helper",
        ] {
            assert!(n.iter().any(|x| x == name), "missing {name} in {n:?}");
        }
        // No duplicates from the two passes.
        let mut sorted = p
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.start_line))
            .collect::<Vec<_>>();
        let len = sorted.len();
        sorted.sort();
        sorted.dedup();
        assert_eq!(len, sorted.len(), "{n:?}");
    }

    #[test]
    fn selector_call_names() {
        let src = "void f(id d) { [d setObject:o forKey:k]; [d run]; g(1); }";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_objc::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut found = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if n.kind() == "message_expression" {
                found.push(call_name(n, "?", src));
            }
            let mut c = n.walk();
            stack.extend(n.children(&mut c));
        }
        found.sort();
        assert_eq!(found, ["run", "setObject:forKey:"]);
    }

    #[test]
    fn signatures_come_from_original_text_not_the_mask() {
        let pool = super::super::ParserPool::new();
        let src = "#import <Foundation/Foundation.h>\n#include <vector>\n@interface G : NSObject\n+ (std::vector<facebook::react::Stop>)stops:(int)n;\n@end\n";
        let parsed = pool
            .parse(Language::ObjCpp, &crate::types::RelPath::new("G.h"), src)
            .expect("parses");
        let method = parsed
            .symbols
            .iter()
            .find(|s| s.name.starts_with("stops"))
            .expect("method symbol");
        assert_eq!(
            method.signature,
            "+ (std::vector<facebook::react::Stop>)stops:(int)n;"
        );
        assert!(parsed.symbols.iter().all(|s| !s.signature.contains("__")));
    }

    #[test]
    fn preprocess_is_length_preserving() {
        let src = "NS_ASSUME_NONNULL_BEGIN\ntypedef NS_ENUM(NSInteger, X) {\n XA,\n};\n- (void)a API_AVAILABLE(ios(13.0));\nRCT_EXPORT_MODULE()\nNS_ASSUME_NONNULL_END\n";
        let out = preprocess(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert_eq!(out.lines().count(), src.lines().count());
        assert!(out.contains("enum X : NSInteger"));
        assert!(!out.contains("API_AVAILABLE"));
        assert!(!out.contains("RCT_EXPORT_MODULE"));
        assert!(preprocess("int main(void) { return 0; }").is_none());
        // Strings and comments are never touched.
        assert!(
            preprocess("// NS_ENUM(NSInteger, X)\nchar *s = \"NS_ASSUME_NONNULL_BEGIN\";")
                .is_none()
        );
    }

    #[test]
    fn declarator_identifiers() {
        assert_eq!(declarator_identifier("*session"), "session");
        assert_eq!(declarator_identifier("* _Nullable session"), "session");
        assert_eq!(
            declarator_identifier("(^completion)(BOOL ok)"),
            "completion"
        );
        assert_eq!(declarator_identifier("(*fn)(int)"), "fn");
        assert_eq!(declarator_identifier("flag : 1"), "flag");
        assert_eq!(declarator_identifier("buf[16]"), "buf");
        assert_eq!(
            declarator_identifier("std::unique_ptr<facebook::react::Helper> _helper"),
            "_helper"
        );
    }

    #[test]
    fn objc_header_sniff() {
        assert!(header_looks_objc("#import <UIKit/UIKit.h>\n"));
        assert!(header_looks_objc("@interface A : NSObject\n@end\n"));
        assert!(!header_looks_objc("#include <stdio.h>\nint f(void);\n"));
        assert_eq!(
            header_language("#import <UIKit/UIKit.h>\nstd::string f();\n"),
            Some(Language::ObjCpp)
        );
        assert_eq!(
            header_language("#import <UIKit/UIKit.h>\n"),
            Some(Language::ObjC)
        );
    }

    /// Debug helper: writes the ObjC-pass text (`.objc`) and the C++-pass
    /// text (`.cpp`, ObjC++ only) of `XFORM_FILE` next to `XFORM_OUT`.
    #[test]
    fn objcpp_cpp_class_with_message_sends() {
        let src = "class H : public mmkv::Handler {\npublic:\n    void log(int l) override {\n        if ([g respondsToSelector:@selector(x:)]) { [g x:@\"a\"]; }\n    }\n    std::mutex &mutex() { return m_; }\n};\nRCTX::WebSocket::WebSocket(int a) : a_(a) {}\nvoid f() { for (id k in xs) { dispatch_async(q, ^{ [k run]; }); } }\n";
        let names = names(&parse(Language::ObjCpp, src));
        for want in ["H", "log", "mutex", "WebSocket", "f"] {
            assert!(names.iter().any(|n| n == want), "{want} missing: {names:?}");
        }
    }

    #[test]
    fn objc_conditional_members_and_alternate_heads() {
        let src = "@interface U : NSObject\n#ifdef MAC\n@property (nonatomic) NSImage *profileImage;\n#endif\n#ifdef __cplusplus\n- (mmkv::MMKV *)cppInstance;\n#endif\n@end\n@implementation V\n#if SD_MAC\n- (void)viewDidMoveToWindow\n#else\n- (void)didMoveToWindow\n#endif\n{\n}\nRCT_ENUM_CONVERTER(\n    A,\n    (@{ @\"x\" : @(1) }),\n    B,\n    integerValue);\n- (void)after\n{\n}\n@end\n";
        let names = names(&parse(Language::ObjCpp, src));
        for want in [
            "profileImage",
            "cppInstance",
            "viewDidMoveToWindow",
            "didMoveToWindow",
            "after",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} missing: {names:?}");
        }
    }

    #[test]
    #[ignore]
    fn zz_transform() {
        let path = std::env::var("XFORM_FILE").unwrap();
        let out = std::env::var("XFORM_OUT").unwrap();
        let src = fs::read_to_string(&path).unwrap();
        let mut objc = preprocess(&src).unwrap_or_else(|| src.clone());
        if path.ends_with(".mm") || header_language(&src) == Some(Language::ObjCpp) {
            objc = mask_cpp(&objc);
            fs::write(format!("{out}.cpp"), mask_objc(&src)).unwrap();
        }
        fs::write(format!("{out}.objc"), objc).unwrap();
    }

    /// Bench helper: `DUMP_DIR` + `DUMP_LIST` (repo-relative paths) →
    /// `DUMP_OUT` lines `file\tname\tkind\tline`. `.h` is treated as ObjC.
    #[test]
    #[ignore]
    fn zz_dump_objc_symbols() {
        let root = std::path::PathBuf::from(std::env::var("DUMP_DIR").unwrap());
        let list = fs::read_to_string(std::env::var("DUMP_LIST").unwrap()).unwrap();
        let raw = std::env::var("DUMP_RAW").is_ok();
        let mut cache = CachedRaw::default();
        let mut lines = String::new();
        let mut failures = 0;
        let mut errors = 0usize;
        let started = std::time::Instant::now();
        let mut bytes = 0usize;
        for rel in list.lines() {
            let Ok(src) = fs::read_to_string(root.join(rel)) else {
                continue;
            };
            bytes += src.len();
            let lang = if rel.ends_with(".mm") {
                Language::ObjCpp
            } else if rel.ends_with(".h") {
                header_language(&src).unwrap_or(Language::ObjC)
            } else {
                Language::ObjC
            };
            let result = if raw {
                cache.parse(Language::ObjC, &src)
            } else {
                parse_with(lang, &src, |l, s| cache.parse(l, s))
            };
            errors += cache.error_bytes;
            match result {
                Ok(p) => {
                    for s in p.symbols {
                        lines.push_str(&format!(
                            "{rel}\t{}\t{:?}\t{}\n",
                            s.name, s.kind, s.start_line
                        ));
                    }
                }
                Err(e) => {
                    failures += 1;
                    eprintln!("FAIL {rel}: {e:?}");
                }
            }
        }
        let secs = started.elapsed().as_secs_f64();
        eprintln!(
            "files={} bytes={bytes} failures={failures} objc_error_bytes={errors} secs={secs:.3} MB/s={:.1}",
            list.lines().count(),
            bytes as f64 / 1e6 / secs
        );
        fs::write(std::env::var("DUMP_OUT").unwrap(), lines).unwrap();
    }

    /// Bench-only raw parser with queries compiled once per language — the
    /// production pool recompiles per file (see lead diff).
    #[derive(Default)]
    struct CachedRaw {
        langs: std::collections::HashMap<
            Language,
            (
                tree_sitter::Parser,
                tree_sitter::Query,
                tree_sitter::Query,
                tree_sitter::Query,
            ),
        >,
        /// ERROR-node bytes of the last ObjC-grammar parse.
        error_bytes: usize,
    }

    impl CachedRaw {
        fn parse(&mut self, lang: Language, src: &str) -> Result<ParsedFile, ParseError> {
            use super::super::{
                extract_calls_for, extract_imports, extract_symbols, grammar, queries,
            };
            let (parser, sq, iq, cq) = self.langs.entry(lang).or_insert_with(|| {
                let g = grammar(lang).unwrap();
                let q = queries::for_language(lang).unwrap();
                let mut p = tree_sitter::Parser::new();
                p.set_language(&g).unwrap();
                (
                    p,
                    tree_sitter::Query::new(&g, q.symbols).unwrap(),
                    tree_sitter::Query::new(&g, q.imports).unwrap(),
                    tree_sitter::Query::new(&g, q.calls).unwrap(),
                )
            });
            let tree = parser.parse(src, None).ok_or(ParseError::NoTree)?;
            let root = tree.root_node();
            if lang == Language::ObjC {
                let mut err = 0;
                let mut stack = vec![root];
                while let Some(n) = stack.pop() {
                    if n.is_error() {
                        err += n.end_byte() - n.start_byte();
                        continue;
                    }
                    let mut c = n.walk();
                    stack.extend(n.children(&mut c));
                }
                self.error_bytes = err;
            }
            Ok(ParsedFile {
                symbols: extract_symbols(lang, sq, root, src),
                imports: extract_imports(lang, iq, root, src),
                calls: extract_calls_for(lang, cq, root, src),
                partial: root.has_error(),
                c_family_rewrite: false,
            })
        }
    }
}
