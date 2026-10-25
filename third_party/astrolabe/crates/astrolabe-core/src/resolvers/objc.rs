//! Objective-C / Objective-C++ (`.m`, `.mm`, `.h`) import resolution.
//!
//! `#import` / `#include` specifiers resolve like Xcode's header search:
//!
//! 1. `"Foo.h"` / `"../x/Foo.h"` relative to the importing file;
//! 2. the repo-relative path itself (`#import "Sources/Foo.h"`);
//! 3. a header whose path ends with the specifier (`<AFNetworking/AFURLSessionManager.h>`
//!    → `…/AFNetworking/AFURLSessionManager.h`), nearest to the importer;
//! 4. framework-style `<Module/Header.h>` where the header is not under a
//!    `Module/` directory (CocoaPods / SwiftPM flatten public headers): any
//!    header with that file name, nearest to the importer;
//! 5. a bare `"Foo.h"` with no relative hit: any header with that file name
//!    (Xcode's flat "headermap" lookup), nearest to the importer.
//!
//! "Nearest" is the longest shared directory prefix with the importer, then
//! the shortest path. `@import UIKit;` and system headers that are not in the
//! repo (`<Foundation/Foundation.h>`) are external: `None`.
//!
//! `detect` indexes every header by file name into `ProjectMeta::modules`
//! (`name` = file name, `dir` = repo path), sorted, so `resolve` is a binary
//! search without disk access.

use crate::types::{
    join_rel, FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath,
};

pub struct ObjCResolver;

const HEADER_EXTENSIONS: [&str; 4] = ["h", "hh", "hpp", "hxx"];

impl ModuleResolver for ObjCResolver {
    fn language(&self) -> Language {
        Language::ObjC
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut meta = ProjectMeta::default();
        for path in files.iter() {
            if path
                .extension()
                .is_some_and(|e| HEADER_EXTENSIONS.iter().any(|h| e.eq_ignore_ascii_case(h)))
            {
                meta.modules.push(ModuleUnit {
                    name: path.file_name().to_owned(),
                    dir: path.as_str().to_owned(),
                });
            }
        }
        meta.modules
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.dir.cmp(&b.dir)));
        meta
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let spec = spec.trim().trim_start_matches("./");
        // `@import Module;` has no file extension; system / module imports.
        if spec.is_empty() || !spec.contains('.') {
            return None;
        }
        if let Some(p) = join_rel(from.dir(), spec).filter(|p| files.contains(p)) {
            return Some(RelPath::new(&p));
        }
        if files.contains(spec) {
            return Some(RelPath::new(spec));
        }
        let file_name = spec.rsplit('/').next().unwrap_or(spec);
        let start = meta
            .modules
            .partition_point(|m| m.name.as_str() < file_name);
        let candidates: Vec<&str> = meta.modules[start..]
            .iter()
            .take_while(|m| m.name == file_name)
            .map(|m| m.dir.as_str())
            .filter(|p| files.contains(p))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let suffix = format!("/{spec}");
        let suffixed: Vec<&str> = candidates
            .iter()
            .copied()
            .filter(|p| p.ends_with(&suffix))
            .collect();
        let pool = if suffixed.is_empty() {
            candidates
        } else {
            suffixed
        };
        nearest(from.dir(), &pool).map(RelPath::new)
    }
}

/// Candidate sharing the longest directory prefix with `dir`; ties → the
/// shorter path, then lexicographic (deterministic).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/repo", paths.iter().copied().map(RelPath::new))
    }

    fn resolve(files: &FileIndex, from: &str, spec: &str) -> Option<String> {
        let meta = ObjCResolver.detect(files);
        ObjCResolver
            .resolve(&RelPath::new(from), spec, files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn relative_quoted_import() {
        let f = index(&["Lib/AFURLSessionManager.h", "Lib/AFURLSessionManager.m"]);
        assert_eq!(
            resolve(&f, "Lib/AFURLSessionManager.m", "AFURLSessionManager.h").as_deref(),
            Some("Lib/AFURLSessionManager.h")
        );
    }

    #[test]
    fn framework_style_angle_import() {
        let f = index(&[
            "AFNetworking/AFURLSessionManager.h",
            "UIKit+AFNetworking/UIImageView+AFNetworking.m",
        ]);
        assert_eq!(
            resolve(
                &f,
                "UIKit+AFNetworking/UIImageView+AFNetworking.m",
                "AFNetworking/AFURLSessionManager.h"
            )
            .as_deref(),
            Some("AFNetworking/AFURLSessionManager.h")
        );
        // CocoaPods-flattened public headers: `<SDWebImage/SDImageCache.h>`.
        let f = index(&["SDWebImage/Core/SDImageCache.h", "Tests/A.m"]);
        assert_eq!(
            resolve(&f, "Tests/A.m", "SDWebImage/SDImageCache.h").as_deref(),
            Some("SDWebImage/Core/SDImageCache.h")
        );
    }

    #[test]
    fn flat_headermap_prefers_nearest() {
        let f = index(&["A/Util.h", "B/Util.h", "B/sub/X.m"]);
        assert_eq!(
            resolve(&f, "B/sub/X.m", "Util.h").as_deref(),
            Some("B/Util.h")
        );
    }

    #[test]
    fn external_imports_are_unresolved() {
        let f = index(&["A/X.m"]);
        assert_eq!(resolve(&f, "A/X.m", "Foundation/Foundation.h"), None);
        assert_eq!(resolve(&f, "A/X.m", "UIKit"), None);
    }
}
