//! TypeScript / JavaScript / TSX module resolution.
//!
//! Target: hold 100% on the 211 in-repo imports in the `openvisio-oss` corpus.
//! (An earlier draft said 205, before the denominator was rebuilt to mask
//! comments and string literals.)
//! This is the one language the prior art already got right, because relative
//! specifiers are literally file paths. The job here is to match that and then
//! cover what it missed.
//!
//! Must handle: relative specifiers with extension inference
//! (`.ts`/`.tsx`/`.js`/`.mjs`/`.cjs` and `index.*` in a directory), the
//! ESM-style `./x.js` specifier that actually refers to `x.ts`, `tsconfig.json`
//! `compilerOptions.paths` + `baseUrl` including the `*` wildcard, nearest
//! enclosing `tsconfig.json`/`jsconfig.json` per directory, `extends` chains
//! between tsconfigs, monorepo workspace packages (resolve `@scope/pkg` to the
//! sibling package's entry point via its `package.json`), and JSONC comments
//! in tsconfig files.
//!
//! Returning `None` for bare `node_modules` specifiers is correct unless they
//! resolve to a workspace package inside the repo.
//!
//! # How configuration is encoded in [`ProjectMeta`]
//!
//! `detect` is the only phase allowed to read disk, so everything `resolve`
//! needs is flattened into `ProjectMeta::modules`:
//!
//! * **Workspace packages** are plain units: `name` is the `package.json`
//!   `name`, `dir` is the package directory. `ProjectMeta::module_for` works
//!   on these unchanged.
//! * **Aliases** (tsconfig `paths` and `baseUrl`, `package.json` `exports` and
//!   `imports`) are units whose `name` is `"<scope>::<pattern>"`. `<scope>` is
//!   the directory the rule applies to (`""` = the whole repo), `<pattern>` is
//!   the import-side pattern with at most one `*`, and `dir` is the
//!   repo-relative target, also with at most one `*`. [`alias_units`] decodes
//!   them. A file consults the aliases of its nearest enclosing scope first,
//!   then each ancestor scope, then the repo-wide scope.
//!
//! `source_roots` lists every tsconfig and package directory. `excludes` lists
//! declared `outDir`s plus tsconfig `exclude` entries that name build output.
//!
//! # JSONC
//!
//! `tsconfig.json` allows `//` and `/* */` comments and trailing commas, which
//! `serde_json` rejects. [`strip_jsonc`] removes both (string-literal aware)
//! before parsing; the same routine is used for `package.json` so a stray
//! comment there does not cost a workspace package.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;

use crate::types::{
    join_rel, FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath,
};

pub struct TypeScriptResolver;

/// Separator between the scope directory and the pattern in alias unit names.
pub const ALIAS_SCOPE_SEP: &str = "::";

/// Extensions appended to an extension-less specifier, in TypeScript's own
/// preference order (TS sources, declarations, then JS, then JSON).
const IMPLICIT_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "d.ts", "mts", "d.mts", "cts", "d.cts", "js", "jsx", "mjs", "cjs", "json",
];

/// Build-output directory names that may be remapped to `src` when a
/// `package.json` entry or alias target points at compiled output that is not
/// part of the index.
const OUTPUT_DIRS: &[&str] = &[
    "dist", "build", "out", "output", "lib", "esm", "cjs", "umd", "es", "types", "typings",
];

/// The unambiguous subset of [`OUTPUT_DIRS`] used when remapping *relative*
/// specifiers, where a wrong guess is worse than `None`.
const OUTPUT_DIRS_STRICT: &[&str] = &["dist", "build", "out", "output"];

/// Entry points tried when `package.json` does not name a usable one.
const ENTRY_FALLBACKS: &[&str] = &["src/index", "index", "src/main", "main", "lib/index"];

/// Preferred order for `exports` condition keys.
const EXPORT_CONDITION_ORDER: &[&str] = &[
    "import", "module", "default", "node", "browser", "require", "types",
];

/// tsconfig `exclude` entries whose last segment is one of these are treated
/// as declared build output and surfaced in `ProjectMeta::excludes`.
const OUTPUT_EXCLUDE_NAMES: &[&str] = &[
    "dist", "build", "out", "output", "coverage", ".next", ".nuxt", ".turbo", ".cache", "tmp",
    "temp",
];

// ------------------------------------------------------------------ aliases

/// One alias rule decoded from a [`ModuleUnit`] produced by this resolver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alias<'a> {
    /// Directory the rule applies to; `""` means the whole repo.
    pub scope: &'a str,
    /// Import-side pattern, with at most one `*`.
    pub pattern: &'a str,
    /// Repo-relative target, with at most one `*`.
    pub target: &'a str,
}

/// Iterate the alias rules stored in `meta.modules`.
pub fn alias_units(meta: &ProjectMeta) -> impl Iterator<Item = Alias<'_>> {
    meta.modules.iter().filter_map(alias_at)
}

fn alias_at(u: &ModuleUnit) -> Option<Alias<'_>> {
    let (scope, pattern) = u.name.split_once(ALIAS_SCOPE_SEP)?;
    Some(Alias {
        scope,
        pattern,
        target: &u.dir,
    })
}

fn alias_unit(scope: &str, pattern: &str, target: &str) -> ModuleUnit {
    ModuleUnit {
        name: format!("{scope}{ALIAS_SCOPE_SEP}{pattern}"),
        dir: target.to_string(),
    }
}

// ----------------------------------------------------------------- resolver

impl ModuleResolver for TypeScriptResolver {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let packages = collect_packages(files);
        let configs = collect_configs(files, &packages);
        let authored = authored_ts_dirs(files);

        let mut meta = ProjectMeta::default();
        let mut roots = BTreeSet::new();
        let mut excludes = BTreeSet::new();

        for cfg in &configs {
            roots.insert(cfg.dir.clone());
            if let Some((paths_dir, raw)) = &cfg.paths {
                // `paths` are relative to `baseUrl` when set (wherever it was
                // inherited from), otherwise to the config that defines them.
                let base = cfg.base_url.as_deref().unwrap_or(paths_dir);
                for (pattern, targets) in raw {
                    for target in targets {
                        let target = expand_config_dir(target);
                        if let Some(abs) = join_rel(base, &target) {
                            meta.modules.push(alias_unit(&cfg.dir, pattern, &abs));
                        }
                    }
                }
            }
            if let Some(base) = &cfg.base_url {
                // Non-relative names may be resolved against `baseUrl`. The
                // empty prefix sorts this rule after every explicit `paths`.
                meta.modules
                    .push(alias_unit(&cfg.dir, "*", &join_dir(base, "*")));
            }
            if let Some(out) = &cfg.out_dir {
                if out_dir_is_excludable(cfg, out, &authored) {
                    excludes.insert(out.clone());
                }
            }
            for e in &cfg.exclude {
                if is_output_dir_path(e) && !dir_has_authored_ts(&authored, e) {
                    excludes.insert(e.clone());
                }
            }
        }

        for pkg in &packages {
            roots.insert(pkg.dir.clone());
            if let Some(entry) = &pkg.entry {
                meta.modules.push(alias_unit("", &pkg.name, entry));
            }
            for (sub, target) in &pkg.subpaths {
                meta.modules
                    .push(alias_unit("", &format!("{}/{sub}", pkg.name), target));
            }
            for (pattern, target) in &pkg.imports {
                meta.modules.push(alias_unit(&pkg.dir, pattern, target));
            }
            meta.modules.push(ModuleUnit {
                name: pkg.name.clone(),
                dir: pkg.dir.clone(),
            });
        }

        meta.source_roots = roots.into_iter().collect();
        meta.excludes = excludes.into_iter().collect();
        for (i, u) in meta.modules.iter().enumerate() {
            if let Some((scope, _)) = u.name.split_once(ALIAS_SCOPE_SEP) {
                meta.scope_index
                    .0
                    .entry(scope.to_string())
                    .or_default()
                    .push(i);
            }
        }
        meta
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let spec = clean_spec(spec)?;
        let spec = spec.as_ref();
        if is_relative(spec) {
            return resolve_relative(from, spec, files);
        }
        if let Some(rooted) = spec.strip_prefix('/') {
            return resolve_rooted(from, rooted, files, meta);
        }
        resolve_bare(from, spec, files, meta)
    }
}

// --------------------------------------------------------- specifier shapes

/// Trim, drop a bundler query suffix (`./worker.js?worker`), normalise
/// backslashes. `None` for an empty specifier.
fn clean_spec(spec: &str) -> Option<Cow<'_, str>> {
    let s = spec.trim();
    let s = match s.find('?') {
        Some(i) => &s[..i],
        None => s,
    };
    if s.is_empty() {
        return None;
    }
    Some(if s.contains('\\') {
        Cow::Owned(s.replace('\\', "/"))
    } else {
        Cow::Borrowed(s)
    })
}

