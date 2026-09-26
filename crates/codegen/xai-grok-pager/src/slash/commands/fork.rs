//! `/fork`: branch the current session into a peer top-level agent.
//!
//! The command parses optional flags (`--worktree`, `--no-worktree`, `--at <prompt>`) and an optional free-form directive.
//! It returns [`Action::Fork`](crate::app::actions::Action::Fork) carrying a [`ForkArgs`] payload.
//! The placeholder construction, modal routing, and effect emission live in `dispatch::dispatch_fork`.
//! The fork itself is dispatched in `dispatch_fork_resolved`, after the fork point and worktree question are resolved and the placeholder spawn succeeds.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// [`parse_fork_args`] returns this, and [`Action::Fork`](crate::app::actions::Action::Fork) carries it to the dispatcher.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ForkArgs {
    /// `None`        -> open the worktree question modal (the user is
    ///                  asked every time; the choice is never persisted).
    /// `Some(true)`  -> force worktree, skipping the modal.
    /// `Some(false)` -> force no-worktree, skipping the modal.
    pub worktree_override: Option<bool>,
    /// Optional first prompt for the new session. Whitespace-trimmed.
    /// `None` when the user typed `/fork` (with or without flags) and no directive text; the new agent then opens with no first prompt.
    pub directive: Option<String>,
    /// `Some(k)` -> fork the conversation up to (not including) user prompt `k`, the one-based-by-history
    /// position the fork-point picker shows. `None` -> keep the whole conversation (the default and the
    /// picker's "current state" row).
    pub at_prompt: Option<usize>,
}

/// Where the fork's copy of the parent conversation ends.
///
/// The shell's `target_prompt_index` keeps every prompt up to and including the index, so
/// "fork before prompt `k`" is the wire value `k - 1`. The picker and `--at k` both resolve to this.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkCut {
    /// Wire value for `targetPromptIndex`; `None` copies the whole conversation.
    pub target_prompt_index: Option<usize>,
    /// Text of the prompt the cut lands before, pre-filled into the child's composer so the user can rewrite it.
    pub prefill: Option<String>,
}

/// Parse the raw argument string after `/fork`.
///
/// Recognised flags appear at the start; everything after the last flag is the directive.
/// An unknown flag is treated as the start of the directive, so `/fork --foo bar` becomes the directive `--foo bar`.
/// The args are user-typed text, and a directive that happens to begin with `--` must not be rejected.
///
/// Errors:
/// - `--worktree` and `--no-worktree` cannot both appear.
/// - `--at` needs a numeric prompt position of at least 1 (forking before the first prompt is `/new`).
pub fn parse_fork_args(args: &str) -> Result<ForkArgs, String> {
    let mut worktree_override: Option<bool> = None;
    let mut at_prompt: Option<usize> = None;
    let mut rest = args.trim_start();

    while !rest.is_empty() {
        let (flag, after) = match rest.split_once(char::is_whitespace) {
            Some(parts) => parts,
            None => (rest, ""),
        };
        match flag {
            "--worktree" => {
                if worktree_override == Some(false) {
                    return Err("--worktree and --no-worktree are mutually exclusive".into());
                }
                if worktree_override == Some(true) {
                    return Err("--worktree specified twice".into());
                }
                worktree_override = Some(true);
                rest = after.trim_start();
            }
            "--no-worktree" => {
                if worktree_override == Some(true) {
                    return Err("--worktree and --no-worktree are mutually exclusive".into());
                }
                if worktree_override == Some(false) {
                    return Err("--no-worktree specified twice".into());
                }
                worktree_override = Some(false);
                rest = after.trim_start();
            }
            "--at" => {
                if at_prompt.is_some() {
                    return Err("--at specified twice".into());
                }
                let (value, after_value) = after
                    .trim_start()
                    .split_once(char::is_whitespace)
                    .map(|(v, rest)| (v, rest.trim_start()))
                    .unwrap_or((after.trim_start(), ""));
                let Some(position) = value.parse::<usize>().ok().filter(|p| *p > 0) else {
                    return Err(
                        "--at needs the position of the prompt to fork before (1 or greater); use /new to start an empty session"
                            .into(),
                    );
                };
                at_prompt = Some(position);
                rest = after_value;
            }
            _ => break,
        }
    }

    let directive = if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    };
    Ok(ForkArgs {
        worktree_override,
        directive,
        at_prompt,
    })
}

pub struct ForkCommand;

