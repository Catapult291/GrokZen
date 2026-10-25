//! Kotlin (`.kt`, `.kts`) import resolution.
//!
//! Kotlin, unlike Java, does not tie packages to directories, and one file may
//! declare many top-level classes, functions and properties
//! (`kotlinx-coroutines-core/common/src/Builders.common.kt` declares
//! `launch`, `async`, ... in package `kotlinx.coroutines`). Path arithmetic
//! (`a.b.C` → `a/b/C.kt`) therefore misses most imports, so the resolver
//! indexes declarations instead.
//!
//! # `detect`
//!
//! Every `.kt` / `.kts` / `.java` file (bounded by [`MAX_FILES`]) is read
//! once and scanned line by line:
//!   * `package a.b.c` (first one, comments and `@file:` annotations skipped);
//!   * Kotlin: every **top-level** declaration — a line starting in column 0
//!     whose first keyword after modifiers/annotations is `class`,
//!     `interface`, `object`, `fun`, `val`, `var` or `typealias`. Receiver
//!     and type-parameter prefixes are stripped (`fun <T> Foo.bar(` → `bar`);
//!   * Java: the file stem (the public type), so Kotlin→Java imports in
//!     mixed Gradle modules resolve too.
//!
//! Each hit becomes a [`TypeDecl`] `(package, name, file)` in
//! `ProjectMeta::type_decls`, sorted so `resolve` can binary-search.
//!
//! # `resolve`
//!
//! The specifier `a.b.C.D` (optional `import`, `.*`, `as X` stripped) is tried
//! as `package = a.b.C, name = D`, then `package = a.b, name = C`, and so on
//! down to a one-segment package. Backing off is what makes nested classes
//! (`a.b.Outer.Inner` → the file declaring `Outer`) work. When several files
//! declare the same top-level name (KMP `expect`/`actual`, overloads split
//! across files) the one sharing the longest directory prefix with the
//! importing file wins; ties break on path order, so the result is
//! deterministic.
//!
//! With several declaring files the single-file `resolve` keeps that rule, but
//! `resolve_import` returns [`ImportResolution::Files`] with *all* of them
//! (excluding the importer), so a KMP `expect`/`actual` set (common, android,
//! ios) is fully linked instead of only one platform source set.
//!
//! A star import naming a package (`import a.b.*`) is `None` — a package is
//! not a file, exactly like the Java resolver. A star import naming a type
//! (`import a.b.Outer.*`) resolves to that type's file. Stdlib and third-party
//! imports find no declaration and return `None`.

use std::collections::BTreeSet;

use crate::types::{
    FileIndex, ImportResolution, Language, ModuleResolver, ProjectMeta, RelPath, TypeDecl,
};

pub struct KotlinResolver;

/// Upper bound on files read during `detect`.
const MAX_FILES: usize = 250_000;
use crate::types::READ_CHUNK;

