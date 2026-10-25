//! C# preprocessor pre-pass for the error retry.
//!
//! tree-sitter-c-sharp parses `#if` / `#elif` / `#else` / `#endif` as syntax
//! and only accepts them in some positions. A directive that splits a
//! declaration (`class X\n#if A\n : Base\n#endif\n{`), a type parameter list
//! or a switch section yields ERROR nodes, drops type declarations and
//! truncates symbol ranges. Roslyn treats inactive branches as disabled text.
//! Without the project's `DefineConstants` we keep the first branch of every
//! conditional (`#if false` keeps the next one instead) and blank the others.
//! Declarations that exist only in a later branch (`#if NET20 … #else class
//! New … #endif`) are then recovered from a second, alternate retry that
//! keeps the *last* branch of every conditional: its symbols and calls on
//! lines the first-branch text blanked are added (see [`merge_alternate`]).
//!
//! Choosing branches from the owning project's `DefineConstants` and target
//! framework instead was measured on Newtonsoft.Json / serilog / Polly and
//! rejected: a single configuration drops whole files of the other targets
//! (`#if HAVE_HALF` test files under the tests' first framework `net46`),
//! 156 symbols lost against 1 gained.
//!
//! .NET 10 file-based apps start with `#:package` / `#:property` lines the
//! grammar does not know; those lines are blanked too.
//!
//! Blanking replaces every byte of a line except `\r` / `\n` with a space, so
//! byte offsets and line numbers of the remaining code are unchanged and the
//! result is valid UTF-8 (whole lines are replaced).

