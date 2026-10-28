use super::*;

fn policy(context_window: u64) -> ResumeWindowPolicy {
    ResumeWindowPolicy { context_window }
}

#[test]
fn fit_percent_keeps_prelude_headroom() {
    let window = policy(1_000);
    assert_eq!(window.token_limit(), 950);
    assert!(window.fits(950));
    assert!(!window.fits(951));
}

#[test]
fn rejects_missing_windows_and_copies_without_headroom() {
    let window = policy(1_000);
    assert!(!policy(0).fits(1));
    assert!(!window.fits(951));
    assert!(!window.fits(1_000));
    assert!(!window.fits(1_001));
    assert!(window.fits(949));
}

#[test]
fn resumed_prefix_keeps_only_the_system_head() {
    use xai_grok_sampling_types::conversation::ConversationItem;

    let conversation = vec![
        ConversationItem::system("system"),
        ConversationItem::user("prior work"),
        ConversationItem::assistant("done"),
    ];
    assert_eq!(resume_inherited_prefix_len(&conversation), 1);
}
