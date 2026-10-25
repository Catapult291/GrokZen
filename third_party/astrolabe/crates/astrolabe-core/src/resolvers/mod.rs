//! Module-path resolution, one implementation per language.
//!
//! This is the highest-value layer in Astrolabe. Measured against the prior
//! art on five real repositories, reading build configuration lifts in-repo
//! import resolution from 15% to 100%.
//!
//! Counts below are the syntax-extraction denominator enforced by
//! `tests/import_resolution.rs` — comments and string literals masked out,
//! PEP 420 namespace packages included, Rust `use` counted per statement —
//! measured against the corpus revisions pinned in `scripts/fetch-corpora.sh`.
//! A working copy checked out at some other revision will produce a different
//! denominator; the resolution rate must still be 100%.
//!
//! Earlier drafts quoted line-scan figures (1557 / 91 / 205) that counted
//! imports inside docstrings and missed namespace packages; they are
//! superseded.
//!
//! | language   | corpus        | in-repo imports | prior art | target |
//! |------------|---------------|-----------------|-----------|--------|
//! | Python     | serena        | 1575            | 12%       | 100%   |
//! | Go         | gin           | 31              | 0%        | 100%   |
//! | Java       | gson          | 971             | 0%        | 100%   |
//! | Rust       | ripgrep       | 197             | 23%       | 100%   |
//! | TypeScript | openvisio-oss | 211             | 100%      | 100%   |
//! | Dart       | flutter-pkg   | 4067            | 0%        | 100%   |
//!
//! Note on Dart resolution: maps `pubspec.yaml` package names to `<pubspec_dir>/lib`,
//! resolving `package:<name>/<rest>` and relative imports to in-repo files while
//! correctly returning `None` for SDK (`dart:`) and third-party packages.
//!
//! C# reads SDK-style projects (`csproj` + the nearest `Directory.Build.props`).
//! An in-repo `ProjectReference` is one file-level import. A namespace `using`
//! is an [`crate::types::EdgeKind::Namespace`] relationship to every file that
//! declares that namespace (partials included), never a single import.
//! `System.*` and names no file declares (NuGet) resolve to nothing.
//! `using static` and aliases are an import only when exactly one in-repo
//! file declares the type.
//!
//! Each resolver owns exactly one file in this directory and must not touch
//! another. Shared behaviour belongs in [`crate::types`].

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::types::{FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub mod c_cpp;
pub mod csharp;
pub mod dart;
pub mod go;
pub mod java;
pub mod php;
pub mod python;
pub mod rust;
pub mod typescript;
pub mod visualbasic;
// lang-batch-8 stubs (one file per worker).
pub mod kotlin;
pub mod lua;
pub mod objc;
pub mod ruby;
pub mod shell;
pub mod swift;

/// All resolvers, one per supported language.
pub fn all() -> Vec<Box<dyn ModuleResolver>> {
    vec![
        Box::new(python::PythonResolver),
        Box::new(go::GoResolver),
        Box::new(java::JavaResolver),
        Box::new(rust::RustResolver),
        Box::new(typescript::TypeScriptResolver),
        Box::new(dart::DartResolver),
        Box::new(csharp::CSharpResolver),
        Box::new(visualbasic::VisualBasicResolver),
        Box::new(swift::SwiftResolver),
        Box::new(objc::ObjCResolver),
        Box::new(c_cpp::CResolver),
        Box::new(c_cpp::CppResolver),
        Box::new(kotlin::KotlinResolver),
        Box::new(shell::ShellResolver),
        Box::new(ruby::RubyResolver),
        Box::new(lua::LuaResolver),
        Box::new(php::PhpResolver),
    ]
}

/// Runs `detect` once per language, then routes each import to the resolver
/// that owns the importing file's language.
///
/// Each resolver is isolated: a panic in one language's `detect`/`resolve`
/// is recorded and skipped so the other languages still index. A hole that
/// is named (`detect_failures`) is better than an index that looks complete
/// because one `todo!()` took the whole run down.
pub struct ResolverSet {
    resolvers: Vec<Arc<dyn ModuleResolver>>,
    meta: Vec<(Language, ProjectMeta)>,
    detect_failures: Vec<Language>,
    /// Per resolver (same order as `resolvers`): the files its `detect` read.
    /// Together with the path set this is everything `detect` depends on.
    detect_reads: Vec<BTreeSet<String>>,
}

/// The resolver that owns `lang`'s imports (TS/TSX/JS/Vue share one, ObjC++
/// is ObjC).
pub fn resolver_owner(lang: Language) -> Language {
    match lang {
        Language::Tsx | Language::JavaScript | Language::Vue => Language::TypeScript,
        Language::ObjCpp => Language::ObjC,
        other => other,
    }
}

impl ResolverSet {
    pub fn detect(files: &FileIndex) -> Self {
        Self::from_resolvers(all().into_iter().map(Arc::from).collect(), files)
    }

    /// Incremental `detect`. `self` was detected over a file set identical to
    /// `files` (the caller must guarantee that: added/removed paths force a
    /// full [`Self::detect`]); only the content of `modified` paths may
    /// differ. A resolver whose recorded `detect` reads do not include any
    /// modified path would produce the same metadata, so it is reused
    /// as-is; the others re-run `detect`.
    ///
    /// Returns the new set and the languages whose metadata actually
    /// changed (re-run resolvers that came out different, or panicked).
    pub fn redetect(
        &self,
        files: &FileIndex,
        modified: &BTreeSet<RelPath>,
    ) -> (Self, Vec<Language>) {
        let mut meta = Vec::with_capacity(self.resolvers.len());
        let mut detect_failures = Vec::new();
        let mut detect_reads = Vec::with_capacity(self.resolvers.len());
        let mut changed = Vec::new();
        for (i, r) in self.resolvers.iter().enumerate() {
            let lang = r.language();
            let old_failed = self.detect_failures.contains(&lang);
            let stale = old_failed
                || self.detect_reads[i]
                    .iter()
                    .any(|p| modified.contains(&RelPath::new(p)));
            if !stale {
                meta.push(self.meta[i].clone());
                detect_reads.push(self.detect_reads[i].clone());
                continue;
            }
            files.start_recording_reads();
            let detected = isolate_resolver(lang, "detect", || r.detect(files));
            let reads = files.take_recorded_reads();
            let new_meta = match detected {
                Some(m) => m,
                None => {
                    detect_failures.push(lang);
                    ProjectMeta::default()
                }
            };
            if new_meta != self.meta[i].1 {
                changed.push(lang);
            }
            meta.push((lang, new_meta));
            detect_reads.push(reads);
        }
        (
            ResolverSet {
                resolvers: self.resolvers.clone(),
                meta,
                detect_failures,
                detect_reads,
            },
            changed,
        )
    }

    /// Same isolated `detect` path as [`Self::detect`], with an injected
    /// resolver list. `pub(crate)` so tests can plant a panicking double
    /// without changing [`all`].
    pub(crate) fn from_resolvers(
        resolvers: Vec<Arc<dyn ModuleResolver>>,
        files: &FileIndex,
    ) -> Self {
        let mut meta = Vec::with_capacity(resolvers.len());
        let mut detect_failures = Vec::new();
        let mut detect_reads = Vec::with_capacity(resolvers.len());
        for r in &resolvers {
            let lang = r.language();
            files.start_recording_reads();
            let detected = isolate_resolver(lang, "detect", || r.detect(files));
            detect_reads.push(files.take_recorded_reads());
            match detected {
                Some(m) => meta.push((lang, m)),
                None => {
                    detect_failures.push(lang);
                    meta.push((lang, ProjectMeta::default()));
                }
            }
        }
        ResolverSet {
            resolvers,
            meta,
            detect_failures,
            detect_reads,
        }
    }

    pub fn meta_for(&self, lang: Language) -> Option<&ProjectMeta> {
        self.meta.iter().find(|(l, _)| *l == lang).map(|(_, m)| m)
    }

    /// Languages whose `detect` panicked. Empty means every resolver finished.
    /// Callers must treat a non-empty list as a gap in the index, not as
    /// "this language has no modules".
    pub fn detect_failures(&self) -> &[Language] {
        &self.detect_failures
    }

    /// Resolve one import. `None` means the target is outside the repo, which
    /// is a valid answer for stdlib and third-party imports. A panic inside
    /// the owning resolver is also `None` — unresolved, not a crashed index.
    pub fn resolve(&self, from: &RelPath, spec: &str, files: &FileIndex) -> Option<RelPath> {
        match self.resolve_import(from, spec, files) {
            crate::types::ImportResolution::File(path) => Some(path),
            crate::types::ImportResolution::Files(v) => v.into_iter().next(),
            _ => None,
        }
    }

    /// Full resolution. [`ImportResolution::Namespace`] must be stored as
    /// [`crate::types::EdgeKind::Namespace`], not collapsed to one import.
    /// A panic in the owning resolver is [`ImportResolution::Unresolved`].
    pub fn resolve_import(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
    ) -> crate::types::ImportResolution {
        match Language::from_path(from) {
            Some(lang) => self.resolve_import_as(lang, from, spec, files),
            None => crate::types::ImportResolution::Unresolved,
        }
    }

    /// [`Self::resolve_import`] with the importing file's language supplied by
    /// the caller — needed for extension-less shebang scripts, whose language
    /// comes from content rather than the path.
    pub fn resolve_import_as(
        &self,
        lang: Language,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
    ) -> crate::types::ImportResolution {
        // TS and TSX and JS share one resolver.
        let owner = resolver_owner(lang);
        let Some(r) = self.resolvers.iter().find(|r| r.language() == owner) else {
            return crate::types::ImportResolution::Unresolved;
        };
        let Some(meta) = self.meta_for(owner) else {
            return crate::types::ImportResolution::Unresolved;
        };
        isolate_resolver(owner, "resolve", || {
            r.resolve_import(from, spec, files, meta)
        })
        .unwrap_or(crate::types::ImportResolution::Unresolved)
    }

    pub fn iter_meta(&self) -> impl Iterator<Item = &(Language, ProjectMeta)> {
        self.meta.iter()
    }
}

/// Run `f` and turn a panic into `None` so one resolver cannot take the rest
/// of the index down.
///
/// `AssertUnwindSafe` is required because `&dyn ModuleResolver` does not
/// implement `UnwindSafe` (trait objects never do — the compiler cannot see
/// through the vtable). It is sound here: every production resolver is a
/// field-less unit struct (`PythonResolver`, `GoResolver`, `JavaResolver`,
/// `RustResolver`, `TypeScriptResolver`); the trait only exposes `&self`
/// methods, with no interior mutability in the contract; and after unwind we
/// never resume in-flight mutable state of the panicked resolver — there is
/// none. Subsequent calls are fresh `&self` invocations. A test double with
/// `bool` flags is the same: the flags are set at construction and not mutated.
///
/// The default panic hook is left in place. `catch_unwind` still prints the
/// panic to stderr, which is the correct side-channel for an MCP server that
/// owns stdout as the protocol. Silencing via `panic::set_hook` would be a
/// process-global race — indexing uses rayon, and `cargo test` is multi-threaded
/// — and would hide the failure. Visibility is `tracing::warn!` plus
/// [`ResolverSet::detect_failures`].
fn isolate_resolver<T>(lang: Language, phase: &'static str, f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(payload) => {
            let panic = panic_payload_message(payload);
            tracing::warn!(
                language = lang.name(),
                phase,
                panic = %panic,
                "module resolver panicked; isolating this language so others keep working"
            );
            None
        }
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        String::from("unknown panic payload")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

    struct PanicResolver {
        lang: Language,
        panic_detect: bool,
        panic_resolve: bool,
    }

    impl ModuleResolver for PanicResolver {
        fn language(&self) -> Language {
            self.lang
        }

        fn detect(&self, _files: &FileIndex) -> ProjectMeta {
            if self.panic_detect {
                panic!("boom detect {}", self.lang.name());
            }
            ProjectMeta {
                source_roots: vec!["panicky-but-detected".into()],
                ..Default::default()
            }
        }

        fn resolve(
            &self,
            _from: &RelPath,
            _spec: &str,
            _files: &FileIndex,
            _meta: &ProjectMeta,
        ) -> Option<RelPath> {
            if self.panic_resolve {
                panic!("boom resolve {}", self.lang.name());
            }
            Some(RelPath::new("from-panicky.rs"))
        }
    }

    struct OkResolver {
        lang: Language,
    }

    impl ModuleResolver for OkResolver {
        fn language(&self) -> Language {
            self.lang
        }

        fn detect(&self, _files: &FileIndex) -> ProjectMeta {
            ProjectMeta {
                source_roots: vec![self.lang.name().into()],
                ..Default::default()
            }
        }

        fn resolve(
            &self,
            _from: &RelPath,
            _spec: &str,
            _files: &FileIndex,
            _meta: &ProjectMeta,
        ) -> Option<RelPath> {
            Some(RelPath::new(format!("{}.ok", self.lang.name())))
        }
    }

    fn empty_index() -> FileIndex {
        FileIndex::new("/tmp", Vec::<RelPath>::new())
    }

    #[test]
    fn detect_isolates_a_panicking_resolver() {
        let files = empty_index();
        let set = ResolverSet::from_resolvers(
            vec![
                Arc::new(OkResolver {
                    lang: Language::Python,
                }),
                Arc::new(PanicResolver {
                    lang: Language::Rust,
                    panic_detect: true,
                    panic_resolve: false,
                }),
                Arc::new(OkResolver { lang: Language::Go }),
            ],
            &files,
        );

        assert_eq!(set.detect_failures(), &[Language::Rust]);
        assert_eq!(
            set.meta_for(Language::Python).unwrap().source_roots,
            ["python"]
        );
        assert_eq!(set.meta_for(Language::Go).unwrap().source_roots, ["go"]);
        // Failed detect still occupies a slot, with empty meta — not omitted.
        assert!(set
            .meta_for(Language::Rust)
            .unwrap()
            .source_roots
            .is_empty());
        assert!(set.meta_for(Language::Rust).unwrap().modules.is_empty());
    }

    #[test]
    fn resolve_isolates_a_panicking_resolver() {
        let files = empty_index();
        let set = ResolverSet::from_resolvers(
            vec![
                Arc::new(OkResolver {
                    lang: Language::Python,
                }),
                Arc::new(PanicResolver {
                    lang: Language::Rust,
                    panic_detect: false,
                    panic_resolve: true,
                }),
            ],
            &files,
        );

        assert!(set.detect_failures().is_empty());
        assert_eq!(
            set.resolve(&RelPath::new("pkg/a.py"), "x", &files),
            Some(RelPath::new("python.ok"))
        );
        assert_eq!(set.resolve(&RelPath::new("src/lib.rs"), "x", &files), None);
    }

    #[test]
    fn healthy_detect_and_resolve_are_unchanged() {
        let files = empty_index();
        let set = ResolverSet::from_resolvers(
            vec![
                Arc::new(OkResolver {
                    lang: Language::Python,
                }),
                Arc::new(OkResolver {
                    lang: Language::TypeScript,
                }),
            ],
            &files,
        );

        assert!(set.detect_failures().is_empty());
        assert_eq!(
            set.meta_for(Language::Python).unwrap().source_roots,
            ["python"]
        );
        assert_eq!(
            set.meta_for(Language::TypeScript).unwrap().source_roots,
            ["typescript"]
        );
        assert_eq!(
            set.resolve(&RelPath::new("a.py"), "mod", &files),
            Some(RelPath::new("python.ok"))
        );
        // TS / TSX / JS still share one resolver.
        assert_eq!(
            set.resolve(&RelPath::new("a.ts"), "mod", &files),
            Some(RelPath::new("typescript.ok"))
        );
        assert_eq!(
            set.resolve(&RelPath::new("a.tsx"), "mod", &files),
            Some(RelPath::new("typescript.ok"))
        );
        assert_eq!(
            set.resolve(&RelPath::new("a.js"), "mod", &files),
            Some(RelPath::new("typescript.ok"))
        );
    }

    #[test]
    fn production_detect_always_returns_a_slot_per_language() {
        // Production resolvers may still be `todo!()` in a sibling workstream.
        // Isolation means this constructor itself must not panic, and every
        // language still has a meta slot (empty if that detect blew up).
        let set = ResolverSet::detect(&empty_index());
        for lang in [
            Language::Python,
            Language::Go,
            Language::Java,
            Language::Rust,
            Language::TypeScript,
            Language::Dart,
            Language::CSharp,
            Language::VisualBasic,
        ] {
            assert!(
                set.meta_for(lang).is_some(),
                "missing meta slot for {}",
                lang.name()
            );
        }
        for lang in set.detect_failures() {
            assert!(
                set.meta_for(*lang).unwrap() == &ProjectMeta::default(),
                "failed detect for {} must yield empty ProjectMeta",
                lang.name()
            );
        }
    }
}
