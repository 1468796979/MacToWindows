use super::{Decision, InputStream, OutputEvent, RuntimeCommand, RuntimeController, RuntimeStatus};
use event_model::{
    DeviceSource, EventKind, EventOrigin, EventPhase, ModifierSet, MouseButton, NormalizedEvent,
    PhysicalCode, ScrollAxis, ScrollDelta,
};
use mac_to_windows_config::{export_toml, import_toml, AppConfig, Rule};
use macos_adapter::KeyboardSourceId;

fn key(code: u16, phase: EventPhase) -> NormalizedEvent {
    NormalizedEvent {
        source: DeviceSource::Keyboard,
        kind: EventKind::Keyboard,
        phase,
        code: PhysicalCode::key(code),
        modifiers: ModifierSet::NONE,
        location: None,
        scroll: None,
        timestamp_ns: 1,
        app_context: None,
        origin: EventOrigin::Hardware,
    }
}

fn controller_with_mappings(mappings: &[(u16, u16)]) -> RuntimeController {
    let mut config = AppConfig::default();
    let rules = &mut config
        .profiles
        .get_mut(&config.active_profile)
        .unwrap()
        .keyboard_rules;
    for &(source, target) in mappings {
        rules.push(Rule {
            name: format!("{source} → {target}"),
            trigger: format!("key:{source}"),
            action: format!("key:{target}"),
            ..Rule::default()
        });
    }
    let mut controller = RuntimeController::from_config(config).unwrap();
    controller.status = RuntimeStatus::Running;
    controller
}

fn send(
    controller: &mut RuntimeController,
    event: &NormalizedEvent,
    software: Option<(KeyboardSourceId, bool)>,
    delivered: &mut Vec<OutputEvent>,
) {
    let mut inject = |outputs: &[OutputEvent], _marker| {
        delivered.extend_from_slice(outputs);
        Ok(())
    };
    let (decision, flags) = if let Some((id, force_full)) = software {
        let result = controller.process_software_event_marked(id, event, force_full, &mut inject);
        (result.decision, result.original_modifiers)
    } else {
        (controller.process_event_marked(event, &mut inject), None)
    };
    if decision == Decision::Pass {
        delivered.push(OutputEvent::Key {
            code: event.code,
            phase: event.phase,
            modifiers: flags.unwrap_or(event.modifiers),
        });
    }
}

fn phases(delivered: &[OutputEvent], code: u16) -> Vec<EventPhase> {
    delivered
        .iter()
        .filter_map(|output| match output {
            OutputEvent::Key {
                code: target,
                phase,
                ..
            } if *target == PhysicalCode::key(code) => Some(*phase),
            _ => None,
        })
        .collect()
}

#[test]
fn changing_native_key_flags_preserves_its_hold_through_release() {
    use EventPhase::{Down, Repeat, Up};
    let mut controller = controller_with_mappings(&[]);
    let mut delivered = Vec::new();
    let modifier_down = NormalizedEvent {
        kind: EventKind::FlagsChanged,
        modifiers: ModifierSet::LEFT_COMMAND,
        ..key(55, Down)
    };
    let modifier_up = NormalizedEvent {
        phase: Up,
        modifiers: ModifierSet::NONE,
        ..modifier_down.clone()
    };
    for event in [
        key(3, Down),
        modifier_down,
        NormalizedEvent {
            modifiers: ModifierSet::LEFT_COMMAND,
            ..key(3, Repeat)
        },
        NormalizedEvent {
            modifiers: ModifierSet::LEFT_COMMAND,
            ..key(3, Up)
        },
        modifier_up,
    ] {
        send(&mut controller, &event, None, &mut delivered);
    }
    assert_eq!(phases(&delivered, 3), [Down, Repeat, Up]);
    assert!(!controller
        .output_ownership
        .has_native(InputStream::Local, PhysicalCode::key(3)));
}

#[test]
fn local_native_and_mapped_sources_release_target_once_in_every_order() {
    use EventPhase::{Down, Up};
    for (first_down, last_down) in [(0, 1), (1, 0), (0, 2), (2, 0)] {
        for (first_up, last_up) in [(first_down, last_down), (last_down, first_down)] {
            let mut controller = controller_with_mappings(&[(0, 1), (2, 1)]);
            let mut delivered = Vec::new();
            for event in [
                key(first_down, Down),
                key(last_down, Down),
                key(first_up, Up),
            ] {
                send(&mut controller, &event, None, &mut delivered);
            }
            assert_eq!(phases(&delivered, 1), [Down]);
            send(&mut controller, &key(last_up, Up), None, &mut delivered);
            assert_eq!(phases(&delivered, 1), [Down, Up]);
            assert!(controller
                .apply(RuntimeCommand::Disable)
                .unwrap()
                .is_empty());
        }
    }
}

