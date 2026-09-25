use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::views::jump::JumpRestore;
use crate::views::prompt_widget::StashedPrompt;

fn rewind_static(
    locale: Option<&crate::locale::LocaleContext>,
    id: &str,
    english: &'static str,
) -> &'static str {
    locale
        .map(|locale| locale.named_static_text(id, english))
        .unwrap_or(english)
}

fn rewind_text(locale: Option<&crate::locale::LocaleContext>, id: &str, english: &str) -> String {
    locale
        .map(|locale| locale.named_text(id, english).into_owned())
        .unwrap_or_else(|| english.to_owned())
}

fn localized_checkpoint_replay_error(
    locale: Option<&crate::locale::LocaleContext>,
    message: &str,
) -> String {
    const SAFETY_SUFFIX: &str = ". Cannot safely rewind past the compaction point.";

    if let Some(path) = message
        .strip_prefix("Compaction checkpoint file missing: ")
        .and_then(|value| value.strip_suffix(SAFETY_SUFFIX))
    {
        return rewind_text(
            locale,
            "rewind.error.checkpoint_missing",
            "Compaction checkpoint file missing: {path}. Cannot safely rewind past the compaction point.",
        )
        .replace("{path}", path);
    }

    if let Some(path) = message
        .strip_prefix("Compaction checkpoint file corrupt: ")
        .and_then(|value| value.strip_suffix(SAFETY_SUFFIX))
    {
        return rewind_text(
            locale,
            "rewind.error.checkpoint_corrupt",
            "Compaction checkpoint file corrupt: {path}. Cannot safely rewind past the compaction point.",
        )
        .replace("{path}", path);
    }

    if let Some(schema_version) = message
        .strip_prefix("Unsupported checkpoint schema version ")
        .and_then(|value| value.strip_suffix(SAFETY_SUFFIX))
    {
        return rewind_text(
            locale,
            "rewind.error.checkpoint_schema_unsupported",
            "Unsupported checkpoint schema version {schema_version}. Cannot safely rewind past the compaction point.",
        )
        .replace("{schema_version}", schema_version);
    }

    message.to_owned()
}

fn localized_rewind_error(locale: Option<&crate::locale::LocaleContext>, message: &str) -> String {
    let Some(locale) = locale.filter(|locale| locale.locale() == crate::locale::UiLocale::ZhCn)
    else {
        return message.to_owned();
    };
    let locale = Some(locale);

    const PROMPT_PREFIX: &str = "Cannot rewind to prompt #";
    const COMPACTION_MIDDLE: &str = " — compaction checkpoint data is unavailable (";
    const COMPACTION_SUFFIX: &str =
        "). Try rewinding to a prompt after the compaction point instead.";
    const INDEX_MIDDLE: &str = " — current prompt index is ";
    const TARGETS_MIDDLE: &str = ". Valid targets: 0..";

    if let Some(rest) = message.strip_prefix(PROMPT_PREFIX) {
        if let Some((prompt_index, detail_with_suffix)) = rest.split_once(COMPACTION_MIDDLE)
            && let Some(detail) = detail_with_suffix.strip_suffix(COMPACTION_SUFFIX)
        {
            let localized_detail = localized_checkpoint_replay_error(locale, detail);
            return rewind_text(
                locale,
                "rewind.error.compaction_unavailable",
                "Cannot rewind to prompt #{prompt_index} — compaction checkpoint data is unavailable ({detail}). Try rewinding to a prompt after the compaction point instead.",
            )
            .replace("{prompt_index}", prompt_index)
            .replace("{detail}", &localized_detail);
        }

        if let Some((prompt_index, index_and_targets)) = rest.split_once(INDEX_MIDDLE)
            && let Some((current_index, last_valid_index)) =
                index_and_targets.split_once(TARGETS_MIDDLE)
        {
            return rewind_text(
                locale,
                "rewind.error.invalid_target",
                "Cannot rewind to prompt #{prompt_index} — current prompt index is {current_index}. Valid targets: 0..{last_valid_index}",
            )
            .replace("{prompt_index}", prompt_index)
            .replace("{current_index}", current_index)
            .replace("{last_valid_index}", last_valid_index);
        }
    }

    match message {
        "External modifications detected. Confirm to revert anyway." => rewind_text(
            locale,
            "rewind.error.external_modifications",
            "External modifications detected. Confirm to revert anyway.",
        ),
        "unknown error" => rewind_text(locale, "rewind.error.unknown", "unknown error"),
        _ => message.to_owned(),
    }
}