impl ModuleResolver for KotlinResolver {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut decls: BTreeSet<TypeDecl> = BTreeSet::new();
        let mut namespaces: BTreeSet<String> = BTreeSet::new();
        let mut cands: Vec<&RelPath> = Vec::new();
        for p in files.iter() {
            let ext = p.extension();
            if !(ext == Some("java") || matches!(ext, Some("kt" | "kts"))) {
                continue;
            }
            if cands.len() >= MAX_FILES {
                tracing::warn!(
                    limit = MAX_FILES,
                    "kotlin declaration scan stopped: too many source files; later declarations stay unresolved"
                );
                break;
            }
            cands.push(p);
        }
        // Read in parallel, chunked so memory stays bounded on huge repos;
        // the fold below stays sequential and in path order.
        for chunk in cands.chunks(READ_CHUNK) {
            let names: Vec<&str> = chunk.iter().map(|p| p.as_str()).collect();
            let texts = files.read_many(&names);
            for (p, src) in chunk.iter().zip(texts) {
                let java = p.extension() == Some("java");
                let Some(src) = src else {
                    continue;
                };
                let pkg = package_decl(&src).unwrap_or_default();
                namespaces.insert(pkg.clone());
                if java {
                    let stem = p.file_name().trim_end_matches(".java");
                    decls.insert(TypeDecl {
                        namespace: pkg,
                        name: stem.to_owned(),
                        file: p.as_str().to_owned(),
                    });
                    continue;
                }
                for name in top_level_names(&src) {
                    decls.insert(TypeDecl {
                        namespace: pkg.clone(),
                        name,
                        file: p.as_str().to_owned(),
                    });
                }
            }
        }
        ProjectMeta {
            type_decls: decls.into_iter().collect(),
            ..Default::default()
        }
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        _files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let (pkg, simple) = locate(spec, meta)?;
        let file = lookup(&meta.type_decls, &pkg, &simple, from.as_str())?;
        // `import q.Self.Companion.x` inside `Self.kt` names the file itself.
        (file != from.as_str()).then(|| RelPath::new(file))
    }

    fn resolve_import(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> ImportResolution {
        let Some((pkg, simple)) = locate(spec, meta) else {
            return ImportResolution::Unresolved;
        };
        let all = lookup_all(&meta.type_decls, &pkg, &simple);
        if all.len() > 1 {
            let targets: Vec<RelPath> = all
                .into_iter()
                .filter(|f| *f != from.as_str())
                .map(RelPath::new)
                .collect();
            return if targets.is_empty() {
                ImportResolution::Unresolved
            } else {
                ImportResolution::Files(targets)
            };
        }
        match self.resolve(from, spec, files, meta) {
            Some(p) => ImportResolution::File(p),
            None => ImportResolution::Unresolved,
        }
    }
}

/// `(package, simple name)` of the first backoff level that has a declaration.
fn locate(spec: &str, meta: &ProjectMeta) -> Option<(String, String)> {
    let (name, star) = normalize_spec(spec)?;
    let segs: Vec<&str> = name.split('.').collect();
    if segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    // `import a.b.*` naming a package: not a file.
    if star && meta.type_decls.iter().any(|d| d.namespace == name) {
        return None;
    }
    for n in (1..segs.len()).rev() {
        let pkg = segs[..n].join(".");
        let simple = segs[n];
        if !lookup_all(&meta.type_decls, &pkg, simple).is_empty() {
            return Some((pkg, simple.to_string()));
        }
    }
    None
}

/// Every file declaring `pkg.name`, in path order.
fn lookup_all<'m>(decls: &'m [TypeDecl], pkg: &str, name: &str) -> Vec<&'m str> {
    let start = decls.partition_point(|d| (d.namespace.as_str(), d.name.as_str()) < (pkg, name));
    let mut v: Vec<&str> = decls[start..]
        .iter()
        .take_while(|d| d.namespace == pkg && d.name == name)
        .map(|d| d.file.as_str())
        .collect();
    v.dedup();
    v
}

/// Best file declaring `pkg.name`, nearest to `from`.
pub(crate) fn lookup<'m>(
    decls: &'m [TypeDecl],
    pkg: &str,
    name: &str,
    from: &str,
) -> Option<&'m str> {
    let start = decls.partition_point(|d| (d.namespace.as_str(), d.name.as_str()) < (pkg, name));
    decls[start..]
        .iter()
        .take_while(|d| d.namespace == pkg && d.name == name)
        .map(|d| d.file.as_str())
        .max_by_key(|f| (common_dir_prefix(f, from), std::cmp::Reverse(*f)))
}

/// Number of shared leading directory components.
fn common_dir_prefix(a: &str, b: &str) -> usize {
    let da = a.rsplit_once('/').map_or("", |(d, _)| d);
    let db = b.rsplit_once('/').map_or("", |(d, _)| d);
    da.split('/')
        .zip(db.split('/'))
        .take_while(|(x, y)| x == y && !x.is_empty())
        .count()
}

