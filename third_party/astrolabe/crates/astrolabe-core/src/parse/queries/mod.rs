//! Tree-sitter queries for symbol, import, and call extraction.
//!
//! One `.scm` file per language per concern, embedded with `include_str!` so
//! the binary stays self-contained. Validate every query compiles against its
//! grammar in a unit test — a query that references a node type the grammar
//! version lacks fails at runtime otherwise, and the prior art degraded to
//! "extract nothing" when that happened.
//!
//! # Capture conventions
//!
//! Follow upstream `tags.scm` where it exists. Capture names starting with
//! `_` (e.g. `@_scope`, `@_mods`, `@_fn`) are pattern-internal and must be
//! ignored by consumers.
//!
//! **symbols** — one match per definition:
//! - [`NAME`] (`@name`): the identifier node; its text is the symbol name.
//! - `@definition.<kind>`: the whole defining node (use its range for
//!   `start_line` / `end_line` and its first line for `signature`). Map the
//!   capture name with [`symbol_kind_for_capture`]. Kinds emitted:
//!   `function`, `method`, `class`, `interface`, `struct`, `enum`, `trait`,
//!   `type`, `const`, `module`, `macro` (Rust `macro_rules!`, mapped to
//!   [`SymbolKind::Function`]), `field` (type members), `variable`
//!   (non-const module bindings).
//!
//!   Patterns are written so a node never carries two `@definition.*`
//!   captures (e.g. a Python method is not also reported as a function), so
//!   consumers do not need to dedupe.
//!
//! **imports** — one match per import specifier:
//! - [`IMPORT`] (`@import`): the raw specifier text. Quotes are already
//!   stripped for Go / TS / JS (the capture is the string *content* node), and
//!   Python relative-import dots are preserved. Rust brace groups are handed
//!   over verbatim — see `rust/imports.scm`.
//! - Optional refinements on separate captures, so a consumer that only reads
//!   `@import` is complete: Python `@import.module` + `@import.name` pairs
//!   (for `from X import a` submodule probing), Java `@import.static` /
//!   `@import.wildcard`, Rust `@import.mod` (`mod foo;` file links).
//!
//! **calls** — one match per call site:
//! - [`NAME`] (`@name`): the callee's final identifier (`a.b.c()` → `c`).
//! - [`CALL`] (`@reference.call`): the call node; its start row is the line.
//!   Name-based only, hence `Confidence::Syntactic` downstream.
//!
//! Quality bar: symbol recall against the language server's document symbols
//! must be >=90% per language. The prior art extracted 4962 symbols from a
//! repo with 6575 Python defs alone.

use crate::types::{Language, SymbolKind};

/// Capture name for the identifier of a definition or callee.
pub const NAME: &str = "name";
/// Capture name for an import specifier.
pub const IMPORT: &str = "import";
/// Capture name for a call-site node.
pub const CALL: &str = "reference.call";
/// Prefix shared by every symbol-definition capture.
pub const DEFINITION_PREFIX: &str = "definition.";

/// Query sources for one language.
pub struct LanguageQueries {
    pub symbols: &'static str,
    pub imports: &'static str,
    pub calls: &'static str,
}

macro_rules! queries {
    ($dir:literal) => {
        LanguageQueries {
            symbols: include_str!(concat!($dir, "/symbols.scm")),
            imports: include_str!(concat!($dir, "/imports.scm")),
            calls: include_str!(concat!($dir, "/calls.scm")),
        }
    };
}

/// Embedded `.scm` sources for `lang`. TypeScript and TSX share one set: the
/// two grammars expose identical node types for everything the queries touch.
pub fn for_language(lang: Language) -> Option<LanguageQueries> {
    Some(match lang {
        Language::Python => queries!("python"),
        Language::Go => queries!("go"),
        Language::Java => queries!("java"),
        Language::Rust => queries!("rust"),
        Language::TypeScript | Language::Tsx => queries!("typescript"),
        Language::JavaScript => queries!("javascript"),
        Language::Php => queries!("php"),
        Language::Dart => queries!("dart"),
        Language::CSharp => queries!("csharp"),
        Language::VisualBasic => queries!("vb"),
        Language::C => queries!("c"),
        Language::Cpp => queries!("cpp"),
        // lang-batch-8: ObjC++ shares the ObjC grammar and queries.
        Language::ObjC | Language::ObjCpp => queries!("objc"),
        Language::Swift => queries!("swift"),
        Language::Kotlin => queries!("kotlin"),
        Language::Shell => queries!("bash"),
        Language::Ruby => queries!("ruby"),
        Language::Lua => queries!("lua"),
        // Vue SFCs are peeled in `parse::vue` and parsed with JS/TS queries.
        Language::Vue => return None,
    })
}

/// Map a `@definition.<kind>` capture name to a [`SymbolKind`]. Returns `None`
/// for anything that is not a definition capture (`name`, `_scope`, ...).
/// TypeScript object-type members are definitions only inside a `type X = …`
/// alias (interfaces use `interface_body`, not `object_type`). Members of
/// inline object types — parameter annotations, `as { x }` casts, generic
/// arguments — are dropped by the symbol extractor via this predicate.
pub(crate) fn ts_inline_object_member(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() != "property_signature" || node.parent().map(|p| p.kind()) != Some("object_type")
    {
        return false;
    }
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        if parent.kind() == "type_alias_declaration" {
            return false;
        }
        cur = parent;
    }
    true
}

