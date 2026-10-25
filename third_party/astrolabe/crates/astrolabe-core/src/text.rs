//! Source-file decoding.
//!
//! Most repositories are UTF-8, but legacy trees (GBK / Shift-JIS / Latin-1
//! comments and string literals) are common. `read_to_string` rejects them
//! outright, which silently drops the whole file from the index. Here
//! UTF-8 stays the fast path; anything else is BOM-sniffed or detected with
//! `chardetng` and decoded (malformed sequences become U+FFFD).

use std::io;
use std::path::Path;

mod cjk_freq;

/// Legacy classic-Mac files use a lone `\r` as the line terminator. Rust,
/// tree-sitter and ripgrep only split on `\n`, so such a file collapses into a
/// single line. When the text has `\r` but no `\n` at all, rewrite every `\r`
/// to `\n` (same byte length, so offsets are unchanged). `\r\n` and mixed
/// files are left untouched.
pub fn normalize_lone_cr(text: String) -> String {
    if text.contains('\r') && !text.contains('\n') {
        text.replace('\r', "\n")
    } else {
        text
    }
}

/// Byte-level twin of [`normalize_lone_cr`], in place.
pub fn normalize_lone_cr_bytes(bytes: &mut [u8]) {
    if bytes.contains(&b'\r') && !bytes.contains(&b'\n') {
        for b in bytes.iter_mut().filter(|b| **b == b'\r') {
            *b = b'\n';
        }
    }
}

/// Decode raw file bytes to text. Never fails. Lone-`\r` line endings are
/// normalized to `\n` (see [`normalize_lone_cr`]).
pub fn decode_source(bytes: Vec<u8>) -> String {
    normalize_lone_cr(decode_source_raw(bytes))
}

fn decode_source_raw(bytes: Vec<u8>) -> String {
    // UTF-16 text is often valid UTF-8 too (ASCII + NUL), so check its BOM first.
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(&bytes) {
            let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
            return text.into_owned();
        }
    }
    let bytes = match String::from_utf8(bytes) {
        Ok(mut text) => {
            if text.starts_with('\u{feff}') {
                text.drain(..'\u{feff}'.len_utf8());
            }
            return text;
        }
        Err(err) => err.into_bytes(),
    };
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(&bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return text.into_owned();
    }
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(&bytes, true);
    let guess = detector.guess(None, chardetng::Utf8Detection::Deny);
    let encoding = cjk_rescore(&bytes, guess);
    let (text, _, _) = encoding.decode(&bytes);
    text.into_owned()
}

/// chardetng is unreliable on short samples. Two failure modes matter here:
///
/// * a few GBK ideographs in an ASCII file are typically guessed as EUC-KR
///   (or Big5/SJIS/EUC-JP), turning `你好` into Hangul garbage;
/// * the reverse: short Shift_JIS / EUC-JP / EUC-KR files (kana, kanji-only,
///   Hangul, hanja-only) also decode cleanly as GB18030 into *some*
///   ideographs, so "GB18030 decodes to ideographs only" alone is not
///   evidence for Chinese.
///
/// When chardetng picks a non-GB CJK encoding, every CJK candidate that
/// decodes without errors is scored by the share of its letters that are
/// among the most frequent characters of that locale. Real text scores high
/// in its own encoding; mis-decoded text lands on rare characters. GB18030
/// wins ties (the previous behaviour), then chardetng's own guess. A guess of
/// GBK or a non-CJK encoding is returned unchanged.
fn cjk_rescore(
    bytes: &[u8],
    guess: &'static encoding_rs::Encoding,
) -> &'static encoding_rs::Encoding {
    use encoding_rs::{BIG5, EUC_JP, EUC_KR, GB18030, GBK, SHIFT_JIS};
    let cjk_guess = guess == EUC_KR || guess == BIG5 || guess == SHIFT_JIS || guess == EUC_JP;
    // Very short Shift_JIS kanji (`// 使用`) is often guessed as a Windows
    // single-byte code page. Only reconsider those when the non-ASCII bytes
    // come in even-length runs (CJK double-byte shape; accented Latin is
    // usually a lone byte between ASCII letters).
    // Also keep the guess when its decoding is all letters: real Cyrillic,
    // Greek, Hebrew or Arabic words are long high-byte runs too, whereas
    // CJK bytes read as a Windows code page land on symbols (`“`, `–`, `•`).
    let single_byte_guess = !cjk_guess
        && guess != GBK
        && guess != GB18030
        && even_high_byte_runs(bytes)
        && !guess
            .decode_without_bom_handling(bytes)
            .0
            .chars()
            .filter(|c| !c.is_ascii())
            .all(char::is_alphabetic);
    if !cjk_guess && !single_byte_guess {
        return guess;
    }
    // Preference order on equal score: GB18030, the guess, then the rest.
    // Big5 is only a candidate when chardetng itself proposed it.
    let mut order: Vec<&'static encoding_rs::Encoding> = vec![GB18030];
    if cjk_guess {
        order.push(guess);
    }
    for e in [SHIFT_JIS, EUC_JP, EUC_KR] {
        if !order.contains(&e) {
            order.push(e);
        }
    }
    let mut best: Option<(&'static encoding_rs::Encoding, f64)> = None;
    for enc in order {
        let Some(text) = enc.decode_without_bom_handling_and_without_replacement(bytes) else {
            continue;
        };
        let Some((letters, score)) = locale_score(&text, enc) else {
            continue;
        };
        // Leaving a single-byte guess needs every letter to be common.
        if single_byte_guess && (letters < 2 || score < 1.0) {
            continue;
        }
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((enc, score));
        }
    }
    best.map_or(guess, |(e, _)| e)
}

