//! Python module resolution.
//!
//! Target: 100% of the 1577 in-repo imports in the `serena` corpus.
//! Prior art resolved 12%, because it only ever tried the repo root while
//! `serena` is a src-layout project (`from solidlsp.ls import X` lives at
//! `src/solidlsp/ls.py`).
//!
//! Source roots come from build config, in priority order:
//!   * `pyproject.toml` — `[tool.poetry] packages`, `[tool.hatch...] packages`,
//!     `[tool.setuptools] package-dir`, `[tool.setuptools.packages.find] where`
//!   * `setup.py` / `setup.cfg` — `package_dir={"": "src"}`
//!   * fallback: probe for `src/*/__init__.py`, else repo root
//!
//! Must handle: absolute imports, `from . import x`, `from ..pkg import y`
//! (leading-dot count sets how far up), package `__init__.py`, `.pyi` stubs,
//! and namespace packages (directory with no `__init__.py`).
//!
//! Returning `None` for `import os` / `from pydantic import X` is correct —
//! they are not in the repo.
//!
//! # Decisions worth knowing about
//!
//! * **The repo root is always the last source root.** Even in a src-layout
//!   project, `test/` and `scripts/` are imported as `from test.conftest
//!   import ...` — pytest puts the rootdir on `sys.path`. Without `""` as a
//!   fallback root, roughly 190 of serena's 1577 in-repo imports would be lost.
//! * **Lookup order per candidate directory follows `importlib`'s
//!   `FileFinder`:** a regular package (`x/__init__.py`, `x/__init__.pyi`)
//!   beats a module (`x.py`, then `x.pyi`), and a namespace package (a bare
//!   directory) is the last resort. `.py` is preferred over `.pyi` because the
//!   implementation is the more useful graph target.
//! * **Namespace packages resolve to the lexicographically-first Python file
//!   directly inside the directory.** The contract returns a single file, so
//!   we pick a deterministic one that lands the edge in the right subtree.
//!   A directory with no Python files directly inside it (a data dir that
//!   happens to share a name with a module) yields `None`. The repo root is
//!   never used as a namespace package.
//! * **Absolute imports that miss every source root fall back to nested
//!   project roots**: the importing file's ancestor directories, innermost
//!   first, but a directory only counts if it is not itself a package and the
//!   import's top-level segment is a regular package (`x/__init__.py`)
//!   directly inside it. This resolves imports inside nested fixture projects
//!   (`test/resources/repos/python/test_repo`) without polluting the global
//!   root list, and — measured on serena — refuses the trap of mapping
//!   `import logging` in `serena/util/dotnet.py` to the sibling
//!   `serena/util/logging.py` (Python 3 has no implicit relative imports).
//! * **`ProjectMeta::modules` carries top-level packages** (`serena` →
//!   `src/serena`), declared in config or probed under each root. They are
//!   consulted first via longest-prefix match, which also covers non-standard
//!   `package_dir = {"mypkg": "lib"}` mappings that no plain root can express.

use crate::types::{
    join_rel, FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath,
};

pub struct PythonResolver;