fn is_relative(spec: &str) -> bool {
    spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../")
}

fn resolve_relative(from: &RelPath, spec: &str, files: &FileIndex) -> Option<RelPath> {
    let joined = join_rel(from.dir(), spec)?;
    // A trailing slash means "this directory", never a file.
    let target = if spec.ends_with('/') && !joined.is_empty() {
        Cow::Owned(format!("{joined}/"))
    } else {
        Cow::Borrowed(joined.as_str())
    };
    resolve_with_output_remap(&target, files, OUTPUT_DIRS_STRICT)
}

/// `/src/x` — Vite-style project-root-absolute. Try the nearest enclosing
/// project (tsconfig or package directory), then the repo root.
fn resolve_rooted(
    from: &RelPath,
    rooted: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    let dir = from.dir();
    let nearest = meta
        .source_roots
        .iter()
        .filter(|r| is_ancestor(r, dir))
        .max_by_key(|r| r.len());
    if let Some(root) = nearest {
        if let Some(p) = join_rel(root, rooted) {
            if let Some(hit) = resolve_file(&p, files) {
                return Some(hit);
            }
        }
    }
    resolve_file(rooted, files)
}

fn resolve_bare(
    from: &RelPath,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    if spec.starts_with("node:") {
        return None;
    }

    // Aliases: nearest scope first, walking up to the repo-wide scope.
    let mut scope = Some(from.dir());
    while let Some(s) = scope {
        if let Some(hit) = resolve_aliases_in_scope(s, spec, files, meta) {
            return Some(hit);
        }
        scope = parent_dir(s);
    }

    // Workspace package without a usable `exports`/`main`, or a deep import
    // into one (`@scope/pkg/src/x.js`).
    let unit = meta
        .module_for(spec)
        .filter(|u| !u.name.contains(ALIAS_SCOPE_SEP))?;
    let sub = spec[unit.name.len()..].trim_start_matches('/');
    if sub.is_empty() {
        ENTRY_FALLBACKS
            .iter()
            .find_map(|f| resolve_file(&join_dir(&unit.dir, f), files))
    } else {
        let direct = join_rel(&unit.dir, sub)?;
        resolve_with_output_remap(&direct, files, OUTPUT_DIRS).or_else(|| {
            join_rel(&join_dir(&unit.dir, "src"), sub)
                .and_then(|p| resolve_with_output_remap(&p, files, OUTPUT_DIRS))
        })
    }
}

struct PatternMatch<'a> {
    exact: bool,
    prefix_len: usize,
    captured: Option<&'a str>,
}

/// TypeScript `paths` matching: an exact key, or one `*` capturing the middle.
fn match_pattern<'a>(pattern: &str, spec: &'a str) -> Option<PatternMatch<'a>> {
    match pattern.split_once('*') {
        None => (pattern == spec).then_some(PatternMatch {
            exact: true,
            prefix_len: pattern.len(),
            captured: None,
        }),
        Some((pre, suf)) => {
            if spec.len() < pre.len() + suf.len() || !spec.starts_with(pre) || !spec.ends_with(suf)
            {
                return None;
            }
            Some(PatternMatch {
                exact: false,
                prefix_len: pre.len(),
                captured: Some(&spec[pre.len()..spec.len() - suf.len()]),
            })
        }
    }
}

struct AliasHit {
    exact: bool,
    prefix_len: usize,
    order: usize,
    target: String,
}

fn resolve_aliases_in_scope(
    scope: &str,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    let mut hits: Vec<AliasHit> = Vec::new();
    let mut visit = |order: usize, alias: Alias<'_>| {
        if alias.scope != scope {
            return;
        }
        let Some(m) = match_pattern(alias.pattern, spec) else {
            return;
        };
        let target = match m.captured {
            Some(c) => alias.target.replacen('*', c, 1),
            None => alias.target.to_string(),
        };
        hits.push(AliasHit {
            exact: m.exact,
            prefix_len: m.prefix_len,
            order,
            target,
        });
    };
    if meta.scope_index.0.is_empty() {
        for (order, alias) in alias_units(meta).enumerate() {
            visit(order, alias);
        }
    } else if let Some(idx) = meta.scope_index.0.get(scope) {
        for &i in idx {
            if let Some(alias) = alias_at(&meta.modules[i]) {
                visit(i, alias);
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    // TypeScript prefers the exact key, then the longest literal prefix; we
    // keep declaration order as the tie-break and fall through on a miss.
    hits.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then(b.prefix_len.cmp(&a.prefix_len))
            .then(a.order.cmp(&b.order))
    });
    hits.iter()
        .find_map(|h| resolve_with_output_remap(&h.target, files, OUTPUT_DIRS))
}

// ------------------------------------------------------- file-level lookups

/// Resolve a repo-relative path the way TypeScript would: ESM `.js` → TS
/// source, exact file, implicit extensions, then `index.*` in a directory.
fn resolve_file(path: &str, files: &FileIndex) -> Option<RelPath> {
    let trimmed = path.trim_end_matches('/');
    let dir_only = trimmed.len() != path.len();
    if !dir_only {
        if let Some(hit) = resolve_as_file(trimmed, files) {
            return Some(hit);
        }
    }
    resolve_dir_index(trimmed, files)
}

fn resolve_as_file(path: &str, files: &FileIndex) -> Option<RelPath> {
    if path.is_empty() {
        return None;
    }
    // `./x.js` in TS source means `x.ts`; TypeScript tries the source
    // extension before the literal one, so do the same.
    if let Some((stem, ext)) = split_ext(path) {
        for swap in ts_source_swaps(ext) {
            let cand = format!("{stem}.{swap}");
            if files.contains(&cand) {
                return Some(RelPath::new(cand));
            }
        }
    }
    if let Some((stem, swaps)) = declaration_swaps(path) {
        for swap in swaps {
            let cand = format!("{stem}.{swap}");
            if files.contains(&cand) {
                return Some(RelPath::new(cand));
            }
        }
    }
    if files.contains(path) {
        return Some(RelPath::new(path));
    }
    for ext in IMPLICIT_EXTENSIONS {
        let cand = format!("{path}.{ext}");
        if files.contains(&cand) {
            return Some(RelPath::new(cand));
        }
    }
    None
}

fn resolve_dir_index(dir: &str, files: &FileIndex) -> Option<RelPath> {
    if !files.has_dir(dir) {
        return None;
    }
    for ext in IMPLICIT_EXTENSIONS {
        let cand = join_dir(dir, &format!("index.{ext}"));
        if files.contains(&cand) {
            return Some(RelPath::new(cand));
        }
    }
    None
}

/// [`resolve_file`], then — if the path runs through a build-output directory
/// that is not indexed — the same path with that directory replaced by `src`
/// or dropped (`pkg/dist/index.js` → `pkg/src/index.ts`, `pkg/index.ts`).
fn resolve_with_output_remap(
    path: &str,
    files: &FileIndex,
    output_dirs: &[&str],
) -> Option<RelPath> {
    if let Some(hit) = resolve_file(path, files) {
        return Some(hit);
    }
    let trailing_slash = path.ends_with('/');
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let i = segs.iter().position(|s| output_dirs.contains(s))?;
    // Swallow a run of output dirs (`dist/esm/`) but never the final segment.
    let mut j = i + 1;
    while j + 1 < segs.len() && output_dirs.contains(&segs[j]) {
        j += 1;
    }
    let mut tails = vec![j];
    if i + 1 != j {
        tails.push(i + 1);
    }
    for tail in tails {
        for repl in [Some("src"), None] {
            let mut parts: Vec<&str> = segs[..i].to_vec();
            if let Some(r) = repl {
                parts.push(r);
            }
            parts.extend_from_slice(&segs[tail..]);
            let mut cand = parts.join("/");
            if trailing_slash && !cand.is_empty() {
                cand.push('/');
            }
            if let Some(hit) = resolve_file(&cand, files) {
                return Some(hit);
            }
        }
    }
    None
}

/// TS source extensions that an emitted-JS extension may stand for.
fn ts_source_swaps(ext: &str) -> &'static [&'static str] {
    match ext {
        "js" => &["ts", "tsx", "d.ts"],
        "jsx" => &["tsx", "ts"],
        "mjs" => &["mts", "d.mts"],
        "cjs" => &["cts", "d.cts"],
        _ => &[],
    }
}

/// `x.d.ts` → (`x`, [`ts`, `tsx`]) so a declaration reference can land on
/// the implementation when only that is indexed.
fn declaration_swaps(path: &str) -> Option<(&str, &'static [&'static str])> {
    if let Some(stem) = path.strip_suffix(".d.ts") {
        return Some((stem, &["ts", "tsx"]));
    }
    if let Some(stem) = path.strip_suffix(".d.mts") {
        return Some((stem, &["mts"]));
    }
    if let Some(stem) = path.strip_suffix(".d.cts") {
        return Some((stem, &["cts"]));
    }
    None
}

