//! `/fork` fork-point picker: an overlay listing the conversation's user prompts.
//!
//! Picking a prompt forks the session the way it stood *before* that prompt and hands its text to the
//! child's composer, so the user can rewrite it and take a different path (pi-style branch-and-edit).
//! The last row keeps the whole conversation, which is what `/fork` did before the picker existed, so
//! Enter on the default cursor still forks at the current state.
//!
//! Unlike `/rewind` nothing is fetched and nothing is mutated: rows come from the rendered scrollback.
//! Chrome, row geometry, and hit-testing come from [`crate::views::overlay_list::ListOverlay`]
//! (shared with the rewind and `/jump` pickers).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::line_utils::truncate_str;
use crate::scrollback::entry::EntryId;
use crate::theme::Theme;
use crate::views::jump::JumpRestore;
use crate::views::overlay_list::ListOverlay;

/// One pickable fork point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkPointRow {
    /// Fork the conversation before this prompt.
    Before {
        /// The prompt's index in the shell's prompt numbering (`0` is the session's first prompt).
        /// The fork copies prompts `0..=prompt_index - 1`, so the wire value is `prompt_index - 1`.
        /// The user-facing position is `prompt_index + 1`; `--at` uses that one-based position.
        prompt_index: usize,
        /// Stable id of the prompt's transcript entry: the preview-scroll target.
        /// Resolved to an index only at the [`crate::scrollback::state::ScrollbackState`] boundary,
        /// so a removal can't make a stale index target another block.
        entry_id: EntryId,
        preview: String,
        /// Full prompt text, handed to the child's composer so the user can edit it.
        text: String,
    },
    /// Keep the whole conversation: the behaviour `/fork` had before the picker existed.
    Current,
}

impl ForkPointRow {
    /// The `targetPromptIndex` this row forks with, and the prompt text to pre-fill.
    /// `None` index keeps every prompt. The row is before the selected prompt, so
    /// `--at` uses the equivalent one-based position (`prompt_index + 1`).
    pub fn cut(&self) -> (Option<usize>, Option<String>) {
        match self {
            Self::Before {
                prompt_index, text, ..
            } => (
                prompt_index.checked_sub(1),
                (!text.is_empty()).then(|| text.clone()),
            ),
            Self::Current => (None, None),
        }
    }

    fn preview(&self) -> Option<&str> {
        match self {
            Self::Before { preview, .. } => Some(preview),
            Self::Current => None,
        }
    }
}

#[derive(Debug)]
pub struct ForkPickerState {
    /// Rows oldest first, with the "keep everything" row last so the default cursor preserves the
    /// pre-picker `/fork` behaviour.
    pub rows: Vec<ForkPointRow>,
    /// Cursor row.
    pub selected: usize,
    /// Viewport to restore on dismiss.
    pub restore: JumpRestore,
    /// `/fork`'s own args, carried across the picker so the post-selection dispatch keeps them.
    pub worktree_override: Option<bool>,
    pub directive: Option<String>,
}

impl ForkPickerState {
    fn list(&self) -> ListOverlay {
        ListOverlay {
            len: self.rows.len(),
            selected: self.selected,
        }
    }
}

pub enum ForkPickerInput {
    /// Fork with the cut of the row under the cursor and close.
    Select(usize),
    Dismissed,
    MoveUp,
    MoveDown,
    Consumed,
}

pub fn handle_fork_picker_key(state: &ForkPickerState, key: &KeyEvent) -> ForkPickerInput {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return ForkPickerInput::Consumed;
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => ForkPickerInput::MoveDown,
        KeyCode::Char('k') | KeyCode::Up => ForkPickerInput::MoveUp,
        KeyCode::Enter => fork_picker_activate(state),
        KeyCode::Esc => ForkPickerInput::Dismissed,
        _ => ForkPickerInput::Consumed,
    }
}

/// Move the cursor by `delta`, clamped to the row list.
pub fn move_cursor(state: &mut ForkPickerState, delta: i32) {
    if state.rows.is_empty() {
        return;
    }
    let max = state.rows.len() as i32 - 1;
    state.selected = (state.selected as i32 + delta).clamp(0, max) as usize;
}

/// Move the cursor to `idx` (mouse hover/click). Returns `true` on change.
pub fn set_fork_picker_cursor(state: &mut ForkPickerState, idx: usize) -> bool {
    if state.rows.is_empty() {
        return false;
    }
    let new = idx.min(state.rows.len() - 1);
    if state.selected != new {
        state.selected = new;
        true
    } else {
        false
    }
}

/// The activation input for the current cursor row (Enter-equivalent).
pub fn fork_picker_activate(state: &ForkPickerState) -> ForkPickerInput {
    if state.selected < state.rows.len() {
        ForkPickerInput::Select(state.selected)
    } else {
        ForkPickerInput::Consumed
    }
}