impl ModuleResolver for PythonResolver {
    fn language(&self) -> Language {
        Language::Python
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut det = Detected::default();

        if files.contains("pyproject.toml") {
            if let Some(text) = files.read("pyproject.toml") {
                roots_from_pyproject(&text, &mut det);
            }
        }
        if files.contains("setup.cfg") {
            if let Some(text) = files.read("setup.cfg") {
                roots_from_setup_cfg(&text, &mut det);
            }
        }
        if files.contains("setup.py") {
            if let Some(text) = files.read("setup.py") {
                roots_from_setup_py(&text, &mut det);
            }
        }
        if det.roots.is_empty() {
            probe_layout(files, &mut det);
        }
        // Monorepo members: every nested project file is detected on its own,
        // with its directory as the base (uv/poetry/pip -e workspaces).
        detect_subprojects(files, &mut det);
        // The repo root is always the last resort: pytest rootdir, `python -m`
        // from the checkout, `scripts/`, `test/`.
        det.add_root(String::new());

        // Top-level packages under every root, so `modules` is useful even
        // when config declared nothing.
        for p in files.iter() {
            if p.file_name() != "__init__.py" {
                continue;
            }
            let dir = p.dir();
            let Some(parent) = parent_dir(dir) else {
                continue;
            };
            if det.roots.iter().any(|r| r == parent) {
                let name = dir.rsplit('/').next().unwrap_or(dir);
                if is_ident(name) {
                    det.add_module(name.to_string(), dir.to_string());
                }
            }
        }
        add_unique_toplevel_packages(files, &mut det);

        ProjectMeta {
            source_roots: det.roots,
            modules: det.modules,
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
        // `from . import m` inside `pkg/__init__.py` names the package itself;
        // a file importing itself is not a dependency.
        resolve_inner(from, spec, files, meta).filter(|p| p != from)
    }
}

fn resolve_inner(
    from: &RelPath,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    {
        // Parsers may hand us `from . foo import x` verbatim; whitespace is
        // never significant inside a module path.
        let cleaned: String;
        let spec = if spec.bytes().any(|b| b.is_ascii_whitespace()) {
            cleaned = spec.chars().filter(|c| !c.is_whitespace()).collect();
            cleaned.as_str()
        } else {
            spec.trim()
        };
        if spec.is_empty() {
            return None;
        }

        // ---- relative import: N leading dots = N-1 levels above the file's dir
        let dots = spec.bytes().take_while(|b| *b == b'.').count();
        if dots > 0 {
            let rest = &spec[dots..];
            let mut base = from.dir();
            for _ in 1..dots {
                // Escaping the repo root is a broken import, not a hit.
                base = parent_dir(base)?;
            }
            let rel = rest.replace('.', "/");
            return resolve_in(files, base, &rel);
        }

        let rel = spec.replace('.', "/");

        // ---- 0. a source root of the project the importer lives in wins over
        //         a same-named package in another monorepo member.
        let mut own: Option<&String> = None;
        let fd = from.dir();
        for r in &meta.source_roots {
            if !r.is_empty()
                && fd.starts_with(r.as_str())
                && fd.as_bytes().get(r.len()) == Some(&b'/')
                && own.is_none_or(|o| r.len() > o.len())
            {
                own = Some(r);
            }
        }
        if let Some(root) = own {
            if let Some(p) = resolve_in(files, root, &rel) {
                return Some(p);
            }
        }

        // ---- 1. declared units, longest prefix first
        if let Some(unit) = meta.module_for(&rel) {
            let remainder = rel[unit.name.len()..].trim_start_matches('/');
            if let Some(p) = resolve_in(files, &unit.dir, remainder) {
                return Some(p);
            }
        }

        // ---- 2. source roots in priority order
        for root in &meta.source_roots {
            if let Some(p) = resolve_in(files, root, &rel) {
                return Some(p);
            }
        }

        // ---- 3. nested project roots. Walk the importing file's ancestors,
        //         innermost first, and accept a directory only if (a) it is not
        //         itself a package and (b) the import's top-level segment is a
        //         *regular package* directly inside it. Bare sibling modules
        //         are deliberately not matched: `import logging` next to a
        //         `logging.py` is the stdlib, not the sibling (PEP 328).
        let first = rel.split('/').next().unwrap_or("");
        let mut dir = from.dir();
        loop {
            let already_root = meta.source_roots.iter().any(|r| r == dir);
            if !already_root && regular_package(files, dir).is_none() {
                let top = if dir.is_empty() {
                    first.to_string()
                } else {
                    format!("{dir}/{first}")
                };
                if regular_package(files, &top).is_some() {
                    if let Some(p) = resolve_in(files, dir, &rel) {
                        return Some(p);
                    }
                }
            }
            match parent_dir(dir) {
                Some(p) => dir = p,
                None => break,
            }
        }

        // ---- 4. a sibling module next to the importing file: pytest's
        //         rootdir-less `tests/`, `python script.py` (sys.path[0]).
        //         Never for stdlib names, so `import logging` beside a
        //         `logging.py` stays the standard library, and never inside a regular
        //         package (Python 3 has no implicit relative imports there).
        if !is_stdlib_module(first) && regular_package(files, from.dir()).is_none() {
            if let Some(p) = resolve_in(files, from.dir(), &rel) {
                return Some(p);
            }
        }
        None
    }
}

// ------------------------------------------------------------------ resolve

/// Resolve a slash-separated module path `rel` beneath `base`. Empty `rel`
/// means "the package that `base` itself is" (`from . import x`).
fn resolve_in(files: &FileIndex, base: &str, rel: &str) -> Option<RelPath> {
    if rel.is_empty() {
        return package_file(files, base);
    }
    let target = join_rel(base, rel)?;
    if target.is_empty() {
        return None;
    }
    // Regular package first (importlib checks the directory before the
    // module file), then module, then stub, then namespace package.
    if let Some(p) = regular_package(files, &target) {
        return Some(p);
    }
    for ext in [".py", ".pyi"] {
        let candidate = format!("{target}{ext}");
        if files.contains(&candidate) {
            return Some(RelPath::new(candidate));
        }
    }
    if files.has_dir(&target) {
        return namespace_pick(files, &target);
    }
    None
}

/// `dir` addressed as a package: `__init__.py`, `__init__.pyi`, else namespace.
fn package_file(files: &FileIndex, dir: &str) -> Option<RelPath> {
    if let Some(p) = regular_package(files, dir) {
        return Some(p);
    }
    if files.has_dir(dir) {
        return namespace_pick(files, dir);
    }
    None
}

fn regular_package(files: &FileIndex, dir: &str) -> Option<RelPath> {
    for init in ["__init__.py", "__init__.pyi"] {
        let candidate = if dir.is_empty() {
            init.to_string()
        } else {
            format!("{dir}/{init}")
        };
        if files.contains(&candidate) {
            return Some(RelPath::new(candidate));
        }
    }
    None
}

/// Namespace package (PEP 420): a directory without `__init__.py`. We return
/// the lexicographically-first Python file directly inside it so the edge
/// lands in the right subtree. The repo root is never a namespace package.
fn namespace_pick(files: &FileIndex, dir: &str) -> Option<RelPath> {
    if dir.is_empty() {
        return None;
    }
    files
        .entries(dir)
        .iter()
        .filter(|p| matches!(p.extension(), Some("py") | Some("pyi")))
        .min()
        .cloned()
}

/// Parent of a repo-relative directory; `None` for the root.
fn parent_dir(dir: &str) -> Option<&str> {
    if dir.is_empty() {
        return None;
    }
    Some(match dir.rfind('/') {
        Some(i) => &dir[..i],
        None => "",
    })
}

// ------------------------------------------------------------------- detect

/// Detect every non-root `pyproject.toml` / `setup.cfg` / `setup.py` as its
/// own project: its roots and packages are prefixed with its directory.
fn detect_subprojects(files: &FileIndex, det: &mut Detected) {
    let mut dirs: Vec<String> = Vec::new();
    for p in files.iter() {
        if !matches!(p.file_name(), "pyproject.toml" | "setup.cfg" | "setup.py") {
            continue;
        }
        let dir = p.dir();
        if dir.is_empty() || dir.split('/').any(is_vendored_dir) {
            continue;
        }
        if !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_string());
        }
    }
    dirs.sort();
    for base in dirs {
        let mut sub = Detected::default();
        for (name, f) in [
            (
                "pyproject.toml",
                roots_from_pyproject as fn(&str, &mut Detected),
            ),
            ("setup.cfg", roots_from_setup_cfg),
            ("setup.py", roots_from_setup_py),
        ] {
            let path = format!("{base}/{name}");
            if files.contains(&path) {
                if let Some(text) = files.read(&path) {
                    f(&text, &mut sub);
                }
            }
        }
        if sub.roots.is_empty() && files.has_dir(&format!("{base}/src")) {
            let src_layout = files.iter().any(|p| {
                p.file_name() == "__init__.py"
                    && parent_dir(p.dir()) == Some(&format!("{base}/src"))
            });
            if src_layout {
                sub.add_root("src");
            }
        }
        // A project's own directory is a root too (flat layout).
        sub.add_root("");
        for r in &sub.roots {
            det.add_root(join_dir(&base, r));
        }
        for m in &sub.modules {
            det.add_module(m.name.clone(), join_dir(&base, &m.dir));
        }
    }
}