/// True when the input has non-ASCII bytes and each maximal run of them has
/// even length.
fn even_high_byte_runs(bytes: &[u8]) -> bool {
    let mut any = false;
    let mut run = 0usize;
    for &b in bytes.iter().chain(std::iter::once(&0u8)) {
        if b >= 0x80 {
            run += 1;
        } else {
            if run % 2 == 1 {
                return false;
            }
            any |= run > 0;
            run = 0;
        }
    }
    any
}

/// Share of letters (Han, kana, Hangul and anything else that is not
/// ASCII or CJK/general punctuation) that are common for `enc`'s locale.
/// Returns `(letters, share)`; `None` when the text has no letters at all.
fn locale_score(text: &str, enc: &'static encoding_rs::Encoding) -> Option<(u32, f64)> {
    use encoding_rs::{BIG5, EUC_JP, EUC_KR, SHIFT_JIS};
    let sets = freq_sets();
    let (letters, common) = text
        .chars()
        .filter(|c| !c.is_ascii() && !is_cjk_punct(*c))
        .fold((0u32, 0u32), |(n, k), c| {
            let han = matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF);
            let ok = if enc == SHIFT_JIS || enc == EUC_JP {
                matches!(c as u32, 0x3041..=0x30FF) || (han && sets.ja.contains(&c))
            } else if enc == EUC_KR {
                sets.ko.contains(&c) || (han && sets.hant.contains(&c))
            } else if enc == BIG5 {
                han && sets.hant.contains(&c)
            } else {
                han && sets.hans.contains(&c)
            };
            (n + 1, k + u32::from(ok))
        });
    (letters > 0).then(|| (letters, f64::from(common) / f64::from(letters)))
}

fn is_cjk_punct(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x3003 | 0x3008..=0x3011 | 0x3014..=0x301F | 0xFF01..=0xFF0F
        | 0xFF1A..=0xFF20 | 0xFF3B..=0xFF40 | 0xFF5B..=0xFF65 | 0x2010..=0x2027
        | 0x00B7 | 0x30FB)
}

struct FreqSets {
    hans: std::collections::HashSet<char>,
    hant: std::collections::HashSet<char>,
    ja: std::collections::HashSet<char>,
    ko: std::collections::HashSet<char>,
}

fn freq_sets() -> &'static FreqSets {
    static SETS: std::sync::OnceLock<FreqSets> = std::sync::OnceLock::new();
    SETS.get_or_init(|| FreqSets {
        hans: cjk_freq::ZH_HANS.chars().collect(),
        hant: cjk_freq::ZH_HANT.chars().collect(),
        ja: cjk_freq::JA_KANJI.chars().collect(),
        ko: cjk_freq::KO_HANGUL.chars().collect(),
    })
}