/// `(stem, ext)` of the last path segment. Dotfiles have no extension.
fn split_ext(path: &str) -> Option<(&str, &str)> {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let name = &path[name_start..];
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some((&path[..name_start + dot], &name[dot + 1..]))
}

fn parent_dir(dir: &str) -> Option<&str> {
    if dir.is_empty() {
        return None;
    }
    Some(match dir.rfind('/') {
        Some(i) => &dir[..i],
        None => "",
    })
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn join_dir(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

fn is_ancestor(root: &str, dir: &str) -> bool {
    root.is_empty()
        || dir == root
        || (dir.len() > root.len() && dir.starts_with(root) && dir.as_bytes()[root.len()] == b'/')
}

fn in_node_modules(path: &str) -> bool {
    path.starts_with("node_modules/") || path.contains("/node_modules/")
}

/// `true` when `dir` is `ancestor` itself or lies below it (`""` = repo root).
fn dir_within(dir: &str, ancestor: &str) -> bool {
    let a = ancestor.trim_end_matches('/');
    let d = dir.trim_end_matches('/');
    a.is_empty() || a == "." || d == a || d.starts_with(&format!("{a}/"))
}

/// Authored TypeScript (not a `.d.ts` declaration emitted next to JS output).
pub fn is_authored_ts(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let is_decl = [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|s| file.ends_with(s))
        || file.contains(".d.");
    !is_decl
        && [".ts", ".tsx", ".mts", ".cts"]
            .iter()
            .any(|s| lower.ends_with(s))
}

/// Does `dir` (or anything below it) contain authored TS sources? A real
/// build directory only holds emitted `.js`/`.d.ts`, so sources there mean
/// the directory is not (only) build output.
fn dir_has_authored_ts(authored: &HashSet<String>, dir: &str) -> bool {
    let d = dir.trim_end_matches('/');
    if d.is_empty() || d == "." {
        return !authored.is_empty();
    }
    authored.contains(d)
}

/// Every directory that is, or contains at any depth, an authored TS file.
/// Computed once per `detect` instead of scanning all files per query.
fn authored_ts_dirs(files: &FileIndex) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for p in files.iter() {
        if !is_authored_ts(p.as_str()) {
            continue;
        }
        let mut dir = p.dir();
        while !dir.is_empty() {
            if !out.insert(dir.to_string()) {
                break;
            }
            dir = parent_dir(dir).unwrap_or("");
        }
        out.insert(String::new());
    }
    out
}

/// `outDir` is only excluded when it is a distinct directory holding no
/// sources. `"outDir": "."` (zod) or an `outDir` that is an ancestor of the
/// config / `rootDir` would otherwise drop the whole package from the index.
fn out_dir_is_excludable(cfg: &TsConfig, out: &str, authored: &HashSet<String>) -> bool {
    if dir_within(&cfg.dir, out) {
        return false;
    }
    if let Some(root) = &cfg.root_dir {
        if dir_within(root, out) {
            return false;
        }
    }
    !dir_has_authored_ts(authored, out)
}

fn is_output_dir_path(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or(path);
    OUTPUT_EXCLUDE_NAMES.contains(&last)
}

/// TS 5.5 `${configDir}` template; we always join against the defining
/// config's directory, so it collapses to `.`.
fn expand_config_dir(s: &str) -> String {
    s.replace("${configDir}", ".")
}

// --------------------------------------------------------------- packages

/// A `package.json` inside the repo (never under `node_modules`).
#[derive(Debug)]
struct Package {
    name: String,
    dir: String,
    /// Resolved entry file for the bare package name, if one was found.
    entry: Option<String>,
    /// `exports` subpaths: `(pattern after "<name>/", repo-relative target)`.
    /// Patterns without `*` are already resolved to an indexed file.
    subpaths: Vec<(String, String)>,
    /// `imports` field: `(#pattern, repo-relative target)`, scoped to `dir`.
    imports: Vec<(String, String)>,
}

fn collect_packages(files: &FileIndex) -> Vec<Package> {
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut pkgs: Vec<Package> = Vec::new();
    let mut parsed: Vec<(String, Value)> = Vec::new();
    for p in files.iter() {
        if p.file_name() != "package.json" || in_node_modules(p.as_str()) {
            continue;
        }
        let Some(text) = files.read(p.as_str()) else {
            continue;
        };
        let Some(json) = parse_jsonc(&text) else {
            tracing::debug!(path = %p, "package.json is not valid JSON(C); ignored");
            continue;
        };
        parsed.push((p.dir().to_string(), json));
    }
    let workspace_globs = workspace_globs(files, &parsed);
    let mut ranks: Vec<(bool, bool, bool, usize)> = Vec::new();
    for (dir, json) in &parsed {
        let Some(name) = json
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let pkg = build_package(name, dir, json, files);
        let rank = package_rank(dir, &workspace_globs);
        match by_name.get(name).copied() {
            // Duplicate names: a workspace-declared member beats an
            // undeclared one; examples/fixtures lose; the monorepo root
            // (often sharing the published member's name) loses to a member;
            // then the shallowest path wins. Order of discovery is irrelevant.
            Some(i) => {
                if rank < ranks[i] {
                    pkgs[i] = pkg;
                    ranks[i] = rank;
                }
            }
            None => {
                by_name.insert(name.to_string(), pkgs.len());
                pkgs.push(pkg);
                ranks.push(rank);
            }
        }
    }
    pkgs
}

/// Lower is better: `(not a declared workspace member, is the repo root,
/// lives under examples/fixtures/…, path length)`.
fn package_rank(dir: &str, globs: &[Vec<String>]) -> (bool, bool, bool, usize) {
    let declared = globs.iter().any(|g| glob_dir_match(g, dir));
    let sample = dir.split('/').any(|seg| {
        matches!(
            seg,
            "examples"
                | "example"
                | "fixtures"
                | "fixture"
                | "test"
                | "tests"
                | "__tests__"
                | "templates"
                | "template"
                | "docs"
                | "playground"
                | "e2e"
                | "demo"
                | "demos"
        )
    });
    (!declared, dir.is_empty(), sample, dir.len())
}

/// Workspace member patterns from `package.json` `workspaces` and
/// `pnpm-workspace.yaml`, already joined onto their declaring directory and
/// split into segments.
fn workspace_globs(files: &FileIndex, parsed: &[(String, Value)]) -> Vec<Vec<String>> {
    let mut raw: Vec<(String, String)> = Vec::new();
    for (dir, json) in parsed {
        let ws = json.get("workspaces");
        let list = match ws {
            Some(Value::Array(a)) => Some(a),
            Some(Value::Object(o)) => o.get("packages").and_then(Value::as_array),
            _ => None,
        };
        for pat in list.into_iter().flatten().filter_map(Value::as_str) {
            raw.push((dir.clone(), pat.to_string()));
        }
    }
    for p in files.iter() {
        if p.file_name() != "pnpm-workspace.yaml" || in_node_modules(p.as_str()) {
            continue;
        }
        let Some(text) = files.read(p.as_str()) else {
            continue;
        };
        let mut in_packages = false;
        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = line.strip_prefix("packages:") {
                in_packages = rest.trim().is_empty();
                continue;
            }
            if !line.starts_with(' ') && !line.starts_with('-') && !t.is_empty() {
                in_packages = false;
            }
            if in_packages {
                if let Some(item) = t.strip_prefix('-') {
                    let item = item.trim().trim_matches(|c| c == '"' || c == '\'');
                    raw.push((p.dir().to_string(), item.to_string()));
                }
            }
        }
    }
    raw.into_iter()
        .filter(|(_, pat)| !pat.starts_with('!'))
        .map(|(base, pat)| {
            let pat = pat.trim_start_matches("./").trim_end_matches('/');
            let mut segs: Vec<String> = base
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            segs.extend(
                pat.split('/')
                    .filter(|s| !s.is_empty() && *s != ".")
                    .map(str::to_string),
            );
            segs
        })
        .collect()
}

fn glob_dir_match(pat: &[String], dir: &str) -> bool {
    let segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    glob_segs(pat, &segs)
}

fn glob_segs(pat: &[String], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((p, rest)) if p == "**" => (0..=segs.len()).any(|i| glob_segs(rest, &segs[i..])),
        Some((p, rest)) => match segs.split_first() {
            Some((s, tail)) => seg_match(p, s) && glob_segs(rest, tail),
            None => false,
        },
    }
}

fn seg_match(pat: &str, seg: &str) -> bool {
    match pat.split_once('*') {
        None => pat == seg,
        Some((pre, post)) => {
            seg.len() >= pre.len() + post.len() && seg.starts_with(pre) && seg.ends_with(post)
        }
    }
}