/// Strip `import`, ``backticks``, ` as X`, `.*`, `;`, whitespace.
fn normalize_spec(spec: &str) -> Option<(String, bool)> {
    let mut s = spec.trim();
    if let Some(rest) = s.strip_prefix("import") {
        if rest.starts_with(char::is_whitespace) {
            s = rest.trim_start();
        }
    }
    if let Some((head, _alias)) = s.split_once(" as ") {
        s = head;
    }
    let s = s.trim_end_matches(';').trim();
    let mut name: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '`')
        .collect();
    let star = if let Some(stripped) = name.strip_suffix(".*") {
        name = stripped.to_owned();
        true
    } else {
        false
    };
    (!name.is_empty()).then_some((name, star))
}

/// Iterate source lines with `//` and `/* */` comments removed.
fn code_lines(src: &str) -> impl Iterator<Item = String> + '_ {
    let mut in_block = false;
    src.lines().map(move |raw| {
        let mut out = String::new();
        let mut rest = raw;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(i) => {
                        rest = &rest[i + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            } else {
                let lc = rest.find("//");
                let bc = rest.find("/*");
                match (lc, bc) {
                    (Some(l), Some(b)) if l < b => {
                        out.push_str(&rest[..l]);
                        break;
                    }
                    (Some(l), None) => {
                        out.push_str(&rest[..l]);
                        break;
                    }
                    (_, Some(b)) => {
                        out.push_str(&rest[..b]);
                        rest = &rest[b + 2..];
                        in_block = true;
                    }
                    (None, None) => {
                        out.push_str(rest);
                        break;
                    }
                }
            }
        }
        out
    })
}

/// First `package a.b.c` (Kotlin or Java) in a source file.
pub(crate) fn package_decl(src: &str) -> Option<String> {
    // Paren depth of an annotation that spans lines
    // (`@file:Suppress(\n  "X",\n)` above the package line).
    let mut depth = 0i32;
    for line in code_lines(src) {
        let t = line.trim();
        if depth > 0 || t.starts_with('@') {
            depth += paren_delta(t);
            continue;
        }
        if t.is_empty() {
            continue;
        }
        let rest = t.strip_prefix("package")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let pkg: String = rest
            .trim()
            .trim_end_matches(';')
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '`')
            .collect();
        return (!pkg.is_empty()).then_some(pkg);
    }
    None
}

/// Net `(` minus `)` outside string literals.
fn paren_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut in_str = false;
    let mut prev = '\0';
    for c in line.chars() {
        match c {
            '"' if prev != '\\' => in_str = !in_str,
            '(' if !in_str => delta += 1,
            ')' if !in_str => delta -= 1,
            _ => {}
        }
        prev = c;
    }
    delta
}

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "internal",
    "protected",
    "open",
    "final",
    "abstract",
    "sealed",
    "data",
    "enum",
    "annotation",
    "inline",
    "value",
    "expect",
    "actual",
    "const",
    "suspend",
    "operator",
    "infix",
    "tailrec",
    "external",
    "lateinit",
    "inner",
    "override",
];

/// Names of top-level declarations (column-0 lines).
pub(crate) fn top_level_names(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in code_lines(src) {
        if line.starts_with(char::is_whitespace) || line.is_empty() {
            continue;
        }
        if let Some(name) = declared_name(&line) {
            out.push(name);
        }
    }
    out
}

