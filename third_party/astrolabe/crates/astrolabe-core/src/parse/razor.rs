//! C# extraction for `.razor` and `.cshtml` before tree-sitter.
//!
//! There is no `Language::Razor` and no storage tag of its own (tag 15 stays
//! C#, tag 16 is reserved for Visual Basic). Markup is blanked in place and
//! only real C# is handed to the existing C# grammar:
//!
//! * `@code { ... }` blocks (outer braces blanked so declarations sit at
//!   compilation-unit scope)
//! * `@{ ... }` statement blocks, same treatment
//! * line-leading `@using` / `@inherits` directives
//!
//! `@using` is rewritten to a C# `using` (semicolon added if Razor omitted
//! it). `@inherits Type` is not a C# declaration; it becomes
//! `using __AstrolabeRazorInherits = Type;` so the type is real C# and does
//! not invent a class symbol. Inline `@expr`, `@(...)`, tags, and text stay
//! blank, so they are not indexed as symbols.
//!
//! # Line mapping
//!
//! Newlines are preserved, so a declaration's line number in the extracted
//! buffer matches the `.razor` / `.cshtml` line in the common case. This is
//! best-effort. Limitations:
//!
//! * Columns are not mapped (`@` is dropped, missing semicolons are inserted).
//! * Brace matching skips ordinary strings, verbatim strings, raw `"""`
//!   strings, character literals, and comments, but treats interpolated
//!   strings as opaque. A `}` inside `$"..."` can close a block early.
//! * Markup transitions inside `@{ }` / `@code` (a `<tag>` in the middle of
//!   C#) are not recognized; they are copied as C# text.
//! * `@functions`, `@section`, `@helper`, and explicit expressions are left
//!   as markup.
//!
//! # LSP
//!
//! Razor LSP is not wired. `lsp/csharp.rs` plus `lsp/transport.rs` know only
//! the Roslyn launcher (`dotnet Microsoft.CodeAnalysis.LanguageServer.dll
//! --stdio`, then `solution/open` or `project/open`). A Razor server (`rzls` /
//! the cohosted Razor endpoints) does not speak that protocol, and this crate
//! has no separate description of it. Inventing `ASTROLABE_LSP_RAZOR` would
//! be a fake client. Syntactic extraction is the shipped behavior.

use crate::types::RelPath;

/// `.razor` / `.cshtml` (not `.cs`, not `.csx`).
pub(crate) fn is_razor_source(path: &str) -> bool {
    matches!(RelPath::new(path).extension(), Some("razor" | "cshtml"))
}

/// Blank markup, keep C# regions, preserve newlines.
pub(crate) fn extract_razor_csharp(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut at_line_start = true;
    while i < source.len() {
        let Some(c) = source[i..].chars().next() else {
            break;
        };
        if c == '\n' {
            out.push('\n');
            i += 1;
            at_line_start = true;
            continue;
        }
        if c == '\r' {
            out.push('\r');
            i += 1;
            continue;
        }
        if at_line_start && (c == ' ' || c == '\t') {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        if source[i..].starts_with("@*") {
            i = blank_razor_comment(source, i, &mut out);
            at_line_start = false;
            continue;
        }
        if at_line_start {
            if let Some(next) = rewrite_leading_directive(source, i, &mut out) {
                i = next;
                at_line_start = false;
                continue;
            }
        }
        if let Some(next) = try_at_code(source, i, &mut out) {
            i = next;
            at_line_start = false;
            continue;
        }
        if source[i..].starts_with("@{") {
            out.push(' ');
            i = splice_block(source, i + 1, &mut out);
            at_line_start = false;
            continue;
        }
        if source[i..].starts_with("@@") {
            out.push(' ');
            out.push(' ');
            i += 2;
            at_line_start = false;
            continue;
        }
        push_blank_char(&mut out, c);
        i += c.len_utf8();
        at_line_start = false;
    }
    out
}

fn push_blank_char(out: &mut String, c: char) {
    if c == '\n' || c == '\r' {
        out.push(c);
    } else {
        out.push(' ');
    }
}

fn blank_range(source: &str, start: usize, end: usize, out: &mut String) {
    for c in source[start..end].chars() {
        push_blank_char(out, c);
    }
}

fn keyword_boundary(source: &str, end: usize) -> bool {
    match source[end..].chars().next() {
        None => true,
        Some(c) => !(c == '_' || c.is_ascii_alphanumeric()),
    }
}

fn blank_razor_comment(source: &str, start: usize, out: &mut String) -> usize {
    let bytes = source.as_bytes();
    let mut i = start + 2;
    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'@') {
        i += 1;
    }
    let end = if i + 1 < bytes.len() {
        i + 2
    } else {
        source.len()
    };
    blank_range(source, start, end, out);
    end
}

