//! Runtime regressions use captured output batches, never the system injector.
use super::*;
use event_model::{
    DeviceSource, EventKind, EventOrigin, EventPhase, ModifierSet as M, PhysicalCode,
};
use macos_adapter::KeyboardSourceId;

fn source(pid: i32, generation: u64) -> KeyboardSourceId {
    KeyboardSourceId {
        process_id: pid,
        process_start: generation,
        marker: 17,
    }
}

fn controller(passthrough: bool) -> RuntimeController {
    let mut config = AppConfig::default();
    config.software_input_passthrough = passthrough;
    config
        .profiles
        .get_mut(&config.active_profile)
        .unwrap()
        .keyboard_rules
        .push(mac_to_windows_config::Rule {
            trigger: "key:11".into(),
            action: "key:12".into(),
            priority: 100,
            ..Default::default()
        });
    let mut controller = RuntimeController::from_config(config).unwrap();
    controller.status = RuntimeStatus::Running;
    controller
}

fn event(code: u16, phase: EventPhase, modifiers: M) -> NormalizedEvent {
    NormalizedEvent {
        source: DeviceSource::Keyboard,
        kind: if matches!(code, 54..=56 | 58..=62) {
            EventKind::FlagsChanged
        } else {
            EventKind::Keyboard
        },
        phase,
        code: PhysicalCode::key(code),
        modifiers,
        location: None,
        scroll: None,
        timestamp_ns: 1,
        app_context: None,
        origin: EventOrigin::Injected { marker: 17 },
    }
}

fn output(code: u16, phase: EventPhase, modifiers: M) -> OutputEvent {
    OutputEvent::Key {
        code: PhysicalCode::key(code),
        phase,
        modifiers,
    }
}

fn send(
    controller: &mut RuntimeController,
    id: KeyboardSourceId,
    input: &NormalizedEvent,
    force_full: bool,
) -> (software_keyboard::SoftwareResult, Vec<OutputEvent>) {
    let mut captured = Vec::new();
    let result = controller.process_software_event_marked(id, input, force_full, |outputs, _| {
        captured.extend_from_slice(outputs);
        Ok(())
    });
    (result, captured)
}

fn set_passthrough(controller: &mut RuntimeController, enabled: bool) {
    let mut config = controller.config().unwrap().clone();
    config.software_input_passthrough = enabled;
    assert!(controller
        .apply(RuntimeCommand::ReplaceConfig(config))
        .unwrap()
        .is_empty());
}

#[test]
fn ordinary_software_single_key_mapping_obeys_the_global_switch() {
    for passthrough in [true, false] {
        let mut controller = controller(passthrough);
        let id = source(10, 1);
        for phase in [EventPhase::Down, EventPhase::Repeat, EventPhase::Up] {
            let (result, outputs) = send(&mut controller, id, &event(11, phase, M::NONE), false);
            if passthrough {
                assert_eq!(result.decision, Decision::Pass);
                assert!(outputs.is_empty());
            } else {
                assert_eq!(result.decision, Decision::Suppress);
                assert_eq!(outputs, [output(12, phase, M::NONE)]);
            }
        }
        assert!(!controller.software[&id]
            .engine
            .state()
            .physical_down(PhysicalCode::key(11)));
    }
}

#[test]
fn deskflow_force_full_keeps_mapping_with_both_switch_states() {
    for passthrough in [true, false] {
        let mut controller = controller(passthrough);
        for phase in [EventPhase::Down, EventPhase::Up] {
            let (result, outputs) = send(
                &mut controller,
                source(10, 1),
                &event(11, phase, M::NONE),
                true,
            );
            assert_eq!(result.decision, Decision::Suppress);
            assert_eq!(outputs, [output(12, phase, M::NONE)]);
        }
    }
}

#[test]
fn software_windows_copy_paste_undo_conversions_survive_both_switch_states() {
    for passthrough in [true, false] {
        let mut controller = controller(passthrough);
        let id = source(10, 1);
        assert_eq!(
            send(
                &mut controller,
                id,
                &event(59, EventPhase::Down, M::LEFT_CONTROL),
                false
            )
            .0
            .decision,
            Decision::Pass
        );
        for code in [8, 9, 6] {
            for phase in [EventPhase::Down, EventPhase::Repeat, EventPhase::Up] {
                let (result, outputs) = send(
                    &mut controller,
                    id,
                    &event(code, phase, M::LEFT_CONTROL),
                    false,
                );
                assert_eq!(result.decision, Decision::Suppress);
                assert_eq!(outputs, [output(code, phase, M::LEFT_COMMAND)]);
            }
        }
        let (result, outputs) = send(
            &mut controller,
            id,
            &event(59, EventPhase::Up, M::NONE),
            false,
        );
        assert_eq!(result.decision, Decision::Pass);
        assert!(outputs.is_empty());
    }
}

