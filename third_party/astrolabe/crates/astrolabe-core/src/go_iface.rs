//! Go implicit interface satisfaction, as graph edges.
//!
//! Go has no `implements` clause: a type satisfies an interface when its
//! method set covers the interface's. Without that relation the graph cannot
//! answer "which types implement `oauth.Provider`", and `resolve_context`
//! never surfaces the implementing files. This module matches method sets
//! syntactically (names, plus parameter and result counts when the one-line
//! signature shows them) and emits [`EdgeKind::Inherit`] edges
//! `implementing type -> interface`, labelled [`Confidence::Syntactic`].
//!
//! Scope and cost bounds:
//!   * candidates live in the interface's package (directory) or in a package
//!     that imports it — the same rule a reader would apply first;
//!   * embedding is followed within one package only (an interface embedding a
//!     type from another package is skipped: its method set is unknown, and
//!     matching only the visible half would over-report);
//!   * an interface with more than [`MAX_IMPLEMENTORS`] matches (`String()
//!     string`, `Close() error`) says nothing about any one type and gets no
//!     edges;
//!   * candidate lookup goes through the rarest method name, so the cost is
//!     the size of that posting list, not types × interfaces.
//!
//! Pointer and value receivers are not distinguished, and promoted methods of
//! embedded fields from other packages are not seen: a syntactic answer with
//! known misses, never a compiler's.

use crate::graph::CodeGraph;
use crate::types::{CodeEdge, CodeSymbol, Confidence, EdgeKind, FileId, Language, SymbolKind};
use std::collections::{BTreeSet, HashMap, HashSet};

/// More matching types than this and the interface is too generic to be a
/// useful edge source.
pub const MAX_IMPLEMENTORS: usize = 50;
/// Embedding depth followed when building a method set.
const MAX_EMBED_DEPTH: usize = 4;

/// Method name plus, when the signature line shows them, parameter and result
/// counts. `None` matches anything (multi-line or truncated signature).
#[derive(Clone, Debug, PartialEq, Eq)]
struct MethodKey {
    name: String,
    params: Option<u16>,
    results: Option<u16>,
}

impl MethodKey {
    fn compatible(&self, other: &MethodKey) -> bool {
        fn eq(a: Option<u16>, b: Option<u16>) -> bool {
            a.is_none() || b.is_none() || a == b
        }
        self.name == other.name && eq(self.params, other.params) && eq(self.results, other.results)
    }
}

#[derive(Default)]
struct TypeEntry {
    /// Declaring symbols (more than one under build-tagged duplicates).
    symbols: Vec<u32>,
    interface: bool,
    methods: Vec<MethodKey>,
    /// Embedded type names; `None` marks an embed outside this package.
    embeds: Vec<Option<String>>,
}

/// Index of the closing bracket matching `s[open]`, tracking `()[]{}`.
fn matching_close(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Number of entries in a parameter or result list (`a, b int` is two).
fn count_list(inner: &str) -> u16 {
    let inner = inner.trim().trim_end_matches(',').trim();
    if inner.is_empty() {
        return 0;
    }
    let mut depth = 0i32;
    let mut n = 1u16;
    for c in inner.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => n = n.saturating_add(1),
            _ => {}
        }
    }
    n
}

/// `Name(params) results` (the part of a signature after any receiver).
fn method_key(name: &str, rest: &str) -> MethodKey {
    let unknown = MethodKey {
        name: name.to_string(),
        params: None,
        results: None,
    };
    if rest.ends_with('…') {
        return unknown;
    }
    let Some(after_name) = rest.trim_start().strip_prefix(name) else {
        return unknown;
    };
    let after_name = after_name.trim_start();
    if !after_name.starts_with('(') {
        return unknown;
    }
    let Some(close) = matching_close(after_name, 0) else {
        return unknown;
    };
    let params = count_list(&after_name[1..close]);
    let mut tail = after_name[close + 1..].trim();
    if let Some(i) = tail.find("//") {
        tail = tail[..i].trim();
    }
    let results = if tail.is_empty() {
        Some(0)
    } else if tail.starts_with('(') {
        matching_close(tail, 0).map(|c| count_list(&tail[1..c]))
    } else {
        Some(1)
    };
    MethodKey {
        name: name.to_string(),
        params: Some(params),
        results,
    }
}

