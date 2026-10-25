//! Lua (`.lua`) extraction hooks (lang-batch-8).
//!
//! Owned by the lua worker; see `LANG-PLAN.md`. `parse/mod.rs` calls these
//! hooks for [`Language::Lua`] only, so all language-specific post-processing
//! lives here and never in shared code.
//!
//! The symbols query reports every chunk-level binding (`local a, b = …`,
//! `X = …`, `M.x = …`) as `@definition.variable`, because a query cannot pair
//! the n-th target with the n-th value. [`symbol_name`] does the pairing:
//! a target whose value is a `function_definition` becomes a function, and a
//! dotted target (`M.x = 1`, a module-table member) becomes a field.

use crate::types::SymbolKind;

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name_node: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    if kind != SymbolKind::Variable {
        return (name.to_owned(), kind);
    }
    let assignment = match definition.kind() {
        "assignment_statement" => Some(definition),
        "variable_declaration" => named_child_of_kind(definition, "assignment_statement"),
        _ => None,
    };
    let Some(assignment) = assignment else {
        return (name.to_owned(), kind); // `local a` without a value
    };
    let Some(targets) = named_child_of_kind(assignment, "variable_list") else {
        return (name.to_owned(), kind);
    };
    let mut cursor = targets.walk();
    let all: Vec<_> = targets
        .children_by_field_name("name", &mut cursor)
        .enumerate()
        .filter(|(_, target)| last_segment(*target, source) == Some(name))
        .collect();
    // Same name on several targets (`a.x, b.x = 1, function…`): pick the
    // target the matched `@name` node belongs to.
    let found = all
        .iter()
        .find(|(_, t)| {
            last_segment_node(*t).is_some_and(|n| n.byte_range() == name_node.byte_range())
        })
        .or(all.first())
        .copied();
    let Some((index, target)) = found else {
        return (name.to_owned(), kind);
    };
    let value = named_child_of_kind(assignment, "expression_list").and_then(|values| {
        let mut cursor = values.walk();
        let value = values
            .children_by_field_name("value", &mut cursor)
            .nth(index);
        value
    });
    let kind = if value.is_some_and(|v| v.kind() == "function_definition") {
        SymbolKind::Function
    } else if matches!(
        target.kind(),
        "dot_index_expression" | "bracket_index_expression"
    ) {
        SymbolKind::Field
    } else {
        kind
    };
    (name.to_owned(), kind)
}

/// Whether a definition is visible outside its file.
///
/// Lua has no export keyword: a module exposes what it returns or assigns to
/// globals. `local` bindings (and `local function`) are file-private unless
/// they are later returned, which a syntactic pass cannot see cheaply, so
/// only `local` declarations at chunk level are reported as not exported.
/// Members (`M.f`, `M:f`, table fields) and globals are exported.
pub(super) fn exported(name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    let is_local = match node.kind() {
        "variable_declaration" => starts_with_keyword(node, source, "local"),
        "function_declaration" => {
            starts_with_keyword(node, source, "local")
                || node
                    .parent()
                    .is_some_and(|p| p.kind() == "variable_declaration")
        }
        _ => false,
    };
    !is_local || returned_at_chunk_end(name, node, source)
}

/// `local M = {} … return M`: the returned binding is the module's export.
fn returned_at_chunk_end(name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    let decl = if node.kind() == "function_declaration" {
        node.parent().unwrap_or(node)
    } else {
        node
    };
    let Some(chunk) = decl.parent().filter(|p| p.kind() == "chunk") else {
        return false;
    };
    let mut c = chunk.walk();
    let found = chunk
        .named_children(&mut c)
        .filter(|n| n.kind() == "return_statement")
        .any(|ret| {
            let mut c2 = ret.walk();
            let found = ret.named_children(&mut c2).any(|list| {
                let mut c3 = list.walk();
                let found = list.named_children(&mut c3).any(|e| {
                    e.kind() == "identifier" && e.utf8_text(source.as_bytes()) == Ok(name)
                });
                found
            });
            found
        });
    found
}

fn starts_with_keyword(node: tree_sitter::Node<'_>, source: &str, keyword: &str) -> bool {
    let text = node.utf8_text(source.as_bytes()).unwrap_or_default();
    text.strip_prefix(keyword)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace()))
}

fn named_child_of_kind<'t>(
    node: tree_sitter::Node<'t>,
    kind: &str,
) -> Option<tree_sitter::Node<'t>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn last_segment_node<'t>(target: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    match target.kind() {
        "identifier" => Some(target),
        "dot_index_expression" => target.child_by_field_name("field"),
        // `M['key']`: the string content of a literal key.
        "bracket_index_expression" => {
            let key = target.child_by_field_name("field")?;
            if key.kind() == "string" {
                key.child_by_field_name("content")
            } else {
                None
            }
        }
        _ => None,
    }
}

