//! Pick the cursor for a name-based language-server query.
//!
//! LSP requests are position-based. Anchoring on the first *textual*
//! occurrence of a name is wrong surprisingly often: Go and Rust doc comments
//! start with the item name, Python docstrings mention it, imports and
//! same-named locals precede the definition. Any of those sends the server to
//! the wrong token, and the answer comes back as an error or as a confidently
//! wrong "no references".
//!
//! This module parses the file with tree-sitter and only considers identifier
//! tokens that are code (never inside a comment or string). Tokens that are the
//! `name` field of a declaration node are preferred; when there are several
//! (overloads, multiple impls, shadowing definitions) all are returned so the
//! caller can query each and merge.

use crate::parse::grammar;
use crate::types::{Language, RelPath};

use super::Position;

/// Where a name-based query should point the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Name tokens of declarations in this file (non-empty, file order).
    Definitions(Vec<Position>),
    /// No declaration here; first code (non-comment, non-string) occurrence.
    Usage(Position),
    /// The name only appears inside comments / strings.
    OnlyInCommentsOrStrings,
    /// The name does not appear at all.
    NotFound,
}

/// Upper bound on definition anchors queried and merged for one name.
pub const MAX_DEFINITION_ANCHORS: usize = 8;

/// Choose anchors for `name` in `source` (the contents of `path`).
///
/// Languages without a tree-sitter grammar fall back to a line-based scan that
/// skips obvious comment lines; that fallback never reports definitions.
pub fn locate(path: &RelPath, source: &str, name: &str) -> Anchor {
    if name.is_empty() {
        return Anchor::NotFound;
    }
    let tree = Language::from_path(path)
        .and_then(grammar)
        .and_then(|lang| {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&lang).ok()?;
            parser.parse(source, None)
        });
    match tree {
        Some(tree) => locate_in_tree(&tree, source, name),
        None => locate_textual(source, name),
    }
}

/// Is `position` inside a comment or string literal? `None` when the file
/// cannot be parsed (unknown grammar).
pub fn position_in_comment_or_string(
    path: &RelPath,
    source: &str,
    position: Position,
) -> Option<bool> {
    let lang = Language::from_path(path).and_then(grammar)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(source, None)?;
    let byte = byte_offset(source, position)?;
    let node = tree
        .root_node()
        .descendant_for_byte_range(byte, byte.saturating_add(1).min(source.len()))?;
    Some(in_comment_or_string(node))
}

/// For each position in `source`, whether it is code (not inside a comment
/// or string literal). One parse for the whole batch. `None` when the file
/// has no grammar.
pub(crate) fn code_mask(path: &RelPath, source: &str, positions: &[Position]) -> Option<Vec<bool>> {
    let lang = Language::from_path(path).and_then(grammar)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(source, None)?;
    Some(
        positions
            .iter()
            .map(|&position| {
                let Some(byte) = byte_offset(source, position) else {
                    return true;
                };
                tree.root_node()
                    .descendant_for_byte_range(byte, byte.saturating_add(1).min(source.len()))
                    .is_none_or(|node| !in_comment_or_string(node))
            })
            .collect(),
    )
}

fn locate_in_tree(tree: &tree_sitter::Tree, source: &str, name: &str) -> Anchor {
    let mut defs = Vec::new();
    let mut first_code: Option<Position> = None;
    let mut seen_text = false;
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.child_count() == 0 || is_leafy_identifier(node.kind()) {
            if node.utf8_text(source.as_bytes()).ok() == Some(name) {
                seen_text = true;
                if !in_comment_or_string(node) {
                    let pos = node_position(source, node);
                    if is_declaration_name(node) {
                        defs.push(pos);
                    }
                    if first_code.is_none_or(|p| pos < p) {
                        first_code = Some(pos);
                    }
                }
            } else if !seen_text && is_comment_or_string_kind(node.kind()) {
                // Name inside a comment/string token is not a separate leaf.
                if let Ok(text) = node.utf8_text(source.as_bytes()) {
                    if contains_identifier(text, name) {
                        seen_text = true;
                    }
                }
            }
            if node.child_count() == 0 {
                continue;
            }
        }
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    if !defs.is_empty() {
        defs.sort();
        defs.dedup();
        return Anchor::Definitions(defs);
    }
    if let Some(pos) = first_code {
        return Anchor::Usage(pos);
    }
    if seen_text || contains_identifier(source, name) {
        Anchor::OnlyInCommentsOrStrings
    } else {
        Anchor::NotFound
    }
}

fn is_leafy_identifier(kind: &str) -> bool {
    // Some grammars wrap identifier text in a node with children (rare); we
    // still treat the named token itself as a candidate.
    kind.ends_with("identifier")
}

fn is_comment_or_string_kind(kind: &str) -> bool {
    kind.contains("comment") || kind.contains("string") || kind == "heredoc_body"
}

fn in_comment_or_string(node: tree_sitter::Node<'_>) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if is_comment_or_string_kind(n.kind()) {
            // Interpolations inside strings (`${x}`, f"{x}") are code.
            return true;
        }
        if n.kind().contains("interpolation") || n.kind() == "template_substitution" {
            return false;
        }
        cur = n.parent();
    }
    false
}

/// True when `node` is the `name` field of a declaration-like parent.
fn is_declaration_name(node: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let is_name_field = parent
        .child_by_field_name("name")
        .is_some_and(|n| n.id() == node.id());
    if !is_name_field {
        // C/C++ declarators: `int foo(...)` → function_declarator.declarator.
        let declarator = parent
            .child_by_field_name("declarator")
            .is_some_and(|n| n.id() == node.id());
        return declarator && parent.kind().ends_with("declarator");
    }
    let kind = parent.kind();
    !(kind.contains("call")
        || kind.contains("scoped")
        || kind.contains("use")
        || kind.contains("import")
        || kind.contains("qualified")
        || kind.contains("path")
        || kind.contains("member")
        || kind.contains("selector")
        || kind.contains("expression")
        || kind.contains("argument")
        || kind.contains("attribute")
        || kind.contains("meta")
        || kind.contains("label"))
}

