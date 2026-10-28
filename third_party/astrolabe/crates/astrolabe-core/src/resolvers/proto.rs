//! Protocol Buffers (`.proto`) import resolution.
//!
//! Specifiers are path-like strings from `import "…"`, `import public "…"`,
//! and `import weak "…"`. Resolution order:
//!
//! 1. relative to the importing file's directory (`import "sibling.proto"`);
//! 2. the specifier as a repo-relative path (`import "api/v1/user.proto"`);
//! 3. under conventional proto roots that exist: `proto/`, `protos/`,
//!    `protobuf/`, `api/`, `apis/`, `idl/`, `third_party/`, `vendor/`.
//!
//! A module never resolves to the importing file itself. Well-known types
//! (`google/protobuf/…`) that are not in the repo yield `None`.

use crate::types::{FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub struct ProtoResolver;

const CONVENTIONAL_ROOTS: [&str; 8] = [
    "proto",
    "protos",
    "protobuf",
    "api",
    "apis",
    "idl",
    "third_party",
    "vendor",
];

impl ModuleResolver for ProtoResolver {
    fn language(&self) -> Language {
        Language::Proto
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut meta = ProjectMeta::default();
        meta.source_roots.push(String::new());
        for root in CONVENTIONAL_ROOTS {
            if files.has_dir(root) {
                meta.source_roots.push(root.to_owned());
            }
        }
        meta
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        resolve_import(from, spec, files, meta).filter(|p| p != from)
    }
}

fn resolve_import(
    from: &RelPath,
    spec: &str,
    files: &FileIndex,
    meta: &ProjectMeta,
) -> Option<RelPath> {
    let spec = spec.trim().trim_start_matches("./");
    if spec.is_empty() || spec.contains('\\') {
        return None;
    }
    let candidates = {
        let mut out = Vec::new();
        // 1. Relative to the importer.
        if let Some(rel) = crate::types::join_rel(from.dir(), spec) {
            out.push(rel);
        }
        // 2. Repo-relative as written.
        out.push(spec.to_owned());
        // 3. Under conventional roots.
        for root in &meta.source_roots {
            if root.is_empty() {
                continue;
            }
            out.push(format!("{root}/{spec}"));
        }
        out
    };
    candidates
        .into_iter()
        .find(|p| files.contains(p) && p != from.as_str())
        .map(RelPath::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/nonexistent", paths.iter().map(RelPath::new))
    }

    fn resolve(files: &FileIndex, from: &str, spec: &str) -> Option<String> {
        let meta = ProtoResolver.detect(files);
        ProtoResolver
            .resolve(&RelPath::new(from), spec, files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn relative_and_repo_root_imports() {
        let files = index(&[
            "api/v1/user.proto",
            "api/v1/common.proto",
            "api/common.proto",
        ]);
        assert_eq!(
            resolve(&files, "api/v1/user.proto", "common.proto").as_deref(),
            Some("api/v1/common.proto")
        );
        assert_eq!(
            resolve(&files, "api/v1/user.proto", "api/common.proto").as_deref(),
            Some("api/common.proto")
        );
    }

    #[test]
    fn conventional_root_prefix() {
        let files = index(&["proto/foo/bar.proto", "src/main.go"]);
        assert_eq!(
            resolve(&files, "src/main.go", "foo/bar.proto").as_deref(),
            Some("proto/foo/bar.proto")
        );
    }

    #[test]
    fn missing_and_self_are_none() {
        let files = index(&["a.proto"]);
        assert_eq!(resolve(&files, "a.proto", "a.proto"), None);
        assert_eq!(
            resolve(&files, "a.proto", "google/protobuf/timestamp.proto"),
            None
        );
    }
}
