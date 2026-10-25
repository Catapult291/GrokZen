//! Index signatures on their own line in an object type inside type
//! arguments of a call (`useState<{ id: number\n  [key: string]: unknown }>()`).
//!
//! tree-sitter-typescript 0.23 reads `number\n  [key` as the start of an
//! indexed-access type (`number[key]`) in that position, so the member is an
//! ERROR node and the whole file is flagged partial. In a plain annotation
//! (`let x: { … }`) the same text parses. TypeScript separates the members
//! by the newline. [`separate_index_signatures`] turns the last indentation
//! byte before such a `[` into `;` (same length, so lines and columns are
//! unchanged), which is the explicit form the grammar accepts.
//!
//! Only `[name: …` / `[name?: …`-shaped brackets that start a line after
//! indentation are touched, and only when the previous code byte is not
//! already a separator. A `[ident:` can not start a JS expression, so array
//! literals, computed keys (`[key]: v`) and mapped types (`[K in T]`) never
//! match. The caller applies it only when the first parse has errors and
//! keeps the retry only if it is clean.

/// Same-length rewrite, `None` when nothing matches.
pub(super) fn separate_index_signatures(source: &str) -> Option<String> {
    if !source.contains('[') {
        return None;
    }
    let b = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    let mut line_start = 0usize;
    while line_start < b.len() {
        let line_end = b[line_start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len(), |p| line_start + p);
        let mut i = line_start;
        while i < line_end && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if i > line_start
            && line_start > 0
            && b.get(i) == Some(&b'[')
            && is_index_signature(&b[i + 1..line_end])
            && !matches!(
                prev_code_byte(b, line_start - 1),
                None | Some(b';' | b',' | b'{')
            )
        {
            out.get_or_insert_with(|| b.to_vec())[i - 1] = b';';
        }
        line_start = line_end + 1;
    }
    out.and_then(|v| String::from_utf8(v).ok())
}

/// `name: T]` / `name?: T]` (spaces allowed) right after `[`.
fn is_index_signature(rest: &[u8]) -> bool {
    let mut j = 0;
    while j < rest.len() && rest[j] == b' ' {
        j += 1;
    }
    let s = j;
    while j < rest.len() && (rest[j].is_ascii_alphanumeric() || matches!(rest[j], b'_' | b'$')) {
        j += 1;
    }
    if j == s || rest[s].is_ascii_digit() {
        return false;
    }
    while j < rest.len() && rest[j] == b' ' {
        j += 1;
    }
    if rest.get(j) == Some(&b'?') {
        j += 1;
    }
    rest.get(j) == Some(&b':') && rest[j..].contains(&b']')
}

/// Last non-whitespace byte at or before `end`, skipping `// …` line
/// comments on the lines walked over (best effort).
fn prev_code_byte(b: &[u8], end: usize) -> Option<u8> {
    let mut k = end as isize;
    while k >= 0 {
        // Start of the line holding `k`.
        let ls = b[..=k as usize]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |p| p + 1);
        let mut line = &b[ls..=k as usize];
        if let Some(p) = line.windows(2).position(|w| w == b"//") {
            line = &line[..p];
        }
        if let Some(&c) = line.iter().rev().find(|c| !c.is_ascii_whitespace()) {
            return Some(c);
        }
        k = ls as isize - 2;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::separate_index_signatures as sep;
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath};

    #[test]
    fn inserts_separator_same_length() {
        let src =
            "const [c, s] = useState<{\n  id: number\n  [key: string]: unknown\n} | null>(null)\n";
        let out = sep(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert_eq!(
            out,
            "const [c, s] = useState<{\n  id: number\n ;[key: string]: unknown\n} | null>(null)\n"
        );
    }

    #[test]
    fn leaves_separated_members_and_expressions_alone() {
        for src in [
            "type T = {\n  id: number;\n  [key: string]: unknown\n}\n",
            "type T = {\n  id: number, // c\n  [key: string]: unknown\n}\n",
            "type T = {\n  [key: string]: unknown\n}\n",
            "const o = {\n  a: 1,\n  [key]: 2,\n}\n",
            "const a = f(x)\n  [0]\n",
            "type M = {\n  readonly x: 1\n  [K in keyof T]: T[K]\n}\n",
        ] {
            assert_eq!(sep(src), None, "{src}");
        }
    }

    #[test]
    fn index_signature_in_type_argument_parses_clean() {
        let src = "export function useX() {\n  const [channel, setChannel] = useState<{\n    id: number\n    [key: string]: unknown\n  } | null>(null)\n  return channel\n}\nexport const after = 1\n";
        let pool = ParserPool::new();
        for (lang, path) in [(Language::TypeScript, "h.ts"), (Language::Tsx, "h.tsx")] {
            let p = pool.parse(lang, &RelPath::new(path), src).unwrap();
            assert!(!p.partial, "{path}");
            assert!(p.symbols.iter().any(|s| s.name == "useX"), "{path}");
            assert!(p.symbols.iter().any(|s| s.name == "after"), "{path}");
        }
    }
}
