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
}
