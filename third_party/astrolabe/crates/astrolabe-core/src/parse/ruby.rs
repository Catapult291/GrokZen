//! Ruby (`.rb`, `Rakefile`, `Gemfile`) extraction hooks (lang-batch-8).
//!
//! Owned by the ruby worker; see `LANG-PLAN.md`. `parse/mod.rs` calls these
//! hooks for [`Language::Ruby`] only, so all language-specific post-processing
//! lives here and never in shared code.
//!
//! * `:sym` / `"str"` names from `attr_*`, `alias_method`, `define_method`
//!   drop their sigil / quotes.
//! * A `def` that is not nested in a `class`, `module` or `class << x` body
//!   is a top-level helper (a private method on `Object`) and reported as a
//!   [`SymbolKind::Function`].
//! * Visibility: methods under a bare `private` / `protected` line, or
//!   wrapped as `private def x`, are not exported. `public` resets it.

use crate::types::{Language, SymbolKind};

#[allow(dead_code)]
const LANG: Language = Language::Ruby;

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    let name = clean_name(name);
    let kind = if kind == SymbolKind::Method
        && definition.kind() == "method"
        && !inside_type_body(definition, source)
    {
        SymbolKind::Function
    } else {
        kind
    };
    (name, kind)
}

fn clean_name(name: &str) -> String {
    let n = name.strip_prefix(':').unwrap_or(name);
    let n = n
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| n.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(n);
    n.to_owned()
}

fn inside_type_body(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let source_text = |n: tree_sitter::Node<'_>| n.utf8_text(source.as_bytes()).unwrap_or("");
    let mut cur = node.parent();
    while let Some(p) = cur {
        if matches!(p.kind(), "class" | "module" | "singleton_class") {
            return true;
        }
        // `Point = Struct.new(:x) do … end` / `Class.new do … end`: the block
        // body is a type body.
        if matches!(p.kind(), "do_block" | "block") {
            if let Some(call) = p.parent().filter(|c| c.kind() == "call") {
                let recv = call
                    .child_by_field_name("receiver")
                    .map(|r| {
                        r.kind() == "constant"
                            && matches!(source_text(r), "Struct" | "Class" | "Module" | "Data")
                    })
                    .unwrap_or(false);
                let is_new = call
                    .child_by_field_name("method")
                    .is_some_and(|m| matches!(source_text(m), "new" | "define"));
                if recv && is_new {
                    return true;
                }
            }
        }
        cur = p.parent();
    }
    false
}

/// Whether a definition is visible outside its file / type.
pub(super) fn exported(name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    let kind = node.kind();
    if !matches!(kind, "method" | "singleton_method" | "call") {
        return true;
    }
    let src = source.as_bytes();
    let text = |n: tree_sitter::Node<'_>| n.utf8_text(src).unwrap_or("");
    // `private def x` / `private attr_reader :y` / `private_class_method def
    // self.c` → the definition is an argument of a restrictive call.
    if let Some(args) = node.parent().filter(|p| p.kind() == "argument_list") {
        if let Some(call) = args.parent().filter(|p| p.kind() == "call") {
            if let Some(m) = call.child_by_field_name("method") {
                if is_restrictive(text(m)) {
                    return false;
                }
            }
        }
    }
    // Bare `private` / `protected` / `public` statements before us
    // (instance methods and accessors; `def self.x` ignores them in Ruby).
    let mut state = true;
    if kind != "singleton_method" {
        let mut sib = node.prev_named_sibling();
        while let Some(s) = sib {
            if s.kind() == "identifier" {
                match text(s) {
                    "private" | "protected" => {
                        state = false;
                        break;
                    }
                    "public" => break,
                    _ => {}
                }
            }
            sib = s.prev_named_sibling();
        }
    }
    // Explicit symbol forms in the same body: `private :a, :b`,
    // `private_class_method :c`, `public :a` (the last one in source order
    // wins).
    let want = clean_name(name);
    if let Some(parent) = node.parent() {
        let mut cur = parent.walk();
        for sib in parent.named_children(&mut cur) {
            if sib.kind() != "call"
                || sib.id() == node.id()
                || sib.child_by_field_name("receiver").is_some()
            {
                continue;
            }
            let Some(m) = sib.child_by_field_name("method").map(text) else {
                continue;
            };
            let vis = match m {
                "private" | "protected" | "private_class_method" => false,
                "public" | "public_class_method" => true,
                _ => continue,
            };
            let Some(args) = sib.child_by_field_name("arguments") else {
                continue;
            };
            let mut ac = args.walk();
            let named = args.named_children(&mut ac).any(|a| {
                matches!(a.kind(), "simple_symbol" | "string") && clean_name(text(a)) == want
            });
            if named {
                state = vis;
            }
        }
    }
    state
}

fn is_restrictive(m: &str) -> bool {
    matches!(m, "private" | "protected" | "private_class_method")
}

#[cfg(test)]
mod tests {
    use crate::parse::{ParsedFile, ParserPool};
    use crate::types::{Language, RelPath, SymbolKind};

    fn parse_source(lang: Language, src: &str) -> Option<ParsedFile> {
        ParserPool::new()
            .parse(lang, &RelPath::new("t.rb"), src)
            .ok()
    }

