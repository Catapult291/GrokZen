//! Go module resolution.
//!
//! Target: 100% of the 31 in-repo imports in the `gin` corpus.
//! Prior art resolved 0%: it took the full import path
//! `github.com/gin-gonic/gin/binding` and substring-matched it against
//! repo-relative paths like `binding/binding.go`, which can never hit.
//!
//! The fix is to read the `module` line from `go.mod` and strip it as a
//! prefix, leaving a repo-relative package directory. A Go import addresses a
//! *package* (a directory), not a file, so resolve to the directory's files.
//!
//! Must handle: the module root package itself (prefix strips to an empty
//! string — this was the one case that broke a naive implementation), nested
//! `go.mod` files (Go workspaces / submodules, innermost wins), `go.work`,
//! `internal/` packages, and vendored code under `vendor/`.
//!
//! Returning `None` for stdlib (`fmt`, `net/http`) and third-party imports is
//! correct.
//!
//! # How `detect` builds [`ProjectMeta`]
//!
//! * Every `go.mod` in the index becomes one [`ModuleUnit`] whose `name` is
//!   the `module` directive and whose `dir` is the directory holding the file.
//!   `go.mod` files under `testdata/`, `.*` or `_*` directories are skipped
//!   (the Go tool ignores those trees; they are fixtures, not modules) unless
//!   a `go.work` `use` directive explicitly points at them.
//! * `go.work` `use ./dir` entries are followed so workspace members that live
//!   in otherwise-ignored directories still register.
//! * If two `go.mod` files declare the same module path, all of them are kept
//!   (`examples/*/go.mod` all saying `module example` is common). `resolve`
//!   picks the one whose directory encloses the importing file, innermost
//!   first, and falls back to the shallowest.
//! * `replace old => ./local/dir` (a local path target) registers a unit named
//!   `old` at that directory, unless a `go.mod` there already declared it.
//! * `source_roots` mirrors the module directories; `excludes` stays empty
//!   because Go has no configured build-output directory.
//!
//! # How `resolve` picks a file
//!
//! An import names a directory, so [`GoResolver::resolve_import`] returns every
//! non-test `.go` file of the package as a real import edge (a change to any
//! file in the package affects the importer). [`GoResolver::resolve`] keeps
//! returning the one file that stands for the package, chosen like this:
//!
//! 1. files starting with `.` or `_` are ignored (the Go tool ignores them);
//! 2. `_test.go` files are ignored;
//! 3. a file whose stem equals the last import-path segment wins
//!    (`binding/binding.go`, `internal/fs/fs.go`; for the module root the last
//!    segment of the module path is used, so `github.com/gin-gonic/gin` →
//!    `gin.go`; a `/vN` major-version suffix is skipped);
//! 4. otherwise the lexicographically smallest remaining `.go` file;
//! 5. if the directory holds *only* test files, the smallest test file is
//!    returned rather than `None` — the directory is demonstrably a package
//!    and an edge to it is more useful than a silent miss.
//!
//! When the import path is not inside any known module, `<module dir>/vendor/
//! <import path>` is tried for the module enclosing the importing file (and
//! the repo root, for GOPATH-style layouts). `internal/` packages need no
//! special handling: visibility is a compiler concern, not a resolution one.

use std::collections::BTreeSet;

use crate::types::{
    join_rel, FileIndex, ImportResolution, Language, ModuleResolver, ModuleUnit, ProjectMeta,
    RelPath,
};

pub struct GoResolver;

