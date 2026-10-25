//! Kotlin (`.kt`, `.kts`) extraction hooks (lang-batch-8).
//!
//! `parse/mod.rs` calls these hooks for [`Language::Kotlin`] only, so all
//! Kotlin-specific post-processing lives here. The query
//! (`queries/kotlin/symbols.scm`) tags every declaration with a coarse kind;
//! [`symbol_name`] refines it from the syntax tree:
//!
//! | node                     | kind                                         |
//! |--------------------------|----------------------------------------------|
//! | `class_declaration`      | Interface / Enum / Class                     |
//! | `object_declaration`, `companion_object` | Class (unnamed companion → `Companion`) |
//! | `function_declaration`   | Method inside a class body, else Function    |
//! | `secondary_constructor`  | Method named after the enclosing class       |
//! | `property_declaration`   | Const (`const val`), Field (member), Variable (top level) |
//! | `class_parameter`        | Field (`val`/`var` constructor property)     |
//! | `enum_entry`             | Const                                        |

use crate::types::SymbolKind;

/// Name the Kotlin compiler gives an unnamed `companion object`.
const COMPANION: &str = "Companion";

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    match definition.kind() {
        "class_declaration" => {
            let kind = if has_token(definition, "interface") {
                SymbolKind::Interface
            } else if has_modifier(definition, source, "enum") {
                SymbolKind::Enum
            } else {
                SymbolKind::Class
            };
            (name.to_owned(), kind)
        }
        "companion_object" if definition.child_by_field_name("name").is_none() => {
            (COMPANION.to_owned(), SymbolKind::Class)
        }
        "function_declaration" => {
            let kind = if in_class_body(definition) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            (name.to_owned(), kind)
        }
        "secondary_constructor" => {
            let owner = enclosing_type_name(definition, source).unwrap_or("constructor");
            (owner.to_owned(), SymbolKind::Method)
        }
        "property_declaration" => {
            let kind = if has_modifier(definition, source, "const") {
                SymbolKind::Const
            } else if in_class_body(definition) {
                SymbolKind::Field
            } else {
                SymbolKind::Variable
            };
            (name.to_owned(), kind)
        }
        _ => (name.to_owned(), kind),
    }
}

/// Whether a definition is visible outside its file: not `private` itself, not
/// nested in a `private` declaration, and not local to a function body.
pub(super) fn exported(_name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            "function_body"
            | "lambda_literal"
            | "anonymous_initializer"
            | "getter"
            | "setter"
            | "object_literal" => return false,
            "class_declaration"
            | "object_declaration"
            | "companion_object"
            | "function_declaration"
            | "property_declaration"
            | "secondary_constructor"
            | "class_parameter"
            | "type_alias"
                if has_modifier(n, source, "private") =>
            {
                return false;
            }
            _ => {}
        }
        cur = n.parent();
    }
    true
}

fn in_class_body(node: tree_sitter::Node<'_>) -> bool {
    // Tolerate grammar error recovery: `class A(val x: Int) { fun m() {} }`
    // can leave the member under an ERROR node or directly under the class.
    let mut cur = node.parent();
    while let Some(p) = cur {
        match p.kind() {
            "class_body" | "enum_class_body" | "class_declaration" | "object_declaration"
            | "companion_object" => return true,
            "ERROR" => cur = p.parent(),
            _ => return false,
        }
    }
    false
}

/// Anonymous keyword child (`interface`, `class`, ...).
fn has_token(node: tree_sitter::Node<'_>, token: &str) -> bool {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == token);
    found
}

/// Any modifier word under the node's own `modifiers` child.
fn has_modifier(node: tree_sitter::Node<'_>, source: &str, word: &str) -> bool {
    let mut cursor = node.walk();
    let Some(mods) = node.children(&mut cursor).find(|c| c.kind() == "modifiers") else {
        return false;
    };
    let mut inner = mods.walk();
    let found = mods.children(&mut inner).any(|m| {
        m.kind() != "annotation" && m.utf8_text(source.as_bytes()).is_ok_and(|t| t == word)
    });
    found
}