#[test]
fn pause_releases_shared_mappings_only_after_the_last_native_stream_releases() {
    use EventPhase::{Down, Up};
    let mut controller = controller_with_mappings(&[(0, 1)]);
    let software = |pid, full| {
        Some((
            KeyboardSourceId {
                process_id: pid,
                process_start: 1,
                marker: 0,
            },
            full,
        ))
    };
    let mut delivered = Vec::new();
    for (event, origin) in [
        (key(0, Down), None),
        (key(0, Down), software(10, true)),
        (key(1, Down), software(11, false)),
        (key(1, Down), None),
    ] {
        send(&mut controller, &event, origin, &mut delivered);
    }
    assert!(controller
        .apply(RuntimeCommand::Disable)
        .unwrap()
        .is_empty());
    send(
        &mut controller,
        &key(1, Up),
        software(11, false),
        &mut delivered,
    );
    assert_eq!(phases(&delivered, 1), [Down]);
    send(&mut controller, &key(1, Up), None, &mut delivered);
    assert_eq!(phases(&delivered, 1), [Down, Up]);
}

#[test]
fn imported_scroll_rules_pair_every_pulse_without_releasing_a_native_holder() {
    use EventPhase::{Down, Motion, Up};
    let mut config = AppConfig::default();
    let profile = config.profiles.get_mut(&config.active_profile).unwrap();
    for (axis, action) in [("vertical", "key:3"), ("horizontal", "mouse:right")] {
        profile.mouse_rules.push(Rule {
            name: format!("{axis} → {action}"),
            trigger: format!("scroll:{axis}"),
            action: action.to_owned(),
            ..Rule::default()
        });
    }
    let config = import_toml(&export_toml(&config).unwrap()).unwrap();
    let mut controller = RuntimeController::from_config(config).unwrap();
    controller.status = RuntimeStatus::Running;
    let vertical = NormalizedEvent {
        source: DeviceSource::Mouse,
        kind: EventKind::Scroll,
        code: PhysicalCode::Scroll(ScrollAxis::Vertical),
        scroll: Some(ScrollDelta { x: 0.0, y: 1.0 }),
        ..key(0, Motion)
    };
    let horizontal = NormalizedEvent {
        code: PhysicalCode::Scroll(ScrollAxis::Horizontal),
        scroll: Some(ScrollDelta { x: 1.0, y: 0.0 }),
        ..vertical.clone()
    };
    let mut batches = Vec::new();
    for event in [&vertical, &vertical, &horizontal, &horizontal] {
        assert_eq!(
            controller.process_event(event, |outputs| {
                batches.push(outputs.to_vec());
                Ok(())
            }),
            Decision::Suppress
        );
    }
    let key_pair = [Down, Up].map(|phase| OutputEvent::Key {
        code: PhysicalCode::key(3),
        phase,
        modifiers: ModifierSet::NONE,
    });
    let button_pair = [Down, Up].map(|phase| OutputEvent::MouseButton {
        button: MouseButton::Right,
        phase,
    });
    assert_eq!(
        batches,
        [key_pair.clone(), key_pair, button_pair.clone(), button_pair]
    );
    assert_eq!(
        controller.process_event(&key(3, Down), |_| unreachable!()),
        Decision::Pass
    );
    assert_eq!(
        controller.process_event(&vertical, |_| unreachable!()),
        Decision::Suppress
    );
    assert_eq!(
        controller.process_event(&key(3, Up), |_| unreachable!()),
        Decision::Pass
    );
    assert!(controller
        .apply(RuntimeCommand::Disable)
        .unwrap()
        .is_empty());
}

#[test]
fn wheel_modifier_pulse_clears_the_modifier_on_its_release() {
    use EventPhase::{Down, Motion, Up};
    let mut config = AppConfig::default();
    let profile = config.profiles.get_mut(&config.active_profile).unwrap();
    profile.mouse_rules.push(Rule {
        name: "Wheel to Command".into(),
        trigger: "scroll:vertical".into(),
        action: "left_windows".into(),
        ..Rule::default()
    });
    let mut controller = RuntimeController::from_config(config).unwrap();
    controller.status = RuntimeStatus::Running;
    let event = NormalizedEvent {
        source: DeviceSource::Mouse,
        kind: EventKind::Scroll,
        code: PhysicalCode::Scroll(ScrollAxis::Vertical),
        scroll: Some(ScrollDelta { x: 0.0, y: 1.0 }),
        ..key(0, Motion)
    };
    let mut delivered = Vec::new();
    send(&mut controller, &event, None, &mut delivered);
    assert_eq!(
        delivered,
        [
            OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: Down,
                modifiers: ModifierSet::LEFT_WINDOWS,
            },
            OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: Up,
                modifiers: ModifierSet::NONE,
            },
        ]
    );
}
