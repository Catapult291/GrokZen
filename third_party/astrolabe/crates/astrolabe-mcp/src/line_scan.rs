//! Whole-buffer candidate search for `search_code`.
//!
//! Calling `Regex::is_match` once per line pays the regex start-up cost and a
//! cache-pool round trip for every line (the dominant cost on large repos,
//! with pool contention across rayon threads). Instead the file's lines are
//! joined into one buffer, searched once with a multi-line version of the
//! pattern, and the match offsets are mapped back to line numbers
//! (grep-searcher style). Only those candidate lines are then checked with
//! the original per-line matcher, so results are identical to a per-line scan.
//!
//! Why candidates are complete: with `(?m)` the anchors `^`/`$` match at line
//! boundaries, and `\b`/`\B` treat the joining `\n` like the end of a line, so
//! every per-line match is also a match of the buffer pattern at the same
//! offsets. Leftmost-first iteration can only skip a position by returning a
//! match that starts earlier and runs over it, and every line such a match
//! touches is a candidate. Patterns that can only match at the very start or
//! end of the haystack (`\A`, `\z`, a disabled `m` flag) would break that, so
//! they fall back to the per-line scan.

use std::cell::RefCell;

/// Buffer-level matcher; `None` means "scan every line".
pub(crate) struct LineScanner {
    buffer: Option<regex::Regex>,
}

impl LineScanner {
    /// `pattern` is the regex source the per-line matcher was built from
    /// (already escaped for literal queries) and `case_insensitive` its flag.
    pub(crate) fn new(pattern: &str, case_insensitive: bool) -> Self {
        let buffer = if buffer_safe(pattern) {
            regex::RegexBuilder::new(pattern)
                .multi_line(true)
                .case_insensitive(case_insensitive)
                .build()
                .ok()
        } else {
            None
        };
        Self { buffer }
    }

    /// Per-line fallback (for tests and unsafe patterns).
    #[cfg(test)]
    pub(crate) fn per_line() -> Self {
        Self { buffer: None }
    }

    /// Line indices (0-based, ascending, unique) that may match. A superset
    /// of the lines the per-line matcher accepts.
    pub(crate) fn candidates(&self, lines: &[String]) -> Vec<usize> {
        let Some(re) = &self.buffer else {
            return (0..lines.len()).collect();
        };
        if lines.is_empty() {
            return Vec::new();
        }
        thread_local! {
            static BUF: RefCell<(String, Vec<usize>)> = const { RefCell::new((String::new(), Vec::new())) };
        }
        BUF.with(|cell| {
            let mut guard = cell.borrow_mut();
            let (buf, starts) = &mut *guard;
            buf.clear();
            starts.clear();
            let total: usize = lines.iter().map(|l| l.len() + 1).sum();
            buf.reserve(total);
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    buf.push('\n');
                }
                starts.push(buf.len());
                buf.push_str(line);
            }
            let line_of = |offset: usize| starts.partition_point(|&s| s <= offset) - 1;
            let mut out: Vec<usize> = Vec::new();
            for m in re.find_iter(buf) {
                let first = line_of(m.start());
                let last = line_of(m.end().saturating_sub(1).max(m.start()));
                let from = out.last().map_or(first, |&l| first.max(l + 1));
                out.extend(from..=last);
            }
            // Keep the reusable buffer from pinning a huge file's memory.
            if buf.capacity() > 4 << 20 {
                *buf = String::new();
                *starts = Vec::new();
            }
            out
        })
    }
}