fn declared_name(line: &str) -> Option<String> {
    let mut rest = line.trim();
    // Annotations: `@Foo`, `@Foo(...)`, `@file:...` — skip them.
    loop {
        if let Some(r) = rest.strip_prefix('@') {
            let end = r
                .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | ':')))
                .unwrap_or(r.len());
            let mut r2 = &r[end..];
            if r2.starts_with('(') {
                let close = matching_paren(r2)?;
                r2 = &r2[close + 1..];
            }
            rest = r2.trim_start();
            continue;
        }
        let word_end = rest
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(rest.len());
        let word = &rest[..word_end];
        if MODIFIERS.contains(&word) {
            rest = rest[word_end..].trim_start();
            continue;
        }
        let after = rest[word_end..].trim_start();
        return match word {
            "class" | "interface" | "object" | "typealias" => ident_prefix(after),
            "fun" => {
                // `fun interface X`
                if let Some(r) = after.strip_prefix("interface") {
                    if r.starts_with(char::is_whitespace) {
                        return ident_prefix(r.trim_start());
                    }
                }
                callable_name(after, '(')
            }
            "val" | "var" => callable_name(after, ':'),
            _ => None,
        };
    }
}

fn matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn ident_prefix(s: &str) -> Option<String> {
    if let Some(r) = s.strip_prefix('`') {
        return r.split_once('`').map(|(n, _)| n.to_owned());
    }
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| s[..end].to_owned())
}