impl ModuleResolver for GoResolver {
    fn language(&self) -> Language {
        Language::Go
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        // 1. Collect candidate go.mod paths: every go.mod outside ignored
        //    trees, plus whatever go.work explicitly `use`s.
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for p in files.iter() {
            match p.file_name() {
                "go.mod" => {
                    if !is_ignored_tree(p.dir()) {
                        candidates.insert(p.as_str().to_string());
                    }
                }
                "go.work" => {
                    let Some(content) = files.read(p.as_str()) else {
                        continue;
                    };
                    for use_dir in parse_go_work_uses(&content) {
                        let Some(dir) = join_rel(p.dir(), &use_dir) else {
                            continue;
                        };
                        let go_mod = join_dir(&dir, "go.mod");
                        if files.contains(&go_mod) {
                            candidates.insert(go_mod);
                        }
                    }
                }
                _ => {}
            }
        }

        // 2. Read the module path out of each candidate.
        let mut modules: Vec<ModuleUnit> = Vec::new();
        for go_mod in &candidates {
            let Some(content) = files.read(go_mod) else {
                continue;
            };
            let Some(name) = parse_go_mod_module(&content) else {
                continue;
            };
            let dir = RelPath::new(go_mod).dir().to_string();
            modules.push(ModuleUnit { name, dir });
        }

        // 3. `replace x => ./local` with a local directory target.
        for go_mod in &candidates {
            let Some(content) = files.read(go_mod) else {
                continue;
            };
            let base = RelPath::new(go_mod).dir().to_string();
            for (old, target) in parse_local_replaces(&content) {
                let Some(dir) = join_rel(&base, &target) else {
                    continue;
                };
                if !files.has_dir(&dir) {
                    continue;
                }
                modules.push(ModuleUnit { name: old, dir });
            }
        }

        // 4. Keep every unit, even with a repeated module path; `resolve`
        //    chooses by proximity. Only exact duplicates are dropped.
        modules.sort_by(|a, b| a.dir.cmp(&b.dir).then_with(|| a.name.cmp(&b.name)));
        modules.dedup();

        let source_roots = modules.iter().map(|m| m.dir.clone()).collect();
        ProjectMeta {
            source_roots,
            modules,
            ..Default::default()
        }
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        resolve_package(from, spec, files, meta)
            .map(|p| p.primary)
            .filter(|p| p != from)
    }

