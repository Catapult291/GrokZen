//! Line lexer for `search_code skip_comments`: marks which bytes of each line
//! are real code, i.e. not inside a comment (line or block) nor, optionally,
//! a string literal. Heuristic, per file, no parsing: good enough to separate
//! `SysLog(` calls from `// SysLog(` notes and `"SysLog("` strings.
//!
//! Languages without a lexer return `None` from [`code_mask`]; callers must
//! say so instead of pretending comments were skipped (see [`has_lexer`]).

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    C,
    Go,
    Rust,
    Java,
    Js,
    Dart,
    Kotlin,
    Swift,
    CSharp,
    Scala,
    Php,
    Vue,
    Python,
    Ruby,
    Shell,
    Yaml,
    Toml,
    Lua,
    Sql,
    Vb,
}

fn lang(path: &str) -> Option<Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "inl" | "ipp" | "cu" | "cuh"
        | "m" | "mm" => Lang::C,
        "go" => Lang::Go,
        "rs" => Lang::Rust,
        "java" => Lang::Java,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Lang::Js,
        "dart" => Lang::Dart,
        "kt" | "kts" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "cs" | "csx" => Lang::CSharp,
        "scala" => Lang::Scala,
        "php" | "phtml" => Lang::Php,
        "vue" => Lang::Vue,
        "py" | "pyi" => Lang::Python,
        "rb" | "rbw" | "rake" | "gemspec" => Lang::Ruby,
        "sh" | "bash" | "zsh" | "ksh" | "bats" | "zsh-theme" => Lang::Shell,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "lua" => Lang::Lua,
        "sql" => Lang::Sql,
        "vb" => Lang::Vb,
        _ => return None,
    })
}

/// Whether [`code_mask`] can lex this path (by extension).
pub(crate) fn has_lexer(path: &str) -> bool {
    lang(path).is_some()
}

/// Short label of a path's type for "no comment skipping" notes.
pub(crate) fn type_label(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) => format!(".{}", ext.to_ascii_lowercase()),
        None => name.to_string(),
    }
}

impl Lang {
    fn slash_line(self) -> bool {
        matches!(
            self,
            Lang::C
                | Lang::Go
                | Lang::Rust
                | Lang::Java
                | Lang::Js
                | Lang::Dart
                | Lang::Kotlin
                | Lang::Swift
                | Lang::CSharp
                | Lang::Scala
                | Lang::Php
                | Lang::Vue
        )
    }
    fn block_c(self) -> bool {
        self.slash_line() || self == Lang::Sql
    }
    fn nested(self) -> bool {
        matches!(
            self,
            Lang::Rust | Lang::Dart | Lang::Kotlin | Lang::Swift | Lang::Scala
        )
    }
    fn hash_line(self) -> bool {
        matches!(
            self,
            Lang::Php | Lang::Python | Lang::Ruby | Lang::Shell | Lang::Yaml | Lang::Toml
        )
    }
    fn triple(self, q: u8) -> bool {
        match self {
            Lang::Python | Lang::Dart | Lang::Toml => q == b'"' || q == b'\'',
            Lang::Java | Lang::Kotlin | Lang::Swift | Lang::CSharp | Lang::Scala => q == b'"',
            _ => false,
        }
    }
    fn backtick_multi(self) -> bool {
        matches!(self, Lang::Go | Lang::Js)
    }
    /// Is `\` an escape inside a string delimited by `q`?
    fn escapes(self, q: u8) -> bool {
        match self {
            Lang::Sql | Lang::Vb => false,
            Lang::Shell => q != b'\'',
            Lang::Go => q != b'`',
            _ => true,
        }
    }
}

#[derive(Clone, PartialEq)]
enum State {
    Code,
    /// `/* */` block comment with nesting depth.
    Block(u32),
    /// Multi-line string: triple quote (the quote byte) or backtick.
    Multi(u8),
    /// C# raw string `"""…"""` (`quotes` >= 3, closed by as many quotes; no
    /// escapes) or verbatim string `@"…"` (`quotes` == 0; `""` is a quote).
    /// Both may span lines. `dollars`: interpolation opens with that many
    /// `{` (`$$"""…{{x}}…"""`); 0 = not interpolated.
    CsStr {
        quotes: usize,
        dollars: usize,
    },
    /// Rust `"` string that continued past the end of a line.
    Str(u8),
    /// Raw string / long comment closed by an exact terminator.
    Raw {
        term: Vec<u8>,
        comment: bool,
    },
    /// Ruby `=begin` … `=end`.
    LineBlock,
    /// Ruby `%r{…}` / `%w[…]` / `%Q(…)` …: bracket delimiters nest.
    Percent {
        open: u8,
        close: u8,
        depth: u32,
        interp: bool,
    },
    /// Heredoc bodies still to read, in opener order (Ruby `<<~EOS`, PHP
    /// `<<<EOT`). Several can open on one line; their bodies follow in turn.
    Heredoc(Vec<Heredoc>),
}

#[derive(Clone, PartialEq)]
struct Heredoc {
    term: Vec<u8>,
    /// The terminator may be indented (Ruby `<<~` / `<<-`, PHP 7.3+).
    indent: bool,
    interp: bool,
    php: bool,
}

/// An open string interpolation (`${ … }`, `#{ … }`, `\( … )`, f-string `{ … }`):
/// code until the matching close, then the string resumes in `resume`.
struct Frame {
    depth: u32,
    open: u8,
    close: u8,
    resume: State,
    fstr: bool,
}

enum Step {
    Skip(usize),
    /// Interpolation opener of this length, with its open/close delimiters.
    Open(usize, u8, u8),
    Plain,
}

/// Does the string quote at `i` carry an interpolating prefix (`f"`, `$"`, `s"`)?
fn interp_prefix(lg: Lang, b: &[u8], i: usize) -> bool {
    let mut s = i;
    while s > 0 && (is_ident(b[s - 1]) || b[s - 1] == b'$' || b[s - 1] == b'@') && i - s < 3 {
        s -= 1;
    }
    let p = &b[s..i];
    if s > 0 && is_ident(b[s - 1]) {
        return false;
    }
    match lg {
        Lang::Python => p.iter().any(|c| matches!(c, b'f' | b'F')),
        Lang::CSharp => p.contains(&b'$'),
        Lang::Scala => matches!(p, b"s" | b"f") || p == b"raw",
        _ => false,
    }
}