/// `<T> Foo<T>.bar(` / `Int.doubled: Int` → `bar` / `doubled`.
fn callable_name(s: &str, stop: char) -> Option<String> {
    let mut s = s.trim_start();
    if s.starts_with('<') {
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in s.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        s = s[end? + 1..].trim_start();
    }
    if s.starts_with('(') {
        return None; // destructuring `val (a, b)` — rare at top level
    }
    // Head up to the stop char / `=` / ` by`, at angle-depth 0.
    let mut depth = 0i32;
    let mut end = s.len();
    let mut tick = false;
    for (i, c) in s.char_indices() {
        match c {
            '`' => tick = !tick,
            _ if tick => {}
            '<' => depth += 1,
            '>' => depth -= 1,
            c if depth == 0 && (c == stop || c == '=' || c == '(' || c.is_whitespace()) => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let head = &s[..end];
    // Last `.`-segment outside angle brackets (and outside backticks).
    let mut depth = 0i32;
    let mut tick = false;
    let mut last = 0;
    for (i, c) in head.char_indices() {
        match c {
            '`' => tick = !tick,
            '<' if !tick => depth += 1,
            '>' if !tick => depth -= 1,
            '.' if depth == 0 && !tick => last = i + 1,
            _ => {}
        }
    }
    let name = head[last..].trim_matches('?').trim_matches('`');
    (!name.is_empty() && !name.contains(['<', '>', ')'])).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(root: &std::path::Path, files: &[(&str, &str)]) -> FileIndex {
        for (p, src) in files {
            let full = root.join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, src).unwrap();
        }
        FileIndex::new(root, files.iter().map(|(p, _)| RelPath::new(p)))
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("astro-kt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn declared_names() {
        let cases = [
            ("class Foo<T>(val x: Int)", Some("Foo")),
            ("public data class Bar(", Some("Bar")),
            ("fun interface Act {", Some("Act")),
            ("sealed interface S", Some("S")),
            ("object Obj : Base()", Some("Obj")),
            ("typealias Cb = () -> Unit", Some("Cb")),
            ("fun <T> CoroutineScope.launch(block: T)", Some("launch")),
            (
                "public suspend fun <T> Flow<T>.toList(): List<T>",
                Some("toList"),
            ),
            ("internal val Int.doubled: Int get() = 1", Some("doubled")),
            ("const val MAX = 1", Some("MAX")),
            ("val x by lazy { 1 }", Some("x")),
            ("@JvmName(\"a\") fun b() = 1", Some("b")),
            ("@file:JvmName(\"X\")", None),
            ("import a.b.C", None),
            ("expect class Platform", Some("Platform")),
            ("fun `weird name`() {}", Some("weird name")),
            ("val (a, b) = 1 to 2", None),
        ];
        for (line, want) in cases {
            assert_eq!(declared_name(line).as_deref(), want, "{line}");
        }
    }

    #[test]
    fn package_and_comments() {
        let src =
            "/* header\n package no */\n@file:JvmName(\"X\")\n// c\npackage a.b.c\n\nclass Q\n";
        assert_eq!(package_decl(src).as_deref(), Some("a.b.c"));
        assert_eq!(package_decl("class X").as_deref(), None);
        assert_eq!(
            package_decl("package x.y;\nclass J {}").as_deref(),
            Some("x.y")
        );
        assert_eq!(top_level_names(src), vec!["Q".to_owned()]);
    }

    #[test]
    fn normalize() {
        assert_eq!(normalize_spec("a.b.C"), Some(("a.b.C".into(), false)));
        assert_eq!(normalize_spec("import a.b.*"), Some(("a.b".into(), true)));
        assert_eq!(normalize_spec("a.b.C as D"), Some(("a.b.C".into(), false)));
        assert_eq!(normalize_spec("a.`fun`.C"), Some(("a.fun.C".into(), false)));
        assert_eq!(normalize_spec("  "), None);
    }

    #[test]
    fn resolves_by_declaration_not_path() {
        let root = tmp("decl");
        let files = index_with(
            &root,
            &[
                (
                    "core/common/src/Builders.common.kt",
                    "package kotlinx.coroutines\n\nfun CoroutineScope.launch() {}\npublic fun <T> async(): T = TODO()\n",
                ),
                (
                    "core/common/src/Job.kt",
                    "package kotlinx.coroutines\n\npublic interface Job {\n    class Key\n}\n",
                ),
                (
                    "core/jvm/src/Dispatchers.kt",
                    "package kotlinx.coroutines\n\nactual object Dispatchers\n",
                ),
                (
                    "core/native/src/Dispatchers.kt",
                    "package kotlinx.coroutines\n\nactual object Dispatchers\n",
                ),
                (
                    "core/jvm/test/LaunchTest.kt",
                    "package kotlinx.coroutines\n\nclass LaunchTest\n",
                ),
                (
                    "lib/src/main/java/com/acme/Util.java",
                    "package com.acme;\npublic class Util {}\n",
                ),
            ],
        );
        let r = KotlinResolver;
        let meta = r.detect(&files);
        let from = RelPath::new("core/jvm/test/LaunchTest.kt");
        let res = |spec: &str| {
            r.resolve(&from, spec, &files, &meta)
                .map(|p| p.as_str().to_owned())
        };
        assert_eq!(
            res("kotlinx.coroutines.launch").as_deref(),
            Some("core/common/src/Builders.common.kt")
        );
        assert_eq!(
            res("kotlinx.coroutines.async").as_deref(),
            Some("core/common/src/Builders.common.kt")
        );
        // Nested class backs off to the outer declaration.
        assert_eq!(
            res("kotlinx.coroutines.Job.Key").as_deref(),
            Some("core/common/src/Job.kt")
        );
        // expect/actual duplicates: nearest directory wins.
        assert_eq!(
            res("kotlinx.coroutines.Dispatchers").as_deref(),
            Some("core/jvm/src/Dispatchers.kt")
        );
        // Kotlin → Java interop.
        assert_eq!(
            res("com.acme.Util").as_deref(),
            Some("lib/src/main/java/com/acme/Util.java")
        );
        // Star on a package: not a file. Star on a type: the type's file.
        assert_eq!(res("kotlinx.coroutines.*"), None);
        assert_eq!(
            res("kotlinx.coroutines.Job.*").as_deref(),
            Some("core/common/src/Job.kt")
        );
        // Stdlib / third party.
        assert_eq!(res("kotlin.collections.List"), None);
        assert_eq!(res("okio.Buffer"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn package_after_multiline_file_annotation() {
        let src = "/* license */\n@file:Suppress(\n  \"INVISIBLE_MEMBER\",\n  \"x(\",\n)\n\npackage mockwebserver3\n\nclass MockResponse\n";
        assert_eq!(package_decl(src).as_deref(), Some("mockwebserver3"));
        assert_eq!(
            package_decl("@file:JvmName(\"A\")\npackage a.b\n").as_deref(),
            Some("a.b")
        );
    }
}
