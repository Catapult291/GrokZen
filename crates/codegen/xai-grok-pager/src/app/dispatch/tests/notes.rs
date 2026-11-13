//! Tests for feedback / remember / btw / recap dispatchers.
use super::*;
use crate::app::dispatch::ctx::NO_SESSION_NOTICE;
use crate::app::dispatch::{recap_unavailable_toast, scrollback_has_user_messages};
fn agent_ref(app: &AppView, id: AgentId) -> &AgentView {
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    agent
}
fn send_minimal_btw(app: &mut AppView, question: &str) -> uuid::Uuid {
    match dispatch(
        Action::SendBtw {
            question: question.into(),
            images: Vec::new(),
        },
        app,
    )
    .as_slice()
    {
        [
            Effect::SendBtw {
                minimal_request_id: Some(id),
                ..
            },
        ] => *id,
        other => panic!("expected correlated minimal /btw effect, got {other:?}"),
    }
}
fn esc() -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ))
}
#[test]
fn remember_save_carries_the_session_pinned_mode() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(
        Action::TaskComplete(TaskResult::WithPinnedMemoryMode {
            agent_id: id,
            memory_mode: Some(xai_grok_shell::config::MemoryMode::V2),
            result: Box::new(TaskResult::SessionCreated {
                agent_id: id,
                session_id: acp::SessionId::new("pinned-v2"),
                models: None,
                modes: None,
            }),
        }),
        &mut app,
    );
    let rewrite = dispatch(
        Action::SendRememberNote("keep this in v2".to_owned()),
        &mut app,
    );
    assert!(matches!(
        rewrite.as_slice(),
        [Effect::RewriteMemoryNote { .. }]
    ));
    let save = dispatch(Action::SaveRememberNoteFromModal, &mut app);
    assert!(matches!(
        save.as_slice(),
        [Effect::SaveMemoryNote {
            pinned_mode: Some(xai_grok_shell::config::MemoryMode::V2),
            ..
        }]
    ));
}
#[test]
fn remember_save_without_session_defers_to_disk_mode() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(
        dispatch(
            Action::SendRememberNote("pre-session note".to_owned()),
            &mut app,
        )
        .is_empty()
    );
    let save = dispatch(Action::SaveRememberNoteFromModal, &mut app);
    assert!(matches!(
        save.as_slice(),
        [Effect::SaveMemoryNote {
            pinned_mode: None,
            ..
        }]
    ));
}
#[test]
fn recap_unavailable_toast_empty_vs_with_messages() {
    assert_eq!(recap_unavailable_toast(false), "No messages yet");
    assert_eq!(recap_unavailable_toast(true), "Couldn't generate recap");
}
#[test]
fn manual_recap_with_no_messages_toasts_empty_state_and_skips_request() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("/recap");
        assert!(!scrollback_has_user_messages(&agent.scrollback));
    }
    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);
    assert!(
        effects.is_empty(),
        "empty session must not fire x.ai/recap: {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_none(), "no loading spinner");
    assert_eq!(
        agent.toast.as_ref().map(|(s, _)| s.as_str()),
        Some("No messages yet"),
        "empty session should say No messages yet, not Couldn't generate recap"
    );
    assert_eq!(agent.prompt.text(), "", "slash command text is cleared");
}
#[test]
fn manual_recap_with_messages_requests_and_shows_spinner() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("hello"));
        assert!(scrollback_has_user_messages(&agent.scrollback));
    }
    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SendRecap { auto: false, .. }]),
        "expected SendRecap effect, got {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(
        agent.pending_recap_entry.is_some(),
        "manual recap shows a loading spinner when there is something to summarize"
    );
    assert!(agent.toast.is_none());
}
/// Regression: during session/load, scrollback is batched so `turn_count()` stays 0 until `end_batch`, but UserPrompt entries may already be present.
/// Manual `/recap` must still request a recap.
#[test]
fn manual_recap_during_batch_load_with_prompts_still_requests() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.scrollback.begin_batch();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("hello from resume"));
        assert_eq!(agent.scrollback.turn_count(), 0);
        assert!(scrollback_has_user_messages(&agent.scrollback));
    }
    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SendRecap { auto: false, .. }]),
        "batched resume with user prompts must still fire x.ai/recap: {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_some());
    assert!(agent.toast.is_none());
    app.agents.get_mut(&id).unwrap().scrollback.end_batch();
}
/// While session replay is still streaming, don't claim "No messages yet" even if scrollback looks empty; history may arrive on the next notification.
#[test]
fn manual_recap_while_loading_replay_still_requests() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.loading_replay = true;
        assert!(!scrollback_has_user_messages(&agent.scrollback));
    }
    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SendRecap { auto: false, .. }]),
        "loading_replay must not short-circuit to No messages yet: {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_some());
    assert!(agent.toast.is_none());
}
#[test]
fn recap_request_transport_failure_with_no_turns_uses_empty_toast() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let session_id = agent_ref(&app, id).session.session_id.clone().unwrap();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let spinner = agent
            .scrollback
            .push(crate::scrollback::entry::ScrollbackEntry::running(
                RenderBlock::session_event(SessionEvent::Recap {
                    summary: String::new(),
                    auto: false,
                }),
            ));
        agent.pending_recap_entry = Some(spinner);
        assert!(!scrollback_has_user_messages(&agent.scrollback));
    }
    dispatch(
        Action::TaskComplete(TaskResult::RecapRequested {
            session_id,
            auto: false,
            error: Some("transport down".into()),
        }),
        &mut app,
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(s, _)| s.as_str()),
        Some("No messages yet")
    );
}
#[test]
fn recap_request_transport_failure_with_turns_uses_generic_toast() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let session_id = agent_ref(&app, id).session.session_id.clone().unwrap();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("hello"));
        let spinner = agent
            .scrollback
            .push(crate::scrollback::entry::ScrollbackEntry::running(
                RenderBlock::session_event(SessionEvent::Recap {
                    summary: String::new(),
                    auto: false,
                }),
            ));
        agent.pending_recap_entry = Some(spinner);
        assert!(scrollback_has_user_messages(&agent.scrollback));
    }
    dispatch(
        Action::TaskComplete(TaskResult::RecapRequested {
            session_id,
            auto: false,
            error: Some("transport down".into()),
        }),
        &mut app,
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(s, _)| s.as_str()),
        Some("Couldn't generate recap")
    );
}
#[test]
fn minimal_btw_response_after_esc_is_ignored() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().active_pane = crate::app::agent_view::AgentPane::Prompt;
    let request_id = send_minimal_btw(&mut app, "side question");
    let _ = app.handle_input(&esc());
    assert!(agent_ref(&app, id).btw_state.is_none());
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: vec![2],
            agent_id: id,
            result: Ok("late".into()),
            minimal_request_id: Some(request_id),
        }),
        &mut app,
    );
    assert!(agent_ref(&app, id).btw_state.is_none());
    assert!(
        !has_skipped_image_notice(&app, id),
        "a dismissed side question must not report its images either"
    );
}
/// Whether the agent shows the unreadable-image notice on either surface.
fn has_skipped_image_notice(app: &AppView, id: AgentId) -> bool {
    let agent = agent_ref(app, id);
    let in_transcript = agent
        .scrollback
        .iter_entries()
        .any(|(_, entry)| {
            matches!(&entry.block, RenderBlock::System(block) if block.text.contains("couldn't be read"))
        });
    in_transcript
        || agent
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message.contains("couldn't be read"))
}
#[test]
fn minimal_done_dismisses_to_exactly_one_btw_block() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().active_pane = ActivePane::Prompt;
    let request_id = send_minimal_btw(&mut app, "original question");
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: Vec::new(),
            agent_id: id,
            result: Ok("original answer".into()),
            minimal_request_id: Some(request_id),
        }),
        &mut app,
    );
    let _ = app.handle_input(&esc());
    let btw_blocks: Vec<_> = agent_ref(&app, id)
        .scrollback
        .iter_entries()
        .filter_map(|(_, entry)| match &entry.block {
            RenderBlock::Btw(block) => Some(block),
            _ => None,
        })
        .collect();
    let [btw] = btw_blocks.as_slice() else {
        panic!("expected exactly one btw block, got {btw_blocks:?}");
    };
    assert_eq!(btw.question, "original question");
    assert_eq!(btw.content().text(), "original answer");
}
#[test]
fn minimal_btw_requests_stay_independent_across_two_agents() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let first = AgentId(0);
    let second = AgentId(1);
    insert_placeholder_agent(&mut app, second);
    let first_old = send_minimal_btw(&mut app, "first old");
    let first_current = send_minimal_btw(&mut app, "first new");
    switch_to_agent(&mut app, second, SwitchCause::Picker);
    let second_request = send_minimal_btw(&mut app, "second");
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: vec![2],
            agent_id: first,
            result: Ok("stale first answer".into()),
            minimal_request_id: Some(first_old),
        }),
        &mut app,
    );
    assert!(matches!(
        agent_ref(&app, first).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Loading { ref question })
            if question == "first new"
    ));
    assert!(
        !has_skipped_image_notice(&app, first) && !has_skipped_image_notice(&app, second),
        "a superseded side question must not report its images"
    );
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: Vec::new(),
            agent_id: first,
            result: Ok("current first answer".into()),
            minimal_request_id: Some(first_current),
        }),
        &mut app,
    );
    assert!(matches!(
        agent_ref(&app, first).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "first new"
    ));
    assert!(matches!(
        agent_ref(&app, second).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Loading { ref question })
            if question == "second"
    ));
    app.agents.get_mut(&second).unwrap().active_pane = ActivePane::Prompt;
    let _ = app.handle_input(&esc());
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: Vec::new(),
            agent_id: second,
            result: Ok("late second answer".into()),
            minimal_request_id: Some(second_request),
        }),
        &mut app,
    );
    assert!(agent_ref(&app, second).btw_state.is_none());
    assert!(agent_ref(&app, second).minimal_btw_lifecycle.is_none());
    assert!(matches!(
        agent_ref(&app, first).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "first new"
    ));
    switch_to_agent(&mut app, first, SwitchCause::Picker);
    let first_request = send_minimal_btw(&mut app, "first reverse");
    switch_to_agent(&mut app, second, SwitchCause::Picker);
    let second_request = send_minimal_btw(&mut app, "second reverse");
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: Vec::new(),
            agent_id: second,
            result: Ok("second reverse answer".into()),
            minimal_request_id: Some(second_request),
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: Vec::new(),
            agent_id: first,
            result: Ok("first reverse answer".into()),
            minimal_request_id: Some(first_request),
        }),
        &mut app,
    );
    assert!(matches!(
        agent_ref(&app, second).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "second reverse"
    ));
    assert!(matches!(
        agent_ref(&app, first).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "first reverse"
    ));
}
#[test]
fn fullscreen_btw_response_after_dismiss_keeps_existing_behavior() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch(
        Action::SendBtw {
            question: "side question".into(),
            images: Vec::new(),
        },
        &mut app,
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::SendBtw {
            minimal_request_id: None,
            ..
        }]
    ));
    app.agents.get_mut(&id).unwrap().btw_state = None;
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: None,
            skipped_image_numbers: Vec::new(),
            agent_id: id,
            result: Ok("late".into()),
            minimal_request_id: None,
        }),
        &mut app,
    );
    assert!(matches!(
        agent_ref(&app, id).btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question.is_empty()
    ));
}
/// Attachments the side question dropped over the cap and attachments it could not load arrive as one
/// visible notice when the answer lands; neither replaces the other.
#[test]
fn btw_response_toasts_skipped_image_numbers() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(
        Action::SendBtw {
            question: "side question".into(),
            images: Vec::new(),
        },
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            image_notice: Some(
                "1 attached image(s) were not included (over the 50MB side-question limit)".into(),
            ),
            skipped_image_numbers: vec![2],
            agent_id: id,
            result: Ok("answer".into()),
            minimal_request_id: None,
        }),
        &mut app,
    );
    assert_eq!(
        agent_ref(&app, id)
            .toast
            .as_ref()
            .map(|(message, _)| message.as_str()),
        Some(
            "1 attached image(s) were not included (over the 50MB side-question limit); Image #2 couldn't be read — not sent"
        )
    );
}
fn test_pasted_image(mime_type: &str) -> crate::prompt_images::PastedImage {
    crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
        mime_type: mime_type.to_string(),
    })
}
fn test_pasted_png() -> crate::prompt_images::PastedImage {
    test_pasted_image("image/png")
}
#[test]
fn minimal_voice_refusal_is_a_scrollback_block() {
    use crate::app::app_view::{Partial, VoiceState, VoiceTarget};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        partial: Partial::Shown("dictated text".to_owned()),
        route: Some(xai_grok_voice::VoiceRoute::Streaming),
    };
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("/feedback ");
    let effects = dispatch(Action::SendPrompt("/feedback ".to_owned()), &mut app);
    assert!(effects.is_empty(), "{effects:?}");
    let agent = test_agent(&app, id);
    assert!(agent.toast.is_none(), "toasts are invisible in minimal");
    assert!(last_system_text(&app, id).contains("Stop voice input"));
}
fn composer_image() -> crate::prompt_images::PastedImage {
    crate::prompt_images::PastedImage {
        element_id: xai_ratatui_textarea::ElementId::from_raw(0),
        display_number: 0,
        mime_type: "image/png".into(),
        dimensions: Some((100, 80)),
        byte_len: 16,
        encoded_bytes: Some(vec![0u8; 16].into()),
        source_path: None,
        staged_temp_path: None,
        session_image_path: None,
        preview: crate::prompt_images::PromptImagePreview::default(),
    }
}
/// `/btw` is a model call. Composer images must ride on `x.ai/btw`, not be deleted with the other slash actions.
#[test]
fn btw_submit_sends_composer_images() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("/btw what is this");
        agent.prompt.set_cursor(agent.prompt.text().len());
        agent
            .prompt
            .insert_image(composer_image())
            .expect("image chip");
    }
    let text = test_agent(&app, id).prompt.text().to_string();
    let effects = dispatch(Action::SendPrompt(text.clone()), &mut app);
    let images = effects.iter().find_map(|effect| match effect {
        Effect::SendBtw {
            images, question, ..
        } => {
            assert!(
                question.contains("what is this"),
                "question={question:?} text={text:?}"
            );
            assert!(
                question.contains("[Image]"),
                "overlay and wire question should show the image chip, got {question:?}"
            );
            assert!(
                !question.contains("[Image #"),
                "the /btw title does not number the image chip, got {question:?}"
            );
            Some(images.clone())
        }
        _ => None,
    });
    let images = images
        .unwrap_or_else(|| panic!("SendBtw effect missing; text={text:?} effects={effects:?}"));
    assert!(
        !images.is_empty(),
        "composer images must ride on the effect; encoding happens off the TUI thread"
    );
    let encoded = crate::app::dispatch::notes::encode_btw_images(
        "what is this",
        &images,
        std::path::Path::new("."),
    );
    assert_eq!(encoded.omitted, 0, "a small image must not be dropped");
    assert_eq!(encoded.omitted_by_cap, 0);
    let blocks = encoded.blocks.expect("image content blocks");
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, acp::ContentBlock::Image(_))),
        "attached image must encode onto the side-question wire, got {blocks:?}"
    );
    assert!(
        test_agent(&app, id).prompt.images.is_empty(),
        "composer images are consumed by the side question"
    );
}

