//! Swift (`.swift`) extraction hooks (lang-batch-8).
//!
//! `parse/mod.rs` calls these hooks for [`Language::Swift`] only, so all
//! Swift-specific post-processing lives here.
//!
//! `queries/swift/symbols.scm` emits one coarse kind per syntax node; this file
//! refines it by context:
//!
//! * `func` directly inside a type / extension body → [`SymbolKind::Method`];
//!   at file scope or nested in a body it stays a [`SymbolKind::Function`].
//! * stored / computed properties in a type body → [`SymbolKind::Field`],
//!   except `static let` / `class let`, which are [`SymbolKind::Const`].
//! * file-scope `let` → [`SymbolKind::Const`], file-scope `var` →
//!   [`SymbolKind::Variable`].
//!
//! Visibility: a definition is exported (visible outside its file) unless it,
//! or any enclosing type / extension, is `private` or `fileprivate`, or it is
//! local to a function, closure or accessor body. `private(set)` restricts
//! only the setter and does not hide the symbol.

use crate::types::SymbolKind;

/// Bodies whose direct children are type members.
const MEMBER_BODIES: &[&str] = &["class_body", "enum_class_body", "protocol_body"];

/// Bodies that make anything inside them local.
const LOCAL_SCOPES: &[&str] = &[
    "function_body",
    "lambda_literal",
    "computed_property",
    "computed_getter",
    "computed_setter",
    "willset_didset_block",
    "statements",
];

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    let parent_kind = definition.parent().map(|p| p.kind());
    let kind = match definition.kind() {
        "function_declaration" if parent_kind.is_some_and(|k| MEMBER_BODIES.contains(&k)) => {
            SymbolKind::Method
        }
        "property_declaration" => {
            let is_let = binding_is_let(definition, source);
            if parent_kind == Some("source_file") {
                if is_let {
                    SymbolKind::Const
                } else {
                    SymbolKind::Variable
                }
            } else if is_let && has_modifier(definition, source, &["static", "class"]) {
                SymbolKind::Const
            } else {
                SymbolKind::Field
            }
        }
        _ => kind,
    };
    (name.to_owned(), kind)
}

/// Whether a definition is visible outside its file.
pub(super) fn exported(_name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.id() != node.id() && LOCAL_SCOPES.contains(&n.kind()) {
            return false;
        }
        if matches!(
            n.kind(),
            "class_declaration"
                | "protocol_declaration"
                | "function_declaration"
                | "property_declaration"
                | "protocol_property_declaration"
                | "protocol_function_declaration"
                | "init_declaration"
                | "deinit_declaration"
                | "subscript_declaration"
                | "typealias_declaration"
                | "associatedtype_declaration"
                | "macro_declaration"
                | "enum_entry"
        ) && is_file_private(n, source)
        {
            return false;
        }
        cur = n.parent();
    }
    true
}

fn modifiers(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == "modifiers");
    found
}

fn modifier_texts<'s>(node: tree_sitter::Node<'_>, source: &'s str) -> Vec<&'s str> {
    let Some(mods) = modifiers(node) else {
        return Vec::new();
    };
    let mut cursor = mods.walk();
    mods.children(&mut cursor)
        .filter_map(|c| c.utf8_text(source.as_bytes()).ok())
        .map(str::trim)
        .collect()
}

fn has_modifier(node: tree_sitter::Node<'_>, source: &str, wanted: &[&str]) -> bool {
    modifier_texts(node, source)
        .iter()
        .any(|t| wanted.contains(t))
}

fn is_file_private(node: tree_sitter::Node<'_>, source: &str) -> bool {
    has_modifier(node, source, &["private", "fileprivate"])
}

fn binding_is_let(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|c| {
        c.kind() == "value_binding_pattern"
            && c.utf8_text(source.as_bytes())
                .is_ok_and(|t| t.trim_start().starts_with("let"))
    });
    found
}

#[cfg(test)]
mod tests {
    use crate::parse::ParserPool as Parser;
    use crate::types::{Language, RelPath, SymbolKind};

