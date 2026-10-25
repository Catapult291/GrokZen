//! Java module resolution.
//!
//! Target: 100% of the 984 in-repo imports in the `gson` corpus.
//! Prior art resolved 0%: it turned `com.google.gson.Gson` into
//! `com/google/gson/Gson.java` and looked at the repo root, but the file is at
//! `gson/src/main/java/com/google/gson/Gson.java`.
//!
//! Java is the easiest of the five because package names map to directories
//! one-to-one. The only missing piece is the source root. Detect them from:
//!   * Maven convention: every `*/src/main/java` and `*/src/test/java`
//!     (the gson corpus has 12 of them across modules)
//!   * Gradle `sourceSets { main { java { srcDirs = [...] } } }`
//!   * fallback: any directory whose sub-path matches an observed package decl
//!
//! Must handle: multi-module builds, `import static a.b.C.method`, wildcard
//! `import a.b.*` (resolves to a package — emit edges to all files in it or
//! none, but be consistent and document which), and nested classes
//! (`a.b.Outer.Inner` resolves to `a/b/Outer.java`, so back off trailing
//! segments until a file matches).
//!
//! # Source-root detection (`detect`)
//!
//! Roots are collected into `ProjectMeta::source_roots` (sorted, de-duplicated)
//! from four sources, in this order:
//!
//! 1. **Layout convention.** Any path containing the directory sequence
//!    `src/<sourceSet>/java` yields the prefix up to and including `java`.
//!    This covers Maven's `src/main/java` / `src/test/java` and Gradle's
//!    custom source sets (`src/testFixtures/java`, `src/jmh/java`, ...).
//!    Matching is on whole path components, so `src/main/javascript` does not
//!    match.
//! 2. **Maven overrides.** `<sourceDirectory>` / `<testSourceDirectory>` in
//!    every `pom.xml`, relative to the pom's directory. `${basedir}` and
//!    `${project.basedir}` prefixes are stripped; anything else containing a
//!    property reference is ignored.
//! 3. **Gradle.** Every string literal following a `srcDir` / `srcDirs` /
//!    `setSrcDirs` call or assignment inside `build.gradle` /
//!    `build.gradle.kts`, relative to the build file's directory.
//! 4. **Package-declaration fallback.** For `.java` files not covered by any
//!    root found above, one file per directory is read (bounded), its
//!    `package a.b.c;` is parsed, and the directory minus the package path is
//!    the root. This rescues ad-hoc layouts (`src/`, repo root, ...).
//!
//! Directories from 2–4 are only kept when they exist in the file index.
//!
//! # Resolution (`resolve`)
//!
//! The specifier is normalised (optional leading `import` / `static`, trailing
//! `;`, whitespace) and split on `.`. For each candidate length from the full
//! specifier down to two segments, each source root is tried for
//! `<root>/<segs...>.java`; the first hit wins. Longer matches are preferred
//! over closer roots so `a.b.Outer.Inner` never picks `a/b/Outer.java` when
//! `a/b/Outer/Inner.java` exists. Among equal lengths, roots are ordered by
//! proximity to the importing file: its own root first, then roots of the same
//! module (`<module>/src/*/java`), then everything else.
//!
//! Backing off trailing segments is what makes nested classes
//! (`a.b.Outer.Inner` → `a/b/Outer.java`) and static member imports
//! (`import static a.b.C.method` → `a/b/C.java`) fall out of the same loop.
//!
//! **Kotlin targets.** In a mixed Java/Kotlin repo, `detect` also records the
//! top-level declarations of `.kt` files in `ProjectMeta::type_decls` (same
//! scanner as the Kotlin resolver), and `resolve` falls back to them when no
//! `.java` file matches, so `import com.z.KtThing;` links to `KtThing.kt`.
//! Nothing is read when the repo has no `.kt` file.
//!
//! Roots whose first-segment directory does not exist (`<root>/java` for
//! `java.util.List`) are pruned before the loop, so JDK and third-party
//! imports cost one directory lookup per root and return `None`, which is the
//! correct answer.
//!
//! # Wildcard imports
//!
//! `import a.b.*` is a *type-import-on-demand*; the name before `.*` may be a
//! package **or** a type (JLS §7.5.2 — `import java.util.Map.*;` is legal and
//! imports `Map`'s nested types). `import static a.b.C.*` always names a type.
//! The rule, applied uniformly:
//!
//! * if the name before `.*` denotes a **package** (a directory under some
//!   source root), [`JavaResolver::resolve`] still returns `None` (it cannot
//!   express "all files"), but [`JavaResolver::resolve_import`] returns
//!   [`ImportResolution::Files`]: every `.java` file directly in that package
//!   directory (all source roots) plus Kotlin files declaring that package,
//!   capped at [`WILDCARD_FILE_CAP`]. That is the honest, over-approximate
//!   reading of "on demand": the importer may use any of them. The Java
//!   parser reports `import com.y.*;` as plain `com.y`, so a spec whose last
//!   segment is lowercase, that no type file matches and that names a package
//!   directory is treated the same way. The importing
//!   file itself is never a target.
//! * otherwise resolve the name as a type exactly like a plain import, so
//!   `import a.b.Outer.*` and `import static a.b.C.*` land on `Outer.java` /
//!   `C.java`.

