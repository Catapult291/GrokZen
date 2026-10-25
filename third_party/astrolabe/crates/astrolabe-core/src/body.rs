//! 符号体提取（symbol body extraction）。
//!
//! 这是 Serena `include_body` 能力在 core 层的基石：[`crate::types::CodeSymbol`]
//! 只携带 `start_line`/`end_line`（1-based 含端点），本模块负责把这对行号变成
//! 可直接塞进 agent 上下文的源码文本——agent 不必再为看一个函数 Read 整个文件。
//!
//! MCP 工具层的接线（工具命名、参数校验、按 `RepoIndex` 把 `FileId` 还原成
//! 绝对路径）由主线另行完成，本模块只提供与文件系统直接打交道的两个纯函数。
//!
//! 输出格式：每行带 `"{line:>width$} | "` 前缀，与 Claude Code Read 工具的
//! 行号展示习惯一致，方便 agent 对照符号锚点；`width = max(5, end_line 位数 + 1)`，
//! 保证小行号右对齐、5 位数行号也不会挤压内容。

use std::fs::File;
use std::path::Path;

use crate::types::CodeSymbol;

/// `end_line` 的十进制位数；0 记作 1 位。
fn digits_of(n: u32) -> usize {
    if n == 0 {
        1
    } else {
        n.ilog10() as usize + 1
    }
}

/// 行号前缀宽度：`max(5, digits + 1)`。5 是常态下限（个位数到 4 位数行号都
/// 用宽 5），一旦行号本身有 5 位就放宽到位数 + 1，与 ` | ` 分隔符留出间距。
fn prefix_width(end_line: u32) -> usize {
    std::cmp::max(5, digits_of(end_line) + 1)
}

/// 按行范围提取源码文本（符号体）。1-based 闭区间 [start_line, end_line]，
/// 与 CodeSymbol 字段语义一致。行号越界时钳制到文件实际范围（返回空字符串
/// 当 start > 文件行数）。返回带行号前缀的文本，格式 `"{line:>5} | {text}"`
/// （与 Claude Code Read 工具的行号展示习惯一致，方便 agent 对照锚点）。
/// file_abs: 文件绝对路径。
///
/// 行为细节：
/// - 经 `text::read_source` 整体读入并解码（与索引一致，支持 GBK/Latin1），
///   只收集目标区间；越过 `end_line` 后立即停止。
/// - 输出统一用 `\n` 行尾（源文件的 `\r\n` 被归一）；源文件最后一行没有
///   换行符也接受，输出中每一行（含最后一行）都以 `\n` 结尾。
/// - `start_line > end_line` 视为空区间，返回空字符串；但仍会 `open` 文件，
///   因此缺失文件同样返回 `Err`（与正常区间一致，调用方不必特判）。
/// - 文件行数 < `start_line` 时返回 `Ok(String::new())`；文件不存在或读取
///   失败返回 `Err`，由调用方决定降级策略。
pub fn extract_lines(file_abs: &Path, start_line: u32, end_line: u32) -> std::io::Result<String> {
    if start_line > end_line {
        // 仍 open：倒挂区间不掩盖"文件不存在"。
        let _ = File::open(file_abs)?;
        return Ok(String::new());
    }
    let width = prefix_width(end_line);
    // 与索引同一条解码路径（GBK/Latin1/BOM），行切分与符号行号一致；
    // 非 UTF-8 文件不再整体失败。
    let source = crate::text::read_source(file_abs)?;

    let mut out = String::new();
    for (idx, text) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        if line_no > end_line {
            // 已越过窗口右端，无需再处理后面的行。
            break;
        }
        if line_no >= start_line {
            // 写入 String 不会失败，忽略 Result 是安全的。
            let _ =
                std::fmt::Write::write_fmt(&mut out, format_args!("{line_no:>width$} | {text}\n"));
        }
    }
    Ok(out)
}