impl SlashCommand for ForkCommand {
    slash_meta! {
        name: "fork",
        description: "Branch the session into a peer agent, optionally from an earlier prompt",
        usage: "/fork [--worktree|--no-worktree] [--at <prompt>] [directive]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[directive]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match parse_fork_args(args) {
            Ok(parsed) => CommandResult::Action(Action::Fork(parsed)),
            Err(msg) => CommandResult::Error(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    // -- parse_fork_args ---------------------------------------------------

    #[test]
    fn parse_empty_returns_none_directive_and_no_override() {
        let parsed = parse_fork_args("").expect("empty args parse");
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_directive_only_returns_directive_with_no_override() {
        let parsed =
            parse_fork_args("explore the rate-limit hypothesis").expect("directive-only parse");
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(
            parsed.directive.as_deref(),
            Some("explore the rate-limit hypothesis")
        );
    }

    #[test]
    fn parse_worktree_flag_alone_sets_override_true() {
        let parsed = parse_fork_args("--worktree").expect("--worktree alone parse");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_no_worktree_flag_alone_sets_override_false() {
        let parsed = parse_fork_args("--no-worktree").expect("--no-worktree alone parse");
        assert_eq!(parsed.worktree_override, Some(false));
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_worktree_flag_with_directive_sets_both() {
        let parsed = parse_fork_args("--worktree investigate the bug")
            .expect("--worktree + directive parse");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive.as_deref(), Some("investigate the bug"));
    }

    #[test]
    fn parse_no_worktree_flag_with_directive_sets_both() {
        let parsed =
            parse_fork_args("--no-worktree quick fix").expect("--no-worktree + directive parse");
        assert_eq!(parsed.worktree_override, Some(false));
        assert_eq!(parsed.directive.as_deref(), Some("quick fix"));
    }

    #[test]
    fn parse_worktree_then_no_worktree_is_mutual_exclusion_error() {
        let err = parse_fork_args("--worktree --no-worktree x")
            .expect_err("conflicting flags must error");
        assert!(
            err.contains("mutually exclusive"),
            "error should explain mutual exclusion: {err}"
        );
    }

    #[test]
    fn parse_no_worktree_then_worktree_is_mutual_exclusion_error() {
        let err = parse_fork_args("--no-worktree --worktree x")
            .expect_err("conflicting flags must error");
        assert!(
            err.contains("mutually exclusive"),
            "error should explain mutual exclusion: {err}"
        );
    }

    #[test]
    fn parse_worktree_repeated_returns_error() {
        let err = parse_fork_args("--worktree --worktree foo")
            .expect_err("duplicate --worktree must error");
        assert!(
            err.contains("twice"),
            "error should mention duplicate: {err}"
        );
    }

    #[test]
    fn parse_at_flag_records_the_prompt_position() {
        let parsed = parse_fork_args("--at 3").expect("--at parse");
        assert_eq!(parsed.at_prompt, Some(3));
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_at_flag_with_directive_keeps_both() {
        let parsed = parse_fork_args("--at 2 try the other design").expect("--at + directive");
        assert_eq!(parsed.at_prompt, Some(2));
        assert_eq!(parsed.directive.as_deref(), Some("try the other design"));
    }

    #[test]
    fn parse_at_flag_with_worktree_keeps_both() {
        let parsed = parse_fork_args("--worktree --at 4").expect("--worktree + --at");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.at_prompt, Some(4));
    }

    #[test]
    fn parse_at_flag_rejects_missing_value() {
        let err = parse_fork_args("--at").expect_err("--at without a value must error");
        assert!(err.contains("--at needs"), "got: {err}");
    }

    #[test]
    fn parse_at_flag_rejects_non_numeric_value() {
        let err = parse_fork_args("--at abc").expect_err("non-numeric --at must error");
        assert!(err.contains("--at needs"), "got: {err}");
    }

    #[test]
    fn parse_at_flag_rejects_zero() {
        // Forking before the first prompt leaves nothing to fork; `/new` is that operation.
        let err = parse_fork_args("--at 0").expect_err("--at 0 must error");
        assert!(err.contains("--at needs"), "got: {err}");
    }

    #[test]
    fn parse_at_flag_repeated_returns_error() {
        let err = parse_fork_args("--at 1 --at 2").expect_err("duplicate --at must error");
        assert!(err.contains("twice"), "got: {err}");
    }

    #[test]
    fn parse_leading_whitespace_is_trimmed_before_flag_lookup() {
        let parsed = parse_fork_args("   --worktree foo bar").expect("leading whitespace allowed");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive.as_deref(), Some("foo bar"));
    }

    #[test]
    fn parse_unknown_token_is_treated_as_directive_start() {
        // Conservative behaviour: a bareword that isn't a recognised flag becomes the directive
        // `/fork --foo bar` is not rejected as a typo; the model receives `--foo bar` as its first prompt
        let parsed = parse_fork_args("--foo bar").expect("unknown flag parse");
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(parsed.directive.as_deref(), Some("--foo bar"));
    }

    #[test]
    fn parse_extra_whitespace_between_flag_and_directive_is_trimmed() {
        let parsed =
            parse_fork_args("--worktree    investigate").expect("extra whitespace allowed");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive.as_deref(), Some("investigate"));
    }

    // -- ForkCommand SlashCommand impl ------------------------------------

    fn make_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        let bundle = Box::leak(Box::new(crate::app::bundle::BundleState::default()));
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn run_no_args_returns_fork_action_with_default_args() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "") {
            CommandResult::Action(Action::Fork(args)) => {
                assert_eq!(args.worktree_override, None);
                assert_eq!(args.directive, None);
            }
            other => panic!("expected Action(Fork(..)), got {other:?}"),
        }
    }

    #[test]
    fn run_worktree_with_directive_returns_action_carrying_both() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--worktree fix the test") {
            CommandResult::Action(Action::Fork(args)) => {
                assert_eq!(args.worktree_override, Some(true));
                assert_eq!(args.directive.as_deref(), Some("fix the test"));
            }
            other => panic!("expected Action(Fork(..)), got {other:?}"),
        }
    }

    #[test]
    fn run_conflicting_flags_returns_error_result() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--worktree --no-worktree") {
            CommandResult::Error(msg) => {
                assert!(msg.contains("mutually exclusive"), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_at_flag_returns_fork_action_carrying_the_position() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--at 5") {
            CommandResult::Action(Action::Fork(args)) => {
                assert_eq!(args.at_prompt, Some(5));
            }
            other => panic!("expected Action(Fork(..)), got {other:?}"),
        }
    }

    #[test]
    fn run_at_flag_without_value_returns_error_result() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--at") {
            CommandResult::Error(msg) => {
                assert!(msg.contains("--at needs"), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn metadata_matches_design() {
        let cmd = ForkCommand;
        assert_eq!(cmd.name(), "fork");
        assert!(cmd.takes_args(), "/fork accepts args");
        assert!(!cmd.args_required(), "/fork allows no args");
        assert_eq!(cmd.arg_placeholder(), Some("[directive]"));
    }
}
