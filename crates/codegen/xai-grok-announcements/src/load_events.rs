//! Process-local announcement load signals for optional display consumers.
//! No IO, serialized fields, or dependency on a consumer's completion.

use std::sync::OnceLock;

use tokio::sync::watch;

fn events() -> &'static watch::Sender<bool> {
    static EVENTS: OnceLock<watch::Sender<bool>> = OnceLock::new();
    EVENTS.get_or_init(|| watch::channel(false).0)
}

/// Observe official loads, including a startup prefetch that began before the
/// TUI was created. The retained flag never schedules additional checks.
pub fn subscribe() -> watch::Receiver<bool> {
    subscribe_to(events())
}

fn subscribe_to(events: &watch::Sender<bool>) -> watch::Receiver<bool> {
    let mut receiver = events.subscribe();
    let already_started = *receiver.borrow_and_update();
    if already_started {
        receiver.mark_changed();
    }
    receiver
}

/// Call at the start of a load, before waiting on either cache or network.
/// Repeated `true` values still advance the watch channel's change version.
pub fn notify_started() {
    events().send_replace(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribers_observe_early_loads_and_repeated_loads_without_a_timer() {
        let (sender, _) = watch::channel(false);
        let mut early = subscribe_to(&sender);
        assert!(!early.has_changed().unwrap());
        sender.send_replace(true);
        assert!(early.has_changed().unwrap());
        early.borrow_and_update();
        assert!(!early.has_changed().unwrap());

        // The binary can prefetch before the TUI subscribes.
        let mut late = subscribe_to(&sender);
        assert!(late.has_changed().unwrap());
        late.borrow_and_update();
        assert!(!late.has_changed().unwrap());
        sender.send_replace(true);
        assert!(early.has_changed().unwrap());
        assert!(late.has_changed().unwrap());
    }
}