/// 符号体提取：按 symbol.start_line..end_line 读文件。文件缺失/被删返回
/// Err（调用方决定降级）。
///
/// 直接委托 [`extract_lines`]；`debug_assert` 检查 `start_line <= end_line`，
/// 捕获解析器产出倒挂行号这类契约破坏（release 下按空区间处理，不 panic）。
pub fn symbol_body(file_abs: &Path, symbol: &CodeSymbol) -> std::io::Result<String> {
    debug_assert!(
        symbol.start_line <= symbol.end_line,
        "symbol {} has inverted line range: {} > {}",
        symbol.name,
        symbol.start_line,
        symbol.end_line
    );
    extract_lines(file_abs, symbol.start_line, symbol.end_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileId, SymbolId, SymbolKind};

    /// 唯一临时目录：pid + 纳秒时钟，避免并行测试互踩。用完整个删掉。
    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astrolabe-body-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_file(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// CodeSymbol 字段齐全地手工构造——它没有 derive Default。
    fn symbol(start_line: u32, end_line: u32) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(0),
            file: FileId(0),
            name: "greet".to_string(),
            kind: SymbolKind::Function,
            signature: "fn greet()".to_string(),
            start_line,
            end_line,
            exported: true,
        }
    }

    #[test]
    fn middle_range_is_prefixed_line_by_line() {
        let dir = scratch("mid");
        let path = write_file(&dir, "a.txt", "one\ntwo\nthree\nfour\nfive\nsix\n");

        // 提取 2..=4：逐字符断言，覆盖行号前缀格式与 \n 行尾。
        let got = extract_lines(&path, 2, 4).unwrap();
        let want = "    2 | two\n    3 | three\n    4 | four\n";
        assert_eq!(got, want);
        assert!(got.starts_with("    2 | two\n"), "prefix format: {got:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn first_line_only() {
        let dir = scratch("first");
        let path = write_file(&dir, "a.txt", "alpha\nbeta\ngamma\n");

        let got = extract_lines(&path, 1, 1).unwrap();
        assert_eq!(got, "    1 | alpha\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn end_line_clamps_to_eof() {
        let dir = scratch("clamp");
        // 最后一行故意不带换行符：也必须被接受并归一为 \n 结尾。
        let path = write_file(&dir, "a.txt", "l1\nl2\nl3");

        let got = extract_lines(&path, 2, 99).unwrap();
        assert_eq!(got, "    2 | l2\n    3 | l3\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn start_past_eof_returns_empty() {
        let dir = scratch("past");
        let path = write_file(&dir, "a.txt", "only\n");

        let got = extract_lines(&path, 10, 12).unwrap();
        assert_eq!(got, "", "start 超过文件行数应返回空串");
        // 倒挂区间同样按空处理。
        assert_eq!(extract_lines(&path, 3, 1).unwrap(), "");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn symbol_body_extracts_symbol_range() {
        let dir = scratch("sym");
        let path = write_file(
            &dir,
            "a.rs",
            "mod m {\n    fn greet() {\n        todo!()\n    }\n}\n",
        );

        let sym = symbol(2, 3);
        let got = symbol_body(&path, &sym).unwrap();
        assert_eq!(got, "    2 |     fn greet() {\n    3 |         todo!()\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_err_for_both_entry_points() {
        let dir = scratch("missing");
        let ghost = dir.join("nope.txt");
        assert!(extract_lines(&ghost, 1, 5).is_err());
        assert!(symbol_body(&ghost, &symbol(1, 5)).is_err());
        // 倒挂区间同样先 open，缺失文件不得伪装成 Ok("")。
        // （不经 symbol_body：其 debug_assert 会在测试构建下先拦倒挂行号。）
        assert!(extract_lines(&ghost, 5, 1).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefix_width_stays_five_until_five_digit_lines() {
        let dir = scratch("width");

        // 单行文件、end_line=1：宽度取下限 5（"    1 | "）。
        let small = write_file(&dir, "small.txt", "solo\n");
        let got = extract_lines(&small, 1, 1).unwrap();
        assert_eq!(got, "    1 | solo\n");
        assert!(got.starts_with("    1 | "), "width 5: {got:?}");

        // 5 位数行号：宽度升到 6，且同一窗口内各行对齐。
        let big_body: String = (0..10_000).map(|i| format!("line-{i}\n")).collect();
        let big = write_file(&dir, "big.txt", &big_body);
        let got = extract_lines(&big, 9_999, 10_000).unwrap();
        assert_eq!(
            got, "  9999 | line-9998\n 10000 | line-9999\n",
            "宽度 6 且右对齐: {got:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // 非 UTF-8（GBK）文件：与索引同一解码，前面的非 UTF-8 行也不影响（审计 parse-front #3）
    #[test]
    fn gbk_file_decodes_like_index() {
        let d = scratch("gbk");
        let (bytes, _, _) =
            encoding_rs::GBK.encode("// 中文注释，测试\nint f() { return 1; }\nint g;\n");
        let p = d.join("a.c");
        std::fs::write(&p, bytes.as_ref()).unwrap();
        let out = extract_lines(&p, 2, 3).unwrap();
        assert!(out.contains("int f()"));
        assert!(out.contains("int g;"));
        let first = extract_lines(&p, 1, 1).unwrap();
        assert!(first.contains("中文注释"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