fn build_package(name: &str, dir: &str, json: &Value, files: &FileIndex) -> Package {
    let exports = json.get("exports");

    let mut entry_candidates: Vec<String> = Vec::new();
    if let Some(root) = exports.and_then(exports_root) {
        entry_candidates.extend(string_leaves(root));
    }
    for key in ["module", "main", "browser", "types", "typings"] {
        if let Some(s) = json.get(key).and_then(Value::as_str) {
            entry_candidates.push(s.to_string());
        }
    }
    let entry = entry_candidates
        .iter()
        .filter_map(|c| join_rel(dir, c))
        .find_map(|p| resolve_with_output_remap(&p, files, OUTPUT_DIRS))
        .or_else(|| {
            ENTRY_FALLBACKS
                .iter()
                .find_map(|f| resolve_file(&join_dir(dir, f), files))
        })
        .map(|r| r.as_str().to_string());

    let mut subpaths = Vec::new();
    if let Some(Value::Object(map)) = exports {
        for (key, value) in map {
            let Some(sub) = key.strip_prefix("./") else {
                continue;
            };
            if sub.is_empty() {
                continue;
            }
            // Legacy folder mapping `"./lib/": "./dist/lib/"` ≡ `./lib/*`.
            let (sub, folder) = match sub.strip_suffix('/') {
                Some(s) => (format!("{s}/*"), true),
                None => (sub.to_string(), false),
            };
            for leaf in string_leaves(value) {
                let leaf = if folder { format!("{leaf}*") } else { leaf };
                let Some(target) = join_rel(dir, &leaf) else {
                    continue;
                };
                if sub.contains('*') {
                    subpaths.push((sub.clone(), target));
                } else if let Some(hit) = resolve_with_output_remap(&target, files, OUTPUT_DIRS) {
                    subpaths.push((sub.clone(), hit.as_str().to_string()));
                    break;
                }
            }
        }
    }

    let mut imports = Vec::new();
    if let Some(Value::Object(map)) = json.get("imports") {
        for (key, value) in map {
            if !key.starts_with('#') {
                continue;
            }
            for leaf in string_leaves(value) {
                if !leaf.starts_with('.') {
                    continue; // maps to another package; not an in-repo file
                }
                if let Some(target) = join_rel(dir, &leaf) {
                    imports.push((key.clone(), target));
                }
            }
        }
    }

    Package {
        name: name.to_string(),
        dir: dir.to_string(),
        entry,
        subpaths,
        imports,
    }
}

/// The value describing the package root: `exports["."]` for a subpath map,
/// otherwise `exports` itself (a string, array, or conditions object).
fn exports_root(exports: &Value) -> Option<&Value> {
    match exports {
        Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => map.get("."),
        _ => Some(exports),
    }
}

/// Every string reachable from a `package.json` target value, conditions
/// visited in [`EXPORT_CONDITION_ORDER`] first.
fn string_leaves(value: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(map) => {
                for key in EXPORT_CONDITION_ORDER {
                    if let Some(v) = map.get(*key) {
                        walk(v, out);
                    }
                }
                for (k, v) in map {
                    if !EXPORT_CONDITION_ORDER.contains(&k.as_str()) {
                        walk(v, out);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, &mut out);
    out
}

fn string_or_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------- tsconfig

/// Raw `compilerOptions.paths`: `(pattern, targets)` in declaration order.
type RawPaths = Vec<(String, Vec<String>)>;

/// Effective settings of one tsconfig after applying its `extends` chain.
#[derive(Clone, Debug, Default)]
struct TsConfig {
    path: String,
    dir: String,
    /// Repo-relative `baseUrl`, already resolved against its defining config.
    base_url: Option<String>,
    /// `(directory of the config that defines `paths`, raw `paths` map)`.
    paths: Option<(String, RawPaths)>,
    out_dir: Option<String>,
    /// Repo-relative `rootDir`, resolved against its defining config.
    root_dir: Option<String>,
    exclude: Vec<String>,
}

impl TsConfig {
    fn inherit(&mut self, parent: &TsConfig) {
        if parent.base_url.is_some() {
            self.base_url = parent.base_url.clone();
        }
        if parent.paths.is_some() {
            self.paths = parent.paths.clone();
        }
        if parent.out_dir.is_some() {
            self.out_dir = parent.out_dir.clone();
        }
        if parent.root_dir.is_some() {
            self.root_dir = parent.root_dir.clone();
        }
        if !parent.exclude.is_empty() {
            self.exclude = parent.exclude.clone();
        }
    }
}

fn is_tsconfig_name(name: &str) -> bool {
    name == "jsconfig.json" || (name.starts_with("tsconfig") && name.ends_with(".json"))
}

fn is_primary_config_name(name: &str) -> bool {
    name == "tsconfig.json" || name == "jsconfig.json"
}

struct ConfigLoader<'a> {
    files: &'a FileIndex,
    packages: &'a [Package],
    cache: HashMap<String, Option<TsConfig>>,
    stack: Vec<String>,
}

impl ConfigLoader<'_> {
    fn load(&mut self, path: &str) -> Option<TsConfig> {
        if let Some(cached) = self.cache.get(path) {
            return cached.clone();
        }
        if self.stack.iter().any(|p| p == path) {
            tracing::debug!(%path, "tsconfig extends cycle; breaking");
            return None;
        }
        self.stack.push(path.to_string());
        let result = self.load_uncached(path);
        self.stack.pop();
        self.cache.insert(path.to_string(), result.clone());
        result
    }

    fn load_uncached(&mut self, path: &str) -> Option<TsConfig> {
        let text = self.files.read(path)?;
        let Some(json) = parse_jsonc(&text) else {
            tracing::debug!(%path, "tsconfig is not valid JSONC; ignored");
            return None;
        };
        let dir = dir_of(path).to_string();
        let mut cfg = TsConfig {
            path: path.to_string(),
            dir: dir.clone(),
            ..Default::default()
        };

        // Later entries of an `extends` array override earlier ones; the
        // child's own settings override all of them.
        for parent_spec in string_or_array(json.get("extends")) {
            let Some(parent_path) = resolve_extends(&dir, &parent_spec, self.files, self.packages)
            else {
                continue;
            };
            if let Some(parent) = self.load(&parent_path) {
                cfg.inherit(&parent);
            }
        }

        let compiler = json.get("compilerOptions");
        let option = |key: &str| compiler.and_then(|c| c.get(key));

        if let Some(b) = option("baseUrl").and_then(Value::as_str) {
            if let Some(abs) = join_rel(&dir, &expand_config_dir(b)) {
                cfg.base_url = Some(abs);
            }
        }
        if let Some(paths) = option("paths").and_then(Value::as_object) {
            let raw = paths
                .iter()
                .map(|(k, v)| (k.clone(), string_leaves(v)))
                .collect();
            cfg.paths = Some((dir.clone(), raw));
        }
        if let Some(o) = option("outDir").and_then(Value::as_str) {
            if let Some(abs) = join_rel(&dir, &expand_config_dir(o)) {
                cfg.out_dir = Some(abs);
            }
        }
        if let Some(r) = option("rootDir").and_then(Value::as_str) {
            if let Some(abs) = join_rel(&dir, &expand_config_dir(r)) {
                cfg.root_dir = Some(abs);
            }
        }
        if let Some(ex) = json.get("exclude").and_then(Value::as_array) {
            cfg.exclude = ex
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|e| join_rel(&dir, &expand_config_dir(e)))
                .collect();
        }
        Some(cfg)
    }
}

/// Locate the file named by `extends`: relative path (with or without
/// `.json`, or a directory holding `tsconfig.json`) or a workspace package.
fn resolve_extends(
    dir: &str,
    spec: &str,
    files: &FileIndex,
    packages: &[Package],
) -> Option<String> {
    let first_existing = |base: String| -> Option<String> {
        let with_json = format!("{base}.json");
        let in_dir = join_dir(&base, "tsconfig.json");
        [base, with_json, in_dir]
            .into_iter()
            .find(|c| files.contains(c))
    };

    if let Some(rooted) = spec.strip_prefix('/') {
        return first_existing(join_rel("", rooted)?);
    }
    if spec.starts_with('.') {
        return first_existing(join_rel(dir, spec)?);
    }
    let pkg = packages
        .iter()
        .filter(|p| spec == p.name || spec.starts_with(&format!("{}/", p.name)))
        .max_by_key(|p| p.name.len())?;
    let rest = spec[pkg.name.len()..].trim_start_matches('/');
    let base = if rest.is_empty() {
        pkg.dir.clone()
    } else {
        join_rel(&pkg.dir, rest)?
    };
    first_existing(base)
}

