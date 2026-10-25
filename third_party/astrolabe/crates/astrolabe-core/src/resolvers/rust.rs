//! Rust module resolution — the hardest of the five.
//!
//! Target: 100% of the 209 in-workspace `use` statements in the `ripgrep`
//! corpus (10 crates). Prior art resolved 23%. (An earlier draft quoted 91,
//! from a line-scanning denominator that missed `use` inside nested items;
//! the acceptance suite now counts per statement at any nesting depth.)
//!
//! Three things have to be right, and prior art got none of them:
//!
//! 1. **Crate name mapping.** `[package] name = "grep-searcher"` is imported
//!    as `grep_searcher`. Hyphens become underscores.
//! 2. **Crate-relative anchoring.** `crate::` means that crate's `src/`, not
//!    the repo root. `self::` is the current file's directory; each `super::`
//!    climbs one module level.
//! 3. **Trailing-segment back-off.** `use crate::foo::Bar` ends at a *type*,
//!    not a module. Try `foo/Bar.rs`, then `foo.rs`, then `foo/mod.rs`. Without
//!    this, almost every `use` fails.
//!
//! Must also handle: `pub use` re-exports, brace groups
//! (`use a::{b, c::d}` — expand before resolving), `use a::b as c`,
//! `#[path = "..."] mod x;` overrides, `mod x;` declaring `x.rs` or
//! `x/mod.rs`, and 2015-edition paths. Workspace members come from the root
//! `Cargo.toml` `[workspace] members` plus every nested `[package]`.
//!
//! # How it works
//!
//! **`detect`** reads every `Cargo.toml` in the index (plus any `[workspace]
//! members`, glob-expanded, that the scanner may have skipped). Each
//! `[package]` yields a [`ModuleUnit`] `{ name: <hyphens→underscores>,
//! dir: <pkg>/src }`. Targets whose `path` lies outside `src/` (ripgrep's
//! `[[bin]] path = "crates/core/main.rs"`, `[[test]] path = "tests/tests.rs"`)
//! yield an extra unit for their root directory under the same package name,
//! so `crate::` inside them anchors correctly. Path dependencies
//! (`foo = { path = "..." }`) are added under the key used in source.
//! `#[path = "…"] mod name;` redirections are collected here into
//! [`ProjectMeta::overrides`] from a small set of candidate `.rs` files
//! (module files that can declare children), so `resolve` never opens a
//! file.
//!
//! **`resolve`** first finds the crate owning `from` (longest `dir` prefix),
//! then anchors the path:
//!
//! | first segment          | anchor                                          |
//! |------------------------|-------------------------------------------------|
//! | `crate`                | the owning crate's root directory / root file   |
//! | `self`                 | the current file's module                       |
//! | `super` (repeated)     | one module level up per `super`, clamped at root|
//! | a known crate name     | that crate's `src/` (prefers the dir with lib.rs)|
//! | `std`/`core`/`alloc`…  | `None` — toolchain crates are not in the repo   |
//! | anything else          | a module visible from the current scope or the  |
//! |                        | crate root (2015 / uniform paths); else `None`  |
//!
//! The remaining segments are walked with back-off: a `#[path]` override
//! collected at detect time wins (matching rustc); otherwise from the longest
//! prefix down, try `<base>/<segs>.rs` then `<base>/<segs>/mod.rs`; finally
//! fall back to the anchoring module's own file. `resolve` is index lookups
//! and an in-memory `override_for` probe — no disk I/O.

use std::collections::BTreeSet;

use crate::types::{
    join_rel, FileIndex, ImportResolution, Language, ModuleResolver, ModuleUnit, PathOverride,
    ProjectMeta, RelPath,
};

pub struct RustResolver;

/// Crates shipped with the toolchain. They can never be in the repo.
const TOOLCHAIN_CRATES: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];

impl ModuleResolver for RustResolver {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut manifests: Vec<String> = files
            .iter()
            .filter(|p| p.file_name() == "Cargo.toml" && !in_build_output(p.as_str()))
            .map(|p| p.as_str().to_string())
            .collect();

        let mut meta = ProjectMeta::default();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut i = 0;
        // Index-based loop: workspace members discovered along the way are
        // appended and processed in the same pass.
        while i < manifests.len() {
            let path = manifests[i].clone();
            i += 1;
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(text) = files.read(&path) else {
                continue;
            };
            let pkg_dir = RelPath::new(&path).dir().to_string();
            let manifest = Manifest::parse(&text);

            for member in &manifest.members {
                for dir in expand_member(&pkg_dir, member, files) {
                    let mp = join_path(&dir, "Cargo.toml");
                    if !manifests.contains(&mp) {
                        manifests.push(mp);
                    }
                }
            }

            if let Some(name) = &manifest.package_name {
                let name = normalize_crate_name(name);
                push_unit(&mut meta.modules, &name, join_path(&pkg_dir, "src"));
                let lib_name = manifest
                    .lib_name
                    .as_deref()
                    .map(normalize_crate_name)
                    .filter(|l| *l != name);
                if let Some(l) = &lib_name {
                    push_unit(&mut meta.modules, l, join_path(&pkg_dir, "src"));
                }
                for target in &manifest.target_paths {
                    let Some(file) = join_rel(&pkg_dir, target) else {
                        continue;
                    };
                    if files.contains(&file) {
                        let dir = RelPath::new(&file).dir().to_string();
                        push_unit(&mut meta.modules, &name, dir.clone());
                        if let Some(l) = &lib_name {
                            push_unit(&mut meta.modules, l, dir);
                        }
                    }
                }
            }

            for (dep, rel) in &manifest.path_deps {
                let Some(dir) = join_rel(&pkg_dir, rel) else {
                    continue;
                };
                let src = join_path(&dir, "src");
                if files.has_dir(&src) {
                    push_unit(&mut meta.modules, &normalize_crate_name(dep), src);
                }
            }

            let target_dir = join_path(&pkg_dir, "target");
            if !meta.excludes.contains(&target_dir) {
                meta.excludes.push(target_dir);
            }
        }

        meta.source_roots = meta
            .modules
            .iter()
            .map(|u| u.dir.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        meta.overrides = collect_path_overrides(files, &meta.modules);
        meta
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        self.resolve_all(from, spec, files, meta).into_iter().next()
    }