#[test]
fn switching_policy_preserves_the_binding_of_keys_already_pressed() {
    let mut controller = controller(false);
    let id = source(10, 1);
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(11, EventPhase::Down, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Down, M::NONE)]
    );
    set_passthrough(&mut controller, true);
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(11, EventPhase::Repeat, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Repeat, M::NONE)]
    );
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(11, EventPhase::Up, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Up, M::NONE)]
    );
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(11, EventPhase::Down, M::NONE),
            false
        )
        .0
        .decision,
        Decision::Pass
    );
    set_passthrough(&mut controller, false);
    for phase in [EventPhase::Repeat, EventPhase::Up] {
        let (result, outputs) = send(&mut controller, id, &event(11, phase, M::NONE), false);
        assert_eq!(result.decision, Decision::Pass);
        assert!(outputs.is_empty());
    }
}

#[test]
fn two_software_sources_do_not_release_their_shared_target_early() {
    let mut controller = controller(false);
    let first = source(10, 1);
    let second = source(11, 1);
    assert_eq!(
        send(
            &mut controller,
            first,
            &event(11, EventPhase::Down, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Down, M::NONE)]
    );
    let (result, outputs) = send(
        &mut controller,
        second,
        &event(11, EventPhase::Down, M::NONE),
        false,
    );
    assert_eq!(result.decision, Decision::Suppress);
    assert!(outputs.is_empty());
    let (result, outputs) = send(
        &mut controller,
        first,
        &event(11, EventPhase::Up, M::NONE),
        false,
    );
    assert_eq!(result.decision, Decision::Suppress);
    assert!(outputs.is_empty());
    assert_eq!(
        send(
            &mut controller,
            second,
            &event(11, EventPhase::Up, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Up, M::NONE)]
    );
}

#[test]
fn local_native_target_is_not_released_while_software_still_maps_to_it() {
    let mut controller = controller(false);
    let id = source(10, 1);
    let local_down = NormalizedEvent {
        origin: EventOrigin::Hardware,
        ..event(12, EventPhase::Down, M::NONE)
    };
    assert_eq!(
        controller.process_event_marked(&local_down, |_, _| Ok(())),
        Decision::Pass
    );
    let (result, outputs) = send(
        &mut controller,
        id,
        &event(11, EventPhase::Down, M::NONE),
        false,
    );
    assert_eq!(result.decision, Decision::Suppress);
    assert!(outputs.is_empty());
    assert_eq!(
        controller.process_event_marked(
            &NormalizedEvent {
                phase: EventPhase::Up,
                ..local_down
            },
            |_, _| Ok(())
        ),
        Decision::Suppress
    );
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(11, EventPhase::Up, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Up, M::NONE)]
    );
}

#[test]
fn retiring_process_generation_releases_its_native_modifier() {
    let mut controller = controller(true);
    let old = source(10, 1);
    let new = source(10, 2);
    assert_eq!(
        send(
            &mut controller,
            old,
            &event(59, EventPhase::Down, M::LEFT_CONTROL),
            false
        )
        .0
        .decision,
        Decision::Pass
    );
    assert!(controller
        .output_ownership
        .has_native(InputStream::Software(old), PhysicalCode::left_control()));
    let (result, outputs) = send(
        &mut controller,
        new,
        &event(45, EventPhase::Down, M::NONE),
        false,
    );
    assert_eq!(result.decision, Decision::Pass);
    assert_eq!(outputs, [output(59, EventPhase::Up, M::NONE)]);
    assert!(!controller.software.contains_key(&old));
    assert!(controller.software.contains_key(&new));
    assert!(!controller
        .output_ownership
        .has_native(InputStream::Software(old), PhysicalCode::left_control()));
    let (_, outputs) = send(
        &mut controller,
        new,
        &event(8, EventPhase::Down, M::LEFT_CONTROL),
        false,
    );
    assert_eq!(outputs, [output(8, EventPhase::Down, M::LEFT_COMMAND)]);
}

