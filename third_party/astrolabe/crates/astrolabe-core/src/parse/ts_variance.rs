//! TypeScript 4.7 variance annotations on type parameters
//! (`interface I<in T, out U, in out V>`) are unknown to
//! tree-sitter-typescript 0.23 (upstream issue #370): the whole declaration
//! becomes an ERROR node. [`normalize_variance`] blanks the `in` / `out`
//! keywords with spaces of the same byte length, so offsets, lines and
//! columns are unchanged and the type parameter parses as a plain `T`.
//!
//! Only a keyword directly after `<` or `,` (comments and whitespace,
//! including newlines, may sit in between) and directly followed by a
//! type-parameter name is touched. Mapped types (`[K in T]`), `for (k in o)`
//! and comparisons (`a < out && b`) never match. The caller applies it only
//! when the first parse has errors and keeps the retry only if it is clean.

/// Same-length rewrite of variance keywords, `None` when nothing matches.
pub(super) fn normalize_variance(source: &str) -> Option<String> {
    if !source.contains("in ") && !source.contains("out ") {
        return None;
    }
    let b = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    for (i, &c) in b.iter().enumerate() {
        if c != b'<' && c != b',' {
            continue;
        }
        let mut j = skip_trivia(b, i + 1);
        // Up to two modifiers: `in`, `out`, or `in out`.
        let mut hits: Vec<(usize, usize)> = Vec::new();
        while hits.len() < 2 {
            let Some(len) = keyword_at(b, j) else { break };
            let after = skip_trivia(b, j + len);
            if after == j + len {
                break; // keyword must be followed by whitespace / a comment
            }
            hits.push((j, len));
            j = after;
        }
        if hits.is_empty() || !is_type_param_name(b, j) {
            continue;
        }
        // `in out T` is fine, `out in T` is not TypeScript.
        if hits.len() == 2 && b[hits[0].0] != b'i' {
            continue;
        }
        // `in T` alone: the name must be followed by `,`, `>`, `=` or
        // `extends`, so `<in x ...` inside an expression is left alone.
        let name_end = ident_end(b, j);
        let next = skip_trivia(b, name_end);
        let ok = matches!(b.get(next), Some(b',' | b'>' | b'='))
            || b[next..].starts_with(b"extends")
                && !b
                    .get(next + 7)
                    .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$');
        if !ok {
            continue;
        }
        let buf = out.get_or_insert_with(|| b.to_vec());
        for (at, len) in hits {
            for x in &mut buf[at..at + len] {
                *x = b' ';
            }
        }
    }
    out.and_then(|v| String::from_utf8(v).ok())
}

fn keyword_at(b: &[u8], j: usize) -> Option<usize> {
    for kw in [&b"out"[..], &b"in"[..]] {
        if b[j.min(b.len())..].starts_with(kw)
            && !b
                .get(j + kw.len())
                .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
        {
            return Some(kw.len());
        }
    }
    None
}

fn is_type_param_name(b: &[u8], j: usize) -> bool {
    b.get(j)
        .is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80)
}

fn ident_end(b: &[u8], mut j: usize) -> usize {
    while j < b.len()
        && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$' || b[j] >= 0x80)
    {
        j += 1;
    }
    j
}

/// Skip whitespace, `// line` and `/* block */` comments.
fn skip_trivia(b: &[u8], mut j: usize) -> usize {
    loop {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b[j.min(b.len())..].starts_with(b"//") {
            while j < b.len() && b[j] != b'\n' {
                j += 1;
            }
        } else if b[j.min(b.len())..].starts_with(b"/*") {
            match b[j + 2..].windows(2).position(|w| w == b"*/") {
                Some(p) => j += 2 + p + 2,
                None => return b.len(),
            }
        } else {
            return j;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_variance;

    #[test]
    fn blanks_variance_keywords_same_length() {
        let src = "interface I<in T, out U = unknown, in out V extends X> {}\n";
        let fixed = normalize_variance(src).unwrap();
        assert_eq!(fixed.len(), src.len());
        assert_eq!(
            fixed,
            "interface I<   T,     U = unknown,        V extends X> {}\n"
        );
    }

    #[test]
    fn multiline_and_commented_type_parameters() {
        let src = "export interface Z<\n  out Output = unknown,\n  /* c */ out Input,\n> {}\n";
        let fixed = normalize_variance(src).unwrap();
        assert_eq!(fixed.len(), src.len());
        assert!(!fixed.contains("out "), "{fixed}");
        assert!(fixed.contains("Output") && fixed.contains("Input"));
    }

    #[test]
    fn leaves_mapped_types_loops_and_expressions_alone() {
        for src in [
            "type M<T> = { [K in keyof T]: T[K] };\n",
            "for (const k in obj) {}\n",
            "if (a < out && b) {}\n",
            "const x = f(a, in_y, outer);\n",
            "type R = Record<string, number>;\n",
            "call(a, out);\n",
        ] {
            assert_eq!(normalize_variance(src), None, "{src}");
        }
    }

    #[test]
    fn variance_file_parses_clean_with_same_symbols_and_lines() {
        use crate::parse::ParserPool;
        use crate::types::{Language, RelPath};
        let src = "export interface ZodType<\n  out Output = unknown,\n  out Input = unknown,\n> {\n  parse(x: unknown): Output;\n}\nexport interface Check<in T = never> {\n  run(v: T): void;\n}\nexport function make(): number {\n  return 1;\n}\n";
        let plain = src.replace("out ", "    ").replace("in T", "   T");
        let pool = ParserPool::new();
        let path = RelPath::new("schemas.ts");
        let got = pool.parse(Language::TypeScript, &path, src).unwrap();
        let want = pool.parse(Language::TypeScript, &path, &plain).unwrap();
        assert!(!got.partial, "variance file must parse clean");
        let sig = |p: &crate::parse::ParsedFile| {
            p.symbols
                .iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        format!("{:?}", s.kind),
                        s.start_line,
                        s.end_line,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(sig(&got), sig(&want));
        assert!(got.symbols.iter().any(|s| s.name == "ZodType"));
        assert!(got.symbols.iter().any(|s| s.name == "Check"));
        // Tsx too.
        let tsx = pool
            .parse(Language::Tsx, &RelPath::new("a.tsx"), src)
            .unwrap();
        assert!(!tsx.partial);
    }
}