fn join_dir(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

fn is_vendored_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | "site-packages" | ".venv" | "venv" | ".tox" | "build" | "dist"
    )
}

/// Global fallback for monorepo packages no config declared: a top-level
/// package (a dir with `__init__.py` whose parent has none) whose name is
/// unique in the repo. Ambiguous names and stdlib names are skipped.
fn add_unique_toplevel_packages(files: &FileIndex, det: &mut Detected) {
    use std::collections::BTreeMap;
    let mut tops: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in files.iter() {
        if p.file_name() != "__init__.py" {
            continue;
        }
        let dir = p.dir();
        if dir.is_empty() || dir.split('/').any(is_vendored_dir) {
            continue;
        }
        let parent = parent_dir(dir).unwrap_or("");
        if !parent.is_empty() && regular_package(files, parent).is_some() {
            continue;
        }
        let name = dir.rsplit('/').next().unwrap_or(dir);
        if is_ident(name) && !is_stdlib_module(name) {
            tops.entry(name.to_string())
                .or_default()
                .push(dir.to_string());
        }
    }
    for (name, dirs) in tops {
        if dirs.len() == 1 {
            det.add_module(name, dirs[0].clone());
        }
    }
}

/// Top-level standard-library module names (CPython 3.x, plus `typing_extensions`
/// -free). Used to keep `import logging` from binding to a sibling file.
fn is_stdlib_module(name: &str) -> bool {
    const STD: &[&str] = &[
        "__future__",
        "_thread",
        "abc",
        "argparse",
        "array",
        "ast",
        "asyncio",
        "atexit",
        "base64",
        "bdb",
        "binascii",
        "bisect",
        "builtins",
        "bz2",
        "calendar",
        "cmath",
        "cmd",
        "code",
        "codecs",
        "collections",
        "colorsys",
        "compileall",
        "concurrent",
        "configparser",
        "contextlib",
        "contextvars",
        "copy",
        "copyreg",
        "cProfile",
        "csv",
        "ctypes",
        "curses",
        "dataclasses",
        "datetime",
        "dbm",
        "decimal",
        "difflib",
        "dis",
        "doctest",
        "email",
        "encodings",
        "enum",
        "errno",
        "faulthandler",
        "fcntl",
        "filecmp",
        "fileinput",
        "fnmatch",
        "fractions",
        "ftplib",
        "functools",
        "gc",
        "getopt",
        "getpass",
        "gettext",
        "glob",
        "graphlib",
        "grp",
        "gzip",
        "hashlib",
        "heapq",
        "hmac",
        "html",
        "http",
        "imaplib",
        "importlib",
        "inspect",
        "io",
        "ipaddress",
        "itertools",
        "json",
        "keyword",
        "linecache",
        "locale",
        "logging",
        "lzma",
        "mailbox",
        "marshal",
        "math",
        "mimetypes",
        "mmap",
        "multiprocessing",
        "netrc",
        "numbers",
        "operator",
        "optparse",
        "os",
        "pathlib",
        "pdb",
        "pickle",
        "pkgutil",
        "platform",
        "plistlib",
        "poplib",
        "posix",
        "posixpath",
        "pprint",
        "profile",
        "pstats",
        "pty",
        "pwd",
        "py_compile",
        "queue",
        "quopri",
        "random",
        "re",
        "readline",
        "reprlib",
        "resource",
        "runpy",
        "sched",
        "secrets",
        "select",
        "selectors",
        "shelve",
        "shlex",
        "shutil",
        "signal",
        "site",
        "smtplib",
        "socket",
        "socketserver",
        "sqlite3",
        "ssl",
        "stat",
        "statistics",
        "string",
        "stringprep",
        "struct",
        "subprocess",
        "symtable",
        "sys",
        "sysconfig",
        "syslog",
        "tarfile",
        "telnetlib",
        "tempfile",
        "termios",
        "textwrap",
        "threading",
        "time",
        "timeit",
        "tkinter",
        "token",
        "tokenize",
        "tomllib",
        "trace",
        "traceback",
        "tracemalloc",
        "tty",
        "turtle",
        "types",
        "typing",
        "unicodedata",
        "unittest",
        "urllib",
        "uuid",
        "venv",
        "warnings",
        "wave",
        "weakref",
        "webbrowser",
        "winreg",
        "wsgiref",
        "xml",
        "xmlrpc",
        "zipapp",
        "zipfile",
        "zipimport",
        "zlib",
        "zoneinfo",
    ];
    STD.contains(&name)
}

#[derive(Default)]
struct Detected {
    roots: Vec<String>,
    modules: Vec<ModuleUnit>,
}

impl Detected {
    fn add_root(&mut self, dir: impl AsRef<str>) {
        let dir = norm_dir(dir.as_ref());
        if !self.roots.contains(&dir) {
            self.roots.push(dir);
        }
    }

    fn add_module(&mut self, name: String, dir: String) {
        let dir = norm_dir(&dir);
        if name.is_empty() || dir.is_empty() {
            return;
        }
        if !self.modules.iter().any(|m| m.name == name) {
            self.modules.push(ModuleUnit { name, dir });
        }
    }

    /// A package declared by its directory path (`src/serena`): the parent is
    /// a root and the last segment is the package name.
    fn add_package_path(&mut self, path: &str) {
        let path = norm_dir(path);
        if path.is_empty() || path.contains(['*', '?', '[']) {
            return;
        }
        if let Some(stem) = path
            .strip_suffix(".py")
            .or_else(|| path.strip_suffix(".pyi"))
        {
            // Single-file top-level module: its directory is a root.
            self.add_root(parent_dir(stem).unwrap_or(""));
            return;
        }
        let parent = parent_dir(&path).unwrap_or("");
        self.add_root(parent);
        let name = path.rsplit('/').next().unwrap_or(&path);
        if is_ident(name) {
            self.add_module(name.to_string(), path.clone());
        }
    }