/// Kind of a preprocessor line relevant to the pre-pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Directive<'a> {
    If(&'a str),
    Elif(&'a str),
    Else,
    Endif,
    /// `#:` file-level directive (.NET 10 file-based apps).
    FileLevel,
}

fn directive(line: &str, vb: bool) -> Option<Directive<'_>> {
    let rest = line.trim_start_matches([' ', '\t']).strip_prefix('#')?;
    if !vb && rest.starts_with(':') {
        return Some(Directive::FileLevel);
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let word_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    let (word, tail) = rest.split_at(word_end);
    if vb {
        // VB keywords are case-insensitive: `#If`, `#ElseIf`, `#Else`, `#End If`.
        let word = word.to_ascii_lowercase();
        return match word.as_str() {
            "if" => Some(Directive::If(tail)),
            "elseif" => Some(Directive::Elif(tail)),
            "else" => Some(Directive::Else),
            "end"
                if tail
                    .trim_start()
                    .get(..2)
                    .is_some_and(|w| w.eq_ignore_ascii_case("if")) =>
            {
                Some(Directive::Endif)
            }
            _ => None,
        };
    }
    match word {
        "if" => Some(Directive::If(tail)),
        "elif" => Some(Directive::Elif(tail)),
        "else" => Some(Directive::Else),
        "endif" => Some(Directive::Endif),
        _ => None,
    }
}

/// Whether the branch guarded by `expr` is taken. Only a literal `false` (or
/// `!true`; VB `False` / `Not True`) is known to be inactive; every other
/// condition is assumed true.
fn branch_taken(expr: &str, vb: bool) -> bool {
    let cut = if vb { '\'' } else { '/' };
    let mut expr = expr.split(cut).next().unwrap_or("").trim().to_owned();
    if vb {
        expr = expr.to_ascii_lowercase();
        if let Some(e) = expr.strip_suffix("then") {
            expr = e.trim().to_owned();
        }
    }
    let expr = expr.trim_start_matches('(').trim_end_matches(')').trim();
    let compact = expr.replace(' ', "");
    !(expr == "false" || compact == "!true" || (vb && compact == "nottrue"))
}

/// One open `#if` group.
struct Group {
    /// The group is inside an active region.
    parent_active: bool,
    /// Some branch of this group was already taken.
    taken: bool,
    /// Index of this group in source order (see [`Pick::Last`]).
    id: usize,
    /// Index of the current branch within the group.
    branch: usize,
}

/// Which branch of each conditional stays.
enum Pick {
    /// The first branch whose condition is not literally false.
    First,
    /// Per group (source order), the last such branch.
    Last(Vec<Option<usize>>),
}

/// Returns the source with directive lines and inactive conditional branches
/// blanked (same byte length), or `None` when the source has no `#if` /
/// `#:` directive at all.
pub(super) fn blank_inactive(source: &str) -> Option<String> {
    blank_inactive_for(source, false, &Pick::First)
}

/// [`blank_inactive`] for Visual Basic (`#If … Then` / `#ElseIf` / `#Else` /
/// `#End If`; no `#:` lines).
pub(super) fn blank_inactive_vb(source: &str) -> Option<String> {
    blank_inactive_for(source, true, &Pick::First)
}

/// Like [`blank_inactive`] / [`blank_inactive_vb`] but keeps the last
/// branch of every conditional (`#else`, or the last `#elif`), for
/// [`merge_alternate`].
pub(super) fn blank_alternate(source: &str, vb: bool) -> Option<String> {
    if !source.contains('#') {
        return None;
    }
    let mut last: Vec<Option<usize>> = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for line in source.split_inclusive('\n') {
        match directive(line, vb) {
            Some(Directive::If(expr)) => {
                let id = last.len();
                last.push(branch_taken(expr, vb).then_some(0));
                stack.push((id, 0));
            }
            Some(d @ (Directive::Elif(_) | Directive::Else)) => {
                if let Some((id, branch)) = stack.last_mut() {
                    *branch += 1;
                    if !matches!(d, Directive::Elif(e) if !branch_taken(e, vb)) {
                        last[*id] = Some(*branch);
                    }
                }
            }
            Some(Directive::Endif) => {
                stack.pop();
            }
            _ => {}
        }
    }
    blank_inactive_for(source, vb, &Pick::Last(last))
}

fn blank_inactive_for(source: &str, vb: bool, pick: &Pick) -> Option<String> {
    if !source.contains('#') {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    let mut changed = false;
    let mut stack: Vec<Group> = Vec::new();
    let mut active = true;
    let mut pos = 0usize;
    let mut next_id = 0usize;
    for line in source.split_inclusive('\n') {
        let blank = match directive(line, vb) {
            Some(Directive::FileLevel) => true,
            Some(Directive::If(expr)) => {
                let id = next_id;
                next_id += 1;
                let taken = match pick {
                    Pick::First => branch_taken(expr, vb),
                    Pick::Last(last) => last.get(id).copied().flatten() == Some(0),
                };
                stack.push(Group {
                    parent_active: active,
                    taken,
                    id,
                    branch: 0,
                });
                active = active && taken;
                true
            }
            Some(d @ (Directive::Elif(_) | Directive::Else)) => {
                if let Some(group) = stack.last_mut() {
                    group.branch += 1;
                    let cur = match (pick, d) {
                        (Pick::Last(last), _) => {
                            last.get(group.id).copied().flatten() == Some(group.branch)
                        }
                        (Pick::First, Directive::Elif(expr)) => {
                            !group.taken && branch_taken(expr, vb)
                        }
                        (Pick::First, _) => !group.taken,
                    };
                    group.taken |= cur;
                    active = group.parent_active && cur;
                }
                true
            }
            Some(Directive::Endif) => {
                if let Some(group) = stack.pop() {
                    active = group.parent_active;
                }
                true
            }
            None => !active,
        };
        if blank {
            if !changed {
                out.reserve(source.len());
                out.extend_from_slice(&source.as_bytes()[..pos]);
                changed = true;
            }
            out.extend(
                line.bytes()
                    .map(|b| if b == b'\n' || b == b'\r' { b } else { b' ' }),
            );
        } else if changed {
            out.extend_from_slice(line.as_bytes());
        }
        pos += line.len();
    }
    if !changed {
        return None;
    }
    // Whole lines were replaced by ASCII, so the bytes stay valid UTF-8.
    String::from_utf8(out).ok()
}

/// tree-sitter-vb-dotnet only accepts `Option` / `Imports` statements as one
/// contiguous block at the top: a blank or comment-only line before or
/// between them (`Imports System\n\nImports Acme`) ends the header, and every
/// later `Imports` is an ERROR that fails the whole file. Returns the source
/// with the header lines regrouped (`Option`s, then `Imports`, then the
/// blank / comment lines, each line kept whole), or `None` when the header
/// is already contiguous. Total length and line count are unchanged; only
/// header lines move, and they carry no symbols, so every symbol and call
/// keeps its line.
pub(super) fn regroup_vb_header(source: &str) -> Option<String> {
    let mut options: Vec<&str> = Vec::new();
    let mut imports: Vec<&str> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    let mut end = 0usize;
    // Lines of one logical statement (` _` continuations) stay together.
    let mut lines = source.split_inclusive('\n').peekable();
    while let Some(first) = lines.peek().copied() {
        if !first.ends_with('\n') {
            break;
        }
        let head = first.trim_start_matches([' ', '\t']);
        let word = head
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        let is_option = word.eq_ignore_ascii_case("Option");
        let is_imports = word.eq_ignore_ascii_case("Imports");
        let blank_or_comment = head.trim().is_empty()
            || head.starts_with('\'')
            || (word.eq_ignore_ascii_case("REM") && head.len() > 3);
        if !(is_option || is_imports || blank_or_comment) {
            break;
        }
        let start = end;
        for line in lines.by_ref() {
            end += line.len();
            let continued = line.trim_end().ends_with(" _") || line.trim_end() == "_";
            if !continued || !line.ends_with('\n') {
                break;
            }
        }
        if !source[start..end].ends_with('\n') {
            return None;
        }
        let chunk = &source[start..end];
        if is_option {
            options.push(chunk);
        } else if is_imports {
            imports.push(chunk);
        } else {
            other.push(chunk);
        }
    }
    let mut out = String::with_capacity(source.len());
    for chunk in options.iter().chain(&imports).chain(&other) {
        out.push_str(chunk);
    }
    if out == source[..end] {
        return None;
    }
    out.push_str(&source[end..]);
    Some(out)
}

/// Attribute-target keywords that tree-sitter-c-sharp 0.23 also takes for
/// a target inside a collection expression (`Invoke(this, [method])` is an
/// ERROR).
const ATTRIBUTE_TARGETS: &[&str] = &[
    "assembly", "event", "field", "method", "module", "param", "property", "return", "type",
    "typevar",
];

/// `[method]` / `[field, x]`: a target keyword alone as a collection
/// element (no `:` after it, so not an attribute). Its first letter becomes
/// `_` (same length, still an identifier). `None` when there is none.
pub(super) fn rename_collection_targets(source: &str) -> Option<String> {
    if !source.contains('[') {
        return None;
    }
    let b = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    for (open, _) in source.match_indices('[') {
        let mut i = open + 1;
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_lowercase() {
            i += 1;
        }
        if !ATTRIBUTE_TARGETS.contains(&&source[start..i]) {
            continue;
        }
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if matches!(b.get(i), Some(b']' | b',')) {
            out.get_or_insert_with(|| b.to_vec())[start] = b'_';
        }
    }
    out.and_then(|bytes| String::from_utf8(bytes).ok())
}

/// Imports of the original (partial) parse in source order, then those only
/// the retry found. The blanked branches' `using`s are real dependencies of
/// some build configuration, so dropping them would lose import edges.
pub(super) fn union_imports(original: Vec<String>, retry: Vec<String>) -> Vec<String> {
    let mut out = original;
    let known: std::collections::HashSet<String> = out.iter().cloned().collect();
    out.extend(retry.into_iter().filter(|i| !known.contains(i)));
    out
}

/// Add to `primary` (the clean parse of the first-branch text `kept`) what
/// the clean parse `alt` of the last-branch text found on lines `kept`
/// blanked although the source has code there: symbols whose name
/// `primary` does not already have, calls, and imports. Line numbers are
/// shared because blanking keeps every line in place.
pub(super) fn merge_alternate(
    primary: &mut super::ParsedFile,
    source: &str,
    kept: &str,
    alt: super::ParsedFile,
) {
    // 1-based line -> blanked by the first-branch text although it has code.
    let mut dropped = vec![false];
    for (orig, kept) in source.split('\n').zip(kept.split('\n')) {
        dropped.push(kept.trim().is_empty() && !orig.trim().is_empty());
    }
    let on_dropped = |line: u32| dropped.get(line as usize).copied().unwrap_or(false);
    // By name: the same member declared per branch with another kind
    // (`const` vs `static readonly`) or signature is one declaration.
    let mut known: std::collections::HashSet<String> =
        primary.symbols.iter().map(|s| s.name.clone()).collect();
    for sym in alt.symbols {
        if on_dropped(sym.start_line) && known.insert(sym.name.clone()) {
            primary.symbols.push(sym);
        }
    }
    primary
        .calls
        .extend(alt.calls.into_iter().filter(|(_, line)| on_dropped(*line)));
    let imports = std::mem::take(&mut primary.imports);
    primary.imports = union_imports(imports, alt.imports);
}

#[cfg(test)]
mod tests {
    use super::{blank_inactive, union_imports};

    #[test]
    fn union_keeps_original_order_and_adds_retry_only() {
        let got = union_imports(vec!["A".into(), "B".into()], vec!["B".into(), "C".into()]);
        assert_eq!(got, ["A", "B", "C"]);
    }

    #[test]
    fn keeps_first_branch_and_preserves_offsets() {
        let src = "class X\n#if A\n : Base\n#elif B\n : Other\n#else\n : Third\n#endif\n{ }\n";
        let out = blank_inactive(src).expect("changed");
        assert_eq!(out.len(), src.len());
        assert_eq!(out.lines().count(), src.lines().count());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "class X");
        assert_eq!(lines[1].trim(), "");
        assert_eq!(lines[2], " : Base");
        assert_eq!(lines[4].trim(), "");
        assert_eq!(lines[6].trim(), "");
        assert_eq!(lines[8], "{ }");
    }

    #[test]
    fn if_false_takes_next_branch_and_nesting_is_respected() {
        let src = "#if false\nA\n#elif X\nB\n#if Y\nC\n#else\nD\n#endif\n#else\nE\n#endif\n";
        let out = blank_inactive(src).unwrap();
        let kept: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(kept, ["B", "C"]);
    }

    #[test]
    fn file_level_directives_and_crlf_and_multibyte() {
        let src = "#:package Foo@1.0\r\n#:property X=é\r\nclass C {}\r\n";
        let out = blank_inactive(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert!(out.ends_with("class C {}\r\n"));
        assert_eq!(out.matches("\r\n").count(), 3);
    }

    #[test]
    fn vb_directives_case_insensitive_and_false() {
        let src = "#If False Then\nA\n#ElseIf X Then ' c\nB\n#Else\nC\n#End If\n#Region \"R\"\n#End Region\n";
        let out = super::blank_inactive_vb(src).unwrap();
        assert_eq!(out.len(), src.len());
        let kept: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(kept, ["B", "#Region \"R\"", "#End Region"]);
    }

    #[test]
    fn alternate_keeps_last_branch_and_skips_literal_false() {
        let src = "#if A\nA\n#elif B\nB\n#else\nC\n#endif\n#if X\nX\n#endif\n#if P\nP\n#elif false\nF\n#endif\n#if Q\n#if R\nR\n#else\nS\n#endif\n#else\nT\n#endif\n";
        let out = super::blank_alternate(src, false).unwrap();
        assert_eq!(out.len(), src.len());
        let kept: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        // Last branch of each group; a lone `#if` stays; `#elif false` is
        // never chosen; nested groups inside a dropped branch stay dropped.
        assert_eq!(kept, ["C", "X", "P", "T"]);
        let vb = "#If A Then\nA\n#Else\nB\n#End If\n";
        let out = super::blank_alternate(vb, true).unwrap();
        assert_eq!(
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>(),
            ["B"]
        );
    }

    #[test]
    fn no_directive_is_none() {
        assert!(blank_inactive("class C { string s = \"#\"; }\n").is_none());
        assert!(blank_inactive("#region R\nclass C {}\n#endregion\n").is_none());
    }

    mod retry {
        use crate::parse::{ParsedFile, ParserPool};
        use crate::types::{Language, RelPath};

        fn parse(path: &str, src: &str) -> ParsedFile {
            ParserPool::new()
                .parse(Language::CSharp, &RelPath::new(path), src)
                .expect("parse")
        }

        fn has(p: &ParsedFile, kind: &str, name: &str) -> Option<(u32, u32)> {
            p.symbols
                .iter()
                .find(|s| format!("{:?}", s.kind) == kind && s.name == name)
                .map(|s| (s.start_line, s.end_line))
        }

        #[test]
        fn if_splitting_base_list_recovers_class_and_range() {
            let src = "public class X\n#if A\n  : Base\n#endif\n{\n  void M() {}\n}\n";
            let p = parse("X.cs", src);
            assert!(!p.partial, "retry must parse clean");
            assert_eq!(has(&p, "Class", "X"), Some((1, 7)));
            assert!(has(&p, "Method", "M").is_some());
            assert!(!p.symbols.iter().any(|s| s.name == "if"));
        }

        #[test]
        fn if_inside_type_parameters_and_switch_arms() {
            let p = parse(
                "I.cs",
                "public interface IJ<\n#if V\n  out\n#endif\n  T> { }\n",
            );
            assert!(!p.partial);
            assert_eq!(has(&p, "Interface", "IJ"), Some((1, 5)));
            let p = parse(
                "C.cs",
                "class C { int F(int t) => t switch {\n  1 => 2,\n#if !N\n  3 => 4,\n#endif\n  _ => 0 }; }\n",
            );
            assert!(!p.partial);
            assert!(has(&p, "Method", "F").is_some());
        }

        #[test]
        fn dotnet10_file_directives_in_cs_and_csx() {
            let src =
                "#:package Foo@1.0\n#:property LangVersion=preview\nclass C { void Run() {} }\n";
            for path in ["app.cs", "script.csx"] {
                let p = parse(path, src);
                assert!(!p.partial, "{path}");
                assert_eq!(has(&p, "Class", "C"), Some((3, 3)), "{path}");
            }
        }

        #[test]
        fn blanked_branch_usings_stay_imports() {
            let src = "#if NET20\nusing Old.Linq;\n#else\nusing System.Linq;\n#endif\npublic class X\n#if A\n  : Base\n#endif\n{ }\n";
            let p = parse("U.cs", src);
            assert!(!p.partial);
            assert!(
                p.imports.iter().any(|i| i.contains("System.Linq")),
                "{:?}",
                p.imports
            );
            assert!(
                p.imports.iter().any(|i| i.contains("Old.Linq")),
                "{:?}",
                p.imports
            );
        }

        #[test]
        fn vb_if_directives_in_class_body() {
            // tree-sitter-vb has no `#If` support at all: any directive is an
            // ERROR.
            let src = "Imports System.Linq\nPublic Class X\n#If DEBUG Then\n    Sub M()\n    End Sub\n#ElseIf B Then\n    Sub Q()\n    End Sub\n#End If\n    Sub N()\n    End Sub\nEnd Class\n";
            let p = ParserPool::new()
                .parse(Language::VisualBasic, &RelPath::new("X.vb"), src)
                .expect("parse");
            assert!(!p.partial, "VB retry must parse clean");
            assert!(has(&p, "Class", "X").is_some());
            assert_eq!(has(&p, "Method", "M").map(|r| r.0), Some(4));
            assert!(has(&p, "Method", "N").is_some());
            // Only in the `#ElseIf` branch: recovered by the last-branch retry.
            assert_eq!(has(&p, "Method", "Q").map(|r| r.0), Some(7));
        }

        #[test]
        fn else_only_declarations_are_recovered_once() {
            // The `#if` splitting the base list forces the retry; `New` and
            // `Helper` exist only in `#else` branches, `Size` in both (with
            // another kind), so it is not duplicated.
            let src = "public class X\n#if A\n  : Base\n#endif\n{\n#if NET20\n  void Old() { Call1(); }\n  const int Size = 1;\n#else\n  void New() { Call2(); }\n  static readonly int Size = 2;\n#endif\n}\n#if NET20\n#else\nclass Helper { }\n#endif\n";
            let p = parse("X.cs", src);
            assert!(!p.partial);
            assert_eq!(has(&p, "Method", "Old").map(|r| r.0), Some(7));
            assert_eq!(has(&p, "Method", "New").map(|r| r.0), Some(10));
            assert_eq!(has(&p, "Class", "Helper").map(|r| r.0), Some(16));
            assert_eq!(p.symbols.iter().filter(|s| s.name == "Size").count(), 1);
            assert_eq!(p.symbols.iter().filter(|s| s.name == "X").count(), 1);
            assert!(p.calls.iter().any(|(n, l)| n == "Call2" && *l == 10));
        }

        fn vb(src: &str) -> ParsedFile {
            ParserPool::new()
                .parse(Language::VisualBasic, &RelPath::new("M.vb"), src)
                .expect("parse")
        }

        #[test]
        fn vb_blank_and_comment_lines_in_imports_header() {
            for src in [
                "Imports System\n\nImports System.Linq\nPublic Class X\n    Sub M()\n    End Sub\nEnd Class\n",
                "\nImports System\nImports System.Linq\nPublic Class X\n    Sub M()\n    End Sub\nEnd Class\n",
                "' header comment\r\nImports System\r\n\r\nImports System.Linq\r\nPublic Class X\r\n    Sub M()\r\n    End Sub\r\nEnd Class\r\n",
                "Option Strict On\n\nImports System, _\n    System.Linq\n\nPublic Class X\n    Sub M()\n    End Sub\nEnd Class\n",
            ] {
                let p = vb(src);
                assert!(!p.partial, "{src:?}");
                let class_line = src.lines().position(|l| l.contains("Class X")).unwrap() as u32 + 1;
                assert_eq!(has(&p, "Class", "X").map(|r| r.0), Some(class_line), "{src:?}");
                assert_eq!(has(&p, "Method", "M").map(|r| r.0), Some(class_line + 1));
                assert!(p.imports.iter().any(|i| i.contains("System.Linq")), "{:?}", p.imports);
            }
        }

        #[test]
        fn vb_header_regroup_shape() {
            let src =
                "' c\nOption Strict On\n\nImports A\nImports B, _\n  C\nModule M\nEnd Module\n";
            let out = super::super::regroup_vb_header(src).unwrap();
            assert_eq!(out.len(), src.len());
            assert_eq!(out.lines().count(), src.lines().count());
            assert!(out
                .starts_with("Option Strict On\nImports A\nImports B, _\n  C\n' c\n\nModule M\n"));
            assert!(super::super::regroup_vb_header(
                "Imports A\nImports B\n\nModule M\nEnd Module\n"
            )
            .is_none());
            assert!(super::super::regroup_vb_header("Module M\nEnd Module\n").is_none());
        }

        #[test]
        fn collection_element_named_like_attribute_target() {
            let src = "class T {\n  void Run(System.Reflection.MethodInfo m, object method) {\n    m.Invoke(this, [method]);\n    Use([method, field ]);\n  }\n  [method: Obsolete] void Old() {}\n}\n";
            let p = parse("T.cs", src);
            assert!(!p.partial);
            assert!(has(&p, "Method", "Run").is_some());
            assert!(has(&p, "Method", "Old").is_some());
            assert!(
                p.calls.iter().any(|(n, l)| n == "Use" && *l == 4),
                "{:?}",
                p.calls
            );
            let out = super::super::rename_collection_targets(src).unwrap();
            assert_eq!(out.len(), src.len());
            assert!(out.contains("[_ethod]") && out.contains("[_ethod, field ]"));
            assert!(out.contains("[method: Obsolete]"), "attribute untouched");
        }

        #[test]
        fn unrelated_error_keeps_first_pass() {
            // The retry is still partial (real syntax error), so the original
            // parse is returned unchanged.
            let src = "public class X\n#if A\n  : Base\n#endif\n{\n  void M() { int = ; }\n}\n";
            let p = parse("E.cs", src);
            assert!(p.partial);
        }

        #[test]
        fn clean_file_with_directives_is_untouched() {
            let src = "class C {\n#if DEBUG\n  void D() {}\n#else\n  void R() {}\n#endif\n}\n";
            let p = parse("Clean.cs", src);
            assert!(!p.partial);
            // First pass was clean: both branches stay visible.
            assert!(has(&p, "Method", "D").is_some());
            assert!(has(&p, "Method", "R").is_some());
        }
    }
}
