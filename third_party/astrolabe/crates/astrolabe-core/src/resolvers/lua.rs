//! Lua module resolution (lang-batch-8).
//!
//! `require("a.b")` follows the default `package.path` templates relative to
//! a set of source roots: `<root>/a/b.lua`, then `<root>/a/b/init.lua`.
//! `dofile("x.lua")` / `loadfile("x.lua")` specifiers end in `.lua` and are
//! tried as plain paths (repo root, then the importing file's directory).
//!
//! Source roots, in priority order:
//!   * the repo root,
//!   * conventional roots that exist: `lua/` (Neovim plugins, runtimepath),
//!     `src/`, `lib/`, `script/` (LuaLS), `kong/..`-style roots are the repo
//!     root itself,
//!   * rockspec `build.modules` entries (`["kong.foo"] = "kong/foo.lua"`),
//!     which map a module name straight to a file,
//!   * finally, only for files living under a `test`/`tests`/`spec`/`script`/
//!     `scripts` directory, the importing file's ancestor directories,
//!     innermost first — projects that set `package.path` to their own
//!     directory (`test/`, `spec/` helpers, LuaLS `script/`). Library code
//!     does not get this fallback, so `require("json")` for an external
//!     module is not matched to a sibling `json.lua`.
//!
//! A module never resolves to the importing file itself.
//!
//! Rockspecs are read anywhere in the tree (paths relative to the rockspec's
//! directory); every `["name"] = "file.lua"` and `name = "file.lua"` pair is
//! picked up, one per line or several on a line.
//!
//! External modules (`require "cjson"`, `require "vim.lsp"`) yield `None`.

use crate::types::{FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath};

pub struct LuaResolver;

const CONVENTIONAL_ROOTS: [&str; 4] = ["lua", "src", "lib", "script"];

impl ModuleResolver for LuaResolver {
    fn language(&self) -> Language {
        Language::Lua
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut meta = ProjectMeta::default();
        meta.source_roots.push(String::new());
        for root in CONVENTIONAL_ROOTS {
            if files.has_dir(root) {
                meta.source_roots.push(root.to_owned());
            }
        }
        for path in files.iter() {
            if path.extension() == Some("rockspec") {
                if let Some(text) = files.read(path.as_str()) {
                    rockspec_modules(&text, path.dir(), files, &mut meta.modules);
                }
            }
        }
        meta.modules
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.dir.cmp(&b.dir)));
        meta.modules.dedup_by(|a, b| a.name == b.name);
        meta
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        resolve_module(from, spec, files, meta).filter(|p| p != from)
    }
}

fn resolve_module(
    from: &RelPath,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some(stem) = spec.strip_suffix(".lua") {
            // dofile / loadfile: a path, not a module name.
            let stem = stem.trim_start_matches("./");
            let from_dir = from.dir();
            return [String::new(), from_dir.to_owned()]
                .iter()
                .filter_map(|base| crate::types::join_rel(base, &format!("{stem}.lua")))
                .find(|p| files.contains(p))
                .map(RelPath::new);
        }
        if spec.contains(['/', '\\', ' ']) {
            return None;
        }
        let first = meta.modules.partition_point(|m| m.name.as_str() < spec);
        for m in meta.modules[first..].iter().take_while(|m| m.name == spec) {
            if files.contains(&m.dir) && m.dir != from.as_str() {
                return Some(RelPath::new(&m.dir));
            }
        }
        let rel = spec.replace('.', "/");
        let probe = |root: &str| -> Option<RelPath> {
            let base = if root.is_empty() {
                rel.clone()
            } else {
                format!("{root}/{rel}")
            };
            [format!("{base}.lua"), format!("{base}/init.lua")]
                .into_iter()
                .find(|p| files.contains(p) && p != from.as_str())
                .map(RelPath::new)
        };
        if let Some(hit) = meta.source_roots.iter().find_map(|r| probe(r)) {
            return Some(hit);
        }
        // Ancestor directories of the importing file, innermost first; only
        // for test/spec/script helpers.
        if !from.dir().split('/').any(|seg| {
            matches!(
                seg,
                "test" | "tests" | "spec" | "specs" | "script" | "scripts"
            )
        }) {
            return None;
        }
        let mut dir = from.dir();
        while !dir.is_empty() {
            if let Some(hit) = probe(dir) {
                return Some(hit);
            }
            dir = match dir.rfind('/') {
                Some(i) => &dir[..i],
                None => "",
            };
        }
        None
    }
}