fn enclosing_type_name<'s>(node: tree_sitter::Node<'_>, source: &'s str) -> Option<&'s str> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "class_declaration" | "object_declaration") {
            return n
                .child_by_field_name("name")?
                .utf8_text(source.as_bytes())
                .ok();
        }
        cur = n.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath, SymbolKind};

    const SAMPLE: &str = include_str!("../../tests/fixtures/kotlin/Sample.kt");

    fn symbols(src: &str) -> Vec<(String, SymbolKind, bool)> {
        let pool = ParserPool::new();
        let parsed = pool
            .parse(Language::Kotlin, &RelPath::new("Sample.kt"), src)
            .expect("parse");
        parsed
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.exported))
            .collect()
    }

    fn find(syms: &[(String, SymbolKind, bool)], name: &str) -> Vec<(SymbolKind, bool)> {
        syms.iter()
            .filter(|(n, _, _)| n == name)
            .map(|(_, k, e)| (*k, *e))
            .collect()
    }

    #[test]
    fn every_declaration_kind_is_extracted() {
        let syms = symbols(SAMPLE);
        use SymbolKind::*;
        let expect: &[(&str, SymbolKind)] = &[
            ("Callback", Type),
            ("MAX", Const),
            ("first", Variable),
            ("second", Variable),
            ("doubled", Variable),
            ("describe", Function),
            ("hidden", Function),
            ("Outer", Class),
            ("id", Field),
            ("label", Field),
            ("lazyValue", Field),
            ("a", Field),
            ("b", Field),
            ("work", Method),
            ("innerHelper", Function),
            ("Companion", Class),
            ("KEY", Const),
            ("create", Method),
            ("Registry", Class),
            ("entries", Field),
            ("Inner", Class),
            ("plus", Method),
            ("anon", Field),
            ("run", Method),
            ("Named", Class),
            ("Factory", Class),
            ("Shape", Interface),
            ("area", Field),
            ("draw", Method),
            ("Action", Interface),
            ("invoke", Method),
            ("Color", Enum),
            ("rgb", Field),
            ("RED", Const),
            ("GREEN", Const),
            ("Point", Class),
            ("x", Field),
            ("y", Field),
            ("Marker", Class),
            ("Result", Class),
            ("Ok", Class),
            ("value", Field),
            ("Fail", Class),
            ("Base", Class),
        ];
        for (name, kind) in expect {
            let got = find(&syms, name);
            assert!(
                got.iter().any(|(k, _)| k == kind),
                "{name}: expected {kind:?}, got {got:?}"
            );
        }
        // Secondary constructor is named after its class.
        assert_eq!(
            find(&syms, "Outer")
                .iter()
                .filter(|(k, _)| *k == Method)
                .count(),
            1
        );
        // Locals and plain constructor parameters are not symbols.
        for absent in ["local", "plain", "other"] {
            assert!(find(&syms, absent).is_empty(), "{absent} should be absent");
        }
        // `label()` overrides in enum entry + enum body.
        assert_eq!(find(&syms, "label").len(), 3);
    }

    #[test]
    fn visibility() {
        let syms = symbols(SAMPLE);
        assert_eq!(find(&syms, "hidden"), vec![(SymbolKind::Function, false)]);
        assert!(find(&syms, "innerHelper").iter().all(|(_, e)| !e));
        assert!(find(&syms, "a").iter().all(|(_, e)| !e));
        assert!(find(&syms, "describe").iter().all(|(_, e)| *e));
        assert!(find(&syms, "KEY").iter().all(|(_, e)| *e));
        // Members of an anonymous object are not reachable by name.
        assert!(find(&syms, "run").iter().all(|(_, e)| !e));
    }

    #[test]
    fn kts_script_top_level() {
        let src = "plugins { id(\"x\") }\nval version = \"1\"\nfun helper() {}\n";
        let syms = symbols(src);
        assert_eq!(find(&syms, "version"), vec![(SymbolKind::Variable, true)]);
        assert_eq!(find(&syms, "helper"), vec![(SymbolKind::Function, true)]);
    }

    /// Corpus dump for the recall bench (`gt_kotlin.py`):
    /// `DUMP_DIR=<root> DUMP_LIST=<files> DUMP_OUT=<tsv> cargo test ... -- --ignored`.
    #[test]
    #[ignore]
    fn zz_dump_kotlin_symbols() {
        use std::{fs, path::PathBuf, time::Instant};
        let root = PathBuf::from(std::env::var("DUMP_DIR").unwrap());
        let out = std::env::var("DUMP_OUT").unwrap();
        let list = fs::read_to_string(std::env::var("DUMP_LIST").unwrap()).unwrap();
        let pool = ParserPool::new();
        let mut lines = String::new();
        let (mut files, mut bytes, mut errs) = (0usize, 0usize, 0usize);
        let t = Instant::now();
        for rel in list.lines() {
            let Ok(src) = fs::read_to_string(root.join(rel)) else {
                continue;
            };
            files += 1;
            bytes += src.len();
            match pool.parse(Language::Kotlin, &RelPath::new(rel), &src) {
                Ok(p) => {
                    for s in p.symbols {
                        lines.push_str(&format!("{rel}\t{}\t{}\n", s.name, s.start_line));
                    }
                }
                Err(_) => errs += 1,
            }
        }
        let el = t.elapsed();
        eprintln!(
            "DUMP files={files} bytes={bytes} errs={errs} elapsed_ms={} MBps={:.1}",
            el.as_millis(),
            bytes as f64 / 1e6 / el.as_secs_f64()
        );
        fs::write(out, lines).unwrap();
    }
}
