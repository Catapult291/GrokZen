//! Shell / Bash (`.sh`, `.bash`, shebang) import resolution (lang-batch-8).
//!
//! Specifiers come from `source FILE` / `. FILE` (see `queries/bash/imports.scm`).
//! Shell resolves a bare relative path against the *caller's* working
//! directory, which is unknowable statically, so we probe in this order:
//!
//! 1. Literal paths (no `$`/backtick): the importing file's directory, then
//!    the repo root. Absolute paths (`/etc/profile`) are outside the repo.
//! 2. Dynamic prefixes — `"$DIR/lib.sh"`, `"${BASH_SOURCE%/*}/x.sh"`,
//!    `"$(dirname "$0")/x.sh"`: the literal tail after the last expansion
//!    (`/x.sh`) is tried against the importing directory and each ancestor
//!    (covers `$ROOT/lib/x.sh` style roots), and finally against a unique
//!    path suffix anywhere in the repo. Ambiguous suffixes stay `None`.

use crate::types::{join_rel, FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub struct ShellResolver;

impl ModuleResolver for ShellResolver {
    fn language(&self) -> Language {
        Language::Shell
    }

    fn detect(&self, _files: &FileIndex) -> ProjectMeta {
        ProjectMeta::default()
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        _meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let dir = from.dir();
        let probe = |base: &str, rest: &str| -> Option<RelPath> {
            let joined = join_rel(base, rest)?;
            (!joined.is_empty() && files.contains(&joined)).then(|| RelPath::new(joined))
        };

        if !spec.contains('$') && !spec.contains('`') {
            if spec.starts_with('/') || spec.starts_with('~') {
                return None;
            }
            return probe(dir, spec).or_else(|| probe("", spec));
        }

        let tail = literal_tail(spec)?;
        // Ancestors of the importing directory, innermost first, then root.
        let mut base = dir;
        loop {
            if let Some(hit) = probe(base, tail) {
                return Some(hit);
            }
            if base.is_empty() {
                break;
            }
            base = base.rfind('/').map_or("", |i| &base[..i]);
        }
        unique_suffix(files, tail)
    }
}

/// Literal `/path` after the last expansion in a dynamic specifier, with
/// surrounding quotes removed (`"$D"/x.sh`, `$D/"x.sh"`).
fn literal_tail(spec: &str) -> Option<&str> {
    let bytes = spec.as_bytes();
    let mut end = 0usize; // byte index just past the last expansion
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'$' if i + 1 < bytes.len() && (bytes[i + 1] == b'{' || bytes[i + 1] == b'(') => {
                let (open, close) = if bytes[i + 1] == b'{' {
                    (b'{', b'}')
                } else {
                    (b'(', b')')
                };
                let mut depth = 0i32;
                let mut j = i + 1;
                while j < bytes.len() {
                    if bytes[j] == open {
                        depth += 1;
                    } else if bytes[j] == close {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                i = j + 1;
                end = i.min(bytes.len());
            }
            b'$' => {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j == i + 1 && j < bytes.len() {
                    j += 1; // special parameter: $0, $@ …
                }
                i = j;
                end = i;
            }
            b'`' => {
                let close = spec[i + 1..].find('`').map(|k| i + 1 + k)?;
                i = close + 1;
                end = i;
            }
            _ => i += 1,
        }
    }
    let tail = spec[end.min(spec.len())..].trim_matches(|c| c == '"' || c == '\'');
    let tail = tail.strip_prefix('/')?;
    let tail = tail.trim_matches(|c| c == '"' || c == '\'');
    (!tail.is_empty() && !tail.contains('$') && !tail.contains('*')).then_some(tail)
}

fn unique_suffix(files: &FileIndex, tail: &str) -> Option<RelPath> {
    // Only files whose base name equals the tail's last segment can match
    // `s == tail || s.ends_with("/tail")`; the name index replaces a walk
    // over every file in the repo per unresolved `source`.
    let base = tail.rsplit('/').next().unwrap_or(tail);
    let mut found: Option<&RelPath> = None;
    for p in files.files_named(base) {
        let s = p.as_str();
        let hit = s == tail
            || (s.len() > tail.len()
                && s.ends_with(tail)
                && s.as_bytes()[s.len() - tail.len() - 1] == b'/');
        if hit {
            if found.is_some() {
                return None;
            }
            found = Some(p);
        }
    }
    found.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/repo", paths.iter().map(RelPath::new))
    }

    fn res(files: &FileIndex, from: &str, spec: &str) -> Option<String> {
        ShellResolver
            .resolve(&RelPath::new(from), spec, files, &ProjectMeta::default())
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn literal_paths_resolve_relative_then_root() {
        let f = index(&[
            "bin/run.sh",
            "bin/common.sh",
            "lib/util.bash",
            "test/a/b.sh",
        ]);
        assert_eq!(
            res(&f, "bin/run.sh", "./common.sh").as_deref(),
            Some("bin/common.sh")
        );
        assert_eq!(
            res(&f, "bin/run.sh", "../lib/util.bash").as_deref(),
            Some("lib/util.bash")
        );
        assert_eq!(
            res(&f, "bin/run.sh", "lib/util.bash").as_deref(),
            Some("lib/util.bash")
        );
        assert_eq!(res(&f, "bin/run.sh", "/etc/profile"), None);
        assert_eq!(res(&f, "bin/run.sh", "missing.sh"), None);
    }

    #[test]
    fn unique_suffix_needs_a_path_boundary() {
        let f = index(&["xlib/a.sh", "ops/lib/a.sh", "lib/b.sh", "b.sh.bak"]);
        // `xlib/a.sh` must not satisfy the tail `lib/a.sh`.
        assert_eq!(
            res(&f, "run.sh", "$D/lib/a.sh").as_deref(),
            Some("ops/lib/a.sh")
        );
        assert_eq!(
            res(&f, "run.sh", "$D/lib/b.sh").as_deref(),
            Some("lib/b.sh")
        );
        assert_eq!(res(&f, "run.sh", "$D/b.sh").as_deref(), Some("lib/b.sh"));
    }

    #[test]
    fn dynamic_prefixes_use_the_literal_tail() {
        let f = index(&[
            "nvm.sh",
            "test/fast/x.sh",
            "test/common.sh",
            "lib/bats-core/common.bash",
            "a/dup.sh",
            "b/dup.sh",
        ]);
        assert_eq!(
            res(&f, "test/fast/x.sh", "$NVM_DIR/nvm.sh").as_deref(),
            Some("nvm.sh")
        );
        assert_eq!(
            res(&f, "test/fast/x.sh", "${BASH_SOURCE%/*}/../common.sh").as_deref(),
            Some("test/common.sh")
        );
        assert_eq!(
            res(&f, "test/fast/x.sh", "$(dirname \"$0\")/../common.sh").as_deref(),
            Some("test/common.sh")
        );
        assert_eq!(
            res(&f, "x.sh", "\"$BATS_ROOT\"/lib/bats-core/common.bash").as_deref(),
            Some("lib/bats-core/common.bash")
        );
        assert_eq!(
            res(&f, "test/fast/x.sh", "$LIB/bats-core/common.bash").as_deref(),
            Some("lib/bats-core/common.bash")
        );
        assert_eq!(res(&f, "x.sh", "$D/dup.sh"), None, "ambiguous suffix");
        assert_eq!(res(&f, "x.sh", "$FILE"), None);
        assert_eq!(res(&f, "x.sh", "$D/*.sh"), None);
    }
}