fn collect_configs(files: &FileIndex, packages: &[Package]) -> Vec<TsConfig> {
    let mut loader = ConfigLoader {
        files,
        packages,
        cache: HashMap::new(),
        stack: Vec::new(),
    };
    let mut configs: Vec<TsConfig> = files
        .iter()
        .filter(|p| is_tsconfig_name(p.file_name()) && !in_node_modules(p.as_str()))
        .filter_map(|p| loader.load(p.as_str()))
        .collect();
    // Deterministic, with `tsconfig.json` ahead of `tsconfig.*.json` so its
    // aliases are consulted first within a directory.
    configs.sort_by(|a, b| {
        let rank = |c: &TsConfig| {
            u8::from(!is_primary_config_name(
                c.path.rsplit('/').next().unwrap_or(&c.path),
            ))
        };
        a.dir
            .cmp(&b.dir)
            .then(rank(a).cmp(&rank(b)))
            .then(a.path.cmp(&b.path))
    });
    configs
}

// ------------------------------------------------------------------- JSONC

/// Parse JSON that may carry `//` / `/* */` comments and trailing commas.
pub fn parse_jsonc(text: &str) -> Option<Value> {
    let stripped = strip_jsonc(text);
    if stripped.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&stripped).ok()
}

/// Remove comments and trailing commas, leaving string literals untouched so
/// `"http://x//y"` and `"a,}"` survive. Newlines are preserved.
pub fn strip_jsonc(src: &str) -> String {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < len {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                out.push_str(&src[start..i.min(len)]);
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                i = skip_line_comment(bytes, i);
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                i = skip_block_comment(bytes, i);
                out.push(' ');
            }
            b',' => {
                let k = skip_trivia(bytes, i + 1);
                if !(k < len && (bytes[k] == b'}' || bytes[k] == b']')) {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                // Copy a run of ordinary bytes. Every stop byte is ASCII, so
                // slicing here never splits a UTF-8 sequence.
                let start = i;
                i += 1;
                while i < len && !matches!(bytes[i], b'"' | b'/' | b',') {
                    i += 1;
                }
                out.push_str(&src[start..i]);
            }
        }
    }
    out
}

fn skip_line_comment(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], i: usize) -> usize {
    let mut k = i + 2;
    while k + 1 < bytes.len() {
        if bytes[k] == b'*' && bytes[k + 1] == b'/' {
            return k + 2;
        }
        k += 1;
    }
    bytes.len()
}