/// What the string-body byte at `i` does: escape skip, interpolation, nothing.
fn step(lg: Lang, q: u8, fstr: bool, b: &[u8], i: usize) -> Step {
    let next = b.get(i + 1).copied().unwrap_or(0);
    if lg == Lang::Swift && b[i] == b'\\' && next == b'(' {
        return Step::Open(2, b'(', b')');
    }
    if b[i] == b'\\' && lg.escapes(q) {
        return Step::Skip(2);
    }
    if fstr && matches!(lg, Lang::Python | Lang::CSharp) && b[i] == b'{' {
        return if next == b'{' {
            Step::Skip(2)
        } else {
            Step::Open(1, b'{', b'}')
        };
    }
    let dollar = b[i] == b'$' && next == b'{';
    // PHP `"{$x->y}"`: the brace opens, `$x->y` is code.
    if lg == Lang::Php && q == b'"' && b[i] == b'{' && next == b'$' {
        return Step::Open(1, b'{', b'}');
    }
    let open = match lg {
        Lang::Php => q == b'"' && dollar,
        Lang::Kotlin => q == b'"' && dollar,
        Lang::Dart => dollar,
        Lang::Js => q == b'`' && dollar,
        Lang::Scala => q == b'"' && fstr && dollar,
        Lang::Ruby => (q == b'"' || q == b'`') && b[i] == b'#' && next == b'{',
        _ => false,
    };
    if open {
        Step::Open(2, b'{', b'}')
    } else {
        Step::Plain
    }
}

/// Can a `/` at `i` start a Ruby regex literal? (`a / b` is division.)
fn ruby_regex_allowed(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && (b[j - 1] == b' ' || b[j - 1] == b'\t') {
        j -= 1;
    }
    if j == 0 {
        return true;
    }
    let p = b[j - 1];
    if b"=(,:[!&|?{;+-*%<>~^".contains(&p) {
        return true;
    }
    if is_ident(p) {
        let mut st = j;
        while st > 0 && is_ident(b[st - 1]) {
            st -= 1;
        }
        return matches!(
            &b[st..j],
            b"if"
                | b"elsif"
                | b"unless"
                | b"while"
                | b"until"
                | b"when"
                | b"case"
                | b"and"
                | b"or"
                | b"not"
                | b"then"
                | b"else"
                | b"do"
                | b"return"
                | b"yield"
                | b"in"
        );
    }
    false
}

/// Ruby `%` literal at `i`: (opener length, open, close, interpolating).
/// `a % b` and `x %= 2` stay operators.
fn ruby_percent_open(b: &[u8], i: usize) -> Option<(usize, u8, u8, bool)> {
    let mut j = i + 1;
    let kind = b.get(j).copied().filter(|c| b"qQwWiIrsx".contains(c));
    if kind.is_some() {
        j += 1;
    }
    let d = *b.get(j)?;
    if !d.is_ascii_punctuation() || d == b'_' {
        return None;
    }
    // `puts %w[a b]`: a command argument after a space following a method
    // name. Not after a value (`"%d" %[x]`, `n %(m)` style modulo) and bare
    // `%` only with bracket-like delimiters.
    let mut k = i;
    while k > 0 && matches!(b[k - 1], b' ' | b'\t') {
        k -= 1;
    }
    let mut w = k;
    while w > 0 && is_ident(b[w - 1]) {
        w -= 1;
    }
    let arg = k < i
        && w < k
        && !b[w].is_ascii_digit()
        && d != b'='
        && (kind.is_some() || b"([{<|!".contains(&d));
    if b")]}>".contains(&d) {
        return None;
    }
    if !(ruby_regex_allowed(b, i) || arg) {
        return None;
    }
    let close = match d {
        b'(' => b')',
        b'[' => b']',
        b'{' => b'}',
        b'<' => b'>',
        _ => d,
    };
    let interp = !matches!(kind, Some(b'q' | b'w' | b'i' | b's'));
    Some((j + 1 - i, d, close, interp))
}

/// Ruby heredoc opener `<<~ID`, `<<-ID`, `<<ID`, `<<~'ID'`, `<<~"ID"` at `i`.
fn ruby_heredoc_open(b: &[u8], i: usize) -> Option<(usize, Heredoc)> {
    let mut j = i + 2;
    let flag = matches!(b.get(j), Some(b'~' | b'-'));
    if flag {
        j += 1;
    }
    let (term, interp, end) = heredoc_ident(b, j)?;
    if !flag {
        // `a <<b` / `x << y` are appends: bare `<<ID` needs an upper-case
        // identifier and no operand glued right before it.
        let first = term.first().copied().unwrap_or(0);
        let prev = if i > 0 { b[i - 1] } else { b' ' };
        if !(first.is_ascii_uppercase() || b[j] != first) || is_ident(prev) {
            return None;
        }
    }
    Some((
        end - i,
        Heredoc {
            term,
            indent: flag,
            interp,
            php: false,
        },
    ))
}

/// PHP `<<<ID`, `<<<"ID"`, `<<<'ID'` (nowdoc) at `i`.
fn php_heredoc_open(b: &[u8], i: usize) -> Option<(usize, Heredoc)> {
    let mut j = i + 3;
    while matches!(b.get(j), Some(b' ' | b'\t')) {
        j += 1;
    }
    let (term, interp, end) = heredoc_ident(b, j)?;
    // The opener must end the line.
    if b[end..].iter().any(|c| !c.is_ascii_whitespace()) {
        return None;
    }
    Some((
        end - i,
        Heredoc {
            term,
            indent: true,
            interp,
            php: true,
        },
    ))
}

/// Identifier at `j`, optionally quoted: (identifier, interpolating, end).
fn heredoc_ident(b: &[u8], j: usize) -> Option<(Vec<u8>, bool, usize)> {
    let q = b
        .get(j)
        .copied()
        .filter(|c| matches!(c, b'\'' | b'"' | b'`'));
    let s = j + usize::from(q.is_some());
    let mut e = s;
    while e < b.len() && is_ident(b[e]) {
        e += 1;
    }
    if e == s || b[s].is_ascii_digit() {
        return None;
    }
    let mut end = e;
    if let Some(q) = q {
        if b.get(e) != Some(&q) {
            return None;
        }
        end += 1;
    }
    Some((b[s..e].to_vec(), q != Some(b'\''), end))
}

/// Does `line` close heredoc `h`? Returns the end of the terminator.
fn heredoc_close(h: &Heredoc, b: &[u8]) -> Option<usize> {
    let mut s = 0;
    if h.indent {
        while s < b.len() && matches!(b[s], b' ' | b'\t') {
            s += 1;
        }
    }
    let e = s + h.term.len();
    if b.get(s..e) != Some(h.term.as_slice()) {
        return None;
    }
    let rest = &b[e..];
    let ok = if h.php {
        rest.first().is_none_or(|c| !is_ident(*c))
    } else {
        rest.iter().all(|c| c.is_ascii_whitespace())
    };
    ok.then_some(e)
}