    fn resolve_import(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> ImportResolution {
        let Some(pkg) = resolve_package(from, spec, files, meta) else {
            return ImportResolution::Unresolved;
        };
        let all: Vec<RelPath> = pkg.all.into_iter().filter(|p| p != from).collect();
        if all.is_empty() {
            ImportResolution::Unresolved
        } else {
            ImportResolution::Files(all)
        }
    }
}

struct Package {
    primary: RelPath,
    all: Vec<RelPath>,
}

fn resolve_package(
    from: &RelPath,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<Package> {
    {
        let spec = normalize_spec(spec);
        // `import "C"` is the cgo pseudo-package.
        if spec.is_empty() || spec == "C" {
            return None;
        }

        // Relative import paths are only legal in ad-hoc (non-module) builds
        // but cost nothing to support.
        if spec.starts_with("./") || spec.starts_with("../") {
            let dir = join_rel(from.dir(), spec)?;
            return package_files(files, &dir, package_hint(&dir));
        }

        // Longest-prefix module match, then strip the module path. For the
        // module root package `rest` is empty and the package directory is the
        // module directory itself.
        for unit in module_candidates(meta, from.dir(), spec) {
            let rest = spec[unit.name.len()..].trim_start_matches('/');
            let dir = join_dir(&unit.dir, rest);
            let hint = if rest.is_empty() {
                package_hint(&unit.name)
            } else {
                package_hint(rest)
            };
            if let Some(found) = package_files(files, &dir, hint) {
                return Some(found);
            }
        }

        // Vendored dependency: `<enclosing module dir>/vendor/<import path>`.
        // Innermost enclosing module first, repo root last (GOPATH layouts).
        let from_dir = from.dir();
        let mut roots: Vec<&str> = meta
            .modules
            .iter()
            .map(|m| m.dir.as_str())
            .filter(|d| dir_within(from_dir, d))
            .collect();
        roots.push("");
        roots.sort_by_key(|d| std::cmp::Reverse(d.len()));
        roots.dedup();
        for root in roots {
            let vendor = join_dir(&join_dir(root, "vendor"), spec);
            if let Some(found) = package_files(files, &vendor, package_hint(spec)) {
                return Some(found);
            }
        }

        None
    }
}

// ----------------------------------------------------------------- helpers

/// Units whose module path prefixes `spec`, best first: longest module path,
/// then the innermost unit whose directory encloses the importing file, then
/// the shallowest directory. Several units may share one module path.
fn module_candidates<'m>(meta: &'m ProjectMeta, from_dir: &str, spec: &str) -> Vec<&'m ModuleUnit> {
    let mut v: Vec<&ModuleUnit> = meta
        .modules
        .iter()
        .filter(|m| {
            spec == m.name
                || spec
                    .strip_prefix(m.name.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        })
        .collect();
    v.sort_by_key(|m| {
        let encloses = dir_within(from_dir, &m.dir);
        (
            std::cmp::Reverse(m.name.len()),
            std::cmp::Reverse(encloses),
            // Enclosing units: innermost (longest dir) first. Others: shallowest.
            if encloses {
                usize::MAX - m.dir.len()
            } else {
                m.dir.len()
            },
            m.dir.clone(),
        )
    });
    v
}

/// The package in `dir`: `primary` is the file that stands for it in
/// [`GoResolver::resolve`] (see the module docs for the selection order),
/// `all` is every file of the package for [`GoResolver::resolve_import`].
fn package_files(files: &FileIndex, dir: &str, hint: &str) -> Option<Package> {
    let mut best: Option<&RelPath> = None;
    let mut best_test: Option<&RelPath> = None;
    let mut hinted: Option<&RelPath> = None;
    let mut all: Vec<RelPath> = Vec::new();
    for p in files.entries(dir) {
        let name = p.file_name();
        let Some(stem) = name.strip_suffix(".go") else {
            continue;
        };
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        if stem.ends_with("_test") {
            if best_test.is_none_or(|b| p < b) {
                best_test = Some(p);
            }
            continue;
        }
        all.push(p.clone());
        if stem == hint {
            hinted = Some(p);
        }
        if best.is_none_or(|b| p < b) {
            best = Some(p);
        }
    }
    all.sort();
    match hinted.or(best) {
        Some(primary) => Some(Package {
            primary: primary.clone(),
            all,
        }),
        None => best_test.map(|t| Package {
            primary: t.clone(),
            all: vec![t.clone()],
        }),
    }
}

/// `replace` directives whose target is a local directory: `(old module,
/// relative target)`. Handles single-line and block forms and an optional
/// version after the old module path.
fn parse_local_replaces(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, arg) in directives(content) {
        if name != "replace" {
            continue;
        }
        let Some((lhs, rhs)) = arg.split_once("=>") else {
            continue;
        };
        let old = first_token(lhs);
        let target = first_token(rhs);
        if old.is_empty() || !(target.starts_with("./") || target.starts_with("../")) {
            continue;
        }
        out.push((old.to_string(), target.to_string()));
    }
    out
}

/// Last path segment, skipping a `/vN` major-version suffix
/// (`github.com/foo/bar/v2` → `bar`).
fn package_hint(path: &str) -> &str {
    let mut segs = path.trim_end_matches('/').rsplit('/');
    let last = segs.next().unwrap_or("");
    if is_major_version(last) {
        segs.next().unwrap_or(last)
    } else {
        last
    }
}

fn is_major_version(seg: &str) -> bool {
    match seg.strip_prefix('v') {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Accepts what a parser might hand over: a bare path, a quoted path, or an
/// aliased import clause such as `fs "example.com/x/fs"`.
fn normalize_spec(spec: &str) -> &str {
    let spec = spec.trim();
    let spec = spec.rsplit(char::is_whitespace).next().unwrap_or(spec);
    spec.trim_matches(|c| c == '"' || c == '`')
}

fn join_dir(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => base.to_string(),
        _ => format!("{base}/{rest}"),
    }
}

/// `dir` is `root` itself or somewhere beneath it. `""` is the repo root and
/// contains everything.
fn dir_within(dir: &str, root: &str) -> bool {
    root.is_empty()
        || dir
            .strip_prefix(root)
            .is_some_and(|r| r.is_empty() || r.starts_with('/'))
}

/// Trees the Go tool never treats as part of a module: `testdata`, and any
/// directory whose name starts with `.` or `_`.
fn is_ignored_tree(dir: &str) -> bool {
    dir.split('/')
        .any(|seg| seg == "testdata" || seg.starts_with('.') || seg.starts_with('_'))
}

// ------------------------------------------------------ go.mod / go.work

/// Directives of a `go.mod` / `go.work` file as `(name, argument)` pairs.
/// `name ( ... )` blocks are flattened into one pair per inner line; `//`
/// comments are stripped.
fn directives(content: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut block: Option<&str> = None;
    for raw in content.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = block {
            if line == ")" {
                block = None;
            } else {
                out.push((name, line));
            }
            continue;
        }
        let (name, rest) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        if rest == "(" {
            block = Some(name);
            continue;
        }
        out.push((name, rest));
    }
    out
}