/// `["kong.api"] = "kong/api/init.lua"` and `kong = "kong.lua"` pairs of a
/// rockspec, one or many per line. `base` is the rockspec's directory.
fn rockspec_modules(text: &str, base: &str, files: &FileIndex, out: &mut Vec<ModuleUnit>) {
    use std::sync::OnceLock;
    static PAIR: OnceLock<regex::Regex> = OnceLock::new();
    let re = PAIR.get_or_init(|| {
        regex::Regex::new(
            r#"(?:\[\s*["']([^"']+)["']\s*\]|\b([A-Za-z_][A-Za-z0-9_]*))\s*=\s*(?:"([^"]+\.lua)"|'([^']+\.lua)')"#,
        )
        .expect("static regex")
    });
    for cap in re.captures_iter(text) {
        let Some(name) = cap.get(1).or_else(|| cap.get(2)) else {
            continue;
        };
        let Some(path) = cap.get(3).or_else(|| cap.get(4)) else {
            continue;
        };
        let Some(full) = crate::types::join_rel(base, path.as_str()) else {
            continue;
        };
        if files.contains(&full) {
            out.push(ModuleUnit {
                name: name.as_str().to_owned(),
                dir: full,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/nonexistent", paths.iter().map(RelPath::new))
    }

    fn resolve(files: &FileIndex, from: &str, spec: &str) -> Option<String> {
        let meta = LuaResolver.detect(files);
        LuaResolver
            .resolve(&RelPath::new(from), spec, files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn dotted_module_maps_to_file_then_init() {
        let files = index(&["kong/db/init.lua", "kong/tools/utils.lua", "kong/init.lua"]);
        let r = |s| resolve(&files, "kong/init.lua", s);
        assert_eq!(
            r("kong.tools.utils").as_deref(),
            Some("kong/tools/utils.lua")
        );
        assert_eq!(r("kong.db").as_deref(), Some("kong/db/init.lua"));
        // `require("kong")` from kong/init.lua itself is a self-reference.
        assert_eq!(r("kong"), None);
        assert_eq!(
            resolve(&files, "kong/db/init.lua", "kong").as_deref(),
            Some("kong/init.lua")
        );
        assert_eq!(r("cjson.safe"), None);
    }

    #[test]
    fn neovim_lua_root() {
        let files = index(&[
            "lua/lspconfig/util.lua",
            "lua/lspconfig.lua",
            "plugin/x.lua",
        ]);
        assert_eq!(
            resolve(&files, "plugin/x.lua", "lspconfig.util").as_deref(),
            Some("lua/lspconfig/util.lua")
        );
        assert_eq!(
            resolve(&files, "plugin/x.lua", "lspconfig").as_deref(),
            Some("lua/lspconfig.lua")
        );
        assert_eq!(resolve(&files, "plugin/x.lua", "vim.lsp"), None);
    }

    #[test]
    fn ancestor_directory_fallback() {
        let files = index(&["spec/gen/a.lua", "spec/gen/helpers/b.lua"]);
        assert_eq!(
            resolve(&files, "spec/gen/a.lua", "helpers.b").as_deref(),
            Some("spec/gen/helpers/b.lua")
        );
    }

    #[test]
    fn dofile_paths() {
        let files = index(&["conf.lua", "spec/helpers.lua", "spec/a_spec.lua"]);
        assert_eq!(
            resolve(&files, "spec/a_spec.lua", "conf.lua").as_deref(),
            Some("conf.lua")
        );
        assert_eq!(
            resolve(&files, "spec/a_spec.lua", "helpers.lua").as_deref(),
            Some("spec/helpers.lua")
        );
    }

    #[test]
    fn rockspec_module_table() {
        let mut out = Vec::new();
        let files = index(&["kong/api/init.lua"]);
        rockspec_modules(
            "build = { modules = {\n  [\"kong.api\"] = \"kong/api/init.lua\",\n  [\"x\"] = \"missing.lua\",\n} }",
            "",
            &files,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kong.api");
    }

    #[test]
    fn rockspec_single_line_unquoted_and_subdir() {
        let files = index(&["pkg/lua/foo/bar.lua", "pkg/main.lua", "pkg/lua/plain.lua"]);
        let mut out = Vec::new();
        rockspec_modules(
            r#"build = { type = "builtin", modules = { ["m.a"]="lua/foo/bar.lua", plain = "lua/plain.lua", main='main.lua' } }"#,
            "pkg",
            &files,
            &mut out,
        );
        let names: Vec<_> = out
            .iter()
            .map(|m| (m.name.as_str(), m.dir.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("m.a", "pkg/lua/foo/bar.lua"),
                ("plain", "pkg/lua/plain.lua"),
                ("main", "pkg/main.lua")
            ]
        );
    }

    #[test]
    fn ancestor_fallback_is_limited_to_test_dirs_and_never_self() {
        // Library code: `require("json")` is an external module even though a
        // sibling json.lua exists; `require("bar")` must not loop back.
        let files = index(&["lua/foo/bar.lua", "lua/foo/json.lua"]);
        assert_eq!(resolve(&files, "lua/foo/bar.lua", "json"), None);
        assert_eq!(resolve(&files, "lua/foo/bar.lua", "bar"), None);
        // Same shape under a spec dir keeps the helper fallback, minus self.
        let files = index(&["spec/foo/bar.lua", "spec/foo/json.lua"]);
        assert_eq!(
            resolve(&files, "spec/foo/bar.lua", "json").as_deref(),
            Some("spec/foo/json.lua")
        );
        assert_eq!(resolve(&files, "spec/foo/bar.lua", "bar"), None);
    }
}