fn localized_conflict_label<'a>(
    locale: Option<&crate::locale::LocaleContext>,
    label: &'a str,
) -> &'a str {
    let Some(locale) = locale else {
        return label;
    };
    let id = match label {
        "deleted" => "rewind.conflict.deleted",
        "added" => "rewind.conflict.added",
        "modified" => "rewind.conflict.modified",
        "conflict" => "rewind.conflict.conflict",
        _ => return label,
    };
    locale.named_static_text(
        id,
        match label {
            "deleted" => "deleted",
            "added" => "added",
            "modified" => "modified",
            _ => "conflict",
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RewindPointInfo {
    #[serde(alias = "promptIndex")]
    pub prompt_index: usize,
    #[serde(default, alias = "createdAt")]
    pub created_at: String,
    #[serde(default, alias = "numFileSnapshots")]
    pub num_file_snapshots: usize,
    #[serde(default, alias = "promptPreview")]
    pub prompt_preview: Option<String>,
    #[serde(default, alias = "hasFileChanges")]
    pub has_file_changes: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RewindPointsResponse {
    #[serde(alias = "rewindPoints")]
    pub rewind_points: Vec<RewindPointInfo>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RewindResponse {
    pub success: bool,
    #[serde(alias = "targetPromptIndex")]
    pub target_prompt_index: usize,
    #[serde(default, alias = "revertedFiles")]
    pub reverted_files: Vec<String>,
    #[serde(default, alias = "cleanFiles")]
    pub clean_files: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<RewindConflictInfo>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default, alias = "promptText")]
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RewindConflictInfo {
    pub path: String,
    #[serde(alias = "conflictType")]
    pub conflict_type: String,
}

/// Which halves of the session a rewind touches, for the confirm dialog.
/// The wire values are the shell's `RewindMode` serde names (`all` / `conversation_only` / `files_only`),
/// so the dialog and the shell can't drift apart silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindMode {
    /// Conversation and files (the shell's protocol default).
    All,
    /// Conversation only; files stay on disk.
    ConversationOnly,
    /// Files only; the conversation keeps every turn.
    FilesOnly,
}

impl RewindMode {
    pub const fn wire(self) -> &'static str {
        match self {
            RewindMode::All => "all",
            RewindMode::ConversationOnly => "conversation_only",
            RewindMode::FilesOnly => "files_only",
        }
    }
    /// i18n id for the mode row label.
    const fn label_id(self) -> &'static str {
        match self {
            RewindMode::All => "rewind.mode.all",
            RewindMode::ConversationOnly => "rewind.mode.conversation",
            RewindMode::FilesOnly => "rewind.mode.files",
        }
    }
    /// English label used when the locale has no entry.
    const fn label_default(self) -> &'static str {
        match self {
            RewindMode::All => "Conversation and files",
            RewindMode::ConversationOnly => "Conversation only",
            RewindMode::FilesOnly => "Files only",
        }
    }
    /// Confirm-dialog title prefix: states what the cursor row would do.
    const fn title_prefix_id(self) -> &'static str {
        match self {
            RewindMode::All => "rewind.confirm.all_prefix",
            RewindMode::ConversationOnly => "rewind.confirm.conversation_prefix",
            RewindMode::FilesOnly => "rewind.confirm.files_prefix",
        }
    }
    const fn title_prefix_default(self) -> &'static str {
        match self {
            RewindMode::All => "Rewind files and conversation to \u{201C}",
            RewindMode::ConversationOnly => "Rewind conversation only to \u{201C}",
            RewindMode::FilesOnly => "Rewind files only to \u{201C}",
        }
    }
    /// Post-rewind report text.
    pub const fn reverted_id(self) -> &'static str {
        match self {
            RewindMode::All => "rewind.reverted.all",
            RewindMode::ConversationOnly => "rewind.reverted.conversation",
            RewindMode::FilesOnly => "rewind.reverted.files",
        }
    }
    pub const fn reverted_default(self) -> &'static str {
        match self {
            RewindMode::All => "Reverted conversation and files",
            RewindMode::ConversationOnly => "Reverted conversation",
            RewindMode::FilesOnly => "Reverted file changes",
        }
    }
    /// Whether this mode truncates the conversation (and therefore the rendered scrollback).
    pub const fn rewinds_conversation(self) -> bool {
        matches!(self, RewindMode::All | RewindMode::ConversationOnly)
    }
    /// Read the mode back from a `RewindResponse`. An absent or unknown value keeps the
    /// historical conversation-only behaviour, so an older shell can't strand the client.
    pub fn from_wire(value: Option<&str>) -> Self {
        match value {
            Some("files_only") | Some("code_only") => RewindMode::FilesOnly,
            Some("all") => RewindMode::All,
            _ => RewindMode::ConversationOnly,
        }
    }
}

/// Confirm-dialog rows, in order: the three modes. The cursor starts on `All`,
/// matching the protocol default.
pub const CONFIRM_MODES: [RewindMode; 3] = [
    RewindMode::All,
    RewindMode::ConversationOnly,
    RewindMode::FilesOnly,
];