    fn resolve_import(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> ImportResolution {
        let mut all = self.resolve_all(from, spec, files, meta);
        match all.len() {
            0 => ImportResolution::Unresolved,
            1 => ImportResolution::File(all.remove(0)),
            _ => ImportResolution::Files(all),
        }
    }
}

impl RustResolver {
    /// Every distinct repo file a `use` statement points at, in source order.
    fn resolve_all(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Vec<RelPath> {
        let cleaned = clean_spec(spec);
        if cleaned.body.is_empty() {
            return Vec::new();
        }
        let ctx = CrateCtx::for_file(from, files, meta);

        if cleaned.is_mod_decl {
            // `mod x;` — a child of the current module, honouring `#[path]`.
            let Some(name) = cleaned.body.split_whitespace().next() else {
                return Vec::new();
            };
            let name = name.trim_start_matches("r#");
            return child_module(&ctx.self_module(), name, files, meta, true)
                .and_then(|c| c.file)
                .filter(|f| files.contains(f))
                .map(|f| vec![RelPath::new(f)])
                .unwrap_or_default();
        }

        let mut out: Vec<RelPath> = Vec::new();
        for segs in expand_use_tree(&cleaned.body) {
            if let Some(hit) = resolve_path(&ctx, &segs, files, meta) {
                let hit = RelPath::new(hit);
                if !out.contains(&hit) {
                    out.push(hit);
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------------ detect

/// The parts of a `Cargo.toml` this resolver cares about.
#[derive(Debug, Default)]
struct Manifest {
    package_name: Option<String>,
    /// `[lib] name`: the identifier `use` statements actually write.
    lib_name: Option<String>,
    /// `[lib]`, `[[bin]]`, `[[test]]`, `[[bench]]`, `[[example]]` `path`s.
    target_paths: Vec<String>,
    /// `(dependency key, relative path)` for every `{ path = "..." }` dep.
    path_deps: Vec<(String, String)>,
    /// `[workspace] members`, unexpanded.
    members: Vec<String>,
}

impl Manifest {
    fn parse(text: &str) -> Manifest {
        let mut m = Manifest::default();
        let table: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(_) => {
                // Malformed TOML: salvage the package name so the crate still
                // gets a unit.
                m.package_name = scan_package_name(text);
                return m;
            }
        };

        m.package_name = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string);

        m.lib_name = table
            .get("lib")
            .and_then(|l| l.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string);
        if let Some(p) = table
            .get("lib")
            .and_then(|l| l.get("path"))
            .and_then(|v| v.as_str())
        {
            m.target_paths.push(p.to_string());
        }
        for key in ["bin", "test", "bench", "example"] {
            let Some(arr) = table.get(key).and_then(|v| v.as_array()) else {
                continue;
            };
            for t in arr {
                if let Some(p) = t.get("path").and_then(|v| v.as_str()) {
                    m.target_paths.push(p.to_string());
                }
            }
        }

        const DEP_KEYS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
        let mut dep_tables: Vec<&toml::Table> = Vec::new();
        for key in DEP_KEYS {
            if let Some(t) = table.get(key).and_then(|v| v.as_table()) {
                dep_tables.push(t);
            }
        }
        if let Some(ws) = table.get("workspace").and_then(|v| v.as_table()) {
            if let Some(t) = ws.get("dependencies").and_then(|v| v.as_table()) {
                dep_tables.push(t);
            }
            if let Some(arr) = ws.get("members").and_then(|v| v.as_array()) {
                m.members = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect();
            }
        }
        if let Some(targets) = table.get("target").and_then(|v| v.as_table()) {
            for cfg in targets.values() {
                for key in DEP_KEYS {
                    if let Some(t) = cfg.get(key).and_then(|v| v.as_table()) {
                        dep_tables.push(t);
                    }
                }
            }
        }
        for t in dep_tables {
            for (name, val) in t {
                if let Some(p) = val.get("path").and_then(|v| v.as_str()) {
                    m.path_deps.push((name.clone(), p.to_string()));
                }
            }
        }
        m
    }
}

/// Line-based fallback for a `Cargo.toml` the TOML parser rejects.
fn scan_package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                let quoted = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
                let end = quoted.find(['"', '\''])?;
                return Some(quoted[..end].to_string());
            }
        }
    }
    None
}

/// `grep-searcher` is imported as `grep_searcher`.
fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Expand one `[workspace] members` entry (possibly a glob such as
/// `crates/*`) into repo-relative directories.
fn expand_member(ws_dir: &str, pattern: &str, files: &FileIndex) -> Vec<String> {
    let Some(full) = join_rel(ws_dir, pattern) else {
        return Vec::new();
    };
    if !full.contains(['*', '?', '[']) {
        return vec![full];
    }
    let Ok(glob) = globset::GlobBuilder::new(&full)
        .literal_separator(true)
        .build()
    else {
        return Vec::new();
    };
    let matcher = glob.compile_matcher();
    files
        .iter()
        .map(|p| p.dir())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|d| !d.is_empty() && matcher.is_match(d))
        .map(str::to_string)
        .collect()
}

fn push_unit(units: &mut Vec<ModuleUnit>, name: &str, dir: String) {
    if !units.iter().any(|u| u.name == name && u.dir == dir) {
        units.push(ModuleUnit {
            name: name.to_string(),
            dir,
        });
    }
}

fn in_build_output(path: &str) -> bool {
    path.starts_with("target/") || path.contains("/target/")
}

/// Collect `#[path = "..."] mod name;` redirections.
///
/// `#[path]` is rare. Reading every `.rs` file would only move the I/O from
/// the resolve hot path onto startup. Candidate files are those that can
/// declare children:
/// - `lib.rs` / `main.rs` / `mod.rs` / `build.rs`
/// - `foo.rs` sitting next to a `foo/` directory
/// - crate-root files already implied by detected units
///
/// After the read, a cheap `path` substring check skips the line parser.
fn collect_path_overrides(files: &FileIndex, units: &[ModuleUnit]) -> Vec<PathOverride> {
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for p in files.iter() {
        if is_path_attr_candidate(p, files) {
            candidates.insert(p.as_str().to_string());
        }
    }
    for u in units {
        if let Some(root) = find_root_file(&u.dir, files) {
            if !in_build_output(&root) {
                candidates.insert(root);
            }
        }
    }
    let mut out = Vec::new();
    for path in candidates {
        let Some(text) = files.read(&path) else {
            continue;
        };
        scan_path_overrides(&path, &text, files, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn is_path_attr_candidate(p: &RelPath, files: &FileIndex) -> bool {
    if p.extension() != Some("rs") || in_build_output(p.as_str()) {
        return false;
    }
    let name = p.file_name();
    if matches!(name, "lib.rs" | "main.rs" | "mod.rs" | "build.rs") {
        return true;
    }
    let Some(stem) = name.strip_suffix(".rs") else {
        return false;
    };
    files.has_dir(&join_path(p.dir(), stem))
}

/// Scan `text` of `decl_file` for every `#[path = "..."] mod name;` whose
/// target exists in the index. Same line / `cfg_attr` / interleaved `#[cfg]`
/// forms as rustc accepts.
fn scan_path_overrides(
    decl_file: &str,
    text: &str,
    files: &FileIndex,
    out: &mut Vec<PathOverride>,
) {
    if !text.contains("path") {
        return;
    }
    let decl_dir = RelPath::new(decl_file).dir().to_string();
    let mut pending: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let mut rest = line;
        if line.starts_with('#') {
            if let Some(p) = quoted_path_attr(line) {
                pending = Some(p);
            }
            // `#[path = "x.rs"] mod y;` on one line.
            match line.rfind(']') {
                Some(i) => rest = line[i + 1..].trim(),
                None => continue,
            }
            if rest.is_empty() {
                continue; // keep `pending` across interleaved `#[cfg]`s
            }
        }
        let declared = mod_decl_name(rest);
        if let (Some(p), Some(m)) = (pending.take(), declared) {
            if let Some(target) = join_rel(&decl_dir, &p) {
                if files.contains(&target) {
                    out.push(PathOverride {
                        declared_in: decl_file.to_string(),
                        name: m.to_string(),
                        target,
                    });
                }
            }
        }
    }
}

// ----------------------------------------------------------------- resolve

/// A module's position on disk: the directory its child modules live in and
/// the file that declares it (`None` when that file could not be located).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Module {
    child_dir: String,
    file: Option<String>,
}

/// The crate that owns the importing file.
struct CrateCtx<'a> {
    from: &'a RelPath,
    /// Directory holding the crate root's child modules (`<pkg>/src`).
    unit_dir: String,
    /// `lib.rs` / `main.rs` / explicit target path, when found.
    root_file: Option<String>,
}

impl<'a> CrateCtx<'a> {
    fn for_file(from: &'a RelPath, files: &FileIndex, meta: &ProjectMeta) -> Self {
        let path = from.as_str();
        let (unit_dir, root_file) = match owning_unit(path, meta) {
            Some(unit) => {
                let rel = strip_dir(path, &unit.dir).unwrap_or(path);
                match rel.strip_prefix("bin/") {
                    // `src/bin/<name>.rs` and `src/bin/<name>/main.rs` are
                    // crate roots of their own (Cargo autobins).
                    Some(rest) => match rest.find('/') {
                        None => (from.dir().to_string(), Some(path.to_string())),
                        Some(i) => {
                            let dir = join_path(&join_path(&unit.dir, "bin"), &rest[..i]);
                            let root = find_root_file(&dir, files);
                            (dir, root)
                        }
                    },
                    None => (unit.dir.clone(), find_root_file(&unit.dir, files)),
                }
            }
            // Not inside any known crate: integration tests, examples and
            // benches are each their own crate root.
            None => (from.dir().to_string(), Some(path.to_string())),
        };
        CrateCtx {
            from,
            unit_dir,
            root_file,
        }
    }

    fn is_root(&self) -> bool {
        self.root_file.as_deref() == Some(self.from.as_str())
    }

    fn crate_module(&self) -> Module {
        Module {
            child_dir: self.unit_dir.clone(),
            file: self.root_file.clone(),
        }
    }

    /// The module declared by `from` itself.
    fn self_module(&self) -> Module {
        if self.is_root() {
            return self.crate_module();
        }
        let path = self.from.as_str();
        let child_dir = if self.from.file_name() == "mod.rs" {
            self.from.dir().to_string()
        } else {
            path.strip_suffix(".rs").unwrap_or(path).to_string()
        };
        Module {
            child_dir,
            file: Some(path.to_string()),
        }
    }

