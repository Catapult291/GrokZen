//! C / C++ `#include` resolution (ObjC keeps its own resolver).
//!
//! The extractor strips quotes but keeps the brackets of `<x.h>`. Quoted
//! includes are an edge when a file in this repository matches. Angle
//! includes skip the includer's directory, the repo root and bare-name tail
//! matching, so `<stdio.h>` never links to a local `stdio.h`; they match only
//! through include directories or a path whose prefix ends in `include`,
//! `inc`, `includes`, `public` or `pub`.
//!
//! Order:
//! 1. relative to the including file's directory;
//! 2. each include directory (`compile_commands.json` `-I`/`-isystem`/`-iquote`,
//!    then CMake `include_directories` / `target_include_directories`),
//!    then the repo root;
//! 3. fallback: a file whose path ends with the spec (`a/b.h` matches
//!    `x/a/b.h`), nearest to the importer; ambiguity → longest shared
//!    directory prefix, then shortest path, then lexicographic.
//!
//! Extension-less specs (`<vector>`) only match through steps 1–2.
//! Pairing `.h` with its `.c`/`.cpp` is not an include; [`pair_edges`] adds
//! it as its own [`crate::types::EdgeKind::Pair`] edge.
//!
//! `detect` records include dirs in `ProjectMeta::source_roots` and indexes
//! files by name in `ProjectMeta::modules`, so `resolve` is disk-free.

use std::sync::OnceLock;

use regex::Regex;

use crate::types::{
    join_rel, CodeEdge, CodeFile, Confidence, EdgeKind, FileIndex, Language, ModuleResolver,
    ModuleUnit, ProjectMeta, RelPath,
};

pub struct CResolver;
pub struct CppResolver;

impl ModuleResolver for CResolver {
    fn language(&self) -> Language {
        Language::C
    }
    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        detect(files)
    }
    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        resolve(from, spec, files, meta)
    }
}

impl ModuleResolver for CppResolver {
    fn language(&self) -> Language {
        Language::Cpp
    }
    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        detect(files)
    }
    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        resolve(from, spec, files, meta)
    }
}

const INCLUDABLE: [&str; 14] = [
    "h", "hh", "hpp", "hxx", "h++", "inc", "inl", "def", "tcc", "ipp", "cuh", "c", "cc", "cpp",
];

fn includable(path: &RelPath) -> bool {
    path.extension()
        .is_some_and(|e| INCLUDABLE.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn detect(files: &FileIndex) -> ProjectMeta {
    let mut meta = ProjectMeta::default();
    for path in files.iter() {
        if includable(path) {
            meta.modules.push(ModuleUnit {
                name: path.file_name().to_owned(),
                dir: path.as_str().to_owned(),
            });
        }
    }
    meta.modules
        .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.dir.cmp(&b.dir)));

    let mut dirs: Vec<String> = Vec::new();
    for cc in ["compile_commands.json", "build/compile_commands.json"] {
        if let Some(text) = files.read(cc) {
            dirs.extend(compile_commands_dirs(&text, files));
        }
    }
    let cmakes: Vec<&RelPath> = files
        .iter()
        .filter(|p| p.file_name() == "CMakeLists.txt")
        .take(500)
        .collect();
    for p in cmakes {
        if let Some(text) = files.read(p.as_str()) {
            dirs.extend(cmake_dirs(&text, p.dir()));
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| (d.is_empty() || files.has_dir(d)) && seen.insert(d.clone()));
    meta.source_roots = dirs;
    meta
}

/// Repo-relative form of `dir` (absolute paths under the root are stripped).
fn to_repo_rel(dir: &str, base: &str, files: &FileIndex) -> Option<String> {
    let root = files.root().to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/');
    let dir = dir.replace('\\', "/");
    if dir.starts_with('/') || dir.chars().nth(1) == Some(':') {
        let rest = dir.strip_prefix(root)?;
        return join_rel("", rest);
    }
    join_rel(base, &dir)
}

fn compile_commands_dirs(text: &str, files: &FileIndex) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(entries) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.iter().take(200_000) {
        let wd = e.get("directory").and_then(|d| d.as_str()).unwrap_or("");
        let base = match to_repo_rel(wd, "", files) {
            Some(b) => b,
            // Build directory outside the repo: only absolute -I can help.
            None => String::from("\u{0}"),
        };
        let args: Vec<String> = if let Some(a) = e.get("arguments").and_then(|a| a.as_array()) {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        } else if let Some(c) = e.get("command").and_then(|c| c.as_str()) {
            c.split_whitespace().map(str::to_owned).collect()
        } else {
            continue;
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let dir = ["-I", "-isystem", "-iquote", "-idirafter"]
                .iter()
                .find_map(|f| {
                    a.strip_prefix(f).map(|r| {
                        if r.is_empty() {
                            it.next().cloned().unwrap_or_default()
                        } else {
                            r.to_owned()
                        }
                    })
                });
            let Some(dir) = dir else { continue };
            let dir = dir.trim_matches('"');
            if dir.is_empty() {
                continue;
            }
            let rel = if base == "\u{0}" {
                if dir.starts_with('/') {
                    to_repo_rel(dir, "", files)
                } else {
                    None
                }
            } else {
                to_repo_rel(dir, &base, files)
            };
            if let Some(r) = rel {
                out.push(r);
            }
        }
    }
    out
}

