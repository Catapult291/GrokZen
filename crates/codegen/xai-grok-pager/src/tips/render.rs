//! Tip renderer.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::render::SafeBuf;
use crate::theme::Theme;

/// Compute the number of rows a tip needs when rendered at the given `width`.
pub fn tip_height(width: u16, tip: &str) -> u16 {
    tip_height_with_locale(width, tip, None)
}

/// Locale-aware counterpart used by composition roots. Unknown server-authored
/// tips retain their original body; only stable known tips and the UI prefix
/// are translated.
pub fn tip_height_with_locale(
    width: u16,
    tip: &str,
    locale: Option<&crate::locale::LocaleContext>,
) -> u16 {
    if width == 0 {
        return 0;
    }
    let line = tip_line(tip, locale);
    let line_width = line.width() as u16;
    if line_width <= width {
        1
    } else {
        // Ceiling division — word wrapping may use slightly more rows than
        // a naive character split, but this is a close-enough upper bound.
        (line_width as u32)
            .div_ceil(width as u32)
            .min(u16::MAX as u32) as u16
    }
}

fn localized_tip_body<'a>(
    tip: &'a str,
    locale: Option<&crate::locale::LocaleContext>,
) -> std::borrow::Cow<'a, str> {
    let Some(locale) = locale else {
        return std::borrow::Cow::Borrowed(tip);
    };
    let catalog_id = match tip {
        "Use @ to attach files like @src/main.rs." => Some("tips.attach_files"),
        "Use @! for hidden or ignored files: @!.github/workflows." => Some("tips.hidden_files"),
        "Press Ctrl+O to toggle auto-approve mode." => Some("tips.toggle_auto_approve"),
        "Use Shift+Tab to cycle between modes like Plan mode." => Some("tips.cycle_modes"),
        "Run /compact [context] when chat gets long." => Some("tips.compact_context"),
        "Press Ctrl+B to background a running terminal command." => {
            Some("tips.background_terminal")
        }
        "Start Grok in a fresh worktree with `-w`; add `-r <session-id>` to resume an existing session there." => {
            Some("tips.fresh_worktree_resume")
        }
        "Use Ctrl+Enter to interject messages. Or just Enter to queue messages." => {
            Some("tips.interject_queue")
        }
        "Run /dashboard (or Ctrl+\\) to see and manage all your agents in one place." => {
            Some("tips.dashboard")
        }
        _ => None,
    };
    if let Some(catalog_id) = catalog_id {
        return locale.named_text(catalog_id, tip);
    }
    std::borrow::Cow::Borrowed(tip)
}

fn tip_line(tip: &str, locale: Option<&crate::locale::LocaleContext>) -> Line<'static> {
    let theme = Theme::current();
    let prefix = locale
        .map(|locale| locale.named_text("tips.prefix", "Tip: "))
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("Tip: "));
    let body = localized_tip_body(tip, locale);
    Line::from(vec![
        Span::styled(
            prefix.into_owned(),
            Style::default().fg(theme.gray).add_modifier(Modifier::BOLD),
        ),
        Span::styled(body.into_owned(), Style::default().fg(theme.gray)),
    ])
}

/// Render a tip into the provided area, word-wrapping if it exceeds the width.
pub fn render_tip(area: Rect, buf: &mut Buffer, tip: &str) {
    render_tip_with_locale(area, buf, tip, None);
}

pub fn render_tip_with_locale(
    area: Rect,
    buf: &mut Buffer,
    tip: &str,
    locale: Option<&crate::locale::LocaleContext>,
) {
    if area.height == 0 {
        return;
    }

    let theme = Theme::current();

    Paragraph::new(tip_line(tip, locale))
        .style(Style::default().bg(theme.bg_base))
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

/// Blank every cell of `area` (chars, colors, and modifiers) in `color`.
///
/// Modifiers MUST be reset here: ratatui's `Cell::set_style` only *merges*
/// modifiers (`insert(add)` / `remove(sub)`), so a later paint whose style
/// carries no `sub_modifier` inherits whatever BOLD/ITALIC/… an earlier
/// same-frame paint left behind (e.g. the welcome tip's bold `Tip: ` prefix
/// bleeding into the ephemeral tip as "**Queue**d · Enter to send now").
fn clear_rect(buf: &mut Buffer, area: Rect, color: Color) {
    for row in 0..area.height {
        for col in 0..area.width {
            if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                cell.set_char(' ');
                cell.fg = color;
                cell.bg = color;
                cell.modifier = Modifier::empty();
            }
        }
    }
}