/// Hit-test a screen position against the picker's clickable rows.
pub fn fork_picker_row_at(
    state: &ForkPickerState,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<usize> {
    state.list().row_at(area, col, row)
}

pub fn fork_picker_overlay_height(state: &ForkPickerState, screen_h: u16) -> u16 {
    state.list().height(screen_h)
}

pub fn render_fork_picker_overlay(
    buf: &mut Buffer,
    area: Rect,
    state: &ForkPickerState,
    focused: bool,
) {
    render_fork_picker_overlay_with_locale(buf, area, state, focused, None)
}

pub fn render_fork_picker_overlay_with_locale(
    buf: &mut Buffer,
    area: Rect,
    state: &ForkPickerState,
    focused: bool,
    locale: Option<&crate::locale::LocaleContext>,
) {
    let theme = Theme::current();
    let title = locale
        .map(|locale| locale.named_static_text("fork.picker.title", "Fork before which prompt?"))
        .unwrap_or("Fork before which prompt?");
    let current_label = locale
        .map(|locale| {
            locale.named_static_text(
                "fork.picker.current",
                "Keep the whole conversation (current state)",
            )
        })
        .unwrap_or("Keep the whole conversation (current state)");
    let no_preview = locale
        .map(|locale| locale.named_static_text("fork.picker.no_preview", "(no preview)"))
        .unwrap_or("(no preview)");

    state.list().render(buf, area, title, focused, |i, ctx| {
        let entry = &state.rows[i];
        let text_style = Style::default()
            .fg(theme.text_primary)
            .bg(ctx.row_bg)
            .add_modifier(if ctx.is_cursor {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let gut_style = Style::default().fg(theme.gray).bg(ctx.row_bg);
        let (gutter, body) = match entry {
            ForkPointRow::Current => ("\u{2192} ", current_label),
            ForkPointRow::Before { .. } => (
                "\u{00B7} ",
                entry
                    .preview()
                    .filter(|p| !p.is_empty())
                    .unwrap_or(no_preview),
            ),
        };
        Line::from(vec![
            Span::styled(gutter, gut_style),
            Span::styled(
                truncate_str(body, ctx.content_width.saturating_sub(4) as usize),
                text_style,
            ),
        ])
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn row(prompt_index: usize) -> ForkPointRow {
        ForkPointRow::Before {
            prompt_index,
            entry_id: EntryId::new(prompt_index as u64),
            preview: format!("prompt {prompt_index}"),
            text: format!("full text {prompt_index}"),
        }
    }

    fn state(n: usize) -> ForkPickerState {
        let mut rows: Vec<ForkPointRow> = (1..n).map(row).collect();
        rows.push(ForkPointRow::Current);
        ForkPickerState {
            rows,
            selected: 0,
            restore: JumpRestore {
                bookmark: None,
                selected: None,
                follow_mode: false,
            },
            worktree_override: None,
            directive: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        }
    }

    #[test]
    fn keys_map_to_inputs() {
        let s = state(3);
        assert!(matches!(
            handle_fork_picker_key(&s, &key(KeyCode::Char('j'))),
            ForkPickerInput::MoveDown
        ));
        assert!(matches!(
            handle_fork_picker_key(&s, &key(KeyCode::Up)),
            ForkPickerInput::MoveUp
        ));
        assert!(matches!(
            handle_fork_picker_key(&s, &key(KeyCode::Enter)),
            ForkPickerInput::Select(0)
        ));
        assert!(matches!(
            handle_fork_picker_key(&s, &key(KeyCode::Esc)),
            ForkPickerInput::Dismissed
        ));
        assert!(matches!(
            handle_fork_picker_key(&s, &key(KeyCode::Char('x'))),
            ForkPickerInput::Consumed
        ));
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let mut s = state(3);
        move_cursor(&mut s, 1);
        assert_eq!(s.selected, 1);
        move_cursor(&mut s, 10);
        assert_eq!(s.selected, s.rows.len() - 1);
        move_cursor(&mut s, -10);
        assert_eq!(s.selected, 0);

        assert!(set_fork_picker_cursor(&mut s, 2));
        assert!(!set_fork_picker_cursor(&mut s, 2));
        assert!(
            !set_fork_picker_cursor(&mut s, 99),
            "clamps to last (no change)"
        );
        assert_eq!(s.selected, s.rows.len() - 1);
    }

    #[test]
    fn activate_selects_the_row_under_the_cursor() {
        let mut s = state(3);
        s.selected = 2;
        assert!(matches!(
            fork_picker_activate(&s),
            ForkPickerInput::Select(2)
        ));

        let empty = ForkPickerState {
            rows: Vec::new(),
            ..state(1)
        };
        assert!(matches!(
            fork_picker_activate(&empty),
            ForkPickerInput::Consumed
        ));
    }

    #[test]
    fn row_hit_test_maps_to_row_index() {
        let s = state(3);
        // Title at y+1; rows start at y+2 (ListOverlay geometry).
        assert_eq!(fork_picker_row_at(&s, area(), 5, 1), None);
        assert_eq!(fork_picker_row_at(&s, area(), 5, 2), Some(0));
        assert_eq!(fork_picker_row_at(&s, area(), 5, 4), Some(2));
    }

    #[test]
    fn before_row_forks_one_prompt_earlier_and_carries_the_text() {
        let (target, prefill) = row(2).cut();
        assert_eq!(target, Some(1));
        assert_eq!(prefill.as_deref(), Some("full text 2"));
    }

    #[test]
    fn first_prompt_row_is_not_forkable() {
        // prompt 0 has nothing before it; the picker never builds this row, and the cut degrades to
        // "keep everything" rather than underflowing.
        let (target, _) = row(0).cut();
        assert_eq!(target, None);
    }

    #[test]
    fn current_row_keeps_every_prompt() {
        assert_eq!(ForkPointRow::Current.cut(), (None, None));
    }
}