/// Key that activates the mode row at the same index.
pub const MODE_KEYS: [char; 3] = ['1', '2', '3'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindPhase {
    Loading,
    /// Turn picker: one row per rewindable turn, oldest first, so the list reads top-to-bottom in
    /// conversation order (the `/fork` picker's row order). The cursor starts on the newest turn,
    /// the least destructive cut, which is also the row the flow picked before the order changed.
    /// The cursor row's prompt is anchored at the transcript top while the picker is open.
    Picker {
        points: Vec<RewindPointInfo>,
        selected: usize,
    },
    CancelOffer {
        active_idx: usize,
    },
    /// Confirm step: choose what to rewind for the selected turn.
    /// Row order is `CONFIRM_MODES` (the cursor starts on `All`), then a cancel row.
    Confirm {
        target_prompt_index: usize,
        active_idx: usize,
        prompt_preview: Option<String>,
        /// File snapshots recorded at that turn; the files-only title shows the count.
        num_file_snapshots: usize,
    },
    Executing {
        target_prompt_index: usize,
    },
    Error {
        message: String,
    },
}

#[derive(Debug)]
pub struct RewindState {
    pub phase: RewindPhase,
    pub anchor_entry_idx: usize,
    pub stashed_draft: Option<StashedPrompt>,
    pub selected_prompt_index: Option<usize>,
    /// Fixed mode for the whole flow, skipping the mode dialog.
    /// `/undo` sets `ConversationOnly`: files are never touched, and no confirm step is offered.
    pub fixed_mode: Option<RewindMode>,
    /// Viewport the flow opened from, put back when it is dismissed (`Esc`) — the picker's preview
    /// scrolling is a side effect of choosing, not a move the user asked for (the `/jump` and
    /// `/fork` rule). Dropped with the state on the execute path, so it can never go stale.
    pub restore: JumpRestore,
}

impl RewindState {
    pub fn new_cancel_offer(
        anchor: usize,
        draft: Option<StashedPrompt>,
        selected_prompt_index: Option<usize>,
        fixed_mode: Option<RewindMode>,
        restore: JumpRestore,
    ) -> Self {
        Self {
            phase: RewindPhase::CancelOffer { active_idx: 0 },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index,
            fixed_mode,
            restore,
        }
    }
}

pub enum RewindInput {
    Dismissed,
    CancelTurnThenProceed,
    DismissError,
    Confirm(usize, RewindMode),
    /// Execute this rewind with `mode` and turn off confirm-before-rewind.
    ConfirmNeverAsk(usize, RewindMode),
    PickerSelect(usize),
    MoveUp,
    MoveDown,
    ConfirmCursor,
    Consumed,
}

const CANCEL_OFFER_OPTIONS: usize = 2;
/// Conversation+files / conversation only / files only / cancel.
const CONFIRM_OPTIONS: usize = CONFIRM_MODES.len() + 1;

/// Mode under the confirm cursor; `None` on the trailing cancel row.
fn confirm_mode_at(active_idx: usize) -> Option<RewindMode> {
    CONFIRM_MODES.get(active_idx).copied()
}

pub fn handle_rewind_key(state: &RewindState, key: &KeyEvent) -> RewindInput {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return RewindInput::Consumed;
    }
    match &state.phase {
        RewindPhase::Picker { points, selected } => match key.code {
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => {
                if let Some(p) = points.get(*selected) {
                    RewindInput::PickerSelect(p.prompt_index)
                } else {
                    RewindInput::Consumed
                }
            }
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::CancelOffer { .. } => match key.code {
            KeyCode::Char('y') => RewindInput::CancelTurnThenProceed,
            KeyCode::Char('n') => RewindInput::Dismissed,
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => RewindInput::ConfirmCursor,
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Confirm {
            target_prompt_index,
            active_idx,
            ..
        } => match key.code {
            // 1/2/3 pick a mode straight away; y and a act on the highlighted row
            KeyCode::Char('1') => RewindInput::Confirm(*target_prompt_index, CONFIRM_MODES[0]),
            KeyCode::Char('2') => RewindInput::Confirm(*target_prompt_index, CONFIRM_MODES[1]),
            KeyCode::Char('3') => RewindInput::Confirm(*target_prompt_index, CONFIRM_MODES[2]),
            KeyCode::Char('y') => match confirm_mode_at(*active_idx) {
                Some(mode) => RewindInput::Confirm(*target_prompt_index, mode),
                None => RewindInput::Dismissed,
            },
            KeyCode::Char('a') => match confirm_mode_at(*active_idx) {
                Some(mode) => RewindInput::ConfirmNeverAsk(*target_prompt_index, mode),
                None => RewindInput::Dismissed,
            },
            KeyCode::Char('n') => RewindInput::Dismissed,
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => RewindInput::ConfirmCursor,
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Error { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter => RewindInput::DismissError,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Loading => match key.code {
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Executing { .. } => RewindInput::Consumed,
    }
}

pub fn move_cursor(phase: &mut RewindPhase, delta: i32) {
    match phase {
        RewindPhase::Picker { points, selected } => {
            if points.is_empty() {
                return;
            }
            let max = points.len() as i32 - 1;
            let new = (*selected as i32 + delta).clamp(0, max);
            *selected = new as usize;
        }
        RewindPhase::CancelOffer { active_idx } => {
            let new = (*active_idx as i32 + delta).clamp(0, CANCEL_OFFER_OPTIONS as i32 - 1);
            *active_idx = new as usize;
        }
        RewindPhase::Confirm { active_idx, .. } => {
            let new = (*active_idx as i32 + delta).clamp(0, CONFIRM_OPTIONS as i32 - 1);
            *active_idx = new as usize;
        }
        _ => {}
    }
}

pub fn confirm_cursor(phase: &RewindPhase) -> RewindInput {
    match phase {
        RewindPhase::CancelOffer { active_idx } => match active_idx {
            0 => RewindInput::CancelTurnThenProceed,
            _ => RewindInput::Dismissed,
        },
        RewindPhase::Confirm {
            target_prompt_index,
            active_idx,
            ..
        } => match confirm_mode_at(*active_idx) {
            Some(mode) => RewindInput::Confirm(*target_prompt_index, mode),
            None => RewindInput::Dismissed,
        },
        _ => RewindInput::Consumed,
    }
}

/// Hit-test a screen position against the rewind overlay's clickable rows.
///
/// Returns the logical cursor index under `(col, row)` for the current phase, or `None` if the position is not on a selectable row.
///
/// IMPORTANT: the row geometry here mirrors `render_rewind_overlay`.
/// Keep this, `render_rewind_overlay`, and `rewind_overlay_height` in sync when changing layout.
pub fn rewind_row_at(phase: &RewindPhase, area: Rect, col: u16, row: u16) -> Option<usize> {
    if area.height == 0 || area.width < 10 {
        return None;
    }
    if col < area.x || col >= area.x + area.width {
        return None;
    }
    if row < area.y || row >= area.y + area.height {
        return None;
    }
    match phase {
        RewindPhase::Picker { points, selected } => crate::views::overlay_list::ListOverlay {
            len: points.len(),
            selected: *selected,
        }
        .row_at(area, col, row),
        RewindPhase::CancelOffer { .. } => match row.checked_sub(area.y + 3) {
            Some(0) => Some(0),
            Some(1) => Some(1),
            _ => None,
        },
        RewindPhase::Confirm { .. } => {
            // One title row, then the CONFIRM_OPTIONS mode/cancel rows
            match row.checked_sub(area.y + 2) {
                Some(idx) if (idx as usize) < CONFIRM_OPTIONS => Some(idx as usize),
                _ => None,
            }
        }
        RewindPhase::Error { .. } => {
            if row == area.y + 3 {
                Some(0)
            } else {
                None
            }
        }
        RewindPhase::Loading | RewindPhase::Executing { .. } => None,
    }
}

/// Move the overlay cursor/selection to `idx` (used by mouse hover/click).
/// Returns `true` if the stored cursor changed.
pub fn set_rewind_cursor(phase: &mut RewindPhase, idx: usize) -> bool {
    match phase {
        RewindPhase::Picker { points, selected } => {
            if points.is_empty() {
                return false;
            }
            let new = idx.min(points.len() - 1);
            if *selected != new {
                *selected = new;
                true
            } else {
                false
            }
        }
        RewindPhase::CancelOffer { active_idx } => {
            let new = idx.min(CANCEL_OFFER_OPTIONS - 1);
            if *active_idx != new {
                *active_idx = new;
                true
            } else {
                false
            }
        }
        RewindPhase::Confirm { active_idx, .. } => {
            let new = idx.min(CONFIRM_OPTIONS - 1);
            if *active_idx != new {
                *active_idx = new;
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

/// The activation input for the current cursor position, equivalent to pressing Enter on the focused row. Used by mouse-click handling.
pub fn rewind_activate(phase: &RewindPhase) -> RewindInput {
    match phase {
        RewindPhase::Picker { points, selected } => points
            .get(*selected)
            .map(|p| RewindInput::PickerSelect(p.prompt_index))
            .unwrap_or(RewindInput::Consumed),
        RewindPhase::Error { .. } => RewindInput::DismissError,
        other => confirm_cursor(other),
    }
}

pub fn rewind_overlay_height(phase: &RewindPhase, screen_h: u16) -> u16 {
    let content = match phase {
        RewindPhase::Loading => 2,
        RewindPhase::Picker { points, selected } => {
            return crate::views::overlay_list::ListOverlay {
                len: points.len(),
                selected: *selected,
            }
            .height(screen_h);
        }
        RewindPhase::CancelOffer { .. } => 5,
        RewindPhase::Executing { .. } => 2,
        RewindPhase::Confirm { .. } => 6,
        RewindPhase::Error { .. } => 4,
    };
    content + 1
}

pub fn render_rewind_overlay(
    buf: &mut Buffer,
    area: Rect,
    phase: &RewindPhase,
    focused: bool,
    locale: Option<&crate::locale::LocaleContext>,
) {
    if area.height == 0 || area.width < 10 {
        return;
    }

    let theme = Theme::current();
    let bg = theme.bg_light;

    buf.set_style(area, Style::default().bg(bg));

    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::glyphs::accent_bar());
            cell.set_style(accent_style);
        }
    }

    let content_x = area.x + 3;
    let content_w = area.width.saturating_sub(5);

    let title_style = Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD);

    match phase {
        RewindPhase::Loading => {
            let y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    rewind_static(locale, "rewind.loading_points", "Loading rewind points..."),
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
        }
        RewindPhase::Picker { points, selected } => {
            // Shared list-overlay frame and row geometry (also used by /jump)
            // It applies the unfocus dim itself, so return before the shared blend at the bottom of this function
            crate::views::overlay_list::ListOverlay {
                len: points.len(),
                selected: *selected,
            }
            .render(
                buf,
                area,
                rewind_static(locale, "rewind.picker.title", "Rewind to which turn?"),
                focused,
                |i, ctx| {
                    let point = &points[i];
                    let dot_style = Style::default().fg(theme.gray).bg(ctx.row_bg);
                    let file_info = if point.has_file_changes {
                        rewind_text(locale, "rewind.files_count", " · {count} files")
                            .replace("{count}", &point.num_file_snapshots.to_string())
                    } else {
                        String::new()
                    };
                    let preview_width = ctx.content_width.saturating_sub(2).saturating_sub(
                        unicode_width::UnicodeWidthStr::width(file_info.as_str()) as u16,
                    );
                    let preview: String = crate::render::line_utils::truncate_str(
                        point.prompt_preview.as_deref().unwrap_or_else(|| {
                            rewind_static(locale, "rewind.no_preview", "(no preview)")
                        }),
                        preview_width as usize,
                    );
                    let text_style = Style::default()
                        .fg(theme.text_primary)
                        .bg(ctx.row_bg)
                        .add_modifier(if ctx.is_cursor {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        });
                    let meta_style = Style::default().fg(theme.gray).bg(ctx.row_bg);

                    Line::from(vec![
                        Span::styled("\u{00B7} ", dot_style),
                        Span::styled(preview, text_style),
                        Span::styled(file_info, meta_style),
                    ])
                },
            );
            return;
        }
        RewindPhase::CancelOffer { active_idx } => {
            let mut y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    rewind_static(
                        locale,
                        "rewind.turn_running",
                        "A turn is currently running.",
                    ),
                    title_style,
                )),
                content_w,
            );
            y += 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    rewind_static(
                        locale,
                        "rewind.cancel_question",
                        "Would you like to cancel it before rewinding?",
                    ),
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'y',
                rewind_static(locale, "rewind.cancel_and_rewind", "Cancel turn and rewind"),
                *active_idx == 0,
                focused,
                &theme,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'n',
                rewind_static(locale, "rewind.let_finish", "Let it finish"),
                *active_idx == 1,
                focused,
                &theme,
            );
        }
        RewindPhase::Executing { .. } => {
            let y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    rewind_static(locale, "rewind.executing", "Rewinding..."),
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
        }
        RewindPhase::Confirm {
            active_idx,
            prompt_preview,
            num_file_snapshots,
            ..
        } => {
            let mut y = area.y + 1;
            let cursor_mode = confirm_mode_at(*active_idx);
            let preview_text = prompt_preview
                .as_deref()
                .unwrap_or_else(|| rewind_static(locale, "rewind.this_turn", "this turn"));
            // The title states what the highlighted row would do, so the consequence is
            // legible before committing to it.
            let (prefix, suffix) = match cursor_mode {
                Some(mode) => {
                    let suffix = if mode == RewindMode::FilesOnly {
                        rewind_text(
                            locale,
                            "rewind.confirm.files_suffix",
                            "\u{201D}? ({count} files)",
                        )
                        .replace("{count}", &num_file_snapshots.to_string())
                    } else {
                        rewind_static(locale, "rewind.confirm.suffix", "\u{201D}?").to_string()
                    };
                    (
                        rewind_static(locale, mode.title_prefix_id(), mode.title_prefix_default())
                            .to_string(),
                        suffix,
                    )
                }
                // Cancel row: drop the preview and just ask what to rewind.
                None => (
                    rewind_static(locale, "rewind.mode.title", "What should be rewound?")
                        .to_string(),
                    String::new(),
                ),
            };
            let chrome = unicode_width::UnicodeWidthStr::width(prefix.as_str())
                + unicode_width::UnicodeWidthStr::width(suffix.as_str());
            let max_preview = (content_w as usize).saturating_sub(chrome);
            let preview_trunc = match cursor_mode {
                Some(_) => crate::render::line_utils::truncate_str(preview_text, max_preview),
                None => String::new(),
            };
            let title = format!("{prefix}{preview_trunc}{suffix}");
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(title, title_style)),
                content_w,
            );
            y += 1;
            for (idx, mode) in CONFIRM_MODES.iter().enumerate() {
                render_radio_row(
                    buf,
                    content_x,
                    y,
                    content_w,
                    MODE_KEYS[idx],
                    &rewind_static(locale, mode.label_id(), mode.label_default()),
                    *active_idx == idx,
                    focused,
                    &theme,
                );
                y += 1;
            }
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'n',
                rewind_static(locale, "rewind.confirm.no", "No"),
                *active_idx == CONFIRM_MODES.len(),
                focused,
                &theme,
            );
        }
        RewindPhase::Error { message } => {
            let mut y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    rewind_static(locale, "rewind.failed", "Rewind failed"),
                    Style::default()
                        .fg(theme.accent_error)
                        .add_modifier(Modifier::BOLD),
                )),
                content_w,
            );
            y += 1;
            let localized_message = localized_rewind_error(locale, message);
            let truncated =
                crate::render::line_utils::truncate_str(&localized_message, content_w as usize);
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    truncated,
                    Style::default().fg(theme.text_primary),
                )),
                content_w,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                '\x1b',
                rewind_static(locale, "rewind.dismiss", "Dismiss"),
                true,
                focused,
                &theme,
            );
        }
    }

    // Unfocus dim: when the prompt area is unfocused (user moved to scrollback), blend foregrounds toward `bg_light` so the panel recedes
    // Mirrors the unfocused prompt widget pattern (see `prompt_widget.rs`)
    if !focused {
        crate::render::color::blend_area(buf, area, Some((bg, 0.66)), None);
    }
}