pub fn symbol_kind_for_capture(capture: &str) -> Option<SymbolKind> {
    Some(match capture.strip_prefix(DEFINITION_PREFIX)? {
        "function" | "macro" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::Type,
        "const" | "constant" => SymbolKind::Const,
        "module" => SymbolKind::Module,
        "field" => SymbolKind::Field,
        "variable" => SymbolKind::Variable,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

    const ALL: [Language; 20] = [
        Language::Python,
        Language::Go,
        Language::Java,
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Php,
        Language::C,
        Language::Cpp,
        Language::Dart,
        Language::CSharp,
        Language::VisualBasic,
        Language::ObjC,
        Language::ObjCpp,
        Language::Swift,
        Language::Kotlin,
        Language::Shell,
        Language::Ruby,
        Language::Lua,
    ];

    fn grammar(lang: Language) -> tree_sitter::Language {
        match lang {
            Language::Python => tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
            Language::Go => tree_sitter::Language::new(tree_sitter_go::LANGUAGE),
            Language::Java => tree_sitter::Language::new(tree_sitter_java::LANGUAGE),
            Language::Rust => tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
            Language::TypeScript => {
                tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
            }
            Language::Tsx => tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX),
            Language::JavaScript => tree_sitter::Language::new(tree_sitter_javascript::LANGUAGE),
            Language::Php => tree_sitter::Language::new(tree_sitter_php::LANGUAGE_PHP),
            Language::C => tree_sitter::Language::new(tree_sitter_c::LANGUAGE),
            Language::Cpp => tree_sitter::Language::new(tree_sitter_cpp::LANGUAGE),
            Language::Dart => tree_sitter::Language::new(tree_sitter_dart::LANGUAGE),
            Language::CSharp => tree_sitter::Language::new(tree_sitter_c_sharp::LANGUAGE),
            Language::VisualBasic => tree_sitter::Language::new(tree_sitter_vb_dotnet::LANGUAGE),
            Language::ObjC | Language::ObjCpp => {
                tree_sitter::Language::new(tree_sitter_objc::LANGUAGE)
            }
            Language::Swift => tree_sitter::Language::new(tree_sitter_swift::LANGUAGE),
            Language::Kotlin => tree_sitter::Language::new(tree_sitter_kotlin_ng::LANGUAGE),
            Language::Shell => tree_sitter::Language::new(tree_sitter_bash::LANGUAGE),
            Language::Ruby => tree_sitter::Language::new(tree_sitter_ruby::LANGUAGE),
            Language::Lua => tree_sitter::Language::new(tree_sitter_lua::LANGUAGE),
            Language::Vue => {
                panic!("{lang:?} grammar not registered in query tests yet")
            }
        }
    }

    /// lang-batch-8 placeholder `.scm` (comment-only). Workers delete the
    /// `placeholder` marker line when real patterns land; the capture checks
    /// then apply in full. Compilation is still checked for placeholders.
    fn is_placeholder(src: &str) -> bool {
        src.lines()
            .next()
            .is_some_and(|l| l.contains("placeholder (lang-batch-8)"))
    }

    fn compile(lang: Language, src: &str, what: &str) -> Query {
        Query::new(&grammar(lang), src)
            .unwrap_or_else(|e| panic!("{lang:?} {what} query failed to compile: {e}"))
    }

    /// One capture inside a match: its text plus the node's byte range, so
    /// tests can tell "same node reported twice" from "two distinct nodes with
    /// the same name" (a trait signature and its impl, for instance).
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Cap {
        text: String,
        range: (usize, usize),
    }

    /// Run `query_src` over `source`; return every match as a
    /// `capture name -> Cap` map (internal `_` captures dropped).
    fn run(
        lang: Language,
        query_src: &str,
        what: &str,
        source: &str,
    ) -> Vec<BTreeMap<String, Cap>> {
        let ts_lang = grammar(lang);
        let query = compile(lang, query_src, what);
        let mut parser = Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).expect("parse");
        assert!(
            !tree.root_node().has_error(),
            "{lang:?} sample source does not parse cleanly:\n{}",
            tree.root_node().to_sexp()
        );
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut out = Vec::new();
        while let Some(m) = matches.next() {
            if m.captures()
                .iter()
                .any(|c| super::ts_inline_object_member(c.node))
            {
                continue;
            }
            let mut row = BTreeMap::new();
            for c in m.captures() {
                let name = names[c.index as usize];
                if name.starts_with('_') {
                    continue;
                }
                let text = c.node.utf8_text(source.as_bytes()).unwrap().to_string();
                row.insert(
                    name.to_string(),
                    Cap {
                        text,
                        range: (c.node.start_byte(), c.node.end_byte()),
                    },
                );
            }
            out.push(row);
        }
        out
    }

    /// Text of the `capture` in each match that has it, in match order.
    fn texts(rows: Vec<BTreeMap<String, Cap>>, capture: &str) -> Vec<String> {
        rows.into_iter()
            .filter_map(|m| m.get(capture).map(|c| c.text.clone()))
            .collect()
    }

    /// `(kind, name, definition-node range)` from the symbols query, asserting
    /// each match has exactly one `@definition.*` capture plus a `@name`.
    fn symbols(lang: Language, source: &str) -> Vec<(String, String, (usize, usize))> {
        let q = for_language(lang).unwrap();
        let mut out = Vec::new();
        for m in run(lang, q.symbols, "symbols", source) {
            let defs: Vec<&String> = m
                .keys()
                .filter(|k| k.starts_with(DEFINITION_PREFIX))
                .collect();
            assert_eq!(
                defs.len(),
                1,
                "{lang:?}: match must carry exactly one definition capture: {m:?}"
            );
            let kind = defs[0].strip_prefix(DEFINITION_PREFIX).unwrap().to_string();
            let name = m
                .get(NAME)
                .unwrap_or_else(|| panic!("{lang:?}: missing @name in {m:?}"))
                .text
                .clone();
            out.push((kind, name, m[defs[0]].range));
        }
        out
    }

    fn assert_symbols(lang: Language, source: &str, expected: &[(&str, &str)]) {
        let got = symbols(lang, source);
        // No (name, definition node) may be reported twice (e.g. as method
        // *and* function). Keyed on the name too because a multi-declarator
        // `static final int A = 1, B = 2;` legitimately reports two names
        // against one declaration node.
        let nodes: BTreeSet<(&str, (usize, usize))> =
            got.iter().map(|(_, n, r)| (n.as_str(), *r)).collect();
        assert_eq!(
            got.len(),
            nodes.len(),
            "{lang:?}: the same definition node was reported more than once: {got:?}"
        );
        let got_set: BTreeSet<(String, String)> =
            got.iter().map(|(k, n, _)| (k.clone(), n.clone())).collect();
        let want: BTreeSet<(String, String)> = expected
            .iter()
            .map(|(k, n)| (k.to_string(), n.to_string()))
            .collect();
        let missing: Vec<_> = want.difference(&got_set).collect();
        let extra: Vec<_> = got_set.difference(&want).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{lang:?} symbols differ.\n  missing: {missing:?}\n  extra:   {extra:?}"
        );
    }

    fn imports(lang: Language, source: &str) -> Vec<String> {
        let q = for_language(lang).unwrap();
        texts(run(lang, q.imports, "imports", source), IMPORT)
    }

    fn calls(lang: Language, source: &str) -> Vec<String> {
        let q = for_language(lang).unwrap();
        run(lang, q.calls, "calls", source)
            .into_iter()
            .map(|m| {
                assert!(
                    m.contains_key(CALL),
                    "{lang:?}: call match lacks @reference.call: {m:?}"
                );
                m.get(NAME)
                    .unwrap_or_else(|| panic!("{lang:?}: call match lacks @name: {m:?}"))
                    .text
                    .clone()
            })
            .collect()
    }

    // ------------------------------------------------------------ compile

    #[test]
    fn every_query_compiles_against_its_grammar() {
        for lang in ALL {
            let q = for_language(lang).unwrap_or_else(|| panic!("no queries for {lang:?}"));
            compile(lang, q.symbols, "symbols");
            compile(lang, q.imports, "imports");
            compile(lang, q.calls, "calls");
        }
    }

    #[test]
    fn every_definition_capture_maps_to_a_symbol_kind() {
        for lang in ALL {
            let q = for_language(lang).unwrap();
            if is_placeholder(q.symbols) {
                continue; // lang-batch-8 worker has not landed queries yet.
            }
            let query = compile(lang, q.symbols, "symbols");
            let mut saw_name = false;
            for cap in query.capture_names() {
                if *cap == NAME {
                    saw_name = true;
                } else if cap.starts_with(DEFINITION_PREFIX) {
                    assert!(
                        symbol_kind_for_capture(cap).is_some(),
                        "{lang:?}: capture @{cap} has no SymbolKind mapping"
                    );
                } else {
                    assert!(
                        cap.starts_with('_'),
                        "{lang:?}: unexpected capture @{cap} in symbols query"
                    );
                }
            }
            assert!(saw_name, "{lang:?}: symbols query never captures @name");
        }
    }

    #[test]
    fn import_and_call_queries_use_the_documented_captures() {
        for lang in ALL {
            let q = for_language(lang).unwrap();
            if is_placeholder(q.symbols) {
                continue; // lang-batch-8 worker has not landed queries yet.
            }
            let imports = compile(lang, q.imports, "imports");
            assert!(
                imports.capture_names().contains(&IMPORT),
                "{lang:?}: no @import capture"
            );
            for cap in imports.capture_names() {
                assert!(
                    *cap == IMPORT || cap.starts_with("import.") || cap.starts_with('_'),
                    "{lang:?}: unexpected capture @{cap} in imports query"
                );
            }
            let calls = compile(lang, q.calls, "calls");
            if lang != Language::Dart {
                assert!(
                    calls.capture_names().contains(&NAME),
                    "{lang:?}: no @name capture in calls"
                );
                assert!(
                    calls.capture_names().contains(&CALL),
                    "{lang:?}: no @reference.call capture"
                );
            }
        }
    }

    #[test]
    fn symbol_kind_mapping() {
        assert_eq!(
            symbol_kind_for_capture("definition.function"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            symbol_kind_for_capture("definition.macro"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            symbol_kind_for_capture("definition.method"),
            Some(SymbolKind::Method)
        );
        assert_eq!(
            symbol_kind_for_capture("definition.struct"),
            Some(SymbolKind::Struct)
        );
        assert_eq!(
            symbol_kind_for_capture("definition.module"),
            Some(SymbolKind::Module)
        );
        assert_eq!(
            symbol_kind_for_capture("definition.field"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            symbol_kind_for_capture("definition.variable"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(symbol_kind_for_capture("name"), None);
        assert_eq!(symbol_kind_for_capture("_scope"), None);
        assert_eq!(symbol_kind_for_capture("definition.bogus"), None);
    }

    // ------------------------------------------------------------- python

    const PY: &str = r#"
import os
import a.b as ab, sys
from x.y import z, w as ww
from . import views
from ..pkg import mod
from .sib import *

MAX = 10
name: str = "x"

def top(a):
    def inner():
        pass
    return inner

async def atop():
    pass

@decorator
def decorated():
    pass

class Foo(Base):
    attr = 1

    def method(self):
        def helper():
            pass
        return helper

    @property
    def prop(self):
        return 1

    async def amethod(self):
        pass

    @staticmethod
    def smethod():
        pass

    class Inner:
        def inner_method(self):
            pass

if TYPE_CHECKING:
    def cond_fn():
        pass
else:
    def else_fn():
        pass

try:
    def try_fn():
        pass
except ImportError:
    def except_fn():
        pass

with ctx():
    def with_fn():
        pass

def uses():
    top(1)
    obj.method(2)
    a.b.c(3)
    print("x")
    Foo()
"#;

    #[test]
    fn python_symbols() {
        assert_symbols(
            Language::Python,
            PY,
            &[
                ("const", "MAX"),
                ("variable", "name"),
                ("function", "top"),
                ("function", "inner"),
                ("function", "atop"),
                ("function", "decorated"),
                ("function", "helper"),
                ("function", "cond_fn"),
                ("function", "else_fn"),
                ("function", "try_fn"),
                ("function", "except_fn"),
                ("function", "with_fn"),
                ("function", "uses"),
                ("class", "Foo"),
                ("class", "Inner"),
                ("field", "attr"),
                ("method", "method"),
                ("method", "prop"),
                ("method", "amethod"),
                ("method", "smethod"),
                ("method", "inner_method"),
            ],
        );
    }

    #[test]
    fn python_imports() {
        assert_eq!(
            imports(Language::Python, PY),
            ["os", "a.b", "sys", "x.y", ".", "..pkg", ".sib"]
        );
        // (module, name) pairs for submodule probing.
        let q = for_language(Language::Python).unwrap();
        let pairs: Vec<(String, String)> = run(Language::Python, q.imports, "imports", PY)
            .into_iter()
            .filter_map(|m| {
                Some((
                    m.get("import.module")?.text.clone(),
                    m.get("import.name")?.text.clone(),
                ))
            })
            .collect();
        let want: Vec<(String, String)> =
            [("x.y", "z"), ("x.y", "w"), (".", "views"), ("..pkg", "mod")]
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
        assert_eq!(pairs, want);
    }

    #[test]
    fn python_calls() {
        assert_eq!(
            calls(Language::Python, PY),
            ["ctx", "top", "method", "c", "print", "Foo"]
        );
    }

    #[test]
    fn python_class_fields_and_non_uppercase_bindings() {
        assert_symbols(
            Language::Python,
            r#"
MAX = 1
HTTP_OK = 200
count = 0
name: str = "x"

class Point:
    origin = 0
    x: int
    y: int = 1
"#,
            &[
                ("const", "MAX"),
                ("const", "HTTP_OK"),
                ("variable", "count"),
                ("variable", "name"),
                ("class", "Point"),
                ("field", "origin"),
                ("field", "x"),
                ("field", "y"),
            ],
        );
    }

    // ----------------------------------------------------------------- go

    const GO: &str = r#"
package main

import (
	"fmt"
	str "strings"
	_ "embed"
	`raw/path`
)
import "os"

const Max = 10
const (
	A = 1
	B = 2
)
var Global = 3
var (
	V1 int
	V2 = "x"
)

type Point struct{ X, Y int }
type Shape interface {
	Area() float64
	Perimeter() float64
}
type ID = string
type Handler func(int) error
type Meters float64
type List[T any] struct{ items []T }
type Ptr *Point

func New(x int) *Point { return &Point{X: x} }

func (p *Point) Move(dx int) {
	p.X += dx
	fmt.Println(p)
}

func (p Point) String() string { return str.Repeat("x", 2) }

func main() {
	p := New(1)
	p.Move(2)
	go helper()
	defer (fmt.Println)("done")
	const local = 5
	var lv = os.Args
	_ = lv
}
"#;

    #[test]
    fn go_symbols() {
        assert_symbols(
            Language::Go,
            GO,
            &[
                ("function", "New"),
                ("function", "main"),
                ("method", "Move"),
                ("method", "String"),
                ("method", "Area"),
                ("method", "Perimeter"),
                ("struct", "Point"),
                ("struct", "List"),
                ("interface", "Shape"),
                ("type", "ID"),
                ("type", "Handler"),
                ("type", "Meters"),
                ("type", "Ptr"),
                ("const", "Max"),
                ("const", "A"),
                ("const", "B"),
                ("const", "local"),
                ("variable", "Global"),
                ("variable", "V1"),
                ("variable", "V2"),
                ("field", "X"),
                ("field", "Y"),
                ("field", "items"),
            ],
        );
    }

    #[test]
    fn go_imports_are_unquoted() {
        assert_eq!(
            imports(Language::Go, GO),
            ["fmt", "strings", "embed", "raw/path", "os"]
        );
    }

    #[test]
    fn go_calls() {
        assert_eq!(
            calls(Language::Go, GO),
            ["Println", "Repeat", "New", "Move", "helper", "Println"]
        );
    }

    #[test]
    fn go_interface_embedded_types_are_fields() {
        assert_symbols(
            Language::Go,
            r#"
package p
type RW interface {
	io.Reader
	Closer
	Close() error
}
type Embed struct {
	*Point
	io.Writer
}
"#,
            &[
                ("interface", "RW"),
                ("struct", "Embed"),
                ("method", "Close"),
                ("field", "Reader"),
                ("field", "Closer"),
                ("field", "Point"),
                ("field", "Writer"),
            ],
        );
    }

    // --------------------------------------------------------------- java

    const JAVA: &str = r#"
package com.example;

import java.util.List;
import java.util.*;
import static java.lang.Math.max;
import static java.util.Collections.*;

public class Outer<T> extends Base implements Runnable {
    public static final int MAX = 10;
    private static final String NAME = "x", OTHER = "y";
    private int count;
    private final List<T> items = new ArrayList<>();

    public Outer() {
        super();
        this.count = max(1, 2);
    }

    @Override
    public void run() {
        helper();
        this.run();
        new Inner().go();
        new java.util.HashMap<String, Integer>();
        Runnable r = () -> System.out.println("x");
    }

    private static void helper() {}

    class Inner {
        void go() {}
    }

    static class Nested {}

    interface Callback {
        int CODE = 1;
        void call();
    }

    enum Color {
        RED, GREEN;
        Color() {}
        String label() { return name(); }
    }

    record Pair(int a, int b) {
        Pair {}
    }

    @interface Marker {}
}
"#;

    #[test]
    fn java_symbols() {
        assert_symbols(
            Language::Java,
            JAVA,
            &[
                ("class", "Outer"),
                ("class", "Inner"),
                ("class", "Nested"),
                ("class", "Pair"),
                ("interface", "Callback"),
                ("interface", "Marker"),
                ("enum", "Color"),
                ("method", "Outer"),
                ("method", "run"),
                ("method", "helper"),
                ("method", "go"),
                ("method", "call"),
                ("method", "Color"),
                ("method", "label"),
                ("method", "Pair"),
                ("const", "MAX"),
                ("const", "NAME"),
                ("const", "OTHER"),
                ("const", "CODE"),
                ("const", "RED"),
                ("const", "GREEN"),
                ("field", "a"),
                ("field", "b"),
                ("field", "count"),
                ("field", "items"),
            ],
        );
    }

    #[test]
    fn java_imports() {
        assert_eq!(
            imports(Language::Java, JAVA),
            [
                "java.util.List",
                "java.util",
                "java.lang.Math.max",
                "java.util.Collections"
            ]
        );
        let q = for_language(Language::Java).unwrap();
        let flags: Vec<(bool, bool)> = run(Language::Java, q.imports, "imports", JAVA)
            .into_iter()
            .map(|m| {
                (
                    m.contains_key("import.static"),
                    m.contains_key("import.wildcard"),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [(false, false), (false, true), (true, false), (true, true)]
        );
    }

    #[test]
    fn java_calls() {
        assert_eq!(
            calls(Language::Java, JAVA),
            [
                "ArrayList",
                "max",
                "helper",
                "run",
                "Inner",
                "go",
                "HashMap",
                "println",
                "name"
            ]
        );
    }

    #[test]
    fn java_static_final_is_const_not_field() {
        assert_symbols(
            Language::Java,
            r#"
class C {
    static final int CONST = 1;
    static int shared;
    final int id = 0;
    int x;
}
"#,
            &[
                ("class", "C"),
                ("const", "CONST"),
                ("field", "shared"),
                ("field", "id"),
                ("field", "x"),
            ],
        );
    }

    // --------------------------------------------------------------- rust

    const RUST: &str = r#"
use std::collections::{HashMap, HashSet};
use crate::types::Language;
use super::parent;
use self::child::Thing as Other;
pub use std::io::*;
use serde;
use {a::b, c};
extern crate alloc;
mod submodule;

pub const MAX: u32 = 10;
static GLOBAL: &str = "x";
static mut COUNTER: u32 = 0;
pub type Alias = Vec<u8>;
pub struct Point { x: i32 }
pub union U { a: u32, b: f32 }
pub enum Shape { Circle(f32), Square }

pub trait Area {
    fn area(&self) -> f32;
    fn describe(&self) -> String { String::new() }
}

impl Area for Point {
    fn area(&self) -> f32 {
        helper::<f32>();
        self.x as f32
    }
}

impl Point {
    pub fn new(x: i32) -> Self { Point { x } }
}

pub fn free() {
    fn nested() {}
    nested();
    Point::new(1).area();
    println!("x");
    Vec::<u8>::with_capacity(3);
    std::mem::drop(1);
    log::info!("y");
}

macro_rules! my_macro { () => {}; }

mod inner {
    pub fn in_mod() {}
    pub struct InnerS;
}

extern "C" {
    fn c_fn(x: i32) -> i32;
}
"#;

    #[test]
    fn rust_symbols() {
        assert_symbols(
            Language::Rust,
            RUST,
            &[
                ("function", "free"),
                ("function", "nested"),
                ("function", "in_mod"),
                ("function", "c_fn"),
                ("method", "area"),
                ("method", "describe"),
                ("method", "new"),
                ("struct", "Point"),
                ("struct", "U"),
                ("struct", "InnerS"),
                ("enum", "Shape"),
                ("trait", "Area"),
                ("type", "Alias"),
                ("const", "MAX"),
                ("const", "GLOBAL"),
                ("variable", "COUNTER"),
                ("macro", "my_macro"),
                ("module", "inner"),
                ("module", "submodule"),
                ("field", "x"),
                ("field", "a"),
                ("field", "b"),
                ("field", "Circle"),
                ("field", "Square"),
            ],
        );
        // `area` is defined twice (trait signature + impl); both must be seen.
        let got = symbols(Language::Rust, RUST);
        assert_eq!(
            got.iter()
                .filter(|(k, n, _)| k == "method" && n == "area")
                .count(),
            2
        );
    }

    #[test]
    fn rust_imports_keep_prefixes_and_groups_verbatim() {
        assert_eq!(
            imports(Language::Rust, RUST),
            [
                "std::collections::{HashMap, HashSet}",
                "crate::types::Language",
                "super::parent",
                "self::child::Thing",
                "std::io::*",
                "serde",
                "{a::b, c}",
                "alloc",
            ]
        );
        let q = for_language(Language::Rust).unwrap();
        let mods = texts(
            run(Language::Rust, q.imports, "imports", RUST),
            "import.mod",
        );
        assert_eq!(mods, ["submodule"]);
    }

    #[test]
    fn rust_calls() {
        // `Point::new(1).area()` reports the inner call first: both start at
        // the same byte and tree-sitter emits the match that completes first.
        assert_eq!(
            calls(Language::Rust, RUST),
            [
                "new",
                "helper",
                "nested",
                "new",
                "area",
                "println",
                "with_capacity",
                "drop",
                "info"
            ]
        );
    }

    #[test]
    fn rust_static_mut_vs_mut_in_type() {
        assert_symbols(
            Language::Rust,
            "static FOO: &mut i32 = ptr;\nstatic mut BAR: i32 = 0;\n",
            &[("const", "FOO"), ("variable", "BAR")],
        );
    }

    // ---------------------------------------------------------------- ts

    const TS: &str = r#"
import React, { useState } from 'react';
import type { Foo } from './types';
import * as path from "path";
import fs = require('fs');
import './side-effect';
export { a } from './a';
export * from './b';
const lazy = () => import('./lazy');
const cp = require('child_process');

export const MAX = 10;
const obj = { key: 1 };
export const handler = async (e: Event) => {};
const gen = function* () {};
let counter = 0;
export function greet(name: string): string { return hello(name); }
function* generator() {}
export default class App extends Base<Props> {
  private count = 0;
  static create() { return new App(); }
  constructor() { super(); }
  render(): void { this.helper(); util.format(); this.#secret(); }
  onClick = (e: Event) => {};
  #secret() {}
}
abstract class Shape { abstract area(): number; }
const Expr = class Named {};
const Anon = class {};
export interface Props { title: string; onSave(): void; }
export type ID = string | number;
export enum Color { Red, Green }
namespace NS { export const inner = 1; }
declare module 'ext' { export function ext(): void; }
declare function declared(): void;
declare const DECL: number;
const api = { fetch: () => {}, save: function () {} };
module.exports.legacy = function () {};
Foo.prototype.bar = () => {};
const casted = value as number;
const nn = maybe!;
new ns.Widget();
"#;

    const TS_EXPECTED: &[(&str, &str)] = &[
        ("function", "lazy"),
        ("function", "handler"),
        ("function", "gen"),
        ("function", "greet"),
        ("function", "generator"),
        ("function", "ext"),
        ("function", "declared"),
        ("function", "fetch"),
        ("function", "save"),
        ("function", "legacy"),
        ("function", "bar"),
        ("class", "App"),
        ("class", "Shape"),
        ("class", "Named"),
        ("class", "Anon"),
        ("method", "create"),
        ("method", "constructor"),
        ("method", "render"),
        ("method", "onClick"),
        ("method", "#secret"),
        ("method", "area"),
        ("method", "onSave"),
        ("interface", "Props"),
        ("field", "title"),
        ("field", "count"),
        ("variable", "counter"),
        ("type", "ID"),
        ("enum", "Color"),
        ("field", "Red"),
        ("field", "Green"),
        ("module", "NS"),
        ("module", "ext"),
        ("const", "cp"),
        ("const", "MAX"),
        ("const", "obj"),
        ("const", "inner"),
        ("const", "DECL"),
        ("const", "api"),
        ("const", "casted"),
        ("const", "nn"),
    ];

    const TS_IMPORTS: &[&str] = &[
        "react",
        "./types",
        "path",
        "fs",
        "./side-effect",
        "./a",
        "./b",
        "./lazy",
        "child_process",
    ];

    const TS_CALLS: &[&str] = &["hello", "App", "helper", "format", "#secret", "Widget"];

    #[test]
    fn typescript_symbols() {
        assert_symbols(Language::TypeScript, TS, TS_EXPECTED);
    }

    #[test]
    fn typescript_imports_are_unquoted() {
        assert_eq!(imports(Language::TypeScript, TS), TS_IMPORTS);
    }

    #[test]
    fn typescript_calls() {
        assert_eq!(calls(Language::TypeScript, TS), TS_CALLS);
    }

    #[test]
    fn tsx_shares_typescript_queries_and_handles_jsx() {
        assert_symbols(Language::Tsx, TS, TS_EXPECTED);
        assert_eq!(imports(Language::Tsx, TS), TS_IMPORTS);
        assert_eq!(calls(Language::Tsx, TS), TS_CALLS);

        let tsx = r#"
import { useState } from 'react';
export const Button = ({ label }: { label: string }) => <button onClick={() => track(label)}>{label}</button>;
export default function Page() { const [n, setN] = useState(0); return <Button label="x" />; }
"#;
        assert_symbols(
            Language::Tsx,
            tsx,
            &[("function", "Button"), ("function", "Page")],
        );
        assert_eq!(imports(Language::Tsx, tsx), ["react"]);
        assert_eq!(calls(Language::Tsx, tsx), ["track", "useState"]);
    }

    #[test]
    fn anonymous_default_exports_are_named_default() {
        for lang in [Language::TypeScript, Language::Tsx, Language::JavaScript] {
            assert_symbols(
                lang,
                "export default function () {}\n",
                &[("function", "default")],
            );
            assert_symbols(
                lang,
                "export default async () => {};\n",
                &[("function", "default")],
            );
            assert_symbols(lang, "export default class {}\n", &[("class", "default")]);
            // Named default exports keep their real name and are not doubled.
            assert_symbols(
                lang,
                "export default function named() {}\n",
                &[("function", "named")],
            );
            assert_symbols(
                lang,
                "export default class Named {}\n",
                &[("class", "Named")],
            );
        }
    }

    // ---------------------------------------------------------------- js

    const JS: &str = r#"
import React, { useState } from 'react';
import * as path from "path";
import './side-effect';
export { a } from './a';
export * from './b';
const lazy = () => import('./lazy');
const cp = require('child_process');
const { join } = require('node:path');

export const MAX = 10;
const obj = { key: 1 };
export const handler = async (e) => {};
var legacyVar = function () {};
let counter = 0;
export function greet(name) { return hello(name); }
function* generator() {}
export default class App extends Base {
  count = 0;
  static create() { return new App(); }
  constructor() { super(); }
  render() { this.helper(); util.format(); }
  onClick = (e) => {};
  #secret() {}
}
const Expr = class Named {};
const Anon = class {};
const api = { fetch: () => {}, save: function () {} };
module.exports.legacy = function () {};
Foo.prototype.bar = () => {};
exports.plain = 1;
"#;

    #[test]
    fn javascript_symbols() {
        assert_symbols(
            Language::JavaScript,
            JS,
            &[
                ("function", "lazy"),
                ("function", "handler"),
                ("function", "legacyVar"),
                ("function", "greet"),
                ("function", "generator"),
                ("function", "fetch"),
                ("function", "save"),
                ("function", "legacy"),
                ("function", "bar"),
                ("class", "App"),
                ("class", "Named"),
                ("class", "Anon"),
                ("method", "create"),
                ("method", "constructor"),
                ("method", "render"),
                ("method", "onClick"),
                ("method", "#secret"),
                ("const", "cp"),
                ("const", "MAX"),
                ("const", "obj"),
                ("const", "api"),
                ("field", "count"),
                ("variable", "counter"),
            ],
        );
    }

    #[test]
    fn javascript_imports_are_unquoted() {
        assert_eq!(
            imports(Language::JavaScript, JS),
            [
                "react",
                "path",
                "./side-effect",
                "./a",
                "./b",
                "./lazy",
                "child_process",
                "node:path"
            ]
        );
    }

    #[test]
    fn javascript_calls() {
        assert_eq!(
            calls(Language::JavaScript, JS),
            ["hello", "App", "helper", "format"]
        );
    }

    // --------------------------------------------------------------- corpus
    //
    // Walks the checked-in language corpora and prints per-kind counts.
    // Ignored so `cargo test --workspace` stays fast; run with
    // `--ignored dump_corpus_symbol_kind_counts --nocapture`.

    fn walk_sources(dir: &std::path::Path, exts: &[&str], out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(
                    name,
                    "node_modules" | "target" | ".git" | "dist" | "build" | "vendor"
                ) {
                    continue;
                }
                walk_sources(&path, exts, out);
            } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if exts.contains(&ext) {
                    out.push(path);
                }
            }
        }
    }

    fn count_corpus(
        lang: Language,
        dir: &std::path::Path,
        exts: &[&str],
    ) -> (usize, usize, usize, BTreeMap<String, usize>) {
        let mut files = Vec::new();
        walk_sources(dir, exts, &mut files);
        files.sort();
        let q = for_language(lang).unwrap();
        let ts_lang = grammar(lang);
        let query = compile(lang, q.symbols, "symbols");
        let names = query.capture_names();
        let mut parser = Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        let mut parsed = 0usize;
        let mut skipped = 0usize;
        for path in &files {
            let Ok(source) = std::fs::read_to_string(path) else {
                skipped += 1;
                continue;
            };
            let Some(tree) = parser.parse(&source, None) else {
                skipped += 1;
                continue;
            };
            parsed += 1;
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
            while let Some(m) = matches.next() {
                let mut kind = None;
                for c in m.captures() {
                    let name = names[c.index as usize];
                    if let Some(k) = name.strip_prefix(DEFINITION_PREFIX) {
                        kind = Some(k.to_string());
                        break;
                    }
                }
                if let Some(k) = kind {
                    *kinds.entry(k).or_insert(0) += 1;
                }
            }
        }
        (files.len(), parsed, skipped, kinds)
    }

    #[test]
    #[ignore]
    fn dump_corpus_symbol_kind_counts() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.join("../..");
        let sets: &[(&str, Language, std::path::PathBuf, &[&str])] = &[
            (
                "corpus/java (gson)",
                Language::Java,
                workspace.join("corpus/java"),
                &["java"],
            ),
            (
                "corpus/rust (ripgrep)",
                Language::Rust,
                workspace.join("corpus/rust"),
                &["rs"],
            ),
            (
                "../openvisio-oss (TypeScript)",
                Language::TypeScript,
                workspace.join("../openvisio-oss"),
                &["ts", "tsx"],
            ),
        ];
        for (label, lang, dir, exts) in sets {
            assert!(
                dir.is_dir(),
                "{label}: missing corpus dir {}",
                dir.display()
            );
            let (files, parsed, skipped, kinds) = count_corpus(*lang, dir, exts);
            let total: usize = kinds.values().copied().sum();
            eprintln!("=== {label} ===");
            eprintln!("files={files} parsed={parsed} skipped_unreadable={skipped} symbols={total}");
            for (k, n) in &kinds {
                eprintln!("  {k}: {n}");
            }
            assert!(parsed > 0, "{label}: parsed no files");
        }
    }
    // ---------------------------------------------------------------- C / C++

    const C_SRC: &str = r#"
#include "local.h"
#include <stdio.h>

struct Point {
    int x;
    int y;
};

enum Color { RED, GREEN };

typedef struct Point Point_t;

int add(int a, int b) {
    return a + b;
}

int main(void) {
    Point_t p;
    printf("%d", add(1, 2));
    return 0;
}
"#;

    const CPP_SRC: &str = r#"
#include "util.hpp"
#include <vector>

namespace geom {
class Shape {
public:
    virtual int area() const;
};

struct Box {
    int w;
    int h;
    int area() const { return w * h; }
};

int Shape::area() const { return 0; }

void draw(Shape* s) {
    s->area();
    geom::Shape* t = s;
    t->area();
}
}  // namespace geom
"#;

    #[test]
    fn c_symbols_cover_functions_structs_enums() {
        // symbols() yields (kind, name, range)
        let got = symbols(Language::C, C_SRC);
        let by_name: BTreeMap<_, _> = got
            .iter()
            .map(|(k, n, _)| (n.as_str(), k.as_str()))
            .collect();
        for need in ["add", "main", "Point", "Color", "Point_t", "RED", "GREEN"] {
            assert!(by_name.contains_key(need), "missing {need} in {by_name:?}");
        }
        assert_eq!(by_name.get("add"), Some(&"function"));
        assert_eq!(by_name.get("Point"), Some(&"struct"));
        assert_eq!(by_name.get("Color"), Some(&"enum"));
        assert_eq!(by_name.get("Point_t"), Some(&"type"));
        assert_eq!(by_name.get("RED"), Some(&"const"));
    }

    #[test]
    fn c_imports_and_calls() {
        // system_lib_string nodes include angle brackets; production
        // extract_imports runs `unquote` which strips them.
        let raw = imports(Language::C, C_SRC);
        let got: Vec<_> = raw
            .iter()
            .map(|s| {
                let b = s.as_bytes();
                if s.len() >= 2 && matches!((b[0], b[s.len() - 1]), (b'"', b'"') | (b'<', b'>')) {
                    &s[1..s.len() - 1]
                } else {
                    s.as_str()
                }
            })
            .collect();
        assert_eq!(got, ["local.h", "stdio.h"]);
        let got = calls(Language::C, C_SRC);
        let names: BTreeSet<_> = got.iter().map(|n| n.as_str()).collect();
        assert!(names.contains("add"), "{names:?}");
        assert!(names.contains("printf"), "{names:?}");
    }

    #[test]
    fn cpp_symbols_cover_class_struct_namespace_methods() {
        let got = symbols(Language::Cpp, CPP_SRC);
        let by_name: BTreeMap<_, _> = got
            .iter()
            .map(|(k, n, _)| (n.as_str(), k.as_str()))
            .collect();
        for need in ["geom", "Shape", "Box", "area", "draw"] {
            assert!(by_name.contains_key(need), "missing {need} in {by_name:?}");
        }
        assert_eq!(by_name.get("geom"), Some(&"module"));
        assert_eq!(by_name.get("Shape"), Some(&"class"));
        assert_eq!(by_name.get("Box"), Some(&"struct"));
        assert_eq!(by_name.get("draw"), Some(&"function"));
        // `area` appears as method (decl + out-of-line / inline) — at least one method.
        assert!(
            got.iter().any(|(k, n, _)| k == "method" && n == "area"),
            "expected method area, got {got:?}"
        );
    }

    #[test]
    fn cpp_imports_and_calls() {
        let raw = imports(Language::Cpp, CPP_SRC);
        let got: Vec<_> = raw
            .iter()
            .map(|s| {
                let b = s.as_bytes();
                if s.len() >= 2 && matches!((b[0], b[s.len() - 1]), (b'"', b'"') | (b'<', b'>')) {
                    &s[1..s.len() - 1]
                } else {
                    s.as_str()
                }
            })
            .collect();
        assert_eq!(got, ["util.hpp", "vector"]);
        let got = calls(Language::Cpp, CPP_SRC);
        let names: BTreeSet<_> = got.iter().map(|n| n.as_str()).collect();
        assert!(names.contains("area"), "{names:?}");
    }

    // ---------------------------------------------------------------- PHP

    const PHP_SRC: &str = r#"
<?php
namespace App\Geom;

use App\Support\Helper;
require_once "bootstrap.php";

interface Drawable {
    public function draw(): void;
}

trait HasId {
    public int $id;
}

enum Color: string {
    case Red = "r";
}

class Shape implements Drawable {
    public const PI = 3.14;
    private string $name;

    public function draw(): void {
        $this->paint();
    }

    private function paint(): void {}
}

function area(Shape $s): int {
    return Helper::boost($s);
}

$s = new Shape();
$s->draw();
area($s);
"#;

    #[test]
    fn php_symbols_cover_class_interface_trait_enum_method_const() {
        let got = symbols(Language::Php, PHP_SRC);
        let by_name: BTreeMap<_, _> = got
            .iter()
            .map(|(k, n, _)| (n.as_str(), k.as_str()))
            .collect();
        for need in [
            "Drawable", "HasId", "Color", "Shape", "draw", "paint", "PI", "area",
        ] {
            assert!(by_name.contains_key(need), "missing {need} in {by_name:?}");
        }
        assert!(
            by_name
                .keys()
                .any(|n| n.contains("Geom") || n.contains("App")),
            "expected namespace module, got {by_name:?}"
        );
        assert_eq!(by_name.get("Drawable"), Some(&"interface"));
        assert_eq!(by_name.get("HasId"), Some(&"trait"));
        assert_eq!(by_name.get("Color"), Some(&"enum"));
        assert_eq!(by_name.get("Shape"), Some(&"class"));
        assert_eq!(by_name.get("draw"), Some(&"method"));
        assert_eq!(by_name.get("area"), Some(&"function"));
        assert_eq!(by_name.get("PI"), Some(&"const"));
    }

    #[test]
    fn php_imports_and_calls() {
        let imps = imports(Language::Php, PHP_SRC);
        assert!(
            imps.iter().any(|s| s.contains("Helper")),
            "expected use Helper, got {imps:?}"
        );
        assert!(
            imps.iter().any(|s| s.contains("bootstrap")),
            "expected require_once, got {imps:?}"
        );
        let got = calls(Language::Php, PHP_SRC);
        let names: BTreeSet<_> = got.iter().map(|n| n.as_str()).collect();
        assert!(
            names.contains("draw")
                || names.contains("area")
                || names.contains("boost")
                || names.contains("Shape")
                || names.contains("paint"),
            "expected call names, got {names:?}"
        );
    }

    // ---------------------------------------------------------------- Dart

    const DART_SRC: &str = r#"
import 'package:flutter/material.dart';
import 'relative/helper.dart';
import 'dart:async';
export 'package:flutter/widgets.dart';
part 'part_file.dart';
part of 'main_lib.dart';

class MyClass {
  int get myGetter => 42;
  set mySetter(int value) {}
}

mixin MyMixin {
  String get name => "mixin";
}

enum Status {
  active,
  inactive,
}

void topLevelFunction() {
  print("hello");
}
"#;

    #[test]
    fn dart_symbols_cover_class_mixin_enum_functions() {
        let got = symbols(Language::Dart, DART_SRC);
        let by_name: BTreeMap<_, _> = got
            .iter()
            .map(|(k, n, _)| (n.as_str(), k.as_str()))
            .collect();
        for need in [
            "MyClass",
            "MyMixin",
            "Status",
            "topLevelFunction",
            "myGetter",
            "mySetter",
        ] {
            assert!(by_name.contains_key(need), "missing {need} in {by_name:?}");
        }
        assert_eq!(by_name.get("MyClass"), Some(&"class"));
        assert_eq!(by_name.get("MyMixin"), Some(&"trait"));
        assert_eq!(by_name.get("Status"), Some(&"enum"));
        assert_eq!(by_name.get("topLevelFunction"), Some(&"function"));
        assert_eq!(by_name.get("myGetter"), Some(&"method"));
        assert_eq!(by_name.get("mySetter"), Some(&"method"));
    }

    #[test]
    fn dart_named_constructors_are_symbols() {
        let src = "class Point {\n  final int x;\n  Point(this.x);\n  Point.origin() : x = 0;\n  const Point.fixed(this.x);\n  factory Point.parse(String s) => Point(int.parse(s));\n  factory Point.alias(int x) = Point;\n}\n";
        let got = symbols(Language::Dart, src);
        let names: Vec<(&str, &str)> = got
            .iter()
            .map(|(k, n, _)| (n.as_str(), k.as_str()))
            .collect();
        for need in ["origin", "fixed", "parse", "alias"] {
            assert!(
                names.contains(&(need, "method")),
                "missing ctor {need} in {names:?}"
            );
        }
        // The class plus every constructor (unnamed and named) under the class
        // name, so a class rename finds all construction sites.
        assert_eq!(
            names
                .iter()
                .filter(|(n, k)| *n == "Point" && *k == "class")
                .count(),
            1,
            "{names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|(n, k)| *n == "Point" && *k == "method")
                .count(),
            5,
            "{names:?}"
        );
    }

    #[test]
    fn go_function_local_consts_are_symbols() {
        let src = "package p\n\nconst Top = 1\n\nfunc f() int {\n\tconst limit = 3\n\tif true {\n\t\tconst (\n\t\t\tinner = 1\n\t\t)\n\t\t_ = inner\n\t}\n\tvar skipped = 2\n\treturn limit + skipped\n}\n";
        let got = symbols(Language::Go, src);
        let names: Vec<(&str, &str)> = got
            .iter()
            .map(|(k, n, _)| (n.as_str(), k.as_str()))
            .collect();
        for need in ["Top", "limit", "inner"] {
            assert!(
                names.contains(&(need, "const")),
                "missing {need} in {names:?}"
            );
        }
        assert!(!names.iter().any(|(n, _)| *n == "skipped"), "{names:?}");
        assert_eq!(names.iter().filter(|(n, _)| *n == "Top").count(), 1);
    }

    #[test]
    fn dart_imports_and_calls() {
        let imps = imports(Language::Dart, DART_SRC);
        let imp_set: BTreeSet<_> = imps.iter().map(|s| s.as_str()).collect();
        let expected_imps: BTreeSet<_> = [
            "'package:flutter/material.dart'",
            "'relative/helper.dart'",
            "'dart:async'",
            "'package:flutter/widgets.dart'",
            "'part_file.dart'",
            "'main_lib.dart'",
        ]
        .into_iter()
        .collect();
        assert_eq!(imp_set, expected_imps);
        let clls = calls(Language::Dart, DART_SRC);
        assert!(clls.is_empty(), "Dart calls query should be empty");
    }

    // --------------------------------------------------------------- C#

    const CSHARP_SRC: &str = r#"
using System;
using System.Collections.Generic;
using static System.Math;
using IO = System.IO;
using global::System.Text;

namespace App.Demo;

public class Widget {
    public Widget() {}
    public int Count { get; set; }
    public void Run() {
        Console.WriteLine(Max(1, Count));
        var created = new Widget();
        Helper.Go();
    }
}

public struct Point {}
public interface IShape {}
public enum Color { Red }
public record Person(string Name);
"#;

    #[test]
    fn csharp_symbols_cover_types_members_and_namespace() {
        let got = symbols(Language::CSharp, CSHARP_SRC);
        let pairs: Vec<(&str, &str)> = got
            .iter()
            .map(|(k, n, _)| (k.as_str(), n.as_str()))
            .collect();
        for need in [
            ("module", "App.Demo"),
            ("class", "Widget"),
            ("method", "Widget"),
            ("field", "Count"),
            ("method", "Run"),
            ("struct", "Point"),
            ("interface", "IShape"),
            ("enum", "Color"),
            ("class", "Person"),
        ] {
            assert!(pairs.contains(&need), "missing {need:?} in {pairs:?}");
        }
    }

    #[test]
    fn csharp_imports_cover_using_static_and_alias() {
        let imps = imports(Language::CSharp, CSHARP_SRC);
        let set: BTreeSet<_> = imps.iter().map(|s| s.as_str()).collect();
        for need in [
            "System",
            "System.Collections.Generic",
            "System.Math",
            "System.IO",
            "global::System.Text",
        ] {
            assert!(set.contains(need), "missing import {need} in {set:?}");
        }
        let q = for_language(Language::CSharp).unwrap();
        let rows = run(Language::CSharp, q.imports, "imports", CSHARP_SRC);
        assert!(
            rows.iter().any(|m| m.get("import.static").is_some()
                && m.get("import").map(|c| c.text.as_str()) == Some("System.Math")),
            "using static not flagged: {rows:?}"
        );
        assert!(
            rows.iter().any(
                |m| m.get("import.alias").map(|c| c.text.as_str()) == Some("IO")
                    && m.get("import").map(|c| c.text.as_str()) == Some("System.IO")
            ),
            "alias using not flagged: {rows:?}"
        );
    }

    #[test]
    fn csharp_calls_cover_invocation_and_construction() {
        let got = calls(Language::CSharp, CSHARP_SRC);
        let names: BTreeSet<_> = got.iter().map(|s| s.as_str()).collect();
        for need in ["WriteLine", "Max", "Widget", "Go"] {
            assert!(names.contains(need), "missing call {need} in {names:?}");
        }
    }

    const VB_SRC: &str = r#"Imports System.Text
Namespace Acme.Lib
    Public Class Widget
        Public Sub Run()
            Console.WriteLine("hi")
        End Sub
        Public Function Count() As Integer
            Return Math.Max(1, 2)
        End Function
        Public Property Name As String
    End Class
    Public Module Helper
        Public Function N() As Integer
            Return 1
        End Function
    End Module
    Public Structure Point
    End Structure
    Public Interface IFoo
    End Interface
    Public Enum Color
        Red
    End Enum
End Namespace
"#;

    #[test]
    fn vb_symbols_cover_types_methods_and_properties() {
        assert_symbols(
            Language::VisualBasic,
            VB_SRC,
            &[
                ("module", "Acme.Lib"),
                ("class", "Widget"),
                ("method", "Run"),
                ("method", "Count"),
                ("field", "Name"),
                ("module", "Helper"),
                ("method", "N"),
                ("struct", "Point"),
                ("interface", "IFoo"),
                ("enum", "Color"),
            ],
        );
    }

    #[test]
    fn vb_imports_capture_each_namespace() {
        let got = imports(Language::VisualBasic, "Imports System.Text, Acme.Lib\n");
        assert_eq!(got, vec!["System.Text".to_string(), "Acme.Lib".to_string()]);
    }

    #[test]
    fn vb_calls_cover_invocation_and_construction() {
        let got = calls(Language::VisualBasic, VB_SRC);
        let names: BTreeSet<_> = got.iter().map(|s| s.as_str()).collect();
        for need in ["WriteLine", "Max"] {
            assert!(names.contains(need), "missing call {need} in {names:?}");
        }
        let with_new = format!("{VB_SRC}\nClass Boot\n    Sub Main()\n        Dim w = New Widget()\n    End Sub\nEnd Class\n");
        let got = calls(Language::VisualBasic, &with_new);
        let names: BTreeSet<_> = got.iter().map(|s| s.as_str()).collect();
        assert!(names.contains("Widget"), "missing New Widget in {names:?}");
    }

    /// Small `.csx` script: top-level statements plus a local function and a type.
    /// Classified as C# (`Language::from_path`), extracted with the C# queries.
    const CSX_SRC: &str = r#"
using System;
using Acme.Lib;

Console.WriteLine("hi");

void Greet(string name) {
    Console.WriteLine(name);
}

public class ScriptBox {
    public int Count { get; set; }
    public void Run() {}
}
"#;

    #[test]
    fn csharp_csx_script_yields_symbols() {
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("scripts/main.csx")),
            Some(Language::CSharp)
        );
        let got = symbols(Language::CSharp, CSX_SRC);
        let pairs: Vec<(&str, &str)> = got
            .iter()
            .map(|(k, n, _)| (k.as_str(), n.as_str()))
            .collect();
        for need in [
            ("function", "Greet"),
            ("class", "ScriptBox"),
            ("field", "Count"),
            ("method", "Run"),
        ] {
            assert!(pairs.contains(&need), "missing {need:?} in {pairs:?}");
        }
        let imps = imports(Language::CSharp, CSX_SRC);
        let set: BTreeSet<_> = imps.iter().map(|s| s.as_str()).collect();
        assert!(set.contains("System"), "missing using System in {set:?}");
        assert!(
            set.contains("Acme.Lib"),
            "missing using Acme.Lib in {set:?}"
        );
    }
}