use std::collections::BTreeSet;

use crate::types::{
    join_rel, FileIndex, ImportResolution, Language, ModuleResolver, ProjectMeta, RelPath, TypeDecl,
};

pub struct JavaResolver;

/// Upper bound on `.java` files read from disk during the package-declaration
/// fallback. One file per directory is enough — every file in a directory
/// shares a package — so this caps I/O on huge, oddly laid-out repos.
const FALLBACK_DIR_LIMIT: usize = 16_384;

/// Most files one `import pkg.*` may link to.
const WILDCARD_FILE_CAP: usize = 300;

/// Upper bound on `.kt` files read for Java→Kotlin resolution.
const KOTLIN_FILE_LIMIT: usize = 250_000;

impl ModuleResolver for JavaResolver {
    fn language(&self) -> Language {
        Language::Java
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut roots: BTreeSet<String> = BTreeSet::new();

        // 1. Layout convention: `src/<set>/java` anywhere in the path.
        for p in files.iter() {
            if let Some(root) = convention_root(p.as_str()) {
                roots.insert(root.to_string());
            }
        }

        // 2. Maven `<sourceDirectory>` / `<testSourceDirectory>` overrides.
        // 3. Gradle `srcDirs`.
        for p in files.iter() {
            let dirs: Vec<String> = match p.file_name() {
                "pom.xml" => match files.read(p.as_str()) {
                    Some(src) => maven_source_dirs(&src),
                    None => continue,
                },
                "build.gradle" | "build.gradle.kts" => match files.read(p.as_str()) {
                    Some(src) => gradle_src_dirs(&src),
                    None => continue,
                },
                _ => continue,
            };
            for d in dirs {
                if let Some(full) = join_rel(p.dir(), &d) {
                    if files.has_dir(&full) {
                        roots.insert(full);
                    }
                }
            }
        }

        // 4. Package-declaration fallback for files no root covers.
        let mut seen_dirs: BTreeSet<&str> = BTreeSet::new();
        for p in files.iter() {
            if p.extension() != Some("java") {
                continue;
            }
            let dir = p.dir();
            if roots.iter().any(|r| under_root(r, p.as_str())) || !seen_dirs.insert(dir) {
                continue;
            }
            if seen_dirs.len() > FALLBACK_DIR_LIMIT {
                tracing::warn!(
                    limit = FALLBACK_DIR_LIMIT,
                    "java source-root fallback stopped: too many directories without a known source root; remaining packages stay unresolved"
                );
                break;
            }
            let Some(src) = files.read(p.as_str()) else {
                continue;
            };
            let Some(pkg) = package_decl(&src) else {
                continue;
            };
            if let Some(root) = root_from_package(dir, &pkg) {
                roots.insert(root.to_string());
            }
        }

        // 5. Kotlin declarations, so Java can import Kotlin classes.
        let mut decls: BTreeSet<TypeDecl> = BTreeSet::new();
        let mut kt: Vec<&RelPath> = Vec::new();
        for p in files.iter().filter(|p| p.extension() == Some("kt")) {
            if kt.len() >= KOTLIN_FILE_LIMIT {
                tracing::warn!(
                    limit = KOTLIN_FILE_LIMIT,
                    "java->kotlin declaration scan stopped: too many .kt files; later Kotlin classes cannot be imported from Java"
                );
                break;
            }
            kt.push(p);
        }
        for chunk in kt.chunks(crate::types::READ_CHUNK) {
            let names: Vec<&str> = chunk.iter().map(|p| p.as_str()).collect();
            let texts = files.read_many(&names);
            for (p, src) in chunk.iter().zip(texts) {
                let Some(src) = src else {
                    continue;
                };
                let pkg = crate::resolvers::kotlin::package_decl(&src).unwrap_or_default();
                for name in crate::resolvers::kotlin::top_level_names(&src) {
                    decls.insert(TypeDecl {
                        namespace: pkg.clone(),
                        name,
                        file: p.as_str().to_owned(),
                    });
                }
            }
        }

        ProjectMeta {
            source_roots: roots.into_iter().collect(),
            type_decls: decls.into_iter().collect(),
            ..Default::default()
        }
    }