/// First whitespace-delimited token, honouring `"..."` and `` `...` ``
/// quoting, with the quotes removed.
fn first_token(arg: &str) -> &str {
    let arg = arg.trim();
    for q in ['"', '`'] {
        if let Some(rest) = arg.strip_prefix(q) {
            return rest.split(q).next().unwrap_or(rest);
        }
    }
    arg.split(char::is_whitespace).next().unwrap_or(arg)
}

/// The `module` directive of a `go.mod`, or `None` if it has none.
fn parse_go_mod_module(content: &str) -> Option<String> {
    directives(content)
        .into_iter()
        .find(|(name, _)| *name == "module")
        .map(|(_, arg)| first_token(arg))
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// Directories listed by `use` directives in a `go.work`, as written
/// (relative to the `go.work` directory).
fn parse_go_work_uses(content: &str) -> Vec<String> {
    directives(content)
        .into_iter()
        .filter(|(name, _)| *name == "use")
        .map(|(_, arg)| first_token(arg))
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect()
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Scratch directory for `detect` tests, which read `go.mod` from disk.
    /// Only build files need to exist on disk; source files live in the
    /// `FileIndex` alone.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "astrolabe-go-{}-{}-{}",
                std::process::id(),
                tag,
                n
            ));
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn write(&self, rel: &str, content: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn index(root: impl Into<PathBuf>, files: &[&str]) -> FileIndex {
        FileIndex::new(root, files.iter().map(RelPath::new))
    }

    fn meta(units: &[(&str, &str)]) -> ProjectMeta {
        ProjectMeta {
            modules: units
                .iter()
                .map(|(name, dir)| ModuleUnit {
                    name: name.to_string(),
                    dir: dir.to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn resolve(files: &FileIndex, meta: &ProjectMeta, from: &str, spec: &str) -> Option<String> {
        GoResolver
            .resolve(&RelPath::new(from), spec, files, meta)
            .map(|p| p.as_str().to_string())
    }

    // --------------------------------------------------------- go.mod text

    #[test]
    fn parses_module_directive_variants() {
        assert_eq!(
            parse_go_mod_module("module github.com/gin-gonic/gin\n\ngo 1.25.0\n").as_deref(),
            Some("github.com/gin-gonic/gin")
        );
        assert_eq!(
            parse_go_mod_module("module \"example.com/quoted\" // trailing comment\n").as_deref(),
            Some("example.com/quoted")
        );
        assert_eq!(
            parse_go_mod_module("// leading comment\n\nmodule (\n\texample.com/block\n)\n")
                .as_deref(),
            Some("example.com/block")
        );
        // A `require` entry whose path starts with "module" is not a directive.
        assert_eq!(
            parse_go_mod_module(
                "go 1.22\n\nrequire (\n\tmodule.example.com/dep v1.0.0\n)\n\nmodule example.com/real\n"
            )
            .as_deref(),
            Some("example.com/real")
        );
        assert_eq!(parse_go_mod_module("go 1.22\n"), None);
    }

    #[test]
    fn parses_go_work_uses() {
        let work = "go 1.22\n\nuse ./a\n\nuse (\n\t.\n\t./tools/b // comment\n\t\"./with space\"\n)\n\nreplace x => ./y\n";
        assert_eq!(
            parse_go_work_uses(work),
            vec!["./a", ".", "./tools/b", "./with space"]
        );
    }

    // ------------------------------------------------------------- detect

    #[test]
    fn detect_reads_single_go_mod() {
        let tmp = TempDir::new("single");
        tmp.write("go.mod", "module example.com/app\n\ngo 1.22\n");
        let files = index(&tmp.0, &["go.mod", "app.go", "pkg/util/util.go"]);

        let meta = GoResolver.detect(&files);
        assert_eq!(
            meta.modules,
            vec![ModuleUnit {
                name: "example.com/app".into(),
                dir: "".into()
            }]
        );
        assert_eq!(meta.source_roots, vec![String::new()]);
        assert!(meta.excludes.is_empty());
    }

    #[test]
    fn detect_collects_nested_go_mod_and_go_work_members() {
        let tmp = TempDir::new("multi");
        tmp.write(
            "go.work",
            "go 1.22\n\nuse (\n\t.\n\t./tools\n\t./_fixtures/hidden\n)\n",
        );
        tmp.write("go.mod", "module example.com/app\n");
        tmp.write("tools/go.mod", "module example.com/app/tools\n");
        // Ignored tree, but whitelisted by go.work above.
        tmp.write("_fixtures/hidden/go.mod", "module example.com/hidden\n");
        // Ignored tree, not whitelisted: must not appear.
        tmp.write("testdata/mod/go.mod", "module example.com/fixture\n");
        // Duplicate module path: the shallower go.mod wins.
        tmp.write("copy/go.mod", "module example.com/app\n");
        let files = index(
            &tmp.0,
            &[
                "go.work",
                "go.mod",
                "app.go",
                "tools/go.mod",
                "tools/cmd/main.go",
                "_fixtures/hidden/go.mod",
                "_fixtures/hidden/h.go",
                "testdata/mod/go.mod",
                "testdata/mod/m.go",
                "copy/go.mod",
                "copy/c.go",
            ],
        );

        let meta = GoResolver.detect(&files);
        assert_eq!(
            meta.modules,
            vec![
                ModuleUnit {
                    name: "example.com/app".into(),
                    dir: "".into()
                },
                ModuleUnit {
                    name: "example.com/hidden".into(),
                    dir: "_fixtures/hidden".into()
                },
                // Same module path as the root: kept (nearest wins at resolve).
                ModuleUnit {
                    name: "example.com/app".into(),
                    dir: "copy".into()
                },
                ModuleUnit {
                    name: "example.com/app/tools".into(),
                    dir: "tools".into()
                },
            ]
        );
    }

    #[test]
    fn detect_without_go_mod_yields_no_modules() {
        let tmp = TempDir::new("none");
        let files = index(&tmp.0, &["main.go", "lib/lib.go"]);
        assert!(GoResolver.detect(&files).modules.is_empty());
    }

    // ------------------------------------------------------------ resolve

    #[test]
    fn resolves_subpackage_preferring_same_name_file() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "app.go",
                "pkg/util/aaa.go",
                "pkg/util/util.go",
                "pkg/util/util_test.go",
            ],
        );
        let m = meta(&[("example.com/app", "")]);
        assert_eq!(
            resolve(&files, &m, "app.go", "example.com/app/pkg/util").as_deref(),
            Some("pkg/util/util.go")
        );
    }

    #[test]
    fn resolves_subpackage_falling_back_to_lexicographic_non_test_file() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "render/zeta.go",
                "render/beta.go",
                "render/alpha_test.go",
                "render/_gen.go",
                "render/README.md",
            ],
        );
        let m = meta(&[("example.com/app", "")]);
        assert_eq!(
            resolve(&files, &m, "main.go", "example.com/app/render").as_deref(),
            Some("render/beta.go")
        );
    }

    #[test]
    fn resolves_module_root_package() {
        // The critical edge: stripping the module path leaves "", so the
        // package directory is the module directory itself.
        let files = index(
            "/nonexistent",
            &["go.mod", "auth.go", "gin.go", "gin_test.go", "ginS/gins.go"],
        );
        let m = meta(&[("github.com/gin-gonic/gin", "")]);
        assert_eq!(
            resolve(&files, &m, "ginS/gins.go", "github.com/gin-gonic/gin").as_deref(),
            Some("gin.go")
        );

        // Without a same-name file, the smallest non-test file in the root.
        let files = index(
            "/nonexistent",
            &["go.mod", "zzz.go", "context.go", "auth_test.go"],
        );
        assert_eq!(
            resolve(&files, &m, "ginS/gins.go", "github.com/gin-gonic/gin").as_deref(),
            Some("context.go")
        );

        // Module root in a nested module directory.
        let files = index(
            "/nonexistent",
            &["go.mod", "tools/go.mod", "tools/tools.go", "tools/x.go"],
        );
        let m = meta(&[("example.com/app", ""), ("example.com/app/tools", "tools")]);
        assert_eq!(
            resolve(&files, &m, "main.go", "example.com/app/tools").as_deref(),
            Some("tools/tools.go")
        );

        // Major-version suffix does not confuse the same-name hint.
        let files = index("/nonexistent", &["go.mod", "bar.go", "aaa.go"]);
        let m = meta(&[("github.com/foo/bar/v2", "")]);
        assert_eq!(
            resolve(&files, &m, "x/x.go", "github.com/foo/bar/v2").as_deref(),
            Some("bar.go")
        );
    }

    #[test]
    fn resolves_with_longest_module_prefix() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "app.go",
                "tools/go.mod",
                "tools/cmd/main.go",
                "toolsx/toolsx.go",
                "tools/cmd/vendor/github.com/dep/dep.go",
            ],
        );
        let m = meta(&[("example.com/app", ""), ("example.com/app/tools", "tools")]);
        // Innermost module owns `tools/cmd`.
        assert_eq!(
            resolve(&files, &m, "app.go", "example.com/app/tools/cmd").as_deref(),
            Some("tools/cmd/main.go")
        );
        // `toolsx` is not under `tools` — prefix match is segment-aware.
        assert_eq!(
            resolve(&files, &m, "app.go", "example.com/app/toolsx").as_deref(),
            Some("toolsx/toolsx.go")
        );
        // Package that does not exist inside the module namespace.
        assert_eq!(resolve(&files, &m, "app.go", "example.com/app/nope"), None);
    }

    #[test]
    fn resolves_internal_packages() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "gin.go",
                "internal/bytesconv/bytesconv.go",
                "internal/fs/fs.go",
            ],
        );
        let m = meta(&[("github.com/gin-gonic/gin", "")]);
        assert_eq!(
            resolve(
                &files,
                &m,
                "gin.go",
                "github.com/gin-gonic/gin/internal/bytesconv"
            )
            .as_deref(),
            Some("internal/bytesconv/bytesconv.go")
        );
        assert_eq!(
            resolve(
                &files,
                &m,
                "render/html.go",
                "github.com/gin-gonic/gin/internal/fs"
            )
            .as_deref(),
            Some("internal/fs/fs.go")
        );
    }

    #[test]
    fn stdlib_and_third_party_return_none() {
        // A repo directory that happens to share a stdlib name must not hijack it.
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "gin.go",
                "fmt/fmt.go",
                "net/http/http.go",
                "github.com/x/y.go",
            ],
        );
        let m = meta(&[("github.com/gin-gonic/gin", "")]);
        for spec in ["fmt", "net/http", "sync", "html/template", "C", ""] {
            assert_eq!(resolve(&files, &m, "gin.go", spec), None, "{spec}");
        }
        for spec in [
            "github.com/stretchr/testify/assert",
            "golang.org/x/net/http2",
            "github.com/gin-gonic/ginx",
            "github.com/x",
        ] {
            assert_eq!(resolve(&files, &m, "gin.go", spec), None, "{spec}");
        }
    }

    #[test]
    fn resolves_vendored_packages_from_enclosing_module() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "app.go",
                "vendor/github.com/dep/lib/lib.go",
                "vendor/github.com/dep/lib/other.go",
                "sub/go.mod",
                "sub/sub.go",
                "sub/vendor/github.com/dep/lib/nested.go",
            ],
        );
        let m = meta(&[("example.com/app", ""), ("example.com/app/sub", "sub")]);
        assert_eq!(
            resolve(&files, &m, "app.go", "github.com/dep/lib").as_deref(),
            Some("vendor/github.com/dep/lib/lib.go")
        );
        // Files inside the nested module see that module's vendor tree first.
        assert_eq!(
            resolve(&files, &m, "sub/sub.go", "github.com/dep/lib").as_deref(),
            Some("sub/vendor/github.com/dep/lib/nested.go")
        );
        assert_eq!(
            resolve(&files, &m, "app.go", "github.com/dep/missing"),
            None
        );
    }

    #[test]
    fn tolerates_quoted_and_aliased_specs() {
        let files = index("/nonexistent", &["go.mod", "gin.go", "internal/fs/fs.go"]);
        let m = meta(&[("github.com/gin-gonic/gin", "")]);
        for spec in [
            "\"github.com/gin-gonic/gin/internal/fs\"",
            "filesystem \"github.com/gin-gonic/gin/internal/fs\"",
            "  github.com/gin-gonic/gin/internal/fs  ",
        ] {
            assert_eq!(
                resolve(&files, &m, "gin.go", spec).as_deref(),
                Some("internal/fs/fs.go"),
                "{spec}"
            );
        }
    }

    #[test]
    fn test_only_directory_falls_back_to_test_file() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "app.go",
                "fixtures/b_test.go",
                "fixtures/a_test.go",
            ],
        );
        let m = meta(&[("example.com/app", "")]);
        assert_eq!(
            resolve(&files, &m, "app.go", "example.com/app/fixtures").as_deref(),
            Some("fixtures/a_test.go")
        );
    }

    #[test]
    fn resolves_relative_import_paths() {
        let files = index("/nonexistent", &["cmd/main.go", "cmd/lib/lib.go"]);
        let m = meta(&[]);
        assert_eq!(
            resolve(&files, &m, "cmd/main.go", "./lib").as_deref(),
            Some("cmd/lib/lib.go")
        );
        assert_eq!(resolve(&files, &m, "cmd/main.go", "../../escape"), None);
    }

    // -------------------------------------------------- real corpus (gin)

    /// Walk `root` and collect every regular file as a repo-relative path,
    /// skipping `.git`.
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                walk(root, &path, out);
            } else if path.is_file() {
                let rel = path.strip_prefix(root).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    /// Remove `//` and `/* */` comments while leaving string, raw-string and
    /// rune literals intact.
    fn strip_comments(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        let mut keep_from = 0;
        while i < b.len() {
            match b[i] {
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    out.push_str(&src[keep_from..i]);
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    keep_from = i;
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    out.push_str(&src[keep_from..i]);
                    i += 2;
                    while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                        if b[i] == b'\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                    i = (i + 2).min(b.len());
                    keep_from = i;
                }
                q @ (b'"' | b'\'' | b'`') => {
                    i += 1;
                    while i < b.len() && b[i] != q {
                        if b[i] == b'\\' && q != b'`' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        out.push_str(&src[keep_from.min(b.len())..]);
        out
    }

    fn quoted(line: &str) -> Option<String> {
        let (_, rest) = line.split_once('"')?;
        let (path, _) = rest.split_once('"')?;
        Some(path.to_string())
    }

    /// Import paths of a Go source file: single `import "x"` clauses and
    /// `import ( ... )` blocks, with optional aliases.
    fn extract_imports(src: &str) -> Vec<String> {
        let clean = strip_comments(src);
        let mut out = Vec::new();
        let mut in_block = false;
        for raw in clean.lines() {
            let line = raw.trim();
            if in_block {
                if line.starts_with(')') {
                    in_block = false;
                } else if let Some(p) = quoted(line) {
                    out.push(p);
                }
                continue;
            }
            let Some(rest) = line.strip_prefix("import") else {
                continue;
            };
            if !rest.starts_with(|c: char| c.is_whitespace() || c == '(' || c == '"') {
                continue;
            }
            let rest = rest.trim_start();
            if let Some(inner) = rest.strip_prefix('(') {
                if let Some(p) = quoted(inner) {
                    out.push(p);
                }
                in_block = !inner.contains(')');
            } else if let Some(p) = quoted(rest) {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn extract_imports_handles_blocks_aliases_and_comments() {
        let src = r#"
// Copyright
/*
	import "github.com/commented/out"
*/
package gin // import "github.com/commented/out2"

import "fmt"

import (
	"net/http"

	filesystem "github.com/gin-gonic/gin/internal/fs" // alias
	_ "embed"
)

func f() string { return "import \"not/an/import\"" }
"#;
        assert_eq!(
            extract_imports(src),
            vec![
                "fmt",
                "net/http",
                "github.com/gin-gonic/gin/internal/fs",
                "embed"
            ]
        );
    }

    /// Acceptance check against the real gin checkout in `corpus/go`:
    /// every import that points inside the module must resolve.
    ///
    /// Run with `cargo test -p astrolabe-core go -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs corpus/go; run with --ignored"]
    fn corpus_gin_resolves_every_in_module_import() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/go");
        let root = root
            .canonicalize()
            .expect("corpus/go not found relative to the workspace root");

        let mut rels = Vec::new();
        walk(&root, &root, &mut rels);
        let files = FileIndex::new(&root, rels.iter().map(RelPath::new));
        let go_files = files.iter().filter(|p| p.extension() == Some("go")).count();

        let resolver = GoResolver;
        let meta = resolver.detect(&files);
        assert_eq!(
            meta.modules
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["github.com/gin-gonic/gin"],
            "unexpected module set: {:?}",
            meta.modules
        );

        let mut total = 0usize;
        let mut resolved = 0usize;
        let mut external = 0usize;
        let mut missed: Vec<(String, String)> = Vec::new();
        for file in files.iter().filter(|p| p.extension() == Some("go")) {
            let src = files.read(file.as_str()).expect("read go file");
            for spec in extract_imports(&src) {
                if meta.module_for(&spec).is_none() {
                    external += 1;
                    continue;
                }
                total += 1;
                match resolver.resolve(file, &spec, &files, &meta) {
                    Some(_) => resolved += 1,
                    None => missed.push((file.as_str().to_string(), spec)),
                }
            }
        }

        let rate = if total == 0 {
            100.0
        } else {
            resolved as f64 * 100.0 / total as f64
        };
        println!(
            "go corpus ({go_files} .go files): {resolved}/{total} in-module imports resolved \
             ({rate:.1}%), {external} stdlib/third-party imports correctly left unresolved"
        );
        for (file, spec) in &missed {
            println!("  MISS {file} -> {spec}");
        }
        assert!(
            total > 0,
            "no in-module imports found — extractor or corpus broken"
        );
        assert_eq!(resolved, total, "unresolved in-module imports: {missed:?}");
    }

    #[test]
    fn same_module_name_resolves_to_enclosing_module() {
        let files = index(
            "/nonexistent",
            &[
                "examples/a/util/util.go",
                "examples/a/main.go",
                "examples/b/util/util.go",
                "examples/b/main.go",
            ],
        );
        let m = meta(&[("example", "examples/a"), ("example", "examples/b")]);
        assert_eq!(
            resolve(&files, &m, "examples/b/main.go", "example/util").as_deref(),
            Some("examples/b/util/util.go")
        );
        assert_eq!(
            resolve(&files, &m, "examples/a/main.go", "example/util").as_deref(),
            Some("examples/a/util/util.go")
        );
    }

    #[test]
    fn resolve_import_returns_every_non_test_file_of_the_package() {
        let files = index(
            "/nonexistent",
            &[
                "go.mod",
                "app.go",
                "p/a.go",
                "p/b.go",
                "p/b_test.go",
                "p/_x.go",
            ],
        );
        let m = meta(&[("m", "")]);
        match GoResolver.resolve_import(&RelPath::new("app.go"), "m/p", &files, &m) {
            ImportResolution::Files(v) => {
                let got: Vec<&str> = v.iter().map(RelPath::as_str).collect();
                assert_eq!(got, vec!["p/a.go", "p/b.go"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_local_replace_directives() {
        let got = parse_local_replaces(
            "module m\nreplace example.com/x => ./third/x\nreplace (\n  a.b/c v1.0.0 => ../c // note\n  d.e/f => github.com/g/h v1\n)\n",
        );
        assert_eq!(
            got,
            vec![
                ("example.com/x".to_string(), "./third/x".to_string()),
                ("a.b/c".to_string(), "../c".to_string())
            ]
        );
    }
}