#[test]
fn identity_cache_fill_does_not_change_a_held_keys_route() {
    let mut controller = controller(false);
    let provisional = source(10, 0);
    let confirmed = source(10, 20);
    assert_eq!(
        send(
            &mut controller,
            provisional,
            &event(11, EventPhase::Down, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Down, M::NONE)]
    );
    assert_eq!(
        send(
            &mut controller,
            confirmed,
            &event(11, EventPhase::Up, M::NONE),
            false
        )
        .1,
        [output(12, EventPhase::Up, M::NONE)]
    );
    assert!(controller.software.contains_key(&provisional));
    assert!(!controller.software.contains_key(&confirmed));
    send(
        &mut controller,
        confirmed,
        &event(45, EventPhase::Down, M::NONE),
        false,
    );
    assert!(!controller.software.contains_key(&provisional));
    assert!(controller.software.contains_key(&confirmed));
}

#[test]
fn paused_windows_mode_does_not_apply_software_shortcuts_or_key_rules() {
    for passthrough in [true, false] {
        let mut controller = controller(passthrough);
        controller.status = RuntimeStatus::Disabled;
        let id = source(10, 1);
        for (code, modifiers) in [(11, M::NONE), (8, M::LEFT_CONTROL), (48, M::LEFT_OPTION)] {
            for phase in [EventPhase::Down, EventPhase::Up] {
                let (result, outputs) =
                    send(&mut controller, id, &event(code, phase, modifiers), false);
                assert_eq!(result.decision, Decision::Pass);
                assert!(outputs.is_empty());
            }
        }
        assert_eq!(controller.status(), &RuntimeStatus::Disabled);
    }
}

#[test]
fn disable_releases_software_mapping_and_preserves_following_native_up() {
    let mut controller = controller(false);
    let id = source(10, 1);
    send(
        &mut controller,
        id,
        &event(11, EventPhase::Down, M::NONE),
        false,
    );
    assert_eq!(
        controller.apply(RuntimeCommand::Disable).unwrap(),
        [output(12, EventPhase::Up, M::NONE)]
    );
    let (result, outputs) = send(
        &mut controller,
        id,
        &event(11, EventPhase::Up, M::NONE),
        false,
    );
    assert_eq!(result.decision, Decision::Pass);
    assert!(outputs.is_empty());
}

#[test]
fn own_marker_precedes_text_guard_and_does_not_create_software_state() {
    let mut controller = controller(false);
    let own = controller.own_marker();
    let input = NormalizedEvent {
        origin: EventOrigin::TextInjection { marker: own },
        ..event(11, EventPhase::Down, M::NONE)
    };
    let (result, outputs) = send(&mut controller, source(10, 1), &input, false);
    assert_eq!(result.decision, Decision::Pass);
    assert!(outputs.is_empty());
    assert!(controller.software.is_empty());
    assert_eq!(
        controller
            .diagnostics
            .snapshot()
            .external_keyboard_events_passed,
        0
    );
}

#[test]
fn known_text_is_protected_in_both_modes_without_fake_key_holds() {
    for passthrough in [true, false] {
        let mut controller = controller(passthrough);
        let input = NormalizedEvent {
            origin: EventOrigin::TextInjection { marker: 100 },
            ..event(0, EventPhase::Down, M::NONE)
        };
        for _ in 0..4 {
            let (result, outputs) = send(&mut controller, source(10, 1), &input, false);
            assert_eq!(result.decision, Decision::Pass);
            assert!(outputs.is_empty());
        }
        assert!(controller.software.is_empty());
        assert_eq!(
            controller
                .diagnostics
                .snapshot()
                .external_keyboard_events_passed,
            4
        );
    }
}

#[test]
fn failed_retirement_keeps_releases_recoverable_and_degrades_runtime() {
    let mut controller = controller(false);
    let old = source(10, 1);
    send(
        &mut controller,
        old,
        &event(11, EventPhase::Down, M::NONE),
        false,
    );
    let result = controller.process_software_event_marked(
        source(10, 2),
        &event(45, EventPhase::Down, M::NONE),
        false,
        |_, _| Err(InjectionError::Unavailable),
    );
    assert_eq!(result.decision, Decision::Pass);
    assert_eq!(controller.status(), &RuntimeStatus::Degraded);
    assert!(controller.software.contains_key(&old));
    assert!(controller
        .pending_releases
        .contains(&output(12, EventPhase::Up, M::NONE)));
}