    /// One `super::` step. Clamps at the crate root.
    fn parent_of(&self, m: &Module, files: &FileIndex) -> Module {
        let Some(rel) = strip_dir(&m.child_dir, &self.unit_dir) else {
            return self.crate_module();
        };
        if rel.is_empty() {
            return self.crate_module();
        }
        let pdir = parent_dir(&m.child_dir).to_string();
        if pdir == self.unit_dir {
            return self.crate_module();
        }
        // The parent module is either `<pdir>/mod.rs` or `<pdir>.rs`.
        let name = basename(&pdir);
        let mod_rs = join_path(&pdir, "mod.rs");
        let file_rs = join_path(parent_dir(&pdir), &format!("{name}.rs"));
        let file = [mod_rs, file_rs].into_iter().find(|f| files.contains(f));
        Module {
            child_dir: pdir,
            file,
        }
    }
}

fn resolve_path(
    ctx: &CrateCtx<'_>,
    segs: &[String],
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<String> {
    let first = segs.first()?.as_str();
    let (module, rest): (Module, &[String]) = match first {
        "crate" => (ctx.crate_module(), &segs[1..]),
        "self" => (ctx.self_module(), &segs[1..]),
        "super" => {
            let mut m = ctx.self_module();
            let mut i = 0;
            while i < segs.len() && segs[i] == "super" {
                m = ctx.parent_of(&m, files);
                i += 1;
            }
            (m, &segs[i..])
        }
        name => {
            if let Some(unit) = unit_by_name(name, meta, files) {
                let root = find_root_file(&unit.dir, files);
                (
                    Module {
                        child_dir: unit.dir.clone(),
                        file: root,
                    },
                    &segs[1..],
                )
            } else if TOOLCHAIN_CRATES.contains(&name) {
                return None;
            } else if let Some(m) = child_module(&ctx.self_module(), name, files, meta, false) {
                // 2018 uniform path: a module in the current scope.
                (m, &segs[1..])
            } else if let Some(m) = child_module(&ctx.crate_module(), name, files, meta, false) {
                // 2015 path: a module at the crate root.
                (m, &segs[1..])
            } else {
                // Third-party crate. Correctly not in this repo.
                return None;
            }
        }
    };
    resolve_in(&module, rest, files, meta)
}

/// Walk `rest` below `m`, backing off trailing segments that name items
/// rather than modules.
fn resolve_in(
    m: &Module,
    rest: &[String],
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<String> {
    if rest.is_empty() {
        return m.file.clone().filter(|f| files.contains(f));
    }
    // A detect-time `#[path]` wins over conventional files, matching rustc.
    if let Some(child) = path_child(m, &rest[0], files, meta) {
        return resolve_in(&child, &rest[1..], files, meta);
    }
    // Longest prefix in the index: `<dir>/a/b.rs`, `<dir>/a/b/mod.rs`, then
    // `<dir>/a.rs`, `<dir>/a/mod.rs`, ...
    for k in (1..=rest.len()).rev() {
        let joined = join_path(&m.child_dir, &rest[..k].join("/"));
        let file_rs = format!("{joined}.rs");
        let mod_rs = join_path(&joined, "mod.rs");
        let file = if files.contains(&file_rs) {
            file_rs
        } else if files.contains(&mod_rs) {
            mod_rs
        } else {
            continue;
        };
        if k == rest.len() {
            return Some(file);
        }
        // Segments remain. They are items defined in `file` — unless `file`
        // declares the next one as a module living elsewhere via `#[path]`.
        let found = Module {
            child_dir: joined,
            file: Some(file),
        };
        if let Some(child) = path_child(&found, &rest[k], files, meta) {
            return resolve_in(&child, &rest[k + 1..], files, meta);
        }
        return found.file;
    }
    // The remaining segments name items defined in this module's file.
    m.file.clone().filter(|f| files.contains(f))
}

/// Child module `name` of `m`: a `#[path]` override when `read_path_attr`
/// is set, otherwise `<dir>/name.rs` or `<dir>/name/mod.rs`.
fn child_module(
    m: &Module,
    name: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
    read_path_attr: bool,
) -> Option<Module> {
    if read_path_attr {
        if let Some(child) = path_child(m, name, files, meta) {
            return Some(child);
        }
    }
    let base = join_path(&m.child_dir, name);
    let file_rs = format!("{base}.rs");
    if files.contains(&file_rs) {
        return Some(Module {
            child_dir: base,
            file: Some(file_rs),
        });
    }
    let mod_rs = join_path(&base, "mod.rs");
    if files.contains(&mod_rs) {
        return Some(Module {
            child_dir: base,
            file: Some(mod_rs),
        });
    }
    None
}

/// Child module `name` of `m` declared with `#[path = "..."]` in `m.file`.
/// Gated on `name` looking like a module so type/constant segments skip the
/// override probe. The target comes from [`ProjectMeta::overrides`].
fn path_child(m: &Module, name: &str, files: &FileIndex, meta: &ProjectMeta) -> Option<Module> {
    if !looks_like_module(name) {
        return None;
    }
    let ov = meta.override_for(m.file.as_deref()?, name)?;
    if !files.contains(&ov.target) {
        return None;
    }
    let target = ov.target.as_str();
    let target_rel = RelPath::new(target);
    let child_dir = if target_rel.file_name() == "mod.rs" {
        target_rel.dir().to_string()
    } else {
        target.strip_suffix(".rs").unwrap_or(target).to_string()
    };
    Some(Module {
        child_dir,
        file: Some(ov.target.clone()),
    })
}

/// `#[path = "foo.rs"]` / `#[cfg_attr(unix, path = "foo.rs")]` → `foo.rs`.
fn quoted_path_attr(attr: &str) -> Option<String> {
    let i = attr.find("path")?;
    let after = attr[i + 4..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// `pub(crate) mod foo;` → `foo`. Inline `mod foo { … }` is not a declaration.
fn mod_decl_name(line: &str) -> Option<&str> {
    let mut s = line.trim();
    if let Some(r) = s.strip_prefix("pub") {
        s = r.trim_start();
        if let Some(r) = s.strip_prefix('(') {
            s = r[r.find(')')? + 1..].trim_start();
        }
    }
    let r = s.strip_prefix("mod")?;
    if !r.starts_with(char::is_whitespace) {
        return None;
    }
    let r = r.trim_start();
    let end = r
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#'))
        .unwrap_or(r.len());
    let name = r[..end].trim_start_matches("r#");
    if name.is_empty() || !r[end..].trim_start().starts_with(';') {
        return None;
    }
    Some(name)
}

/// Module names are snake_case; a segment with an uppercase letter is a type
/// or constant and never has a `#[path]` declaration worth probing for.
fn looks_like_module(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && !name.chars().any(|c| c.is_ascii_uppercase())
}

/// The crate root file for a unit directory.
fn find_root_file(dir: &str, files: &FileIndex) -> Option<String> {
    for cand in ["lib.rs", "main.rs"] {
        let p = join_path(dir, cand);
        if files.contains(&p) {
            return Some(p);
        }
    }
    // `[[test]] path = "tests/tests.rs"` style: file named after its dir.
    let base = basename(dir);
    if !base.is_empty() {
        let p = join_path(dir, &format!("{base}.rs"));
        if files.contains(&p) {
            return Some(p);
        }
    }
    // A single `.rs` file in the directory is the root by elimination.
    let mut rs = files
        .entries(dir)
        .iter()
        .filter(|p| p.extension() == Some("rs"));
    match (rs.next(), rs.next()) {
        (Some(only), None) => Some(only.as_str().to_string()),
        _ => None,
    }
}

/// Longest-`dir`-prefix owner of `path`.
fn owning_unit<'m>(path: &str, meta: &'m ProjectMeta) -> Option<&'m ModuleUnit> {
    meta.modules
        .iter()
        .filter(|u| strip_dir(path, &u.dir).is_some())
        .max_by_key(|u| u.dir.len())
}

/// The unit imported as `name`. Several units may share a name (a package
/// with a `src/` lib and an out-of-tree `[[bin]]`); prefer the library.
fn unit_by_name<'m>(
    name: &str,
    meta: &'m ProjectMeta,
    files: &FileIndex,
) -> Option<&'m ModuleUnit> {
    let mut best: Option<(u8, &ModuleUnit)> = None;
    for u in meta.modules.iter().filter(|u| u.name == name) {
        let score = if files.contains(&join_path(&u.dir, "lib.rs")) {
            2
        } else if files.has_dir(&u.dir) {
            1
        } else {
            0
        };
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, u));
        }
    }
    best.map(|(_, u)| u)
}

// -------------------------------------------------------------- spec text

struct Cleaned {
    body: String,
    is_mod_decl: bool,
}

/// Normalize whatever the caller hands us — a bare path, a brace group, or
/// the raw statement text (`pub(crate) use a::{b, c};`, possibly multi-line
/// with comments) — into just the path part.
fn clean_spec(spec: &str) -> Cleaned {
    let mut flat = String::with_capacity(spec.len());
    for line in spec.lines() {
        let line = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        flat.push_str(line.trim());
        flat.push(' ');
    }
    let mut s = flat.trim().trim_end_matches(';').trim();

    if let Some(rest) = s.strip_prefix("pub") {
        if rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
            let rest = rest.trim_start();
            s = match rest.strip_prefix('(') {
                Some(r) => match r.find(')') {
                    Some(i) => r[i + 1..].trim_start(),
                    None => r,
                },
                None => rest,
            };
        }
    }

    let mut is_mod_decl = false;
    if let Some(rest) = strip_keyword(s, "use") {
        s = rest;
    } else if let Some(rest) = strip_keyword(s, "mod") {
        s = rest;
        is_mod_decl = true;
    } else if let Some(rest) = strip_keyword(s, "extern") {
        s = strip_keyword(rest, "crate").unwrap_or(rest);
    }
    Cleaned {
        body: s.trim().to_string(),
        is_mod_decl,
    }
}