    #[test]
    fn struct_and_class_new_blocks_hold_methods_not_functions() {
        let src = "Pt = Struct.new(:x) do\n  def dist; end\nend\nK = Class.new do\n  def hi; end\nend\ndef top; end\nitems.each do |i|\n  def nested; end\nend\n";
        let parsed = parse_source(Language::Ruby, src).expect("parse");
        let kind = |n: &str| parsed.symbols.iter().find(|s| s.name == n).unwrap().kind;
        assert_eq!(kind("dist"), SymbolKind::Method);
        assert_eq!(kind("hi"), SymbolKind::Method);
        assert_eq!(kind("top"), SymbolKind::Function);
        assert_eq!(kind("nested"), SymbolKind::Function);
    }

    const SRC: &str = r#"require "foo/bar"
require_relative '../x'
autoload :Baz, "baz/q"
module A::B
  X, Y = 1, 2
  C::D = 3
  Z ||= 4
  class Foo < Bar
    attr_accessor :a, :b
    attr_reader "s"
    alias c d
    alias_method :e, :f
    define_method(:g) { }
    class << self
      def h; end
    end
    def self.i; end
    def ==(o) end
    def j=(v); end
    def pub; end
    private
    def k; end
    public
    def m; end
  end
  private def l; end
end
def top; end
"#;

    fn syms() -> Vec<(String, SymbolKind, bool)> {
        let parsed = parse_source(Language::Ruby, SRC).expect("parse");
        parsed
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.exported))
            .collect()
    }

    fn find(name: &str) -> (SymbolKind, bool) {
        syms()
            .into_iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, k, e)| (k, e))
            .unwrap_or_else(|| panic!("missing {name}: {:?}", syms()))
    }

    #[test]
    fn extracts_every_declaration_kind() {
        let names: Vec<String> = syms().into_iter().map(|s| s.0).collect();
        for n in [
            "B", "X", "Y", "D", "Z", "Foo", "a", "b", "s", "c", "e", "g", "h", "i", "==", "j=",
            "pub", "k", "m", "l", "top",
        ] {
            assert!(names.contains(&n.to_string()), "missing {n}: {names:?}");
        }
        assert_eq!(find("Foo").0, SymbolKind::Class);
        assert_eq!(find("B").0, SymbolKind::Module);
        assert_eq!(find("X").0, SymbolKind::Const);
        assert_eq!(find("a").0, SymbolKind::Field);
        assert_eq!(find("h").0, SymbolKind::Method);
        assert_eq!(find("top").0, SymbolKind::Function);
    }

    #[test]
    fn visibility() {
        assert!(find("pub").1);
        assert!(!find("k").1);
        assert!(find("m").1);
        assert!(!find("l").1);
    }

    #[test]
    fn visibility_symbol_forms_and_attrs() {
        let src = "class K\n  def a; end\n  def b; end\n  private :a\n  private_class_method def self.c; end\n  def self.d; end\n  private_class_method :d\n  attr_reader :pub1\n  private\n  attr_reader :x\n  private attr_reader :y\n  public\n  attr_reader :z\nend\n";
        let p = parse_source(Language::Ruby, src).expect("parse");
        let ex = |n: &str| {
            p.symbols
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("missing {n}: {:?}", p.symbols))
                .exported
        };
        assert!(!ex("a"));
        assert!(ex("b"));
        assert!(!ex("c"));
        assert!(!ex("d"));
        assert!(ex("pub1"));
        assert!(!ex("x"));
        assert!(!ex("y"));
        assert!(ex("z"));
    }

    #[test]
    fn imports_and_calls() {
        let parsed = parse_source(Language::Ruby, SRC).expect("parse");
        assert_eq!(parsed.imports, vec!["foo/bar", "relative:../x", "baz/q"]);
        let calls: Vec<&str> = parsed.calls.iter().map(|(c, _)| c.as_str()).collect();
        assert!(calls.contains(&"attr_accessor"), "{calls:?}");
    }

    /// Corpus dump for the recall bench: `DUMP_DIR`, `DUMP_LIST` (one
    /// relative path per line), `DUMP_OUT` (`file\tline\tname`).
    #[test]
    #[ignore]
    fn zz_dump_ruby_symbols() {
        use std::io::Write;
        let dir = std::env::var("DUMP_DIR").unwrap();
        let list = std::fs::read_to_string(std::env::var("DUMP_LIST").unwrap()).unwrap();
        let mut out = std::fs::File::create(std::env::var("DUMP_OUT").unwrap()).unwrap();
        let pool = ParserPool::new();
        let (mut files, mut syms, mut errs, mut bytes) = (0, 0, 0, 0);
        let t = std::time::Instant::now();
        for rel in list.lines().filter(|l| !l.is_empty()) {
            let Ok(src) = std::fs::read_to_string(format!("{dir}/{rel}")) else {
                continue;
            };
            files += 1;
            bytes += src.len();
            match pool.parse(Language::Ruby, &RelPath::new(rel), &src) {
                Ok(p) => {
                    for s in p.symbols {
                        syms += 1;
                        writeln!(out, "{rel}\t{}\t{}", s.start_line, s.name).unwrap();
                    }
                }
                Err(e) => {
                    errs += 1;
                    eprintln!("ERR {rel}: {e}");
                }
            }
        }
        eprintln!(
            "DUMP files={files} symbols={syms} errors={errs} bytes={bytes} ms={}",
            t.elapsed().as_millis()
        );
    }
}