    fn resolve_import(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> ImportResolution {
        let explicit = normalize_spec(spec).is_some_and(|(_, on_demand)| on_demand);
        let as_files = |targets: Vec<RelPath>| {
            if targets.is_empty() {
                ImportResolution::Unresolved
            } else {
                ImportResolution::Files(targets)
            }
        };
        if explicit {
            if let Some(targets) = wildcard_package_files(from, spec, files, meta, true) {
                return as_files(targets);
            }
        }
        if let Some(p) = self.resolve(from, spec, files, meta) {
            return ImportResolution::File(p);
        }
        // The Java parser reports `import com.y.*;` as plain `com.y` (the star
        // is only a capture flag). A lowercase last segment that is a package
        // directory and no type is read as the wildcard it came from.
        if !explicit {
            if let Some(targets) = wildcard_package_files(from, spec, files, meta, false) {
                return as_files(targets);
            }
        }
        ImportResolution::Unresolved
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let found = self.resolve_inner(from, spec, files, meta)?;
        // `import static a.B.CONST` inside `a/B.java` names the file itself.
        (found != *from).then_some(found)
    }
}

impl JavaResolver {
    fn resolve_inner(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        self.resolve_java(from, spec, files, meta)
            .or_else(|| resolve_kotlin(from, spec, meta))
    }