fn last_segment<'s>(target: tree_sitter::Node<'_>, source: &'s str) -> Option<&'s str> {
    last_segment_node(target)?.utf8_text(source.as_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath, SymbolKind};

    fn parse(source: &str) -> crate::parse::ParsedFile {
        ParserPool::new()
            .parse(Language::Lua, &RelPath::new("m.lua"), source)
            .expect("lua parses")
    }

    fn kinds(source: &str) -> Vec<(String, SymbolKind, u32, bool)> {
        let mut out: Vec<_> = parse(source)
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.start_line, s.exported))
            .collect();
        out.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
        out
    }

    const MODULE: &str = include_str!("../../tests/fixtures/lua/module.lua");

    #[test]
    fn module_fixture_extracts_every_declaration() {
        use SymbolKind::*;
        let got = kinds(MODULE);
        let want: Vec<(&str, SymbolKind, u32, bool)> = vec![
            ("M", Variable, 3, false),
            ("a", Variable, 4, false),
            ("b", Variable, 4, false),
            ("lazy", Variable, 5, false),
            ("VERSION", Variable, 6, true),
            ("name", Field, 7, true),
            ("handler", Function, 8, true),
            ("foo", Function, 10, true),
            ("bar", Method, 12, true),
            ("baz", Function, 14, false),
            ("glob", Function, 16, true),
            ("cfg", Variable, 18, false),
            ("timeout", Field, 19, true),
            ("on_attach", Function, 20, true),
            ("inner_attach", Function, 21, true),
            ("nested", Field, 21, true),
            ("outer", Function, 25, false),
            ("helper", Function, 26, false),
            ("cb", Function, 27, true),
            ("deps", Function, 31, false),
            ("filetypes", Field, 41, true),
            ("cmd", Field, 42, true),
            ("setup", Function, 43, true),
        ];
        let want: Vec<_> = want
            .into_iter()
            .map(|(n, k, l, e)| (n.to_owned(), k, l, e))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn imports_cover_every_require_form() {
        let parsed = parse(MODULE);
        assert_eq!(
            parsed.imports,
            vec!["util.strings", "util.tables", "a.b", "lib.long", "conf.lua"]
        );
    }

    #[test]
    fn calls_name_the_last_segment() {
        // The shared extractor emits the `@reference.call` text and then the
        // `@name` text per call; keep the names.
        let names: Vec<String> = parse("f()\na.b.g(1)\nobj:h()\ni 'x'\nj { }\n")
            .calls
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| n.chars().all(|c| c.is_alphanumeric() || c == '_'))
            .collect();
        assert_eq!(names, vec!["f", "g", "h", "i", "j"]);
    }

    #[test]
    fn bracket_keys_return_export_and_same_name_targets() {
        use SymbolKind::*;
        let got = kinds(
            "local M = {}\nlocal helper = 1\nM['c'] = function() end\nM[\"d\"] = 5\na.x, b.x = 1, function() end\nreturn M\n",
        );
        let find = |n: &str, l: u32| {
            got.iter()
                .find(|g| g.0 == n && g.2 == l)
                .unwrap_or_else(|| panic!("missing {n}@{l}: {got:?}"))
                .clone()
        };
        assert_eq!(find("c", 3).1, Function);
        assert_eq!(find("d", 4).1, Field);
        assert!(find("M", 1).3, "returned M is exported");
        assert!(!find("helper", 2).3);
        let xs: Vec<_> = got.iter().filter(|g| g.0 == "x").collect();
        assert_eq!(xs.len(), 2, "{got:?}");
        assert!(xs.iter().any(|g| g.1 == Function), "{xs:?}");
        assert!(xs.iter().any(|g| g.1 == Field), "{xs:?}");
    }

    #[test]
    fn nested_plain_locals_are_not_symbols() {
        let got = kinds("local function f()\n  local x = 1\n  y = 2\nend\n");
        assert_eq!(got.len(), 1, "{got:?}");
    }

    /// Corpus dump for the recall bench (`gt_lua.py`):
    /// `ASTRO_LUA_DUMP=<root> ASTRO_LUA_OUT=<tsv> cargo test -p astrolabe-core
    /// --lib parse::lua::tests::dump_corpus -- --ignored`.
    #[test]
    #[ignore]
    fn dump_corpus() {
        use std::io::Write;
        let root = std::path::PathBuf::from(std::env::var("ASTRO_LUA_DUMP").unwrap());
        let out = std::env::var("ASTRO_LUA_OUT").unwrap();
        let mut files = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "lua") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let pool = ParserPool::new();
        let mut w = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
        let (mut bytes, mut symbols) = (0usize, 0usize);
        let start = std::time::Instant::now();
        for path in &files {
            let source = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
            bytes += source.len();
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let parsed = pool
                .parse(Language::Lua, &RelPath::new(&rel), &source)
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
            for s in parsed.symbols {
                symbols += 1;
                writeln!(w, "{rel}\t{}\t{}\t{:?}", s.start_line, s.name, s.kind).unwrap();
            }
        }
        eprintln!(
            "files={} bytes={} symbols={} parse_ms={}",
            files.len(),
            bytes,
            symbols,
            start.elapsed().as_millis()
        );
    }
}