/// `func (r *T[K]) Name(...)` -> (`T`, `Name(...) ...`).
fn receiver_and_rest(sig: &str) -> Option<(&str, &str)> {
    let s = sig.trim_start().strip_prefix("func")?.trim_start();
    if !s.starts_with('(') {
        return None;
    }
    let close = matching_close(s, 0)?;
    let recv = s[1..close].trim();
    let ty = recv
        .rsplit(char::is_whitespace)
        .next()?
        .trim_start_matches('*');
    let ty = ty.split('[').next()?.trim();
    if ty.is_empty() || ty.contains('.') {
        return None;
    }
    Some((ty, &s[close + 1..]))
}

/// An embedded field's signature line is just its type (`*Base`, `io.Reader`,
/// `Base `json:"-"``). `Some(Some(name))` for a same-package embed,
/// `Some(None)` for a qualified one, `None` when the field is named.
fn embedded(sym: &CodeSymbol) -> Option<Option<String>> {
    let mut parts = sym.signature.split_whitespace();
    let first = parts.next()?.trim_start_matches('*');
    if let Some(next) = parts.next() {
        if !(next.starts_with('`') || next.starts_with("//")) {
            return None;
        }
    }
    let first = first.split('[').next().unwrap_or(first);
    if first == sym.name {
        Some(Some(sym.name.clone()))
    } else if first.ends_with(&format!(".{}", sym.name)) {
        Some(None)
    } else {
        None
    }
}

