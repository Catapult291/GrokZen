//! Decorator lines belong to the symbol they decorate.
//!
//! Python wraps `@dec` lines and the `def` / `class` in a `decorated_definition`
//! node, but the symbols query captures the inner definition, so the decorator
//! lines fell outside `start_line..end_line` (body extraction lost `@property`,
//! `@staticmethod`, `@app.route(..)`). TS / JS / Java / Kotlin / C# keep
//! decorators or annotations inside the declaration node already; Java / C# /
//! VB / Swift deliberately anchor on the name line instead (see `mod.rs`).

/// Start row (0-based) of the decorators attached to `definition`, if any.
pub(super) fn start_row(definition: tree_sitter::Node<'_>) -> Option<usize> {
    let parent = definition.parent()?;
    if parent.kind() == "decorated_definition" {
        return Some(parent.start_position().row);
    }
    // Decorators emitted as preceding siblings (some grammars).
    let mut row = None;
    let mut prev = definition.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "decorator" {
            break;
        }
        row = Some(p.start_position().row);
        prev = p.prev_named_sibling();
    }
    row
}

#[cfg(test)]
mod tests {
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath, SymbolKind};

    fn syms(lang: Language, path: &str, src: &str) -> Vec<(String, SymbolKind, u32, u32)> {
        ParserPool::new()
            .parse(lang, &RelPath::new(path), src)
            .unwrap()
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.start_line, s.end_line))
            .collect()
    }

    fn get<'a>(
        v: &'a [(String, SymbolKind, u32, u32)],
        name: &str,
    ) -> &'a (String, SymbolKind, u32, u32) {
        v.iter()
            .find(|s| s.0 == name)
            .unwrap_or_else(|| panic!("missing {name}: {v:?}"))
    }

    #[test]
    fn python_decorators_join_the_symbol_range() {
        let s = syms(
            Language::Python,
            "a.py",
            "class A:\n    @property\n    @cached\n    def f(self):\n        return 1\n\n    def g(self):\n        pass\n",
        );
        assert_eq!(get(&s, "f").2, 2, "{s:?}");
        assert_eq!(get(&s, "g").2, 7, "{s:?}");
    }

    #[test]
    fn c_and_cpp_macros_and_struct_members() {
        for (lang, path) in [(Language::C, "m.c"), (Language::Cpp, "m.cpp")] {
            let s = syms(
                lang,
                path,
                "#ifndef GUARD_H\n#define GUARD_H\n#define M(x) ((x)+1)\n#define K 3\n#endif\n",
            );
            assert!(s.iter().any(|x| x.0 == "M"), "{path} {s:?}");
            assert!(s.iter().any(|x| x.0 == "K"), "{path} {s:?}");
            assert!(!s.iter().any(|x| x.0 == "GUARD_H"), "{path} {s:?}");
        }
        let s = syms(
            Language::C,
            "t.c",
            "typedef struct {\n  int b;\n  char *name;\n  int arr[4];\n} T;\nstruct N { long c; };\n",
        );
        for n in ["T", "b", "name", "arr", "c"] {
            assert!(s.iter().any(|x| x.0 == n), "missing {n}: {s:?}");
        }
        assert_eq!(get(&s, "b").1, SymbolKind::Field);
    }

    #[test]
    fn ts_ctor_param_properties_and_java_record_components() {
        let s = syms(
            Language::TypeScript,
            "a.ts",
            "class A {\n  constructor(private x: number, readonly y: string, z: number, public w?: number) {}\n}\n",
        );
        for n in ["x", "y", "w"] {
            assert_eq!(get(&s, n).1, SymbolKind::Field, "{n}: {s:?}");
        }
        assert!(!s.iter().any(|x| x.0 == "z"), "{s:?}");
        let j = syms(
            Language::Java,
            "R.java",
            "record R(int a, String b) {\n  void m() {}\n}\n",
        );
        assert_eq!(get(&j, "a").1, SymbolKind::Field, "{j:?}");
        assert!(j.iter().any(|x| x.0 == "b"), "{j:?}");
    }

    #[test]
    fn dart_methods_and_kotlin_one_line_members_are_methods() {
        let d = syms(
            Language::Dart,
            "a.dart",
            "class A {\n  void m() {}\n  int get g => 1;\n}\nvoid top() {}\n",
        );
        assert_eq!(get(&d, "m").1, SymbolKind::Method, "{d:?}");
        assert_eq!(get(&d, "top").1, SymbolKind::Function, "{d:?}");
        let k = syms(
            Language::Kotlin,
            "a.kt",
            "class A(val x: Int) { fun m() {} }\nfun top() {}\n",
        );
        assert_eq!(get(&k, "m").1, SymbolKind::Method, "{k:?}");
        assert_eq!(get(&k, "top").1, SymbolKind::Function, "{k:?}");
    }
}