/// Read a source file, decoding non-UTF-8 content instead of failing.
pub fn read_source(path: &Path) -> io::Result<String> {
    std::fs::read(path).map(decode_source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passthrough_and_bom_strip() {
        assert_eq!(decode_source(b"fn a() {}".to_vec()), "fn a() {}");
        assert_eq!(decode_source(b"\xEF\xBB\xBFx".to_vec()), "x");
    }

    #[test]
    fn gbk_is_decoded() {
        // "// 中文注释\nint main_func() { return 0; }\n" in GBK.
        let (bytes, _, _) =
            encoding_rs::GBK.encode("// 中文注释，这是一个测试\nint main_func() { return 0; }\n");
        let text = decode_source(bytes.into_owned());
        assert!(text.contains("main_func"));
        assert!(text.contains("中文注释"));
    }

    #[test]
    fn utf16_bom_is_decoded_even_when_ascii_only() {
        let mut le = vec![0xFF, 0xFE];
        for u in "a\nb\n".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_source(le), "a\nb\n");
    }

    #[test]
    fn short_gbk_not_misdetected_as_euc_kr() {
        for src in ["// 你好 SysLog(gbkgo)\n", "x = 1 // 中文\n", "// 你好\n"] {
            let (bytes, _, _) = encoding_rs::GBK.encode(src);
            assert_eq!(decode_source(bytes.into_owned()), src);
        }
    }

    /// Short legacy files whose non-ASCII text is only kana/kanji/Hangul/hanja
    /// used to be forced to GB18030 because they decode cleanly there too.
    #[test]
    fn short_cjk_files_keep_their_own_encoding() {
        use encoding_rs::{EUC_JP, EUC_KR, GBK, SHIFT_JIS};
        let cases: &[(&str, &'static encoding_rs::Encoding)] = &[
            ("// 日本語文字列処理\n", SHIFT_JIS),
            ("// 日本\n", SHIFT_JIS),
            ("x = 1 # 東京都\n", SHIFT_JIS),
            ("// 漢字変換\nint a;\n", SHIFT_JIS),
            ("// こんにちは\n", SHIFT_JIS),
            ("// 設定ファイルを読み込む\n", SHIFT_JIS),
            // Guessed as a Windows code page by chardetng.
            ("// 以下\n", SHIFT_JIS),
            ("// 情報\n", SHIFT_JIS),
            ("// 実装\n", SHIFT_JIS),
            ("// 日本語文字列処理\n", EUC_JP),
            ("// 東京\n", EUC_JP),
            ("// 안녕하세요\n", EUC_KR),
            ("// 한국어 주석입니다\nint a;\n", EUC_KR),
            ("# 사용자 이름\n", EUC_KR),
            // Chinese must not regress.
            ("// 你好\n", GBK),
            ("// 中文\n", GBK),
            ("x = 1 // 中文\n", GBK),
            ("// 计算总和\n", GBK),
            ("// 中文注释，测试\n", GBK),
            ("// 你好 SysLog(gbkgo)\n", GBK),
            ("# 读取配置文件\n", GBK),
            ("// 用户名或密码错误\n", GBK),
        ];
        let mut bad = Vec::new();
        for (src, enc) in cases {
            let (bytes, _, unmappable) = enc.encode(src);
            assert!(!unmappable, "{src}");
            let got = decode_source(bytes.into_owned());
            if got != *src {
                bad.push(format!("{} {src:?} -> {got:?}", enc.name()));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// The single-byte rescue path must not steal real Cyrillic / Hebrew /
    /// Greek / Latin words whose high bytes also form even-length runs.
    #[test]
    fn single_byte_words_not_reread_as_cjk() {
        use encoding_rs::{WINDOWS_1251, WINDOWS_1252, WINDOWS_1253, WINDOWS_1255};
        let cases: &[(&str, &'static encoding_rs::Encoding)] = &[
            ("// број\n", WINDOWS_1251),
            ("// e164: \"E.164 број\",\n", WINDOWS_1251),
            ("// number: \"מספר\",\n", WINDOWS_1255),
            ("// פריט\n", WINDOWS_1255),
            ("# Größe der Datei\n", WINDOWS_1252),
            ("// αριθμός\n", WINDOWS_1253),
        ];
        for (src, enc) in cases {
            let (bytes, _, _) = enc.encode(src);
            let got = decode_source(bytes.into_owned());
            assert!(
                !got.chars()
                    .any(|c| matches!(c as u32, 0x3040..=0x9FFF | 0xAC00..=0xD7A3)),
                "{src:?} -> {got:?}"
            );
        }
    }

    #[test]
    fn valid_utf8_never_transcoded() {
        let s = "// 你好 SysLog\n";
        assert_eq!(decode_source(s.as_bytes().to_vec()), s);
    }

    #[test]
    fn lone_cr_is_normalized_but_crlf_and_mixed_are_not() {
        assert_eq!(decode_source(b"a\rb\rc\r".to_vec()), "a\nb\nc\n");
        assert_eq!(decode_source(b"a\r\nb\r\n".to_vec()), "a\r\nb\r\n");
        assert_eq!(decode_source(b"a\rb\nc".to_vec()), "a\rb\nc");
        let mut le = vec![0xFF, 0xFE];
        for u in "a\rb\r".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_source(le), "a\nb\n");
    }

    #[test]
    fn lone_cr_python_symbols_get_real_lines() {
        let dir = std::env::temp_dir().join(format!("astrolabe-lonecr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("old.py");
        std::fs::write(
            &f,
            "def alpha():\r    return 1\r\rdef beta():\r    return 2\r",
        )
        .unwrap();
        let src = read_source(&f).unwrap();
        let pool = crate::parse::ParserPool::new();
        let p = pool
            .parse(
                crate::types::Language::Python,
                &crate::types::RelPath::new("old.py"),
                &src,
            )
            .unwrap();
        let lines: Vec<(String, u32)> = p
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.start_line))
            .collect();
        assert_eq!(lines, vec![("alpha".into(), 1), ("beta".into(), 4)]);
    }

    #[test]
    fn latin1_is_decoded() {
        let text = decode_source(b"# caf\xE9\ndef hello(): pass\n".to_vec());
        assert!(text.contains("def hello()"));
        assert!(text.contains("caf\u{e9}"));
    }
}