#[test]
fn software_alt_tab_keeps_command_until_alt_release_and_supports_reverse_tabs() {
    let mut controller = controller(true);
    let id = source(10, 1);
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(58, EventPhase::Down, M::LEFT_OPTION),
            false
        )
        .0
        .decision,
        Decision::Pass
    );
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(48, EventPhase::Down, M::LEFT_OPTION),
            false
        )
        .1,
        [
            output(55, EventPhase::Down, M::LEFT_COMMAND),
            output(48, EventPhase::Down, M::LEFT_COMMAND)
        ]
    );
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(48, EventPhase::Up, M::LEFT_OPTION),
            false
        )
        .1,
        [output(48, EventPhase::Up, M::LEFT_COMMAND)]
    );
    let (shift, outputs) = send(
        &mut controller,
        id,
        &event(56, EventPhase::Down, M::LEFT_OPTION | M::LEFT_SHIFT),
        false,
    );
    assert_eq!(shift.decision, Decision::Pass);
    assert_eq!(
        shift.original_modifiers,
        Some(M::LEFT_COMMAND | M::LEFT_SHIFT)
    );
    assert!(outputs.is_empty());
    for phase in [EventPhase::Down, EventPhase::Repeat, EventPhase::Up] {
        assert_eq!(
            send(
                &mut controller,
                id,
                &event(48, phase, M::LEFT_OPTION | M::LEFT_SHIFT),
                false
            )
            .1,
            [output(48, phase, M::LEFT_COMMAND | M::LEFT_SHIFT)]
        );
    }
    let (shift, outputs) = send(
        &mut controller,
        id,
        &event(56, EventPhase::Up, M::LEFT_OPTION),
        false,
    );
    assert_eq!(shift.decision, Decision::Pass);
    assert_eq!(shift.original_modifiers, Some(M::LEFT_COMMAND));
    assert!(outputs.is_empty());
    let (alt, outputs) = send(
        &mut controller,
        id,
        &event(58, EventPhase::Up, M::NONE),
        false,
    );
    assert_eq!(alt.decision, Decision::Pass);
    assert_eq!(outputs, [output(55, EventPhase::Up, M::NONE)]);
    assert!(!controller.software[&id].switcher.active());
}

#[test]
fn active_software_alt_tab_finishes_when_the_policy_changes() {
    let mut controller = controller(true);
    let id = source(10, 1);
    send(
        &mut controller,
        id,
        &event(58, EventPhase::Down, M::LEFT_OPTION),
        false,
    );
    send(
        &mut controller,
        id,
        &event(48, EventPhase::Down, M::LEFT_OPTION),
        false,
    );
    set_passthrough(&mut controller, false);
    assert_eq!(
        send(
            &mut controller,
            id,
            &event(48, EventPhase::Up, M::LEFT_OPTION),
            false
        )
        .1,
        [output(48, EventPhase::Up, M::LEFT_COMMAND)]
    );
    let (result, outputs) = send(
        &mut controller,
        id,
        &event(58, EventPhase::Up, M::NONE),
        false,
    );
    assert_eq!(result.decision, Decision::Pass);
    assert_eq!(outputs, [output(55, EventPhase::Up, M::NONE)]);
    assert_eq!(
        controller
            .output_ownership
            .preserved_modifiers(InputStream::Software(id)),
        M::NONE
    );
}

#[test]
fn local_native_modifier_pressed_while_paused_survives_remote_shortcut_after_enable() {
    let mut controller = controller(true);
    controller.status = RuntimeStatus::Disabled;
    let local_shift = NormalizedEvent {
        origin: EventOrigin::Hardware,
        ..event(56, EventPhase::Down, M::LEFT_SHIFT)
    };
    assert_eq!(
        controller.process_event_marked(&local_shift, |_, _| Ok(())),
        Decision::Pass
    );
    controller.status = RuntimeStatus::Running;
    let (_, outputs) = send(
        &mut controller,
        source(10, 1),
        &event(8, EventPhase::Down, M::LEFT_CONTROL),
        false,
    );
    assert_eq!(
        outputs,
        [output(8, EventPhase::Down, M::LEFT_COMMAND | M::LEFT_SHIFT)]
    );
}
