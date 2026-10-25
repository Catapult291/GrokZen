//! Dart package resolution.
//!
//! Maps `package:<name>/<path>.dart` and relative imports to repo files based on
//! `pubspec.yaml` manifests.
//!
//! # How `detect` builds [`ProjectMeta`]
//!
//! * Every `pubspec.yaml` in the index (excluding those in `.dart_tool`, or in
//!   a `build/` directory whose parent has its own `pubspec.yaml` — that is
//!   package build output; `tool/build/tools/pubspec.yaml` with no package
//!   directly above `build/` is a normal package) is parsed for its `name:` declaration at column 0.
//! * Each valid pubspec produces a [`ModuleUnit`] mapping `<name>` to
//!   `<pubspec_dir>/lib`. Same-name packages are all kept — see the note in
//!   `detect`.
//! * `source_roots` mirrors each package's `lib` directory.
//! * `excludes` declares `<pkg_dir>/.dart_tool` for each package.
//!
//! # How `resolve` picks a file
//!
//! 1. `dart:` imports are SDK libraries and return `None`.
//! 2. `package:<name>/<rest>` resolves against the matching unit's directory
//!    (`<unit.dir>/<rest>`). A bare `package:<name>` resolves to `<unit.dir>/<name>.dart`.
//!    When several units share the name, they are tried nearest to `from`
//!    first, so an example app resolves to its own sub-project's copy.
//!    Third-party packages (not in `meta.modules`) return `None`.
//! 3. Relative paths (`./`, `../`, or bare `x.dart`) resolve relative to `from.dir()`.
//!    First tried verbatim, then with a `.dart` extension appended.
//! 4. Any other specifier returns `None`.

use crate::types::{
    join_rel, FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath,
};

pub struct DartResolver;

impl ModuleResolver for DartResolver {
    fn language(&self) -> Language {
        Language::Dart
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut candidates: Vec<String> = Vec::new();
        for p in files.iter() {
            if p.file_name() == "pubspec.yaml" && !is_ignored_dart_path(p.as_str(), files) {
                candidates.push(p.as_str().to_string());
            }
        }
        candidates.sort();

        let mut modules: Vec<ModuleUnit> = Vec::new();
        let mut pkg_dirs: Vec<String> = Vec::new();

        for pubspec in &candidates {
            let Some(content) = files.read(pubspec) else {
                continue;
            };
            let Some(name) = parse_pubspec_name(&content) else {
                continue;
            };
            let pkg_dir = RelPath::new(pubspec).dir().to_string();
            let lib_dir = join_dir(&pkg_dir, "lib");
            modules.push(ModuleUnit { name, dir: lib_dir });
            pkg_dirs.push(pkg_dir);
        }

        // Same-name packages are the norm in Dart monorepos: every platform
        // implementation ships an `example` app with an identical package name
        // (flutter/packages carries `camera_example` x3 and
        // `google_maps_flutter_example` x5). Collapsing them to one unit would
        // silently drop modules, so only exact (name, dir) duplicates — one
        // pubspec picked up twice — are removed; `candidates` is sorted, so
        // identical units end up adjacent. Ambiguity between same-name units
        // is settled per import in `resolve`, which routes to the unit nearest
        // the importing file.
        modules.sort_by(|a, b| a.dir.cmp(&b.dir));
        modules.dedup();

        let mut source_roots: Vec<String> = modules.iter().map(|m| m.dir.clone()).collect();
        source_roots.dedup();
        let mut excludes: Vec<String> = pkg_dirs
            .into_iter()
            .map(|d| join_dir(&d, ".dart_tool"))
            .collect();
        excludes.sort();
        excludes.dedup();

        ProjectMeta {
            source_roots,
            modules,
            excludes,
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
        let spec = normalize_spec(spec);
        if spec.is_empty() {
            return None;
        }

        // 1. SDK libraries
        if spec.starts_with("dart:") {
            return None;
        }

        // 2. Package imports. Same-name packages are common (see `detect`), so
        //    every unit matching the name is a candidate; they are tried
        //    nearest to `from` first, so an example app's import resolves to
        //    its own sub-project's copy rather than an alphabetical
        //    sibling's.
        if let Some(pkg_spec) = spec.strip_prefix("package:") {
            let mut candidates: Vec<&ModuleUnit> = meta
                .modules
                .iter()
                // Same match semantics as `ProjectMeta::module_for`.
                .filter(|m| {
                    pkg_spec == m.name
                        || (pkg_spec.len() > m.name.len()
                            && pkg_spec.starts_with(m.name.as_str())
                            && pkg_spec.as_bytes()[m.name.len()] == b'/')
                })
                .collect();
            candidates.sort_by(|a, b| {
                common_prefix_depth(&b.dir, from.dir())
                    .cmp(&common_prefix_depth(&a.dir, from.dir()))
            });
            for unit in candidates {
                if let Some(target) = package_target(unit, pkg_spec, files) {
                    return Some(target);
                }
            }
            return None;
        }

        // 3. Relative paths (./, ../, or bare filename/relative path)
        if !spec.contains(':') && !spec.starts_with('/') {
            if let Some(target) = join_rel(from.dir(), spec) {
                if files.contains(&target) {
                    return Some(RelPath::new(target));
                }
                if !target.ends_with(".dart") {
                    let with_dart = format!("{target}.dart");
                    if files.contains(&with_dart) {
                        return Some(RelPath::new(with_dart));
                    }
                }
            }
        }

        // 4. Anything else -> None
        None
    }
}

// ----------------------------------------------------------------- helpers

fn normalize_spec(spec: &str) -> &str {
    // 只做 trim 与引号剥离：imports.scm 捕获的是纯 string_literal 内容，
    // 不含 `as x` 等尾缀；按空白切 token 会破坏合法的含空格路径
    // （`import 'src/my widget.dart'`）。
    spec.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
}

fn join_dir(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => base.to_string(),
        _ => format!("{base}/{rest}"),
    }
}

