//! Real-corpus acceptance tests for in-repository import resolution.
//!
//! Ground truth is derived from source text and the on-disk file set. It
//! never asks a resolver whether an import is internal:
//!
//! * Extractors mask comments and string literals, then parse real import
//!   syntax (including multi-line forms). Python `import a, b` is two
//!   specifiers; a Rust `use a::{b, c}` group is one statement if any expanded
//!   path points at a workspace crate.
//! * "In-repo" is a file-set / build-file check: Python source roots from
//!   `pyproject.toml` (plus namespace packages), Go module path from `go.mod`,
//!   Java FQN under Maven-convention roots, Rust crate names from `Cargo.toml`,
//!   TypeScript relative path existence.

use astrolabe_core::resolvers::ResolverSet;
use astrolabe_core::types::join_rel;
use astrolabe_core::{FileIndex, Language, RelPath};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct Import {
    from: RelPath,
    spec: String,
    source: String,
}

#[derive(Clone, Copy)]
enum Kind {
    Python,
    Go,
    Java,
    Rust,
    TypeScript,
    Dart,
}

struct Corpus {
    language: &'static str,
    name: &'static str,
    root: PathBuf,
    kind: Kind,
    threshold: f64,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("astrolabe-core must be inside the workspace")
        .to_path_buf()
}

/// Locate a corpus without assuming anyone's directory layout.
///
/// Order: `ASTROLABE_CORPUS_<NAME>` env override, then a vendored copy under
/// `corpus/<dir>`, then a sibling of the workspace. A hardcoded absolute path
/// would make this gate runnable only on the machine it was written on, which
/// for the project's primary quality gate is worse than no gate at all —
/// nobody else, CI included, could reproduce the numbers.
///
/// Returns the last candidate when none exist so the caller can report a
/// concrete missing path rather than a vague failure.
fn locate_corpus(env_key: &str, vendored: &str, sibling: &str) -> PathBuf {
    let workspace = workspace_root();
    if let Some(p) = std::env::var_os(env_key).map(PathBuf::from) {
        return p;
    }
    let vendored = workspace.join(vendored);
    if vendored.is_dir() {
        return vendored;
    }
    workspace
        .parent()
        .map(|p| p.join(sibling))
        .unwrap_or(vendored)
}

fn corpus(kind: Kind) -> Corpus {
    let workspace = workspace_root();
    match kind {
        Kind::Python => Corpus {
            language: "Python",
            name: "serena",
            root: locate_corpus("ASTROLABE_CORPUS_PYTHON", "corpus/python", "serena"),
            kind,
            // Measured at 100%. The original 0.95 was a target set before the
            // resolver existed; leaving it there would let a real regression
            // of up to 78 imports pass unnoticed.
            threshold: 1.0,
        },
        Kind::Go => Corpus {
            language: "Go",
            name: "gin",
            root: workspace.join("corpus/go"),
            kind,
            threshold: 1.0,
        },
        Kind::Java => Corpus {
            language: "Java",
            name: "gson",
            root: workspace.join("corpus/java"),
            kind,
            threshold: 1.0,
        },
        Kind::Rust => Corpus {
            language: "Rust",
            name: "ripgrep",
            root: workspace.join("corpus/rust"),
            kind,
            threshold: 1.0,
        },
        Kind::TypeScript => Corpus {
            language: "TypeScript",
            name: "openvisio-oss",
            root: locate_corpus(
                "ASTROLABE_CORPUS_TYPESCRIPT",
                "corpus/typescript",
                "openvisio-oss",
            ),
            kind,
            threshold: 1.0,
        },
        Kind::Dart => Corpus {
            language: "Dart",
            name: "flutter-packages",
            root: locate_corpus(
                "ASTROLABE_CORPUS_DART",
                "corpus/flutter-packages",
                "flutter-packages",
            ),
            kind,
            threshold: 1.0,
        },
    }
}

fn ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".venv"
            | "venv"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "coverage"
            | "__pycache__"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".next"
            | ".openvisio"
            | ".turbo"
            | ".cache"
            | ".dart_tool"
    )
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<RelPath>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !ignored_dir(&entry.file_name().to_string_lossy()) {
                walk(&path, root, out);
            }
        } else if kind.is_file() {
            if let Ok(relative) = path.strip_prefix(root) {
                out.push(RelPath::new(relative.to_string_lossy()));
            }
        }
    }
}

