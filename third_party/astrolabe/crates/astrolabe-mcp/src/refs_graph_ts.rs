//! TypeScript / JavaScript scope for the index answer of `find_references`
//! (batch 7, stream TS).
//!
//! [`crate::refs_graph::graph_references`] answers from "the declaring file
//! plus the files that import it". In TS/JS that misses two common shapes
//! (`/workspace/audit/arch2026/lang-graph.md` §3.3, R1/R2):
//!
//! * **Barrel re-exports.** `export * from './x'`, `export { A } from './x'`,
//!   `export { A as B } from './x'`, `export * as ns from './x'`, and the
//!   two-step `import { A } from './x'; export { A }` in an `index.ts`: the
//!   importers of the barrel (and of a barrel of that barrel, up to
//!   [`MAX_DEPTH`] hops, with a cycle guard) reach the declaration too.
//! * **Renamed bindings.** `import { A as B }`, `import B from` (default
//!   export), `const B = require()` (`module.exports = A`),
//!   `const { A: B } = require()`: the importer names it `B`, so `B` is
//!   searched there as well.
//!
//! All of it is query-time text parsing of the few files on the import path;
//! the index format is unchanged. The scope only ever grows, and only along
//! files that really (re-)export the name.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::OnceLock;

use astrolabe_core::types::{EdgeKind, FileId, Language};
use regex::Regex;

use crate::index::RepoIndex;

/// Re-export hops followed from the declaring file.
pub(crate) const MAX_DEPTH: usize = 8;
/// Files a scope may reach before the expansion stops.
const MAX_FILES: usize = 5_000;

/// `ASTROLABE_TS_GRAPH_FIRST=off` (or `0`/`false`/`no`) keeps the
/// language server first for TypeScript too.
pub(crate) const TS_GRAPH_FIRST_ENV: &str = "ASTROLABE_TS_GRAPH_FIRST";

fn ts_graph_first_enabled() -> bool {
    !std::env::var(TS_GRAPH_FIRST_ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    })
}

/// Whether `find_references` for `name` (queried from a file of
/// `language`) is answered from the index before any language server.
///
/// Measured (`docs/find-references-graph-first.md`, batch 7): TS/TSX
/// top-level declarations with exactly one definition in the repo recall
/// ≥ 0.999 at precision ≥ 0.993 on aiproxy web and the trpc monorepo.
/// So: the query is TS-family, the TS/JS/Vue family holds exactly one
/// declaration of `name`, it sits in a `.ts`/`.tsx` file and is a
/// Function / Class / Interface / Type / Const. Names with several
/// definitions, methods and fields keep the language server first, and
/// so does JavaScript (not switched yet).
pub(crate) fn ts_graph_first(index: &RepoIndex, language: Language, name: &str) -> bool {
    use astrolabe_core::types::SymbolKind;
    if !matches!(language, Language::TypeScript | Language::Tsx) && language != Language::Vue {
        return false;
    }
    if name.is_empty() || !ts_graph_first_enabled() {
        return false;
    }
    let graph = index.graph();
    let mut decls: Vec<&astrolabe_core::CodeSymbol> = index
        .names()
        .by_lower(&name.to_lowercase())
        .iter()
        .map(|&i| &graph.symbols[i as usize])
        .filter(|s| s.name == name)
        .filter(|s| {
            index
                .file(s.file)
                .and_then(|f| f.language)
                .is_some_and(is_ts_family)
        })
        .collect();
    decls.sort_by_key(|s| (s.file, s.start_line));
    decls.dedup_by_key(|s| (s.file, s.start_line));
    let [decl] = decls.as_slice() else {
        return false;
    };
    matches!(
        decl.kind,
        SymbolKind::Function
            | SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Type
            | SymbolKind::Const
    ) && index
        .file(decl.file)
        .and_then(|f| f.language)
        .is_some_and(|l| matches!(l, Language::TypeScript | Language::Tsx))
}

/// TS/JS family (the languages whose imports this module parses).
pub(crate) fn is_ts_family(language: Language) -> bool {
    matches!(
        language,
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Vue
    )
}

/// Files that reach a declaration, with the extra local names it goes by
/// in each of them.
#[derive(Debug, Default)]
pub(crate) struct TsScope {
    /// File id → local names other than the declared name (sorted).
    pub files: HashMap<u32, Vec<String>>,
    /// Barrel files followed (files re-exporting the name).
    pub barrels: usize,
    /// File id → locals holding a module object that exports it
    /// (`import * as ns`, `const m = require()`, `import { ns }` of an
    /// `export * as ns`): `ns.name` there is ours.
    pub namespaces: HashMap<u32, HashSet<String>>,
    /// Apply [`Self::member_reaches`] (off only in before/after measurement).
    pub member_rule: bool,
    /// The declaration is a property assignment (`app.handle = function
    /// handle(`, `exports.load = function(`): its references are member
    /// accesses `x.name`, and a bare `name` in the declaring file is the
    /// function expression's own name or an unrelated local.
    pub decl_is_member: bool,
    /// Object of that assignment (`app` in `app.handle =`).
    pub decl_base: Option<String>,
    /// The re-export walk stopped at [`MAX_DEPTH`] / [`MAX_FILES`]: the
    /// scope may miss files, so the answer must not be called complete.
    pub truncated: bool,
    /// TS `namespace N {` / `module N {` blocks of the declaring file:
    /// `N.name` reaches a declaration inside one.
    pub containers: HashSet<String>,
}

impl TsScope {
    pub(crate) fn contains(&self, file: u32) -> bool {
        self.files.contains_key(&file)
    }

    /// `base.name` in `file` reaches the declaration: `base` holds a
    /// module object exporting it. Any other `x.name` is a same-named
    /// member (`app.handle`, `this.route`), not a top-level declaration.
    pub(crate) fn member_reaches(&self, file: u32, base: &str, in_def_file: bool) -> bool {
        if !self.member_rule {
            return true;
        }
        if self.decl_is_member {
            // In its own file, through the same object or `this`
            // (`this.router.handle` / `fn.handle` are other objects).
            // Elsewhere the object's type is unknown: kept.
            return !in_def_file || base == "this" || self.decl_base.as_deref() == Some(base);
        }
        self.containers.contains(base)
            || self.namespaces.get(&file).is_some_and(|n| n.contains(base))
    }