fn strip_keyword<'s>(s: &'s str, kw: &str) -> Option<&'s str> {
    let rest = s.strip_prefix(kw)?;
    if rest.starts_with(char::is_whitespace) || rest.starts_with(['{', ':']) {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// `a::{b, c::{d, e}}` → `[[a, b], [a, c, d], [a, c, e]]`. Tolerates
/// unbalanced braces (never panics; takes what it can).
fn expand_use_tree(body: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    expand_into(body, &[], &mut out, 0);
    out
}

fn expand_into(s: &str, prefix: &[String], out: &mut Vec<Vec<String>>, depth: usize) {
    if depth > 64 || out.len() > 4096 {
        return;
    }
    for item in split_top_level(s) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        match item.find('{') {
            Some(open) => {
                let close = matching_brace(item, open).unwrap_or(item.len());
                let mut pre = prefix.to_vec();
                pre.extend(split_segments(&item[..open]));
                expand_into(&item[open + 1..close], &pre, out, depth + 1);
            }
            None => {
                let mut segs = prefix.to_vec();
                segs.extend(split_segments(item));
                // `a::*` and `a::{self}` both name module `a`.
                while segs.len() > 1
                    && matches!(segs.last().map(String::as_str), Some("*" | "self"))
                {
                    segs.pop();
                }
                if segs.last().map(String::as_str) == Some("*") {
                    segs.pop();
                }
                if !segs.is_empty() {
                    out.push(segs);
                }
            }
        }
    }
}

/// Split on commas that are not nested inside braces.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Byte index of the `}` matching the `{` at `open`.
fn matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
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

/// `a::b as c` → `[a, b]`; `::std::x` → `[std, x]`; `r#type` → `type`.
/// Trailing `*` / `self` are handled by the caller once the prefix is known.
fn split_segments(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = match s.find(" as ") {
        Some(i) => &s[..i],
        None => s,
    };
    s.split("::")
        .map(|t| t.trim().trim_matches(['{', '}']).trim_start_matches("r#"))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

// ------------------------------------------------------------ path helpers

fn join_path(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    }
}

fn parent_dir(d: &str) -> &str {
    match d.rfind('/') {
        Some(i) => &d[..i],
        None => "",
    }
}

fn basename(d: &str) -> &str {
    d.rsplit('/').next().unwrap_or(d)
}