/// `@using` / `@inherits` at the start of a line. Returns the index of the
/// newline (not consumed) or EOL.
fn rewrite_leading_directive(source: &str, i: usize, out: &mut String) -> Option<usize> {
    let rest = &source[i..];
    if rest.starts_with("@using") && keyword_boundary(source, i + "@using".len()) {
        // `@using (var x = ...) { ... }` is a Razor statement, not a directive.
        let after = source[i + "@using".len()..].trim_start_matches([' ', '\t']);
        if after.starts_with('(') {
            blank_range(source, i, i + "@using".len(), out);
            return Some(i + "@using".len());
        }
        let mut j = i + 1; // keep the word `using`
        let copy_from = j;
        let bytes = source.as_bytes();
        while j < bytes.len() && bytes[j] != b'\n' && bytes[j] != b'\r' && bytes[j] != b';' {
            j += 1;
        }
        out.push(' ');
        out.push_str(&source[copy_from..j]);
        out.push(';');
        if j < bytes.len() && bytes[j] == b';' {
            j += 1;
        }
        return Some(j);
    }
    if rest.starts_with("@inherits") && keyword_boundary(source, i + "@inherits".len()) {
        let bytes = source.as_bytes();
        let mut j = i + "@inherits".len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        let value_start = j;
        while j < bytes.len() && bytes[j] != b'\n' && bytes[j] != b'\r' {
            j += 1;
        }
        let value = source[value_start..j].trim().trim_end_matches(';').trim();
        if value.is_empty() {
            blank_range(source, i, j, out);
        } else {
            out.push_str("using __AstrolabeRazorInherits = ");
            out.push_str(value);
            out.push(';');
        }
        return Some(j);
    }
    None
}

/// `@code` / `@functions` + optional whitespace + `{ ... }`. The keyword is
/// replaced by `class __R` and the braces are kept, so members parse as real
/// class members (fields, properties, methods) with line numbers unchanged.
fn try_at_code(source: &str, i: usize, out: &mut String) -> Option<usize> {
    let kw = ["@code", "@functions"]
        .into_iter()
        .find(|k| source[i..].starts_with(k) && keyword_boundary(source, i + k.len()))?;
    let after_kw = i + kw.len();
    let brace = find_block_open(source, after_kw)?;
    out.push_str("class __R");
    // Whitespace between the keyword and `{` keeps its newlines.
    out.push_str(&source[after_kw..brace]);
    Some(copy_block_keep_braces(source, brace, out))
}

/// Copy a `{ ... }` block verbatim, including both outer braces.
fn copy_block_keep_braces(source: &str, open: usize, out: &mut String) -> usize {
    out.push('{');
    let mut i = open + 1;
    let mut depth = 1;
    while i < source.len() && depth > 0 {
        if let Some(next) = copy_opaque(source, i, out) {
            i = next;
            continue;
        }
        let c = source[i..].chars().next().unwrap();
        out.push(c);
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
        }
        i += c.len_utf8();
    }
    i
}

fn find_block_open(source: &str, mut i: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'{' => return Some(i),
            _ => return None,
        }
    }
    None
}