/// The pattern cannot use haystack-only anchors. Conservative: a false
/// "unsafe" only costs speed.
fn buffer_safe(pattern: &str) -> bool {
    if pattern.contains("\\A") || pattern.contains("\\z") {
        return false;
    }
    // Any inline flag group that clears flags (`(?-m)`, `(?i-m:…)`) might
    // clear `m`.
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while let Some(pos) = pattern[i..].find("(?") {
        let start = i + pos + 2;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b')' && bytes[j] != b':' {
            if bytes[j] == b'-' {
                return false;
            }
            j += 1;
        }
        i = start;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn per_line(re: &regex::Regex, lines: &[String]) -> Vec<usize> {
        (0..lines.len())
            .filter(|&i| re.is_match(&lines[i]))
            .collect()
    }

    fn check(pattern: &str, ci: bool, text: &str) {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .build()
            .unwrap();
        let want = per_line(&re, &lines);
        let scanner = LineScanner::new(pattern, ci);
        let cands = scanner.candidates(&lines);
        let got: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| re.is_match(&lines[i]))
            .collect();
        assert_eq!(got, want, "pattern {pattern:?} on {text:?}");
        assert!(cands.windows(2).all(|w| w[0] < w[1]), "{cands:?}");
        assert_eq!(
            LineScanner::per_line().candidates(&lines).len(),
            lines.len()
        );
    }

    const TEXT: &str = "fn main() {\n    let x = foo(1);\n\n  // TODO: Foo bar\nfoo\n}\r\nend foo\nFOO\nbaz\n\nquxfoo\n";

    #[test]
    fn candidates_match_per_line_scan() {
        let patterns = [
            "foo",
            "FOO",
            "^foo",
            "foo$",
            "^$",
            "^",
            "$",
            "\\bfoo\\b",
            "\\Bfoo",
            "x*",
            "a[^b]*c",
            "o\\s+b",
            "[^z]+",
            "(?s).",
            "(?s)foo.*bar",
            "\\s",
            "(?i)foo",
            "\\Afoo",
            "foo\\z",
            "(?-m)^foo",
            "(?i-m)^end",
            "^\\s*//",
            "\\}$",
            "let \\w+ =",
            "zzz",
            "",
            ".*",
            "o\\n?b",
            "[\\s\\S]{2}",
            "(?x)foo # c)",
            "a|^b",
            "(?i)x|^foo",
        ];
        for p in patterns {
            for ci in [false, true] {
                check(p, ci, TEXT);
                check(p, ci, "");
                check(p, ci, "\n\n");
                check(p, ci, "foo");
                check(p, ci, "ab\nc\nabc\n  foo bar\nfoo\tbar");
                check(p, ci, "日本語 foo\nÉté FOO\nß");
            }
        }
    }

    /// Pseudo-random texts over a tiny alphabet, so lines collide with the
    /// patterns often and matches straddle line ends.
    #[test]
    fn candidates_match_per_line_scan_on_random_texts() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = ['a', 'b', 'A', ' ', '\n', '\n', '_', '(', 'é'];
        let patterns = [
            "ab", "a b", "^a", "b$", "^$", "\\ba\\b", "a\\s*b", "[^b]+b", "(?s)a.b", "a|^b", "A",
            "\\(", "b\\s", "_\\w", "\\Ba", "é", "^\\s*\\(",
        ];
        for _ in 0..400 {
            let len = (next() % 60) as usize;
            let text: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            for p in patterns {
                check(p, false, &text);
                check(p, true, &text);
                check(&regex::escape(p), false, &text);
            }
        }
    }

    #[test]
    fn haystack_anchors_fall_back_to_every_line() {
        for p in ["\\Afoo", "foo\\z", "(?-m)^foo", "(?i-m:x)"] {
            assert!(!buffer_safe(p), "{p}");
        }
        for p in ["foo", "(?i)foo", "(?s:a.b)", "a-b", "^x$"] {
            assert!(buffer_safe(p), "{p}");
        }
    }

    #[test]
    fn literal_candidates_are_only_matching_lines() {
        let lines: Vec<String> = TEXT.lines().map(str::to_owned).collect();
        let scanner = LineScanner::new(&regex::escape("foo"), false);
        assert_eq!(scanner.candidates(&lines), vec![1, 4, 6, 10]);
    }
}