/// Render a pre-styled tip line into the banner rect. The whole rect is
/// cleared first (it can be taller than one row when a wrapped session tip
/// reserved it) and the line paints on the first row, truncated at width.
pub fn render_ephemeral_tip(area: Rect, buf: &mut Buffer, line: &Line<'static>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = Theme::current();
    clear_rect(buf, area, theme.bg_base);
    buf.set_line_safe(area.x, area.y, line, area.width);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zh_cn_locale() -> crate::locale::LocaleContext {
        crate::locale::LocaleContext::new(crate::locale::ResolvedLocale {
            locale: crate::locale::UiLocale::ZhCn,
            source: crate::locale::LocaleSource::Cli,
        })
    }

    fn row_text(buf: &Buffer, area: Rect, y: u16) -> String {
        (0..area.width)
            .map(|x| buf.cell((area.x + x, y)).expect("cell in area").symbol())
            .collect()
    }

    #[test]
    fn clears_full_rect_and_truncates_to_width() {
        let area = Rect::new(0, 0, 8, 2);
        let mut buf = Buffer::empty(area);
        // Pre-dirty both rows to simulate stale banner content underneath.
        buf.set_string(0, 0, "XXXXXXXX", Style::default());
        buf.set_string(0, 1, "XXXXXXXX", Style::default());

        let line = Line::from("0123456789"); // wider than the rect
        render_ephemeral_tip(area, &mut buf, &line);

        assert_eq!(row_text(&buf, area, 0), "01234567", "truncated at width");
        assert_eq!(
            row_text(&buf, area, 1),
            "        ",
            "stale rows below the line are cleared"
        );
    }

    #[test]
    fn zero_sized_area_is_a_noop() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "XXXXXXXX", Style::default());
        render_ephemeral_tip(Rect::new(0, 0, 8, 0), &mut buf, &Line::from("tip"));
        assert_eq!(row_text(&buf, area, 0), "XXXXXXXX", "untouched");
    }

    #[test]
    fn known_attach_files_tip_is_localized_without_rewriting_tokens() {
        let locale = zh_cn_locale();
        let line = tip_line("Use @ to attach files like @src/main.rs.", Some(&locale));
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "提示：可用 @ 附加文件，例如 @src/main.rs。");
    }

    #[test]
    fn localization_regression_compact_tip_unknown_remote_tip_is_opaque() {
        let locale = zh_cn_locale();
        let line = tip_line("Run /compact [context] when chat gets long.", Some(&locale));
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "提示：对话过长时，可运行 /compact [context]。");

        let unknown = tip_line("Review the current policy.", Some(&locale));
        let unknown_text = unknown
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(unknown_text, "提示：Review the current policy.");
    }

    #[test]
    fn current_remote_tip_catalog_is_localized_without_rewriting_shortcuts() {
        let locale = zh_cn_locale();
        let cases = [
            (
                "Use @! for hidden or ignored files: @!.github/workflows.",
                "提示：可用 @! 引用隐藏或被忽略的文件：@!.github/workflows。",
            ),
            (
                "Press Ctrl+O to toggle auto-approve mode.",
                "提示：按 Ctrl+O 切换自动批准模式。",
            ),
            (
                "Use Shift+Tab to cycle between modes like Plan mode.",
                "提示：使用 Shift+Tab 在不同模式之间循环切换，例如计划模式。",
            ),
            (
                "Press Ctrl+B to background a running terminal command.",
                "提示：按 Ctrl+B 将正在运行的终端命令发送到后台。",
            ),
            (
                "Start Grok in a fresh worktree with `-w`; add `-r <session-id>` to resume an existing session there.",
                "提示：使用 `-w` 在全新的工作树中启动 Grok；加上 `-r <session-id>` 可在其中恢复已有会话。",
            ),
            (
                "Use Ctrl+Enter to interject messages. Or just Enter to queue messages.",
                "提示：使用 Ctrl+Enter 插话；也可以直接按 Enter 将消息排入队列。",
            ),
            (
                "Run /dashboard (or Ctrl+\\) to see and manage all your agents in one place.",
                "提示：运行 /dashboard（或按 Ctrl+\\）即可集中查看和管理所有智能体。",
            ),
        ];
        for (english, expected) in cases {
            let text = tip_line(english, Some(&locale))
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(text, expected);
        }
    }

    /// Regression: a bold underpaint in the banner rect (e.g. the welcome
    /// tip's `Tip: ` prefix painted the same frame) must not bleed BOLD into
    /// the ephemeral tip. `Cell::set_style` merges modifiers, so the clear
    /// pass has to reset them explicitly — otherwise `Queued · Enter …`
    /// rendered as bold `Queue` + regular `d` (5 leaked bold cells).
    #[test]
    fn clears_leaked_modifiers_from_underpaint() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        // Simulate the phantom session-tip underpaint: 5 bold cells ("Tip: ").
        buf.set_string(
            0,
            0,
            "Tip: never gonna give you up",
            Style::default().add_modifier(Modifier::BOLD),
        );

        // The send-now tip shape: dim text with a single bold key chord.
        let dim = Style::default();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::styled("Queued · ", dim),
            Span::styled("Enter", bold),
            Span::styled(" to send now", dim),
        ]);
        render_ephemeral_tip(area, &mut buf, &line);

        assert_eq!(
            row_text(&buf, area, 0).trim_end(),
            "Queued · Enter to send now"
        );
        let bold_cols: Vec<u16> = (0..area.width)
            .filter(|&x| {
                buf.cell((x, 0))
                    .expect("cell in area")
                    .modifier
                    .contains(Modifier::BOLD)
            })
            .collect();
        // "Queued · " occupies cols 0..9, "Enter" cols 9..14.
        assert_eq!(
            bold_cols,
            (9..14).collect::<Vec<u16>>(),
            "only the Enter chord may be bold — no leak from the underpaint"
        );
    }
}
