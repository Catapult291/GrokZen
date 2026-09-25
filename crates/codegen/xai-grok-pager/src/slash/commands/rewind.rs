use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct RewindCommand;

impl SlashCommand for RewindCommand {
    slash_meta! {
        name: "rewind",
        description: "Rewind conversation and files to an earlier turn",
        usage: "/rewind",
        session_scoped: true,
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::RewindShowPicker)
    }
}

/// Conversation-only counterpart of `/rewind`.
/// Its own command rather than an alias: the two differ in what they roll back, and `/undo` runs
/// without the mode dialog (see `dispatch::rewind::dispatch_undo`).
pub struct UndoCommand;

impl SlashCommand for UndoCommand {
    slash_meta! {
        name: "undo",
        description: "Rewind the conversation to an earlier turn",
        usage: "/undo",
        session_scoped: true,
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::UndoShowPicker)
    }
}