/// Edges `type -> interface` for every Go interface whose method set the type
/// covers. `import_edges` are file-level [`EdgeKind::Import`] edges (only
/// those are read).
pub fn implementation_edges(g: &CodeGraph, import_edges: &[CodeEdge]) -> Vec<CodeEdge> {
    let mut dir_of: HashMap<FileId, &str> = HashMap::new();
    for f in &g.files {
        if f.language == Some(Language::Go) {
            dir_of.insert(f.id, f.path.dir());
        }
    }
    if dir_of.is_empty() {
        return Vec::new();
    }
    // Symbols are stored file by file; collect the Go ones per file.
    let mut by_file: HashMap<FileId, Vec<&CodeSymbol>> = HashMap::new();
    for s in &g.symbols {
        if dir_of.contains_key(&s.file) {
            by_file.entry(s.file).or_default().push(s);
        }
    }

    let mut types: HashMap<(&str, &str), TypeEntry> = HashMap::new();
    let mut files: Vec<FileId> = by_file.keys().copied().collect();
    files.sort_unstable();
    for file in files {
        let dir = dir_of[&file];
        let syms = &by_file[&file];
        // Containers whose members (fields, interface methods) are attributed
        // by line span: the smallest enclosing struct or interface.
        let mut containers: Vec<&CodeSymbol> = syms
            .iter()
            .copied()
            .filter(|s| matches!(s.kind, SymbolKind::Interface | SymbolKind::Struct))
            .collect();
        containers.sort_by_key(|s| (s.start_line, std::cmp::Reverse(s.end_line)));
        for s in syms {
            if matches!(
                s.kind,
                SymbolKind::Interface | SymbolKind::Struct | SymbolKind::Type
            ) {
                let e = types.entry((dir, s.name.as_str())).or_default();
                e.symbols.push(s.id.0);
                e.interface |= s.kind == SymbolKind::Interface;
            }
        }
        let enclosing = |line: u32| -> Option<&CodeSymbol> {
            let end = containers.partition_point(|c| c.start_line <= line);
            containers[..end]
                .iter()
                .rev()
                .take(8)
                .filter(|c| c.end_line >= line)
                .min_by_key(|c| c.end_line - c.start_line)
                .copied()
        };
        for s in syms {
            match s.kind {
                SymbolKind::Method if s.signature.trim_start().starts_with("func") => {
                    if let Some((recv, rest)) = receiver_and_rest(&s.signature) {
                        let key = method_key(&s.name, rest);
                        types.entry((dir, recv)).or_default().methods.push(key);
                    }
                }
                SymbolKind::Method => {
                    if let Some(c) = enclosing(s.start_line)
                        .filter(|c| c.kind == SymbolKind::Interface && c.id != s.id)
                    {
                        let key = method_key(&s.name, &s.signature);
                        types
                            .entry((dir, c.name.as_str()))
                            .or_default()
                            .methods
                            .push(key);
                    }
                }
                SymbolKind::Field => {
                    let Some(emb) = embedded(s) else { continue };
                    if let Some(c) = enclosing(s.start_line) {
                        types
                            .entry((dir, c.name.as_str()))
                            .or_default()
                            .embeds
                            .push(emb);
                    }
                }
                _ => {}
            }
        }
    }

    // Full method sets, following same-package embeds. `None` = unknown
    // (an embed from another package or past the depth limit).
    fn method_set(
        types: &HashMap<(&str, &str), TypeEntry>,
        key: (&str, &str),
        depth: usize,
        seen: &mut HashSet<String>,
        out: &mut Vec<MethodKey>,
    ) -> bool {
        let Some(e) = types.get(&key) else {
            return false;
        };
        out.extend(e.methods.iter().cloned());
        let mut complete = true;
        for emb in &e.embeds {
            match emb {
                Some(name) if depth < MAX_EMBED_DEPTH && seen.insert(name.clone()) => {
                    complete &= method_set(types, (key.0, name.as_str()), depth + 1, seen, out);
                }
                Some(_) if depth < MAX_EMBED_DEPTH => {}
                _ => complete = false,
            }
        }
        complete
    }

    let mut keys: Vec<(&str, &str)> = types.keys().copied().collect();
    keys.sort_unstable();
    let mut concrete: Vec<((&str, &str), Vec<MethodKey>)> = Vec::new();
    let mut interfaces: Vec<((&str, &str), Vec<MethodKey>)> = Vec::new();
    for &key in &keys {
        let e = &types[&key];
        if e.symbols.is_empty() {
            // Methods on a type declared outside the index (or a parse gap).
            continue;
        }
        let mut seen = HashSet::from([key.1.to_string()]);
        let mut set = Vec::new();
        let complete = method_set(&types, key, 0, &mut seen, &mut set);
        if e.interface {
            if complete && !set.is_empty() {
                interfaces.push((key, set));
            }
        } else if !set.is_empty() {
            concrete.push((key, set));
        }
    }
    if interfaces.is_empty() || concrete.is_empty() {
        return Vec::new();
    }
    let mut by_method: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, set)) in concrete.iter().enumerate() {
        let names: BTreeSet<&str> = set.iter().map(|m| m.name.as_str()).collect();
        for n in names {
            by_method.entry(n).or_default().push(i);
        }
    }
    // Package dir -> dirs of Go files importing it.
    let mut importers: HashMap<&str, HashSet<&str>> = HashMap::new();
    for e in import_edges.iter().filter(|e| e.kind == EdgeKind::Import) {
        if let (Some(&from), Some(&to)) = (dir_of.get(&FileId(e.from)), dir_of.get(&FileId(e.to))) {
            importers.entry(to).or_default().insert(from);
        }
    }

    let mut edges = Vec::new();
    for (ikey, iface_set) in &interfaces {
        let dir = &ikey.0;
        let Some(rarest) = iface_set
            .iter()
            .map(|m| by_method.get(m.name.as_str()).map_or(0, Vec::len))
            .zip(iface_set)
            .min_by_key(|(n, _)| *n)
            .map(|(_, m)| m.name.as_str())
        else {
            continue;
        };
        let Some(cands) = by_method.get(rarest) else {
            continue;
        };
        let near = importers.get(dir);
        let mut found: Vec<usize> = Vec::new();
        for &ci in cands {
            let ((cdir, _), cset) = &concrete[ci];
            if cdir != dir && !near.is_some_and(|n| n.contains(cdir)) {
                continue;
            }
            if iface_set
                .iter()
                .all(|need| cset.iter().any(|have| need.compatible(have)))
            {
                found.push(ci);
                if found.len() > MAX_IMPLEMENTORS {
                    break;
                }
            }
        }
        if found.is_empty() || found.len() > MAX_IMPLEMENTORS {
            continue;
        }
        let iface_syms = &types[ikey].symbols;
        for ci in found {
            for &from in &types[&concrete[ci].0].symbols {
                for &to in iface_syms {
                    edges.push(CodeEdge {
                        from,
                        to,
                        kind: EdgeKind::Inherit,
                        weight: 1,
                        confidence: Confidence::Syntactic,
                    });
                }
            }
        }
    }
    edges
}