    fn resolve_java(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let (name, on_demand) = normalize_spec(spec)?;
        let segs: Vec<&str> = name.split('.').collect();
        // `import Foo;` is not legal Java and a single segment cannot name a
        // file under a package, so two segments is the floor.
        if segs.len() < 2 || !segs.iter().all(|s| is_identifier(s)) {
            return None;
        }

        // Roots that can possibly contain `segs[0]/...`, ordered by proximity
        // to the importing file. One small allocation per call.
        let mut roots: Vec<&str> = meta
            .source_roots
            .iter()
            .map(String::as_str)
            .filter(|r| files.has_dir(&join(r, segs[0])))
            .collect();
        if roots.is_empty() {
            return None;
        }
        roots.sort_by_key(|r| root_rank(r, from.as_str()));

        // Type-import-on-demand naming a package: not a file. See module docs.
        if on_demand {
            let pkg_dir = segs.join("/");
            if roots.iter().any(|r| files.has_dir(&join(r, &pkg_dir))) {
                return None;
            }
        }

        // Longest match first, then closest root.
        let mut cand = String::with_capacity(96);
        for n in (2..=segs.len()).rev() {
            for root in &roots {
                cand.clear();
                if !root.is_empty() {
                    cand.push_str(root);
                    cand.push('/');
                }
                cand.push_str(segs[0]);
                for s in &segs[1..n] {
                    cand.push('/');
                    cand.push_str(s);
                }
                cand.push_str(".java");
                if files.contains(&cand) {
                    return Some(RelPath::new(&cand));
                }
            }
        }
        None
    }
}

// ----------------------------------------------------------------- helpers

/// Fallback: the import names a Kotlin top-level declaration. Backs off
/// trailing segments like the Kotlin resolver does (`a.b.Outer.Inner`).
fn resolve_kotlin(from: &RelPath, spec: &str, meta: &ProjectMeta) -> Option<RelPath> {
    if meta.type_decls.is_empty() {
        return None;
    }
    let (name, on_demand) = normalize_spec(spec)?;
    let segs: Vec<&str> = name.split('.').collect();
    if segs.len() < 2 || !segs.iter().all(|s| is_identifier(s)) {
        return None;
    }
    if on_demand && meta.type_decls.iter().any(|d| d.namespace == name) {
        return None;
    }
    for n in (1..segs.len()).rev() {
        let pkg = segs[..n].join(".");
        if let Some(file) =
            crate::resolvers::kotlin::lookup(&meta.type_decls, &pkg, segs[n], from.as_str())
        {
            return Some(RelPath::new(file));
        }
    }
    None
}

/// `import a.b.*` naming a package: every `.java` file directly in that
/// package directory (all roots) plus Kotlin files declaring the package.
/// `None` when the import is not a package wildcard.
fn wildcard_package_files(
    from: &RelPath,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
    explicit: bool,
) -> Option<Vec<RelPath>> {
    let (name, _) = normalize_spec(spec)?;
    let segs: Vec<&str> = name.split('.').collect();
    if segs.len() < 2 || segs.iter().any(|s| !is_identifier(s)) {
        return None;
    }
    if !explicit
        && !segs
            .last()
            .is_some_and(|l| l.starts_with(|c: char| c.is_lowercase()))
    {
        return None;
    }
    let pkg_dir = segs.join("/");
    let mut out: BTreeSet<RelPath> = BTreeSet::new();
    let mut is_package = false;
    for root in &meta.source_roots {
        let dir = join(root, &pkg_dir);
        if !files.has_dir(&dir) || dir.is_empty() {
            continue;
        }
        is_package = true;
        for p in files.entries(&dir) {
            if p.extension() == Some("java") && p != from {
                out.insert(p.clone());
            }
        }
    }
    for d in meta.type_decls.iter().filter(|d| d.namespace == name) {
        is_package = true;
        let p = RelPath::new(&d.file);
        if p != *from {
            out.insert(p);
        }
    }
    if !is_package {
        return None;
    }
    Some(out.into_iter().take(WILDCARD_FILE_CAP).collect())
}

/// `"root/rest"`, or just `rest` for the repo-root source root.
fn join(root: &str, rest: &str) -> String {
    if root.is_empty() {
        rest.to_string()
    } else {
        let mut s = String::with_capacity(root.len() + 1 + rest.len());
        s.push_str(root);
        s.push('/');
        s.push_str(rest);
        s
    }
}

fn under_root(root: &str, path: &str) -> bool {
    root.is_empty() || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// 0 = `from` lives under this root, 1 = same module (`<m>/src/*/java`),
/// 2 = anywhere else.
fn root_rank(root: &str, from: &str) -> u8 {
    if under_root(root, from) {
        return 0;
    }
    match (module_of(root), module_of(from)) {
        (Some(a), Some(b)) if a == b => 1,
        _ => 2,
    }
}

/// Directory before the first `src/` component, i.e. the Maven/Gradle module
/// directory. `""` for a top-level `src/`; `None` when there is no `src/`.
fn module_of(path: &str) -> Option<&str> {
    let mut idx = 0;
    for comp in path.split('/') {
        if comp == "src" {
            return Some(path[..idx].trim_end_matches('/'));
        }
        idx += comp.len() + 1;
    }
    None
}

/// Prefix of `path` up to and including the first `src/<set>/java` component
/// sequence, if any.
fn convention_root(path: &str) -> Option<&str> {
    let comps: Vec<&str> = path.split('/').collect();
    // Need `src`, `<set>`, `java`, plus at least one more component (a file
    // or directory below the root) so a file literally named `java` at
    // `src/x/java` does not register a root.
    if comps.len() < 4 {
        return None;
    }
    let mut end = 0;
    for i in 0..comps.len() - 3 {
        end += comps[i].len() + 1; // include the trailing '/'
        if comps[i] == "src" && !comps[i + 1].is_empty() && comps[i + 2] == "java" {
            let root_end = end + comps[i + 1].len() + 1 + "java".len();
            return Some(&path[..root_end]);
        }
    }
    None
}

/// Strip an optional `import` / `static` keyword, trailing `;`, and all
/// whitespace. Returns the dotted name and whether it ended in `.*`.
fn normalize_spec(spec: &str) -> Option<(String, bool)> {
    let mut s = spec.trim();
    if let Some(rest) = s.strip_prefix("import") {
        if rest.starts_with(char::is_whitespace) {
            s = rest.trim_start();
        }
    }
    if let Some(rest) = s.strip_prefix("static") {
        if rest.starts_with(char::is_whitespace) {
            s = rest.trim_start();
        }
    }
    let s = s.trim_end_matches(';').trim();
    let mut name: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let on_demand = if let Some(stripped) = name.strip_suffix(".*") {
        name = stripped.to_string();
        true
    } else {
        false
    };
    if name.is_empty() {
        return None;
    }
    Some((name, on_demand))
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// `dir` minus the trailing `a/b/c` implied by `package a.b.c;`, when `dir`
/// actually ends with it on a component boundary.
fn root_from_package<'a>(dir: &'a str, pkg: &str) -> Option<&'a str> {
    if pkg.is_empty() {
        return None;
    }
    let pkg_path = pkg.replace('.', "/");
    let head = dir.strip_suffix(pkg_path.as_str())?;
    if head.is_empty() {
        Some("")
    } else {
        head.strip_suffix('/')
    }
}

/// First `package a.b.c;` declaration in a Java source, skipping comments.
fn package_decl(src: &str) -> Option<String> {
    let mut in_block = false;
    for raw in src.lines() {
        let mut line = raw.trim();
        loop {
            if in_block {
                match line.find("*/") {
                    Some(i) => {
                        in_block = false;
                        line = line[i + 2..].trim_start();
                    }
                    None => break,
                }
            }
            if line.starts_with("/*") {
                in_block = true;
                line = &line[2..];
                continue;
            }
            break;
        }
        if in_block || line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package") {
            if rest.starts_with(char::is_whitespace) {
                let name: String = rest
                    .split(';')
                    .next()?
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let valid = name.split('.').all(is_identifier);
                return valid.then_some(name);
            }
        }
        // Anything else that is not an annotation means we are past the
        // package declaration.
        if !line.starts_with('@') {
            return None;
        }
    }
    None
}

