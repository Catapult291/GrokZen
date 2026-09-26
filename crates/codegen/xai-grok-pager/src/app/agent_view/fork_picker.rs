use super::AgentView;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::views::fork_picker::{
    ForkPickerInput, ForkPointRow, fork_picker_activate, fork_picker_row_at,
    handle_fork_picker_key, move_cursor, set_fork_picker_cursor,
};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};

impl AgentView {
    /// Close the fork-point picker (if open) and restore the viewport it opened from.
    pub(crate) fn dismiss_fork_picker(&mut self) {
        if let Some(state) = self.fork_picker_state.take() {
            self.restore_jump_viewport(state.restore);
        }
    }

    /// True when another prompt overlay owns the input slot, so the fork-point picker must not open and an open one must be dismissed.
    /// The owners match [`Self::jump_slot_taken`] minus the `/jump` picker: opening the fork picker closes a lingering jump picker
    /// (`open_fork_picker` does this), while `/jump` refuses to open on top of the fork picker.
    pub(crate) fn fork_picker_slot_taken(&self) -> bool {
        self.rewind_state.is_some()
            || self.inline_edit.is_some()
            || self.btw_state.is_some()
            || !self.no_input_overlay_pending()
    }

    /// Drop the picker when another overlay owns the input slot ([`Self::fork_picker_slot_taken`]), so it can't eat wheel/keys while hidden.
    /// Returns whether it dropped one, so an `Esc` caller can spend that key here.
    pub(super) fn dismiss_fork_picker_if_suppressed(&mut self) -> bool {
        if self.fork_picker_state.is_some() && self.fork_picker_slot_taken() {
            self.dismiss_fork_picker();
            return true;
        }
        false
    }

    /// Live-scroll the transcript to the prompt under the cursor, anchored at the viewport top, so the
    /// row being read is the one Enter cuts at. The "keep the whole conversation" row scrolls nothing.
    pub(in crate::app) fn sync_fork_picker_preview(&mut self) {
        let Some(entry_id) = self
            .fork_picker_state
            .as_ref()
            .and_then(|state| state.rows.get(state.selected))
            .and_then(|row| match row {
                ForkPointRow::Before { entry_id, .. } => Some(*entry_id),
                ForkPointRow::Current => None,
            })
        else {
            return;
        };
        // Resolve the stable id at the boundary; a removal since capture just means no preview scroll
        // rather than landing on the wrong block.
        if let Some(idx) = self.scrollback.index_of_id(entry_id) {
            self.scrollback.scroll_to_entry_top(idx);
        }
    }

    pub(super) fn handle_fork_picker_key(&mut self, key: &KeyEvent) -> InputOutcome {
        let Some(ref state) = self.fork_picker_state else {
            return InputOutcome::Unchanged;
        };
        match handle_fork_picker_key(state, key) {
            ForkPickerInput::MoveUp => {
                if let Some(ref mut state) = self.fork_picker_state {
                    move_cursor(state, -1);
                }
                self.sync_fork_picker_preview();
                InputOutcome::Changed
            }
            ForkPickerInput::MoveDown => {
                if let Some(ref mut state) = self.fork_picker_state {
                    move_cursor(state, 1);
                }
                self.sync_fork_picker_preview();
                InputOutcome::Changed
            }
            other => Self::fork_picker_input_to_outcome(other),
        }
    }

    /// Map a terminal [`ForkPickerInput`] to its `InputOutcome`. Shared by the key and mouse paths so they can't drift.
    fn fork_picker_input_to_outcome(input: ForkPickerInput) -> InputOutcome {
        match input {
            ForkPickerInput::Select(row) => InputOutcome::Action(Action::ForkPointSelect(row)),
            ForkPickerInput::Dismissed => InputOutcome::Action(Action::ForkPointDismiss),
            ForkPickerInput::MoveUp | ForkPickerInput::MoveDown | ForkPickerInput::Consumed => {
                InputOutcome::Changed
            }
        }
    }

    /// `Moved` moves the cursor (and previews); `Down(Left)` activates the row (Enter-equivalent).
    pub(super) fn handle_fork_picker_mouse(&mut self, mouse: &MouseEvent) -> InputOutcome {
        let Some(state) = self.fork_picker_state.as_mut() else {
            return InputOutcome::Unchanged;
        };

        let area = self.pane_areas.prompt;
        let Some(idx) = fork_picker_row_at(state, area, mouse.column, mouse.row) else {
            return InputOutcome::Unchanged;
        };

        match mouse.kind {
            MouseEventKind::Moved => {
                if set_fork_picker_cursor(state, idx) {
                    self.sync_fork_picker_preview();
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                set_fork_picker_cursor(state, idx);
                let activated = fork_picker_activate(state);
                Self::fork_picker_input_to_outcome(activated)
            }
            _ => InputOutcome::Unchanged,
        }
    }
}