/// Copy a `{ ... }` block. The opening brace at `open` and its match become
/// spaces; inner braces stay so classes and namespaces still parse.
fn splice_block(source: &str, open: usize, out: &mut String) -> usize {
    out.push(' ');
    let bytes = source.as_bytes();
    let mut i = open + 1;
    let mut depth = 1;
    while i < bytes.len() && depth > 0 {
        if let Some(next) = copy_opaque(source, i, out) {
            i = next;
            continue;
        }
        let c = source[i..].chars().next().unwrap();
        if c == '{' {
            depth += 1;
            out.push('{');
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                out.push(' ');
                i += 1;
                break;
            }
            out.push('}');
        } else {
            out.push(c);
        }
        i += c.len_utf8();
    }
    i
}

/// Comments and string-like literals whose braces must not affect depth.
/// Returns the index after the literal when `i` is at its start.
fn copy_opaque(source: &str, i: usize, out: &mut String) -> Option<usize> {
    let rest = &source[i..];
    if rest.starts_with("//") {
        let bytes = source.as_bytes();
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        out.push_str(&source[i..j]);
        return Some(j);
    }
    if rest.starts_with("/*") {
        let bytes = source.as_bytes();
        let mut j = i + 2;
        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
            j += 1;
        }
        let end = if j + 1 < bytes.len() {
            j + 2
        } else {
            source.len()
        };
        out.push_str(&source[i..end]);
        return Some(end);
    }
    if rest.starts_with("\"\"\"") {
        return Some(copy_raw_string(source, i, out));
    }
    if rest.starts_with("@\"") || rest.starts_with("$@\"") || rest.starts_with("@$\"") {
        let quote_at = i + rest.find('"').unwrap();
        return Some(copy_verbatim_string(source, i, quote_at, out));
    }
    if rest.starts_with("$\"") || rest.starts_with('"') {
        let quote_at = i + rest.find('"').unwrap();
        return Some(copy_regular_string(source, i, quote_at, out));
    }
    if rest.starts_with('\'') {
        return Some(copy_char(source, i, out));
    }
    None
}

fn copy_raw_string(source: &str, i: usize, out: &mut String) -> usize {
    let bytes = source.as_bytes();
    let mut j = i + 3;
    while j + 2 < bytes.len() && !(bytes[j] == b'"' && bytes[j + 1] == b'"' && bytes[j + 2] == b'"')
    {
        j += 1;
    }
    let end = if j + 2 < bytes.len() {
        j + 3
    } else {
        source.len()
    };
    out.push_str(&source[i..end]);
    end
}

fn copy_verbatim_string(source: &str, i: usize, quote_at: usize, out: &mut String) -> usize {
    let bytes = source.as_bytes();
    let mut j = quote_at + 1;
    while j < bytes.len() {
        if bytes[j] == b'"' {
            if j + 1 < bytes.len() && bytes[j + 1] == b'"' {
                j += 2;
                continue;
            }
            j += 1;
            break;
        }
        j += 1;
    }
    out.push_str(&source[i..j]);
    j
}

fn copy_regular_string(source: &str, i: usize, quote_at: usize, out: &mut String) -> usize {
    let bytes = source.as_bytes();
    let mut j = quote_at + 1;
    while j < bytes.len() {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 2;
            continue;
        }
        if bytes[j] == b'"' || bytes[j] == b'\n' {
            if bytes[j] == b'"' {
                j += 1;
            }
            break;
        }
        j += 1;
    }
    out.push_str(&source[i..j]);
    j
}