    /// A bare `name` (no `x.` before it) counts; `in_def_file` is whether
    /// the hit is in the declaring file.
    pub(crate) fn bare_reaches(&self, in_def_file: bool) -> bool {
        !(self.member_rule && self.decl_is_member && in_def_file)
    }

    /// Words to search in `file`: the declared name plus its local aliases.
    pub(crate) fn words<'a>(&'a self, file: u32, name: &'a str) -> Vec<&'a str> {
        let mut out = vec![name];
        if let Some(extra) = self.files.get(&file) {
            out.extend(extra.iter().map(String::as_str).filter(|w| *w != name));
        }
        out
    }

    /// All alias names, for the answer's note.
    pub(crate) fn aliases(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self
            .files
            .values()
            .flat_map(|v| v.iter().map(String::as_str))
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only switch for before/after measurement: 2 restores the batch-6
    /// scope (declaring file plus direct importers, no aliases), 1 the first
    /// batch-7 commit (no member rule), 0 is current.
    pub(crate) static LEGACY: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

fn legacy() -> u8 {
    #[cfg(test)]
    {
        LEGACY.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        0
    }
}

/// How a module exports something that is our declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    /// The export is the declaration itself (possibly renamed).
    Direct,
    /// The export is a namespace object holding it (`export * as ns`).
    Namespace,
}

/// Scope of `name` declared in `def`: the declaring file, its importers,
/// and the importers of every barrel that re-exports it.
pub(crate) fn ts_scope(
    index: &RepoIndex,
    root: &Path,
    def: FileId,
    decl_line: u32,
    name: &str,
) -> TsScope {
    let graph = index.graph();
    let family = |id: u32| {
        index
            .file(FileId(id))
            .and_then(|f| f.language)
            .is_some_and(is_ts_family)
    };
    // Reverse import edges and per-file import targets (TS family only).
    let mut importers: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut targets: HashMap<u32, Vec<u32>> = HashMap::new();
    for e in &graph.edges {
        if e.kind == EdgeKind::Import && e.from != e.to && family(e.from) && family(e.to) {
            importers.entry(e.to).or_default().push(e.from);
            targets.entry(e.from).or_default().push(e.to);
        }
    }
    let mut scope = TsScope {
        member_rule: legacy() == 0,
        ..TsScope::default()
    };
    let def_src = index
        .file(def)
        .and_then(|f| astrolabe_core::text::read_source(&root.join(f.path.as_str())).ok());
    if let Some(src) = &def_src {
        if let Some(base) = declared_as_property(src, decl_line, name) {
            scope.decl_is_member = true;
            scope.decl_base = Some(base);
        }
        static CONTAINER: OnceLock<Regex> = OnceLock::new();
        scope.containers = re(&CONTAINER, r"\b(?:namespace|module)\s+([\w$]+)\s*\{")
            .captures_iter(src)
            .map(|c| c[1].to_string())
            .collect();
    }
    scope.files.insert(def.0, Vec::new());
    if legacy() == 2 {
        for &i in importers.get(&def.0).into_iter().flatten() {
            scope.files.entry(i).or_default();
        }
        return scope;
    }
    let path_of = |id: u32| index.file(FileId(id)).map(|f| f.path.as_str().to_string());
    let mut facts: HashMap<u32, Option<Facts>> = HashMap::new();
    let mut facts_of = |id: u32| -> Option<Facts> {
        facts
            .entry(id)
            .or_insert_with(|| {
                let path = path_of(id)?;
                let src = astrolabe_core::text::read_source(&root.join(&path)).ok()?;
                Some(Facts::parse(&src))
            })
            .clone()
    };
    // Seed: the declared name, plus what the declaring file exports it as.
    let mut seed: HashMap<String, Via> = HashMap::new();
    seed.insert(name.to_string(), Via::Direct);
    if let Some(f) = facts_of(def.0) {
        for (local, exported) in &f.local_exports {
            if local == name {
                seed.insert(exported.clone(), Via::Direct);
            }
        }
    }
    let mut exported: HashMap<u32, HashMap<String, Via>> = HashMap::new();
    exported.insert(def.0, seed.clone());
    let mut queue: VecDeque<(u32, HashMap<String, Via>, usize)> = VecDeque::new();
    queue.push_back((def.0, seed, 0));
    while let Some((module, names, depth)) = queue.pop_front() {
        let Some(module_path) = path_of(module) else {
            continue;
        };
        for &imp in importers.get(&module).into_iter().flatten() {
            if scope.files.len() >= MAX_FILES {
                scope.truncated = true;
                break;
            }
            let locals = scope.files.entry(imp).or_default();
            let Some(f) = facts_of(imp) else { continue };
            let Some(imp_path) = path_of(imp) else {
                continue;
            };
            let imp_targets: Vec<(u32, String)> = targets
                .get(&imp)
                .into_iter()
                .flatten()
                .filter_map(|&t| Some((t, path_of(t)?)))
                .collect();
            // Does a statement's `spec` name `module`? When no statement of
            // the file matches `module` by path, unmatched specs (aliases the
            // path test misses, workspace packages) count as possibly ours;
            // otherwise only the matching ones do (`lucide-react` is not).
            let specs = f
                .imports
                .iter()
                .map(|(s, _)| s)
                .chain(f.reexports.iter().map(|(s, _)| s));
            let any_matched = specs
                .clone()
                .any(|s| spec_target(&imp_path, s, &imp_targets) == Some(module));
            let reaches = |spec: &str| match spec_target(&imp_path, spec, &imp_targets) {
                Some(t) => t == module,
                None => {
                    !any_matched
                        && (!is_relative(spec) || relative_could_be(&imp_path, spec, &module_path))
                }
            };
            // Local bindings of our declaration in the importer.
            let mut bound: HashSet<String> = HashSet::new();
            let mut extra: HashSet<String> = HashSet::new();
            for (spec, bindings) in &f.imports {
                if !reaches(spec) {
                    continue;
                }
                for (imported, local) in bindings {
                    if imported == "*" || names.get(imported) == Some(&Via::Namespace) {
                        scope
                            .namespaces
                            .entry(imp)
                            .or_default()
                            .insert(local.clone());
                        continue;
                    }
                    if names.get(imported) == Some(&Via::Direct) {
                        bound.insert(local.clone());
                        // `{ Card as C }`: `Card` here names it too.
                        if imported != "default" && imported != name {
                            extra.insert(imported.clone());
                        }
                    }
                }
            }
            for l in bound.iter().chain(&extra) {
                if l != name && !locals.contains(l) {
                    locals.push(l.clone());
                    locals.sort();
                }
            }
            if depth + 1 > MAX_DEPTH {
                if !f.reexports.is_empty() || !f.local_exports.is_empty() {
                    scope.truncated = true;
                }
                continue;
            }
            // What the importer re-exports of it.
            let mut out: HashMap<String, Via> = HashMap::new();
            for (spec, re) in &f.reexports {
                if !reaches(spec) {
                    continue;
                }
                match re {
                    ReExport::Star => {
                        for (n, via) in &names {
                            if n != "default" {
                                out.insert(n.clone(), *via);
                            }
                        }
                    }
                    ReExport::Whole => {
                        out.extend(names.iter().map(|(n, via)| (n.clone(), *via)));
                    }
                    ReExport::StarAs(ns) => {
                        out.insert(ns.clone(), Via::Namespace);
                    }
                    ReExport::Named(list) => {
                        for (imported, as_name) in list {
                            if let Some(via) = names.get(imported) {
                                out.insert(as_name.clone(), *via);
                            }
                        }
                    }
                }
            }
            for (local, as_name) in &f.local_exports {
                if bound.contains(local) {
                    out.insert(as_name.clone(), Via::Direct);
                }
            }
            if out.is_empty() {
                continue;
            }
            // A renamed re-export (`export { A as B }`) names it `B` here.
            if let Some(words) = scope.files.get_mut(&imp) {
                for (n, via) in &out {
                    if *via == Via::Direct && n != name && n != "default" && !words.contains(n) {
                        words.push(n.clone());
                        words.sort();
                    }
                }
            }
            let seen = exported.entry(imp).or_default();
            let fresh: HashMap<String, Via> = out
                .into_iter()
                .filter(|(n, _)| !seen.contains_key(n))
                .collect();
            if fresh.is_empty() {
                continue; // cycle or already followed
            }
            seen.extend(fresh.iter().map(|(n, v)| (n.clone(), *v)));
            scope.barrels += 1;
            queue.push_back((imp, fresh, depth + 1));
        }
    }
    scope
}

/// `name` at `byte` is an object-literal key or type member (`name:` right
/// after `{`, `,` or the start of the line; not `::`, not a `case` label,
/// not a destructuring `require`/`import` binding).
pub(crate) fn is_object_key(line: &str, byte: usize) -> bool {
    let Some(rest) = line.get(byte..) else {
        return false;
    };
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(rest.len());
    let after = rest[end..].trim_start();
    let after = after.strip_prefix('?').unwrap_or(after).trim_start();
    if !after.starts_with(':') || after.starts_with("::") {
        return false;
    }
    if line.contains("require(") || line.trim_start().starts_with("import ") {
        return false;
    }
    let before = line[..byte].trim_end();
    before.is_empty() || before.ends_with('{') || before.ends_with(',')
}

/// When the first occurrence of `name` on 1-based `line` is a property
/// assignment target `x.name =` (also `x.y.name =`), the object right
/// before the dot (`x`, or `y`).
fn declared_as_property(src: &str, line: u32, name: &str) -> Option<String> {
    let text = src.lines().nth(line.saturating_sub(1) as usize)?;
    let &b = crate::refs_graph::word_offsets(text, name).first()?;
    let after = text[b + name.len()..].trim_start();
    if !after.starts_with('=') || after.starts_with("==") || after.starts_with("=>") {
        return None;
    }
    let before = text[..b].strip_suffix('.')?;
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .last()
        .map(|(i, _)| i)?;
    Some(before[start..].to_string())
}

// ------------------------------------------------------------- parsing

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReExport {
    Star,
    /// CommonJS `module.exports = require(spec)`: everything, `default` too.
    Whole,
    StarAs(String),
    /// `(imported, exported)` pairs.
    Named(Vec<(String, String)>),
}

/// Import / export statements of one module.
#[derive(Clone, Debug, Default)]
struct Facts {
    /// `(spec, [(imported, local)])`; imported is a name, `default` or `*`.
    imports: Vec<(String, Vec<(String, String)>)>,
    /// `(spec, what)` for `export … from spec`.
    reexports: Vec<(String, ReExport)>,
    /// `(local, exported)` for exports of local bindings: `export { a as b }`,
    /// `export default a`, `module.exports = a` (as `default`),
    /// `module.exports = { b: a }`, `exports.b = a`.
    local_exports: Vec<(String, String)>,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

const KEYWORDS: &[&str] = &[
    "function",
    "class",
    "async",
    "new",
    "abstract",
    "await",
    "const",
    "let",
    "var",
    "typeof",
    "void",
    "null",
    "undefined",
    "true",
    "false",
    "this",
    "interface",
    "type",
    "enum",
];

fn ident(s: &str) -> Option<String> {
    let s = s.trim();
    let ok = !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !s.starts_with(|c: char| c.is_ascii_digit());
    ok.then(|| s.to_string())
}

/// `a, b as c, type d, default as e` → `[(a,a), (b,c), (d,d), (default,e)]`.
fn as_list(list: &str) -> Vec<(String, String)> {
    list.split(',')
        .filter_map(|item| {
            let item = item.trim();
            let item = item.strip_prefix("type ").unwrap_or(item).trim();
            let (a, b) = match item.split_once(" as ") {
                Some((a, b)) => (a, b),
                None => (item, item),
            };
            let a = a.trim().trim_matches(|c| c == '"' || c == '\'');
            Some((ident(a)?, ident(b)?))
        })
        .collect()
}

/// `a, b: c, d = 1` (destructuring) → `[(a,a), (b,c), (d,d)]`.
fn colon_list(list: &str) -> Vec<(String, String)> {
    list.split(',')
        .filter_map(|item| {
            let item = item.split('=').next()?.trim();
            if item.starts_with("...") {
                return None;
            }
            let (a, b) = match item.split_once(':') {
                Some((a, b)) => (a, b),
                None => (item, item),
            };
            let a = a.trim().trim_matches(|c| c == '"' || c == '\'');
            Some((ident(a)?, ident(b)?))
        })
        .collect()
}

/// Same length as `src`, with comments blanked (strings kept).
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    } else if b[i] == b'\n' && q != b'`' {
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                let end = (i + 2).min(b.len());
                for (k, byte) in out.iter_mut().enumerate().take(end).skip(start) {
                    if b[k] != b'\n' {
                        *byte = b' ';
                    }
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    // Only ASCII bytes were replaced (by ASCII spaces), but a comment may
    // have held multi-byte text: blanking every byte keeps UTF-8 valid.
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

impl Facts {
    fn parse(src: &str) -> Facts {
        static IMPORT: OnceLock<Regex> = OnceLock::new();
        static EXPORT_FROM: OnceLock<Regex> = OnceLock::new();
        static EXPORT_LIST: OnceLock<Regex> = OnceLock::new();
        static EXPORT_DEFAULT: OnceLock<Regex> = OnceLock::new();
        static REQUIRE_ID: OnceLock<Regex> = OnceLock::new();
        static REQUIRE_OBJ: OnceLock<Regex> = OnceLock::new();
        static MODEXP_ID: OnceLock<Regex> = OnceLock::new();
        static MODEXP_OBJ: OnceLock<Regex> = OnceLock::new();
        static EXPORTS_PROP: OnceLock<Regex> = OnceLock::new();
        static MODEXP_REQUIRE: OnceLock<Regex> = OnceLock::new();
        let src = strip_comments(src);
        let mut f = Facts::default();
        for c in re(
            &IMPORT,
            r#"\bimport\s+(?:type\s+)?(?P<clause>[\w$\s,{}*]*?)\s*\bfrom\s*['"](?P<spec>[^'"\n]+)['"]"#,
        )
        .captures_iter(&src)
        {
            let clause = c["clause"].trim();
            let mut bindings = Vec::new();
            let (outside, braces) = match (clause.find('{'), clause.rfind('}')) {
                (Some(a), Some(b)) if a < b => (
                    format!("{} {}", &clause[..a], &clause[b + 1..]),
                    Some(&clause[a + 1..b]),
                ),
                _ => (clause.to_string(), None),
            };
            if let Some(list) = braces {
                bindings.extend(as_list(list));
            }
            for part in outside.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                if let Some(ns) = part.strip_prefix('*') {
                    if let Some(l) = ns.trim().strip_prefix("as").and_then(ident) {
                        bindings.push(("*".to_string(), l));
                    }
                } else if let Some(l) = ident(part) {
                    bindings.push(("default".to_string(), l));
                }
            }
            f.imports.push((c["spec"].to_string(), bindings));
        }
        for c in re(
            &EXPORT_FROM,
            r#"\bexport\s+(?:type\s+)?(?P<clause>\*(?:\s+as\s+[\w$]+)?|\{[^}]*\})\s*from\s*['"](?P<spec>[^'"\n]+)['"]"#,
        )
        .captures_iter(&src)
        {
            let clause = c["clause"].trim();
            let what = if let Some(rest) = clause.strip_prefix('*') {
                match rest.trim().strip_prefix("as").and_then(ident) {
                    Some(ns) => ReExport::StarAs(ns),
                    None => ReExport::Star,
                }
            } else {
                ReExport::Named(as_list(clause.trim_matches(|c| c == '{' || c == '}')))
            };
            f.reexports.push((c["spec"].to_string(), what));
        }
        for c in re(
            &EXPORT_LIST,
            r"\bexport\s+(?:type\s+)?\{(?P<list>[^}]*)\}(?P<from>\s*from\b)?",
        )
        .captures_iter(&src)
        {
            if c.name("from").is_none() {
                f.local_exports.extend(as_list(&c["list"]));
            }
        }
        for c in re(
            &EXPORT_DEFAULT,
            r"\bexport\s+default\s+(?:async\s+)?(?:function\s*\*?\s*|(?:abstract\s+)?class\s+)?(?P<id>[\w$]+)",
        )
        .captures_iter(&src)
        {
            push_default(&mut f, &c["id"]);
        }
        for c in re(
            &REQUIRE_ID,
            r#"\b(?:const|let|var)\s+(?P<id>[\w$]+)\s*=\s*require\(\s*['"](?P<spec>[^'"\n]+)['"]\s*\)"#,
        )
        .captures_iter(&src)
        {
            if let Some(l) = ident(&c["id"]) {
                // Whole module: its `module.exports` (our `default`).
                f.imports.push((
                    c["spec"].to_string(),
                    vec![("default".to_string(), l.clone()), ("*".to_string(), l)],
                ));
            }
        }
        for c in re(
            &REQUIRE_OBJ,
            r#"\b(?:const|let|var)\s*\{(?P<list>[^}]*)\}\s*=\s*require\(\s*['"](?P<spec>[^'"\n]+)['"]\s*\)"#,
        )
        .captures_iter(&src)
        {
            f.imports
                .push((c["spec"].to_string(), colon_list(&c["list"])));
        }
        for c in re(
            &MODEXP_REQUIRE,
            r#"\bmodule\.exports\s*=\s*require\(\s*['"](?P<spec>[^'"\n]+)['"]\s*\)"#,
        )
        .captures_iter(&src)
        {
            f.reexports.push((c["spec"].to_string(), ReExport::Whole));
        }
        for c in re(
            &MODEXP_ID,
            r"\bmodule\.exports\s*=\s*(?P<id>[\w$]+)\s*(?:;|\n|$)",
        )
        .captures_iter(&src)
        {
            push_default(&mut f, &c["id"]);
        }
        for c in re(&MODEXP_OBJ, r"\bmodule\.exports\s*=\s*\{(?P<list>[^}]*)\}").captures_iter(&src)
        {
            // `{ exported: local }`.
            for (exported, local) in colon_list(&c["list"]) {
                f.local_exports.push((local, exported));
            }
        }
        for c in re(
            &EXPORTS_PROP,
            r"\b(?:module\.)?exports\.(?P<exp>[\w$]+)\s*=\s*(?P<id>[\w$]+)",
        )
        .captures_iter(&src)
        {
            if !KEYWORDS.contains(&&c["id"]) && &c["id"] != "exports" && &c["id"] != "module" {
                f.local_exports
                    .push((c["id"].to_string(), c["exp"].to_string()));
            }
        }
        f
    }
}

fn push_default(f: &mut Facts, id: &str) {
    if !KEYWORDS.contains(&id) {
        if let Some(id) = ident(id) {
            f.local_exports.push((id, "default".to_string()));
        }
    }
}

// ---------------------------------------------------- spec → import edge

fn is_relative(spec: &str) -> bool {
    spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../")
}

const EXTS: &[&str] = &[
    ".d.ts", ".tsx", ".ts", ".mts", ".cts", ".jsx", ".js", ".mjs", ".cjs", ".vue",
];

fn strip_ext(p: &str) -> &str {
    for e in EXTS {
        if let Some(s) = p.strip_suffix(e) {
            return s;
        }
    }
    p
}

/// `a/b/../c/./d` → `a/c/d` (`None` above the root).
fn normalize(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Module key of a file path: without extension and without `/index`.
fn keys(path: &str) -> (String, String) {
    let stem = strip_ext(path).to_string();
    let no_index = if stem == "index" {
        String::new() // the repo root's `index.js` (`require('..')`)
    } else {
        stem.strip_suffix("/index")
            .map(str::to_string)
            .unwrap_or_else(|| stem.clone())
    };
    (stem, no_index)
}

/// The importer's edge target that `spec` names, by path shape. `None`
/// when no target fits (a bare package, an alias the shape test misses):
/// callers then treat the statement as possibly ours.
fn spec_target(importer: &str, spec: &str, targets: &[(u32, String)]) -> Option<u32> {
    let spec = spec.split(['?', '#']).next().unwrap_or(spec);
    if is_relative(spec) {
        let want = normalize(&format!("{}/{}", dir_of(importer), strip_ext(spec)))?;
        return targets
            .iter()
            .find(|(_, p)| {
                let (stem, no_index) = keys(p);
                stem == want || no_index == want
            })
            .map(|(t, _)| *t);
    }
    let find = |tail: &str| {
        let tail = strip_ext(tail).trim_end_matches('/');
        if tail.is_empty() {
            return None;
        }
        let suffix = format!("/{tail}");
        targets
            .iter()
            .filter(|(_, p)| {
                let (stem, no_index) = keys(p);
                stem == tail
                    || no_index == tail
                    || stem.ends_with(&suffix)
                    || no_index.ends_with(&suffix)
            })
            .min_by_key(|(_, p)| p.len())
            .map(|(t, _)| *t)
    };
    // Path aliases (`@/x`, `~/x`, `#/x`): the rest is a source path.
    let alias = spec
        .strip_prefix("@/")
        .or_else(|| spec.strip_prefix("~/"))
        .or_else(|| spec.strip_prefix("#/"))
        .or_else(|| spec.strip_prefix('~'));
    if let Some(rest) = alias {
        return find(rest);
    }
    // A package spec (`pkg`, `pkg/sub`, `@scope/pkg/sub`): a single bare
    // segment (`aws-lambda`) never matches a directory by suffix; a
    // multi-segment one may be a baseUrl path, else its subpath inside the
    // (workspace) package is tried (`@trpc/server/adapters/x`).
    let segs: Vec<&str> = spec.split('/').collect();
    let pkg_len = if spec.starts_with('@') { 2 } else { 1 };
    if segs.len() > pkg_len {
        if !spec.starts_with('@') {
            if let Some(t) = find(spec) {
                return Some(t);
            }
        }
        return find(&segs[pkg_len..].join("/"));
    }
    None
}

/// A relative spec with no matching edge can still be `module` (an
/// unusual extension); only reject it when its path clearly differs.
fn relative_could_be(importer: &str, spec: &str, module: &str) -> bool {
    let Some(want) = normalize(&format!("{}/{}", dir_of(importer), strip_ext(spec))) else {
        return false;
    };
    let (stem, no_index) = keys(module);
    stem.starts_with(&want) || no_index.starts_with(&want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imports_reexports_and_local_exports() {
        let f = Facts::parse(
            "import Def, { a, b as c, type d } from './m';\n\
             import * as ns from '@/x/y';\n\
             import {\n  e,\n  f as g,\n} from \"../z\";\n\
             // import { nope } from './commented'\n\
             export * from './star';\n\
             export * as space from './ns';\n\
             export { h, i as j, default as K } from './named';\n\
             export { c as C2, Def };\n\
             export default Def;\n\
             const R = require('./req');\n\
             const { p, q: Q } = require('./obj');\n\
             module.exports = R;\n\
             exports.Exp = p;\n\
             module.exports = require('./whole');\n",
        );
        let imports: HashMap<&str, &Vec<(String, String)>> =
            f.imports.iter().map(|(s, b)| (s.as_str(), b)).collect();
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            imports["./m"],
            &vec![
                pair("a", "a"),
                pair("b", "c"),
                pair("d", "d"),
                pair("default", "Def")
            ]
        );
        assert_eq!(imports["@/x/y"], &vec![pair("*", "ns")]);
        assert_eq!(imports["../z"], &vec![pair("e", "e"), pair("f", "g")]);
        assert!(!imports.contains_key("./commented"));
        assert_eq!(
            imports["./req"],
            &vec![pair("default", "R"), pair("*", "R")]
        );
        assert_eq!(imports["./obj"], &vec![pair("p", "p"), pair("q", "Q")]);
        assert_eq!(
            f.reexports,
            vec![
                ("./star".to_string(), ReExport::Star),
                ("./ns".to_string(), ReExport::StarAs("space".to_string())),
                (
                    "./named".to_string(),
                    ReExport::Named(vec![pair("h", "h"), pair("i", "j"), pair("default", "K")])
                ),
                ("./whole".to_string(), ReExport::Whole),
            ]
        );
        for want in [
            pair("c", "C2"),
            pair("Def", "Def"),
            pair("Def", "default"),
            pair("R", "default"),
            pair("p", "Exp"),
        ] {
            assert!(
                f.local_exports.contains(&want),
                "{want:?} in {:?}",
                f.local_exports
            );
        }
    }

    #[test]
    fn object_keys_are_not_references() {
        assert!(is_object_key("  smootherstep: smootherstep,", 2));
        assert!(!is_object_key("  smootherstep: smootherstep,", 16));
        assert!(is_object_key("const o = { a: 1, fn: x }", 18));
        assert!(is_object_key("  fn?: string;", 2));
        assert!(!is_object_key("  case fn: break", 7));
        assert!(!is_object_key("cond ? fn : other", 7));
        assert!(!is_object_key("const { fn: f } = require('./x')", 8));
        assert!(!is_object_key("  call(fn)", 7));
    }

    #[test]
    fn spec_matching_by_path_shape() {
        let t = vec![
            (1, "src/a/index.ts".to_string()),
            (2, "src/a/b.tsx".to_string()),
            (3, "src/lib/util.js".to_string()),
        ];
        assert_eq!(spec_target("src/a/x.ts", "./b", &t), Some(2));
        assert_eq!(spec_target("src/a/x.ts", "./b.js", &t), Some(2));
        assert_eq!(spec_target("src/c/x.ts", "../a", &t), Some(1));
        assert_eq!(spec_target("src/c/x.ts", "@/a", &t), Some(1));
        assert_eq!(spec_target("src/c/x.ts", "@/lib/util", &t), Some(3));
        assert_eq!(spec_target("src/c/x.ts", "react", &t), None);
        // A bare single-segment package never matches a directory name.
        let t2 = vec![(
            4,
            "packages/server/src/adapters/aws-lambda/index.ts".to_string(),
        )];
        assert_eq!(spec_target("t/a.ts", "aws-lambda", &t2), None);
        assert_eq!(
            spec_target("t/a.ts", "@trpc/server/adapters/aws-lambda", &t2),
            Some(4)
        );
    }
    fn repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-refs-ts-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    /// `path:line` (1-based) of every hit.
    fn hits(dir: &Path, name: &str, origin: &str) -> Vec<String> {
        let index = crate::index::build(dir).expect("index");
        let r = crate::refs_graph::graph_references(
            &index,
            dir,
            name,
            &astrolabe_core::types::RelPath::new(origin),
        )
        .expect("graph answer");
        let mut out: Vec<String> = r
            .value
            .iter()
            .map(|l| {
                format!(
                    "{}:{}:{}",
                    l.path.as_str(),
                    l.range.start.line + 1,
                    l.range.end.character - l.range.start.character
                )
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn barrel_chains_and_aliases_reach_importers() {
        let dir = repo(&[
            (
                "tsconfig.json",
                r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#,
            ),
            (
                "src/icons/discord.tsx",
                "export function IconDiscord() {\n  return null\n}\n",
            ),
            (
                "src/icons/index.ts",
                "export { IconDiscord } from './discord'\n",
            ),
            ("src/ui/index.ts", "export * from '../icons'\n"),
            (
                "src/pages/a.tsx",
                "import { IconDiscord } from '@/icons'\nexport const A = () => IconDiscord()\n",
            ),
            (
                "src/pages/b.tsx",
                "import { IconDiscord as Disc } from '../ui'\nexport const B = () => Disc()\n",
            ),
            (
                "src/pages/c.ts",
                "import * as ui from '../ui'\nexport const C = ui.IconDiscord\n",
            ),
            // Cycle: two barrels re-exporting each other.
            (
                "src/cyc/one.ts",
                "export * from './two'\nexport * from '../icons'\n",
            ),
            ("src/cyc/two.ts", "export * from './one'\n"),
            (
                "src/pages/d.ts",
                "import { IconDiscord } from '../cyc/two'\nIconDiscord()\n",
            ),
            // Unrelated file with the same word, not importing it.
            (
                "src/other.ts",
                "export const x = 'IconDiscord'\nfunction IconDiscordX() {}\n",
            ),
        ]);
        let got = hits(&dir, "IconDiscord", "src/icons/discord.tsx");
        for must in [
            "src/icons/discord.tsx:1:11",
            "src/icons/index.ts:1:11",
            "src/pages/a.tsx:1:11",
            "src/pages/a.tsx:2:11",
            "src/pages/b.tsx:1:11",
            "src/pages/b.tsx:1:4",
            "src/pages/b.tsx:2:4",
            "src/pages/c.ts:2:11",
            "src/pages/d.ts:1:11",
            "src/pages/d.ts:2:11",
        ] {
            assert!(got.contains(&must.to_string()), "missing {must} in {got:?}");
        }
        assert!(
            !got.iter().any(|h| h.starts_with("src/other.ts")),
            "{got:?}"
        );
        // Batch-6 scope: declaring file and its direct importer only.
        LEGACY.with(|l| l.set(2));
        let old = hits(&dir, "IconDiscord", "src/icons/discord.tsx");
        LEGACY.with(|l| l.set(0));
        assert_eq!(
            old,
            vec!["src/icons/discord.tsx:1:11", "src/icons/index.ts:1:11"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_and_commonjs_renames_are_searched() {
        let dir = repo(&[
            (
                "lib/express.js",
                "exports = module.exports = createApplication;\nfunction createApplication() {\n  return 1\n}\n",
            ),
            (
                "examples/app.js",
                "var express = require('../lib/express');\nvar app = express();\n",
            ),
            ("index.js", "module.exports = require('./lib/express');\n"),
            (
                "test/app.js",
                "var express = require('..');\nvar escape = require('escape-html');\nexpress();\n",
            ),
            (
                "lib/card.tsx",
                "function MyCard() { return null }\nexport default MyCard\n",
            ),
            ("lib/index.ts", "export { default as Card } from './card'\n"),
            ("app/page.tsx", "import Card from '../lib/card'\nconst c = Card()\n"),
            ("app/list.tsx", "import { Card as C } from '../lib'\nconst d = C()\n"),
        ]);
        let got = hits(&dir, "createApplication", "lib/express.js");
        for must in [
            "examples/app.js:1:7",
            "examples/app.js:2:7",
            "lib/express.js:2:17",
            "test/app.js:1:7",
            "test/app.js:3:7",
        ] {
            assert!(got.contains(&must.to_string()), "missing {must} in {got:?}");
        }
        // `require('escape-html')` is not ours: another statement matched.
        assert!(!got.contains(&"test/app.js:2:6".to_string()), "{got:?}");
        let got = hits(&dir, "MyCard", "lib/card.tsx");
        for must in [
            "app/page.tsx:1:4",
            "app/page.tsx:2:4",
            "lib/index.ts:1:4",
            "app/list.tsx:1:4",
            "app/list.tsx:1:1",
            "app/list.tsx:2:1",
        ] {
            assert!(got.contains(&must.to_string()), "missing {must} in {got:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn member_access_counts_only_through_a_module_object() {
        let dir = repo(&[
            (
                "lib/app.js",
                "var app = exports = module.exports = {};\napp.handle = function handle(req) {\n  this.handle(req)\n  this.router.handle(req)\n};\nexports.helper = function helper() {}\n",
            ),
            (
                "lib/use.js",
                "var a = require('./app');\na.helper();\nother.helper();\nimport * as ns from './app'\nns.helper()\n",
            ),
        ]);
        // Declared by property assignment: `app.handle` and `this.handle`
        // are it, the function expression's own name is not.
        let got = hits(&dir, "handle", "lib/app.js");
        assert_eq!(got, vec!["lib/app.js:2:6", "lib/app.js:3:6"], "{got:?}");
        let got = hits(&dir, "helper", "lib/app.js");
        assert_eq!(
            got,
            vec![
                "lib/app.js:6:6",
                "lib/use.js:2:6",
                "lib/use.js:3:6",
                "lib/use.js:5:6"
            ],
            "{got:?}"
        );
        // A free function: only `ns.f` through a module object counts.
        let dir2 = repo(&[
            ("lib/f.js", "function route(p) {\n  return p\n}\nmodule.exports = { route };\n"),
            (
                "lib/g.js",
                "var f = require('./f');\nf.route(1);\nrouter.route(2);\nconst { route } = require('./f');\nroute(3);\n",
            ),
        ]);
        let got = hits(&dir2, "route", "lib/f.js");
        assert_eq!(
            got,
            vec![
                "lib/f.js:1:5",
                "lib/f.js:4:5",
                "lib/g.js:2:5",
                "lib/g.js:4:5",
                "lib/g.js:5:5"
            ],
            "{got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn namespace_members_and_bare_package_specs() {
        let dir = repo(&[
            (
                "src/rpc.ts",
                "export namespace JSONRPC2 {\n  export type RequestId = number\n}\nexport type Env = { id: JSONRPC2.RequestId }\n",
            ),
            (
                "src/use.ts",
                "import type { JSONRPC2 } from './rpc'\nconst x: JSONRPC2.RequestId = 1\nconst y = other.RequestId\n",
            ),
        ]);
        let got = hits(&dir, "RequestId", "src/rpc.ts");
        assert_eq!(
            got,
            vec!["src/rpc.ts:2:9", "src/rpc.ts:4:9", "src/use.ts:2:9"],
            "{got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graph_first_gate_only_for_unique_top_level_ts() {
        let dir = repo(&[
            (
                "src/a.ts",
                "export function solo() {}\nexport class Box {\n  open() {}\n}\nexport function twice() {}\nexport interface Shape { w: number }\n",
            ),
            ("src/b.ts", "export function twice() {}\n"),
            ("lib/c.js", "function jsOnly() {}\nmodule.exports = jsOnly;\n"),
        ]);
        let index = crate::index::build(&dir).expect("index");
        let ts = Language::TypeScript;
        assert!(ts_graph_first(&index, ts, "solo"));
        assert!(ts_graph_first(&index, ts, "Box"));
        assert!(ts_graph_first(&index, ts, "Shape"));
        assert!(ts_graph_first(&index, Language::Tsx, "solo"));
        // Several definitions, a method, a field, JavaScript: server first.
        assert!(!ts_graph_first(&index, ts, "twice"));
        assert!(!ts_graph_first(&index, ts, "open"));
        assert!(!ts_graph_first(&index, ts, "w"));
        assert!(!ts_graph_first(&index, Language::JavaScript, "jsOnly"));
        assert!(!ts_graph_first(&index, ts, "jsOnly"));
        assert!(!ts_graph_first(&index, ts, "missing"));
        // The switch.
        std::env::set_var(TS_GRAPH_FIRST_ENV, "off");
        let off = ts_graph_first(&index, ts, "solo");
        std::env::remove_var(TS_GRAPH_FIRST_ENV);
        assert!(!off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deep_reexport_chain_marks_the_scope_truncated() {
        let mut files: Vec<(String, String)> =
            vec![("src/l0.ts".into(), "export function deep() {}\n".into())];
        for i in 1..=MAX_DEPTH + 2 {
            files.push((
                format!("src/l{i}.ts"),
                format!("export * from './l{}'\n", i - 1),
            ));
        }
        files.push((
            "src/use.ts".into(),
            format!("import {{ deep }} from './l{}'\ndeep()\n", MAX_DEPTH + 2),
        ));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let dir = repo(&refs);
        let index = crate::index::build(&dir).expect("index");
        let def = index.file_id("src/l0.ts").unwrap();
        assert!(ts_scope(&index, &dir, def, 1, "deep").truncated);
        let r = crate::refs_graph::graph_references(
            &index,
            &dir,
            "deep",
            &astrolabe_core::types::RelPath::new("src/l0.ts"),
        )
        .expect("answer");
        assert!(r.note.unwrap().contains("可能不全"));
        // A short chain is complete.
        let dir2 = repo(&[
            ("src/l0.ts", "export function deep() {}\n"),
            ("src/l1.ts", "export * from './l0'\n"),
            ("src/use.ts", "import { deep } from './l1'\ndeep()\n"),
        ]);
        let index2 = crate::index::build(&dir2).expect("index");
        let def2 = index2.file_id("src/l0.ts").unwrap();
        assert!(!ts_scope(&index2, &dir2, def2, 1, "deep").truncated);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Measurement dump (batch 7): for `TS_RECALL_REPO` and either
    /// `TS_RECALL_CASES="path:sym,…"` or `TS_RECALL_SAMPLE="<ext|ext>:<n>"`
    /// (same pool rule as `live_backend::recall_harness`; `TS_RECALL_KINDS`
    /// overrides the kinds, e.g. `Method,Field`), write one JSON
    /// line per case to `TS_RECALL_OUT` with the declaration position and
    /// the index answer before (batch-6 scope) and after this module. The
    /// reference side is computed separately (TypeScript LanguageService).
    #[test]
    #[ignore = "needs a corpus (TS_RECALL_REPO)"]
    fn dump_graph_answers() {
        let Some(root) = std::env::var_os("TS_RECALL_REPO").map(std::path::PathBuf::from) else {
            return;
        };
        let out = std::env::var("TS_RECALL_OUT").expect("TS_RECALL_OUT");
        let index = crate::index::build(&root).expect("index");
        let g = index.graph();
        let mut cases: Vec<(String, String)> = Vec::new();
        if let Ok(list) = std::env::var("TS_RECALL_CASES") {
            for c in list.split(',').filter(|c| !c.is_empty()) {
                let (p, s) = c.rsplit_once(':').expect("path:sym");
                cases.push((p.to_string(), s.to_string()));
            }
        }
        if let Some(spec) = std::env::var("TS_RECALL_SAMPLE")
            .ok()
            .filter(|s| !s.is_empty())
        {
            let (exts, n) = spec.rsplit_once(':').expect("ext:n");
            let n: usize = n.parse().unwrap();
            let exts: Vec<&str> = exts.split('|').collect();
            // Optional kind filter, e.g. `TS_RECALL_KINDS=Method,Field`.
            let kinds: Option<Vec<String>> = std::env::var("TS_RECALL_KINDS")
                .ok()
                .filter(|k| !k.is_empty())
                .map(|k| k.split(',').map(str::to_string).collect());
            let mut count: HashMap<&str, usize> = HashMap::new();
            for s in &g.symbols {
                *count.entry(s.name.as_str()).or_default() += 1;
            }
            let mut pool: Vec<(String, String)> = Vec::new();
            for s in &g.symbols {
                let Some(f) = index.file(s.file) else {
                    continue;
                };
                let p = f.path.as_str();
                if !exts.iter().any(|e| p.ends_with(e))
                    || crate::explain::is_test_caller(p, &s.name)
                    || count[s.name.as_str()] != 1
                    || s.name.len() < 4
                    || !match &kinds {
                        Some(k) => k.contains(&format!("{:?}", s.kind)),
                        None => matches!(
                            s.kind,
                            astrolabe_core::SymbolKind::Function
                                | astrolabe_core::SymbolKind::Method
                                | astrolabe_core::SymbolKind::Class
                                | astrolabe_core::SymbolKind::Interface
                                | astrolabe_core::SymbolKind::Struct
                                | astrolabe_core::SymbolKind::Type
                        ),
                    }
                {
                    continue;
                }
                pool.push((p.to_string(), s.name.clone()));
            }
            pool.sort();
            let step = (pool.len() / n.max(1)).max(1);
            cases.extend(pool.into_iter().step_by(step).take(n));
        }
        let mut lines_out = Vec::new();
        for (path, sym) in &cases {
            let rel = astrolabe_core::types::RelPath::new(path);
            let decl = g.symbols.iter().find(|s| {
                s.name == *sym && index.file(s.file).is_some_and(|f| f.path.as_str() == path)
            });
            let Some(decl) = decl else { continue };
            let src = astrolabe_core::text::read_source(&root.join(path)).unwrap_or_default();
            let text_lines: Vec<&str> = src.lines().collect();
            // Declaration name position: first occurrence from start_line.
            let mut pos = None;
            for l in decl.start_line.saturating_sub(1)..decl.end_line.max(decl.start_line) {
                if let Some(line) = text_lines.get(l as usize) {
                    if let Some(&b) = crate::refs_graph::word_offsets(line, sym).first() {
                        pos = Some((l, line[..b].encode_utf16().count()));
                        break;
                    }
                }
            }
            let Some((line0, col)) = pos else { continue };
            let run = |legacy: u8| {
                LEGACY.with(|l| l.set(legacy));
                let t = std::time::Instant::now();
                let r = crate::refs_graph::graph_references(&index, &root, sym, &rel);
                let ms = t.elapsed().as_millis();
                LEGACY.with(|l| l.set(0));
                let hits: Vec<serde_json::Value> = r
                    .map(|r| {
                        r.value
                            .iter()
                            .map(|l| {
                                serde_json::json!([
                                    l.path.as_str(),
                                    l.range.start.line,
                                    l.range.start.character
                                ])
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (hits, ms)
            };
            let (before, _) = run(2);
            let (mid, _) = run(1);
            let (after, ms) = run(0);
            lines_out.push(
                serde_json::json!({
                    "path": path, "sym": sym, "kind": format!("{:?}", decl.kind),
                    "line": line0, "col": col,
                    "before": before, "mid": mid, "after": after, "after_ms": ms,
                })
                .to_string(),
            );
        }
        std::fs::write(&out, lines_out.join("\n") + "\n").unwrap();
        println!("DUMPED {} cases to {out}", lines_out.len());
    }
}
