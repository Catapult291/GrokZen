//! Ruby (`.rb`, `Rakefile`, `Gemfile`) import resolution (lang-batch-8).
//!
//! The parser hands over the path of `require` / `load` / `autoload` as
//! written, and `require_relative "x"` as `relative:x` ([`RELATIVE_PREFIX`]):
//!
//! * `relative:x`, `./x`, `../x` — relative to the requiring file (only), so
//!   `require_relative "version"` never lands on a same-named `lib/version.rb`.
//! * otherwise, in order: the `$LOAD_PATH` roots Bundler / gemspecs set up
//!   (every `lib/` directory, the one enclosing the requiring file first),
//!   the `spec/` / `test/` root enclosing the requiring file (RSpec and
//!   Rake's test task put it on the load path: `require "support/x"`), then
//!   the requiring file's directory, then the repo root
//!   (`require "spec/support/x"` with `-I .`).
//!
//! `.rb` is appended unless present. Gems (`require "json"`) are `None`.

use crate::types::{join_rel, FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

/// Marks a `require_relative` specifier (set by the parser's Ruby import
/// extraction).
pub const RELATIVE_PREFIX: &str = "relative:";

pub struct RubyResolver;

impl ModuleResolver for RubyResolver {
    fn language(&self) -> Language {
        Language::Ruby
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut roots: Vec<String> = Vec::new();
        for p in files.iter() {
            if !p.as_str().ends_with(".rb") {
                continue;
            }
            let dir = p.dir();
            // Innermost `lib` ancestor of this file.
            let mut acc = String::new();
            let mut found: Option<String> = None;
            for seg in dir.split('/').filter(|s| !s.is_empty()) {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(seg);
                if seg == "lib" {
                    found = Some(acc.clone());
                }
            }
            if let Some(r) = found {
                if !roots.contains(&r) {
                    roots.push(r);
                }
            }
        }
        // Shallowest first: `lib` before `vendor/x/lib`.
        roots.sort_by_key(|r| (r.matches('/').count(), r.clone()));
        ProjectMeta {
            source_roots: roots,
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
        let (relative, spec) = match spec.strip_prefix(RELATIVE_PREFIX) {
            Some(rest) => (true, rest.trim()),
            None => (false, spec),
        };
        if spec.is_empty() || spec.contains("#{") {
            return None;
        }
        let spec = spec.strip_suffix(".rb").unwrap_or(spec);
        let probe = |base: &str| -> Option<RelPath> {
            let joined = join_rel(base, &format!("{spec}.rb"))?;
            files.contains(&joined).then(|| RelPath::new(&joined))
        };
        if relative || spec.starts_with("./") || spec.starts_with("../") {
            return probe(from.dir());
        }
        if spec.starts_with('/') {
            return None;
        }
        // Enclosing lib root first, then the others.
        let from_s = from.as_str();
        let enclosing = meta
            .source_roots
            .iter()
            .filter(|r| {
                from_s.len() > r.len()
                    && from_s.starts_with(r.as_str())
                    && from_s.as_bytes()[r.len()] == b'/'
            })
            .max_by_key(|r| r.len());
        if let Some(hit) = enclosing.and_then(|r| probe(r)) {
            return Some(hit);
        }
        for r in &meta.source_roots {
            if let Some(hit) = probe(r) {
                return Some(hit);
            }
        }
        test_root(from_s)
            .and_then(&probe)
            .or_else(|| probe(from.dir()))
            .or_else(|| probe(""))
    }
}

/// Innermost `spec` / `test` directory enclosing `from` (`spec/a/b_spec.rb`
/// → `spec`, `gems/x/test/y.rb` → `gems/x/test`).
fn test_root(from: &str) -> Option<&str> {
    let mut end = None;
    let mut offset = 0;
    for seg in from.split('/') {
        let next = offset + seg.len();
        if next < from.len() && (seg == "spec" || seg == "test") {
            end = Some(next);
        }
        offset = next + 1;
    }
    end.map(|e| &from[..e])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> FileIndex {
        FileIndex::new(
            "/nonexistent",
            [
                "lib/sinatra.rb",
                "lib/sinatra/base.rb",
                "lib/sinatra/main.rb",
                "rack-protection/lib/rack/protection.rb",
                "rack-protection/lib/rack/protection/base.rb",
                "test/test_helper.rb",
                "test/base_test.rb",
            ]
            .map(RelPath::new),
        )
    }

    fn r(from: &str, spec: &str) -> Option<String> {
        let files = idx();
        let meta = RubyResolver.detect(&files);
        RubyResolver
            .resolve(&RelPath::new(from), spec, &files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn load_path_and_relative() {
        assert_eq!(
            r("lib/sinatra/main.rb", "sinatra/base").as_deref(),
            Some("lib/sinatra/base.rb")
        );
        assert_eq!(
            r("lib/sinatra/main.rb", "./base").as_deref(),
            Some("lib/sinatra/base.rb")
        );
        assert_eq!(
            r("test/base_test.rb", "test_helper").as_deref(),
            Some("test/test_helper.rb")
        );
        assert_eq!(
            r("test/base_test.rb", "../lib/sinatra.rb").as_deref(),
            Some("lib/sinatra.rb")
        );
        assert_eq!(
            r(
                "rack-protection/lib/rack/protection.rb",
                "rack/protection/base"
            )
            .as_deref(),
            Some("rack-protection/lib/rack/protection/base.rb")
        );
        assert_eq!(r("test/base_test.rb", "json"), None);
        assert_eq!(
            r("test/base_test.rb", "relative:test_helper").as_deref(),
            Some("test/test_helper.rb")
        );
        assert_eq!(r("test/base_test.rb", "relative:sinatra/base"), None);
        assert_eq!(r("lib/sinatra.rb", "#{x}/y"), None);
    }

    #[test]
    fn require_relative_never_falls_back_to_load_path() {
        let files = FileIndex::new(
            "/nonexistent",
            ["lib/version.rb", "gems/x/version.rb", "gems/x/x.rb"].map(RelPath::new),
        );
        let meta = RubyResolver.detect(&files);
        let res = |spec: &str| {
            RubyResolver
                .resolve(&RelPath::new("gems/x/x.rb"), spec, &files, &meta)
                .map(|p| p.as_str().to_owned())
        };
        // Unmarked, `version` is a load-path require and hits `lib/`.
        assert_eq!(res("version").as_deref(), Some("lib/version.rb"));
        assert_eq!(
            res("relative:version").as_deref(),
            Some("gems/x/version.rb")
        );
        assert_eq!(res("relative:missing"), None);
    }

    #[test]
    fn spec_root_is_on_the_load_path() {
        let files = FileIndex::new(
            "/nonexistent",
            [
                "lib/rubocop.rb",
                "spec/support/file_helper.rb",
                "spec/rubocop/rake_task_spec.rb",
            ]
            .map(RelPath::new),
        );
        let meta = RubyResolver.detect(&files);
        let got = RubyResolver
            .resolve(
                &RelPath::new("spec/rubocop/rake_task_spec.rb"),
                "support/file_helper",
                &files,
                &meta,
            )
            .map(|p| p.as_str().to_owned());
        assert_eq!(got.as_deref(), Some("spec/support/file_helper.rb"));
        assert_eq!(test_root("spec/a/b_spec.rb"), Some("spec"));
        assert_eq!(test_root("gems/x/test/y.rb"), Some("gems/x/test"));
        assert_eq!(test_root("lib/spec.rb"), None);
    }
}
