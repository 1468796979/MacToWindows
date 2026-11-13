#[cfg(test)]
mod tests {
    use super::super::{
        is_own_marker, next_marker, AdapterOutcome, EventAdapter, EventTapAction, EventTapEvent,
        InjectionError, SafeAdapter,
    };
    use diagnostics::{PermissionState, TapState};
    #[test]
    fn marker_is_filtered_only_for_own_instance() {
        let marker = next_marker();
        assert!(is_own_marker(marker, marker));
        assert!(!is_own_marker(marker, marker.wrapping_add(1)));
        let adapter = SafeAdapter::new();
        let own = adapter.own_marker();
        assert!(adapter.should_filter_marker(own));
        assert!(!adapter.should_filter_marker(own.wrapping_add(42)));
    }
    #[test]
    fn injection_failure_passes_original_event_through() {
        let adapter = SafeAdapter::new();
        let original = vec![1u8, 2, 3];
        let out = adapter.process(original.clone(), |_event| Err(InjectionError::Unavailable));
        assert_eq!(out, AdapterOutcome::Pass(original));
    }

    #[test]
    fn unknown_permission_is_not_promoted_by_start() {
        let adapter = SafeAdapter::new();
        assert_eq!(adapter.permission_state(), PermissionState::Unknown);

        adapter.start().expect("safe non-macOS adapter can start");

        #[cfg(target_os = "macos")]
        assert_eq!(adapter.permission_state(), PermissionState::Granted);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(adapter.permission_state(), PermissionState::Unknown);
        assert_eq!(adapter.tap_state(), TapState::Running);
    }

    #[test]
    fn only_our_injected_marker_is_suppressed() {
        let adapter = SafeAdapter::new();
        let own = adapter.own_marker();
        let foreign = own.wrapping_add(1);

        assert_eq!(
            adapter.process_injected(1u8, own),
            AdapterOutcome::Pass(1u8)
        );
        assert_eq!(
            adapter.process_injected(2u8, foreign),
            AdapterOutcome::Pass(2u8)
        );
        assert_eq!(adapter.process_injected(3u8, 0), AdapterOutcome::Pass(3u8));
    }

    #[test]
    fn restart_marks_tap_restarting_before_running_again() {
        let adapter = SafeAdapter::new();
        adapter.start().unwrap();
        adapter.restart().unwrap();

        let snapshot = adapter.diagnostics().snapshot();
        assert_eq!(snapshot.tap_state, TapState::Running);
        assert_eq!(snapshot.tap_restarts, 1);
    }

    #[test]
    #[ignore = "requests macOS Input Monitoring and Post Event access; run explicitly for hardware validation"]
    fn permission_request_refreshes_diagnostics() {
        let adapter = SafeAdapter::new();
        let state = adapter.request_permissions();
        assert_eq!(adapter.permission_state(), state);
    }

    #[test]
    fn callback_action_has_explicit_pass_and_suppress_boundary() {
        let event = EventTapEvent::new(42, std::ptr::null_mut());
        assert_eq!(event.event_type(), 42);
        assert_eq!(event.raw_event(), std::ptr::null_mut());
        assert_ne!(EventTapAction::Pass, EventTapAction::Suppress);
        assert!(
            EventTapEvent::new(super::super::TAP_DISABLED_BY_TIMEOUT, std::ptr::null_mut())
                .is_tap_disabled()
        );
    }

    #[test]
    fn user_event_tap_uses_the_session_location() {
        assert_eq!(super::super::platform::EVENT_TAP_LOCATION, 1);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_event_tap_is_an_unavailable_stub() {
        let error = super::super::platform::EventTap::create(0, |_event| EventTapAction::Pass)
            .expect_err("CoreGraphics must never be used off macOS");
        assert_eq!(error, super::super::platform::EventTapError::Unavailable);
        assert_eq!(
            super::super::platform::preflight_permissions(),
            (false, false)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires exclusive macOS Event Tap access; run explicitly for hardware validation"]
    fn macos_event_tap_can_be_created_and_started() {
        let mut tap =
            super::super::platform::EventTap::create(1u64 << 10, |_event| EventTapAction::Pass)
                .expect("Accessibility and Input Monitoring permissions are required");
        tap.start().expect("CoreGraphics event tap should start");
        assert!(tap.is_running());
        tap.stop();
        assert!(!tap.is_running());
    }
}