fn cmake_dirs(text: &str, cmake_dir: &str) -> Vec<String> {
    static CALL: OnceLock<Regex> = OnceLock::new();
    let call = CALL.get_or_init(|| {
        Regex::new(r"(?is)\b(target_include_directories|include_directories)\s*\(([^)]*)\)")
            .expect("static regex")
    });
    let mut out = Vec::new();
    for c in call.captures_iter(text) {
        let is_target = c[1].eq_ignore_ascii_case("target_include_directories");
        let mut toks = c[2].split_whitespace().peekable();
        if is_target {
            toks.next(); // target name
        }
        for t in toks {
            let t = t.trim_matches('"');
            if matches!(
                t.to_ascii_uppercase().as_str(),
                "PUBLIC" | "PRIVATE" | "INTERFACE" | "SYSTEM" | "BEFORE" | "AFTER"
            ) {
                continue;
            }
            // Generator expressions and unresolvable variables: skip.
            let t = t
                .replace("${CMAKE_CURRENT_SOURCE_DIR}", ".")
                .replace("${CMAKE_CURRENT_LIST_DIR}", ".");
            let (t, base) = if let Some(r) = t
                .strip_prefix("${PROJECT_SOURCE_DIR}")
                .or_else(|| t.strip_prefix("${CMAKE_SOURCE_DIR}"))
            {
                (r.trim_start_matches('/').to_owned(), "")
            } else {
                (t, cmake_dir)
            };
            if t.contains('$') || t.contains('<') || t.starts_with('/') {
                continue;
            }
            if let Some(r) = join_rel(base, &t) {
                out.push(r);
            }
        }
    }
    out
}

fn resolve(from: &RelPath, spec: &str, files: &FileIndex, meta: &ProjectMeta) -> Option<RelPath> {
    // `<x.h>` keeps its brackets from the extractor: a system-style include
    // that only matches through include directories (or an `include/`-like
    // directory), never the includer's own directory or a bare-name tail.
    let spec = spec.trim();
    let angle = spec.starts_with('<') && spec.ends_with('>');
    let spec = spec
        .trim_matches(|c| c == '"' || c == '<' || c == '>')
        .trim_start_matches("./");
    if spec.is_empty() {
        return None;
    }
    let hit = |p: String| files.contains(&p).then(|| RelPath::new(&p));
    let is_target = |p: &String| files.contains(p) && p != from.as_str();

    if !angle {
        if let Some(p) = join_rel(from.dir(), spec).filter(is_target) {
            return hit(p);
        }
    }
    for d in &meta.source_roots {
        if let Some(p) = join_rel(d, spec).filter(is_target) {
            return hit(p);
        }
    }
    if !angle {
        if let Some(p) = join_rel("", spec).filter(is_target) {
            return hit(p);
        }
    }
    // Tail match; needs an extension so `<vector>` stays external.
    let tail = spec.trim_start_matches("../");
    let name = tail.rsplit('/').next().unwrap_or(tail);
    if !name.contains('.') || tail.is_empty() {
        return None;
    }
    let start = meta.modules.partition_point(|m| m.name.as_str() < name);
    let suffix = format!("/{tail}");
    let pool: Vec<&str> = meta.modules[start..]
        .iter()
        .take_while(|m| m.name == name)
        .map(|m| m.dir.as_str())
        .filter(|p| *p != from.as_str() && (*p == tail || p.ends_with(&suffix)))
        .filter(|p| {
            !angle || {
                // Angle form: the matched prefix must end in an include-style dir.
                let prefix = p.strip_suffix(tail).unwrap_or("").trim_end_matches('/');
                matches!(
                    prefix.rsplit('/').next().unwrap_or(""),
                    "include" | "includes" | "inc" | "public" | "pub"
                )
            }
        })
        .collect();
    nearest(from.dir(), &pool).map(RelPath::new)
}