    fn parse(src: &str) -> Vec<(String, SymbolKind, bool)> {
        let parser = Parser::new();
        let parsed = parser
            .parse(Language::Swift, &RelPath::new("t.swift"), src)
            .expect("parse");
        parsed
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.exported))
            .collect()
    }

    fn find(syms: &[(String, SymbolKind, bool)], name: &str) -> (SymbolKind, bool) {
        let s = syms
            .iter()
            .find(|s| s.0 == name)
            .unwrap_or_else(|| panic!("missing {name}: {syms:?}"));
        (s.1, s.2)
    }

    const SRC: &str = include_str!("../../tests/fixtures/swift/Sources/Kit/Kit.swift");

    #[test]
    fn swift_declaration_kinds() {
        let syms = parse(SRC);
        use SymbolKind::*;
        for (name, kind) in [
            ("Session", Class),
            ("Worker", Class),
            ("Point", Struct),
            ("Method", Enum),
            ("get", Const),
            ("post", Const),
            ("custom", Const),
            ("Requestable", Interface),
            ("Handler", Type),
            ("Element", Type),
            ("request", Method),
            ("perform", Method),
            ("ext", Method),
            ("helper", Function),
            ("inner", Function),
            ("x", Field),
            ("y", Field),
            ("label", Field),
            ("defaultPort", Const),
            ("version", Const),
            ("counter", Variable),
            ("init", Method),
            ("deinit", Method),
            ("subscript", Method),
            ("==", Method),
            ("<~>", Method),
            ("stringify", Function),
            ("url", Field),
            ("first", Field),
            ("second", Field),
        ] {
            assert_eq!(find(&syms, name).0, kind, "{name}");
        }
        // Locals are not symbols.
        assert!(!syms.iter().any(|s| s.0 == "local"), "{syms:?}");
        // The extension itself is not a definition of `Point`.
        assert_eq!(syms.iter().filter(|s| s.0 == "Point").count(), 1);
    }

    #[test]
    fn swift_visibility() {
        let syms = parse(SRC);
        assert!(find(&syms, "Session").1);
        assert!(find(&syms, "request").1);
        assert!(!find(&syms, "secret").1, "private member");
        assert!(!find(&syms, "fileHelper").1, "fileprivate function");
        assert!(!find(&syms, "hiddenExt").1, "member of private extension");
        assert!(!find(&syms, "inner").1, "local function");
        assert!(find(&syms, "label").1, "private(set) only hides the setter");
    }

    #[test]
    fn swift_reparse_after_edit_sees_new_member() {
        let edited = SRC.replace("func perform()", "func performNow() {}\n    func perform()");
        let syms = parse(&edited);
        assert_eq!(find(&syms, "performNow").0, SymbolKind::Method);
    }

    #[test]
    fn swift_imports_and_calls() {
        let parser = Parser::new();
        let parsed = parser
            .parse(Language::Swift, &RelPath::new("t.swift"), SRC)
            .unwrap();
        assert_eq!(parsed.imports, vec!["Foundation", "Util", "Darwin.C"]);
        let calls: Vec<&str> = parsed.calls.iter().map(|c| c.0.as_str()).collect();
        for want in ["helper", "perform", "Point", "append"] {
            assert!(calls.contains(&want), "missing call {want}: {calls:?}");
        }
    }

    /// Corpus dump for the recall bench: `ASTROLABE_SWIFT_DUMP=<root>` writes
    /// `file\tname\tkind\tline` for every `.swift` file to stdout.
    #[test]
    #[ignore]
    fn swift_dump_corpus_symbols() {
        let Ok(root) = std::env::var("ASTROLABE_SWIFT_DUMP") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let mut stack = vec![root.clone()];
        let mut files = Vec::new();
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if !p.file_name().unwrap().to_string_lossy().starts_with('.') {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "swift") {
                    files.push(p);
                }
            }
        }
        files.sort();
        let parser = Parser::new();
        let start = std::time::Instant::now();
        let mut out = String::new();
        let mut bytes = 0usize;
        for f in &files {
            let rel = f.strip_prefix(&root).unwrap().to_string_lossy().to_string();
            let src = std::fs::read_to_string(f).unwrap_or_default();
            bytes += src.len();
            let parsed = parser
                .parse(Language::Swift, &RelPath::new(&rel), &src)
                .unwrap();
            for s in parsed.symbols {
                out.push_str(&format!(
                    "{rel}\t{}\t{:?}\t{}\n",
                    s.name, s.kind, s.start_line
                ));
            }
        }
        let el = start.elapsed();
        eprintln!(
            "PARSE files={} bytes={} elapsed_ms={}",
            files.len(),
            bytes,
            el.as_millis()
        );
        let dst = std::env::var("ASTROLABE_SWIFT_DUMP_OUT").unwrap();
        std::fs::write(dst, out).unwrap();
    }
}