/// Code ranges inside one heredoc body line: Ruby `#{…}`, PHP `{$…}` / `${…}`.
fn heredoc_interp(b: &[u8], php: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        let open = if php {
            (b[i] == b'{' && b[i + 1] == b'$') || (b[i] == b'$' && b[i + 1] == b'{')
        } else {
            b[i] == b'#' && b[i + 1] == b'{'
        };
        if !open {
            i += 1;
            continue;
        }
        let s = if php && b[i] == b'{' { i + 1 } else { i + 2 };
        let mut d = 0u32;
        let mut j = s;
        while j < b.len() {
            match b[j] {
                b'{' => d += 1,
                b'}' if d == 0 => break,
                b'}' => d -= 1,
                _ => {}
            }
            j += 1;
        }
        out.push((s, j));
        i = j + 1;
    }
    out
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || from > b.len() || b.len() - from < pat.len() {
        return None;
    }
    (from..=b.len() - pat.len()).find(|&p| &b[p..p + pat.len()] == pat)
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `[=*[` at `i`: returns the level and the length of the opener.
fn lua_long_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'=') {
        j += 1;
    }
    (b.get(j) == Some(&b'[')).then_some((j - i - 1, j - i + 1))
}

/// Can a `/` at `i` start a JS regex literal? (Previous token is an operator.)
fn regex_allowed(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && (b[j - 1] == b' ' || b[j - 1] == b'\t') {
        j -= 1;
    }
    if j == 0 {
        return true;
    }
    let p = b[j - 1];
    if b"=(,:[!&|?{};+-*%<>~^".contains(&p) {
        return true;
    }
    if is_ident(p) {
        let mut s = j;
        while s > 0 && is_ident(b[s - 1]) {
            s -= 1;
        }
        return matches!(
            &b[s..j],
            b"return"
                | b"typeof"
                | b"case"
                | b"in"
                | b"of"
                | b"delete"
                | b"void"
                | b"throw"
                | b"new"
                | b"else"
                | b"do"
                | b"yield"
                | b"await"
        );
    }
    false
}