/// `path` relative to `dir`, or `None` if `path` is not inside `dir`.
fn strip_dir<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
    if dir.is_empty() {
        return Some(path);
    }
    if path == dir {
        return Some("");
    }
    path.strip_prefix(dir)?.strip_prefix('/')
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn idx(paths: &[&str]) -> FileIndex {
        FileIndex::new(
            "/nonexistent-astrolabe-test-root",
            paths.iter().map(RelPath::new),
        )
    }

    fn unit(name: &str, dir: &str) -> ModuleUnit {
        ModuleUnit {
            name: name.into(),
            dir: dir.into(),
        }
    }

    fn meta(units: &[ModuleUnit]) -> ProjectMeta {
        ProjectMeta {
            modules: units.to_vec(),
            ..Default::default()
        }
    }

    fn res(files: &FileIndex, meta: &ProjectMeta, from: &str, spec: &str) -> Option<String> {
        RustResolver
            .resolve(&RelPath::new(from), spec, files, meta)
            .map(|p| p.as_str().to_string())
    }

    /// A throw-away directory tree under the OS temp dir, for tests that
    /// need `FileIndex::read` to hit real files (`detect`).
    struct TempRepo {
        root: PathBuf,
        paths: Vec<String>,
    }

    impl TempRepo {
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "astrolabe-rust-resolver-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            let mut paths = Vec::new();
            for (rel, contents) in files {
                let full = root.join(rel);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, contents).unwrap();
                paths.push(rel.to_string());
            }
            TempRepo { root, paths }
        }

        fn index(&self) -> FileIndex {
            FileIndex::new(&self.root, self.paths.iter().map(RelPath::new))
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    // ---------------------------------------------------------- detect

    #[test]
    fn detect_maps_hyphenated_crate_names_to_src() {
        let repo = TempRepo::new(
            "detect",
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"crates/*\", \"tools/extra\"]\n",
                ),
                (
                    "crates/grep-searcher/Cargo.toml",
                    "[package]\nname = \"grep-searcher\"\n",
                ),
                ("crates/grep-searcher/src/lib.rs", ""),
                (
                    "crates/globset/Cargo.toml",
                    "[package]\nname = \"globset\"\n[lib]\nname = \"globset\"\n",
                ),
                ("crates/globset/src/lib.rs", ""),
                (
                    "tools/extra/Cargo.toml",
                    "[package]\nname = \"extra-tool\"\n",
                ),
                ("tools/extra/src/main.rs", ""),
            ],
        );
        let files = repo.index();
        let m = RustResolver.detect(&files);

        assert!(m
            .modules
            .contains(&unit("grep_searcher", "crates/grep-searcher/src")));
        assert!(m.modules.contains(&unit("globset", "crates/globset/src")));
        assert!(m.modules.contains(&unit("extra_tool", "tools/extra/src")));
        // The workspace-only root manifest has no `[package]` → no unit.
        assert!(!m.modules.iter().any(|u| u.dir == "src"));
        assert!(m.excludes.contains(&"target".to_string()));
        assert!(m.source_roots.contains(&"crates/globset/src".to_string()));
    }

    #[test]
    fn detect_adds_units_for_out_of_tree_targets_and_path_deps() {
        let repo = TempRepo::new(
            "targets",
            &[
                (
                    "Cargo.toml",
                    r#"
[package]
name = "ripgrep"

[[bin]]
name = "rg"
path = "crates/core/main.rs"

[[test]]
name = "integration"
path = "tests/tests.rs"

[workspace]
members = ["crates/grep"]

[dependencies]
grep = { version = "0.4", path = "crates/grep" }
renamed = { package = "grep", path = "crates/grep" }
serde = "1"
"#,
                ),
                ("crates/core/main.rs", ""),
                ("tests/tests.rs", ""),
                ("crates/grep/Cargo.toml", "[package]\nname = \"grep\"\n"),
                ("crates/grep/src/lib.rs", ""),
            ],
        );
        let files = repo.index();
        let m = RustResolver.detect(&files);

        // Spec: every package gets `<dir>/src`, even when it does not exist.
        assert!(m.modules.contains(&unit("ripgrep", "src")));
        assert!(m.modules.contains(&unit("ripgrep", "crates/core")));
        assert!(m.modules.contains(&unit("ripgrep", "tests")));
        assert!(m.modules.contains(&unit("grep", "crates/grep/src")));
        // Path dependency under the key used in source.
        assert!(m.modules.contains(&unit("renamed", "crates/grep/src")));
        // No duplicate for `grep`, which is both a member and a path dep.
        assert_eq!(m.modules.iter().filter(|u| u.name == "grep").count(), 1);
    }

    #[test]
    fn detect_survives_malformed_manifest() {
        let repo = TempRepo::new(
            "malformed",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"broken-crate\"\nversion = \n",
                ),
                ("src/lib.rs", ""),
            ],
        );
        let m = RustResolver.detect(&repo.index());
        assert!(m.modules.contains(&unit("broken_crate", "src")));
    }

    // ------------------------------------------------------- anchoring

    #[test]
    fn crate_anchors_to_owning_crate_src_not_repo_root() {
        let files = idx(&[
            "crates/a/src/lib.rs",
            "crates/a/src/foo.rs",
            "crates/b/src/lib.rs",
            "crates/b/src/foo.rs",
            "foo.rs",
        ]);
        let m = meta(&[unit("a", "crates/a/src"), unit("b", "crates/b/src")]);
        assert_eq!(
            res(&files, &m, "crates/a/src/bar.rs", "crate::foo::Bar"),
            Some("crates/a/src/foo.rs".into())
        );
        assert_eq!(
            res(&files, &m, "crates/b/src/deep/x.rs", "crate::foo"),
            Some("crates/b/src/foo.rs".into())
        );
        // Item directly at the crate root → the root file.
        assert_eq!(
            res(&files, &m, "crates/a/src/bar.rs", "crate::Thing"),
            Some("crates/a/src/lib.rs".into())
        );
        assert_eq!(
            res(&files, &m, "crates/a/src/bar.rs", "crate::*"),
            Some("crates/a/src/lib.rs".into())
        );
    }

    #[test]
    fn self_resolves_relative_to_current_module() {
        let files = idx(&[
            "src/lib.rs",
            "src/net/mod.rs",
            "src/net/tcp.rs",
            "src/io.rs",
            "src/io/buf.rs",
        ]);
        let m = meta(&[unit("x", "src")]);
        // mod.rs layout: children live beside it.
        assert_eq!(
            res(&files, &m, "src/net/mod.rs", "self::tcp::Stream"),
            Some("src/net/tcp.rs".into())
        );
        // foo.rs layout: children live in foo/.
        assert_eq!(
            res(&files, &m, "src/io.rs", "self::buf::Reader"),
            Some("src/io/buf.rs".into())
        );
        // `self::Item` is the file itself.
        assert_eq!(
            res(&files, &m, "src/io.rs", "self::Kind::*"),
            Some("src/io.rs".into())
        );
        // From the crate root, `self` is the root.
        assert_eq!(
            res(&files, &m, "src/lib.rs", "self::io::Reader"),
            Some("src/io.rs".into())
        );
    }

    #[test]
    fn super_climbs_one_module_level_per_step() {
        let files = idx(&[
            "src/lib.rs",
            "src/a/mod.rs",
            "src/a/y.rs",
            "src/a/b/mod.rs",
            "src/a/b/c.rs",
            "src/a/b/x.rs",
            "src/flat.rs",
            "src/flat/leaf.rs",
        ]);
        let m = meta(&[unit("x", "src")]);
        let from = "src/a/b/c.rs";
        assert_eq!(
            res(&files, &m, from, "super::x::X"),
            Some("src/a/b/x.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "super::Thing"),
            Some("src/a/b/mod.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "super::*"),
            Some("src/a/b/mod.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "super::super::y::Y"),
            Some("src/a/y.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "super::super::Y"),
            Some("src/a/mod.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "super::super::super::Z"),
            Some("src/lib.rs".into())
        );
        // Climbing past the root clamps at the root instead of failing.
        assert_eq!(
            res(&files, &m, from, "super::super::super::super::Z"),
            Some("src/lib.rs".into())
        );
        // `foo/mod.rs` and `foo.rs` sit at the same module level: their
        // parent is the crate root.
        assert_eq!(
            res(&files, &m, "src/a/mod.rs", "super::Q"),
            Some("src/lib.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/flat.rs", "super::Q"),
            Some("src/lib.rs".into())
        );
        // ... whereas their children's parent is them.
        assert_eq!(
            res(&files, &m, "src/flat/leaf.rs", "super::Q"),
            Some("src/flat.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/a/y.rs", "super::Q"),
            Some("src/a/mod.rs".into())
        );
    }

    #[test]
    fn trailing_segments_back_off_to_deepest_module_file() {
        let files = idx(&[
            "src/lib.rs",
            "src/foo.rs",
            "src/foo/bar/mod.rs",
            "src/flags/mod.rs",
            "src/flags/defs.rs",
        ]);
        let m = meta(&[unit("x", "src")]);
        let from = "src/other.rs";
        // `Bar` is a type: `foo/Bar.rs` misses, `foo.rs` hits.
        assert_eq!(
            res(&files, &m, from, "crate::foo::Bar"),
            Some("src/foo.rs".into())
        );
        // Enum variant path: two trailing item segments.
        assert_eq!(
            res(&files, &m, from, "crate::foo::Kind::Variant"),
            Some("src/foo.rs".into())
        );
        // Deepest existing module wins over a shallower one.
        assert_eq!(
            res(&files, &m, from, "crate::foo::bar::Baz"),
            Some("src/foo/bar/mod.rs".into())
        );
        // mod.rs layout with an item at the end.
        assert_eq!(
            res(&files, &m, from, "crate::flags::defs::FLAGS"),
            Some("src/flags/defs.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "crate::flags::Flag"),
            Some("src/flags/mod.rs".into())
        );
    }

    #[test]
    fn cross_crate_imports_anchor_at_that_crates_src() {
        let files = idx(&[
            "crates/searcher/src/lib.rs",
            "crates/searcher/src/sinks.rs",
            "crates/printer/src/lib.rs",
            "crates/printer/src/json.rs",
            "crates/core/main.rs",
            "tests/tests.rs",
        ]);
        let m = meta(&[
            unit("grep_searcher", "crates/searcher/src"),
            unit("grep_printer", "crates/printer/src"),
            unit("ripgrep", "src"),
            unit("ripgrep", "crates/core"),
            unit("ripgrep", "tests"),
        ]);
        assert_eq!(
            res(
                &files,
                &m,
                "crates/printer/src/json.rs",
                "grep_searcher::SearcherBuilder"
            ),
            Some("crates/searcher/src/lib.rs".into())
        );
        assert_eq!(
            res(
                &files,
                &m,
                "crates/printer/src/json.rs",
                "grep_searcher::sinks::UTF8"
            ),
            Some("crates/searcher/src/sinks.rs".into())
        );
        // Importing crate names are underscore form even though the package
        // is hyphenated — `detect` already normalized `grep-printer`.
        assert_eq!(
            res(&files, &m, "crates/core/main.rs", "grep_printer::JSON"),
            Some("crates/printer/src/lib.rs".into())
        );
        // A file that belongs to no crate (bench/example) still reaches
        // workspace crates by name.
        assert_eq!(
            res(
                &files,
                &m,
                "crates/searcher/examples/demo.rs",
                "grep_printer::json::JSON"
            ),
            Some("crates/printer/src/json.rs".into())
        );
    }

    #[test]
    fn explicit_bin_and_test_target_paths_are_crate_roots() {
        let files = idx(&[
            "crates/core/main.rs",
            "crates/core/flags/mod.rs",
            "crates/core/flags/defs.rs",
            "crates/core/flags/lowargs.rs",
            "tests/tests.rs",
            "tests/util.rs",
            "tests/hay.rs",
            "tests/index/mod.rs",
            "tests/index/basic.rs",
        ]);
        let m = meta(&[
            unit("ripgrep", "src"),
            unit("ripgrep", "crates/core"),
            unit("ripgrep", "tests"),
        ]);
        assert_eq!(
            res(
                &files,
                &m,
                "crates/core/flags/defs.rs",
                "crate::flags::lowargs::ContextSeparator as Sep"
            ),
            Some("crates/core/flags/lowargs.rs".into())
        );
        assert_eq!(
            res(
                &files,
                &m,
                "crates/core/flags/defs.rs",
                "super::CompletionType"
            ),
            Some("crates/core/flags/mod.rs".into())
        );
        assert_eq!(
            res(
                &files,
                &m,
                "crates/core/main.rs",
                "crate::flags::{HiArgs, SearchMode}"
            ),
            Some("crates/core/flags/mod.rs".into())
        );
        // `tests/tests.rs` is the root of the integration-test crate.
        assert_eq!(
            res(
                &files,
                &m,
                "tests/index/basic.rs",
                "crate::util::{Dir, TestCommand}"
            ),
            Some("tests/util.rs".into())
        );
        assert_eq!(
            res(&files, &m, "tests/index/basic.rs", "super::Shared"),
            Some("tests/index/mod.rs".into())
        );
        assert_eq!(
            res(&files, &m, "tests/util.rs", "crate::hay::SHERLOCK"),
            Some("tests/hay.rs".into())
        );
    }

    #[test]
    fn standalone_files_are_their_own_crate_root() {
        let files = idx(&[
            "crates/ignore/src/lib.rs",
            "crates/ignore/src/gitignore.rs",
            "crates/ignore/tests/skip_bom.rs",
            "crates/ignore/src/bin/tool.rs",
            "crates/ignore/src/bin/multi/main.rs",
            "crates/ignore/src/bin/multi/cli.rs",
        ]);
        let m = meta(&[unit("ignore", "crates/ignore/src")]);
        assert_eq!(
            res(
                &files,
                &m,
                "crates/ignore/tests/skip_bom.rs",
                "ignore::gitignore::GitignoreBuilder"
            ),
            Some("crates/ignore/src/gitignore.rs".into())
        );
        assert_eq!(
            res(
                &files,
                &m,
                "crates/ignore/tests/skip_bom.rs",
                "crate::helper"
            ),
            Some("crates/ignore/tests/skip_bom.rs".into())
        );
        // src/bin targets are separate crates, not modules of the lib.
        assert_eq!(
            res(&files, &m, "crates/ignore/src/bin/tool.rs", "crate::Thing"),
            Some("crates/ignore/src/bin/tool.rs".into())
        );
        assert_eq!(
            res(
                &files,
                &m,
                "crates/ignore/src/bin/multi/cli.rs",
                "crate::cli::Args"
            ),
            Some("crates/ignore/src/bin/multi/cli.rs".into())
        );
        assert_eq!(
            res(
                &files,
                &m,
                "crates/ignore/src/bin/multi/cli.rs",
                "crate::Root"
            ),
            Some("crates/ignore/src/bin/multi/main.rs".into())
        );
    }

    #[test]
    fn edition_2015_and_uniform_paths_find_local_modules() {
        let files = idx(&[
            "src/lib.rs",
            "src/index.rs",
            "src/util/mod.rs",
            "src/util/fmt.rs",
        ]);
        let m = meta(&[unit("grep_index", "src")]);
        // `pub use index::{Index}` in lib.rs with `mod index;`.
        assert_eq!(
            res(
                &files,
                &m,
                "src/lib.rs",
                "pub use index::{Index, IndexBuilder};"
            ),
            Some("src/index.rs".into())
        );
        // 2015: crate-root module from a nested file.
        assert_eq!(
            res(&files, &m, "src/util/fmt.rs", "index::Index"),
            Some("src/index.rs".into())
        );
        // Sibling module visible from the current scope.
        assert_eq!(
            res(&files, &m, "src/util/mod.rs", "fmt::pretty"),
            Some("src/util/fmt.rs".into())
        );
    }

    #[test]
    fn external_and_toolchain_crates_return_none() {
        let files = idx(&["crates/a/src/lib.rs", "crates/a/src/foo.rs"]);
        let m = meta(&[unit("a", "crates/a/src")]);
        let from = "crates/a/src/foo.rs";
        assert_eq!(res(&files, &m, from, "serde::Serialize"), None);
        assert_eq!(res(&files, &m, from, "std::io::Write"), None);
        assert_eq!(res(&files, &m, from, "core::fmt"), None);
        assert_eq!(res(&files, &m, from, "alloc::vec::Vec"), None);
        assert_eq!(res(&files, &m, from, "::regex::Regex"), None);
        assert_eq!(res(&files, &m, from, "grep_matcher::Matcher"), None);
        assert_eq!(res(&files, &m, from, "use std::{io, fmt};"), None);
        assert_eq!(res(&files, &m, from, ""), None);
    }

    // ------------------------------------------------------- spec forms

    #[test]
    fn brace_groups_aliases_globs_and_raw_statements() {
        let files = idx(&["src/lib.rs", "src/foo.rs", "src/baz.rs"]);
        let m = meta(&[unit("x", "src")]);
        let from = "src/other.rs";
        let foo = Some("src/foo.rs".to_string());

        assert_eq!(
            res(&files, &m, from, "crate::{foo::Bar, baz::Qux}"),
            foo.clone()
        );
        assert_eq!(
            res(&files, &m, from, "crate::foo::{self, Bar}"),
            foo.clone()
        );
        assert_eq!(
            res(&files, &m, from, "crate::foo::{Bar as B, Baz}"),
            foo.clone()
        );
        assert_eq!(res(&files, &m, from, "crate::foo::Bar as B"), foo.clone());
        assert_eq!(res(&files, &m, from, "crate::foo::*"), foo.clone());
        assert_eq!(
            res(&files, &m, from, "pub use crate::foo::Bar;"),
            foo.clone()
        );
        assert_eq!(
            res(&files, &m, from, "pub(crate) use crate::foo::Bar;"),
            foo.clone()
        );
        assert_eq!(
            res(&files, &m, from, "pub(in crate::x) use crate::foo::Bar;"),
            foo.clone()
        );
        assert_eq!(res(&files, &m, from, "use crate::r#foo::Bar;"), foo.clone());
        assert_eq!(
            res(&files, &m, from, "extern crate x;"),
            Some("src/lib.rs".into())
        );
        // Multi-line with nested groups and a comment, as extracted raw.
        let raw = "pub use crate::{\n    // re-exports\n    baz::{Qux, Quux},\n    foo::{Bar, Kind::*},\n};";
        assert_eq!(res(&files, &m, from, raw), Some("src/baz.rs".into()));
        // A group whose first item is an item at the root still yields the
        // root, and later items are tried when earlier ones fail.
        assert_eq!(
            res(&files, &m, from, "crate::{Missing, foo::Bar}"),
            Some("src/lib.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "{crate::nothing::here, crate::foo::Bar}"),
            Some("src/lib.rs".into())
        );
    }

    #[test]
    fn malformed_specs_never_panic() {
        let files = idx(&["src/lib.rs", "src/foo.rs"]);
        let m = meta(&[unit("x", "src")]);
        let from = "src/other.rs";
        for spec in [
            "crate::{foo",
            "crate::foo}",
            "crate::{{{",
            "}}}",
            "{",
            "::",
            ":::::",
            "crate::",
            "use",
            "pub(",
            "pub",
            ",,,",
            "crate::{,}",
            "as",
            " as ",
            "crate::foo as",
            "*",
            "self",
            "super",
            "🦀::🦀",
            "mod",
            "mod ;",
        ] {
            let _ = res(&files, &m, from, spec);
        }
        // Partial input still yields the sensible answer.
        assert_eq!(
            res(&files, &m, from, "crate::{foo"),
            Some("src/foo.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "crate::foo}"),
            Some("src/foo.rs".into())
        );
    }

    #[test]
    fn use_group_resolves_every_target_file() {
        let files = FileIndex::new(
            "/nonexistent-astrolabe-rust-group",
            ["src/lib.rs", "src/a.rs", "src/b.rs", "src/main.rs"].map(RelPath::new),
        );
        let m = meta(&[unit("x", "src")]);
        let from = RelPath::new("src/main.rs");
        match RustResolver.resolve_import(&from, "use crate::{a::X, b::Y};", &files, &m) {
            ImportResolution::Files(v) => {
                assert_eq!(v, vec![RelPath::new("src/a.rs"), RelPath::new("src/b.rs")])
            }
            other => panic!("expected two files, got {other:?}"),
        }
        assert_eq!(
            RustResolver.resolve_import(&from, "use crate::{a::X, a::Z};", &files, &m),
            ImportResolution::File(RelPath::new("src/a.rs"))
        );
        assert_eq!(
            res(&files, &m, "src/main.rs", "use crate::{a::X, b::Y};"),
            Some("src/a.rs".into())
        );
    }

    #[test]
    fn lib_name_differs_from_package_name() {
        let repo = TempRepo::new(
            "libname",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"foo-bar\"\n[lib]\nname = \"foo\"\n",
                ),
                ("src/lib.rs", "pub mod m;"),
                ("src/m.rs", ""),
                ("src/main.rs", ""),
            ],
        );
        let files = repo.index();
        let m = RustResolver.detect(&files);
        assert_eq!(
            res(&files, &m, "src/main.rs", "use foo::m::T;"),
            Some("src/m.rs".into())
        );
    }

    #[test]
    fn nested_groups_mod_decl_and_workspace_path_dep_all_resolve() {
        let repo = TempRepo::new(
            "nested",
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"app\", \"libs/*\"]\n"),
                (
                    "app/Cargo.toml",
                    "[package]\nname = \"app\"\n[dependencies]\nutil = { path = \"../libs/util\" }\nserde = \"1\"\n",
                ),
                ("app/src/main.rs", "mod foo;\nmod bar;"),
                ("app/src/foo.rs", ""),
                ("app/src/bar/mod.rs", "pub mod deep;"),
                ("app/src/bar/deep.rs", ""),
                ("libs/util/Cargo.toml", "[package]\nname = \"util\"\n"),
                ("libs/util/src/lib.rs", "pub mod io;"),
                ("libs/util/src/io.rs", ""),
            ],
        );
        let files = repo.index();
        let m = RustResolver.detect(&files);
        let from = RelPath::new("app/src/main.rs");
        let r = RustResolver.resolve_import(
            &from,
            "use {crate::{foo::A, bar::{deep::B, self}}, util::io::C, serde::X};",
            &files,
            &m,
        );
        match r {
            ImportResolution::Files(v) => {
                let v: Vec<&str> = v.iter().map(|p| p.as_str()).collect();
                for want in [
                    "app/src/foo.rs",
                    "app/src/bar/deep.rs",
                    "libs/util/src/io.rs",
                ] {
                    assert!(v.contains(&want), "missing {want} in {v:?}");
                }
                assert!(!v.iter().any(|p| p.contains("serde")));
            }
            other => panic!("expected files, got {other:?}"),
        }
        assert_eq!(
            res(&files, &m, "app/src/main.rs", "use serde::Deserialize;"),
            None
        );
        assert_eq!(
            res(&files, &m, "app/src/main.rs", "mod foo;"),
            Some("app/src/foo.rs".into())
        );
        assert_eq!(
            res(&files, &m, "app/src/main.rs", "use util::io::C;"),
            Some("libs/util/src/io.rs".into())
        );
    }

    #[test]
    fn expand_use_tree_flattens_nested_groups() {
        let paths = expand_use_tree("a::{b, c::{d, e as f}, g::*, h::{self}}");
        let joined: Vec<String> = paths.iter().map(|p| p.join("::")).collect();
        assert_eq!(joined, ["a::b", "a::c::d", "a::c::e", "a::g", "a::h"]);
        assert_eq!(expand_use_tree("::std::io"), vec![vec!["std", "io"]]);
        assert_eq!(expand_use_tree("super::*"), vec![vec!["super"]]);
    }

    // --------------------------------------------- mod decls and #[path]

    #[test]
    fn mod_declarations_and_path_attribute_overrides() {
        let repo = TempRepo::new(
            "pathattr",
            &[
                ("Cargo.toml", "[package]\nname = \"rg\"\n[[bin]]\nname = \"rg\"\npath = \"core/main.rs\"\n"),
                ("core/main.rs", "mod index;\nmod flags;\n"),
                ("core/flags.rs", ""),
                (
                    "core/index/mod.rs",
                    "pub(crate) use self::imp::*;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"disabled.rs\"]\nmod imp;\n#[cfg(feature = \"x\")]\n#[path = \"enabled.rs\"]\nmod imp;\n#[path = \"../other/one_line.rs\"] pub mod inline;\n",
                ),
                ("core/index/disabled.rs", "pub fn run() {}"),
                ("core/index/enabled.rs", "pub fn run() {}"),
                ("core/other/one_line.rs", ""),
            ],
        );
        let files = repo.index();
        let m = RustResolver.detect(&files);
        assert!(m.modules.contains(&unit("rg", "core")));

        let from = "core/index/mod.rs";
        // `use self::imp::*` goes through the `#[path]` override.
        assert_eq!(
            res(&files, &m, from, "pub(crate) use self::imp::*;"),
            Some("core/index/disabled.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "self::imp::run"),
            Some("core/index/disabled.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "crate::index::imp::run"),
            Some("core/index/disabled.rs".into())
        );
        // Attribute and declaration on one line, relative `..` path.
        assert_eq!(
            res(&files, &m, from, "self::inline::X"),
            Some("core/other/one_line.rs".into())
        );
        // `mod x;` declarations: x.rs, x/mod.rs, and #[path].
        assert_eq!(
            res(&files, &m, "core/main.rs", "mod index;"),
            Some("core/index/mod.rs".into())
        );
        assert_eq!(
            res(&files, &m, "core/main.rs", "mod flags;"),
            Some("core/flags.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "mod imp;"),
            Some("core/index/disabled.rs".into())
        );
        assert_eq!(
            res(&files, &m, from, "pub mod inline;"),
            Some("core/other/one_line.rs".into())
        );
        assert_eq!(res(&files, &m, "core/main.rs", "mod nothing;"), None);
    }

    #[test]
    fn detect_collects_path_overrides_in_cfg_attr_same_line_and_cfg() {
        let repo = TempRepo::new(
            "pathcollect",
            &[
                ("Cargo.toml", "[package]\nname = \"p\"\n"),
                (
                    "src/lib.rs",
                    "#[cfg_attr(unix, path = \"via_cfg_attr.rs\")]\nmod via_cfg_attr;\n",
                ),
                ("src/via_cfg_attr.rs", ""),
                (
                    "src/sub/mod.rs",
                    "#[path = \"same_line.rs\"] pub mod inline;\n",
                ),
                ("src/sub/same_line.rs", ""),
                (
                    "src/file_mod.rs",
                    "#[cfg(not(feature = \"x\"))]\n#[path = \"gated.rs\"]\nmod gated;\n",
                ),
                ("src/file_mod/child.rs", ""),
                ("src/gated.rs", ""),
            ],
        );
        let files = repo.index();
        let m = RustResolver.detect(&files);

        assert_eq!(
            m.override_for("src/lib.rs", "via_cfg_attr"),
            Some(&PathOverride {
                declared_in: "src/lib.rs".into(),
                name: "via_cfg_attr".into(),
                target: "src/via_cfg_attr.rs".into(),
            })
        );
        assert_eq!(
            m.override_for("src/sub/mod.rs", "inline"),
            Some(&PathOverride {
                declared_in: "src/sub/mod.rs".into(),
                name: "inline".into(),
                target: "src/sub/same_line.rs".into(),
            })
        );
        assert_eq!(
            m.override_for("src/file_mod.rs", "gated"),
            Some(&PathOverride {
                declared_in: "src/file_mod.rs".into(),
                name: "gated".into(),
                target: "src/gated.rs".into(),
            })
        );
        let mut sorted = m.overrides.clone();
        sorted.sort();
        assert_eq!(m.overrides, sorted);
    }

    #[test]
    fn resolve_uses_override_else_conventional_fallback() {
        let files = idx(&[
            "src/lib.rs",
            "src/normal.rs",
            "src/elsewhere.rs",
            "src/shadowed.rs",
        ]);
        let mut m = meta(&[unit("x", "src")]);
        m.overrides.push(PathOverride {
            declared_in: "src/lib.rs".into(),
            name: "moved".into(),
            target: "src/elsewhere.rs".into(),
        });
        m.overrides.push(PathOverride {
            declared_in: "src/lib.rs".into(),
            name: "shadowed".into(),
            target: "src/elsewhere.rs".into(),
        });

        assert_eq!(
            res(&files, &m, "src/lib.rs", "mod moved;"),
            Some("src/elsewhere.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/lib.rs", "self::moved::X"),
            Some("src/elsewhere.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/lib.rs", "mod shadowed;"),
            Some("src/elsewhere.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/lib.rs", "mod normal;"),
            Some("src/normal.rs".into())
        );
        assert_eq!(res(&files, &m, "src/lib.rs", "mod missing;"), None);
    }

    #[test]
    fn resolve_path_override_does_not_read_disk() {
        let files = FileIndex::new(
            "/nonexistent-astrolabe-rust-resolve-no-io",
            ["src/lib.rs", "src/elsewhere.rs"].map(RelPath::new),
        );
        assert!(
            files.read("src/lib.rs").is_none(),
            "index root must not exist on disk"
        );
        let mut m = meta(&[unit("x", "src")]);
        m.overrides.push(PathOverride {
            declared_in: "src/lib.rs".into(),
            name: "imp".into(),
            target: "src/elsewhere.rs".into(),
        });
        assert_eq!(
            res(&files, &m, "src/lib.rs", "mod imp;"),
            Some("src/elsewhere.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/lib.rs", "self::imp::run"),
            Some("src/elsewhere.rs".into())
        );
        assert_eq!(
            res(&files, &m, "src/lib.rs", "crate::imp::run"),
            Some("src/elsewhere.rs".into())
        );
    }

    #[test]
    fn helper_parsers() {
        assert_eq!(mod_decl_name("mod foo;"), Some("foo"));
        assert_eq!(mod_decl_name("pub(crate) mod foo ;"), Some("foo"));
        assert_eq!(mod_decl_name("pub mod r#type;"), Some("type"));
        assert_eq!(mod_decl_name("mod foo {"), None);
        assert_eq!(mod_decl_name("module foo;"), None);
        assert_eq!(
            quoted_path_attr("#[path = \"a/b.rs\"]").as_deref(),
            Some("a/b.rs")
        );
        assert_eq!(
            quoted_path_attr("#[cfg_attr(unix, path=\"u.rs\")]").as_deref(),
            Some("u.rs")
        );
        assert_eq!(quoted_path_attr("#[cfg(feature = \"pathutil\")]"), None);
        assert_eq!(normalize_crate_name("grep-searcher"), "grep_searcher");
        assert!(looks_like_module("imp") && looks_like_module("_x"));
        assert!(
            !looks_like_module("Bar") && !looks_like_module("SHERLOCK") && !looks_like_module("*")
        );
    }

    // --------------------------------------------------- real corpus

    /// Every `use` statement in the `ripgrep` corpus that points inside the
    /// workspace must resolve. Run with `cargo test -p astrolabe-core rust
    /// -- --ignored --nocapture` to see the numbers.
    #[test]
    #[ignore]
    fn corpus_ripgrep_resolves_every_in_workspace_use() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/rust");
        let root = root.canonicalize().expect("corpus/rust is missing");
        let mut paths = Vec::new();
        walk_dir(&root, &root, &mut paths);
        let files = FileIndex::new(&root, paths.iter().map(RelPath::new));
        let r = RustResolver;
        let detect_t0 = std::time::Instant::now();
        let m = r.detect(&files);
        let detect_elapsed = detect_t0.elapsed();
        let scan_t0 = std::time::Instant::now();
        let scanned = collect_path_overrides(&files, &m.modules);
        let scan_elapsed = scan_t0.elapsed();

        println!("\n== detected units ==");
        for u in &m.modules {
            println!("  {:<16} {}", u.name, u.dir);
        }
        println!("\n== detect cost ==");
        println!("  detect                        : {detect_elapsed:?}");
        println!("  path-override scan (included)  : {scan_elapsed:?}");
        println!("  path overrides                : {}", m.overrides.len());
        for o in &m.overrides {
            println!("    {} :: {} -> {}", o.declared_in, o.name, o.target);
        }
        assert_eq!(scanned, m.overrides);
        assert_eq!(
            m.override_for("crates/core/index/mod.rs", "imp")
                .map(|o| o.target.as_str()),
            Some("crates/core/index/disabled.rs")
        );
        for expected in [
            "globset",
            "grep",
            "grep_cli",
            "grep_index",
            "grep_matcher",
            "grep_pcre2",
            "grep_printer",
            "grep_regex",
            "grep_searcher",
            "ignore",
        ] {
            assert!(
                m.modules
                    .iter()
                    .any(|u| u.name == expected && u.dir.ends_with("/src")),
                "missing crate {expected}"
            );
        }

        let mut total = 0usize;
        let mut resolved = 0usize;
        let mut top_total = 0usize;
        let mut top_resolved = 0usize;
        let mut items_total = 0usize;
        let mut items_resolved = 0usize;
        let mut mods_total = 0usize;
        let mut mods_resolved = 0usize;
        let mut misses = Vec::new();

        for p in files.iter().filter(|p| p.extension() == Some("rs")) {
            let text = files.read(p.as_str()).unwrap();
            let masked = mask_rust(&text);
            for stmt in extract_use_statements(&masked) {
                let cleaned = clean_spec(&stmt.text);
                let items = expand_use_tree(&cleaned.body);
                // A statement points into the workspace if any of its items
                // does (`use { termcolor::X, grep::Y }` counts).
                if !items
                    .iter()
                    .any(|i| first_segment_in_workspace(&i.join("::"), &m))
                {
                    continue;
                }
                total += 1;
                top_total += stmt.top_level as usize;
                match r.resolve(p, &stmt.text, &files, &m) {
                    Some(t) => {
                        assert!(files.contains(t.as_str()), "{p}: resolved to missing {t}");
                        resolved += 1;
                        top_resolved += stmt.top_level as usize;
                    }
                    None => misses.push(format!("{p}:{}: {}", stmt.line, stmt.text)),
                }
                // Expanded items, as a caller that pre-expands groups would
                // pass them. `use { grep::X, termcolor::Y }` mixes in- and
                // out-of-workspace items, so classify each item on its own.
                for item in &items {
                    let spec = item.join("::");
                    if !first_segment_in_workspace(&spec, &m) {
                        continue;
                    }
                    items_total += 1;
                    if r.resolve(p, &spec, &files, &m).is_some() {
                        items_resolved += 1;
                    } else {
                        misses.push(format!("{p}:{}: item {spec}", stmt.line));
                    }
                }
            }
            for line in masked.lines() {
                if let Some(name) = mod_decl_name(line) {
                    mods_total += 1;
                    match r.resolve(p, &format!("mod {name};"), &files, &m) {
                        Some(_) => mods_resolved += 1,
                        None => misses.push(format!("{p}: mod {name};")),
                    }
                }
            }
        }

        println!("\n== ripgrep corpus ==");
        println!(
            "  .rs files                    : {}",
            files.iter().filter(|p| p.extension() == Some("rs")).count()
        );
        println!(
            "  in-workspace use statements  : {resolved}/{total} ({:.1}%)",
            pct(resolved, total)
        );
        println!(
            "    of which top-level         : {top_resolved}/{top_total} ({:.1}%)",
            pct(top_resolved, top_total)
        );
        println!(
            "  expanded single-path items   : {items_resolved}/{items_total} ({:.1}%)",
            pct(items_resolved, items_total)
        );
        println!(
            "  `mod x;` declarations        : {mods_resolved}/{mods_total} ({:.1}%)",
            pct(mods_resolved, mods_total)
        );
        println!("  detect                        : {detect_elapsed:?}");
        println!(
            "  path-override scan            : {scan_elapsed:?} ({} overrides)",
            m.overrides.len()
        );
        if !misses.is_empty() {
            println!("\n== unresolved ==");
            for miss in &misses {
                println!("  {miss}");
            }
        }
        assert!(
            total >= 91,
            "expected at least 91 in-workspace uses, found {total}"
        );
        assert!(misses.is_empty(), "{} unresolved", misses.len());
        assert_eq!(resolved, total);
        assert_eq!(items_resolved, items_total);
    }

    fn pct(a: usize, b: usize) -> f64 {
        if b == 0 {
            100.0
        } else {
            a as f64 * 100.0 / b as f64
        }
    }

    fn first_segment_in_workspace(body: &str, m: &ProjectMeta) -> bool {
        let first = body
            .split(|c: char| c == ':' || c == '{' || c.is_whitespace())
            .find(|s| !s.is_empty())
            .unwrap_or("");
        matches!(first, "crate" | "self" | "super") || m.modules.iter().any(|u| u.name == first)
    }

    fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == "target" {
                continue;
            }
            if path.is_dir() {
                walk_dir(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    struct UseStmt {
        text: String,
        line: usize,
        top_level: bool,
    }

    /// `use …;` statements in comment- and string-masked source, with the
    /// visibility prefix kept when it is on the same line.
    fn extract_use_statements(masked: &str) -> Vec<UseStmt> {
        let b = masked.as_bytes();
        let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut out = Vec::new();
        let mut i = 0;
        while i + 3 < b.len() {
            let at_kw = &b[i..i + 3] == b"use"
                && (i == 0 || !is_ident(b[i - 1]))
                && b[i + 3].is_ascii_whitespace();
            if !at_kw {
                i += 1;
                continue;
            }
            let Some(semi) = masked[i..].find(';') else {
                break;
            };
            let line_start = masked[..i].rfind('\n').map(|n| n + 1).unwrap_or(0);
            let before = masked[line_start..i].trim();
            let is_vis = before.is_empty()
                || (before.starts_with("pub") && before[3..].trim().is_empty()
                    || (before.starts_with("pub(") && before.ends_with(')')));
            let start = if is_vis { line_start } else { i };
            let text = masked[start..i + semi + 1].trim().to_string();
            let line = masked[..i].matches('\n').count() + 1;
            let top_level = !masked[line_start..].starts_with([' ', '\t']);
            out.push(UseStmt {
                text,
                line,
                top_level,
            });
            i += semi + 1;
        }
        out
    }

    /// Replace comments and string/char literals with spaces (keeping
    /// newlines) so keyword scanning cannot be fooled by them.
    fn mask_rust(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let blank = |c: u8| if c == b'\n' { b'\n' } else { b' ' };
        let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            // line comment
            if c == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
                continue;
            }
            // nested block comment
            if c == b'/' && b.get(i + 1) == Some(&b'*') {
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out.extend([b' ', b' ']);
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out.extend([b' ', b' ']);
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        out.push(blank(b[i]));
                        i += 1;
                    }
                }
                continue;
            }
            // raw string r"…" / r#"…"# / br"…"
            if c == b'r' {
                let prev_ok = i == 0
                    || !is_ident(b[i - 1])
                    || (b[i - 1] == b'b' && (i < 2 || !is_ident(b[i - 2])));
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if prev_ok && b.get(j) == Some(&b'"') {
                    let mut k = j + 1;
                    while k < b.len() {
                        if b[k] == b'"' && (0..hashes).all(|h| b.get(k + 1 + h) == Some(&b'#')) {
                            k += 1 + hashes;
                            break;
                        }
                        k += 1;
                    }
                    let k = k.min(b.len());
                    out.extend(b[i..k].iter().map(|&x| blank(x)));
                    i = k;
                    continue;
                }
            }
            // string literal
            if c == b'"' {
                out.push(b' ');
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        out.push(b' ');
                        out.push(blank(b[i + 1]));
                        i += 2;
                        continue;
                    }
                    out.push(blank(b[i]));
                    i += 1;
                }
                if i < b.len() {
                    out.push(b' ');
                    i += 1;
                }
                continue;
            }
            // char literal ('x', '\n'); lifetimes fall through untouched
            if c == b'\'' {
                if b.get(i + 1) == Some(&b'\\') {
                    out.push(b' ');
                    i += 1;
                    while i < b.len() && b[i] != b'\'' {
                        out.push(blank(b[i]));
                        i += 1;
                    }
                    if i < b.len() {
                        out.push(b' ');
                        i += 1;
                    }
                    continue;
                }
                if b.get(i + 2) == Some(&b'\'') {
                    out.extend([b' ', b' ', b' ']);
                    i += 3;
                    continue;
                }
            }
            out.push(c);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}