fn node_position(source: &str, node: tree_sitter::Node<'_>) -> Position {
    let start = node.start_position();
    let line_start = node.start_byte() - start.column;
    let character = source[line_start..node.start_byte()].encode_utf16().count() as u32;
    Position {
        line: start.row as u32,
        character,
    }
}

fn byte_offset(source: &str, position: Position) -> Option<usize> {
    let mut offset = 0usize;
    for (idx, line) in source.split('\n').enumerate() {
        if idx as u32 == position.line {
            let mut units = 0u32;
            for (b, ch) in line.char_indices() {
                if units >= position.character {
                    return Some(offset + b);
                }
                units += ch.len_utf16() as u32;
            }
            return Some(offset + line.len());
        }
        offset += line.len() + 1;
    }
    None
}

fn locate_textual(source: &str, name: &str) -> Anchor {
    let mut seen = false;
    for (line_idx, line) in source.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(character) = identifier_column(line, name) {
            seen = true;
            let trimmed = line.trim_start();
            let comment = ["//", "#", "/*", "*", "--", "'''", "\"\"\""]
                .iter()
                .any(|p| trimmed.starts_with(p));
            if !comment {
                return Anchor::Usage(Position {
                    line: line_idx as u32,
                    character,
                });
            }
        }
    }
    if seen {
        Anchor::OnlyInCommentsOrStrings
    } else {
        Anchor::NotFound
    }
}

fn contains_identifier(text: &str, name: &str) -> bool {
    text.split('\n')
        .any(|l| identifier_column(l, name).is_some())
}

/// UTF-16 column of the first whole-identifier match of `name` in `line`.
pub(crate) fn identifier_column(line: &str, name: &str) -> Option<u32> {
    for (byte_idx, _) in line.match_indices(name) {
        let before_ok = match line[..byte_idx].chars().next_back() {
            None => true,
            Some(ch) => !is_ident_char(ch),
        };
        let after = byte_idx + name.len();
        let after_ok = match line[after..].chars().next() {
            None => true,
            Some(ch) => !is_ident_char(ch),
        };
        if before_ok && after_ok {
            return Some(line[..byte_idx].encode_utf16().count() as u32);
        }
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn go_doc_comment_is_skipped_for_func_name() {
        let src = "package cmd\n\n// Execute runs the command.\nfunc Execute() error {\n\treturn nil\n}\n";
        let anchor = locate(&RelPath::new("cmd.go"), src, "Execute");
        assert_eq!(anchor, Anchor::Definitions(vec![pos(3, 5)]));
    }

    #[test]
    fn rust_doc_comment_and_use_are_skipped() {
        let src = "use crate::pool::LspPool as _P;\n/// LspPool keeps servers.\npub struct LspPool {\n    n: u32,\n}\n";
        let anchor = locate(&RelPath::new("src/a.rs"), src, "LspPool");
        assert_eq!(anchor, Anchor::Definitions(vec![pos(2, 11)]));
    }

    #[test]
    fn rust_multiple_impl_methods_return_all_definitions() {
        let src = "struct A;\nstruct B;\nimpl A { fn run(&self) {} }\nimpl B { fn run(&self) {} }\nfn main() { A.run(); }\n";
        let anchor = locate(&RelPath::new("m.rs"), src, "run");
        assert_eq!(anchor, Anchor::Definitions(vec![pos(2, 12), pos(3, 12)]));
    }

    #[test]
    fn python_docstring_mention_falls_to_code_usage() {
        let src = "\"\"\"Uses helper a lot.\"\"\"\nfrom m import helper\n\nhelper()\n";
        let anchor = locate(&RelPath::new("x.py"), src, "helper");
        assert_eq!(anchor, Anchor::Usage(pos(1, 14)));
    }

    #[test]
    fn name_only_in_comment_is_reported() {
        let src = "package a\n// Foo is gone.\nvar s = \"Foo\"\n";
        let anchor = locate(&RelPath::new("a.go"), src, "Foo");
        assert_eq!(anchor, Anchor::OnlyInCommentsOrStrings);
        assert_eq!(locate(&RelPath::new("a.go"), src, "Bar"), Anchor::NotFound);
    }

    #[test]
    fn edit_then_relocate_sees_new_definition() {
        let before = "package a\n// Run does it.\nfunc helper() {}\n";
        assert_eq!(
            locate(&RelPath::new("a.go"), before, "Run"),
            Anchor::OnlyInCommentsOrStrings
        );
        let after = "package a\n// Run does it.\nfunc Run() {}\n";
        assert_eq!(
            locate(&RelPath::new("a.go"), after, "Run"),
            Anchor::Definitions(vec![pos(2, 5)])
        );
    }

    #[test]
    fn position_in_comment_detected() {
        let src = "package a\n// Run does it.\nfunc Run() {}\n";
        let p = RelPath::new("a.go");
        assert_eq!(
            position_in_comment_or_string(&p, src, pos(1, 3)),
            Some(true)
        );
        assert_eq!(
            position_in_comment_or_string(&p, src, pos(2, 5)),
            Some(false)
        );
    }

    #[test]
    fn unknown_grammar_uses_textual_fallback() {
        let src = "// Foo here\nlet x = Foo()\n";
        assert_eq!(
            locate(&RelPath::new("a.swift"), src, "Foo"),
            Anchor::Usage(pos(1, 8))
        );
    }
}
