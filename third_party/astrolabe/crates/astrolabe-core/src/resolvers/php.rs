//! PHP (`.php`, `.phtml`, `.inc`) import resolution.
//!
//! The parser hands over two kinds of specifier:
//!
//! * a namespace name from `use A\B\C;`, `use A\B\C as D;`, `use A\B\{C, D}`
//!   (grouped clauses arrive already joined to their prefix, leading `\`
//!   dropped), and
//! * the literal string of `require` / `require_once` / `include` /
//!   `include_once`. For `__DIR__ . '/x.php'` that is `/x.php`.
//!
//! Namespaces go through every `composer.json` outside `vendor/`
//! (`autoload` and `autoload-dev`, since a monorepo has several):
//!
//! * `psr-4`: the longest declared prefix wins; the rest of the name is the
//!   path under the mapped directory(ies). Shorter prefixes are fallbacks.
//! * `psr-0`: the whole name, with `_` in the class part also a separator.
//! * `classmap`: directories or files; a class is matched by file stem
//!   (`Foo\Bar` -> `Bar.php`), preferring the candidate whose path mentions
//!   the namespace segments, and giving up when that is still ambiguous.
//!
//! Paths are tried relative to the including file (a leading `/` is the
//! `__DIR__ . '/x'` form), then the repo root. Names that no composer map
//! covers (framework and vendor classes) are `None`.

use crate::types::{
    join_rel, FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath,
};

pub struct PhpResolver;

const PSR4: &str = "psr4:";
const PSR0: &str = "psr0:";
const CLASSMAP: &str = "classmap:";
/// `files:<lowercased ns\\function>` / `files:const:<ns\\NAME>` -> declaring file.
const FILES: &str = "files:";

fn is_php_file(p: &str) -> bool {
    p.ends_with(".php") || p.ends_with(".inc") || p.ends_with(".phtml")
}

/// `function f` (lowercased, case-insensitive in PHP) and `const NAME` declared
/// at top level, qualified by the file's `namespace`. Methods are skipped by
/// requiring the declaration to start its line.
fn declared_symbols(text: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static NS: OnceLock<regex::Regex> = OnceLock::new();
    static FN: OnceLock<regex::Regex> = OnceLock::new();
    static CONST: OnceLock<regex::Regex> = OnceLock::new();
    let ns_re = NS.get_or_init(|| {
        regex::Regex::new(r"(?m)^\s*namespace\s+([A-Za-z_][\w\\]*)\s*[;{]").expect("regex")
    });
    let fn_re = FN.get_or_init(|| {
        regex::Regex::new(r"(?m)^(?:if\s*\([^)]*\)\s*\{\s*)?function\s+&?\s*([A-Za-z_]\w*)\s*\(")
            .expect("regex")
    });
    let const_re =
        CONST.get_or_init(|| regex::Regex::new(r"(?m)^const\s+([A-Za-z_]\w*)\s*=").expect("regex"));
    let ns = ns_re
        .captures(text)
        .map(|c| format!("{}\\", &c[1]))
        .unwrap_or_default();
    let mut out = Vec::new();
    for c in fn_re.captures_iter(text) {
        out.push(format!("{ns}{}", &c[1]).to_ascii_lowercase());
    }
    for c in const_re.captures_iter(text) {
        out.push(format!("const:{ns}{}", &c[1]));
    }
    out
}

