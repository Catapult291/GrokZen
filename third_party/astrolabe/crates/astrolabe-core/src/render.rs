//! Rendering results for an agent to read.
//!
//! Every line that refers to code carries a `path:line` anchor, so the agent
//! can jump straight to a slice instead of reading whole files. This is the
//! single highest-leverage output convention in the prior art and it is worth
//! copying exactly.
//!
//! Bodies are elided: show the signature plus a few leading lines, then a
//! pointer. Measured, the graph path returned the same file-level answer as a
//! language server in half the characters.
//!
//! Confidence is part of the output, not a footnote. When a result comes from
//! name matching rather than resolved bindings, say so on the result — an
//! agent that knows an answer is uncertain will verify it, and one that does
//! not will ship the bug.

use crate::types::{CodeSymbol, Confidence, RelPath};

const MAX_SIGNATURE_CHARS: usize = 120;

/// `@path:line`
pub fn anchor(path: &RelPath, line: u32) -> String {
    format!("@{}:{line}", path.as_str())
}

/// Render a symbol signature and its jump target on one line.
///
/// Whitespace is collapsed so multiline signatures cannot break the
/// one-result-per-line format. Long signatures are truncated on a Unicode
/// scalar boundary and end in an ellipsis.
pub fn symbol_line(sym: &CodeSymbol, path: &RelPath) -> String {
    let signature = if sym.signature.trim().is_empty() {
        sym.name.as_str()
    } else {
        sym.signature.as_str()
    };
    let signature = signature.split_whitespace().collect::<Vec<_>>().join(" ");
    let signature = truncate_chars(&signature, MAX_SIGNATURE_CHARS);

    format!("{signature}  {}", anchor(path, sym.start_line))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }

    text.chars()
        .take(max_chars - 1)
        .chain(std::iter::once('…'))
        .collect()
}

/// Human-readable marker appended to a result set whose backing data is not
/// exact, e.g. "(syntactic: name-matched, may miss dynamic dispatch)".
pub fn confidence_note(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "",
        Confidence::Scoped => "(scoped: resolved from imports and lexical scope)",
        Confidence::Syntactic => "(syntactic: based on name matching; may miss dynamic dispatch)",
        Confidence::Unknown => "(unknown: unable to determine)",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::types::{FileId, SymbolId, SymbolKind};

    fn symbol(signature: impl Into<String>) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(1),
            file: FileId(2),
            name: "示例".into(),
            kind: SymbolKind::Function,
            signature: signature.into(),
            start_line: 42,
            end_line: 43,
            exported: true,
        }
    }

    #[test]
    fn anchor_has_jump_target_format() {
        assert_eq!(anchor(&RelPath::new("src/lib.rs"), 17), "@src/lib.rs:17");
    }

    #[test]
    fn long_signature_is_truncated_without_splitting_utf8() {
        let signature = format!("fn 测试_{}😀()", "非常长".repeat(60));
        let line = symbol_line(&symbol(&signature), &RelPath::new("src/中文.rs"));
        let rendered_signature = line.split_once("  @").unwrap().0;

        assert!(rendered_signature.ends_with('…'));
        assert!(rendered_signature.chars().count() <= MAX_SIGNATURE_CHARS);
        assert!(!line.contains(&signature));
        assert!(line.ends_with("@src/中文.rs:42"));
    }

    #[test]
    fn confidence_notes_are_distinct_and_actionable() {
        let notes = [
            confidence_note(Confidence::Exact),
            confidence_note(Confidence::Scoped),
            confidence_note(Confidence::Syntactic),
            confidence_note(Confidence::Unknown),
        ];

        assert_eq!(notes.into_iter().collect::<BTreeSet<_>>().len(), 4);
        assert_eq!(notes[0], "");
        assert!(notes[1].contains("imports"));
        assert!(notes[2].contains("name matching"));
        assert!(notes[2].contains("dynamic dispatch"));
        assert!(notes[3].contains("unable to determine"));
    }
}