#[test]
fn zh_localization_review135_btw_notices_translate_without_rewriting_reply() {
    use crate::locale::{LocaleContext, LocaleSource, ResolvedLocale, UiLocale};
    let zh = LocaleContext::new(ResolvedLocale {
        locale: UiLocale::ZhCn,
        source: LocaleSource::Cli,
    });
    for minimal in [false, true] {
        for (notice, expected) in [
            (
                "Attached images were not included (too large or could not be loaded).",
                "附加图片未能发送（图片过大或无法加载）。",
            ),
            (
                "2 attached image(s) were not included (over the 50MB side-question limit or could not be loaded).",
                "2 张附加图片未能发送（超过侧边提问的 50MB 限制或无法加载）。",
            ),
            (
                "{count} attached image(s) were not included (over the 50MB side-question limit or could not be loaded).",
                "{count} attached image(s) were not included (over the 50MB side-question limit or could not be loaded).",
            ),
        ] {
            let mut app = test_app_with_agent();
            let id = AgentId(0);
            if minimal {
                app.screen_mode = crate::app::ScreenMode::Minimal;
            }
            app.agents.get_mut(&id).unwrap().scrollback.set_locale(&zh);
            let request_id = if minimal {
                Some(send_minimal_btw(&mut app, "side question"))
            } else {
                None
            };
            dispatch(
                Action::TaskComplete(TaskResult::BtwResponse {
                    agent_id: id,
                    result: Ok("Original reply {count}".into()),
                    minimal_request_id: request_id,
                    image_notice: Some(notice.into()),
                    skipped_image_numbers: Vec::new(),
                }),
                &mut app,
            );
            if minimal {
                assert_eq!(last_system_text(&app, id), expected);
                assert!(agent_ref(&app, id).toast.is_none());
            } else {
                assert_eq!(
                    agent_ref(&app, id).toast.as_ref().map(|(s, _)| s.as_str()),
                    Some(expected)
                );
            }
            let Some(crate::views::btw_overlay::BtwOverlayState::Done { content, .. }) =
                &agent_ref(&app, id).btw_state
            else {
                panic!("expected completed side question");
            };
            assert_eq!(content.text(), "Original reply {count}");
        }
    }
}
/// A plain `[Image #1]` with no record behind it rides the side question as text; the send goes out
/// and the toast says the image is not attached, as it does for a queued prompt.
#[test]
fn btw_with_unbound_placeholder_sends_and_toasts() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("/btw what is [Image #1]");
    let effects = dispatch(
        Action::SendPrompt("/btw what is [Image #1]".into()),
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendBtw { question, images, .. }]
                if question == "what is [Image #1]" && images.is_empty()
        ),
        "the side question still goes out as text, got {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, id)
            .toast
            .as_ref()
            .map(|(message, _)| message.as_str()),
        Some("Image #1 not attached — placeholder sent as text")
    );
    assert_eq!(test_agent(&app, id).prompt.text(), "");
}