/// Implementing types per interface symbol id (`EdgeKind::Inherit` edges into
/// any of `ifaces`), each list ordered by path then line.
pub fn implementors_of(
    g: &CodeGraph,
    ifaces: &HashSet<u32>,
) -> std::collections::BTreeMap<u32, Vec<u32>> {
    let mut out: std::collections::BTreeMap<u32, Vec<u32>> = Default::default();
    if ifaces.is_empty() {
        return out;
    }
    for e in g.edges.iter().filter(|e| e.kind == EdgeKind::Inherit) {
        if ifaces.contains(&e.to) {
            out.entry(e.to).or_default().push(e.from);
        }
    }
    let paths: HashMap<FileId, &str> = g.files.iter().map(|f| (f.id, f.path.as_str())).collect();
    let sym = |id: u32| g.symbols.get(id as usize).filter(|s| s.id.0 == id);
    for list in out.values_mut() {
        list.sort_by_key(|&id| {
            sym(id).map(|s| (paths.get(&s.file).copied().unwrap_or(""), s.start_line))
        });
        list.dedup();
    }
    out
}

/// A Go type that is a test double: declared in a `_test.go` file or a
/// mock/fake/testdata package, or named `Mock…` / `Fake…` / `Stub…`.
pub fn is_test_double(path: &str, name: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    lower.ends_with("_test.go")
        || lower.split('/').any(|seg| {
            seg.contains("mock") || seg.contains("fake") || seg == "testdata" || seg == "testutil"
        })
        || ["mock", "fake", "stub"].iter().any(|p| n.starts_with(p))
}

