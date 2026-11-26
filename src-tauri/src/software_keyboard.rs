//! Per-source keyboard state. Submitted text never enters this state machine.
use super::output_ownership::{InputStream, OutputOwner};
use super::{
    ClipboardHotkeyAction, ClipboardHotkeyLatch, KeyboardBypass, NativeKeyboardKeys,
    NormalizedEvent, OutputEvent, RuntimeController, RuntimeStatus, SystemShortcut,
};
use event_model::{Decision, EventKind, EventPhase, ModifierSet as M, PhysicalCode};
use macos_adapter::{
    EventAdapter, EventNormalizer, InjectionError, KeyboardMetadata, KeyboardSourceId,
};
use rule_engine::{Action, CompiledRules, KeyboardMappingPolicy, RuleEngine};
use std::collections::{HashMap, HashSet};

pub(crate) struct SoftwareSession {
    pub engine: RuleEngine,
    pub normalizer: EventNormalizer,
    pub clipboard: ClipboardHotkeyLatch,
    pub switcher: ApplicationSwitch,
    pub last_used: std::time::Instant,
    pub force_full: bool,
    observed_generation: u64,
    raw_keys: HashMap<PhysicalCode, bool>,
    native_keys: NativeKeyboardKeys,
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_model::{DeviceSource, EventOrigin};
    use mac_to_windows_config::{AppConfig, Rule};