fn copy_char(source: &str, i: usize, out: &mut String) -> usize {
    let bytes = source.as_bytes();
    let mut j = i + 1;
    if j < bytes.len() && bytes[j] == b'\\' && j + 1 < bytes.len() {
        j += 2;
    } else if j < bytes.len() {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'\'' {
        j += 1;
    }
    out.push_str(&source[i..j]);
    j
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{index_repo, IndexOptions};
    use crate::parse::ParserPool;
    use crate::resolvers::csharp::CSharpResolver;
    use crate::types::{EdgeKind, FileIndex, ImportResolution, Language, ModuleResolver, RelPath};
    use std::fs;
    use std::path::Path;

    const RAZOR: &str = r#"@page "/counter"
@using Acme.Lib
@inherits LayoutComponentBase

<h1 class="MarkupOnlyTitle">Counter</h1>

@code {
    namespace Acme.Pages {
        public class CounterWidget {
            public int Count { get; set; }
            public void Increment() {}
        }
    }
}
"#;

    const CSHTML: &str = r#"@using Acme.Pages
<div class="NotASymbol">Hi</div>
@{
    var greeting = "brace } in string";
}
@code {
    public class HomeView {
        public string Title { get; set; }
    }
}
"#;

    const VIEW_IMPORTS: &str = "\
@using Acme.Lib
@using System
@using Newtonsoft.Json
";

    fn newlines(s: &str) -> usize {
        s.matches('\n').count()
    }

    #[test]
    fn extract_preserves_newlines_and_drops_markup() {
        let extracted = extract_razor_csharp(RAZOR);
        assert_eq!(newlines(RAZOR), newlines(&extracted));
        assert!(!extracted.contains("MarkupOnlyTitle"));
        assert!(!extracted.contains("@page"));
        assert!(extracted.contains("namespace Acme.Pages"));
        assert!(extracted.contains("class CounterWidget"));
        assert!(extracted.contains("using Acme.Lib;"));
        assert!(extracted.contains("using __AstrolabeRazorInherits = LayoutComponentBase;"));
        let cshtml = extract_razor_csharp(CSHTML);
        assert_eq!(newlines(CSHTML), newlines(&cshtml));
        assert!(!cshtml.contains("NotASymbol"));
        assert!(cshtml.contains("class HomeView"));
        assert!(cshtml.contains("brace } in string"));
    }

    #[test]
    fn razor_and_cshtml_symbols_skip_markup() {
        let pool = ParserPool::new();
        let razor = pool
            .parse(
                Language::CSharp,
                &RelPath::new("Pages/Counter.razor"),
                RAZOR,
            )
            .expect("razor parse");
        let names: Vec<&str> = razor.symbols.iter().map(|s| s.name.as_str()).collect();
        for want in ["Acme.Pages", "CounterWidget", "Count", "Increment"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
        for absent in ["MarkupOnlyTitle", "Counter", "page", "h1", "code"] {
            assert!(
                !names.contains(&absent),
                "{absent} indexed from markup: {names:?}"
            );
        }
        let widget = razor
            .symbols
            .iter()
            .find(|s| s.name == "CounterWidget")
            .unwrap();
        let expect_line = RAZOR
            .lines()
            .position(|line| line.contains("class CounterWidget"))
            .unwrap() as u32
            + 1;
        assert_eq!(
            widget.start_line, expect_line,
            "line mapping should follow the .razor file"
        );
        assert!(
            razor.imports.iter().any(|i| i == "Acme.Lib"),
            "razor @using should be a C# import, got {:?}",
            razor.imports
        );

        let cshtml = pool
            .parse(
                Language::CSharp,
                &RelPath::new("Views/Index.cshtml"),
                CSHTML,
            )
            .expect("cshtml parse");
        let names: Vec<&str> = cshtml.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"HomeView"), "{names:?}");
        assert!(names.contains(&"Title"), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| *n == "NotASymbol" || *n == "Hi" || *n == "greeting"),
            "markup or locals indexed: {names:?}"
        );
        assert!(
            cshtml.imports.iter().any(|i| i == "Acme.Pages"),
            "{:?}",
            cshtml.imports
        );
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn view_imports_using_namespace_edge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "App/App.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
  </PropertyGroup>
</Project>
"#,
        );
        write(
            root,
            "App/Widget.cs",
            "namespace Acme.Lib;\npublic class Widget { public void Ping() {} }\n",
        );
        write(root, "App/Pages/Counter.razor", RAZOR);
        write(root, "App/Views/Index.cshtml", CSHTML);
        write(root, "App/Pages/_ViewImports.cshtml", VIEW_IMPORTS);

        let paths = [
            "App/App.csproj",
            "App/Widget.cs",
            "App/Pages/Counter.razor",
            "App/Views/Index.cshtml",
            "App/Pages/_ViewImports.cshtml",
        ];
        let files = FileIndex::new(root, paths.map(RelPath::new));
        let meta = CSharpResolver.detect(&files);
        let pages = meta
            .namespaces
            .iter()
            .find(|n| n.namespace == "Acme.Pages")
            .expect("namespace inside @code must be visible");
        assert!(
            pages.files.iter().any(|f| f.ends_with("Counter.razor")),
            "{:?}",
            pages.files
        );
        let from = RelPath::new("App/Pages/_ViewImports.cshtml");
        match CSharpResolver.resolve_import(&from, "using Acme.Lib;", &files, &meta) {
            ImportResolution::Namespace { name, files } => {
                assert_eq!(name, "Acme.Lib");
                assert_eq!(files, vec![RelPath::new("App/Widget.cs")]);
            }
            other => panic!("in-repo @using must fan out, got {other:?}"),
        }
        assert_eq!(
            CSharpResolver.resolve_import(&from, "using System;", &files, &meta),
            ImportResolution::Unresolved
        );
        assert_eq!(
            CSharpResolver.resolve_import(&from, "using Newtonsoft.Json;", &files, &meta),
            ImportResolution::Unresolved
        );

        let (graph, report) = index_repo(
            root,
            &IndexOptions {
                persist: false,
                ..IndexOptions::default()
            },
        );
        assert!(
            report.parse_failures.is_empty(),
            "{:?}",
            report.parse_failures
        );
        let file_id = |suffix: &str| {
            graph
                .files
                .iter()
                .find(|f| f.path.as_str().ends_with(suffix))
                .unwrap_or_else(|| panic!("missing {suffix}"))
                .id
                .0
        };
        let view = file_id("_ViewImports.cshtml");
        let widget = file_id("Widget.cs");
        let razor = file_id("Counter.razor");
        assert_eq!(
            graph
                .files
                .iter()
                .find(|f| f.id.0 == razor)
                .unwrap()
                .language,
            Some(Language::CSharp)
        );
        let targets: Vec<u32> = graph
            .edges
            .iter()
            .filter(|e| e.from == view && e.kind == EdgeKind::Namespace)
            .map(|e| e.to)
            .collect();
        assert_eq!(targets, vec![widget], "System/NuGet must not add edges");
        assert!(graph
            .symbols
            .iter()
            .any(|s| { s.name == "CounterWidget" && s.file.0 == razor }));
        assert!(graph
            .symbols
            .iter()
            .all(|s| s.name != "MarkupOnlyTitle" && s.name != "NotASymbol"));
    }

    #[test]
    fn code_block_members_and_using_statement() {
        let src = "@using (var c = new Disposable())\n{\n    <p>x</p>\n}\n@code {\n    [Parameter] public string Title { get; set; }\n    private int count;\n    private const int Max = 3;\n    void Inc() { count++; }\n}\n";
        let ex = extract_razor_csharp(src);
        assert_eq!(newlines(src), newlines(&ex));
        assert!(ex.contains("class __R"), "{ex}");
        assert!(!ex.contains("using (var"), "{ex}");
        let pool = ParserPool::new();
        let p = pool
            .parse(Language::CSharp, &RelPath::new("C.razor"), src)
            .unwrap();
        let find = |n: &str| p.symbols.iter().find(|s| s.name == n);
        for n in ["Title", "count", "Max", "Inc"] {
            assert!(find(n).is_some(), "missing {n}: {:?}", p.symbols);
        }
        assert_eq!(find("Inc").unwrap().kind, crate::types::SymbolKind::Method);
        assert_eq!(find("Inc").unwrap().start_line, 9);
        assert!(
            !p.calls.iter().any(|(c, _)| c.contains("Disposable")),
            "{:?}",
            p.calls
        );
    }
}