/// One line per interface: `Go 隐式实现方（方法 N/N 全匹配…）：I @a.go:3 ← T
/// @b.go:9；…`, production types first, test doubles tagged `[测试]`, at most
/// `max` implementors listed. Empty when the interface has none.
pub fn render_implementors(g: &CodeGraph, iface: u32, impls: &[u32], max: usize) -> String {
    let sym = |id: u32| g.symbols.get(id as usize).filter(|s| s.id.0 == id);
    let path = |s: &CodeSymbol| {
        g.files
            .iter()
            .find(|f| f.id == s.file)
            .map_or("", |f| f.path.as_str())
    };
    let (Some(i), false) = (sym(iface), impls.is_empty()) else {
        return String::new();
    };
    // Direct method declarations inside the interface body; an embedded
    // interface adds methods we do not count one by one.
    let members = g
        .symbols
        .iter()
        .filter(|s| s.file == i.file && s.start_line > i.start_line && s.end_line <= i.end_line);
    let (mut methods, mut embeds) = (0usize, false);
    for m in members {
        match m.kind {
            SymbolKind::Method => methods += 1,
            SymbolKind::Field | SymbolKind::Interface | SymbolKind::Type => embeds = true,
            _ => {}
        }
    }
    let mut ordered: Vec<(&CodeSymbol, bool)> = impls
        .iter()
        .filter_map(|&id| sym(id))
        .map(|s| (s, is_test_double(path(s), &s.name)))
        .collect();
    // Stable: production first, then test doubles, each in path/line order.
    ordered.sort_by_key(|(_, test)| *test);
    let listed: Vec<String> = ordered
        .iter()
        .take(max)
        .map(|(s, test)| {
            let tag = if *test { " [测试]" } else { "" };
            format!("{} @{}:{}{tag}", s.name, path(s), s.start_line)
        })
        .collect();
    let more = ordered.len().saturating_sub(listed.len());
    let tail = if more > 0 {
        format!("；另有 {more} 个")
    } else {
        String::new()
    };
    let rule = match (methods, embeds) {
        (0, _) => "方法集全匹配".to_string(),
        (n, false) => format!("方法 {n}/{n} 全匹配"),
        (n, true) => format!("方法 {n}/{n} 及内嵌接口全匹配"),
    };
    format!(
        "Go 隐式实现方（{rule}，按方法名与参数个数）：{} @{}:{} ← {}{tail}\n",
        i.name,
        path(i),
        i.start_line,
        listed.join("；")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{index_repo, IndexOptions};
    use std::path::{Path, PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("astrolabe-go-iface-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn index(root: &Path) -> CodeGraph {
        let opts = IndexOptions {
            call_edges: true,
            persist: false,
            ..IndexOptions::default()
        };
        index_repo(root, &opts).0
    }

    /// `(implementor path:name, interface path:name)` pairs.
    fn pairs(g: &CodeGraph) -> BTreeSet<(String, String)> {
        let name = |id: u32| {
            let s = &g.symbols[id as usize];
            let f = g.files.iter().find(|f| f.id == s.file).unwrap();
            format!("{}:{}", f.path.as_str(), s.name)
        };
        g.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Inherit)
            .map(|e| {
                assert_eq!(e.confidence, Confidence::Syntactic);
                (name(e.from), name(e.to))
            })
            .collect()
    }

    fn implementors_of(g: &CodeGraph, iface: &str) -> BTreeSet<String> {
        pairs(g)
            .into_iter()
            .filter(|(_, i)| i == iface)
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn signature_parsing() {
        assert_eq!(
            receiver_and_rest("func (p *GitHubProvider) GetName() string"),
            Some(("GitHubProvider", " GetName() string"))
        );
        assert_eq!(
            receiver_and_rest("func (s Set[K]) Len() int").map(|r| r.0),
            Some("Set")
        );
        assert_eq!(receiver_and_rest("func Free() int"), None);
        let k = method_key(
            "ExchangeToken",
            "ExchangeToken(ctx context.Context, code string, c *gin.Context) (*OAuthToken, error)",
        );
        assert_eq!((k.params, k.results), (Some(3), Some(2)));
        let k = method_key("Set", "Set(a, b int, f func(int, int) bool) // note");
        assert_eq!((k.params, k.results), (Some(3), Some(0)));
        let k = method_key("Multi", "Multi(");
        assert_eq!((k.params, k.results), (None, None));
        let k = method_key("Get", "Get() map[string]int");
        assert_eq!((k.params, k.results), (Some(0), Some(1)));
    }

    #[test]
    fn implementors_by_method_set_in_package_and_importers() {
        let root = scratch("basic");
        write(&root, "go.mod", "module example.com/m\n\ngo 1.22\n");
        write(
            &root,
            "oauth/provider.go",
            "package oauth\n\nimport \"context\"\n\ntype Tok struct{}\n\n\
             // Provider is implemented by every login backend.\n\
             type Provider interface {\n\
             \tGetName() string\n\
             \tExchange(ctx context.Context, code string) (*Tok, error)\n\
             }\n",
        );
        write(
            &root,
            "oauth/github.go",
            "package oauth\n\nimport \"context\"\n\ntype GitHub struct{}\n\n\
             func (p *GitHub) GetName() string { return \"gh\" }\n\
             func (p *GitHub) Exchange(ctx context.Context, code string) (*Tok, error) {\n\
             \treturn nil, nil\n}\n\n\
             // Half has only one of the two methods.\n\
             type Half struct{}\n\nfunc (Half) GetName() string { return \"\" }\n\n\
             // Wrong has the right names but the wrong arity.\n\
             type Wrong struct{}\n\nfunc (Wrong) GetName() string { return \"\" }\n\
             func (Wrong) Exchange(code string) (*Tok, error) { return nil, nil }\n",
        );
        write(
            &root,
            "oauth/embed.go",
            "package oauth\n\nimport \"context\"\n\ntype Base struct{}\n\n\
             func (Base) GetName() string { return \"\" }\n\n\
             type Child struct {\n\tBase\n\tlabel string\n}\n\n\
             func (c *Child) Exchange(ctx context.Context, code string) (*Tok, error) { return nil, nil }\n",
        );
        write(
            &root,
            "impls/ext.go",
            "package impls\n\nimport (\n\t\"context\"\n\n\t\"example.com/m/oauth\"\n)\n\n\
             type Ext struct{}\n\nfunc (Ext) GetName() string { return \"\" }\n\
             func (Ext) Exchange(ctx context.Context, code string) (*oauth.Tok, error) { return nil, nil }\n",
        );
        // Same method set, but the package never imports oauth.
        write(
            &root,
            "far/far.go",
            "package far\n\nimport \"context\"\n\ntype Tok struct{}\n\ntype Far struct{}\n\n\
             func (Far) GetName() string { return \"\" }\n\
             func (Far) Exchange(ctx context.Context, code string) (*Tok, error) { return nil, nil }\n",
        );
        // An unrelated interface of the same name in another package.
        write(
            &root,
            "other/provider.go",
            "package other\n\ntype Provider interface {\n\tLogin() error\n}\n\n\
             type Impl struct{}\n\nfunc (Impl) Login() error { return nil }\n",
        );
        // Interface embedding inside the package.
        write(
            &root,
            "oauth/full.go",
            "package oauth\n\ntype Namer interface {\n\tGetName() string\n}\n\n\
             type Full interface {\n\tNamer\n\tExtra()\n}\n\n\
             type FullImpl struct{}\n\nfunc (FullImpl) GetName() string { return \"\" }\n\
             func (FullImpl) Extra() {}\n\n\
             // Embeds a type from another package: method set unknown, no edges.\n\
             type Foreign interface {\n\tfmt.Stringer\n\tExtra()\n}\n",
        );
        let g = index(&root);
        assert_eq!(
            implementors_of(&g, "oauth/provider.go:Provider"),
            BTreeSet::from([
                "oauth/github.go:GitHub".to_string(),
                "oauth/embed.go:Child".to_string(),
                "impls/ext.go:Ext".to_string(),
            ]),
            "{:?}",
            pairs(&g)
        );
        assert_eq!(
            implementors_of(&g, "other/provider.go:Provider"),
            BTreeSet::from(["other/provider.go:Impl".to_string()])
        );
        assert_eq!(
            implementors_of(&g, "oauth/full.go:Full"),
            BTreeSet::from(["oauth/full.go:FullImpl".to_string()])
        );
        assert!(implementors_of(&g, "oauth/full.go:Foreign").is_empty());
        let namer = implementors_of(&g, "oauth/full.go:Namer");
        for t in ["GitHub", "Half", "Wrong", "Base", "Child", "FullImpl"] {
            assert!(
                namer.iter().any(|n| n.ends_with(&format!(":{t}"))),
                "{t}: {namer:?}"
            );
        }
        assert!(!namer.iter().any(|n| n.starts_with("far/")), "{namer:?}");
        // Interfaces never implement interfaces here.
        assert!(!pairs(&g)
            .iter()
            .any(|(t, _)| t.ends_with(":Namer") || t.ends_with(":Full")));
        let provider = g
            .symbols
            .iter()
            .find(|s| {
                s.name == "Provider"
                    && s.kind == SymbolKind::Interface
                    && s.signature.contains("interface")
                    && g.files
                        .iter()
                        .any(|f| f.id == s.file && f.path.as_str() == "oauth/provider.go")
            })
            .unwrap()
            .id
            .0;
        let by = super::implementors_of(&g, &HashSet::from([provider]));
        let line = render_implementors(&g, provider, &by[&provider], 2);
        assert!(line.contains("Provider @oauth/provider.go:"), "{line}");
        assert!(line.contains("Ext @impls/ext.go:"), "{line}");
        assert!(line.ends_with("另有 1 个\n"), "{line}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The implementors line states the matching rule and the method count
    /// instead of "may have false positives", lists production types before
    /// test doubles and tags the doubles.
    #[test]
    fn implementors_line_counts_methods_and_tags_test_doubles() {
        let root = scratch("doubles");
        write(&root, "go.mod", "module example.com/d\n");
        write(
            &root,
            "p/provider.go",
            "package p\n\ntype Provider interface {\n\tName() string\n\tLogin(code string) error\n\tLogout()\n}\n",
        );
        write(
            &root,
            "p/a_test.go",
            "package p\n\ntype aFake struct{}\n\nfunc (aFake) Name() string { return \"\" }\nfunc (aFake) Login(code string) error { return nil }\nfunc (aFake) Logout() {}\n",
        );
        write(
            &root,
            "p/zgithub.go",
            "package p\n\ntype GitHub struct{}\n\nfunc (GitHub) Name() string { return \"\" }\nfunc (GitHub) Login(code string) error { return nil }\nfunc (GitHub) Logout() {}\n",
        );
        let g = index(&root);
        let provider = g
            .symbols
            .iter()
            .find(|s| s.name == "Provider" && s.kind == SymbolKind::Interface)
            .unwrap()
            .id
            .0;
        let by = super::implementors_of(&g, &HashSet::from([provider]));
        let line = render_implementors(&g, provider, &by[&provider], 5);
        assert!(line.contains("方法 3/3 全匹配"), "{line}");
        assert!(!line.contains("误报"), "{line}");
        let gh = line.find("GitHub @p/zgithub.go:3").expect(&line);
        let fake = line.find("aFake @p/a_test.go:3 [测试]").expect(&line);
        assert!(gh < fake, "production first: {line}");
        assert!(is_test_double("mocks/x.go", "X"));
        assert!(is_test_double("a/b.go", "MockStore"));
        assert!(!is_test_double("oauth/github.go", "GitHubProvider"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn generic_interfaces_get_no_edges() {
        let root = scratch("generic");
        write(&root, "go.mod", "module example.com/g\n");
        let mut src = String::from("package g\n\ntype Pinger interface {\n\tPing()\n}\n\n");
        for n in 0..=MAX_IMPLEMENTORS {
            src.push_str(&format!(
                "type T{n} struct{{}}\n\nfunc (T{n}) Ping() {{}}\n\n"
            ));
        }
        src.push_str("type Rare interface {\n\tPing()\n\tPong(a int) int\n}\n\n");
        src.push_str("func (T3) Pong(a int) int { return a }\n");
        write(&root, "g.go", &src);
        let g = index(&root);
        assert!(implementors_of(&g, "g.go:Pinger").is_empty());
        assert_eq!(
            implementors_of(&g, "g.go:Rare"),
            BTreeSet::from(["g.go:T3".to_string()])
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_edges_without_call_edges_switch() {
        let root = scratch("off");
        write(&root, "go.mod", "module example.com/o\n");
        write(
            &root,
            "a.go",
            "package o\n\ntype I interface {\n\tM()\n}\n\ntype T struct{}\n\nfunc (T) M() {}\n",
        );
        let opts = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        let g = index_repo(&root, &opts).0;
        assert!(!g.edges.iter().any(|e| e.kind == EdgeKind::Inherit));
        assert_eq!(implementors_of(&index(&root), "a.go:I").len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Cost/recall probe (ignored):
    /// `ASTROLABE_GO_IFACE_PROBE=/repo cargo test -p astrolabe-core --release --lib go_iface::tests::probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe() {
        let Ok(root) = std::env::var("ASTROLABE_GO_IFACE_PROBE") else {
            return;
        };
        let g = index(Path::new(&root));
        let imports: Vec<CodeEdge> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Import)
            .cloned()
            .collect();
        let mut times = Vec::new();
        let mut n = 0;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            n = implementation_edges(&g, &imports).len();
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let go = g
            .files
            .iter()
            .filter(|f| f.language == Some(Language::Go))
            .count();
        println!(
            "files={} go_files={go} symbols={} inherit_edges={n} ms_median={:.2} ms_all={times:?}",
            g.files.len(),
            g.symbols.len(),
            times[2]
        );
        if let Ok(want) = std::env::var("ASTROLABE_GO_IFACE_SHOW") {
            for (t, i) in pairs(&g).iter().filter(|(_, i)| i.contains(&want)) {
                println!("  {t} -> {i}");
            }
        }
    }
}