/// Visible label for sentinel-encoded keys (`Esc`, `Bksp`).
fn key_label(key: char) -> String {
    match key {
        '\x1b' => "Esc".into(),
        '\x08' => "Bksp".into(),
        other => other.to_string(),
    }
}

fn render_radio_row(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    key: char,
    label: &str,
    is_cursor: bool,
    panel_focused: bool,
    theme: &Theme,
) {
    let bg = if is_cursor && panel_focused {
        theme.bg_visual
    } else {
        theme.bg_light
    };

    let row_rect = Rect {
        x: x.saturating_sub(1),
        y,
        width: w + 2,
        height: 1,
    };
    buf.set_style(row_rect, Style::default().bg(bg));

    let marker = if is_cursor {
        crate::glyphs::filled_dot()
    } else {
        "\u{25CB}"
    };
    let key_display = key_label(key);

    let num_style = Style::default().fg(theme.accent_user).bg(bg);
    let marker_style = if is_cursor {
        Style::default().fg(theme.accent_user).bg(bg)
    } else {
        Style::default().fg(theme.gray).bg(bg)
    };
    let label_style = Style::default()
        .fg(theme.text_primary)
        .bg(bg)
        .add_modifier(if is_cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    let line = Line::from(vec![
        Span::styled(format!("{key_display:<4}"), num_style),
        Span::styled(format!("({marker}) "), marker_style),
        Span::styled(label.to_string(), label_style),
    ]);
    buf.set_line(x, y, &line, w);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn zh_cn_locale() -> crate::locale::LocaleContext {
        crate::locale::LocaleContext::new(crate::locale::ResolvedLocale {
            locale: crate::locale::UiLocale::ZhCn,
            source: crate::locale::LocaleSource::Cli,
        })
    }

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        }
    }

    fn point(prompt_index: usize) -> RewindPointInfo {
        RewindPointInfo {
            prompt_index,
            created_at: String::new(),
            num_file_snapshots: 0,
            prompt_preview: Some(format!("turn {prompt_index}")),
            has_file_changes: false,
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

    fn confirm_state() -> RewindState {
        RewindState {
            phase: RewindPhase::Confirm {
                target_prompt_index: 3,
                active_idx: 0,
                prompt_preview: None,
                num_file_snapshots: 0,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: Some(3),
            fixed_mode: None,
            restore: JumpRestore::none(),
        }
    }

    #[test]
    fn picker_row_hit_test_maps_to_point_index() {
        let phase = RewindPhase::Picker {
            points: vec![point(0), point(1), point(2)],
            selected: 0,
        };
        // Title is at y+1; rows start at y+2.
        assert_eq!(rewind_row_at(&phase, area(), 5, 1), None);
        assert_eq!(rewind_row_at(&phase, area(), 5, 2), Some(0));
        assert_eq!(rewind_row_at(&phase, area(), 5, 4), Some(2));
        // Past the last point.
        assert_eq!(rewind_row_at(&phase, area(), 5, 5), None);
        // Outside the overlay horizontally.
        assert_eq!(rewind_row_at(&phase, area(), 99, 2), None);
    }

    #[test]
    fn cancel_offer_rows() {
        let phase = RewindPhase::CancelOffer { active_idx: 0 };
        assert_eq!(rewind_row_at(&phase, area(), 5, 3), Some(0));
        assert_eq!(rewind_row_at(&phase, area(), 5, 4), Some(1));
        assert_eq!(rewind_row_at(&phase, area(), 5, 5), None);
    }

    #[test]
    fn confirm_rows() {
        let phase = RewindPhase::Confirm {
            target_prompt_index: 0,
            active_idx: 0,
            prompt_preview: None,
            num_file_snapshots: 0,
        };
        assert_eq!(rewind_row_at(&phase, area(), 5, 2), Some(0), "row 0 = All");
        assert_eq!(rewind_row_at(&phase, area(), 5, 3), Some(1));
        assert_eq!(rewind_row_at(&phase, area(), 5, 4), Some(2));
        assert_eq!(rewind_row_at(&phase, area(), 5, 5), Some(3), "cancel row");
        assert_eq!(rewind_row_at(&phase, area(), 5, 6), None);
    }

    #[test]
    fn error_dismiss_row() {
        let phase = RewindPhase::Error {
            message: "boom".into(),
        };
        assert_eq!(rewind_row_at(&phase, area(), 5, 3), Some(0));
        assert_eq!(rewind_row_at(&phase, area(), 5, 2), None);
    }

    #[test]
    fn zh_localization_rewind_errors_translate_client_copy_and_preserve_dynamic_values() {
        let locale = zh_cn_locale();
        let checkpoint_path = r"C:\Users\Joy\.grok\sessions\C%3A%5CUsers%5CJoy\checkpoint-493.json";
        let source = format!(
            "Cannot rewind to prompt #493 — compaction checkpoint data is unavailable \
             (Compaction checkpoint file missing: {checkpoint_path}. Cannot safely rewind past \
             the compaction point.). Try rewinding to a prompt after the compaction point instead."
        );
        let localized = localized_rewind_error(Some(&locale), &source);

        assert_eq!(
            localized,
            format!(
                "无法回退到提示 #493——压缩检查点数据不可用（压缩检查点文件缺失：{checkpoint_path}；无法安全回退到压缩点之前）。请改为回退到压缩点之后的提示。"
            )
        );
        assert!(localized.contains(checkpoint_path));
        assert!(!localized.contains("Cannot rewind"));
        assert_eq!(localized_rewind_error(None, &source), source);

        assert_eq!(
            localized_rewind_error(
                Some(&locale),
                "Cannot rewind to prompt #9 — current prompt index is 9. Valid targets: 0..8",
            ),
            "无法回退到提示 #9——当前提示索引为 9。有效目标范围：0..8"
        );
        assert_eq!(
            localized_rewind_error(
                Some(&locale),
                "Cannot rewind to prompt #493 — compaction checkpoint data is unavailable \
                 (Compaction checkpoint file corrupt: D:\\checkpoints\\broken.json. Cannot safely \
                 rewind past the compaction point.). Try rewinding to a prompt after the \
                 compaction point instead.",
            ),
            "无法回退到提示 #493——压缩检查点数据不可用（压缩检查点文件已损坏：D:\\checkpoints\\broken.json；无法安全回退到压缩点之前）。请改为回退到压缩点之后的提示。"
        );
        assert_eq!(
            localized_rewind_error(
                Some(&locale),
                "Cannot rewind to prompt #493 — compaction checkpoint data is unavailable \
                 (Unsupported checkpoint schema version 2. Cannot safely rewind past the \
                 compaction point.). Try rewinding to a prompt after the compaction point \
                 instead.",
            ),
            "无法回退到提示 #493——压缩检查点数据不可用（不支持检查点架构版本 2；无法安全回退到压缩点之前）。请改为回退到压缩点之后的提示。"
        );
        assert_eq!(
            localized_rewind_error(
                Some(&locale),
                "External modifications detected. Confirm to revert anyway.",
            ),
            "检测到外部修改。请确认仍要回退。"
        );
        assert_eq!(
            localized_rewind_error(Some(&locale), "unknown error"),
            "未知错误"
        );

        let opaque_error = "provider-specific reason: session 7 / path D:\\work";
        assert_eq!(
            localized_rewind_error(Some(&locale), opaque_error),
            opaque_error
        );
    }

    #[test]
    fn zh_localization_rewind_error_overlay_uses_localized_message() {
        let locale = zh_cn_locale();
        let area = Rect::new(0, 0, 160, 5);
        let mut buf = Buffer::empty(area);
        let phase = RewindPhase::Error {
            message:
                "Cannot rewind to prompt #493 — current prompt index is 493. Valid targets: 0..492"
                    .to_owned(),
        };

        render_rewind_overlay(&mut buf, area, &phase, true, Some(&locale));

        let rendered = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|col| buf[(col, row)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rendered_without_cell_padding = rendered.replace(' ', "");
        assert!(rendered_without_cell_padding.contains("回退失败"));
        assert!(
            rendered_without_cell_padding
                .contains("无法回退到提示#493——当前提示索引为493。有效目标范围：0..492")
        );
        assert!(!rendered.contains("Cannot rewind"));
        assert!(rendered_without_cell_padding.contains("关闭"));
    }

    #[test]
    fn non_interactive_phases_have_no_rows() {
        for phase in [
            RewindPhase::Loading,
            RewindPhase::Executing {
                target_prompt_index: 0,
            },
        ] {
            for row in 0..10 {
                assert_eq!(rewind_row_at(&phase, area(), 5, row), None);
            }
        }
    }

    #[test]
    fn set_cursor_moves_and_clamps() {
        let mut phase = RewindPhase::Picker {
            points: vec![point(0), point(1)],
            selected: 0,
        };
        assert!(set_rewind_cursor(&mut phase, 1));
        assert!(!set_rewind_cursor(&mut phase, 1)); // no change
        // Clamp out-of-range to last point (already at last, so no change)
        assert!(!set_rewind_cursor(&mut phase, 99));
        if let RewindPhase::Picker { selected, .. } = phase {
            assert_eq!(selected, 1);
        } else {
            panic!("expected picker");
        }

        let mut confirm = RewindPhase::Confirm {
            target_prompt_index: 0,
            active_idx: 0,
            prompt_preview: None,
            num_file_snapshots: 0,
        };
        set_rewind_cursor(&mut confirm, 3);
        if let RewindPhase::Confirm { active_idx, .. } = confirm {
            assert_eq!(active_idx, 3);
        } else {
            panic!("expected confirm");
        }
        set_rewind_cursor(&mut confirm, 99);
        if let RewindPhase::Confirm { active_idx, .. } = confirm {
            assert_eq!(active_idx, 3, "clamps to the last row (cancel)");
        } else {
            panic!("expected confirm");
        }
    }

    #[test]
    fn activate_matches_enter_semantics() {
        let picker = RewindPhase::Picker {
            points: vec![point(10), point(20)],
            selected: 1,
        };
        assert!(matches!(
            rewind_activate(&picker),
            RewindInput::PickerSelect(20)
        ));

        let error = RewindPhase::Error {
            message: "x".into(),
        };
        assert!(matches!(rewind_activate(&error), RewindInput::DismissError));

        // Enter on a mode row picks that mode; the trailing row cancels.
        let confirm_all = RewindPhase::Confirm {
            target_prompt_index: 4,
            active_idx: 0,
            prompt_preview: None,
            num_file_snapshots: 0,
        };
        assert!(matches!(
            rewind_activate(&confirm_all),
            RewindInput::Confirm(4, RewindMode::All)
        ));

        let confirm_files = RewindPhase::Confirm {
            target_prompt_index: 4,
            active_idx: 2,
            prompt_preview: None,
            num_file_snapshots: 0,
        };
        assert!(matches!(
            rewind_activate(&confirm_files),
            RewindInput::Confirm(4, RewindMode::FilesOnly)
        ));

        let confirm_no = RewindPhase::Confirm {
            target_prompt_index: 4,
            active_idx: 3,
            prompt_preview: None,
            num_file_snapshots: 0,
        };
        assert!(matches!(
            rewind_activate(&confirm_no),
            RewindInput::Dismissed
        ));
    }

    #[test]
    fn confirm_letter_keys() {
        let state = confirm_state();
        // Digits pick a mode directly.
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Char('2'))),
            RewindInput::Confirm(3, RewindMode::ConversationOnly)
        ));
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Char('3'))),
            RewindInput::Confirm(3, RewindMode::FilesOnly)
        ));
        // y and a act on the highlighted row; the cursor starts on `All`.
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Char('y'))),
            RewindInput::Confirm(3, RewindMode::All)
        ));
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Char('a'))),
            RewindInput::ConfirmNeverAsk(3, RewindMode::All)
        ));
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Char('n'))),
            RewindInput::Dismissed
        ));

        // On the cancel row y/a cancel rather than executing an unhighlighted mode.
        let mut cancel_row = confirm_state();
        if let RewindPhase::Confirm { active_idx, .. } = &mut cancel_row.phase {
            *active_idx = CONFIRM_OPTIONS - 1;
        }
        assert!(matches!(
            handle_rewind_key(&cancel_row, &key(KeyCode::Char('y'))),
            RewindInput::Dismissed
        ));
        assert!(matches!(
            handle_rewind_key(&cancel_row, &key(KeyCode::Char('a'))),
            RewindInput::Dismissed
        ));
    }

    #[test]
    fn confirm_title_states_the_highlighted_mode() {
        let locale = zh_cn_locale();
        let area = Rect::new(0, 0, 160, 8);
        let phase_with_cursor = |active_idx: usize| RewindPhase::Confirm {
            target_prompt_index: 1,
            active_idx,
            prompt_preview: Some("alpha".into()),
            num_file_snapshots: 3,
        };

        let render = |phase: &RewindPhase| {
            let mut buf = Buffer::empty(area);
            render_rewind_overlay(&mut buf, area, phase, true, Some(&locale));
            (0..area.height)
                .map(|row| {
                    (0..area.width)
                        .map(|col| buf[(col, row)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
                .replace(' ', "")
        };

        let all = render(&phase_with_cursor(0));
        assert!(all.contains("将文件更改和会话回退到“alpha”"), "got: {all}");
        assert!(all.contains("会话和文件更改"), "got: {all}");

        let conversation = render(&phase_with_cursor(1));
        assert!(
            conversation.contains("仅将会话回退到“alpha”"),
            "got: {conversation}"
        );

        // Files-only carries the snapshot count, which is what makes its cost legible.
        let files = render(&phase_with_cursor(2));
        assert!(
            files.contains("仅将文件更改回退到“alpha”？（3个文件）"),
            "got: {files}"
        );

        let cancel = render(&phase_with_cursor(CONFIRM_OPTIONS - 1));
        assert!(cancel.contains("要回退什么？"), "got: {cancel}");
        assert!(cancel.contains("否"), "got: {cancel}");
    }

    #[test]
    fn esc_dismisses_from_confirm() {
        let state = confirm_state();
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));
    }

    #[test]
    fn backspace_ignored_on_confirm() {
        let state = confirm_state();
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Backspace)),
            RewindInput::Consumed
        ));
    }

    #[test]
    fn esc_dismisses_from_picker_and_other_phases() {
        let s = RewindState {
            phase: RewindPhase::Picker {
                points: vec![],
                selected: 0,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: None,
            fixed_mode: None,
            restore: JumpRestore::none(),
        };
        assert!(matches!(
            handle_rewind_key(&s, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));

        let s = RewindState::new_cancel_offer(0, None, None, None, JumpRestore::none());
        assert!(matches!(
            handle_rewind_key(&s, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));

        let s = RewindState {
            phase: RewindPhase::Loading,
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: None,
            fixed_mode: None,
            restore: JumpRestore::none(),
        };
        assert!(matches!(
            handle_rewind_key(&s, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));
    }

    #[test]
    fn key_label_renders_special_sentinels() {
        assert_eq!(key_label('\x1b'), "Esc");
        assert_eq!(key_label('\x08'), "Bksp");
        assert_eq!(key_label('y'), "y");
        assert_eq!(key_label('a'), "a");
    }
}