fn scan(root: &Path) -> (FileIndex, Vec<RelPath>) {
    assert!(root.is_dir(), "语料目录不存在: {}", root.display());
    let mut paths = Vec::new();
    walk(root, root, &mut paths);
    paths.sort();
    let index = FileIndex::new(root, paths.iter().cloned());
    (index, paths)
}

fn read(root: &Path, path: &RelPath) -> Option<String> {
    fs::read_to_string(root.join(path.as_str())).ok()
}

fn blank_keep_nl(c: u8) -> u8 {
    if c == b'\n' {
        b'\n'
    } else {
        b' '
    }
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Mask Python comments and string literals (including triples). Newlines stay
/// so line structure survives for continuation handling.
fn mask_python(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            while i < b.len() && b[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            let triple = i + 2 < b.len() && b[i + 1] == quote && b[i + 2] == quote;
            if triple {
                out.extend([b' ', b' ', b' ']);
                i += 3;
                while i + 2 < b.len() && !(b[i] == quote && b[i + 1] == quote && b[i + 2] == quote)
                {
                    out.push(blank_keep_nl(b[i]));
                    i += 1;
                }
                for _ in 0..3 {
                    if i < b.len() {
                        out.push(b' ');
                        i += 1;
                    }
                }
                continue;
            }
            out.push(b' ');
            i += 1;
            while i < b.len() && b[i] != quote {
                if b[i] == b'\\' && i + 1 < b.len() {
                    out.push(b' ');
                    out.push(blank_keep_nl(b[i + 1]));
                    i += 2;
                    continue;
                }
                out.push(blank_keep_nl(b[i]));
                i += 1;
            }
            if i < b.len() {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip `//` and `/* */` comments. `blank_strings` blanks string/rune/template
/// literals (Java); otherwise they are kept so Go/TS specifiers survive.
fn strip_c_comments(src: &str, blank_strings: bool) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut keep_from = 0;
    let push_gap = |out: &mut String, from: usize, to: usize| {
        if blank_strings {
            for &c in &b[from..to] {
                out.push(if c == b'\n' { '\n' } else { ' ' });
            }
        } else {
            out.push_str(&src[from..to]);
        }
    };
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            out.push_str(&src[keep_from..i]);
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            keep_from = i;
            continue;
        }
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
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
            continue;
        }
        let quote = match b[i] {
            q @ (b'"' | b'\'' | b'`') => Some(q),
            _ => None,
        };
        if let Some(q) = quote {
            if blank_strings {
                out.push_str(&src[keep_from..i]);
                let start = i;
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' && q != b'`' && i + 1 < b.len() {
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
                push_gap(&mut out, start, i);
                keep_from = i;
            } else {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' && q != b'`' && i + 1 < b.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    out.push_str(&src[keep_from.min(b.len())..]);
    out
}

fn mask_rust(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
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
                    out.push(blank_keep_nl(b[i]));
                    i += 1;
                }
            }
            continue;
        }
        if b[i] == b'r' {
            let prev_ok = i == 0
                || !is_ident_byte(b[i - 1])
                || (b[i - 1] == b'b' && (i < 2 || !is_ident_byte(b[i - 2])));
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
                out.extend(b[i..k].iter().copied().map(blank_keep_nl));
                i = k;
                continue;
            }
        }
        if b[i] == b'"' {
            out.push(b' ');
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    out.push(b' ');
                    out.push(blank_keep_nl(b[i + 1]));
                    i += 2;
                    continue;
                }
                out.push(blank_keep_nl(b[i]));
                i += 1;
            }
            if i < b.len() {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                out.push(b' ');
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    out.push(blank_keep_nl(b[i]));
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
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn python_logical_lines(masked: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth = 0isize;
    for line in masked.lines() {
        let mut t = line.trim_end();
        let cont = t.ends_with('\\');
        if cont {
            t = &t[..t.len() - 1];
        }
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(t);
        depth += t.bytes().filter(|c| *c == b'(').count() as isize
            - t.bytes().filter(|c| *c == b')').count() as isize;
        if !cont && depth <= 0 {
            let stmt = collapse_ws(buf.trim());
            if !stmt.is_empty() {
                out.push(stmt);
            }
            buf.clear();
            depth = 0;
        }
    }
    let stmt = collapse_ws(buf.trim());
    if !stmt.is_empty() {
        out.push(stmt);
    }
    out
}

fn split_top_level(value: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (i, ch) in value.char_indices() {
        match ch {
            '{' | '(' => depth += 1,
            '}' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(value[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(value[start..].trim());
    out
}

fn extract_python_specs(src: &str) -> Vec<String> {
    let masked = mask_python(src);
    let mut out = Vec::new();
    for stmt in python_logical_lines(&masked) {
        let t = stmt.trim_start();
        if let Some(rest) = t.strip_prefix("import ") {
            for part in split_top_level(rest.trim()) {
                let name = part.split(" as ").next().unwrap_or(part).trim();
                if !name.is_empty() && name != "*" {
                    out.push(name.to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("from ") {
            let Some(idx) = rest.find(" import") else {
                continue;
            };
            let module: String = rest[..idx].chars().filter(|c| !c.is_whitespace()).collect();
            if !module.is_empty() {
                out.push(module);
            }
        }
    }
    out
}

fn python_path_exists(index: &FileIndex, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    index.contains(&format!("{path}.py"))
        || index.contains(&format!("{path}.pyi"))
        || index.contains(&format!("{path}/__init__.py"))
        || index.contains(&format!("{path}/__init__.pyi"))
        || index
            .entries(path)
            .iter()
            .any(|p| matches!(p.extension(), Some("py" | "pyi")))
}

fn push_unique(roots: &mut Vec<String>, value: String) {
    if !roots.contains(&value) {
        roots.push(value);
    }
}

/// Source roots from build files, independent of [`ResolverSet`]. Always
/// includes the repo root (`""`) last, matching pytest's rootdir.
fn python_source_roots(root: &Path, index: &FileIndex) -> Vec<String> {
    let mut roots = Vec::new();
    for name in ["pyproject.toml", "setup.cfg", "setup.py"] {
        let Some(text) = read(root, &RelPath::new(name)) else {
            continue;
        };
        for line in text.lines() {
            for quoted in quoted_strings(line) {
                let path = quoted.replace('\\', "/");
                let path = path.trim_matches('/');
                if path == "src" || path.starts_with("src/") {
                    push_unique(&mut roots, "src".to_string());
                }
            }
        }
    }
    if roots.is_empty()
        && index.has_dir("src")
        && index
            .iter()
            .any(|p| matches!(p.extension(), Some("py" | "pyi")) && p.as_str().starts_with("src/"))
    {
        push_unique(&mut roots, "src".to_string());
    }
    push_unique(&mut roots, String::new());
    roots
}

fn python_target(index: &FileIndex, from: &RelPath, spec: &str, roots: &[String]) -> bool {
    let dots = spec.bytes().take_while(|b| *b == b'.').count();
    let tail = &spec[dots..];
    if dots == 0 {
        let module = tail.replace('.', "/");
        return roots.iter().any(|root| {
            let path = if root.is_empty() {
                module.clone()
            } else {
                format!("{root}/{module}")
            };
            python_path_exists(index, &path)
        });
    }
    let mut base: Vec<&str> = from.dir().split('/').filter(|s| !s.is_empty()).collect();
    for _ in 1..dots {
        if base.pop().is_none() {
            return false;
        }
    }
    let mut target = base.join("/");
    if !tail.is_empty() {
        if !target.is_empty() {
            target.push('/');
        }
        target.push_str(&tail.replace('.', "/"));
    }
    python_path_exists(index, &target)
}

fn python_imports(root: &Path, paths: &[RelPath], index: &FileIndex) -> Vec<Import> {
    let roots = python_source_roots(root, index);
    let mut out = Vec::new();
    for from in paths
        .iter()
        .filter(|p| matches!(p.extension(), Some("py" | "pyi")))
    {
        let Some(text) = read(root, from) else {
            continue;
        };
        for spec in extract_python_specs(&text) {
            if python_target(index, from, &spec, &roots) {
                out.push(Import {
                    from: from.clone(),
                    spec: spec.clone(),
                    source: spec,
                });
            }
        }
    }
    out
}

fn quoted_strings(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            let quote = bytes[i];
            let start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            if i <= bytes.len() {
                out.push(line[start..i.min(bytes.len())].to_string());
            }
        }
        i += 1;
    }
    out
}

fn go_modules(root: &Path, paths: &[RelPath]) -> Vec<(String, String)> {
    let mut modules = Vec::new();
    for path in paths.iter().filter(|p| p.file_name() == "go.mod") {
        let Some(text) = read(root, path) else {
            continue;
        };
        if let Some(name) = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("module ")
                .map(str::trim)
                .filter(|s| !s.is_empty())
        }) {
            modules.push((name.to_string(), path.dir().to_string()));
        }
    }
    modules.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    modules
}

fn go_target(index: &FileIndex, modules: &[(String, String)], spec: &str) -> bool {
    let Some((name, root)) = modules
        .iter()
        .find(|(name, _)| spec == name || spec.starts_with(&format!("{name}/")))
    else {
        return false;
    };
    let suffix = spec
        .strip_prefix(name)
        .unwrap_or("")
        .trim_start_matches('/');
    let dir = [root.as_str(), suffix]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    index
        .entries(&dir)
        .iter()
        .any(|p| p.extension() == Some("go") && !p.file_name().ends_with("_test.go"))
}

fn extract_go_specs(src: &str) -> Vec<String> {
    let clean = strip_c_comments(src, false);
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in clean.lines() {
        let line = raw.trim();
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else if let Some(p) = quoted_strings(line).into_iter().find(|s| !s.is_empty()) {
                out.push(p);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("import") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '(' || c == '"' || c == '`') {
            continue;
        }
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('(') {
            if let Some(p) = quoted_strings(inner).into_iter().find(|s| !s.is_empty()) {
                out.push(p);
            }
            in_block = !inner.contains(')');
        } else if let Some(p) = quoted_strings(rest).into_iter().find(|s| !s.is_empty()) {
            out.push(p);
        }
    }
    out
}

fn go_imports(root: &Path, paths: &[RelPath], index: &FileIndex) -> Vec<Import> {
    let modules = go_modules(root, paths);
    let mut out = Vec::new();
    for from in paths.iter().filter(|p| p.extension() == Some("go")) {
        let Some(text) = read(root, from) else {
            continue;
        };
        for spec in extract_go_specs(&text) {
            if go_target(index, &modules, &spec) {
                out.push(Import {
                    from: from.clone(),
                    spec: spec.clone(),
                    source: spec,
                });
            }
        }
    }
    out
}

fn java_classes(paths: &[RelPath]) -> BTreeMap<String, RelPath> {
    let mut classes = BTreeMap::new();
    for path in paths.iter().filter(|p| p.extension() == Some("java")) {
        let raw = path.as_str();
        let relative = raw.rfind("/java/").map(|i| &raw[i + 6..]).unwrap_or(raw);
        if let Some(class) = relative.strip_suffix(".java") {
            classes.insert(class.replace('/', "."), path.clone());
        }
    }
    classes
}

fn extract_java_specs(src: &str) -> Vec<String> {
    let masked = strip_c_comments(src, true);
    let mut out = Vec::new();
    for line in masked.lines() {
        let trimmed = line.trim_start();
        let Some(body) = trimmed.strip_prefix("import ") else {
            continue;
        };
        if body.trim_start().starts_with("static ") {
            continue;
        }
        let Some(spec) = body.trim().strip_suffix(';').map(str::trim) else {
            continue;
        };
        if spec.ends_with(".*") {
            continue;
        }
        if spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.'))
        {
            out.push(spec.to_string());
        }
    }
    out
}

fn java_imports(root: &Path, paths: &[RelPath]) -> Vec<Import> {
    let classes = java_classes(paths);
    let convention_roots: Vec<&str> = paths
        .iter()
        .filter_map(|path| {
            let raw = path.as_str();
            raw.find("/src/main/java/")
                .map(|i| &raw[..i + 14])
                .or_else(|| raw.find("/src/test/java/").map(|i| &raw[..i + 14]))
                .or_else(|| raw.strip_prefix("src/main/java/").map(|_| "src/main/java"))
                .or_else(|| raw.strip_prefix("src/test/java/").map(|_| "src/test/java"))
        })
        .collect();
    let mut out = Vec::new();
    for from in paths.iter().filter(|p| p.extension() == Some("java")) {
        let Some(text) = read(root, from) else {
            continue;
        };
        for spec in extract_java_specs(&text) {
            if let Some(target) = classes.get(&spec) {
                let in_convention_root = convention_roots
                    .iter()
                    .any(|root| target.as_str().starts_with(&format!("{root}/")));
                if !in_convention_root {
                    continue;
                }
                out.push(Import {
                    from: from.clone(),
                    spec: spec.clone(),
                    source: spec,
                });
            }
        }
    }
    out
}

fn expand_rust_use(value: &str, prefix: &str, out: &mut Vec<String>) {
    for part in split_top_level(value) {
        let part = part.trim().trim_start_matches("::");
        if part.is_empty() {
            continue;
        }
        if let Some(open) = part.find('{') {
            let close = part[open + 1..]
                .rfind('}')
                .map(|offset| open + 1 + offset)
                .unwrap_or(part.len());
            let head = part[..open].trim().trim_end_matches("::");
            let next = [prefix, head]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("::");
            expand_rust_use(&part[open + 1..close], &next, out);
        } else {
            let leaf = part.split(" as ").next().unwrap_or(part).trim();
            if leaf == "self" {
                if !prefix.is_empty() {
                    out.push(prefix.to_string());
                }
            } else if leaf != "*" {
                out.push(
                    [prefix, leaf]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("::"),
                );
            }
        }
    }
}

fn cargo_crates(root: &Path, paths: &[RelPath]) -> Vec<(String, String)> {
    let mut crates = Vec::new();
    for manifest in paths.iter().filter(|p| p.file_name() == "Cargo.toml") {
        let Some(text) = read(root, manifest) else {
            continue;
        };
        let mut package = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                package = line == "[package]";
            } else if package && line.starts_with("name") {
                if let Some(name) = quoted_strings(line).into_iter().next() {
                    crates.push((name.replace('-', "_"), manifest.dir().to_string()));
                    break;
                }
            }
        }
    }
    crates
}

fn rust_in_repo(crates: &[(String, String)], spec: &str) -> bool {
    let first = spec.split("::").find(|s| !s.is_empty()).unwrap_or("");
    matches!(first, "crate" | "self" | "super") || crates.iter().any(|(name, _)| name == first)
}

fn extract_rust_use_bodies(src: &str) -> Vec<String> {
    let masked = mask_rust(src);
    let b = masked.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < b.len() {
        let at_kw = &b[i..i + 3] == b"use"
            && (i == 0 || !is_ident_byte(b[i - 1]))
            && b[i + 3].is_ascii_whitespace();
        if !at_kw {
            i += 1;
            continue;
        }
        let Some(semi) = masked[i..].find(';') else {
            break;
        };
        let body = masked[i + 3..i + semi].trim().trim_end_matches(';').trim();
        if !body.is_empty() {
            out.push(collapse_ws(body));
        }
        i += semi + 1;
    }
    out
}

fn rust_imports(root: &Path, paths: &[RelPath]) -> Vec<Import> {
    let crates = cargo_crates(root, paths);
    let mut out = Vec::new();
    for from in paths.iter().filter(|p| p.extension() == Some("rs")) {
        let Some(text) = read(root, from) else {
            continue;
        };
        for body in extract_rust_use_bodies(&text) {
            let mut specs = Vec::new();
            expand_rust_use(&body, "", &mut specs);
            if specs.iter().any(|spec| rust_in_repo(&crates, spec)) {
                out.push(Import {
                    from: from.clone(),
                    spec: body.clone(),
                    source: body,
                });
            }
        }
    }
    out
}

fn ts_target(index: &FileIndex, from: &RelPath, spec: &str) -> bool {
    if !spec.starts_with('.') {
        return false;
    }
    let spec = spec.split(['?', '#']).next().unwrap_or(spec);
    let Some(joined) = join_rel(from.dir(), spec) else {
        return false;
    };
    const EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
    let stem = EXTENSIONS
        .iter()
        .find_map(|ext| joined.strip_suffix(&format!(".{ext}")))
        .unwrap_or(joined.as_str());
    let stem = stem.strip_suffix(".d").unwrap_or(stem);
    let dot = format!("{stem}.");
    let slash = format!("{stem}/");
    index.contains(&joined)
        || index.contains(stem)
        || EXTENSIONS
            .iter()
            .any(|ext| index.contains(&format!("{stem}.{ext}")))
        || EXTENSIONS
            .iter()
            .any(|ext| index.contains(&format!("{stem}/index.{ext}")))
        || index.iter().any(|f| {
            let s = f.as_str();
            s == joined || s == stem || s.starts_with(&dot) || s.starts_with(&slash)
        })
}

fn quoted_prefix(s: &str) -> Option<String> {
    let mut chars = s.chars();
    let q = chars.next()?;
    if q != '\'' && q != '"' {
        return None;
    }
    let body: String = chars.collect();
    let end = body.find(q)?;
    Some(body[..end].to_string())
}

fn extract_ts_specs(src: &str) -> Vec<String> {
    let clean = strip_c_comments(src, false);
    let mut out = Vec::new();
    let mut statement = String::new();
    for line in clean.lines() {
        let trimmed = line.trim();
        if statement.is_empty()
            && (trimmed.starts_with("import ") || trimmed.starts_with("export "))
        {
            statement.push_str(trimmed);
            let side_effect = trimmed.starts_with("import ")
                && !trimmed.contains('{')
                && !trimmed.contains(" from ");
            if !trimmed.contains(" from ") && !side_effect {
                continue;
            }
        } else if !statement.is_empty() {
            statement.push(' ');
            statement.push_str(trimmed);
            if !trimmed.contains(" from ") && !trimmed.contains(';') {
                continue;
            }
        }
        let candidate = if statement.is_empty() {
            trimmed
        } else {
            statement.as_str()
        };
        let mut specs = Vec::new();
        if candidate.starts_with("import ") || candidate.starts_with("export ") {
            if let Some(pos) = candidate.find(" from ") {
                specs.extend(quoted_strings(&candidate[pos + 6..]).into_iter().take(1));
            } else if candidate.starts_with("import ") {
                specs.extend(quoted_strings(candidate).into_iter().take(1));
            }
        }
        let mut rest = candidate;
        while let Some(pos) = rest.find("require(") {
            rest = &rest[pos + 8..];
            if let Some(spec) =
                quoted_prefix(rest.trim_start()).or_else(|| quoted_strings(rest).into_iter().next())
            {
                specs.push(spec);
            }
        }
        rest = candidate;
        while let Some(pos) = rest.find("import(") {
            rest = &rest[pos + 7..];
            if let Some(spec) =
                quoted_prefix(rest.trim_start()).or_else(|| quoted_strings(rest).into_iter().next())
            {
                specs.push(spec);
            }
        }
        out.extend(specs);
        statement.clear();
    }
    out
}

fn ts_imports(root: &Path, paths: &[RelPath], index: &FileIndex) -> Vec<Import> {
    let mut out = Vec::new();
    for from in paths.iter().filter(|p| {
        matches!(
            Language::from_path(p),
            Some(Language::TypeScript | Language::Tsx | Language::JavaScript)
        )
    }) {
        let Some(text) = read(root, from) else {
            continue;
        };
        for spec in extract_ts_specs(&text) {
            if ts_target(index, from, &spec) {
                out.push(Import {
                    from: from.clone(),
                    spec: spec.clone(),
                    source: spec,
                });
            }
        }
    }
    out
}

fn dart_pubspec_names(root: &Path, paths: &[RelPath]) -> Vec<String> {
    let mut names = Vec::new();
    for path in paths.iter().filter(|p| p.file_name() == "pubspec.yaml") {
        let Some(text) = read(root, path) else {
            continue;
        };
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("name:") {
                let name = rest
                    .split('#')
                    .next()
                    .unwrap_or(rest)
                    .trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .trim();
                if !name.is_empty() && !name.contains(char::is_whitespace) {
                    names.push(name.to_string());
                    break;
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn dart_target(pubspecs: &[String], spec: &str) -> bool {
    let stripped = spec.strip_prefix("package:").unwrap_or(spec);
    pubspecs
        .iter()
        .any(|name| stripped == name || stripped.starts_with(&format!("{name}/")))
}

fn extract_dart_specs(src: &str) -> Vec<String> {
    let clean = strip_c_comments(src, false);
    let mut out = Vec::new();
    for raw in clean.lines() {
        let line = raw.trim();
        let is_import = line.starts_with("import ") || line.starts_with("import\t");
        let is_export = line.starts_with("export ") || line.starts_with("export\t");
        let is_part = line.starts_with("part ") || line.starts_with("part\t");
        if !is_import && !is_export && !is_part {
            continue;
        }
        for s in quoted_strings(line) {
            if !s.is_empty() {
                out.push(s);
                break;
            }
        }
    }
    out
}

fn dart_imports(root: &Path, paths: &[RelPath]) -> Vec<Import> {
    let pubspecs = dart_pubspec_names(root, paths);
    let mut out = Vec::new();
    for from in paths.iter().filter(|p| p.extension() == Some("dart")) {
        let Some(text) = read(root, from) else {
            continue;
        };
        for spec in extract_dart_specs(&text) {
            if dart_target(&pubspecs, &spec) {
                out.push(Import {
                    from: from.clone(),
                    spec: spec.clone(),
                    source: spec,
                });
            }
        }
    }
    out
}

/// Dart 语料的"预期未解析"豁免清单：(from, spec) 对。
///
/// 实际未解析集合必须恰好等于本清单——多出的条目是真回归；缺失的条目
/// 说明豁免过时，应删除。豁免条目不计入解析率分母，threshold 仍保持
/// 1.0，对所有应解析的 import 是硬门槛。
///
/// 注: google_maps_flutter_ios_shared_code/example（上游 README 声明 "is not
/// a package"，无 pubspec.yaml）下的两条 google_maps_flutter_example import
/// 不在清单中——就近路由会命中同名 example 包（android/ios/sdk9/sdk10 四个
/// 副本均带 example_google_map.dart），已能解析。
const DART_UNRESOLVED_EXEMPTIONS: &[(&str, &str)] = &[
    // skill eval 脚本运行期动态生成的模拟代码: 目标 dummy_eval_feature.dart
    // 磁盘上不存在，但 ground truth 按 pubspec 名 camera_android_camerax
    // 判为仓库内 import。
    (
        "packages/camera/camera_android_camerax/.agents/skills/pre-push-skill/evals/test_data/setup_mixed_missing_native_test.dart",
        "package:camera_android_camerax/src/dummy_eval_feature.dart",
    ),
];

fn ground_truth(corpus: &Corpus, paths: &[RelPath], index: &FileIndex) -> Vec<Import> {
    match corpus.kind {
        Kind::Python => python_imports(&corpus.root, paths, index),
        Kind::Go => go_imports(&corpus.root, paths, index),
        Kind::Java => java_imports(&corpus.root, paths),
        Kind::Rust => rust_imports(&corpus.root, paths),
        Kind::TypeScript => ts_imports(&corpus.root, paths, index),
        Kind::Dart => dart_imports(&corpus.root, paths),
    }
}

fn run(kind: Kind) {
    run_with_exemptions(kind, &[]);
}

/// Shared acceptance loop. `exemptions` lists `(from, spec)` pairs expected to
/// stay unresolved because of corpus quirks rather than resolver bugs; the
/// actual unresolved set must equal it exactly — extra entries are a real
/// regression, missing entries mean an exemption went stale and must be
/// deleted. Exempted imports leave the rate denominator so the threshold
/// stays a hard 100% gate over everything that must resolve.
fn run_with_exemptions(kind: Kind, exemptions: &[(&str, &str)]) {
    let corpus = corpus(kind);
    let (index, paths) = scan(&corpus.root);
    let imports = ground_truth(&corpus, &paths, &index);
    assert!(
        !imports.is_empty(),
        "{} ground truth 为空；请检查语料路径和提取规则",
        corpus.language
    );

    let resolver_set = catch_unwind(AssertUnwindSafe(|| ResolverSet::detect(&index))).ok();
    let mut resolved = 0usize;
    let mut exempt = 0usize;
    let mut unresolved = Vec::new();
    let mut actual: BTreeSet<(&str, &str)> = BTreeSet::new();
    for import in &imports {
        let target = resolver_set.as_ref().and_then(|set| {
            catch_unwind(AssertUnwindSafe(|| {
                set.resolve(&import.from, &import.spec, &index)
            }))
            .ok()
            .flatten()
        });
        if target
            .as_ref()
            .is_some_and(|path| index.contains(path.as_str()))
        {
            resolved += 1;
        } else {
            actual.insert((import.from.as_str(), import.spec.as_str()));
            if exemptions.contains(&(import.from.as_str(), import.spec.as_str())) {
                exempt += 1;
            } else if unresolved.len() < 20 {
                unresolved.push(format!(
                    "{} => {} ({})",
                    import.from, import.spec, import.source
                ));
            }
        }
    }

    let expected: BTreeSet<(&str, &str)> = exemptions.iter().copied().collect();
    let stale: Vec<_> = expected.difference(&actual).collect();
    assert!(
        stale.is_empty(),
        "{} 豁免清单过时（以下条目已能解析，应从清单删除）: {:?}",
        corpus.language,
        stale
    );

    let total = imports.len();
    let rate = resolved as f64 / (total - exempt).max(1) as f64;
    println!(
        "ASTROLABE_RESULT|{}|{}|{}|{}|{:.2}|{}",
        corpus.language,
        corpus.name,
        total,
        resolved,
        rate * 100.0,
        if rate + f64::EPSILON >= corpus.threshold {
            "PASS"
        } else {
            "FAIL"
        }
    );
    assert!(
        rate + f64::EPSILON >= corpus.threshold,
        "{} 解析率 {:.2}% 低于阈值 {:.0}%，未解析样例: {:?}",
        corpus.language,
        rate * 100.0,
        corpus.threshold * 100.0,
        unresolved
    );
}

#[test]
fn python_extractor_skips_comments_strings_and_splits_import_list() {
    let specs = extract_python_specs(
        r#"
# import decoy
x = "import decoy"
from decoy import x
from real.mod import (
    a,
    b,
)
import first, second as s
from . import local
"#,
    );
    assert_eq!(specs, vec!["decoy", "real.mod", "first", "second", ".",]);
}

#[test]
fn python_namespace_package_counts_as_in_repo() {
    let index = FileIndex::new(
        "/nonexistent",
        [
            RelPath::new("src/pkg/__init__.py"),
            RelPath::new("src/pkg/ns/foo.py"),
        ],
    );
    let from = RelPath::new("src/pkg/a.py");
    let roots = vec!["src".to_string(), String::new()];
    assert!(python_target(&index, &from, "pkg.ns", &roots));
    assert!(python_target(&index, &from, ".ns", &roots));
    assert!(!python_target(&index, &from, "missing", &roots));
}

#[test]
fn go_extractor_skips_comment_block_import() {
    let specs = extract_go_specs(
        r#"
/*
	import "github.com/gin-gonic/gin"
*/
package gin // import "github.com/commented/out"
import "fmt"
import (
	"net/http"
)
func f() string { return "import \"not/an/import\"" }
"#,
    );
    assert_eq!(specs, vec!["fmt", "net/http"]);
}

#[test]
fn java_extractor_skips_comment_and_string_imports() {
    let specs = extract_java_specs(
        r#"
// import com.fake.Skip;
/*
import com.fake.Block;
*/
class T {
  String s = "import com.fake.StringLit;";
  // real:
}
import com.google.gson.Gson;
import static com.google.gson.Gson.fromJson;
import com.google.gson.*;
"#,
    );
    assert_eq!(specs, vec!["com.google.gson.Gson"]);
}

#[test]
fn rust_extractor_skips_comment_use_and_keeps_groups() {
    let bodies = extract_rust_use_bodies(
        r#"
// use fake::Skip;
use grep::{searcher, matcher};
pub use crate::foo::Bar;
fn f() { let _ = "use fake::Skip;"; }
"#,
    );
    assert_eq!(bodies, vec!["grep::{searcher, matcher}", "crate::foo::Bar"]);
    let mut specs = Vec::new();
    expand_rust_use("grep::{searcher, matcher}", "", &mut specs);
    assert_eq!(specs, vec!["grep::searcher", "grep::matcher"]);
}

#[test]
fn ts_extractor_skips_comment_import() {
    let specs = extract_ts_specs(
        r#"
// import "./skip";
/* import { x } from "./block"; */
import { a } from "./real";
require("./cjs");
"#,
    );
    assert_eq!(specs, vec!["./real", "./cjs"]);
}

#[test]
fn dart_extractor_skips_comments_and_extracts_imports() {
    let specs = extract_dart_specs(
        r#"
// import 'package:decoy/decoy.dart';
/*
import 'package:decoy2/decoy2.dart';
*/
import 'package:flutter/material.dart';
import 'package:my_app/app.dart' as app;
export 'package:my_app/src/utils.dart' show helper;
part 'app.g.dart';
"#,
    );
    assert_eq!(
        specs,
        vec![
            "package:flutter/material.dart",
            "package:my_app/app.dart",
            "package:my_app/src/utils.dart",
            "app.g.dart",
        ]
    );
}

#[test]
#[ignore = "真实语料验收；使用 cargo test -- --ignored 运行"]
fn python_import_resolution() {
    run(Kind::Python);
}

#[test]
#[ignore = "真实语料验收；使用 cargo test -- --ignored 运行"]
fn go_import_resolution() {
    run(Kind::Go);
}

#[test]
#[ignore = "真实语料验收；使用 cargo test -- --ignored 运行"]
fn java_import_resolution() {
    run(Kind::Java);
}

#[test]
#[ignore = "真实语料验收；使用 cargo test -- --ignored 运行"]
fn rust_import_resolution() {
    run(Kind::Rust);
}

#[test]
#[ignore = "真实语料验收；使用 cargo test -- --ignored 运行"]
fn typescript_import_resolution() {
    run(Kind::TypeScript);
}

#[test]
#[ignore = "真实语料验收；使用 cargo test -- --ignored 运行"]
fn dart_import_resolution() {
    run_with_exemptions(Kind::Dart, DART_UNRESOLVED_EXEMPTIONS);
}