/// Per line, per byte: `true` when the byte is code. `None` for languages
/// without a lexer. With `strings_are_code`, only comments are masked.
pub(crate) fn code_mask(
    path: &str,
    lines: &[String],
    strings_are_code: bool,
) -> Option<Vec<Vec<bool>>> {
    let lg = lang(path)?;
    let mut state = State::Code;
    // Open string interpolations (innermost last) and whether the current
    // string is an f-string style one.
    let mut frames: Vec<Frame> = Vec::new();
    let mut fs = false;
    // Heredocs opened on the current line; their bodies start on the next.
    let mut pending: Vec<Heredoc> = Vec::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let b = line.as_bytes();
        let n = b.len();
        let mut mask = vec![true; n];
        let comment = |mask: &mut Vec<bool>, from: usize, to: usize| {
            for m in &mut mask[from.min(n)..to.min(n)] {
                *m = false;
            }
        };
        let string = |mask: &mut Vec<bool>, from: usize, to: usize| {
            if !strings_are_code {
                for m in &mut mask[from.min(n)..to.min(n)] {
                    *m = false;
                }
            }
        };
        let mut i = 0;
        if let State::Heredoc(mut queue) = state.clone() {
            let h = queue.remove(0);
            if let Some(end) = heredoc_close(&h, b) {
                string(&mut mask, 0, end);
                state = if queue.is_empty() {
                    State::Code
                } else {
                    State::Heredoc(queue)
                };
                if !h.php || state != State::Code {
                    out.push(mask);
                    continue;
                }
                // PHP: `EOT;` / `EOT, $x);` — lex the rest of the line.
                i = end;
            } else {
                let mut from = 0;
                if h.interp {
                    for (s, e) in heredoc_interp(b, h.php) {
                        string(&mut mask, from, s);
                        from = e;
                    }
                }
                string(&mut mask, from, n);
                out.push(mask);
                continue;
            }
        }
        if lg == Lang::Ruby {
            if state == State::Code && line.starts_with("=begin") {
                state = State::LineBlock;
            }
            if state == State::LineBlock {
                comment(&mut mask, 0, n);
                if line.starts_with("=end") {
                    state = State::Code;
                }
                out.push(mask);
                continue;
            }
        }
        while i < n {
            match state.clone() {
                State::LineBlock | State::Heredoc(_) => i = n,
                State::Percent {
                    open,
                    close,
                    depth,
                    interp,
                } => {
                    let start = i;
                    let mut d = depth;
                    let mut closed = false;
                    let mut opened = false;
                    while i < n {
                        if b[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if interp && b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
                            opened = true;
                            break;
                        }
                        if open != close && b[i] == open {
                            d += 1;
                        } else if b[i] == close {
                            if d == 0 {
                                i += 1;
                                closed = true;
                                break;
                            }
                            d -= 1;
                        }
                        i += 1;
                    }
                    if opened {
                        string(&mut mask, start, i + 2);
                        frames.push(Frame {
                            depth: 0,
                            open: b'{',
                            close: b'}',
                            resume: State::Percent {
                                open,
                                close,
                                depth: d,
                                interp,
                            },
                            fstr: false,
                        });
                        i += 2;
                        state = State::Code;
                    } else {
                        string(&mut mask, start, i);
                        state = if closed {
                            State::Code
                        } else {
                            State::Percent {
                                open,
                                close,
                                depth: d,
                                interp,
                            }
                        };
                    }
                }
                State::Block(depth) => {
                    let start = i;
                    let mut d = depth;
                    let mut closed = false;
                    while i < n {
                        if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                            i += 2;
                            d -= 1;
                            if d == 0 {
                                closed = true;
                                break;
                            }
                            continue;
                        }
                        if lg.nested() && b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                            d += 1;
                            i += 2;
                            continue;
                        }
                        i += 1;
                    }
                    comment(&mut mask, start, i);
                    state = if closed { State::Code } else { State::Block(d) };
                }
                State::Raw {
                    term,
                    comment: is_comment,
                } => {
                    let start = i;
                    let closed = find(b, i, &term);
                    i = closed.map_or(n, |p| p + term.len());
                    if is_comment {
                        comment(&mut mask, start, i);
                    } else {
                        string(&mut mask, start, i);
                    }
                    if closed.is_some() {
                        state = State::Code;
                    }
                }
                State::CsStr { quotes, dollars } => {
                    let start = i;
                    let run =
                        |b: &[u8], i: usize, c: u8| b[i..].iter().take_while(|&&x| x == c).count();
                    let mut closed = false;
                    let mut opened = None;
                    while i < n {
                        if b[i] == b'"' {
                            let r = run(b, i, b'"');
                            if quotes == 0 {
                                // Verbatim: `""` is an escaped quote.
                                if r >= 2 {
                                    i += 2;
                                    continue;
                                }
                                i += 1;
                                closed = true;
                                break;
                            }
                            i += r;
                            if r >= quotes {
                                closed = true;
                                break;
                            }
                            continue;
                        }
                        if dollars > 0 && b[i] == b'{' {
                            let r = run(b, i, b'{');
                            let need = dollars.max(1);
                            if quotes == 0 && r >= 2 {
                                // `{{` is a literal brace in `$@"…"`.
                                i += 2;
                                continue;
                            }
                            if r >= need {
                                // Extra leading braces are content.
                                i += r - need;
                                opened = Some(need);
                                break;
                            }
                            i += r;
                            continue;
                        }
                        i += 1;
                    }
                    if let Some(len) = opened {
                        string(&mut mask, start, i + len);
                        frames.push(Frame {
                            depth: 0,
                            open: b'{',
                            close: b'}',
                            resume: State::CsStr { quotes, dollars },
                            fstr: false,
                        });
                        fs = false;
                        i += len;
                        state = State::Code;
                    } else {
                        string(&mut mask, start, i);
                        if closed {
                            state = State::Code;
                        }
                    }
                }
                State::Str(q) => {
                    let start = i;
                    let mut closed = false;
                    let mut opened = None;
                    while i < n {
                        match step(lg, q, fs, b, i) {
                            Step::Skip(k) => {
                                i += k;
                                continue;
                            }
                            Step::Open(len, o, c) => {
                                opened = Some((len, o, c));
                                break;
                            }
                            Step::Plain => {}
                        }
                        if b[i] == q {
                            i += 1;
                            closed = true;
                            break;
                        }
                        i += 1;
                    }
                    if let Some((len, open, close)) = opened {
                        string(&mut mask, start, i + len);
                        frames.push(Frame {
                            depth: 0,
                            open,
                            close,
                            resume: State::Str(q),
                            fstr: fs,
                        });
                        fs = false;
                        i += len;
                        state = State::Code;
                    } else {
                        string(&mut mask, start, i);
                        if closed {
                            state = State::Code;
                        }
                    }
                }
                State::Multi(q) => {
                    let start = i;
                    let mut opened = None;
                    while i < n {
                        match step(lg, q, fs, b, i) {
                            Step::Skip(k) => {
                                i += k;
                                continue;
                            }
                            Step::Open(len, o, c) => {
                                opened = Some((len, o, c));
                                break;
                            }
                            Step::Plain => {}
                        }
                        if q == b'`' {
                            if b[i] == b'`' {
                                i += 1;
                                state = State::Code;
                                break;
                            }
                        } else if b[i] == q && b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q)
                        {
                            i += 3;
                            state = State::Code;
                            break;
                        }
                        i += 1;
                    }
                    if let Some((len, open, close)) = opened {
                        string(&mut mask, start, i + len);
                        frames.push(Frame {
                            depth: 0,
                            open,
                            close,
                            resume: State::Multi(q),
                            fstr: fs,
                        });
                        fs = false;
                        i += len;
                        state = State::Code;
                    } else {
                        string(&mut mask, start, i);
                    }
                }
                State::Code => {
                    let c = b[i];
                    let next = b.get(i + 1).copied().unwrap_or(0);
                    let prev = if i > 0 { b[i - 1] } else { 0 };
                    if let Some(top) = frames.last_mut() {
                        if c == top.open {
                            top.depth += 1;
                            i += 1;
                            continue;
                        }
                        if c == top.close {
                            if top.depth == 0 {
                                let f = frames.pop().expect("non-empty");
                                state = f.resume;
                                fs = f.fstr;
                            } else {
                                top.depth -= 1;
                            }
                            i += 1;
                            continue;
                        }
                    }
                    if lg.slash_line() && c == b'/' && next == b'/' {
                        comment(&mut mask, i, n);
                        i = n;
                    } else if lg.block_c() && c == b'/' && next == b'*' {
                        state = State::Block(1);
                        comment(&mut mask, i, i + 2);
                        i += 2;
                    } else if lg == Lang::Vue && b[i..].starts_with(b"<!--") {
                        state = State::Raw {
                            term: b"-->".to_vec(),
                            comment: true,
                        };
                        comment(&mut mask, i, i + 4);
                        i += 4;
                    } else if lg.hash_line() && c == b'#' {
                        let is_comment = match lg {
                            Lang::Php => next != b'[',
                            Lang::Shell => {
                                i == 0
                                    || matches!(
                                        prev,
                                        b' ' | b'\t' | b';' | b'|' | b'&' | b'(' | b'`'
                                    )
                            }
                            Lang::Yaml => i == 0 || prev == b' ' || prev == b'\t',
                            _ => true,
                        };
                        if is_comment {
                            comment(&mut mask, i, n);
                            i = n;
                        } else {
                            i += 1;
                        }
                    } else if lg == Lang::Ruby && c == b'%' && ruby_percent_open(b, i).is_some() {
                        let (len, open, close, interp) = ruby_percent_open(b, i).expect("checked");
                        string(&mut mask, i, i + len);
                        state = State::Percent {
                            open,
                            close,
                            depth: 0,
                            interp,
                        };
                        i += len;
                    } else if c == b'<'
                        && next == b'<'
                        && ((lg == Lang::Ruby && ruby_heredoc_open(b, i).is_some())
                            || (lg == Lang::Php
                                && b.get(i + 2) == Some(&b'<')
                                && php_heredoc_open(b, i).is_some()))
                    {
                        let (len, h) = if lg == Lang::Ruby {
                            ruby_heredoc_open(b, i)
                        } else {
                            php_heredoc_open(b, i)
                        }
                        .expect("checked");
                        string(&mut mask, i, i + len);
                        pending.push(h);
                        i += len;
                    } else if lg == Lang::Vb && c == b'\'' {
                        comment(&mut mask, i, n);
                        i = n;
                    } else if matches!(lg, Lang::Lua | Lang::Sql) && c == b'-' && next == b'-' {
                        if lg == Lang::Lua {
                            if let Some((level, len)) = lua_long_open(b, i + 2) {
                                let mut term = vec![b']'];
                                term.extend(std::iter::repeat_n(b'=', level));
                                term.push(b']');
                                state = State::Raw {
                                    term,
                                    comment: true,
                                };
                                comment(&mut mask, i, i + 2 + len);
                                i += 2 + len;
                                continue;
                            }
                        }
                        comment(&mut mask, i, n);
                        i = n;
                    } else if lg == Lang::Lua && c == b'[' && lua_long_open(b, i).is_some() {
                        let (level, len) = lua_long_open(b, i).expect("checked");
                        let mut term = vec![b']'];
                        term.extend(std::iter::repeat_n(b'=', level));
                        term.push(b']');
                        state = State::Raw {
                            term,
                            comment: false,
                        };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if lg == Lang::Rust
                        && matches!(c, b'r' | b'b' | b'c')
                        && !(i > 0 && is_ident(prev))
                        && rust_raw_open(b, i).is_some()
                    {
                        let (hashes, len) = rust_raw_open(b, i).expect("checked");
                        let mut term = vec![b'"'];
                        term.extend(std::iter::repeat_n(b'#', hashes));
                        state = State::Raw {
                            term,
                            comment: false,
                        };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if lg == Lang::C
                        && c == b'"'
                        && prev == b'R'
                        && cpp_raw_open(b, i).is_some()
                    {
                        let (term, len) = cpp_raw_open(b, i).expect("checked");
                        state = State::Raw {
                            term,
                            comment: false,
                        };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if lg == Lang::CSharp && c == b'"' && cs_string_open(b, i).is_some() {
                        let (quotes, dollars, len) = cs_string_open(b, i).expect("checked");
                        state = State::CsStr { quotes, dollars };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if (c == b'"' || c == b'\'')
                        && lg.triple(c)
                        && next == c
                        && b.get(i + 2) == Some(&c)
                    {
                        fs = interp_prefix(lg, b, i);
                        state = State::Multi(c);
                        string(&mut mask, i, i + 3);
                        i += 3;
                    } else if c == b'`' && lg.backtick_multi() {
                        fs = false;
                        state = State::Multi(b'`');
                        string(&mut mask, i, i + 1);
                        i += 1;
                    } else if lg == Lang::Shell && c == b'\\' {
                        i += 2;
                    } else if lg == Lang::Rust && c == b'\'' {
                        // `'a` lifetimes are code, `'x'` / `'\n'` / `b'\''` are chars.
                        let lit = if next == b'\\' {
                            b.get(i + 3..)
                                .and_then(|r| r.iter().position(|&x| x == b'\''))
                                .map(|p| p + 4)
                        } else {
                            let w = line[i + 1..].chars().next().map_or(0, char::len_utf8);
                            (w > 0 && b.get(i + 1 + w) == Some(&b'\'')).then_some(w + 2)
                        };
                        match lit {
                            Some(l) => {
                                string(&mut mask, i, i + l);
                                i += l;
                            }
                            None => i += 1,
                        }
                    } else if lg == Lang::C && c == b'\'' && number_before(b, i) {
                        // C++14 digit separator `1'000`.
                        i += 1;
                    } else if (lg == Lang::Js && c == b'/' && regex_allowed(b, i))
                        || (lg == Lang::Ruby && c == b'/' && ruby_regex_allowed(b, i))
                    {
                        match regex_end(b, i) {
                            Some(end) => {
                                string(&mut mask, i, end);
                                i = end;
                            }
                            None => i += 1,
                        }
                    } else if lg == Lang::Yaml
                        && (c == b'"' || c == b'\'')
                        && !(i == 0
                            || matches!(
                                prev,
                                b' ' | b'\t' | b'[' | b'{' | b',' | b':' | b'-' | b'?'
                            ))
                    {
                        // `don't`: an apostrophe inside a plain scalar.
                        i += 1;
                    } else if c == b'"' || c == b'\'' || c == b'`' {
                        fs = interp_prefix(lg, b, i);
                        string(&mut mask, i, i + 1);
                        state = State::Str(c);
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        // Single-line strings end with the line (Rust strings may continue).
        if !(lg == Lang::Rust && state == State::Str(b'"')) {
            if matches!(state, State::Str(_)) {
                state = State::Code;
            }
            if let Some(p) = frames
                .iter()
                .position(|f| matches!(f.resume, State::Str(_)))
            {
                frames.truncate(p);
                if !matches!(state, State::Multi(_) | State::Percent { .. }) {
                    state = State::Code;
                }
            }
        }
        if !pending.is_empty() {
            if state == State::Code {
                state = State::Heredoc(std::mem::take(&mut pending));
            } else {
                pending.clear();
            }
        }
        out.push(mask);
    }
    Some(out)
}

/// C# raw (`"""`, `$$"""`) or verbatim (`@"`, `$@"`, `@$"`) string opener at
/// the quote `i`: (quote count, 0 = verbatim; `$` count; opener length from
/// `i`). `None` for a regular string.
fn cs_string_open(b: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    let quotes = b[i..].iter().take_while(|&&x| x == b'"').count();
    let mut s = i;
    while s > 0 && matches!(b[s - 1], b'$' | b'@') && i - s < 16 {
        s -= 1;
    }
    if s > 0 && is_ident(b[s - 1]) {
        return None;
    }
    let dollars = b[s..i].iter().filter(|&&x| x == b'$').count();
    if quotes >= 3 {
        return Some((quotes, dollars, quotes));
    }
    b[s..i].contains(&b'@').then_some((0, dollars.min(1), 1))
}

/// Rust `r"`, `r#"`, `br#"`, `cr#"`: (hash count, opener length).
fn rust_raw_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if matches!(b[j], b'b' | b'c') {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let mut h = 0;
    while b.get(j) == Some(&b'#') {
        h += 1;
        j += 1;
    }
    (b.get(j) == Some(&b'"')).then_some((h, j + 1 - i))
}

/// C++ `R"delim(`: terminator `)delim"` and opener length (from the quote).
fn cpp_raw_open(b: &[u8], i: usize) -> Option<(Vec<u8>, usize)> {
    let mut j = i + 1;
    while j < b.len() && j - i <= 17 && b[j] != b'(' {
        if matches!(b[j], b' ' | b')' | b'\\' | b'"') {
            return None;
        }
        j += 1;
    }
    if b.get(j) != Some(&b'(') {
        return None;
    }
    let mut term = vec![b')'];
    term.extend_from_slice(&b[i + 1..j]);
    term.push(b'"');
    Some((term, j + 1 - i))
}

/// Is the token ending right before `i` a numeric literal (`1`, `0x1F`)?
fn number_before(b: &[u8], i: usize) -> bool {
    let mut s = i;
    while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'\'') {
        s -= 1;
    }
    s < i && b[s].is_ascii_digit()
}

/// End (exclusive) of a regex literal starting at `i`, if closed on the line.
fn regex_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut class = false;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => {
                j += 1;
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                return (j > i + 2).then_some(j);
            }
            _ => {}
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls(src: &str) -> Vec<String> {
        src.lines().map(str::to_string).collect()
    }

    /// Whether `needle` on line `l` sits on code bytes.
    fn is_code(path: &str, src: &str, l: usize, needle: &str, strings_code: bool) -> bool {
        let lines = ls(src);
        let m = code_mask(path, &lines, strings_code).unwrap();
        let at = lines[l].find(needle).unwrap();
        m[l][at]
    }

    #[test]
    fn go_line_block_and_strings() {
        let src = "SysLog(1)\n// SysLog(2)\nx := \"SysLog(3)\" + SysLog(4)\n/* a\nSysLog(5)\n*/ SysLog(6)\nf() // SysLog(7)\n`raw\nSysLog(8)`\nSysLog(9)";
        assert!(is_code("a.go", src, 0, "SysLog", false));
        assert!(!is_code("a.go", src, 1, "SysLog", false));
        assert!(!is_code("a.go", src, 2, "SysLog", false));
        assert!(is_code("a.go", src, 2, "SysLog(4", false));
        assert!(!is_code("a.go", src, 4, "SysLog", false));
        assert!(is_code("a.go", src, 5, "SysLog", false));
        assert!(!is_code("a.go", src, 6, "SysLog", false));
        assert!(!is_code("a.go", src, 8, "SysLog", false));
        assert!(is_code("a.go", src, 9, "SysLog", false));
        // Strings count as code when the query itself is a string.
        assert!(is_code("a.go", src, 2, "SysLog", true));
    }

    #[test]
    fn rust_nested_blocks_and_lifetimes() {
        let src = "/* a /* b */ still */ run()\nfn f<'a>(x: &'a str) { run() } // run()\nlet c = 'x'; run()";
        assert!(!is_code("a.rs", src, 0, "a /*", false));
        assert!(is_code("a.rs", src, 0, "run", false));
        assert!(is_code("a.rs", src, 1, "run", false));
        assert!(is_code("a.rs", src, 2, "run", false));
    }

    #[test]
    fn python_hash_and_triple_quotes() {
        let src = "x = 1  # call()\n\"\"\"\ncall()\n\"\"\"\ncall()\ns = 'call()'";
        assert!(!is_code("a.py", src, 0, "call", false));
        assert!(!is_code("a.py", src, 2, "call", false));
        assert!(is_code("a.py", src, 4, "call", false));
        assert!(!is_code("a.py", src, 5, "call", false));
    }

    // ---- audit parse-late A1..A5 / search-text #4..#6 regressions ----

    #[test]
    fn shell_hash_is_comment_only_at_word_start() {
        assert!(is_code("a.sh", "n=$#; foo_call", 0, "foo_call", false));
        assert!(is_code(
            "a.sh",
            "echo ${#arr[@]} foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.sh", "x=1 # foo_call", 0, "foo_call", false));
        assert!(!is_code("a.sh", "# foo_call", 0, "foo_call", false));
        // shell single quotes have no escapes
        assert!(is_code(
            "a.sh",
            "echo 'a\\' ; foo_call",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn php_attribute_is_not_a_comment() {
        assert!(is_code(
            "a.php",
            "#[Route('/x')] foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.php", "# foo_call();", 0, "foo_call", false));
        assert!(!is_code("a.php", "// foo_call();", 0, "foo_call", false));
    }

    #[test]
    fn lua_long_comments_and_strings() {
        let src = "--[[ start\nfoo_call()\n]] bar()";
        assert!(!is_code("a.lua", src, 1, "foo_call", false));
        assert!(is_code("a.lua", src, 2, "bar", false));
        let src = "--[==[ a\n]] foo_call()\n]==] real()\nx = [[\nfoo_call()\n]] foo_call()";
        assert!(!is_code("a.lua", src, 1, "foo_call", false));
        assert!(is_code("a.lua", src, 2, "real", false));
        assert!(!is_code("a.lua", src, 4, "foo_call", false));
        assert!(is_code("a.lua", src, 5, "foo_call", false));
        assert!(!is_code("a.lua", "x() -- foo_call", 0, "foo_call", false));
    }

    #[test]
    fn ruby_begin_end_block() {
        let src = "=begin\nfoo_call()\n=end\nfoo_call()";
        assert!(!is_code("a.rb", src, 1, "foo_call", false));
        assert!(is_code("a.rb", src, 3, "foo_call", false));
    }

    #[test]
    fn sql_block_comment_and_yaml_comment() {
        assert!(!is_code(
            "a.sql",
            "/* a\nfoo_call()\n*/",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.sql",
            "select 1; -- foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.sql",
            "select foo_call(1);",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.yaml",
            "k: don't # c\nfoo_call: 1",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.yaml",
            "k: don't # foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.toml", "a = 1 # foo_call", 0, "foo_call", false));
    }

    #[test]
    fn kotlin_swift_scala_nested_block_comments() {
        for p in ["a.kt", "a.swift", "a.scala", "a.dart"] {
            let src = "/* a /* b */ c\nfoo_call()\n*/ d foo_call()";
            assert!(!is_code(p, src, 1, "foo_call", false), "{p}");
            assert!(is_code(p, src, 2, "foo_call", false), "{p}");
        }
        // Java/C do not nest.
        assert!(is_code(
            "a.java",
            "/* a /* b */ foo_call()",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn rust_multiline_and_raw_strings() {
        assert!(!is_code(
            "a.rs",
            "let s = \"abc\nfoo_call()\"; real()",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rs",
            "let s = \"abc\nfoo_call()\"; real()",
            1,
            "real",
            false
        ));
        let raw = "let s = r#\"abc\nfoo_call()\n\"#; real()";
        assert!(!is_code("a.rs", raw, 1, "foo_call", false));
        assert!(is_code("a.rs", raw, 2, "real", false));
        // `r"C:\dir\"` : backslash is not an escape in raw strings.
        assert!(is_code(
            "a.rs",
            "let p = r\"C:\\dir\\\"; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rs",
            "let q = br#\"x\"#; foo_call();",
            0,
            "foo_call",
            false
        ));
        // identifiers ending in r are not raw-string openers
        assert!(is_code(
            "a.rs",
            "for x in y { foo_call(\"a\") }",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rs",
            "let c = b'\\''; foo_call();",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn text_blocks_triple_quotes() {
        assert!(!is_code(
            "A.java",
            "String s = \"\"\"\n foo_call()\n \"\"\"; foo_call()",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "A.java",
            "String s = \"\"\"\n foo_call()\n \"\"\"; foo_call()",
            2,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.kt",
            "val s = \"\"\"\n foo_call()\n\"\"\"",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.swift",
            "let s = \"\"\"\n foo_call()\n\"\"\"",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.cs",
            "var s = \"\"\"\n foo_call()\n\"\"\";",
            1,
            "foo_call",
            false
        ));
    }

    #[test]
    fn cpp_digit_separator_and_raw_string() {
        assert!(is_code(
            "a.cpp",
            "int n = 1'000; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.cpp",
            "int n = 0x1F'00; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.cpp",
            "char c = 'a'; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.cpp",
            "auto s = R\"x(a\nfoo_call()\n)x\"; real();",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.cpp",
            "auto s = R\"x(a\nfoo_call()\n)x\"; real();",
            2,
            "real",
            false
        ));
    }

    #[test]
    fn js_template_interpolation_and_regex() {
        assert!(is_code(
            "a.js",
            "const s = `a ${foo_call()} b`;",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.js",
            "const s = `foo_call ${x} b`;",
            0,
            "foo_call",
            false
        ));
        let src = "const y = `line1\n ${run_f({a: 1})} tail foo_call\n`;\nrun_g();";
        assert!(is_code("a.js", src, 1, "run_f", false));
        assert!(!is_code("a.js", src, 1, "foo_call", false));
        assert!(is_code("a.js", src, 3, "run_g", false));
        assert!(is_code(
            "a.js",
            "const r = /\"/; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.ts",
            "const r = /[/']/g; foo_call();",
            0,
            "foo_call",
            false
        ));
        // division is not a regex
        assert!(is_code(
            "a.js",
            "x = a / 2; y = \"b\"; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.js", "x = /foo_call/;", 0, "foo_call", false));
    }

    #[test]
    fn ruby_regex_literal_vs_division() {
        for src in [
            "x = /#/; foo_call",
            "if /#/ =~ s then foo_call end",
            "s.split(/#/); foo_call",
            "a = [/#/, 1]; foo_call",
            "when /#x/ then foo_call",
            "/#/.match(s); foo_call",
        ] {
            assert!(is_code("a.rb", src, 0, "foo_call", false), "{src}");
        }
        assert!(!is_code("a.rb", "x = /foo_call/", 0, "foo_call", false));
        // division, then a real comment
        assert!(!is_code(
            "a.rb",
            "y = a / b # foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rb",
            "y = a / b; foo_call # c",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn ruby_percent_literals() {
        // masked bodies, including nested delimiters and multi-line ones
        for src in [
            "x = %r{a{2}#foo_call}",
            "x = %w[a [foo_call] b]",
            "x = %q(a (foo_call) b)",
            "x = %Q<a <foo_call> b>",
            "x = %i|foo_call|",
            "puts %(a foo_call)",
            "x = %w[a\nfoo_call b]",
        ] {
            let lines = ls(src);
            let l = lines.iter().position(|l| l.contains("foo_call")).unwrap();
            assert!(!is_code("a.rb", src, l, "foo_call", false), "{src}");
        }
        // code after the literal (and `#` inside it is not a comment)
        for src in [
            "x = %r{#{y}}; foo_call",
            "x = %w[# not a comment]; foo_call",
            "x = %q{a {b} c} + foo_call",
            "x = %w[a\nb]; foo_call",
            "x = %Q(a #{foo_call} b)",
            "x = %r{a#{foo_call(1)}b}i",
        ] {
            let lines = ls(src);
            let l = lines.iter().position(|l| l.contains("foo_call")).unwrap();
            assert!(is_code("a.rb", src, l, "foo_call", false), "{src}");
        }
        // `%q` / `%w` do not interpolate
        assert!(!is_code(
            "a.rb",
            "x = %q(#{foo_call})",
            0,
            "foo_call",
            false
        ));
        // modulo stays an operator
        for src in [
            "y = a % b; foo_call # c",
            "y = a %b; foo_call",
            "y = \"%d\" %[n]; foo_call",
            "y = 10 %(3); foo_call",
            "x %= 3; foo_call",
            "y = a%w; foo_call",
        ] {
            assert!(is_code("a.rb", src, 0, "foo_call", false), "{src}");
        }
        assert!(!is_code(
            "a.rb",
            "y = a % b # foo_call",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn ruby_heredocs() {
        let src = "x = <<~EOS\n  foo_call # not a comment\n  EOS\nrun_g()";
        assert!(!is_code("a.rb", src, 1, "foo_call", false));
        assert!(is_code("a.rb", src, 3, "run_g", false));
        assert!(is_code("a.rb", src, 1, "foo_call", true));
        // `<<-` and bare `<<EOS`; bare needs the terminator at column 0
        let src = "x = <<-EOS\nfoo_call\n    EOS\nrun_g()";
        assert!(!is_code("a.rb", src, 1, "foo_call", false));
        assert!(is_code("a.rb", src, 3, "run_g", false));
        let src = "x = <<EOS\nfoo_call\n  EOS\nfoo_call2\nEOS\nrun_g()";
        assert!(!is_code("a.rb", src, 3, "foo_call2", false));
        assert!(is_code("a.rb", src, 5, "run_g", false));
        // quoted terminators; single quotes do not interpolate
        let src = "x = <<~'EOS'\n  #{foo_call}\nEOS\nrun_g()";
        assert!(!is_code("a.rb", src, 1, "foo_call", false));
        assert!(is_code("a.rb", src, 3, "run_g", false));
        let src = "x = <<~\"EOS\"\n  a #{foo_call(1)} b\nEOS";
        assert!(is_code("a.rb", src, 1, "foo_call", false));
        assert!(!is_code("a.rb", src, 1, " b", false));
        // two heredocs on one line, rest of the opener line is code
        let src = "f(<<~A, <<~B) + run_g()\n  foo_call\nA\n  foo_call2\nB\nrun_h()";
        assert!(is_code("a.rb", src, 0, "run_g", false));
        assert!(!is_code("a.rb", src, 1, "foo_call", false));
        assert!(!is_code("a.rb", src, 3, "foo_call2", false));
        assert!(is_code("a.rb", src, 5, "run_h", false));
        // method call on the opener, and `=begin` inside a body is text
        let src = "x = <<~SQL.strip\n=begin\nSQL\nrun_g()";
        assert!(is_code("a.rb", src, 3, "run_g", false));
        // appends and singleton classes are not heredocs
        for src in [
            "a << b; foo_call",
            "class << self; foo_call",
            "a <<b; foo_call",
            "a<<B; foo_call",
        ] {
            assert!(is_code("a.rb", src, 0, "foo_call", false), "{src}");
        }
        assert!(is_code("a.rb", "a << b\nfoo_call", 1, "foo_call", false));
    }

    #[test]
    fn php_interpolation_and_heredocs() {
        for src in [
            "$s = \"a {$foo_call->x} b\";",
            "$s = \"a {$x[foo_call()]} b\";",
            "$s = \"a ${foo_call} b\";",
        ] {
            assert!(is_code("a.php", src, 0, "foo_call", false), "{src}");
        }
        for src in [
            "$s = \"a { $x } foo_call\";",
            "$s = \"{$x} foo_call\";",
            "$s = 'a {$foo_call}';",
        ] {
            assert!(!is_code("a.php", src, 0, "foo_call", false), "{src}");
        }
        let src = "$s = <<<EOT\n  a {$foo_call} // no comment foo_call2\n  EOT;\nrun_g();";
        assert!(is_code("a.php", src, 1, "foo_call", false));
        assert!(!is_code("a.php", src, 1, "foo_call2", false));
        assert!(is_code("a.php", src, 3, "run_g", false));
        let src = "$s = <<<\"EOT\"\na ${foo_call} b\nEOT;";
        assert!(is_code("a.php", src, 1, "foo_call", false));
        // nowdoc: no interpolation; terminator followed by code
        let src = "f(<<<'EOT'\n{$foo_call}\nEOT, run_g());\nrun_h();";
        assert!(!is_code("a.php", src, 1, "foo_call", false));
        assert!(is_code("a.php", src, 2, "run_g", false));
        assert!(is_code("a.php", src, 3, "run_h", false));
        // the terminator must not be a prefix of a longer identifier
        let src = "$s = <<<EOT\nEOTX foo_call\nEOT;\nrun_g();";
        assert!(!is_code("a.php", src, 1, "foo_call", false));
        assert!(is_code("a.php", src, 3, "run_g", false));
    }

    #[test]
    fn csharp_raw_and_verbatim_strings() {
        // Raw string over lines, with `""` and a lone `\` inside: all string.
        let raw = "var s = \"\"\"\n  say \"\"hi\"\" foo_call() \\\n  \"\"\";\nreal();";
        assert!(!is_code("a.cs", raw, 1, "foo_call", false));
        assert!(is_code("a.cs", raw, 3, "real", false));
        // Four quotes: a `"""` inside does not close it.
        let four = "var s = \"\"\"\"a \"\"\" foo_call()\"\"\"\"; real();";
        assert!(!is_code("a.cs", four, 0, "foo_call", false));
        assert!(is_code("a.cs", four, 0, "real", false));
        // `$$"""`: `{` is content, `{{…}}` is code.
        let dd = "var j = $$\"\"\"{\"k\": {{foo_call()}}, \"v\": {lit_call()}}\"\"\"; real();";
        assert!(is_code("a.cs", dd, 0, "foo_call", false));
        assert!(!is_code("a.cs", dd, 0, "lit_call", false));
        assert!(is_code("a.cs", dd, 0, "real", false));
        // `$"""` multi-line with an interpolation on a later line.
        let d1 = "var s = $\"\"\"\n  a {foo_call()} lit_call()\n  \"\"\"; real();";
        assert!(is_code("a.cs", d1, 1, "foo_call", false));
        assert!(!is_code("a.cs", d1, 1, "lit_call", false));
        assert!(is_code("a.cs", d1, 2, "real", false));
        // Verbatim: `\` is not an escape, `""` is a quote, lines continue.
        let v = "var p = @\"C:\\dir\\\"; real();\nvar q = @\"a \"\"lit_call()\"\"\n lit_call() \"; real();";
        assert!(is_code("a.cs", v, 0, "real", false));
        assert!(!is_code("a.cs", v, 1, "lit_call", false));
        assert!(!is_code("a.cs", v, 2, "lit_call", false));
        assert!(is_code("a.cs", v, 2, "real", false));
        // `$@"…{{lit}}…{x}"` and `@$"`.
        let vi = "var s = @$\"{{lit_call()}} {foo_call()}\n\"; real();";
        assert!(!is_code("a.cs", vi, 0, "lit_call", false));
        assert!(is_code("a.cs", vi, 0, "foo_call", false));
        assert!(is_code("a.cs", vi, 1, "real", false));
        // A normal empty string next to code is untouched.
        assert!(is_code("a.cs", "var e = \"\" + real();", 0, "real", false));
    }

    #[test]
    fn string_interpolation_is_code() {
        let cases: &[(&str, &str)] = &[
            ("a.kt", "val s = \"$x ${foo_call()}\""),
            ("a.kt", "val s = \"${m.map { \"}\" + foo_call() }} tail\""),
            ("a.kt", "val s = \"\"\"raw ${foo_call()} x\"\"\""),
            ("a.swift", "let s = \"a \\(foo_call(1, (2))) b\""),
            (
                "a.swift",
                "let s = \"a \\(x.map { \"\\($0)\" }.foo_call()) b\"",
            ),
            ("a.rb", "s = \"a #{foo_call({a: 1})} b\""),
            ("a.js", "const s = `a ${foo_call()} b`;"),
            ("a.py", "s = f\"a {foo_call()} b\""),
            ("a.py", "s = f'{{lit}} {foo_call(\"x\")}'"),
            ("a.py", "s = rf\"\"\"x {foo_call()}\"\"\""),
            ("a.cs", "var s = $\"a {foo_call()} b\";"),
            ("a.cs", "var s = $@\"a {foo_call(\"x\")} b\";"),
            ("a.scala", "val s = s\"a ${foo_call()} b\""),
            ("a.dart", "var s = 'a ${foo_call()} b';"),
        ];
        for (p, src) in cases {
            assert!(is_code(p, src, 0, "foo_call", false), "{p}: {src}");
        }
        // text outside the interpolation stays a string
        let neg: &[(&str, &str)] = &[
            ("a.kt", "val s = \"foo_call ${x}\""),
            ("a.kt", "val s = \"${x} foo_call\""),
            ("a.swift", "let s = \"foo_call \\(x)\""),
            ("a.rb", "s = \"foo_call #{x}\""),
            ("a.rb", "s = 'a #{foo_call}'"),
            ("a.py", "s = \"{foo_call}\""),
            ("a.py", "s = f\"{{foo_call}} {x}\""),
            ("a.cs", "var s = \"{foo_call}\";"),
            ("a.cs", "var s = $\"{{foo_call}}\";"),
        ];
        for (p, src) in neg {
            assert!(!is_code(p, src, 0, "foo_call", false), "{p}: {src}");
        }
        // code after the string is still lexed (and a trailing comment masked)
        assert!(is_code(
            "a.kt",
            "f(\"${a}\"); foo_call() // c",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.kt",
            "f(\"${a}\") // foo_call()",
            0,
            "foo_call",
            false
        ));
        // multi-line triple string with interpolation
        let src = "val s = \"\"\"\n a ${foo_call()} foo_call2\n\"\"\"\nrun_g()";
        assert!(is_code("a.kt", src, 1, "foo_call()", false));
        assert!(!is_code("a.kt", src, 1, "foo_call2", false));
        assert!(is_code("a.kt", src, 3, "run_g", false));
        // an unterminated interpolation never leaks past its line
        let src = "val s = \"${oops\nrun_g()";
        assert!(is_code("a.kt", src, 1, "run_g", false));
    }

    #[test]
    fn vue_html_comment() {
        assert!(!is_code(
            "a.vue",
            "<!--\nfoo_call()\n-->",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.vue",
            "<!-- c -->foo_call()",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn go_rune_and_vb_comment_and_crlf() {
        assert!(is_code(
            "a.go",
            "c := '\"'; foo_call()",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.vb", "x = 1 ' foo_call()", 0, "foo_call", false));
        assert!(is_code("a.vb", "x = foo_call(\"a\")", 0, "foo_call", false));
        let lines = vec!["foo_call() // c\r".to_string()];
        let m = code_mask("a.go", &lines, false).unwrap();
        assert!(!m[0][lines[0].len() - 1]);
    }

    #[test]
    fn lexer_coverage_is_explicit() {
        for e in [
            "mts",
            "hxx",
            "inl",
            "cu",
            "ipp",
            "rake",
            "gemspec",
            "bats",
            "ksh",
            "lua",
            "pyi",
            "zsh-theme",
            "vb",
            "csx",
            "kts",
            "sql",
            "yaml",
            "toml",
        ] {
            assert!(has_lexer(&format!("a.{e}")), ".{e}");
        }
        for e in ["hs", "pl", "cshtml", "razor", "txt", "md"] {
            assert!(!has_lexer(&format!("a.{e}")), ".{e}");
        }
        assert_eq!(type_label("x/a.HS"), ".hs");
    }

    #[test]
    fn unknown_language_has_no_mask() {
        assert!(code_mask("a.zzz", &ls("x"), false).is_none());
    }
}