    /// setuptools `package_dir` entry: `"" = "src"` names a root; `"pkg" =
    /// "lib/pkg"` names where one package lives.
    fn add_package_dir_entry(&mut self, key: &str, dir: &str) {
        let key = key.trim();
        let dir = norm_dir(dir);
        if key.is_empty() {
            self.add_root(dir);
            return;
        }
        let key_path = key.replace('.', "/");
        if let Some(root) = dir.strip_suffix(&key_path).map(|r| r.trim_end_matches('/')) {
            // `pkg = src/pkg` → root `src`.
            self.add_root(root);
        }
        if !key.contains('.') {
            self.add_module(key.to_string(), dir);
        }
    }
}

fn norm_dir(s: &str) -> String {
    let s = s.trim().replace('\\', "/");
    let s = s.strip_prefix("./").unwrap_or(&s);
    let s = s.trim_matches('/');
    if s == "." {
        String::new()
    } else {
        s.to_string()
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

fn str_or_list(v: &toml::Value) -> Vec<&str> {
    match v {
        toml::Value::String(s) => vec![s.as_str()],
        toml::Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
        _ => Vec::new(),
    }
}

fn roots_from_pyproject(text: &str, det: &mut Detected) {
    let Ok(doc) = toml::from_str::<toml::Value>(text) else {
        return;
    };
    let Some(tool) = doc.get("tool") else { return };

    // [tool.poetry] packages = [{ include = "serena", from = "src" }]
    if let Some(pkgs) = tool
        .get("poetry")
        .and_then(|p| p.get("packages"))
        .and_then(|p| p.as_array())
    {
        for p in pkgs {
            let Some(include) = p.get("include").and_then(|v| v.as_str()) else {
                continue;
            };
            let from = norm_dir(p.get("from").and_then(|v| v.as_str()).unwrap_or(""));
            det.add_root(&from);
            let include = norm_dir(include);
            let name = include.split('/').next().unwrap_or("");
            if is_ident(name) {
                let dir = if from.is_empty() {
                    name.to_string()
                } else {
                    format!("{from}/{name}")
                };
                det.add_module(name.to_string(), dir);
            }
        }
    }

    // [tool.setuptools] package-dir / packages.find.where
    if let Some(st) = tool.get("setuptools") {
        if let Some(pd) = st.get("package-dir").and_then(|v| v.as_table()) {
            for (k, v) in pd {
                if let Some(dir) = v.as_str() {
                    det.add_package_dir_entry(k, dir);
                }
            }
        }
        if let Some(w) = st
            .get("packages")
            .and_then(|p| p.get("find"))
            .and_then(|f| f.get("where"))
        {
            for dir in str_or_list(w) {
                det.add_root(dir);
            }
        }
    }

    // [tool.hatch.build] and [tool.hatch.build.targets.wheel]:
    //   packages = ["src/serena"], only-include = [...], sources = ["src"] | { "src/x" = "x" }
    if let Some(hb) = tool.get("hatch").and_then(|h| h.get("build")) {
        let wheel = hb.get("targets").and_then(|t| t.get("wheel"));
        for tbl in [Some(hb), wheel].into_iter().flatten() {
            for key in ["packages", "only-include"] {
                if let Some(v) = tbl.get(key) {
                    for p in str_or_list(v) {
                        det.add_package_path(p);
                    }
                }
            }
            match tbl.get("sources") {
                Some(toml::Value::Array(a)) => {
                    for s in a.iter().filter_map(|x| x.as_str()) {
                        det.add_root(s);
                    }
                }
                Some(toml::Value::Table(t)) => {
                    for (k, v) in t {
                        let k = norm_dir(k);
                        let v = v.as_str().map(norm_dir).unwrap_or_default();
                        if v.is_empty() {
                            det.add_root(&k);
                        } else if let Some(root) = k.strip_suffix(&v) {
                            det.add_root(root.trim_end_matches('/'));
                        } else if is_ident(&v) {
                            det.add_module(v, k);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // [tool.maturin] python-source = "python"
    if let Some(ps) = tool
        .get("maturin")
        .and_then(|m| m.get("python-source"))
        .and_then(|v| v.as_str())
    {
        det.add_root(ps);
    }
}

/// Minimal INI reader for `setup.cfg`: `[options] package_dir` and
/// `[options.packages.find] where`. Values may continue on indented lines.
fn roots_from_setup_cfg(text: &str, det: &mut Detected) {
    let mut section = String::new();
    let mut entries: Vec<(String, String, String)> = Vec::new(); // (section, key, value)

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed[1..trimmed.len() - 1].trim().to_string();
            continue;
        }
        let is_continuation = line.starts_with([' ', '\t']);
        if is_continuation {
            if let Some(last) = entries.last_mut() {
                if last.0 == section {
                    last.2.push('\n');
                    last.2.push_str(trimmed);
                    continue;
                }
            }
        }
        let split = trimmed.find('=').into_iter().chain(trimmed.find(':')).min();
        let Some(i) = split else { continue };
        let key = trimmed[..i].trim().to_string();
        let value = trimmed[i + 1..].trim().to_string();
        entries.push((section.clone(), key, value));
    }

    for (sec, key, value) in &entries {
        match (sec.as_str(), key.as_str()) {
            ("options", "package_dir") => {
                for line in value.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    match line.split_once('=') {
                        Some((k, v)) => det.add_package_dir_entry(k, v),
                        None => det.add_root(line),
                    }
                }
            }
            ("options.packages.find", "where") => {
                for dir in value
                    .split(['\n', ','])
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                {
                    det.add_root(dir);
                }
            }
            _ => {}
        }
    }
}

/// Best-effort text scan of `setup.py` for `package_dir={...}` and
/// `find_packages(where=...)` / `find_namespace_packages(where=...)`.
fn roots_from_setup_py(text: &str, det: &mut Detected) {
    // package_dir = { "": "src", "pkg": "lib/pkg" }
    let mut search = text;
    while let Some(i) = search.find("package_dir") {
        let after = &search[i + "package_dir".len()..];
        let after_eq = after.trim_start();
        if let Some(rest) = after_eq.strip_prefix('=') {
            let rest = rest.trim_start();
            if let Some(body) = rest.strip_prefix('{') {
                if let Some(end) = body.find('}') {
                    for pair in body[..end].split(',') {
                        if let Some((k, v)) = pair.split_once(':') {
                            det.add_package_dir_entry(unquote(k), unquote(v));
                        }
                    }
                }
            }
        }
        search = after;
    }

    // find_packages(where="src") / find_packages("src")
    for needle in ["find_namespace_packages(", "find_packages("] {
        let mut search = text;
        while let Some(i) = search.find(needle) {
            let args = &search[i + needle.len()..];
            let args = &args[..args.find(')').unwrap_or(args.len())];
            let mut found = false;
            if let Some(j) = args.find("where") {
                let v = args[j + "where".len()..].trim_start();
                if let Some(v) = v.strip_prefix('=') {
                    let v = v.trim_start();
                    if let Some(q) = quoted_prefix(v) {
                        det.add_root(q);
                        found = true;
                    }
                }
            }
            if !found {
                if let Some(q) = quoted_prefix(args.trim_start()) {
                    det.add_root(q);
                }
            }
            search = &search[i + needle.len()..];
        }
    }
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '"' || c == '\'')
}

/// If `s` starts with a quoted string literal, return its contents.
fn quoted_prefix(s: &str) -> Option<&str> {
    let quote = s.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = &s[1..];
    let end = body.find(quote)?;
    Some(&body[..end])
}

/// No config said anything: src-layout if `src/<pkg>/__init__.py` exists,
/// otherwise the repo root (added by the caller).
fn probe_layout(files: &FileIndex, det: &mut Detected) {
    if !files.has_dir("src") {
        return;
    }
    let src_layout = files
        .iter()
        .any(|p| p.file_name() == "__init__.py" && parent_dir(p.dir()) == Some("src"));
    if src_layout {
        det.add_root("src");
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/nonexistent", paths.iter().map(RelPath::new))
    }

    fn meta(roots: &[&str]) -> ProjectMeta {
        ProjectMeta {
            source_roots: roots.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn resolve(files: &FileIndex, meta: &ProjectMeta, from: &str, spec: &str) -> Option<String> {
        PythonResolver
            .resolve(&RelPath::new(from), spec, files, meta)
            .map(|p| p.as_str().to_string())
    }

    // ---------------------------------------------------------- detect

    #[test]
    fn detect_probes_src_layout_without_config() {
        let files = index(&["src/pkg/__init__.py", "src/pkg/a.py", "test/test_a.py"]);
        let m = PythonResolver.detect(&files);
        assert_eq!(m.source_roots, vec!["src".to_string(), String::new()]);
        assert_eq!(
            m.modules,
            vec![ModuleUnit {
                name: "pkg".into(),
                dir: "src/pkg".into()
            }]
        );
    }

    #[test]
    fn detect_flat_layout_is_repo_root() {
        let files = index(&["pkg/__init__.py", "pkg/a.py", "src/not_a_package.txt"]);
        let m = PythonResolver.detect(&files);
        assert_eq!(m.source_roots, vec![String::new()]);
        assert_eq!(m.modules[0].name, "pkg");
    }

    #[test]
    fn detect_reads_pyproject_from_disk() {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-python-detect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.hatch.build.targets.wheel]\npackages = [\"lib/serena\", \"lib/solidlsp\"]\n",
        )
        .unwrap();
        let files = FileIndex::new(
            &dir,
            [
                "pyproject.toml",
                "lib/serena/__init__.py",
                "lib/solidlsp/ls.py",
            ]
            .map(RelPath::new),
        );
        let m = PythonResolver.detect(&files);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(m.source_roots, vec!["lib".to_string(), String::new()]);
        let names: Vec<_> = m.modules.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["serena", "solidlsp"]);
    }

    #[test]
    fn pyproject_hatch_packages_paths() {
        let mut d = Detected::default();
        roots_from_pyproject(
            r#"
[project]
name = "serena-agent"
[tool.hatch.build.targets.wheel]
packages = ["src/serena", "src/interprompt", "src/solidlsp"]
"#,
            &mut d,
        );
        assert_eq!(d.roots, vec!["src"]);
        assert_eq!(d.modules.len(), 3);
        assert_eq!(
            d.modules[2],
            ModuleUnit {
                name: "solidlsp".into(),
                dir: "src/solidlsp".into()
            }
        );
    }

    #[test]
    fn pyproject_hatch_flat_packages_and_sources() {
        let mut d = Detected::default();
        roots_from_pyproject(
            r#"
[tool.hatch.build.targets.wheel]
packages = ["mypkg"]
sources = ["python"]
"#,
            &mut d,
        );
        assert_eq!(d.roots, vec!["", "python"]);

        let mut d = Detected::default();
        roots_from_pyproject(
            r#"
[tool.hatch.build.targets.wheel.sources]
"src/foo" = "foo"
"#,
            &mut d,
        );
        assert_eq!(d.roots, vec!["src"]);
    }

    #[test]
    fn pyproject_poetry_packages() {
        let mut d = Detected::default();
        roots_from_pyproject(
            r#"
[tool.poetry]
name = "x"
packages = [
  { include = "serena", from = "src" },
  { include = "flat_pkg" },
]
"#,
            &mut d,
        );
        assert_eq!(d.roots, vec!["src", ""]);
        assert_eq!(
            d.modules[0],
            ModuleUnit {
                name: "serena".into(),
                dir: "src/serena".into()
            }
        );
        assert_eq!(
            d.modules[1],
            ModuleUnit {
                name: "flat_pkg".into(),
                dir: "flat_pkg".into()
            }
        );
    }

    #[test]
    fn pyproject_setuptools_package_dir_and_find_where() {
        let mut d = Detected::default();
        roots_from_pyproject(
            r#"
[tool.setuptools]
package-dir = { "" = "src", "mypkg" = "lib/mypkg" }
[tool.setuptools.packages.find]
where = ["src", "lib"]
"#,
            &mut d,
        );
        assert_eq!(d.roots, vec!["src", "lib"]);
        assert_eq!(
            d.modules,
            vec![ModuleUnit {
                name: "mypkg".into(),
                dir: "lib/mypkg".into()
            }]
        );
    }

    #[test]
    fn pyproject_unparseable_is_ignored() {
        let mut d = Detected::default();
        roots_from_pyproject("this is = not [ toml", &mut d);
        assert!(d.roots.is_empty());
    }

    #[test]
    fn setup_cfg_package_dir_and_where() {
        let mut d = Detected::default();
        roots_from_setup_cfg(
            "[metadata]\nname = x\n\n[options]\npackage_dir =\n    = src\n    other = lib/other\npackages = find:\n\n[options.packages.find]\nwhere = src, lib\n",
            &mut d,
        );
        assert_eq!(d.roots, vec!["src", "lib"]);
        assert_eq!(
            d.modules,
            vec![ModuleUnit {
                name: "other".into(),
                dir: "lib/other".into()
            }]
        );

        let mut d = Detected::default();
        roots_from_setup_cfg("[options]\npackage_dir = =src\n", &mut d);
        assert_eq!(d.roots, vec!["src"]);
    }

    #[test]
    fn setup_py_package_dir_and_find_packages() {
        let mut d = Detected::default();
        roots_from_setup_py(
            "from setuptools import setup, find_packages\nsetup(\n    name='x',\n    package_dir={'': 'src'},\n    packages=find_packages(where='src'),\n)\n",
            &mut d,
        );
        assert_eq!(d.roots, vec!["src"]);

        let mut d = Detected::default();
        roots_from_setup_py("setup(packages=find_packages(\"python\"))", &mut d);
        assert_eq!(d.roots, vec!["python"]);

        let mut d = Detected::default();
        roots_from_setup_py(
            "setup(package_dir = {\"\": \"lib\", \"pkg\": \"lib/pkg\"})",
            &mut d,
        );
        assert_eq!(d.roots, vec!["lib"]);
        assert_eq!(d.modules[0].name, "pkg");
    }

    // --------------------------------------------------------- resolve

    #[test]
    fn src_layout_absolute_imports() {
        let files = index(&[
            "src/solidlsp/__init__.py",
            "src/solidlsp/ls.py",
            "src/solidlsp/language_servers/common.py",
            "src/serena/__init__.py",
            "src/serena/agent.py",
            "test/conftest.py",
            "test/__init__.py",
        ]);
        let m = PythonResolver.detect(&files);
        assert_eq!(
            resolve(&files, &m, "src/serena/agent.py", "solidlsp.ls"),
            Some("src/solidlsp/ls.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "src/serena/agent.py", "solidlsp"),
            Some("src/solidlsp/__init__.py".into())
        );
        assert_eq!(
            resolve(
                &files,
                &m,
                "test/conftest.py",
                "solidlsp.language_servers.common"
            ),
            Some("src/solidlsp/language_servers/common.py".into())
        );
        // `test.*` lives at the repo root, which is always the last root.
        assert_eq!(
            resolve(&files, &m, "test/x/test_a.py", "test.conftest"),
            Some("test/conftest.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "test/x/test_a.py", "test"),
            Some("test/__init__.py".into())
        );
    }

    #[test]
    fn flat_layout_absolute_imports() {
        let files = index(&[
            "pkg/__init__.py",
            "pkg/mod.py",
            "pkg/sub/__init__.py",
            "pkg/sub/deep.py",
            "top.py",
        ]);
        let m = meta(&[""]);
        assert_eq!(
            resolve(&files, &m, "pkg/mod.py", "pkg.sub.deep"),
            Some("pkg/sub/deep.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "pkg/mod.py", "pkg.sub"),
            Some("pkg/sub/__init__.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "pkg/mod.py", "top"),
            Some("top.py".into())
        );
        assert_eq!(resolve(&files, &m, "pkg/mod.py", "pkg.missing"), None);
    }

    #[test]
    fn single_dot_relative_imports() {
        let files = index(&[
            "pkg/__init__.py",
            "pkg/a.py",
            "pkg/b.py",
            "pkg/sub/__init__.py",
        ]);
        let m = meta(&[""]);
        assert_eq!(
            resolve(&files, &m, "pkg/a.py", ".b"),
            Some("pkg/b.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "pkg/a.py", ".sub"),
            Some("pkg/sub/__init__.py".into())
        );
        // `from . import b` → the current package.
        assert_eq!(
            resolve(&files, &m, "pkg/a.py", "."),
            Some("pkg/__init__.py".into())
        );
        // Inside __init__.py, `.` is the package itself.
        assert_eq!(
            resolve(&files, &m, "pkg/__init__.py", ".a"),
            Some("pkg/a.py".into())
        );
        // Whitespace from a verbatim parser slice is tolerated.
        assert_eq!(
            resolve(&files, &m, "pkg/a.py", ". b"),
            Some("pkg/b.py".into())
        );
    }

    #[test]
    fn multi_dot_relative_imports() {
        let files = index(&[
            "pkg/__init__.py",
            "pkg/b.py",
            "pkg/sub/__init__.py",
            "pkg/sub/a.py",
            "pkg/sub/deep/c.py",
        ]);
        let m = meta(&[""]);
        assert_eq!(
            resolve(&files, &m, "pkg/sub/a.py", "..b"),
            Some("pkg/b.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "pkg/sub/a.py", ".."),
            Some("pkg/__init__.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "pkg/sub/deep/c.py", "...b"),
            Some("pkg/b.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "pkg/sub/deep/c.py", "..a"),
            Some("pkg/sub/a.py".into())
        );
        // Too many dots escape the repo → not resolvable.
        assert_eq!(resolve(&files, &m, "pkg/sub/a.py", "....b"), None);
    }

    #[test]
    fn package_init_wins_over_same_named_module() {
        let files = index(&["pkg/__init__.py", "pkg/x.py", "pkg/x/__init__.py"]);
        let m = meta(&[""]);
        assert_eq!(
            resolve(&files, &m, "pkg/__init__.py", "pkg.x"),
            Some("pkg/x/__init__.py".into())
        );
    }

    #[test]
    fn pyi_stubs_and_init_pyi() {
        let files = index(&[
            "pkg/__init__.pyi",
            "pkg/stub.pyi",
            "pkg/impl.py",
            "pkg/impl.pyi",
        ]);
        let m = meta(&[""]);
        assert_eq!(
            resolve(&files, &m, "main.py", "pkg.stub"),
            Some("pkg/stub.pyi".into())
        );
        assert_eq!(
            resolve(&files, &m, "main.py", "pkg"),
            Some("pkg/__init__.pyi".into())
        );
        // Implementation preferred over its stub.
        assert_eq!(
            resolve(&files, &m, "main.py", "pkg.impl"),
            Some("pkg/impl.py".into())
        );
    }

    #[test]
    fn namespace_package_resolves_to_first_file() {
        let files = index(&[
            "src/ns/zeta.py",
            "src/ns/alpha.py",
            "src/ns/data.json",
            "src/empty_ns/readme.txt",
            "src/ns/sub/x.py",
        ]);
        let m = meta(&["src", ""]);
        assert_eq!(
            resolve(&files, &m, "src/main.py", "ns"),
            Some("src/ns/alpha.py".into())
        );
        // No Python files directly inside → not a module we can point at.
        assert_eq!(resolve(&files, &m, "src/main.py", "empty_ns"), None);
        // Relative `from . import x` inside a namespace package.
        assert_eq!(
            resolve(&files, &m, "src/ns/zeta.py", "."),
            Some("src/ns/alpha.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "src/ns/zeta.py", ".sub"),
            Some("src/ns/sub/x.py".into())
        );
    }

    #[test]
    fn stdlib_and_third_party_return_none() {
        let files = index(&["src/pkg/__init__.py", "src/pkg/a.py", "test/__init__.py"]);
        let m = PythonResolver.detect(&files);
        for spec in [
            "os",
            "os.path",
            "pydantic",
            "typing",
            "collections.abc",
            "pytest",
            "__future__",
        ] {
            assert_eq!(resolve(&files, &m, "src/pkg/a.py", spec), None, "{spec}");
        }
        assert_eq!(resolve(&files, &m, "src/pkg/a.py", ""), None);
    }

    #[test]
    fn module_units_cover_nonstandard_package_dir() {
        let files = index(&["lib/__init__.py", "lib/core.py"]);
        let m = ProjectMeta {
            source_roots: vec![String::new()],
            modules: vec![ModuleUnit {
                name: "mypkg".into(),
                dir: "lib".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            resolve(&files, &m, "main.py", "mypkg.core"),
            Some("lib/core.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "main.py", "mypkg"),
            Some("lib/__init__.py".into())
        );
    }

    #[test]
    fn nested_project_roots_resolve_only_regular_packages() {
        let files = index(&[
            "src/serena/__init__.py",
            "src/serena/mcp.py",
            "src/serena/tools/__init__.py",
            "src/serena/tools/tools_base.py",
            "src/serena/util/logging.py",
            "src/serena/util/dotnet.py",
            "test/resources/repos/python/test_repo/test_repo/__init__.py",
            "test/resources/repos/python/test_repo/test_repo/models.py",
            "test/resources/repos/python/test_repo/test_repo/services.py",
            "test/resources/repos/python/test_repo/scripts/run_app.py",
        ]);
        let m = PythonResolver.detect(&files);
        // Nested fixture project: `test_repo` is a regular package sitting in
        // an ancestor directory that is not itself a package.
        assert_eq!(
            resolve(
                &files,
                &m,
                "test/resources/repos/python/test_repo/scripts/run_app.py",
                "test_repo.models"
            ),
            Some("test/resources/repos/python/test_repo/test_repo/models.py".into())
        );
        assert_eq!(
            resolve(
                &files,
                &m,
                "test/resources/repos/python/test_repo/test_repo/services.py",
                "test_repo"
            ),
            Some("test/resources/repos/python/test_repo/test_repo/__init__.py".into())
        );
        // Bare sibling modules are NOT implicit-relative targets (PEP 328):
        // these are the stdlib `logging` and the third-party `mcp`.
        assert_eq!(
            resolve(&files, &m, "src/serena/util/dotnet.py", "logging"),
            None
        );
        assert_eq!(
            resolve(&files, &m, "src/serena/tools/tools_base.py", "mcp"),
            None
        );
        assert_eq!(
            resolve(
                &files,
                &m,
                "test/resources/repos/python/test_repo/test_repo/services.py",
                "models"
            ),
            None
        );
        // A directory that is itself a package is never a nested root.
        assert_eq!(
            resolve(&files, &m, "src/serena/mcp.py", "tools.tools_base"),
            None
        );
    }

    #[test]
    fn monorepo_members_resolve_across_packages() {
        let files = index(&[
            "packages/a/pyproject.toml",
            "packages/a/src/a/__init__.py",
            "packages/a/src/a/core.py",
            "packages/b/pyproject.toml",
            "packages/b/src/b/__init__.py",
            "packages/b/src/b/x.py",
        ]);
        let m = PythonResolver.detect(&files);
        assert_eq!(
            resolve(&files, &m, "packages/b/src/b/x.py", "a.core"),
            Some("packages/a/src/a/core.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "packages/a/src/a/core.py", "b.x"),
            Some("packages/b/src/b/x.py".into())
        );
    }

    #[test]
    fn unique_toplevel_package_is_a_global_fallback() {
        let files = index(&[
            "libs/util/__init__.py",
            "libs/util/h.py",
            "svc1/app/__init__.py",
            "svc1/app/m.py",
            "svc2/app/__init__.py",
            "svc2/app/m.py",
            "svc3/main.py",
        ]);
        let m = PythonResolver.detect(&files);
        assert_eq!(
            resolve(&files, &m, "svc3/main.py", "util.h"),
            Some("libs/util/h.py".into())
        );
        // Ambiguous top-level name: not guessed.
        assert_eq!(resolve(&files, &m, "svc3/main.py", "app.m"), None);
    }

    #[test]
    fn sibling_modules_outside_packages_resolve_but_not_stdlib() {
        let files = index(&[
            "tests/conftest.py",
            "tests/helpers.py",
            "tests/logging.py",
            "tools/run.py",
            "tools/util.py",
        ]);
        let m = PythonResolver.detect(&files);
        assert_eq!(
            resolve(&files, &m, "tests/conftest.py", "helpers"),
            Some("tests/helpers.py".into())
        );
        assert_eq!(
            resolve(&files, &m, "tools/run.py", "util"),
            Some("tools/util.py".into())
        );
        assert_eq!(resolve(&files, &m, "tests/conftest.py", "logging"), None);
    }

    #[test]
    fn package_init_importing_itself_is_not_an_edge() {
        let files = index(&["sub/__init__.py", "sub/m.py"]);
        let m = PythonResolver.detect(&files);
        assert_eq!(resolve(&files, &m, "sub/__init__.py", "."), None);
    }

    // ------------------------------------------------- real corpus gate

    /// Acceptance gate against the real serena checkout. Run with
    /// `cargo test -p astrolabe-core python -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn serena_corpus_resolution_rate() {
        // Resolved rather than hardcoded so the corpus can live anywhere:
        // `ASTROLABE_CORPUS_PYTHON`, else a sibling of the workspace.
        let root = std::env::var_os("ASTROLABE_CORPUS_PYTHON")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../serena")
            });
        let root = root.as_path();
        assert!(
            root.is_dir(),
            "corpus not found at {}; set ASTROLABE_CORPUS_PYTHON",
            root.display()
        );

        // Walk the checkout for Python sources, skipping the obvious noise.
        let mut paths: Vec<RelPath> = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if p.is_dir() {
                    if !matches!(
                        name.as_str(),
                        ".git"
                            | ".venv"
                            | "node_modules"
                            | "__pycache__"
                            | ".mypy_cache"
                            | ".ruff_cache"
                    ) {
                        stack.push(p);
                    }
                } else if name.ends_with(".py") || name.ends_with(".pyi") {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                    paths.push(RelPath::new(rel));
                }
            }
        }
        paths.sort();
        let files = FileIndex::new(root, paths.clone());
        let meta = PythonResolver.detect(&files);
        println!("files: {}", files.len());
        println!("source_roots: {:?}", meta.source_roots);
        println!(
            "modules: {:?}",
            meta.modules.iter().map(|m| &m.name).collect::<Vec<_>>()
        );

        // Names that count as "in-repo" for the gate: top-level entries at the
        // repo root and directly under each source root, plus every relative
        // import. Derived from the file list, not from the resolver, so the
        // denominator cannot be gamed.
        let mut in_repo_names = std::collections::BTreeSet::new();
        for p in files.iter() {
            let s = p.as_str();
            let first = s.split('/').next().unwrap();
            in_repo_names.insert(
                first
                    .trim_end_matches(".py")
                    .trim_end_matches(".pyi")
                    .to_string(),
            );
            for r in &meta.source_roots {
                if !r.is_empty() {
                    if let Some(rest) = s.strip_prefix(&format!("{r}/")) {
                        let f = rest.split('/').next().unwrap();
                        in_repo_names.insert(
                            f.trim_end_matches(".py")
                                .trim_end_matches(".pyi")
                                .to_string(),
                        );
                    }
                }
            }
        }

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("python grammar");

        let (mut total, mut in_repo, mut resolved, mut resolved_other) =
            (0usize, 0usize, 0usize, 0usize);
        let mut misses: Vec<(String, String)> = Vec::new();
        let mut others: Vec<(String, String, String)> = Vec::new();
        for p in &paths {
            let Ok(src) = std::fs::read_to_string(root.join(p.as_str())) else {
                continue;
            };
            let Some(tree) = parser.parse(&src, None) else {
                continue;
            };
            let mut specs = Vec::new();
            collect_imports(tree.root_node(), src.as_bytes(), &mut specs);
            for spec in specs {
                total += 1;
                let first = spec.split('.').next().unwrap_or("");
                let counts = spec.starts_with('.') || in_repo_names.contains(first);
                let hit = PythonResolver.resolve(p, &spec, &files, &meta);
                match (counts, hit.is_some()) {
                    (true, true) => {
                        in_repo += 1;
                        resolved += 1;
                    }
                    (true, false) => {
                        in_repo += 1;
                        misses.push((p.as_str().to_string(), spec));
                    }
                    (false, true) => {
                        resolved_other += 1;
                        others.push((
                            p.as_str().to_string(),
                            spec,
                            hit.unwrap().as_str().to_string(),
                        ));
                    }
                    (false, false) => {}
                }
            }
        }

        let rate = if in_repo == 0 {
            1.0
        } else {
            resolved as f64 / in_repo as f64
        };
        println!("total imports: {total}");
        println!("in-repo imports: {in_repo}");
        println!("resolved in-repo: {resolved} ({:.2}%)", rate * 100.0);
        println!("unresolved in-repo ({}):", misses.len());
        for (f, s) in misses.iter().take(60) {
            println!("  {f}  <-  {s}");
        }
        // Everything here should be a nested-project import (e.g. the
        // `test_repo` fixture). A stdlib name showing up means a false positive.
        println!("resolved outside the in-repo name set ({resolved_other}):");
        for (f, s, t) in others.iter().take(60) {
            println!("  {f}  <-  {s}  =>  {t}");
        }
        assert!(rate >= 0.95, "resolution rate {rate:.4} below the 95% gate");
    }

    /// Dev-only import extraction, mirroring what `parse` will emit: the dotted
    /// module for `import a.b`, and the (possibly dotted-prefixed) module of a
    /// `from X import ...` statement. `from __future__` is skipped.
    #[cfg(test)]
    fn collect_imports(node: tree_sitter::Node, src: &[u8], out: &mut Vec<String>) {
        let text = |n: tree_sitter::Node| n.utf8_text(src).unwrap_or("").to_string();
        match node.kind() {
            "import_statement" => {
                let mut c = node.walk();
                for n in node.children_by_field_name("name", &mut c) {
                    let target = if n.kind() == "aliased_import" {
                        n.child_by_field_name("name")
                    } else {
                        Some(n)
                    };
                    if let Some(t) = target {
                        out.push(text(t));
                    }
                }
            }
            "import_from_statement" => {
                if let Some(m) = node.child_by_field_name("module_name") {
                    out.push(text(m));
                }
            }
            _ => {
                let mut c = node.walk();
                for ch in node.children(&mut c) {
                    collect_imports(ch, src, out);
                }
            }
        }
    }
}