    fn source() -> KeyboardSourceId {
        KeyboardSourceId {
            process_id: 501,
            process_start: 10,
            marker: 0,
        }
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
            timestamp_ns: 100_000_000,
            app_context: None,
            origin: EventOrigin::Injected { marker: 100 },
        }
    }
    pub(super) fn controller() -> RuntimeController {
        let mut c = RuntimeController::from_config(AppConfig::default()).unwrap();
        c.status = RuntimeStatus::Running;
        c
    }
    pub(super) fn send(
        c: &mut RuntimeController,
        code: u16,
        phase: EventPhase,
        mods: M,
        outputs: &mut Vec<OutputEvent>,
    ) -> SoftwareResult {
        c.process_software_event_marked(source(), &event(code, phase, mods), false, |batch, _| {
            outputs.extend_from_slice(batch);
            Ok(())
        })
    }

    #[test]
    fn enigo_key_zero_is_protected_even_with_control_and_ascii_payload() {
        let mut metadata = KeyboardMetadata {
            marker: 100,
            process_id: 501,
            source_state: 0,
            raw_flags: 0,
            keycode: 0,
            phase: EventPhase::Down,
            modifiers: M::LEFT_CONTROL,
            unicode_len: 1,
            unicode_is_ascii_a: true,
        };
        assert!(protects_text(&metadata));
        metadata.marker = 0;
        assert!(protects_text(&metadata));
        metadata.unicode_len = 0;
        assert!(!protects_text(&metadata));
        metadata.unicode_len = 1;
        metadata.unicode_is_ascii_a = false;
        assert!(protects_text(&metadata));
        metadata.keycode = 8;
        assert!(!protects_text(&metadata));
    }

    #[test]
    fn confirmed_process_exit_clears_native_modifiers_even_while_paused() {
        let mut controller = controller();
        let mut outputs = Vec::new();
        let control = event(59, EventPhase::Down, M::LEFT_CONTROL);
        assert_eq!(
            send(
                &mut controller,
                59,
                EventPhase::Down,
                M::LEFT_CONTROL,
                &mut outputs
            )
            .decision,
            Decision::Pass
        );
        let survivor = KeyboardSourceId {
            process_id: source().process_id + 1,
            ..source()
        };
        controller.process_software_event_marked(survivor, &control, false, |_, _| Ok(()));
        controller
            .apply(super::super::RuntimeCommand::Disable)
            .unwrap();
        assert!(controller
            .retire_disconnected_software(|_, _| false, |_, _| panic!("unknown is live"))
            .is_empty());
        assert_eq!(
            controller
                .output_ownership
                .modifiers_with_preserved(InputStream::Local, M::NONE),
            M::LEFT_CONTROL
        );

        assert_eq!(
            controller.retire_disconnected_software(
                |id, _| id == source(),
                |_, _| panic!("other source holds Control")
            ),
            vec![source().process_id]
        );
        assert!(!controller
            .output_ownership
            .has_native(InputStream::Software(source()), control.code));
        assert!(controller
            .output_ownership
            .has_native(InputStream::Software(survivor), control.code));
        assert_eq!(
            controller
                .output_ownership
                .modifiers_with_preserved(InputStream::Local, M::NONE),
            M::LEFT_CONTROL
        );

        assert_eq!(
            controller.retire_disconnected_software(
                |id, _| id == survivor,
                |batch, _| {
                    outputs.extend_from_slice(batch);
                    Ok(())
                }
            ),
            vec![survivor.process_id]
        );
        assert_eq!(outputs, vec![key(59, EventPhase::Up, M::NONE)]);
        assert_eq!(
            controller
                .output_ownership
                .modifiers_with_preserved(InputStream::Local, M::NONE),
            M::NONE
        );
        assert_eq!(controller.status(), &RuntimeStatus::Disabled);
    }

    #[test]
    fn process_exit_retries_failed_release_without_dropping_session() {
        let mut controller = controller();
        send(
            &mut controller,
            59,
            EventPhase::Down,
            M::LEFT_CONTROL,
            &mut Vec::new(),
        );
        assert!(controller
            .retire_disconnected_software(
                |id, _| id == source(),
                |_, _| Err(InjectionError::Unavailable)
            )
            .is_empty());
        assert!(controller.software.contains_key(&source()));
        assert!(controller.last_error_message().is_some());
        assert_eq!(controller.status(), &RuntimeStatus::Running);
        assert_eq!(
            controller.retire_disconnected_software(
                |id, _| id == source(),
                |batch, _| {
                    assert_eq!(batch, &[key(59, EventPhase::Up, M::NONE)]);
                    Ok(())
                }
            ),
            vec![source().process_id]
        );
        assert!(!controller.software.contains_key(&source()));
        assert!(controller.last_error_message().is_none());
    }

    #[test]
    fn application_switch_holds_command_across_tabs_and_reverse_switching() {
        let mut c = controller();
        let mut out = Vec::new();
        assert_eq!(
            send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out).decision,
            Decision::Pass
        );
        assert_eq!(
            send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out).decision,
            Decision::Suppress
        );
        send(&mut c, 48, EventPhase::Up, M::LEFT_OPTION, &mut out);
        assert!(!out.iter().any(|o| matches!(
            o,
            OutputEvent::Key {
                code: PhysicalCode::Key(55),
                phase: EventPhase::Up,
                ..
            }
        )));
        let shift = send(
            &mut c,
            56,
            EventPhase::Down,
            M::LEFT_OPTION | M::LEFT_SHIFT,
            &mut out,
        );
        assert_eq!(shift.decision, Decision::Pass);
        assert_eq!(
            shift.original_modifiers,
            Some(M::LEFT_COMMAND | M::LEFT_SHIFT)
        );
        send(
            &mut c,
            48,
            EventPhase::Down,
            M::LEFT_OPTION | M::LEFT_SHIFT,
            &mut out,
        );
        send(
            &mut c,
            48,
            EventPhase::Up,
            M::LEFT_OPTION | M::LEFT_SHIFT,
            &mut out,
        );
        let shift_up = send(&mut c, 56, EventPhase::Up, M::LEFT_OPTION, &mut out);
        assert_eq!(shift_up.decision, Decision::Pass);
        assert_eq!(shift_up.original_modifiers, Some(M::LEFT_COMMAND));
        assert_eq!(
            send(&mut c, 58, EventPhase::Up, M::NONE, &mut out).decision,
            Decision::Pass
        );
        assert_eq!(
            out.iter()
                .filter(|o| matches!(
                    o,
                    OutputEvent::Key {
                        code: PhysicalCode::Key(55),
                        phase: EventPhase::Down,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            out.iter()
                .filter(|o| matches!(
                    o,
                    OutputEvent::Key {
                        code: PhysicalCode::Key(55),
                        phase: EventPhase::Up,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(out.iter().any(|o|matches!(o,OutputEvent::Key{code:PhysicalCode::Key(48),phase:EventPhase::Down,modifiers} if modifiers.contains(M::LEFT_COMMAND|M::LEFT_SHIFT)&&!modifiers.contains(M::LEFT_OPTION))));
        assert!(!c.software[&source()].switcher.active());
    }

    #[test]
    fn application_switch_respects_disabled_rules_and_finishes_after_policy_update() {
        let mut c = controller();
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        let mut config = c.config.clone().unwrap();
        config.software_input_passthrough = false;
        for rule in &mut config
            .profiles
            .get_mut(&config.active_profile.clone())
            .unwrap()
            .keyboard_rules
        {
            if rule.action == "shortcut:switch_application" {
                rule.enabled = false;
            }
        }
        c.apply(super::super::RuntimeCommand::ReplaceConfig(config))
            .unwrap();
        assert_eq!(
            send(&mut c, 48, EventPhase::Up, M::LEFT_OPTION, &mut out).decision,
            Decision::Suppress
        );
        assert_eq!(
            send(&mut c, 58, EventPhase::Up, M::NONE, &mut out).decision,
            Decision::Pass
        );
        let mut config = c.config.clone().unwrap();
        config.software_input_passthrough = true;
        c.apply(super::super::RuntimeCommand::ReplaceConfig(config))
            .unwrap();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        assert_eq!(
            send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out).decision,
            Decision::Pass
        );
    }

    #[test]
    fn interruption_releases_switcher_but_consumes_the_old_tabs_up() {
        let mut c = controller();
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        assert_eq!(
            send(&mut c, 11, EventPhase::Down, M::LEFT_OPTION, &mut out).decision,
            Decision::Pass
        );
        assert_eq!(
            send(&mut c, 48, EventPhase::Up, M::LEFT_OPTION, &mut out).decision,
            Decision::Suppress
        );
        assert_eq!(
            out.iter()
                .filter(|o| matches!(
                    o,
                    OutputEvent::Key {
                        code: PhysicalCode::Key(48),
                        phase: EventPhase::Up,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            out.iter()
                .filter(|o| matches!(
                    o,
                    OutputEvent::Key {
                        code: PhysicalCode::Key(55),
                        phase: EventPhase::Up,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn failed_clipboard_action_keeps_its_original_key_pair_after_policy_change() {
        let mut c = controller();
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        assert_eq!(
            send(&mut c, 9, EventPhase::Down, M::LEFT_OPTION, &mut out).decision,
            Decision::Pass
        );
        c.config.as_mut().unwrap().software_input_passthrough = false;
        assert_eq!(
            send(&mut c, 9, EventPhase::Up, M::LEFT_OPTION, &mut out).decision,
            Decision::Pass
        );
        assert!(out.is_empty());
    }

    #[test]
    fn release_only_function_does_not_swallow_an_already_forwarded_shift_up() {
        let mut config = AppConfig::default();
        config
            .profiles
            .get_mut(&config.active_profile.clone())
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: "modifier_chord:left_shift".into(),
                action: "shortcut:hard_refresh".into(),
                priority: 600,
                ..Default::default()
            });
        let mut c = RuntimeController::from_config(config).unwrap();
        c.status = RuntimeStatus::Running;
        let mut out = Vec::new();
        assert_eq!(
            send(&mut c, 56, EventPhase::Down, M::LEFT_SHIFT, &mut out).decision,
            Decision::Pass
        );
        assert_eq!(
            send(&mut c, 56, EventPhase::Up, M::NONE, &mut out).decision,
            Decision::Pass
        );
        assert!(out.iter().any(|o| matches!(
            o,
            OutputEvent::Key {
                code: PhysicalCode::Key(15),
                phase: EventPhase::Down,
                ..
            }
        )));
        assert!(!out.iter().any(|o| matches!(
            o,
            OutputEvent::Key {
                code: PhysicalCode::Key(56),
                phase: EventPhase::Up,
                ..
            }
        )));
        assert!(!c
            .output_ownership
            .has_native(InputStream::Software(source()), PhysicalCode::key(56)));
    }

    #[test]
    fn actual_alt_tab_rules_take_precedence_over_derived_application_switching() {
        for trigger in [
            "chord:left_option+key:48",
            "chord:left_control|left_option+key:48",
        ] {
            let mut config = AppConfig::default();
            config
                .profiles
                .get_mut(&config.active_profile.clone())
                .unwrap()
                .keyboard_rules
                .push(Rule {
                    trigger: trigger.into(),
                    action: "command:key:11".into(),
                    priority: 1000,
                    ..Default::default()
                });
            let mut c = RuntimeController::from_config(config).unwrap();
            c.status = RuntimeStatus::Running;
            let mut out = Vec::new();
            let mut mods = M::LEFT_OPTION;
            send(&mut c, 58, EventPhase::Down, mods, &mut out);
            if trigger.contains("control") {
                mods |= M::LEFT_CONTROL;
                send(&mut c, 59, EventPhase::Down, mods, &mut out);
            }
            assert_eq!(
                send(&mut c, 48, EventPhase::Down, mods, &mut out).decision,
                Decision::Suppress
            );
            assert!(out.iter().any(|o| matches!(
                o,
                OutputEvent::Key {
                    code: PhysicalCode::Key(11),
                    phase: EventPhase::Down,
                    ..
                }
            )));
            assert!(!c.software[&source()].switcher.active());
        }
    }

    #[test]
    fn local_activity_closes_a_remote_application_switch_before_processing() {
        let mut c = controller();
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        let local = NormalizedEvent {
            origin: EventOrigin::Hardware,
            ..event(11, EventPhase::Down, M::NONE)
        };
        c.process_event_marked(&local, |batch, _| {
            out.extend_from_slice(batch);
            Ok(())
        });
        assert!(!c.software[&source()].switcher.active());
        assert!(out.iter().any(|o| matches!(
            o,
            OutputEvent::Key {
                code: PhysicalCode::Key(55),
                phase: EventPhase::Up,
                ..
            }
        )));
        assert_eq!(
            send(&mut c, 48, EventPhase::Up, M::LEFT_OPTION, &mut out).decision,
            Decision::Suppress
        );
    }

    #[test]
    fn identity_discovery_does_not_hide_a_later_reused_pid_with_held_keys() {
        let mut config = AppConfig::default();
        config.software_input_passthrough = false;
        config
            .profiles
            .get_mut(&config.active_profile.clone())
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: "key:11".into(),
                action: "key:12".into(),
                priority: 900,
                ..Default::default()
            });
        let mut c = RuntimeController::from_config(config).unwrap();
        c.status = RuntimeStatus::Running;
        let old = KeyboardSourceId {
            process_start: 0,
            ..source()
        };
        let first = KeyboardSourceId {
            process_start: 10,
            ..source()
        };
        let replacement = KeyboardSourceId {
            process_start: 20,
            ..source()
        };
        for (id, phase) in [(old, EventPhase::Down), (first, EventPhase::Repeat)] {
            c.process_software_event_marked(id, &event(11, phase, M::NONE), false, |_, _| Ok(()));
        }
        let mut out = Vec::new();
        c.process_software_event_marked(
            replacement,
            &event(11, EventPhase::Down, M::NONE),
            false,
            |batch, _| {
                out.extend_from_slice(batch);
                Ok(())
            },
        );
        assert_eq!(
            out.iter()
                .filter_map(|o| match o {
                    OutputEvent::Key {
                        code: PhysicalCode::Key(12),
                        phase,
                        ..
                    } => Some(*phase),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [EventPhase::Up, EventPhase::Down]
        );
        assert!(!c.software.contains_key(&old));
    }

    #[test]
    fn lost_continuity_forgets_local_native_flags_and_consumed_tab_tails() {
        let mut c = controller();
        let mut out = Vec::new();
        let local = NormalizedEvent {
            origin: EventOrigin::Hardware,
            ..event(56, EventPhase::Down, M::LEFT_SHIFT)
        };
        c.process_event_marked(&local, |_, _| Ok(()));
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        c.reset_keyboard_continuity();
        let releases = c.apply(super::super::RuntimeCommand::Disable).unwrap();
        assert!(releases.iter().any(|o| matches!(
            o,
            OutputEvent::Key {
                code: PhysicalCode::Key(55),
                phase: EventPhase::Up,
                ..
            }
        )));
        c.status = RuntimeStatus::Running;
        out.clear();
        send(&mut c, 59, EventPhase::Down, M::LEFT_CONTROL, &mut out);
        send(&mut c, 8, EventPhase::Down, M::LEFT_CONTROL, &mut out);
        assert!(out.iter().any(|o|matches!(o,OutputEvent::Key{code:PhysicalCode::Key(8),phase:EventPhase::Down,modifiers} if !modifiers.contains(M::LEFT_SHIFT))));
        send(&mut c, 59, EventPhase::Up, M::NONE, &mut out);
        assert_eq!(
            send(&mut c, 48, EventPhase::Down, M::NONE, &mut out).decision,
            Decision::Pass
        );
        assert_eq!(
            send(&mut c, 48, EventPhase::Up, M::NONE, &mut out).decision,
            Decision::Pass
        );
    }

    #[test]
    fn failed_clipboard_recomputes_flags_when_option_is_released_before_v() {
        let mut c = controller();
        c.config.as_mut().unwrap().software_input_passthrough = false;
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        let down = send(&mut c, 9, EventPhase::Down, M::LEFT_OPTION, &mut out);
        assert_eq!(down.decision, Decision::Pass);
        assert!(down
            .original_modifiers
            .is_some_and(|m| m.contains(M::LEFT_WINDOWS) || m.contains(M::LEFT_COMMAND)));
        send(&mut c, 58, EventPhase::Up, M::NONE, &mut out);
        let up = send(&mut c, 9, EventPhase::Up, M::NONE, &mut out);
        assert_eq!(up.decision, Decision::Pass);
        assert_eq!(up.original_modifiers, Some(M::NONE));
    }
}

#[cfg(test)]
mod deferred_tests {
    use super::tests::{controller, send};
    use super::*;
    #[test]
    fn deferred_text_activity_ends_the_same_sources_old_switch_session() {
        let mut c = controller();
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Up, M::LEFT_OPTION, &mut out);
        c.external_keyboard_pending
            .store(true, std::sync::atomic::Ordering::Release);
        out.clear();
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        assert_eq!(
            out.iter()
                .filter_map(|o| match o {
                    OutputEvent::Key {
                        code: PhysicalCode::Key(55),
                        phase,
                        ..
                    } => Some(*phase),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [EventPhase::Up, EventPhase::Down]
        );
    }
    #[test]
    fn interrupted_consumed_tab_does_not_leak_repeat_before_its_up() {
        let mut c = controller();
        let mut out = Vec::new();
        send(&mut c, 58, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 48, EventPhase::Down, M::LEFT_OPTION, &mut out);
        send(&mut c, 11, EventPhase::Down, M::LEFT_OPTION, &mut out);
        let length = out.len();
        assert_eq!(
            send(&mut c, 48, EventPhase::Repeat, M::LEFT_OPTION, &mut out).decision,
            Decision::Suppress
        );
        assert_eq!(
            send(&mut c, 48, EventPhase::Up, M::LEFT_OPTION, &mut out).decision,
            Decision::Suppress
        );
        assert_eq!(out.len(), length);
    }
}

impl SoftwareSession {
    pub fn new(rules: CompiledRules, marker: u64, force_full: bool) -> Self {
        let mut engine = RuleEngine::with_external_output_ownership(rules);
        engine.set_software_input(true);
        Self {
            engine,
            normalizer: EventNormalizer::new(marker),
            clipboard: ClipboardHotkeyLatch::default(),
            switcher: ApplicationSwitch::default(),
            last_used: std::time::Instant::now(),
            force_full,
            observed_generation: 0,
            raw_keys: HashMap::new(),
            native_keys: NativeKeyboardKeys::default(),
        }
    }

    pub fn policy(&self, passthrough: bool) -> KeyboardMappingPolicy {
        if passthrough && !self.force_full {
            KeyboardMappingPolicy::ShortcutsOnly
        } else {
            KeyboardMappingPolicy::Full
        }
    }

    fn idle(&self) -> bool {
        !self.switcher.active()
            && !self.clipboard.active
            && self.raw_keys.is_empty()
            && self.native_keys.0.is_empty()
            && (0..128).all(|code| !self.engine.state().physical_down(PhysicalCode::key(code)))
    }
}

/// Marker 100 is a known channel convention, not an authenticated RustDesk ID.
/// Keycode-zero packets with Unicode are ambiguous even with Ctrl flags, so
/// preserve them rather than guessing Ctrl+A or acquiring a permanent A hold
/// from a Down-only text submission. Deskflow uses the legacy local stream.
pub(crate) fn protects_text(metadata: &KeyboardMetadata) -> bool {
    metadata.keycode == 0
        && (metadata.marker == macos_adapter::RUSTDESK_ENIGO_MARKER || metadata.unicode_len > 0)
}

pub(crate) fn same_process_stream(a: KeyboardSourceId, b: KeyboardSourceId) -> bool {
    a.process_id == b.process_id && a.marker == b.marker
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ApplicationSwitch {
    alt: Option<(PhysicalCode, M)>,
    tab_down: bool,
    // A Tab Down already consumed before the settings changed must finish Up.
    consumed_tabs: HashSet<PhysicalCode>,
}

pub(crate) struct SwitchResult {
    pub decision: Decision,
    pub original_modifiers: Option<M>,
}

impl ApplicationSwitch {
    pub fn active(&self) -> bool {
        self.alt.is_some()
    }

    pub fn consumes(&self, code: PhysicalCode) -> bool {
        self.consumed_tabs.contains(&code)
    }

    pub fn forget_consumed_tabs(&mut self) {
        self.consumed_tabs.clear();
    }

    pub fn release(&mut self, modifiers: M) -> Vec<OutputEvent> {
        let mut result = Vec::new();
        if self.tab_down {
            result.push(key(48, EventPhase::Up, M::LEFT_COMMAND | shifts(modifiers)));
            self.tab_down = false;
        }
        if self.alt.take().is_some() {
            result.push(key(55, EventPhase::Up, without_alt(modifiers)));
        }
        result
    }

    pub fn handle(
        &mut self,
        event: &NormalizedEvent,
        engine: &RuleEngine,
        start_policy: Option<KeyboardMappingPolicy>,
    ) -> Option<SwitchResult> {
        if !matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged) {
            return None;
        }
        if let Some((alt, flag)) = self.alt {
            if event.code == alt
                && (event.phase == EventPhase::Up
                    || (event.kind == EventKind::FlagsChanged
                        && event.phase == EventPhase::Changed
                        && !event.modifiers.contains(flag)))
            {
                return Some(SwitchResult {
                    decision: Decision::Emit(self.release(event.modifiers)),
                    original_modifiers: Some(without_alt(event.modifiers)),
                });
            }
            if event.code == PhysicalCode::key(48) {
                let modifiers = M::LEFT_COMMAND | shifts(event.modifiers);
                match event.phase {
                    EventPhase::Down | EventPhase::Repeat => {
                        self.tab_down = true;
                        self.consumed_tabs.insert(event.code);
                        return Some(SwitchResult {
                            decision: Decision::Emit(vec![key(48, event.phase, modifiers)]),
                            original_modifiers: None,
                        });
                    }
                    EventPhase::Up => {
                        self.tab_down = false;
                        self.consumed_tabs.remove(&event.code);
                        return Some(SwitchResult {
                            decision: Decision::Emit(vec![key(48, EventPhase::Up, modifiers)]),
                            original_modifiers: None,
                        });
                    }
                    _ => {}
                }
            }
            if matches!(event.code, PhysicalCode::Key(56 | 60)) {
                // These modifiers already passed their Down. Keep the original
                // event and its Up, changing only flags while the switcher owns Cmd.
                return Some(SwitchResult {
                    decision: Decision::Pass,
                    original_modifiers: Some(M::LEFT_COMMAND | shifts(event.modifiers)),
                });
            }
        }
        if event.code == PhysicalCode::key(48)
            && self.consumed_tabs.contains(&event.code)
            && matches!(
                event.phase,
                EventPhase::Down | EventPhase::Repeat | EventPhase::Up
            )
        {
            if event.phase == EventPhase::Up {
                self.consumed_tabs.remove(&event.code);
            }
            return Some(SwitchResult {
                decision: Decision::Suppress,
                original_modifiers: None,
            });
        }
        let policy = start_policy?;
        if self.alt.is_some()
            || event.code != PhysicalCode::key(48)
            || event.phase != EventPhase::Down
        {
            return None;
        }
        let actual = engine.action_for_event(event, policy);
        let is_switch = |action: &Action| {
            matches!(action,
            Action::Replace(outputs) if outputs.as_slice() == [OutputEvent::SystemShortcut(SystemShortcut::SwitchApplication)])
        };
        if actual.as_ref().is_some_and(|action| !is_switch(action)) {
            return None;
        }
        if actual.is_none() {
            let allowed = M::LEFT_OPTION | M::RIGHT_OPTION | M::LEFT_SHIFT | M::RIGHT_SHIFT;
            if policy != KeyboardMappingPolicy::ShortcutsOnly
                || event.modifiers.bits() & !allowed.bits() != 0
            {
                return None;
            }
        }
        let alt = [
            (PhysicalCode::left_command(), M::LEFT_COMMAND),
            (PhysicalCode::right_command(), M::RIGHT_COMMAND),
            (PhysicalCode::left_option(), M::LEFT_OPTION),
            (PhysicalCode::right_option(), M::RIGHT_OPTION),
        ]
        .into_iter()
        .find(|(_, bit)| event.modifiers.contains(*bit))?;
        let mut canonical = event.clone();
        canonical.modifiers = M::LEFT_COMMAND | shifts(event.modifiers);
        if actual.is_none()
            && !engine
                .shortcut_action(&canonical)
                .as_ref()
                .is_some_and(is_switch)
        {
            return None;
        }
        self.alt = Some(alt);
        self.tab_down = true;
        self.consumed_tabs.insert(event.code);
        Some(SwitchResult {
            decision: Decision::Emit(vec![
                key(
                    55,
                    EventPhase::Down,
                    M::LEFT_COMMAND | shifts(event.modifiers),
                ),
                key(
                    48,
                    EventPhase::Down,
                    M::LEFT_COMMAND | shifts(event.modifiers),
                ),
            ]),
            original_modifiers: None,
        })
    }
}

fn key(code: u16, phase: EventPhase, modifiers: M) -> OutputEvent {
    OutputEvent::Key {
        code: PhysicalCode::key(code),
        phase,
        modifiers,
    }
}
fn shifts(modifiers: M) -> M {
    M::from_bits(modifiers.bits() & (M::LEFT_SHIFT | M::RIGHT_SHIFT).bits())
}
fn without_alt(modifiers: M) -> M {
    M::from_bits(modifiers.bits() & !(M::LEFT_OPTION | M::RIGHT_OPTION).bits())
}

pub(crate) struct SoftwareResult {
    pub decision: Decision,
    pub original_modifiers: Option<M>,
    pub preserve_original: bool,
}

impl RuntimeController {
    pub(crate) fn reset_keyboard_continuity(&mut self) {
        self.local_switcher.forget_consumed_tabs();
        let _ = self
            .output_ownership
            .release_owner(OutputOwner::Native(InputStream::Local));
        for (id, session) in &mut self.software {
            session.normalizer.reset();
            session.clipboard.reset();
            session.raw_keys.clear();
            session.native_keys.0.clear();
            session.switcher.consumed_tabs.clear();
            // During the gap raw input continues outside our tap. Its Up may
            // already have reached the target, so forget stale native holders
            // without synthesizing releases for someone else's physical input.
            let _ = self
                .output_ownership
                .release_owner(OutputOwner::Native(InputStream::Software(*id)));
        }
    }
    pub(crate) fn observe_text_source<F>(&mut self, id: KeyboardSourceId, inject: &mut F)
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        if id.process_start != 0
            && self.software.iter().any(|(old, session)| {
                same_process_stream(*old, id)
                    && session.observed_generation != 0
                    && session.observed_generation != id.process_start
            })
        {
            if let Err(error) = self.prepare_software_source(id, false, inject) {
                self.degrade_with(format!("文字来源更新时释放失败：{error}"), inject);
                return;
            }
        }
        if id.process_start != 0 {
            for (old, session) in &mut self.software {
                if same_process_stream(*old, id) && session.observed_generation == 0 {
                    session.observed_generation = id.process_start;
                }
            }
        }
        self.close_software_switchers(None, inject);
        self.close_local_switcher(inject);
    }

    pub(crate) fn close_software_switchers<F>(
        &mut self,
        except: Option<KeyboardSourceId>,
        inject: &mut F,
    ) -> bool
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let switching: Vec<_> = self
            .software
            .iter()
            .filter(|(id, session)| Some(**id) != except && session.switcher.active())
            .map(|(id, _)| *id)
            .collect();
        for id in switching {
            let stream = InputStream::Software(id);
            let session = self.software.get_mut(&id).unwrap();
            let checkpoint = session.switcher.clone();
            let outputs = session.switcher.release(M::NONE);
            if let Err(error) = self.dispatch_outputs_for(
                stream,
                OutputOwner::ApplicationSwitch(stream),
                &outputs,
                M::NONE,
                None,
                &mut *inject,
            ) {
                self.software.get_mut(&id).unwrap().switcher = checkpoint;
                self.degrade_with(format!("文字输入前释放切换快捷键失败：{error}"), inject);
                return false;
            }
        }
        true
    }
    fn software_fallback(
        &mut self,
        id: KeyboardSourceId,
        event: &NormalizedEvent,
    ) -> SoftwareResult {
        SoftwareResult {
            decision: if self
                .output_ownership
                .native_pass(InputStream::Software(id), event)
            {
                Decision::Pass
            } else {
                Decision::Suppress
            },
            original_modifiers: None,
            preserve_original: false,
        }
    }
    pub(crate) fn software_preserved_modifiers(&self, id: KeyboardSourceId) -> M {
        self.output_ownership
            .preserved_modifiers(InputStream::Software(self.resolve_software_source(id)))
    }
    pub(crate) fn replace_active_rules(&mut self, rules: CompiledRules) {
        self.engine.replace_rules(rules.clone());
        for session in self.software.values_mut() {
            session.engine.replace_rules(rules.clone());
        }
        self.active_rules = rules;
    }

    pub(crate) fn interrupt_keyboard_gestures(&mut self) {
        self.engine.interrupt_gestures();
        for session in self.software.values_mut() {
            session.engine.interrupt_gestures();
        }
    }

    fn resolve_software_source(&self, id: KeyboardSourceId) -> KeyboardSourceId {
        self.software
            .iter()
            .find_map(|(existing, session)| {
                (same_process_stream(*existing, id)
                    && (id.process_start == 0
                        || (existing.process_start == 0
                            && !session.idle()
                            && (session.observed_generation == 0
                                || session.observed_generation == id.process_start))))
                    .then_some(*existing)
            })
            .unwrap_or(id)
    }

    fn prepare_software_source<F>(
        &mut self,
        id: KeyboardSourceId,
        force_full: bool,
        inject: &mut F,
    ) -> Result<KeyboardSourceId, InjectionError>
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let observed_generation = id.process_start;
        let id = self.resolve_software_source(id);
        let retired: Vec<_> = self
            .software
            .keys()
            .copied()
            .filter(|old| *old != id && same_process_stream(*old, id))
            .collect();
        for old in retired {
            self.retire_software_source(old, inject)?;
        }
        if !self.software.contains_key(&id) {
            // Do not evict a source which may still own an outstanding Up.
            if self.software.len() >= 128 {
                let idle: Vec<_> = self
                    .software
                    .iter()
                    .filter(|(id, s)| {
                        s.idle()
                            && (0..128).all(|code| {
                                !self.output_ownership.has_native(
                                    InputStream::Software(**id),
                                    PhysicalCode::key(code),
                                )
                            })
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for old in idle {
                    self.software.remove(&old);
                }
            }
            if self.software.len() >= 128 {
                return Err(InjectionError::Unavailable);
            }
            self.software.insert(
                id,
                SoftwareSession::new(self.active_rules.clone(), self.own_marker(), force_full),
            );
        }
        let session = self.software.get_mut(&id).unwrap();
        session.force_full = force_full;
        if observed_generation != 0 {
            session.observed_generation = observed_generation;
        }
        Ok(id)
    }

    fn retire_software_source<F>(
        &mut self,
        id: KeyboardSourceId,
        inject: &mut F,
    ) -> Result<(), InjectionError>
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let mut ownership = self.output_ownership.clone();
        let releases = ownership.release_stream(InputStream::Software(id));
        if !releases.is_empty() {
            inject(&releases, self.own_marker())?;
        }
        self.output_ownership = ownership;
        self.software.remove(&id);
        Ok(())
    }

    pub(crate) fn retire_disconnected_software<F>(
        &mut self,
        mut ended: impl FnMut(KeyboardSourceId, std::time::Instant) -> bool,
        mut inject: F,
    ) -> Vec<i32>
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let retired: Vec<_> = self
            .software
            .iter()
            .filter_map(|(&id, session)| {
                let observed = KeyboardSourceId {
                    process_start: session.observed_generation,
                    ..id
                };
                ended(observed, session.last_used).then_some(id)
            })
            .collect();
        let mut disconnected = Vec::with_capacity(retired.len());
        let mut error = None;
        for id in retired {
            match self.retire_software_source(id, &mut inject) {
                Ok(()) => disconnected.push(id.process_id),
                Err(failure) => {
                    self.diagnostics.record_injection_failure();
                    error = Some(format!("软件输入已退出，按键释放尚未完成：{failure}"));
                }
            }
        }
        self.software_cleanup_error = error;
        disconnected
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn process_software_raw<F>(
        &mut self,
        id: KeyboardSourceId,
        raw: macos_adapter::EventTapEvent,
        bypass: KeyboardBypass,
        mut inject: F,
    ) -> SoftwareResult
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let id = match self.prepare_software_source(id, false, &mut inject) {
            Ok(id) => id,
            Err(error) => {
                self.degrade_with(format!("软件来源更新失败：{error}"), &mut inject);
                if let Some(event) = self
                    .software
                    .get_mut(&self.resolve_software_source(id))
                    .and_then(|session| session.normalizer.normalize_software(raw))
                {
                    return self.software_fallback(id, &event);
                }
                return SoftwareResult {
                    decision: Decision::Pass,
                    original_modifiers: None,
                    preserve_original: false,
                };
            }
        };
        let event = self
            .software
            .get_mut(&id)
            .unwrap()
            .normalizer
            .normalize_software(raw);
        match event {
            Some(event) => self.process_software_event_with_bypass(
                id,
                &event,
                false,
                bypass,
                Some(raw),
                inject,
            ),
            None => SoftwareResult {
                decision: Decision::Pass,
                original_modifiers: None,
                preserve_original: false,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn process_software_event_marked<F>(
        &mut self,
        id: KeyboardSourceId,
        event: &NormalizedEvent,
        force_full: bool,
        inject: F,
    ) -> SoftwareResult
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        self.process_software_event_with_bypass(
            id,
            event,
            force_full,
            KeyboardBypass::default(),
            None,
            inject,
        )
    }

    fn process_software_event_with_bypass<F>(
        &mut self,
        id: KeyboardSourceId,
        event: &NormalizedEvent,
        force_full: bool,
        bypass: KeyboardBypass,
        raw: Option<macos_adapter::EventTapEvent>,
        mut inject: F,
    ) -> SoftwareResult
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let pass = || SoftwareResult {
            decision: Decision::Pass,
            original_modifiers: None,
            preserve_original: false,
        };
        self.diagnostics.record_seen();
        if event
            .origin
            .injected_marker()
            .is_some_and(|m| self.adapter.should_filter_marker(m))
        {
            self.diagnostics.record_passed();
            return pass();
        }
        if matches!(event.origin, event_model::EventOrigin::TextInjection { .. }) {
            self.interrupt_keyboard_gestures();
            self.diagnostics.record_external_keyboard_passed();
            self.diagnostics.record_passed();
            return pass();
        }
        let id = match self.prepare_software_source(id, force_full, &mut inject) {
            Ok(id) => id,
            Err(error) => {
                self.degrade_with(format!("软件输入状态无法安全更新：{error}"), &mut inject);
                self.diagnostics.record_passed();
                return self.software_fallback(id, event);
            }
        };
        let external_activity = self.consume_external_keyboard_activity();
        if !self.close_local_switcher(&mut inject)
            || !self.close_software_switchers(
                if external_activity { None } else { Some(id) },
                &mut inject,
            )
        {
            self.diagnostics.record_passed();
            return self.software_fallback(id, event);
        }
        self.engine.interrupt_gestures();
        for (other, session) in &mut self.software {
            if *other != id {
                session.engine.interrupt_gestures();
            }
        }
        if super::is_emergency_hotkey(event) {
            let outputs = self.transition(RuntimeStatus::Disabled);
            let _ = self.flush_with(&outputs, &mut inject);
            self.diagnostics.record_passed();
            return pass();
        }
        let stream = InputStream::Software(id);
        let mut session = self.software.remove(&id).unwrap();
        session.last_used = std::time::Instant::now();
        let checkpoint = session.engine.checkpoint();
        let switch_checkpoint = session.switcher.clone();
        let passthrough = self
            .config
            .as_ref()
            .map_or(true, |c| c.software_input_passthrough);
        let policy = session.policy(passthrough);
        let pinyin = self.input_source.as_ref().is_some_and(|service| {
            let s = service.snapshot();
            s.current_is_pinyin
                || (s.current_id.is_some()
                    && s.current_id == s.last_latin_id
                    && s.last_pinyin_id.is_some())
        });
        session.engine.set_pinyin_active(pinyin);
        let mut original_modifiers = None;
        let mut owner = OutputOwner::Native(stream);
        let mut pass_after_effects = false;
        let mut preserve_original = false;
        let already_bound = session.engine.state().physical_down(event.code)
            || self.output_ownership.has_native(stream, event.code)
            || (super::is_clipboard_key(event) && session.clipboard.active)
            || session.switcher.consumed_tabs.contains(&event.code);
        let mut decision = if let Some(rewrite_flags) = session.raw_keys.get(&event.code).copied() {
            original_modifiers = rewrite_flags.then(|| session.engine.modifiers_for_event(event));
            if event.phase == EventPhase::Up {
                session.raw_keys.remove(&event.code);
            }
            Decision::Pass
        } else if session
            .native_keys
            .forward(event, bypass.applies(event), already_bound)
        {
            session.engine.interrupt_gestures();
            if event.kind == EventKind::FlagsChanged {
                session.clipboard.handle(event);
            }
            if session.switcher.active() {
                let releases = session.switcher.release(event.modifiers);
                if let Err(error) = self.dispatch_outputs_for(
                    stream,
                    OutputOwner::ApplicationSwitch(stream),
                    &releases,
                    event.modifiers,
                    None,
                    &mut inject,
                ) {
                    self.software.insert(id, session);
                    self.degrade_with(format!("切换应用释放失败：{error}"), &mut inject);
                    self.diagnostics.record_passed();
                    return self.software_fallback(id, event);
                }
            }
            preserve_original = true;
            Decision::Pass
        } else if let Some(action) = session.clipboard.handle(event) {
            session.engine.interrupt_gestures();
            if action == ClipboardHotkeyAction::Show
                && !self.clipboard.as_ref().is_some_and(|manager| {
                    manager.try_show(raw.and_then(macos_adapter::EventTapEvent::target_process_id))
                })
            {
                session.clipboard.active = false;
                let rewrite_flags =
                    self.status == RuntimeStatus::Running && policy == KeyboardMappingPolicy::Full;
                session.raw_keys.insert(event.code, rewrite_flags);
                original_modifiers =
                    rewrite_flags.then(|| session.engine.modifiers_for_event(event));
                Decision::Pass
            } else {
                Decision::Suppress
            }
        } else if let Some(result) = session.switcher.handle(
            event,
            &session.engine,
            (self.status == RuntimeStatus::Running
                && policy == KeyboardMappingPolicy::ShortcutsOnly)
                .then_some(policy),
        ) {
            session.engine.interrupt_gestures();
            if result.original_modifiers.is_some() {
                // Modifier Down already passed. Advance its engine binding and
                // preserve its original Up while applying switcher side effects.
                let _ = session.engine.handle_with_policy(event, policy);
                pass_after_effects = true;
            }
            original_modifiers = result.original_modifiers;
            owner = OutputOwner::ApplicationSwitch(stream);
            result.decision
        } else if self.status != RuntimeStatus::Running {
            Decision::Pass
        } else {
            if session.switcher.active() {
                let releases = session.switcher.release(event.modifiers);
                if let Err(error) = self.dispatch_outputs_for(
                    stream,
                    OutputOwner::ApplicationSwitch(stream),
                    &releases,
                    event.modifiers,
                    None,
                    &mut inject,
                ) {
                    self.software.insert(id, session);
                    self.degrade_with(format!("切换应用释放失败：{error}"), &mut inject);
                    self.diagnostics.record_passed();
                    return self.software_fallback(id, event);
                }
            }
            let (decision, binding_kind) = session.engine.handle_with_binding_kind(event, policy);
            owner = OutputOwner::for_binding(stream, event.code, binding_kind);
            if decision == Decision::Pass
                && raw.is_some_and(macos_adapter::EventTapEvent::is_unmanaged_flags_change)
                && policy == KeyboardMappingPolicy::Full
            {
                original_modifiers = Some(session.engine.modifiers_for_event(event));
            }
            decision
        };
        if let Decision::Emit(outputs) = &decision {
            let mut effects = outputs.clone();
            if event.phase == EventPhase::Up && self.output_ownership.has_native(stream, event.code)
            {
                effects.retain(|output| match output {
                    OutputEvent::Key {
                        code,
                        phase: EventPhase::Up,
                        modifiers,
                    } if *code == event.code && owner == OutputOwner::Native(stream) => {
                        original_modifiers = Some(*modifiers);
                        false
                    }
                    _ => true,
                });
                pass_after_effects = true;
            }
            match self.dispatch_outputs_for(
                stream,
                owner,
                &effects,
                session.engine.modifiers_for_event(event),
                raw.and_then(macos_adapter::EventTapEvent::target_process_id),
                &mut inject,
            ) {
                Ok(rejected) => {
                    pass_after_effects |= rejected
                        && super::clipboard_only(&effects)
                        && session.engine.pass_rejected_shortcut(event);
                }
                Err(error) => {
                    session.engine.restore(checkpoint);
                    session.switcher = switch_checkpoint;
                    self.software.insert(id, session);
                    self.degrade_with(format!("软件快捷键处理失败：{error}"), &mut inject);
                    self.diagnostics.record_passed();
                    return self.software_fallback(id, event);
                }
            }
            decision = if pass_after_effects {
                Decision::Pass
            } else {
                Decision::Suppress
            };
        }
        if decision == Decision::Pass {
            if !self.output_ownership.native_pass(stream, event) {
                decision = Decision::Suppress;
            }
            if let Some(flags) = &mut original_modifiers {
                *flags |= self.output_ownership.preserved_modifiers(stream);
            }
        }
        self.software.insert(id, session);
        if decision == Decision::Pass {
            self.diagnostics.record_passed();
        } else {
            self.diagnostics.record_suppressed();
        }
        SoftwareResult {
            decision,
            original_modifiers,
            preserve_original,
        }
    }
}
