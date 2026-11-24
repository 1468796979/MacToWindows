#[cfg(test)]
mod tests {
    use super::super::{Diagnostics, PermissionState, TapState};
    #[test]
    fn atomic_counters_are_recorded() {
        let d = Diagnostics::new();
        d.record_seen();
        d.record_passed();
        d.record_suppressed();
        d.record_injected();
        d.record_injection_failure();
        d.record_tap_restart();
        let s = d.snapshot();
        assert_eq!(s.events_seen, 1);
        assert_eq!(s.events_passed, 1);
        assert_eq!(s.external_keyboard_events_passed, 0);
        assert_eq!(s.events_suppressed, 1);
        assert_eq!(s.events_injected, 1);
        assert_eq!(s.injection_failures, 1);
        assert_eq!(s.tap_restarts, 1);
    }
    #[test]
    fn permission_and_tap_state_are_visible() {
        let d = Diagnostics::new();
        d.set_permission(PermissionState::Granted);
        d.set_tap_state(TapState::Running);
        let s = d.snapshot();
        assert_eq!(s.permission, PermissionState::Granted);
        assert_eq!(s.tap_state, TapState::Running);
    }

    #[test]
    fn external_keyboard_counter_does_not_change_general_event_counters() {
        let d = Diagnostics::new();
        let before = d.snapshot();
        assert_eq!(before.external_keyboard_events_passed, 0);
        d.record_external_keyboard_passed();
        d.record_external_keyboard_passed();
        let after = d.snapshot();
        assert_eq!(after.external_keyboard_events_passed, 2);
        assert_eq!(after.events_seen, before.events_seen);
        assert_eq!(after.events_passed, before.events_passed);
        assert_eq!(after.events_suppressed, before.events_suppressed);
        assert_eq!(after.events_injected, before.events_injected);
        assert_eq!(after.injection_failures, before.injection_failures);
        assert_eq!(after.tap_restarts, before.tap_restarts);
        assert_eq!(after.permission, before.permission);
        assert_eq!(after.tap_state, before.tap_state);
    }

    #[test]
    fn caller_can_record_seen_passed_and_external_once_per_event() {
        let d = Diagnostics::new();
        d.record_seen();
        d.record_passed();
        d.record_external_keyboard_passed();
        let snapshot = d.snapshot();
        assert_eq!(snapshot.events_seen, 1);
        assert_eq!(snapshot.events_passed, 1);
        assert_eq!(snapshot.external_keyboard_events_passed, 1);
    }

    #[test]
    fn external_keyboard_counter_accumulates_across_threads() {
        let d = std::sync::Arc::new(Diagnostics::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let diagnostics = std::sync::Arc::clone(&d);
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        diagnostics.record_external_keyboard_passed();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let snapshot = d.snapshot();
        assert_eq!(snapshot.external_keyboard_events_passed, 2_000);
        assert_eq!(snapshot.events_seen, 0);
        assert_eq!(snapshot.events_passed, 0);
    }
}