/// Skip whitespace and comments starting at `i`.
fn skip_trivia(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i = skip_line_comment(bytes, i);
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i = skip_block_comment(bytes, i);
        } else {
            return i;
        }
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    /// Temp directory for config files `detect` must read from disk. Source
    /// files only need to exist in the `FileIndex`.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("astrolabe-ts-{}-{nonce}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create test directory");
            TestDir(path)
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, contents).expect("write test file");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// `disk` files are written and indexed; `virt` files are only indexed.
    fn fixture(disk: &[(&str, &str)], virt: &[&str]) -> (TestDir, FileIndex, ProjectMeta) {
        let dir = TestDir::new();
        for (p, c) in disk {
            dir.write(p, c);
        }
        let files = FileIndex::new(
            dir.0.clone(),
            disk.iter()
                .map(|(p, _)| RelPath::new(p))
                .chain(virt.iter().map(RelPath::new)),
        );
        let meta = TypeScriptResolver.detect(&files);
        (dir, files, meta)
    }

    fn virt_only(virt: &[&str]) -> (FileIndex, ProjectMeta) {
        let files = FileIndex::new("/nonexistent/astrolabe-ts", virt.iter().map(RelPath::new));
        (files, ProjectMeta::default())
    }

    fn res(files: &FileIndex, meta: &ProjectMeta, from: &str, spec: &str) -> Option<String> {
        TypeScriptResolver
            .resolve(&RelPath::new(from), spec, files, meta)
            .map(|p| p.as_str().to_string())
    }

    #[test]
    fn relative_specifiers_infer_extensions() {
        let (files, meta) = virt_only(&[
            "src/a.ts",
            "src/util.ts",
            "src/Comp.tsx",
            "src/legacy.js",
            "src/m.mjs",
            "src/c.cjs",
            "src/only.d.ts",
            "src/x.jsx",
            "src/mod.mts",
        ]);
        let r = |spec| res(&files, &meta, "src/a.ts", spec);
        assert_eq!(r("./util").as_deref(), Some("src/util.ts"));
        assert_eq!(r("./Comp").as_deref(), Some("src/Comp.tsx"));
        assert_eq!(r("./legacy").as_deref(), Some("src/legacy.js"));
        assert_eq!(r("./m").as_deref(), Some("src/m.mjs"));
        assert_eq!(r("./c").as_deref(), Some("src/c.cjs"));
        assert_eq!(r("./only").as_deref(), Some("src/only.d.ts"));
        assert_eq!(r("./x").as_deref(), Some("src/x.jsx"));
        assert_eq!(r("./mod").as_deref(), Some("src/mod.mts"));
        assert_eq!(r("./missing"), None);
    }

    #[test]
    fn esm_js_specifier_resolves_to_ts_source() {
        let (files, meta) = virt_only(&[
            "core/src/index.ts",
            "core/src/rank.ts",
            "core/src/view.tsx",
            "core/src/m.mts",
            "core/src/c.cts",
            "core/src/real.js",
            "core/src/types.d.ts",
            "core/src/both.ts",
            "core/src/both.js",
            "core/tests/t.test.ts",
        ]);
        let r = |spec| res(&files, &meta, "core/src/index.ts", spec);
        assert_eq!(r("./rank.js").as_deref(), Some("core/src/rank.ts"));
        assert_eq!(r("./view.js").as_deref(), Some("core/src/view.tsx"));
        assert_eq!(r("./m.mjs").as_deref(), Some("core/src/m.mts"));
        assert_eq!(r("./c.cjs").as_deref(), Some("core/src/c.cts"));
        assert_eq!(r("./real.js").as_deref(), Some("core/src/real.js"));
        assert_eq!(r("./types.js").as_deref(), Some("core/src/types.d.ts"));
        assert_eq!(r("./types.d.ts").as_deref(), Some("core/src/types.d.ts"));
        // TypeScript tries the source extension before the literal one.
        assert_eq!(r("./both.js").as_deref(), Some("core/src/both.ts"));
        // `allowImportingTsExtensions`.
        assert_eq!(r("./rank.ts").as_deref(), Some("core/src/rank.ts"));
        assert_eq!(
            res(&files, &meta, "core/tests/t.test.ts", "../src/rank.js").as_deref(),
            Some("core/src/rank.ts")
        );
    }

    #[test]
    fn directory_index_resolution() {
        let (files, meta) = virt_only(&[
            "pkg/index.ts",
            "pkg/src/main.ts",
            "pkg/src/utils/index.ts",
            "pkg/src/comps/index.tsx",
            "pkg/src/js/index.js",
            "pkg/src/utils.ts",
        ]);
        let r = |spec| res(&files, &meta, "pkg/src/main.ts", spec);
        // A file beats a directory of the same name, like TypeScript.
        assert_eq!(r("./utils").as_deref(), Some("pkg/src/utils.ts"));
        assert_eq!(r("./utils/").as_deref(), Some("pkg/src/utils/index.ts"));
        assert_eq!(
            r("./utils/index.js").as_deref(),
            Some("pkg/src/utils/index.ts")
        );
        assert_eq!(r("./comps").as_deref(), Some("pkg/src/comps/index.tsx"));
        assert_eq!(r("./js").as_deref(), Some("pkg/src/js/index.js"));
        assert_eq!(r("..").as_deref(), Some("pkg/index.ts"));
        assert_eq!(r("./nope").as_deref(), None);
    }

    #[test]
    fn relative_assets_queries_and_escapes() {
        let (files, meta) = virt_only(&[
            "app/main.tsx",
            "app/styles.css",
            "app/data.json",
            "app/worker.ts",
            "package.json",
        ]);
        let r = |spec| res(&files, &meta, "app/main.tsx", spec);
        assert_eq!(r("./styles.css").as_deref(), Some("app/styles.css"));
        assert_eq!(r("./styles.css?inline").as_deref(), Some("app/styles.css"));
        assert_eq!(r("./data.json").as_deref(), Some("app/data.json"));
        assert_eq!(r("../package.json").as_deref(), Some("package.json"));
        assert_eq!(r("./worker.js?worker").as_deref(), Some("app/worker.ts"));
        assert_eq!(r(".\\worker"), Some("app/worker.ts".into()));
        assert_eq!(r("../../escape"), None);
        assert_eq!(r(""), None);
    }

    #[test]
    fn tsconfig_paths_with_wildcard_and_base_url() {
        let tsconfig = r##"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@/*": ["src/*"],
                    "@utils": ["src/utils/index.ts"],
                    "@lib/*": ["lib/*", "src/lib/*"],
                    "~/*": ["./src/*"],
                    "#gen/*.js": ["generated/*.ts"]
                }
            }
        }"##;
        let (_d, files, meta) = fixture(
            &[("tsconfig.json", tsconfig)],
            &[
                "src/main.ts",
                "src/a/b.ts",
                "src/utils/index.ts",
                "src/lib/x.ts",
                "src/comp/Foo.tsx",
                "shared/k.ts",
                "generated/schema.ts",
            ],
        );
        assert!(alias_units(&meta)
            .any(|a| a.scope.is_empty() && a.pattern == "@/*" && a.target == "src/*"));
        assert!(meta.source_roots.contains(&"".to_string()));

        let r = |spec| res(&files, &meta, "src/main.ts", spec);
        assert_eq!(r("@/a/b").as_deref(), Some("src/a/b.ts"));
        assert_eq!(r("@/a/b.js").as_deref(), Some("src/a/b.ts"));
        assert_eq!(r("@utils").as_deref(), Some("src/utils/index.ts"));
        assert_eq!(r("@lib/x").as_deref(), Some("src/lib/x.ts"));
        assert_eq!(r("~/comp/Foo").as_deref(), Some("src/comp/Foo.tsx"));
        assert_eq!(r("#gen/schema.js").as_deref(), Some("generated/schema.ts"));
        // Non-relative name resolved against `baseUrl`.
        assert_eq!(r("shared/k").as_deref(), Some("shared/k.ts"));
        assert_eq!(r("@/nope"), None);
        assert_eq!(r("react"), None);
    }

    #[test]
    fn paths_without_base_url_are_scoped_to_their_tsconfig() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "ui/tsconfig.json",
                    r#"{"compilerOptions": {"paths": {"@/*": ["./*"]}}}"#,
                ),
                (
                    "viewer/tsconfig.json",
                    r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["src/*"]}}}"#,
                ),
            ],
            &[
                "ui/app/page.tsx",
                "ui/lib/api/types.ts",
                "ui/lib/utils.ts",
                "ui/lib/utils/keyboardNav.ts",
                "viewer/src/main.tsx",
                "viewer/src/lib/utils.ts",
                "other/x.ts",
            ],
        );
        assert!(
            alias_units(&meta).any(|a| a.scope == "ui" && a.pattern == "@/*" && a.target == "ui/*")
        );
        assert!(alias_units(&meta)
            .any(|a| a.scope == "viewer" && a.pattern == "@/*" && a.target == "viewer/src/*"));

        assert_eq!(
            res(&files, &meta, "ui/app/page.tsx", "@/lib/api/types").as_deref(),
            Some("ui/lib/api/types.ts")
        );
        assert_eq!(
            res(&files, &meta, "ui/app/page.tsx", "@/lib/utils").as_deref(),
            Some("ui/lib/utils.ts")
        );
        assert_eq!(
            res(&files, &meta, "ui/app/page.tsx", "@/lib/utils/keyboardNav").as_deref(),
            Some("ui/lib/utils/keyboardNav.ts")
        );
        assert_eq!(
            res(&files, &meta, "viewer/src/main.tsx", "@/lib/utils").as_deref(),
            Some("viewer/src/lib/utils.ts")
        );
        // No enclosing tsconfig defines `@/*`.
        assert_eq!(res(&files, &meta, "other/x.ts", "@/lib/utils"), None);
    }

    #[test]
    fn extends_chain_inherits_paths_and_base_url() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "tsconfig.base.json",
                    r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@shared/*": ["shared/src/*"]}}}"#,
                ),
                // `extends` without `.json`, two hops deep.
                (
                    "packages/mid/tsconfig.json",
                    r#"{"extends": "../../tsconfig.base"}"#,
                ),
                (
                    "packages/app/tsconfig.json",
                    r#"{"extends": "../mid/tsconfig.json", "compilerOptions": {"outDir": "dist"}}"#,
                ),
                // Parent defines `paths` with no `baseUrl`: relative to the
                // parent's own directory, not the child's.
                (
                    "configs/tsconfig.paths.json",
                    r#"{"compilerOptions": {"paths": {"$root/*": ["../src/*"]}}}"#,
                ),
                (
                    "tsconfig.json",
                    r#"{"extends": ["./configs/tsconfig.paths.json"]}"#,
                ),
                // Cycle must not hang.
                ("cyc/a.json", r#"{"extends": "./b.json"}"#),
                ("cyc/b.json", r#"{"extends": "./a.json"}"#),
                ("cyc/tsconfig.json", r#"{"extends": "./a.json"}"#),
            ],
            &[
                "shared/src/util.ts",
                "packages/app/src/main.ts",
                "src/thing.ts",
                "main.ts",
            ],
        );
        assert_eq!(
            res(&files, &meta, "packages/app/src/main.ts", "@shared/util").as_deref(),
            Some("shared/src/util.ts")
        );
        assert_eq!(
            res(&files, &meta, "main.ts", "$root/thing").as_deref(),
            Some("src/thing.ts")
        );
        assert!(meta.excludes.contains(&"packages/app/dist".to_string()));
    }

    #[test]
    fn out_dir_containing_sources_is_never_excluded() {
        let (_d, _files, meta) = fixture(
            &[
                // zod: `"outDir": "."` in a build tsconfig.
                (
                    "packages/zod/tsconfig.build.json",
                    r#"{"compilerOptions": {"outDir": "."}}"#,
                ),
                // outDir is an ancestor of the config dir.
                (
                    "pkg/sub/tsconfig.json",
                    r#"{"compilerOptions": {"outDir": ".."}}"#,
                ),
                // outDir equals rootDir's parent.
                (
                    "lib/tsconfig.json",
                    r#"{"compilerOptions": {"rootDir": "src/core", "outDir": "src"}}"#,
                ),
                // Distinct subdir that holds sources (not pure build output).
                (
                    "gen/tsconfig.json",
                    r#"{"compilerOptions": {"outDir": "out"}}"#,
                ),
                // Regular dist with only emitted output: still excluded.
                (
                    "app/tsconfig.json",
                    r#"{"compilerOptions": {"outDir": "dist"}}"#,
                ),
            ],
            &[
                "packages/zod/src/v4/classic/errors.ts",
                "pkg/sub/a.ts",
                "lib/src/core/x.ts",
                "gen/out/handwritten.ts",
                "app/src/main.ts",
                "app/dist/main.js",
                "app/dist/main.d.ts",
            ],
        );
        for dir in ["packages/zod", "pkg", "lib/src", "gen/out"] {
            assert!(
                !meta.excludes.iter().any(|e| e == dir),
                "{dir} must not be excluded: {:?}",
                meta.excludes
            );
        }
        assert!(meta.excludes.contains(&"app/dist".to_string()));
    }

    #[test]
    fn authored_ts_excludes_declarations() {
        assert!(is_authored_ts("a/b.ts"));
        assert!(is_authored_ts("a/b.tsx"));
        assert!(is_authored_ts("a.d/b.ts"));
        assert!(!is_authored_ts("a/b.d.ts"));
        assert!(!is_authored_ts("a/b.d.mts"));
        assert!(!is_authored_ts("a/b.js"));
    }

    #[test]
    fn tsconfig_extends_workspace_package() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "tooling/tsconfig/package.json",
                    r#"{"name": "@acme/tsconfig"}"#,
                ),
                (
                    "tooling/tsconfig/base.json",
                    r#"{"compilerOptions": {"paths": {"@acme/*": ["../../packages/*/src/index.ts"]}}}"#,
                ),
                (
                    "packages/app/tsconfig.json",
                    r#"{"extends": "@acme/tsconfig/base.json"}"#,
                ),
            ],
            &["packages/app/src/main.ts", "packages/core/src/index.ts"],
        );
        assert_eq!(
            res(&files, &meta, "packages/app/src/main.ts", "@acme/core").as_deref(),
            Some("packages/core/src/index.ts")
        );
    }

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let tsconfig = "\u{feff}// leading comment\n\
            {\n\
              /* block\n   comment */\n\
              \"compilerOptions\": {\n\
                \"baseUrl\": \".\", // trailing line comment\n\
                \"paths\": {\n\
                  \"@/*\": [\"src/*\",], // trailing comma in array\n\
                  \"@url\": [\"src/url//x.ts\"], /* not a comment: inside a string */\n\
                },\n\
              },\n\
            }\n";
        let (_d, files, meta) = fixture(
            &[("tsconfig.json", tsconfig)],
            &["src/a.ts", "src/url/x.ts"],
        );
        assert_eq!(
            res(&files, &meta, "src/a.ts", "@/a").as_deref(),
            Some("src/a.ts")
        );
        assert_eq!(
            res(&files, &meta, "src/a.ts", "@url").as_deref(),
            Some("src/url/x.ts")
        );

        let v = parse_jsonc(r#"{"a": "http://x//y, }", /*c*/ "b": [1,2,], "c": "\"q\"",}"#)
            .expect("parses");
        assert_eq!(v["a"], "http://x//y, }");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
        assert_eq!(v["c"], "\"q\"");
        assert_eq!(parse_jsonc("// only a comment\n"), None);
        assert_eq!(parse_jsonc("{ not json }"), None);
        // Multi-byte text survives byte-level scanning.
        let v = parse_jsonc("{\"名字\": \"值 // 不是注释\", // 注释\n}").expect("parses");
        assert_eq!(v["名字"], "值 // 不是注释");
    }

    #[test]
    fn monorepo_package_names_resolve_to_source_entries() {
        let core_pkg = r#"{
            "name": "@acme/core",
            "main": "./dist/index.js",
            "types": "./dist/index.d.ts",
            "exports": {
                ".": {"types": "./dist/index.d.ts", "default": "./dist/index.js"},
                "./browser": {"default": "./dist/browser.js"},
                "./features/*": "./dist/features/*.js",
                "./legacy/": "./dist/legacy/"
            }
        }"#;
        let (_d, files, meta) = fixture(
            &[
                (
                    "package.json",
                    r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#,
                ),
                ("packages/core/package.json", core_pkg),
                ("packages/bare/package.json", r#"{"name": "@acme/bare"}"#),
                (
                    "packages/flat/package.json",
                    r#"{"name": "flat", "exports": "./lib/main.js"}"#,
                ),
                ("packages/app/package.json", r#"{"name": "@acme/app"}"#),
            ],
            &[
                "packages/core/src/index.ts",
                "packages/core/src/browser.ts",
                "packages/core/src/util.ts",
                "packages/core/src/features/a.ts",
                "packages/core/src/legacy/old.ts",
                "packages/bare/src/index.ts",
                "packages/flat/main.ts",
                "packages/app/src/main.ts",
            ],
        );
        assert!(meta.modules.contains(&ModuleUnit {
            name: "@acme/core".into(),
            dir: "packages/core".into(),
        }));
        assert_eq!(
            meta.module_for("@acme/core/browser")
                .map(|u| u.dir.as_str()),
            Some("packages/core")
        );
        assert!(alias_units(&meta).any(|a| a.scope.is_empty()
            && a.pattern == "@acme/core"
            && a.target == "packages/core/src/index.ts"));

        let r = |spec| res(&files, &meta, "packages/app/src/main.ts", spec);
        assert_eq!(
            r("@acme/core").as_deref(),
            Some("packages/core/src/index.ts")
        );
        assert_eq!(
            r("@acme/core/browser").as_deref(),
            Some("packages/core/src/browser.ts")
        );
        assert_eq!(
            r("@acme/core/features/a").as_deref(),
            Some("packages/core/src/features/a.ts")
        );
        assert_eq!(
            r("@acme/core/legacy/old").as_deref(),
            Some("packages/core/src/legacy/old.ts")
        );
        // Deep import into the package, ESM-style.
        assert_eq!(
            r("@acme/core/src/util.js").as_deref(),
            Some("packages/core/src/util.ts")
        );
        assert_eq!(
            r("@acme/core/util.js").as_deref(),
            Some("packages/core/src/util.ts")
        );
        // No entry declared → `src/index.ts`.
        assert_eq!(
            r("@acme/bare").as_deref(),
            Some("packages/bare/src/index.ts")
        );
        // `exports` as a bare string, output dir dropped.
        assert_eq!(r("flat").as_deref(), Some("packages/flat/main.ts"));
        assert_eq!(r("@acme/nope"), None);
        assert_eq!(r("@acme/core/nope"), None);
    }

    #[test]
    fn same_named_package_prefers_declared_workspace_over_examples() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "package.json",
                    r#"{"name": "root", "workspaces": ["packages/*"]}"#,
                ),
                ("examples/core/package.json", r#"{"name": "@acme/core"}"#),
                ("packages/core/package.json", r#"{"name": "@acme/core"}"#),
            ],
            &[
                "examples/core/index.ts",
                "packages/core/index.ts",
                "packages/app/x.ts",
            ],
        );
        assert_eq!(
            res(&files, &meta, "packages/app/x.ts", "@acme/core").as_deref(),
            Some("packages/core/index.ts")
        );
    }

    #[test]
    fn same_named_package_without_workspaces_avoids_samples() {
        let (_d, files, meta) = fixture(
            &[
                ("examples/core/package.json", r#"{"name": "@acme/core"}"#),
                ("packages/core/package.json", r#"{"name": "@acme/core"}"#),
            ],
            &["examples/core/index.ts", "packages/core/index.ts", "a/x.ts"],
        );
        assert_eq!(
            res(&files, &meta, "a/x.ts", "@acme/core").as_deref(),
            Some("packages/core/index.ts")
        );
    }

    #[test]
    fn scope_index_matches_linear_scan() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "tsconfig.json",
                    r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["src/*"]}}}"#,
                ),
                (
                    "ui/tsconfig.json",
                    r#"{"compilerOptions": {"paths": {"@/*": ["src/*"]}}}"#,
                ),
            ],
            &["src/a.ts", "ui/src/a.ts", "ui/b.ts", "c.ts"],
        );
        assert!(!meta.scope_index.0.is_empty());
        let mut linear = meta.clone();
        linear.scope_index = Default::default();
        for from in ["ui/b.ts", "c.ts"] {
            assert_eq!(
                res(&files, &meta, from, "@/a"),
                res(&files, &linear, from, "@/a")
            );
        }
        assert_eq!(
            res(&files, &meta, "ui/b.ts", "@/a").as_deref(),
            Some("ui/src/a.ts")
        );
        assert_eq!(
            res(&files, &meta, "c.ts", "@/a").as_deref(),
            Some("src/a.ts")
        );
    }

    #[test]
    fn duplicate_package_names_prefer_the_workspace_member() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "package.json",
                    r#"{"name": "openvisio", "workspaces": ["mcp"]}"#,
                ),
                (
                    "mcp/package.json",
                    r#"{"name": "openvisio", "type": "module"}"#,
                ),
            ],
            &["mcp/src/index.ts", "viewer/src/x.ts"],
        );
        assert_eq!(
            res(&files, &meta, "viewer/src/x.ts", "openvisio").as_deref(),
            Some("mcp/src/index.ts")
        );
        assert_eq!(
            meta.module_for("openvisio").map(|u| u.dir.as_str()),
            Some("mcp")
        );
    }

    #[test]
    fn package_json_imports_field_is_scoped_to_the_package() {
        let (_d, files, meta) = fixture(
            &[(
                "pkg/package.json",
                r##"{"name": "pkg", "imports": {"#internal/*": "./src/internal/*.js", "#config": {"default": "./src/config.js"}, "#dep": "some-dep"}}"##,
            )],
            &[
                "pkg/src/main.ts",
                "pkg/src/internal/db.ts",
                "pkg/src/config.ts",
                "other/y.ts",
            ],
        );
        let r = |spec| res(&files, &meta, "pkg/src/main.ts", spec);
        assert_eq!(r("#internal/db").as_deref(), Some("pkg/src/internal/db.ts"));
        assert_eq!(r("#config").as_deref(), Some("pkg/src/config.ts"));
        assert_eq!(r("#dep"), None);
        assert_eq!(res(&files, &meta, "other/y.ts", "#config"), None);
    }

    #[test]
    fn node_modules_and_builtins_return_none() {
        let (_d, files, meta) = fixture(
            &[
                (
                    "package.json",
                    r#"{"name": "app", "dependencies": {"react": "^19"}}"#,
                ),
                (
                    "node_modules/react/package.json",
                    r#"{"name": "react", "main": "index.js"}"#,
                ),
                (
                    "node_modules/react/tsconfig.json",
                    r#"{"compilerOptions": {"paths": {"@/*": ["./*"]}}}"#,
                ),
            ],
            &[
                "src/main.tsx",
                "node_modules/react/index.js",
                "node_modules/lodash/fp.js",
            ],
        );
        assert!(!meta.modules.iter().any(|m| m.name.contains("react")));
        let r = |spec| res(&files, &meta, "src/main.tsx", spec);
        assert_eq!(r("react"), None);
        assert_eq!(r("react/jsx-runtime"), None);
        assert_eq!(r("lodash/fp"), None);
        assert_eq!(r("node:fs"), None);
        assert_eq!(r("fs"), None);
        assert_eq!(r("@types/node"), None);
        assert_eq!(r("@modelcontextprotocol/sdk/server/mcp.js"), None);
    }

    #[test]
    fn rooted_specifier_uses_nearest_project_root() {
        let (_d, files, meta) = fixture(
            &[(
                "viewer/tsconfig.json",
                r#"{"compilerOptions": {"strict": true}}"#,
            )],
            &["viewer/src/main.tsx", "viewer/src/x.ts", "top.ts"],
        );
        assert_eq!(
            res(&files, &meta, "viewer/src/main.tsx", "/src/x").as_deref(),
            Some("viewer/src/x.ts")
        );
        assert_eq!(
            res(&files, &meta, "viewer/src/main.tsx", "/top").as_deref(),
            Some("top.ts")
        );
    }

    #[test]
    fn detect_is_empty_and_harmless_without_config() {
        let (files, _) = virt_only(&["a.ts", "b.ts"]);
        let meta = TypeScriptResolver.detect(&files);
        assert_eq!(meta, ProjectMeta::default());
        assert_eq!(res(&files, &meta, "a.ts", "./b").as_deref(), Some("b.ts"));
    }

    #[test]
    fn strip_jsonc_handles_edge_cases() {
        assert_eq!(strip_jsonc(""), "");
        assert_eq!(strip_jsonc("/* unterminated"), " ");
        assert_eq!(strip_jsonc("\"unterminated"), "\"unterminated");
        assert_eq!(strip_jsonc("[1,2,3]"), "[1,2,3]");
        assert_eq!(strip_jsonc("[1,\n // c\n]"), "[1\n \n]");
        assert_eq!(strip_jsonc("a / b"), "a / b");
        assert_eq!(strip_jsonc("{\"k\": \"a\\\"b\", }"), "{\"k\": \"a\\\"b\" }");
    }

    // ------------------------------------------------------- real corpus

    /// Corpus location, resolved rather than hardcoded: `ASTROLABE_CORPUS_TYPESCRIPT`
    /// if set, else a sibling of the workspace. An absolute path baked into the
    /// source makes the check reproducible on exactly one machine.
    fn corpus_root() -> std::path::PathBuf {
        std::env::var_os("ASTROLABE_CORPUS_TYPESCRIPT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../openvisio-oss")
            })
    }
    const CORPUS_SKIP_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "dist",
        ".next",
        "build",
        "coverage",
        ".openvisio",
        ".turbo",
        ".cache",
    ];

    fn walk_corpus(root: &Path) -> FileIndex {
        fn visit(root: &Path, dir: &Path, out: &mut Vec<RelPath>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if CORPUS_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    visit(root, &path, out);
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(root)
                        .expect("under root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push(RelPath::new(rel));
                }
            }
        }
        let mut out = Vec::new();
        visit(root, root, &mut out);
        FileIndex::new(root, out)
    }

    fn quoted_prefix(s: &str) -> Option<String> {
        let q = s.chars().next()?;
        if q != '\'' && q != '"' {
            return None;
        }
        let body = &s[1..];
        let end = body.find(q)?;
        Some(body[..end].to_string())
    }

    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    /// Line-based import extraction good enough to audit the corpus: `from
    /// '…'`, side-effect `import '…'`, `import('…')`, `require('…')`.
    fn extract_specifiers(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in src.lines() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            let mut search = 0;
            while let Some(i) = line[search..].find("from") {
                let at = search + i;
                let before_ok = at == 0 || !is_ident_byte(line.as_bytes()[at - 1]);
                let rest = &line[at + 4..];
                let rest_trim = rest.trim_start();
                let after_ok = rest.len() != rest_trim.len() || rest_trim.starts_with(['\'', '"']);
                if before_ok && after_ok {
                    if let Some(s) = quoted_prefix(rest_trim) {
                        out.push(s);
                    }
                }
                search = at + 4;
            }
            if let Some(rest) = t.strip_prefix("import") {
                if let Some(s) = quoted_prefix(rest.trim_start()) {
                    out.push(s);
                }
            }
            for kw in ["import(", "require("] {
                let mut search = 0;
                while let Some(i) = line[search..].find(kw) {
                    let at = search + i;
                    if let Some(s) = quoted_prefix(line[at + kw.len()..].trim_start()) {
                        out.push(s);
                    }
                    search = at + kw.len();
                }
            }
        }
        out
    }

    /// Ground truth independent of the resolver: does *any* indexed file sit
    /// where a relative specifier points (same stem, any extension, or a
    /// directory)? Specifiers inside string fixtures fail this and are not
    /// counted as in-repo.
    fn plausibly_in_repo(from: &RelPath, spec: &str, files: &FileIndex) -> bool {
        let spec = spec.split('?').next().unwrap_or(spec);
        let Some(joined) = join_rel(from.dir(), spec) else {
            return false;
        };
        let code_ext = |e: &str| {
            matches!(
                e,
                "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts"
            )
        };
        let stem = match split_ext(&joined) {
            Some((stem, ext)) if code_ext(ext) => stem.to_string(),
            _ => joined.clone(),
        };
        let stem = stem.strip_suffix(".d").map(str::to_string).unwrap_or(stem);
        let dot = format!("{stem}.");
        let slash = format!("{stem}/");
        files.iter().any(|f| {
            let s = f.as_str();
            s == joined || s == stem || s.starts_with(&dot) || s.starts_with(&slash)
        })
    }

    #[test]
    #[ignore = "needs the openvisio-oss corpus; run with --ignored"]
    fn openvisio_oss_corpus_resolves_every_in_repo_import() {
        let corpus = corpus_root();
        let root = corpus.as_path();
        assert!(
            root.is_dir(),
            "corpus not found at {}; set ASTROLABE_CORPUS_TYPESCRIPT",
            root.display()
        );
        let files = walk_corpus(root);
        let resolver = TypeScriptResolver;
        let meta = resolver.detect(&files);

        let mut source_files = 0usize;
        let mut relative_total = 0usize;
        let mut relative_in_repo = 0usize;
        let mut relative_resolved = 0usize;
        let mut relative_not_in_repo: Vec<(String, String)> = Vec::new();
        let mut unresolved: Vec<(String, String)> = Vec::new();
        let mut bare_total = 0usize;
        let mut bare_resolved: Vec<(String, String)> = Vec::new();
        let mut bare_in_repo_total = 0usize;
        let mut bare_in_repo_unresolved: Vec<(String, String)> = Vec::new();

        for from in files.iter() {
            if !matches!(
                Language::from_path(from),
                Some(Language::TypeScript | Language::Tsx | Language::JavaScript)
            ) {
                continue;
            }
            let Some(src) = files.read(from.as_str()) else {
                continue;
            };
            source_files += 1;
            for spec in extract_specifiers(&src) {
                let hit = resolver.resolve(from, &spec, &files, &meta);
                if is_relative(&spec) {
                    relative_total += 1;
                    if plausibly_in_repo(from, &spec, &files) {
                        relative_in_repo += 1;
                        if hit.is_some() {
                            relative_resolved += 1;
                        } else {
                            unresolved.push((from.as_str().to_string(), spec.clone()));
                        }
                    } else {
                        relative_not_in_repo.push((from.as_str().to_string(), spec.clone()));
                    }
                } else {
                    bare_total += 1;
                    let in_repo_by_construction =
                        spec.starts_with("@/") || spec.starts_with("@openvisio/");
                    if in_repo_by_construction {
                        bare_in_repo_total += 1;
                        if hit.is_none() {
                            bare_in_repo_unresolved.push((from.as_str().to_string(), spec.clone()));
                        }
                    }
                    if let Some(h) = &hit {
                        bare_resolved.push((spec.clone(), h.as_str().to_string()));
                    }
                }
            }
        }

        bare_resolved.sort();
        bare_resolved.dedup();
        let rate = if relative_in_repo == 0 {
            1.0
        } else {
            relative_resolved as f64 / relative_in_repo as f64
        };

        println!("corpus: {}", corpus_root().display());
        println!(
            "indexed files: {}  (ts/tsx/js source files: {source_files})",
            files.len()
        );
        println!(
            "workspace packages: {:?}",
            meta.modules
                .iter()
                .filter(|m| !m.name.contains(ALIAS_SCOPE_SEP))
                .map(|m| format!("{} -> {}", m.name, m.dir))
                .collect::<Vec<_>>()
        );
        println!("alias rules: {}", alias_units(&meta).count());
        println!("relative specifiers: {relative_total}");
        println!("  pointing into the repo: {relative_in_repo}");
        println!("  resolved: {relative_resolved}  ({:.2}%)", rate * 100.0);
        println!("  not in repo (string fixtures etc.): {relative_not_in_repo:?}");
        println!("bare specifiers: {bare_total}");
        println!(
            "  in-repo by construction (@/…, @openvisio/…): {bare_in_repo_total}, unresolved: {}",
            bare_in_repo_unresolved.len()
        );
        println!("  distinct bare specifiers resolved into the repo:");
        for (spec, target) in &bare_resolved {
            println!("    {spec} -> {target}");
        }
        if !unresolved.is_empty() {
            println!("UNRESOLVED relative imports:");
            for (from, spec) in &unresolved {
                println!("    {from}: {spec}");
            }
        }

        assert!(
            relative_in_repo >= 200,
            "expected ≥200 in-repo relative imports, found {relative_in_repo}"
        );
        assert_eq!(
            relative_resolved, relative_in_repo,
            "unresolved in-repo relative imports: {unresolved:#?}"
        );
        assert!(
            bare_in_repo_unresolved.is_empty(),
            "unresolved alias/workspace imports: {bare_in_repo_unresolved:#?}"
        );
    }
}