fn dirs_of(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

impl ModuleResolver for PhpResolver {
    fn language(&self) -> Language {
        Language::Php
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut modules: Vec<ModuleUnit> = Vec::new();
        for p in files.iter() {
            let s = p.as_str();
            let base_dir = match s.strip_suffix("composer.json") {
                Some("") => "",
                Some(d) if d.ends_with('/') => d.trim_end_matches('/'),
                _ => continue,
            };
            if s.split('/')
                .any(|seg| seg == "vendor" || seg == "node_modules")
            {
                continue;
            }
            let Some(text) = files.read(s) else { continue };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            for section in ["autoload", "autoload-dev"] {
                let Some(al) = json.get(section) else {
                    continue;
                };
                for (key, tag) in [("psr-4", PSR4), ("psr-0", PSR0)] {
                    let Some(map) = al.get(key).and_then(|m| m.as_object()) else {
                        continue;
                    };
                    for (prefix, v) in map {
                        for d in dirs_of(v) {
                            if let Some(dir) = join_rel(base_dir, &d) {
                                modules.push(ModuleUnit {
                                    name: format!("{tag}{}", prefix.trim_start_matches('\\')),
                                    dir,
                                });
                            }
                        }
                    }
                }
                // `files` autoload entries declare functions / constants:
                // index `namespace X; function f` as `X\\f` so `use function X\\f`
                // can reach the file. Parsed with regexes, so best effort.
                if let Some(fl) = al.get("files") {
                    for d in dirs_of(fl) {
                        let Some(path) = join_rel(base_dir, &d) else {
                            continue;
                        };
                        if !files.contains(&path) {
                            continue;
                        }
                        let Some(text) = files.read(&path) else {
                            continue;
                        };
                        for name in declared_symbols(&text) {
                            modules.push(ModuleUnit {
                                name: format!("{FILES}{name}"),
                                dir: path.clone(),
                            });
                        }
                    }
                }
                if let Some(cm) = al.get("classmap") {
                    for d in dirs_of(cm) {
                        let Some(root) = join_rel(base_dir, &d) else {
                            continue;
                        };
                        for f in files.iter() {
                            let fs = f.as_str();
                            let under = root.is_empty()
                                || fs == root
                                || fs.starts_with(&format!("{root}/"));
                            if under && is_php_file(fs) {
                                let stem = fs
                                    .rsplit('/')
                                    .next()
                                    .and_then(|n| n.rsplit_once('.').map(|x| x.0))
                                    .unwrap_or("");
                                modules.push(ModuleUnit {
                                    name: format!("{CLASSMAP}{stem}"),
                                    dir: fs.to_owned(),
                                });
                            }
                        }
                    }
                }
            }
        }
        // Longest prefix first within a tag; classmap grouped by stem so it
        // can be binary searched.
        modules.sort_by(|a, b| {
            let (ka, kb) = (a.name.starts_with(CLASSMAP), b.name.starts_with(CLASSMAP));
            ka.cmp(&kb).then_with(|| {
                if ka {
                    a.name.cmp(&b.name).then_with(|| a.dir.cmp(&b.dir))
                } else {
                    b.name
                        .len()
                        .cmp(&a.name.len())
                        .then_with(|| a.name.cmp(&b.name))
                        .then_with(|| a.dir.cmp(&b.dir))
                }
            })
        });
        modules.dedup();
        ProjectMeta {
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
        let spec = spec.trim();
        if spec.is_empty() || spec.contains('$') {
            return None;
        }
        // `use function A\\f` / `use const A\\C`: only composer `files` entries
        // that declare the symbol can answer; otherwise unresolved.
        if let Some((kind, name)) = spec
            .strip_prefix("function:")
            .map(|n| ("", n))
            .or_else(|| spec.strip_prefix("const:").map(|n| ("const:", n)))
        {
            let name = name.trim_start_matches('\\');
            let key = if kind.is_empty() {
                format!("{FILES}{}", name.to_ascii_lowercase())
            } else {
                format!("{FILES}const:{name}")
            };
            let mut hits = meta.modules.iter().filter(|m| m.name == key);
            let first = hits.next()?;
            if hits.next().is_some() {
                return None;
            }
            return (first.dir != from.as_str()).then(|| RelPath::new(&first.dir));
        }
        let is_path = spec.contains('/')
            || spec.starts_with('.')
            || spec.ends_with(".php")
            || spec.ends_with(".inc")
            || spec.ends_with(".phtml");
        if is_path {
            return resolve_path(from, spec, files);
        }
        resolve_namespace(from, spec.trim_start_matches('\\'), files, meta)
    }
}

fn resolve_path(from: &RelPath, spec: &str, files: &FileIndex) -> Option<RelPath> {
    let probe = |base: &str, rest: &str| -> Option<RelPath> {
        let j = join_rel(base, rest)?;
        files.contains(&j).then(|| RelPath::new(&j))
    };
    // `__DIR__ . '/x.php'` arrives as `/x.php`; a real absolute path simply
    // fails to exist.
    let rest = spec.trim_start_matches('/');
    if rest.is_empty() {
        return None;
    }
    probe(from.dir(), rest).or_else(|| {
        if spec.starts_with('/') || spec.starts_with('.') {
            None
        } else {
            probe("", rest)
        }
    })
}

fn resolve_namespace(
    from: &RelPath,
    name: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    let hit = |dir: &str, rest: &str| -> Option<RelPath> {
        let j = join_rel(dir, &format!("{rest}.php"))?;
        files.contains(&j).then(|| RelPath::new(&j))
    };
    let psr = |tag: &'static str| meta.modules.iter().filter(move |m| m.name.starts_with(tag));
    let lower = name.to_ascii_lowercase();
    // PSR-4 (prefix ends with `\` by spec; a bare prefix is tolerated).
    for m in psr(PSR4) {
        let prefix = &m.name[PSR4.len()..];
        let p = prefix.trim_end_matches('\\');
        let rest = if p.is_empty() {
            Some(name)
        } else if let Some(r) = name.strip_prefix(p) {
            r.strip_prefix('\\')
        } else {
            None
        };
        if let Some(rest) = rest {
            if let Some(h) = hit(&m.dir, &rest.replace('\\', "/")) {
                return Some(h);
            }
        }
    }
    // PSR-0: the full name under the directory; `_` splits the class part.
    for m in psr(PSR0) {
        let prefix = m.name[PSR0.len()..].trim_end_matches('\\');
        if !prefix.is_empty() && !name.starts_with(prefix) {
            continue;
        }
        let (ns, class) = match name.rsplit_once('\\') {
            Some((n, c)) => (format!("{}/", n.replace('\\', "/")), c),
            None => (String::new(), name),
        };
        if let Some(h) = hit(&m.dir, &format!("{ns}{}", class.replace('_', "/"))) {
            return Some(h);
        }
    }
    // Classmap: by file stem.
    let class = name.rsplit('\\').next().unwrap_or(name);
    let key = format!("{CLASSMAP}{class}");
    let start = meta.modules.partition_point(|m| {
        let c = m.name.starts_with(CLASSMAP);
        (c, m.name.as_str()) < (true, key.as_str())
    });
    let cands: Vec<&ModuleUnit> = meta.modules[start..]
        .iter()
        .take_while(|m| m.name == key)
        .collect();
    match cands.len() {
        0 => None,
        1 => Some(RelPath::new(&cands[0].dir)),
        _ => {
            let segs: Vec<&str> = lower.split('\\').collect();
            let score = |m: &ModuleUnit| {
                let d = m.dir.to_ascii_lowercase();
                let mut ds = d.split('/');
                // Count namespace segments appearing as directories.
                segs.iter().filter(|s| ds.any(|x| x == **s)).count()
            };
            let best = cands.iter().map(|m| score(m)).max()?;
            let top: Vec<_> = cands.iter().filter(|m| score(m) == best).collect();
            // Ties: prefer the candidate nearest the includer.
            let near: Vec<_> = top
                .iter()
                .filter(|m| m.dir.starts_with(from.dir()))
                .collect();
            match (top.len(), near.len()) {
                (1, _) => Some(RelPath::new(&top[0].dir)),
                (_, 1) => Some(RelPath::new(&near[0].dir)),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup(composer: &str, paths: &[&str]) -> (tempfile::TempDir, FileIndex, ProjectMeta) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("composer.json"), composer).unwrap();
        let mut all = vec![RelPath::new("composer.json")];
        all.extend(paths.iter().map(RelPath::new));
        let files = FileIndex::new(dir.path(), all);
        let meta = PhpResolver.detect(&files);
        (dir, files, meta)
    }

    fn r(
        f: &(tempfile::TempDir, FileIndex, ProjectMeta),
        from: &str,
        spec: &str,
    ) -> Option<String> {
        PhpResolver
            .resolve(&RelPath::new(from), spec, &f.1, &f.2)
            .map(|p| p.to_string())
    }

    const COMPOSER: &str = r#"{
      "autoload": {
        "psr-4": {"App\\": "src/", "App\\Models\\": ["models/", "src/Models/"]},
        "psr-0": {"Legacy_": "lib/"},
        "classmap": ["database/seeds", "extra/Util.php"]
      },
      "autoload-dev": {"psr-4": {"Tests\\": "tests/"}}
    }"#;

    fn fixture() -> (tempfile::TempDir, FileIndex, ProjectMeta) {
        setup(
            COMPOSER,
            &[
                "src/Service/Mailer.php",
                "src/Models/Post.php",
                "models/User.php",
                "lib/Legacy/Db/Conn.php",
                "database/seeds/UserSeeder.php",
                "extra/Util.php",
                "tests/FooTest.php",
                "src/index.php",
                "src/inc/helpers.php",
                "config.php",
            ],
        )
    }

    #[test]
    fn dirname_levels_resolve_or_none() {
        let f = fixture();
        // From src/inc/x.php: `/../helpers.php` is src/helpers.php (absent) ...
        assert_eq!(
            r(&f, "src/inc/x.php", "/../index.php").as_deref(),
            Some("src/index.php")
        );
        assert_eq!(
            r(&f, "src/inc/x.php", "/../../config.php").as_deref(),
            Some("config.php")
        );
        // ... and walking above the repo root is None, not a wrong file.
        assert_eq!(r(&f, "src/inc/x.php", "/../../../config.php"), None);
        // Same-named file in the includer's dir must not win for `../`.
        let g = setup("{}", &["a/x.php", "a/b/x.php", "x.php"]);
        assert_eq!(r(&g, "a/b/y.php", "/../x.php").as_deref(), Some("a/x.php"));
    }

    #[test]
    fn use_function_const_via_composer_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("composer.json"),
            r#"{"autoload":{"files":["src/helpers.php"],"psr-4":{"App\\":"src/"}}}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/helpers.php"),
            "<?php\nnamespace App\\Support;\nfunction Fmt($x) {}\nconst LIMIT = 3;\nclass K { function method() {} }\n",
        )
        .unwrap();
        let all = [
            "composer.json",
            "src/helpers.php",
            "src/Support/Fmt.php",
            "src/a.php",
        ];
        let files = FileIndex::new(dir.path(), all.iter().map(RelPath::new));
        let meta = PhpResolver.detect(&files);
        let rr = |spec: &str| {
            PhpResolver
                .resolve(&RelPath::new("src/a.php"), spec, &files, &meta)
                .map(|p| p.to_string())
        };
        assert_eq!(
            rr("function:App\\Support\\fmt").as_deref(),
            Some("src/helpers.php")
        );
        assert_eq!(
            rr("const:App\\Support\\LIMIT").as_deref(),
            Some("src/helpers.php")
        );
        // Not declared in any `files` entry: no guess via psr-4 / classmap.
        assert_eq!(rr("function:App\\Support\\Fmt2"), None);
        assert_eq!(rr("function:App\\Support\\method"), None);
        assert_eq!(rr("function:Vendor\\f"), None);
    }

    #[test]
    fn psr4_longest_prefix_with_fallbacks_and_leading_backslash() {
        let f = fixture();
        assert_eq!(
            r(&f, "src/index.php", "App\\Service\\Mailer").as_deref(),
            Some("src/Service/Mailer.php")
        );
        assert_eq!(
            r(&f, "src/index.php", "\\App\\Service\\Mailer").as_deref(),
            Some("src/Service/Mailer.php")
        );
        // Longest prefix `App\Models\` first (models/), then its second dir.
        assert_eq!(
            r(&f, "src/index.php", "App\\Models\\User").as_deref(),
            Some("models/User.php")
        );
        assert_eq!(
            r(&f, "src/index.php", "App\\Models\\Post").as_deref(),
            Some("src/Models/Post.php")
        );
        // autoload-dev
        assert_eq!(
            r(&f, "src/index.php", "Tests\\FooTest").as_deref(),
            Some("tests/FooTest.php")
        );
    }

    #[test]
    fn psr0_and_classmap() {
        let f = fixture();
        assert_eq!(
            r(&f, "src/index.php", "Legacy_Db_Conn").as_deref(),
            Some("lib/Legacy/Db/Conn.php")
        );
        assert_eq!(r(&f, "src/index.php", "Legacy\\Db\\Conn"), None);
        assert_eq!(
            r(&f, "src/index.php", "Database\\Seeds\\UserSeeder").as_deref(),
            Some("database/seeds/UserSeeder.php")
        );
        assert_eq!(
            r(&f, "src/index.php", "Util").as_deref(),
            Some("extra/Util.php")
        );
    }

    #[test]
    fn third_party_and_unknown_are_none() {
        let f = fixture();
        assert_eq!(r(&f, "src/index.php", "Illuminate\\Support\\Str"), None);
        assert_eq!(r(&f, "src/index.php", "App\\Missing"), None);
    }

    #[test]
    fn include_paths_relative_dir_and_root() {
        let f = fixture();
        assert_eq!(
            r(&f, "src/index.php", "inc/helpers.php").as_deref(),
            Some("src/inc/helpers.php")
        );
        assert_eq!(
            r(&f, "src/index.php", "/inc/helpers.php").as_deref(),
            Some("src/inc/helpers.php")
        );
        assert_eq!(
            r(&f, "src/index.php", "../config.php").as_deref(),
            Some("config.php")
        );
        assert_eq!(
            r(&f, "src/index.php", "config.php").as_deref(),
            Some("config.php")
        );
        // `./` and `__DIR__` forms never fall back to the repo root.
        assert_eq!(r(&f, "src/index.php", "./config.php"), None);
        assert_eq!(r(&f, "src/index.php", "/nope.php"), None);
    }

    #[test]
    fn nested_composer_is_relative_to_its_dir_and_vendor_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("pkg")).unwrap();
        fs::create_dir_all(dir.path().join("vendor/x")).unwrap();
        fs::write(
            dir.path().join("pkg/composer.json"),
            r#"{"autoload":{"psr-4":{"Pkg\\":"src"}}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("vendor/x/composer.json"),
            r#"{"autoload":{"psr-4":{"V\\":"src"}}}"#,
        )
        .unwrap();
        let files = FileIndex::new(
            dir.path(),
            [
                "pkg/composer.json",
                "pkg/src/A.php",
                "vendor/x/composer.json",
                "vendor/x/src/B.php",
            ]
            .map(RelPath::new),
        );
        let f = (tempfile::tempdir().unwrap(), files, ProjectMeta::default());
        let meta = PhpResolver.detect(&f.1);
        let f = (f.0, f.1, meta);
        assert_eq!(r(&f, "x.php", "Pkg\\A").as_deref(), Some("pkg/src/A.php"));
        assert_eq!(r(&f, "x.php", "V\\B"), None);
    }

    #[test]
    fn malformed_composer_does_not_panic() {
        let f = setup("{ not json", &["a.php"]);
        assert_eq!(r(&f, "a.php", "A\\B"), None);
    }
}