/// Number of leading `/`-separated segments `a` and `b` share. Used to rank
/// same-name module candidates by proximity to the importing file.
fn common_prefix_depth(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Maps `package:<name>/<rest>` onto one candidate unit's directory:
/// `<unit.dir>/<rest>`, the bare name with `.dart` appended, then `.dart`
/// appended to `rest`. Pure file-set probing; no disk IO.
fn package_target(unit: &ModuleUnit, pkg_spec: &str, files: &FileIndex) -> Option<RelPath> {
    let rest = pkg_spec[unit.name.len()..].trim_start_matches('/');
    if rest.is_empty() {
        let target = join_dir(&unit.dir, &format!("{}.dart", unit.name));
        if files.contains(&target) {
            return Some(RelPath::new(target));
        }
        return None;
    }
    let target = join_dir(&unit.dir, rest);
    if files.contains(&target) {
        return Some(RelPath::new(target));
    }
    if !target.ends_with(".dart") {
        let with_dart = format!("{target}.dart");
        if files.contains(&with_dart) {
            return Some(RelPath::new(with_dart));
        }
    }
    None
}

/// `.dart_tool` anywhere, or a `build` directory that is a package's build
/// output, i.e. its parent directory holds a `pubspec.yaml` of its own. A
/// `build` segment elsewhere (`build/tools/pubspec.yaml`, `tool/build/...`)
/// is an ordinary directory name.
fn is_ignored_dart_path(path: &str, files: &FileIndex) -> bool {
    let mut parent = String::new();
    for seg in path.split('/') {
        if seg == ".dart_tool" {
            return true;
        }
        if seg == "build" {
            let manifest = if parent.is_empty() {
                "pubspec.yaml".to_string()
            } else {
                format!("{parent}/pubspec.yaml")
            };
            if files.contains(&manifest) {
                return true;
            }
        }
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push_str(seg);
    }
    false
}

/// Parses the package `name:` from a `pubspec.yaml` content.
///
/// Hand-written line-by-line parsing: strictly accepts lines starting at column 0
/// with `name:` (nested `name:` within dependencies has leading indentation).
pub fn parse_pubspec_name(content: &str) -> Option<String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("name:") else {
            continue;
        };
        let rest = rest.split('#').next().unwrap_or(rest);
        let name = rest.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        if !name.is_empty() && !name.contains(char::is_whitespace) {
            return Some(name.to_string());
        }
    }
    None
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn synthetic_monorepo_resolution() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // 1. Create directory structure
        let app_dir = root.join("src/app");
        let lib_dir = app_dir.join("lib");
        let db_dir = lib_dir.join("data/db");
        fs::create_dir_all(&db_dir).unwrap();

        // src/app/pubspec.yaml with indentation name trap
        let pubspec_content =
            "name: ai_con_app\n\ndependencies:\n  flutter:\n    sdk: flutter\n  collection: ^1.0.0\n";
        fs::write(app_dir.join("pubspec.yaml"), pubspec_content).unwrap();

        // source files
        fs::write(lib_dir.join("main.dart"), "// main").unwrap();
        fs::write(db_dir.join("database.dart"), "// db").unwrap();
        fs::write(db_dir.join("database.g.dart"), "// g.dart").unwrap();

        // FileIndex including pubspec and dart files
        let files = FileIndex::new(
            root,
            [
                RelPath::new("src/app/pubspec.yaml"),
                RelPath::new("src/app/lib/main.dart"),
                RelPath::new("src/app/lib/data/db/database.dart"),
                RelPath::new("src/app/lib/data/db/database.g.dart"),
            ],
        );

        let resolver = DartResolver;
        let meta = resolver.detect(&files);

        // Verify module unit
        assert_eq!(meta.modules.len(), 1);
        assert_eq!(meta.modules[0].name, "ai_con_app");
        assert_eq!(meta.modules[0].dir, "src/app/lib");
        assert_eq!(meta.source_roots, vec!["src/app/lib"]);
        assert!(meta.excludes.contains(&"src/app/.dart_tool".to_string()));

        let from_main = RelPath::new("src/app/lib/main.dart");
        let from_db = RelPath::new("src/app/lib/data/db/database.dart");

        // 断言: package:ai_con_app/main.dart → Some(src/app/lib/main.dart)
        assert_eq!(
            resolver.resolve(&from_main, "package:ai_con_app/main.dart", &files, &meta),
            Some(RelPath::new("src/app/lib/main.dart"))
        );

        // 断言: package:ai_con_app/data/db/database.dart → 对应文件
        assert_eq!(
            resolver.resolve(
                &from_main,
                "package:ai_con_app/data/db/database.dart",
                &files,
                &meta
            ),
            Some(RelPath::new("src/app/lib/data/db/database.dart"))
        );

        // 断言: 从 src/app/lib/data/db/database.dart 解析 part 指令 database.g.dart（相对路径裸名）→ 同目录 .g.dart 文件
        assert_eq!(
            resolver.resolve(&from_db, "database.g.dart", &files, &meta),
            Some(RelPath::new("src/app/lib/data/db/database.g.dart"))
        );

        // 断言: dart:io → None; package:flutter/material.dart → None
        assert_eq!(resolver.resolve(&from_main, "dart:io", &files, &meta), None);
        assert_eq!(
            resolver.resolve(&from_main, "package:flutter/material.dart", &files, &meta),
            None
        );
    }

    #[test]
    fn indented_name_in_dependencies_not_misread() {
        let content = r#"
# No root name here
dependencies:
  some_dep:
    name: false_package
    sdk: flutter
"#;
        assert_eq!(parse_pubspec_name(content), None);

        let content_valid = r#"
name: valid_package
dependencies:
  dep:
    name: nested_trap
"#;
        assert_eq!(
            parse_pubspec_name(content_valid).as_deref(),
            Some("valid_package")
        );
    }

    #[test]
    fn same_name_packages_are_all_kept() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let shallow = root.join("pkg");
        let deep = root.join("deep/nested/pkg");
        fs::create_dir_all(shallow.join("lib")).unwrap();
        fs::create_dir_all(deep.join("lib")).unwrap();

        fs::write(shallow.join("pubspec.yaml"), "name: my_package\n").unwrap();
        fs::write(deep.join("pubspec.yaml"), "name: my_package\n").unwrap();

        let files = FileIndex::new(
            root,
            [
                RelPath::new("pkg/pubspec.yaml"),
                RelPath::new("deep/nested/pkg/pubspec.yaml"),
            ],
        );

        let meta = DartResolver.detect(&files);
        // Same-name packages are distinct modules (Dart monorepos ship one
        // example app per platform implementation); neither may be dropped.
        let mut dirs: Vec<&str> = meta
            .modules
            .iter()
            .filter(|m| m.name == "my_package")
            .map(|m| m.dir.as_str())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec!["deep/nested/pkg/lib", "pkg/lib"]);
        assert_eq!(meta.source_roots, vec!["deep/nested/pkg/lib", "pkg/lib"]);
    }

    #[test]
    fn same_name_examples_route_to_the_nearest_copy() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Two example apps share the package name `demo_app`; each ships its
        // own lib/helpers.dart. A third, uniquely named package is also present.
        let a_lib = root.join("pkgs/a/example/lib");
        let b_lib = root.join("pkgs/b/example/lib");
        let b_tests = root.join("pkgs/b/example/integration_test");
        let c_lib = root.join("pkgs/c/lib");
        for d in [&a_lib, &b_lib, &b_tests, &c_lib] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(root.join("pkgs/a/example/pubspec.yaml"), "name: demo_app\n").unwrap();
        fs::write(root.join("pkgs/b/example/pubspec.yaml"), "name: demo_app\n").unwrap();
        fs::write(root.join("pkgs/c/pubspec.yaml"), "name: plugin_c\n").unwrap();
        fs::write(a_lib.join("helpers.dart"), "// a").unwrap();
        fs::write(b_lib.join("helpers.dart"), "// b").unwrap();
        fs::write(c_lib.join("api.dart"), "// c").unwrap();

        let files = FileIndex::new(
            root,
            [
                RelPath::new("pkgs/a/example/pubspec.yaml"),
                RelPath::new("pkgs/a/example/lib/helpers.dart"),
                RelPath::new("pkgs/b/example/pubspec.yaml"),
                RelPath::new("pkgs/b/example/lib/helpers.dart"),
                RelPath::new("pkgs/b/example/integration_test/app_test.dart"),
                RelPath::new("pkgs/c/pubspec.yaml"),
                RelPath::new("pkgs/c/lib/api.dart"),
            ],
        );

        let resolver = DartResolver;
        let meta = resolver.detect(&files);
        assert_eq!(
            meta.modules.iter().filter(|m| m.name == "demo_app").count(),
            2
        );

        let from_b = RelPath::new("pkgs/b/example/integration_test/app_test.dart");
        // 就近路由: from 在 example B 下，解析到 B 的 lib 而非 A 的。
        assert_eq!(
            resolver.resolve(&from_b, "package:demo_app/helpers.dart", &files, &meta),
            Some(RelPath::new("pkgs/b/example/lib/helpers.dart"))
        );

        // 无歧义路径回归: 唯一名字的包从任何位置照常解析。
        assert_eq!(
            resolver.resolve(&from_b, "package:plugin_c/api.dart", &files, &meta),
            Some(RelPath::new("pkgs/c/lib/api.dart"))
        );

        // 对称: from 在 example A 侧（任意同工程文件）解析到 A 的 lib。
        let from_a = RelPath::new("pkgs/a/example/lib/helpers.dart");
        assert_eq!(
            resolver.resolve(&from_a, "package:demo_app/helpers.dart", &files, &meta),
            Some(RelPath::new("pkgs/a/example/lib/helpers.dart"))
        );
    }

    #[test]
    fn same_name_falls_through_when_nearest_copy_lacks_the_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // `a` declares demo_app but does not ship extra.dart; `b` does. An
        // import from inside `a` must fall through to `b`'s copy instead of
        // failing on the nearest candidate.
        let a_example = root.join("pkgs/a/example");
        let b_example = root.join("pkgs/b/example");
        fs::create_dir_all(a_example.join("lib")).unwrap();
        fs::create_dir_all(a_example.join("test")).unwrap();
        fs::create_dir_all(b_example.join("lib")).unwrap();
        fs::write(a_example.join("pubspec.yaml"), "name: demo_app\n").unwrap();
        fs::write(b_example.join("pubspec.yaml"), "name: demo_app\n").unwrap();
        fs::write(b_example.join("lib/extra.dart"), "// b").unwrap();

        let files = FileIndex::new(
            root,
            [
                RelPath::new("pkgs/a/example/pubspec.yaml"),
                RelPath::new("pkgs/a/example/lib/main.dart"),
                RelPath::new("pkgs/a/example/test/main_test.dart"),
                RelPath::new("pkgs/b/example/pubspec.yaml"),
                RelPath::new("pkgs/b/example/lib/extra.dart"),
            ],
        );

        let resolver = DartResolver;
        let meta = resolver.detect(&files);
        let from_a = RelPath::new("pkgs/a/example/test/main_test.dart");
        assert_eq!(
            resolver.resolve(&from_a, "package:demo_app/extra.dart", &files, &meta),
            Some(RelPath::new("pkgs/b/example/lib/extra.dart"))
        );
        // 就近候选没有的文件绝不误配到别的名字。
        assert_eq!(
            resolver.resolve(&from_a, "package:demo_app/missing.dart", &files, &meta),
            None
        );
    }

    #[test]
    fn dart_tool_and_build_ignored_in_detect_and_excludes_set() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let normal = root.join("app");
        let tool = root.join(".dart_tool/sub");
        let build = root.join("app/build/sub");
        fs::create_dir_all(&normal).unwrap();
        fs::create_dir_all(&tool).unwrap();
        fs::create_dir_all(&build).unwrap();

        fs::write(normal.join("pubspec.yaml"), "name: app\n").unwrap();
        fs::write(tool.join("pubspec.yaml"), "name: tool_fake\n").unwrap();
        fs::write(build.join("pubspec.yaml"), "name: build_fake\n").unwrap();

        let files = FileIndex::new(
            root,
            [
                RelPath::new("app/pubspec.yaml"),
                RelPath::new(".dart_tool/sub/pubspec.yaml"),
                RelPath::new(".dart_tool/hack.d"),
                RelPath::new("app/build/sub/pubspec.yaml"),
            ],
        );

        let meta = DartResolver.detect(&files);
        assert_eq!(meta.modules.len(), 1);
        assert_eq!(meta.modules[0].name, "app");
        assert!(meta.excludes.contains(&"app/.dart_tool".to_string()));

        // .dart_tool/hack.d is covered under .dart_tool exclude rules
        assert!(crate::scan::path_under_excludes(
            &RelPath::new(".dart_tool/hack.d"),
            &[".dart_tool".to_string()]
        ));
    }

    #[test]
    fn pubspec_under_plain_build_directory_is_kept() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("build/tools/lib")).unwrap();
        fs::write(root.join("build/tools/pubspec.yaml"), "name: tools\n").unwrap();
        let files = FileIndex::new(
            root,
            [
                RelPath::new("build/tools/pubspec.yaml"),
                RelPath::new("build/tools/lib/t.dart"),
                RelPath::new("app/lib/a.dart"),
            ],
        );
        let meta = DartResolver.detect(&files);
        assert_eq!(meta.modules.len(), 1);
        assert_eq!(meta.modules[0].name, "tools");
        let from = RelPath::new("app/lib/a.dart");
        assert_eq!(
            DartResolver
                .resolve(&from, "package:tools/t.dart", &files, &meta)
                .map(|p| p.as_str().to_string())
                .as_deref(),
            Some("build/tools/lib/t.dart")
        );
    }
}