/// `<sourceDirectory>` and `<testSourceDirectory>` values from a pom.
fn maven_source_dirs(pom: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tag in ["sourceDirectory", "testSourceDirectory"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let mut rest = pom;
        while let Some(i) = rest.find(&open) {
            let after = &rest[i + open.len()..];
            let Some(j) = after.find(&close) else { break };
            let raw = after[..j].trim();
            let val = raw
                .strip_prefix("${project.basedir}/")
                .or_else(|| raw.strip_prefix("${basedir}/"))
                .unwrap_or(raw);
            if !val.is_empty() && !val.contains("${") {
                out.push(val.trim_matches('/').to_string());
            }
            rest = &after[j + close.len()..];
        }
    }
    out
}

/// String literals following `srcDir`/`srcDirs`/`setSrcDirs` in a Gradle
/// build script (Groovy or Kotlin DSL). Reads from the keyword to the end of
/// the statement, continuing across lines while a `[` / `(` is still open.
fn gradle_src_dirs(script: &str) -> Vec<String> {
    // `srcDir`, `srcDirs`, `setSrcDirs` (Kotlin DSL capitalises the S).
    fn find_keyword(s: &str) -> Option<usize> {
        match (s.find("srcDir"), s.find("SrcDir")) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    let mut out = Vec::new();
    let mut rest = script;
    while let Some(i) = find_keyword(rest) {
        let after = &rest[i + "srcDir".len()..];
        // Capture the statement from the keyword up to the first newline that
        // leaves no bracket open, so multi-line lists are kept whole.
        let mut depth: i32 = 0;
        let mut end = after.len();
        for (k, c) in after.char_indices() {
            match c {
                '[' | '(' => depth += 1,
                ']' | ')' => depth -= 1,
                '\n' if depth <= 0 => {
                    end = k;
                    break;
                }
                _ => {}
            }
        }
        let stmt = &after[..end];
        for lit in string_literals(stmt) {
            let lit = lit.trim().trim_matches('/');
            if !lit.is_empty() && !lit.contains('$') {
                out.push(lit.to_string());
            }
        }
        rest = &after[end..];
    }
    out
}

/// Contents of every `'...'` / `"..."` literal in `s`, in order.
fn string_literals(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'\'' || q == b'"' {
            if let Some(len) = s[i + 1..].find(q as char) {
                out.push(&s[i + 1..i + 1 + len]);
                i += len + 2;
                continue;
            }
            break;
        }
        i += 1;
    }
    out
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/nonexistent", paths.iter().map(RelPath::new))
    }

    fn resolve(files: &FileIndex, meta: &ProjectMeta, from: &str, spec: &str) -> Option<String> {
        JavaResolver
            .resolve(&RelPath::new(from), spec, files, meta)
            .map(|p| p.as_str().to_string())
    }

    #[test]
    fn single_module_maven_layout() {
        let files = index(&[
            "pom.xml",
            "src/main/java/com/acme/App.java",
            "src/main/java/com/acme/util/Strings.java",
            "src/test/java/com/acme/AppTest.java",
        ]);
        let meta = JavaResolver.detect(&files);
        assert_eq!(meta.source_roots, vec!["src/main/java", "src/test/java"]);

        let from = "src/main/java/com/acme/App.java";
        assert_eq!(
            resolve(&files, &meta, from, "com.acme.util.Strings").as_deref(),
            Some("src/main/java/com/acme/util/Strings.java")
        );
        // Test tree reaches into main.
        assert_eq!(
            resolve(
                &files,
                &meta,
                "src/test/java/com/acme/AppTest.java",
                "com.acme.App"
            )
            .as_deref(),
            Some("src/main/java/com/acme/App.java")
        );
        // Raw statement text is tolerated.
        assert_eq!(
            resolve(&files, &meta, from, "import com.acme.util.Strings;").as_deref(),
            Some("src/main/java/com/acme/util/Strings.java")
        );
    }

    #[test]
    fn multi_module_source_roots() {
        let files = index(&[
            "pom.xml",
            "core/pom.xml",
            "core/src/main/java/org/x/core/Engine.java",
            "core/src/test/java/org/x/core/EngineTest.java",
            "plugins/a/src/main/java/org/x/plugins/a/A.java",
            "plugins/b/src/main/java/org/x/plugins/b/B.java",
            "plugins/b/src/testFixtures/java/org/x/plugins/b/Fixture.java",
            // Must not register: `java` is not a whole component here.
            "web/src/main/javascript/org/x/App.js",
            "README.md",
        ]);
        let meta = JavaResolver.detect(&files);
        assert_eq!(
            meta.source_roots,
            vec![
                "core/src/main/java",
                "core/src/test/java",
                "plugins/a/src/main/java",
                "plugins/b/src/main/java",
                "plugins/b/src/testFixtures/java",
            ]
        );

        let from = "plugins/a/src/main/java/org/x/plugins/a/A.java";
        assert_eq!(
            resolve(&files, &meta, from, "org.x.core.Engine").as_deref(),
            Some("core/src/main/java/org/x/core/Engine.java")
        );
        assert_eq!(
            resolve(&files, &meta, from, "org.x.plugins.b.B").as_deref(),
            Some("plugins/b/src/main/java/org/x/plugins/b/B.java")
        );
        assert_eq!(
            resolve(&files, &meta, from, "org.x.plugins.b.Fixture").as_deref(),
            Some("plugins/b/src/testFixtures/java/org/x/plugins/b/Fixture.java")
        );
    }

    #[test]
    fn same_module_root_wins_ties() {
        // Identical FQN in two modules: the importer's own module wins.
        let files = index(&[
            "a/src/main/java/p/Shared.java",
            "a/src/test/java/p/T.java",
            "b/src/main/java/p/Shared.java",
        ]);
        let meta = JavaResolver.detect(&files);
        assert_eq!(
            resolve(&files, &meta, "a/src/test/java/p/T.java", "p.Shared").as_deref(),
            Some("a/src/main/java/p/Shared.java")
        );
        assert_eq!(
            resolve(&files, &meta, "b/src/main/java/p/Other.java", "p.Shared").as_deref(),
            Some("b/src/main/java/p/Shared.java")
        );
    }

    #[test]
    fn nested_class_backs_off_trailing_segments() {
        let files = index(&[
            "src/main/java/a/b/Outer.java",
            "src/main/java/a/b/Other.java",
            // A real package `a.b.Outer2` with class `Inner`: the longer match
            // must win over backing off to a hypothetical `Outer2.java`.
            "src/main/java/a/b/Outer2/Inner.java",
        ]);
        let meta = JavaResolver.detect(&files);
        let from = "src/main/java/a/b/Other.java";
        assert_eq!(
            resolve(&files, &meta, from, "a.b.Outer.Inner").as_deref(),
            Some("src/main/java/a/b/Outer.java")
        );
        assert_eq!(
            resolve(&files, &meta, from, "a.b.Outer.Inner.Deeper").as_deref(),
            Some("src/main/java/a/b/Outer.java")
        );
        assert_eq!(
            resolve(&files, &meta, from, "a.b.Outer2.Inner").as_deref(),
            Some("src/main/java/a/b/Outer2/Inner.java")
        );
        // Never backs off to a single segment.
        assert_eq!(resolve(&files, &meta, from, "a.b"), None);
        assert_eq!(resolve(&files, &meta, from, "Outer"), None);
    }

    #[test]
    fn static_import_strips_member() {
        let files = index(&["src/main/java/a/b/C.java", "src/test/java/a/b/CTest.java"]);
        let meta = JavaResolver.detect(&files);
        let from = "src/test/java/a/b/CTest.java";
        assert_eq!(
            resolve(&files, &meta, from, "static a.b.C.method").as_deref(),
            Some("src/main/java/a/b/C.java")
        );
        assert_eq!(
            resolve(&files, &meta, from, "import static a.b.C.CONSTANT;").as_deref(),
            Some("src/main/java/a/b/C.java")
        );
        // Static on-demand names a type, so it resolves to that type's file.
        assert_eq!(
            resolve(&files, &meta, from, "static a.b.C.*").as_deref(),
            Some("src/main/java/a/b/C.java")
        );
    }

    #[test]
    fn wildcard_on_package_is_none_but_on_type_resolves() {
        let files = index(&[
            "src/main/java/a/b/C.java",
            "src/main/java/a/b/D.java",
            "src/main/java/a/b/Outer.java",
        ]);
        let meta = JavaResolver.detect(&files);
        let from = "src/main/java/a/b/D.java";
        // Package on-demand import: not a single file.
        assert_eq!(resolve(&files, &meta, from, "a.b.*"), None);
        assert_eq!(resolve(&files, &meta, from, "import a.b.*;"), None);
        // Nested-type on-demand import: `Outer` is a type.
        assert_eq!(
            resolve(&files, &meta, from, "a.b.Outer.*").as_deref(),
            Some("src/main/java/a/b/Outer.java")
        );
    }

    #[test]
    fn jdk_and_third_party_return_none() {
        let files = index(&["src/main/java/com/acme/App.java"]);
        let meta = JavaResolver.detect(&files);
        let from = "src/main/java/com/acme/App.java";
        assert_eq!(resolve(&files, &meta, from, "java.util.List"), None);
        assert_eq!(resolve(&files, &meta, from, "java.util.Map.Entry"), None);
        assert_eq!(
            resolve(
                &files,
                &meta,
                from,
                "static java.util.Objects.requireNonNull"
            ),
            None
        );
        assert_eq!(
            resolve(&files, &meta, from, "javax.annotation.Nullable"),
            None
        );
        assert_eq!(resolve(&files, &meta, from, "org.junit.Test"), None);
        assert_eq!(resolve(&files, &meta, from, "com.acme.Missing"), None);
        assert_eq!(resolve(&files, &meta, from, ""), None);
        assert_eq!(resolve(&files, &meta, from, "com/acme/App"), None);
    }

    #[test]
    fn no_roots_means_nothing_resolves() {
        let files = index(&["com/acme/App.java", "com/acme/B.java"]);
        // Reads fail (fake root), so the package fallback yields nothing.
        let meta = JavaResolver.detect(&files);
        assert!(meta.source_roots.is_empty());
        assert_eq!(
            resolve(&files, &meta, "com/acme/B.java", "com.acme.App"),
            None
        );
    }

    #[test]
    fn gradle_and_pom_and_package_fallback_from_disk() {
        // Real files so `FileIndex::read` works.
        let tmp = std::env::temp_dir().join(format!(
            "astrolabe-java-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let write = |rel: &str, body: &str| {
            let p = tmp.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        write(
            "build.gradle",
            "plugins { id 'java' }\nsourceSets {\n  main {\n    java {\n      srcDirs = ['src', \"gen/java\"]\n    }\n  }\n}\n",
        );
        write("src/x/y/A.java", "package x.y;\npublic class A {}\n");
        write("gen/java/x/y/G.java", "package x.y;\npublic class G {}\n");
        write(
            "kts/build.gradle.kts",
            "sourceSets {\n  main {\n    java.srcDirs(\n      \"code\"\n    )\n  }\n}\n",
        );
        write("kts/code/k/K.java", "package k;\nclass K {}\n");
        write(
            "mvn/pom.xml",
            "<project><build><sourceDirectory>${project.basedir}/javasrc</sourceDirectory></build></project>",
        );
        write("mvn/javasrc/m/M.java", "package m;\nclass M {}\n");
        // No build config at all: package declaration is the only clue.
        write(
            "loose/code/q/r/Q.java",
            "/* header\n * comment */\n// line\n@SuppressWarnings(\"x\")\npackage q.r;\n\nimport x.y.A;\nclass Q {}\n",
        );
        write("loose/code/q/r/R.java", "package q.r;\nclass R {}\n");
        // Package that does not match its directory: must not create a root.
        write("bad/Z.java", "package not.here;\nclass Z {}\n");

        let rels = [
            "build.gradle",
            "src/x/y/A.java",
            "gen/java/x/y/G.java",
            "kts/build.gradle.kts",
            "kts/code/k/K.java",
            "mvn/pom.xml",
            "mvn/javasrc/m/M.java",
            "loose/code/q/r/Q.java",
            "loose/code/q/r/R.java",
            "bad/Z.java",
        ];
        let files = FileIndex::new(&tmp, rels.iter().map(RelPath::new));
        let meta = JavaResolver.detect(&files);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(
            meta.source_roots,
            vec!["gen/java", "kts/code", "loose/code", "mvn/javasrc", "src"]
        );
        assert_eq!(
            resolve(&files, &meta, "src/x/y/A.java", "x.y.G").as_deref(),
            Some("gen/java/x/y/G.java")
        );
        assert_eq!(
            resolve(&files, &meta, "loose/code/q/r/Q.java", "q.r.R").as_deref(),
            Some("loose/code/q/r/R.java")
        );
        assert_eq!(
            resolve(&files, &meta, "loose/code/q/r/Q.java", "x.y.A").as_deref(),
            Some("src/x/y/A.java")
        );
    }

    #[test]
    fn helper_edge_cases() {
        assert_eq!(
            convention_root("a/src/main/java/p/X.java"),
            Some("a/src/main/java")
        );
        assert_eq!(
            convention_root("src/test/java/X.java"),
            Some("src/test/java")
        );
        assert_eq!(convention_root("src/main/javascript/x.js"), None);
        assert_eq!(convention_root("src/main/java"), None);
        assert_eq!(convention_root("x/src/java/A.java"), None);

        assert_eq!(
            root_from_package("src/main/java/a/b", "a.b"),
            Some("src/main/java")
        );
        assert_eq!(root_from_package("a/b", "a.b"), Some(""));
        assert_eq!(root_from_package("xa/b", "a.b"), None);
        assert_eq!(root_from_package("src", "a.b"), None);

        assert_eq!(
            package_decl("// c\n/* x */ package a.b ; class X{}"),
            Some("a.b".into())
        );
        assert_eq!(package_decl("class X {}"), None);
        assert_eq!(
            package_decl("/* package fake.pkg; */\npackage real;"),
            Some("real".into())
        );

        assert_eq!(module_of("gson/src/main/java"), Some("gson"));
        assert_eq!(module_of("src/main/java"), Some(""));
        assert_eq!(module_of("lib/code"), None);

        assert_eq!(
            gradle_src_dirs(
                "java.srcDir 'a'\njava { srcDirs = ['b',\n 'c'] }\nsetSrcDirs(listOf(\"d\"))"
            ),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(
            maven_source_dirs("<sourceDirectory>${basedir}/s</sourceDirectory><testSourceDirectory>${weird}/t</testSourceDirectory>"),
            vec!["s"]
        );
    }

    /// Real-corpus acceptance run: `cargo test -p astrolabe-core java -- --ignored --nocapture`.
    ///
    /// Counts every non-wildcard `import` in `corpus/java` (gson, Maven
    /// multi-module) and reports how many resolve to a file in the repo. The
    /// hard target is the 984 plain single-type imports whose FQN maps
    /// directly to a `.java` file under some source root; nested-class and
    /// static-member imports that resolve on top of that are reported
    /// separately.
    #[test]
    #[ignore]
    fn corpus_gson_resolves_all_in_repo_imports() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../corpus/java")
            .canonicalize()
            .expect("corpus/java missing");

        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<RelPath>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == ".git") {
                        continue;
                    }
                    walk(&p, root, out);
                } else {
                    out.push(RelPath::new(
                        p.strip_prefix(root).unwrap().to_string_lossy(),
                    ));
                }
            }
        }
        let mut paths = Vec::new();
        walk(&root, &root, &mut paths);
        let files = FileIndex::new(&root, paths);
        let meta = JavaResolver.detect(&files);
        println!("source roots ({}):", meta.source_roots.len());
        for r in &meta.source_roots {
            println!("  {r}");
        }
        // The 984 baseline was measured against the 12 Maven-convention roots.
        // `gson/pom.xml` additionally declares `src/main/java-templates` for
        // the templating plugin; it is a real root (`GsonBuildConfig.java`)
        // and is reported as a bonus below, not folded into the baseline.
        let convention: Vec<&str> = meta
            .source_roots
            .iter()
            .map(String::as_str)
            .filter(|r| convention_root(&format!("{r}/x")) == Some(*r))
            .collect();
        assert_eq!(
            convention.len(),
            12,
            "gson has 12 Maven-convention source roots"
        );

        // Ground truth: does `<root>/<fqn>.java` exist verbatim under one of
        // the 12 convention roots?
        let exact_exists = |fqn: &str| {
            let rel = format!("{}.java", fqn.replace('.', "/"));
            convention.iter().any(|r| files.contains(&join(r, &rel)))
        };

        let (mut java_files, mut imports, mut exact_total, mut exact_hit) = (0, 0, 0, 0);
        let (mut extra_nested, mut extra_static, mut extra_roots) = (0, 0, 0);
        let mut misses: Vec<(String, String)> = Vec::new();
        for p in files.iter() {
            if p.extension() != Some("java") {
                continue;
            }
            java_files += 1;
            let src = files.read(p.as_str()).unwrap();
            for line in src.lines() {
                let t = line.trim();
                let Some(rest) = t.strip_prefix("import ") else {
                    continue;
                };
                let Some(stmt) = rest.split(';').next() else {
                    continue;
                };
                let stmt = stmt.trim();
                if stmt.ends_with(".*") {
                    continue;
                }
                imports += 1;
                let is_static = stmt.starts_with("static ");
                let fqn = stmt.trim_start_matches("static ").trim();
                let got = JavaResolver.resolve(p, stmt, &files, &meta);
                if !is_static && exact_exists(fqn) {
                    exact_total += 1;
                    match got {
                        Some(_) => exact_hit += 1,
                        None => misses.push((p.as_str().into(), fqn.into())),
                    }
                } else if let Some(target) = got {
                    if is_static {
                        extra_static += 1;
                    } else if convention.iter().any(|r| under_root(r, target.as_str())) {
                        extra_nested += 1;
                    } else {
                        extra_roots += 1;
                    }
                }
            }
        }
        for (f, s) in &misses {
            println!("MISS {f}: {s}");
        }
        println!(
            "java files: {java_files}, non-wildcard imports: {imports}\n\
             in-repo single-type imports (12 convention roots): {exact_total}, \
             resolved: {exact_hit} ({:.1}%)\n\
             additionally resolved: nested-class {extra_nested}, \
             static-member {extra_static}, via pom-declared roots {extra_roots}\n\
             total in-repo edges: {}",
            100.0 * exact_hit as f64 / exact_total.max(1) as f64,
            exact_hit + extra_nested + extra_static + extra_roots
        );
        assert_eq!(java_files, 264);
        assert_eq!(exact_total, 984, "corpus baseline changed");
        assert_eq!(exact_hit, exact_total, "every in-repo import must resolve");
    }
}
