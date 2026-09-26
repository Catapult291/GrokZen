//! Non-interactive dump of the question card, for layout review without a TTY.
//!
//! Usage: `cargo run -p xai-grok-pager --example question_view_dump -- [scenario] [cursor] [focused] [w] [h] [tab]`
//!
//! Prints the rendered rows as plain text plus a per-row background swatch, so the
//! cursor highlight and the card's float can be checked from a log.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use xai_grok_pager::locale::{LocaleContext, LocaleSource, ResolvedLocale, UiLocale};
use xai_grok_pager::theme::Theme;
use xai_grok_pager::views::prompt_widget::StashedPrompt;
use xai_grok_pager::views::question_view::{
    QUESTION_VIEW_HPAD, QuestionViewState, question_view_height, render_question_view_with_placeholder,
};
use xai_grok_tools::implementations::grok_build::ask_user_question::{Question, QuestionOption};

fn opt(label: &str, description: &str) -> QuestionOption {
    QuestionOption {
        label: label.into(),
        description: description.into(),
        preview: None,
        id: None,
    }
}

fn scenarios() -> Vec<(&'static str, Vec<Question>)> {
    vec![
        (
            "single-select 2 options, long question (the reported case)",
            vec![
                Question {
                    question: "现在仍在运行的是旧版 WezTerm 的两个窗口（含两个 Grok 会话）。关闭它们会结束这两个 Grok 会话。是否现在关闭？".into(),
                    options: vec![
                        opt("暂时不关，我自己找时间关（推荐）", "保留现有两个 Grok 会话不动；你稍后手动关闭它们"),
                        opt("现在就关", "立即结束两个 Grok 会话并关闭两个 WezTerm 窗口"),
                    ],
                    multi_select: None,
                    id: None,
                },
                Question {
                    question: "关闭时要一并保存未提交的改动吗？".into(),
                    options: vec![opt("先保存再关", "写入磁盘后关闭"), opt("直接关", "丢弃未提交的改动")],
                    multi_select: None,
                    id: None,
                },
            ],
        ),
        (
            "multi-select 4 options + freeform",
            vec![Question {
                question: "你平时主要用什么技术栈写代码？".into(),
                options: vec![
                    opt("Python", "数据分析、脚本、AI 相关"),
                    opt("JavaScript/TypeScript", "前端、Node.js、全栈"),
                    opt("Go/Rust", "后端服务、系统级开发"),
                    opt("Java/Kotlin", "企业级应用、Android"),
                ],
                multi_select: Some(true),
                id: None,
            }],
        ),
        (
            "single option, no description",
            vec![Question {
                question: "继续吗？".into(),
                options: vec![opt("继续", "")],
                multi_select: None,
                id: None,
            }],
        ),
        (
            "long descriptions that must wrap",
            vec![Question {
                question: "这次改动会影响以下模块的处理路径，请确认影响范围是否正确，尤其是那些依赖旧的默认值并且在升级之后不会自动迁移的历史配置项。".into(),
                options: vec![
                    opt(
                        "迁移全部历史配置",
                        "把所有旧格式的配置项一次性改写成新格式，过程中会写入一份备份文件；如果目标目录没有写权限，这一步会失败并且不会改动任何现有配置。",
                    ),
                    opt("保持旧格式", "不改写，只在新代码里兼容读取旧格式；代价是每次启动都要多做一次解析。"),
                ],
                multi_select: None,
                id: None,
            }],
        ),
    ]
}

fn arg<T: std::str::FromStr>(idx: usize, default: T) -> T {
    std::env::args()
        .nth(idx)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let all = scenarios();
    let idx = arg::<usize>(1, 0).min(all.len() - 1);
    let cursor = arg::<usize>(2, 0);
    let focused = arg::<bool>(3, true);
    let w = arg::<u16>(4, 120);
    let h = arg::<u16>(5, 40);
    let tab = arg::<usize>(6, 0);
    let scroll = arg::<usize>(7, 0);

    let (name, questions) = all[idx].clone();
    let mut state = QuestionViewState::new("dump".into(), questions, StashedPrompt::default());
    state.set_cursor(cursor);
    if tab < state.questions.len() {
        state.active_tab = tab;
    }
    if scroll > 0 {
        state.per_question_scroll[state.active_tab] = scroll as u16;
    }

    let theme = Theme::default();
    let locale = LocaleContext::new(ResolvedLocale {
        locale: UiLocale::ZhCn,
        source: LocaleSource::ProductDefault,
    });

    // The card is laid out in the same bottom-panel rect the pager gives it.
    let screen = Rect::new(0, 0, w, h);
    let content_w = w.saturating_sub(QUESTION_VIEW_HPAD) as usize;
    let qv_h = question_view_height(&mut state, h.saturating_sub(8), content_w);
    let area = Rect {
        x: 0,
        y: h.saturating_sub(qv_h),
        width: w,
        height: qv_h,
    };

    let mut buf = Buffer::empty(screen);
    buf.set_style(screen, ratatui::style::Style::default().bg(theme.bg_base));
    let rr = render_question_view_with_placeholder(
        &mut buf,
        area,
        &state,
        None,
        &theme,
        focused,
        "直接输入你的答案…",
        Some(&locale),
    );
    // Same scrollbar over the same region the pager uses, so an overflowing list can be checked.
    let res = xai_grok_pager::views::question_view::render_question_scrollbar(
        &mut buf,
        w - 1,
        &state,
        &theme,
        (rr.options_start_y, rr.options_end_y),
    );

    println!(
        "scenario = {} | cursor={} focused={} {}x{} tab={} -> card rows {}..{} scrollbar={:?}",
        name,
        cursor,
        focused,
        w,
        h,
        state.active_tab,
        area.y,
        area.y + area.height,
        res.map(|r| (r.y, r.height)),
    );
    println!("{}", "-".repeat(w as usize));
    for y in 0..h {
        // ratatui fills the cell after a double-width grapheme with a reset space; drop just
        // those continuation cells so the dumped row matches what the terminal shows.
        let mut row = String::new();
        let mut prev_was_wide = false;
        for x in 0..w {
            let sym = buf.cell((x, y)).map_or("", |c| c.symbol());
            if prev_was_wide && sym == " " {
                prev_was_wide = false;
                continue;
            }
            prev_was_wide = unicode_width::UnicodeWidthStr::width(sym) > 1;
            row.push_str(sym);
        }
        println!("{row}");
        println!("{:>4} {}", y, swatch(&buf, y, w));
    }
}

/// The row's dominant background, so a highlighted cursor row is visible in plain text.
fn swatch(buf: &Buffer, y: u16, w: u16) -> String {
    let mut counts: Vec<(Color, usize)> = Vec::new();
    for x in 0..w {
        let c = buf.cell((x, y)).map_or(theme_bg_fallback(), |c| c.bg);
        match counts.iter_mut().find(|(col, _)| *col == c) {
            Some((_, n)) => *n += 1,
            None => counts.push((c, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    counts
        .into_iter()
        .take(2)
        .map(|(c, n)| format!("{}x{}", name(c), n))
        .collect::<Vec<_>>()
        .join(" ")
}

fn theme_bg_fallback() -> Color {
    Color::Reset
}

fn name(c: Color) -> String {
    match c {
        Color::Reset => "reset".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => format!("idx{i}"),
        other => format!("{other:?}"),
    }
}