fn nearest<'a>(dir: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let shared = |p: &str| {
        dir.split('/')
            .zip(p.split('/'))
            .take_while(|(a, b)| a == b && !a.is_empty())
            .count()
    };
    candidates.iter().copied().min_by(|a, b| {
        shared(b)
            .cmp(&shared(a))
            .then_with(|| a.len().cmp(&b.len()))
            .then_with(|| a.cmp(b))
    })
}

// ------------------------------------------------------- header <-> source

const PAIR_HEADERS: [&str; 5] = ["h", "hh", "hpp", "hxx", "h++"];
const PAIR_SOURCES: [&str; 7] = ["c", "cc", "cpp", "cxx", "c++", "m", "mm"];
/// Source-tree names an `include/` tree is mirrored into.
const MIRROR_ROOTS: [&str; 3] = ["src", "source", "lib"];
/// A header with more same-stem sources than this is ambiguous (generated
/// per-platform variants) and gets no pair.
pub const MAX_PAIR_SOURCES: usize = 4;

fn ext_in(path: &str, set: &[&str]) -> bool {
    path.rsplit_once('.')
        .filter(|(_, e)| !e.contains('/'))
        .is_some_and(|(_, e)| set.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// `(dir, stem)` of a path: `a/b/foo.cc` -> `("a/b", "foo")`.
fn dir_stem(path: &str) -> (&str, &str) {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    (dir, stem)
}

fn join_dir(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// Header -> implementation pairs among `paths` (indexes into it), as
/// `(header, source)`.
///
/// A header pairs with the sources of the same stem in its own directory
/// (`foo.h` / `foo.c`). Failing that, a header under an `include/` tree pairs
/// with the mirrored source tree: `<p>/include/<rest>/foo.h` with
/// `<p>/{src,source,lib}/<rest>/foo.*`, else `<rest>` minus its first
/// directory, else the source root itself (`include/fmt/format.h` ->
/// `src/format.cc`). Name-based, no
/// compiler: callers label what they derive from it syntactic. One pass plus
/// hash lookups, so linear in the number of files.
pub fn header_source_pairs(paths: &[&str]) -> Vec<(usize, usize)> {
    use std::collections::HashMap;
    let mut sources: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, p) in paths.iter().enumerate() {
        if ext_in(p, &PAIR_SOURCES) {
            sources.entry(dir_stem(p)).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (h, p) in paths.iter().enumerate() {
        if !ext_in(p, &PAIR_HEADERS) {
            continue;
        }
        let (dir, stem) = dir_stem(p);
        let mut found: Vec<usize> = sources.get(&(dir, stem)).cloned().unwrap_or_default();
        if found.is_empty() {
            // Mirror: split at the last `include` segment of the directory.
            let segs: Vec<&str> = if dir.is_empty() {
                Vec::new()
            } else {
                dir.split('/').collect()
            };
            if let Some(at) = segs.iter().rposition(|s| s.eq_ignore_ascii_case("include")) {
                let prefix = segs[..at].join("/");
                let rest = &segs[at + 1..];
                // `<rest>`, `<rest>` minus its first directory, then the
                // source root itself (`include/jemalloc/internal/arena.h`
                // -> `src/arena.c`).
                let mut tails: Vec<String> = vec![rest.join("/")];
                if rest.len() >= 2 {
                    tails.push(rest[1..].join("/"));
                }
                if !rest.is_empty() {
                    tails.push(String::new());
                }
                'roots: for root in MIRROR_ROOTS {
                    for tail in &tails {
                        let cand = join_dir(&join_dir(&prefix, root), tail);
                        if let Some(hit) = sources.get(&(cand.as_str(), stem)) {
                            found = hit.clone();
                            break 'roots;
                        }
                    }
                }
            }
        }
        if found.is_empty() || found.len() > MAX_PAIR_SOURCES {
            continue;
        }
        out.extend(found.into_iter().map(|s| (h, s)));
    }
    out
}

/// [`header_source_pairs`] over the C-family files of a graph, as
/// [`EdgeKind::Pair`] edges `header -> source`.
pub fn pair_edges(files: &[CodeFile]) -> Vec<CodeEdge> {
    let c_family: Vec<&CodeFile> = files
        .iter()
        .filter(|f| {
            matches!(
                f.language,
                Some(Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp)
            )
        })
        .collect();
    let paths: Vec<&str> = c_family.iter().map(|f| f.path.as_str()).collect();
    header_source_pairs(&paths)
        .into_iter()
        .map(|(h, s)| CodeEdge {
            from: c_family[h].id.0,
            to: c_family[s].id.0,
            kind: EdgeKind::Pair,
            weight: 1,
            confidence: Confidence::Syntactic,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(root: &str, paths: &[&str]) -> FileIndex {
        FileIndex::new(root, paths.iter().copied().map(RelPath::new))
    }

    fn run(files: &FileIndex, from: &str, spec: &str) -> Option<String> {
        let meta = CResolver.detect(files);
        CResolver
            .resolve(&RelPath::new(from), spec, files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn relative_first() {
        let f = index(
            "/nonexistent",
            &["src/a.c", "src/a.h", "include/a.h", "src/sub/b.h"],
        );
        assert_eq!(run(&f, "src/a.c", "a.h").as_deref(), Some("src/a.h"));
        assert_eq!(
            run(&f, "src/a.c", "sub/b.h").as_deref(),
            Some("src/sub/b.h")
        );
    }

    #[test]
    fn tail_match_picks_nearest() {
        let f = index(
            "/nonexistent",
            &["liba/src/x.c", "liba/inc/util/u.h", "libb/inc/util/u.h"],
        );
        assert_eq!(
            run(&f, "liba/src/x.c", "util/u.h").as_deref(),
            Some("liba/inc/util/u.h")
        );
        assert_eq!(
            run(&f, "libb/y.c", "util/u.h").as_deref(),
            Some("libb/inc/util/u.h")
        );
    }

    #[test]
    fn system_headers_stay_external() {
        let f = index("/nonexistent", &["src/a.c", "src/a.h"]);
        assert_eq!(run(&f, "src/a.c", "stdio.h"), None);
        assert_eq!(run(&f, "src/a.cpp", "vector"), None);
    }

    #[test]
    fn angle_hits_when_repo_has_path() {
        let f = index("/nonexistent", &["src/a.c", "include/mylib/api.h"]);
        assert_eq!(
            run(&f, "src/a.c", "<mylib/api.h>").as_deref(),
            Some("include/mylib/api.h")
        );
    }

    #[test]
    fn angle_does_not_link_same_named_local_file() {
        let f = index(
            "/nonexistent",
            &["src/a.c", "src/stdio.h", "stdio.h", "util/stdio.h"],
        );
        assert_eq!(run(&f, "src/a.c", "<stdio.h>"), None);
        assert_eq!(
            run(&f, "src/a.c", "stdio.h").as_deref(),
            Some("src/stdio.h")
        );
    }

    #[test]
    fn angle_matches_include_dir_and_include_named_dirs() {
        let f = index("/nonexistent", &["src/a.c", "include/stdio.h", "src/x.h"]);
        assert_eq!(
            run(&f, "src/a.c", "<stdio.h>").as_deref(),
            Some("include/stdio.h")
        );
        // Not an include dir: no relative lookup for angle form.
        assert_eq!(run(&f, "src/a.c", "<x.h>"), None);
    }

    #[test]
    fn no_self_edge() {
        let f = index("/nonexistent", &["src/a.h"]);
        assert_eq!(run(&f, "src/a.h", "a.h"), None);
    }

    #[test]
    fn cmake_include_dirs_parsed() {
        let d = cmake_dirs(
            "include_directories(${PROJECT_SOURCE_DIR}/inc SYSTEM third/party)\n\
             target_include_directories(app PUBLIC \"${CMAKE_CURRENT_SOURCE_DIR}/pub\" $<BUILD_INTERFACE:x>)",
            "lib",
        );
        assert_eq!(d, vec!["inc", "lib/third/party", "lib/pub"]);
    }

    #[test]
    fn compile_commands_dirs_parsed() {
        let f = index("/repo", &["a.c"]);
        let json = r#"[{"directory":"/repo/build","command":"cc -I../inc -isystem /repo/vendor -I /repo/x -c a.c","file":"a.c"},
            {"directory":"/repo","arguments":["cc","-Isrc/inc"],"file":"a.c"}]"#;
        let d = compile_commands_dirs(json, &f);
        assert_eq!(d, vec!["inc", "vendor", "x", "src/inc"]);
    }

    #[test]
    fn detect_reads_compile_commands_and_resolves() {
        let tmp = std::env::temp_dir().join(format!("astro-ccpp-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("compile_commands.json"),
            r#"[{"directory":".","arguments":["cc","-Iinc/deep"],"file":"src/a.c"}]"#,
        )
        .unwrap();
        // `p.h` exists twice; only the -I dir makes `p.h` exact from src/.
        let f = index(
            tmp.to_str().unwrap(),
            &[
                "src/a.c",
                "inc/deep/p.h",
                "zzz/p.h",
                "compile_commands.json",
            ],
        );
        let meta = CResolver.detect(&f);
        assert_eq!(meta.source_roots, vec!["inc/deep"]);
        assert_eq!(
            CResolver
                .resolve(&RelPath::new("src/a.c"), "p.h", &f, &meta)
                .map(|p| p.as_str().to_owned())
                .as_deref(),
            Some("inc/deep/p.h")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cpp_language_wired() {
        let set = crate::resolvers::ResolverSet::detect(&index(
            "/nonexistent",
            &["src/a.cpp", "src/a.hpp"],
        ));
        let r = set.resolve(
            &RelPath::new("src/a.cpp"),
            "a.hpp",
            &index("/nonexistent", &["src/a.cpp", "src/a.hpp"]),
        );
        assert_eq!(
            r.map(|p| p.as_str().to_owned()).as_deref(),
            Some("src/a.hpp")
        );
    }

    #[test]
    fn headers_pair_with_same_dir_and_mirrored_sources() {
        let paths = [
            "src/server.h",
            "src/server.c",
            "src/util/sds.h",
            "src/util/sds.c",
            "include/fmt/format.h",
            "src/format.cc",
            "lib/include/foo/bar.hpp",
            "lib/src/foo/bar.cpp",
            "src/lonely.h",
            "deps/x.h",
            "other/x.c",
            "README.h.md",
        ];
        let got: Vec<(&str, &str)> = header_source_pairs(&paths)
            .into_iter()
            .map(|(h, s)| (paths[h], paths[s]))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/server.h", "src/server.c"),
                ("src/util/sds.h", "src/util/sds.c"),
                ("include/fmt/format.h", "src/format.cc"),
                ("lib/include/foo/bar.hpp", "lib/src/foo/bar.cpp"),
            ]
        );
    }

    #[test]
    fn same_dir_pair_wins_over_mirror_and_ambiguous_stems_get_none() {
        let paths = [
            "include/a.h",
            "include/a.c",
            "src/a.c",
            "p/b.h",
            "p/b.c",
            "p/b.cc",
            "p/b.cpp",
            "p/b.cxx",
            "p/b.m",
        ];
        let got: Vec<(&str, &str)> = header_source_pairs(&paths)
            .into_iter()
            .map(|(h, s)| (paths[h], paths[s]))
            .collect();
        assert_eq!(got, vec![("include/a.h", "include/a.c")]);
    }
}
