//! Native object integration tests. Every injected batch is captured in memory;
//! these tests do not create an event tap or post any keyboard events.

use super::*;
use event_model::{EventPhase, ModifierSet as M, PhysicalCode};
use mac_to_windows_config::{AppConfig, Rule};
use macos_adapter::{
    EventNormalizer, EventTapAction, EventTapEvent, KeyboardRoutingState, KeyboardSource,
    KeyboardSourceId, KeyboardSourceKind, RawEventRef,
};
use std::ffi::c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> RawEventRef;
    fn CGEventSourceSetUserData(source: RawEventRef, marker: i64);
    fn CGEventCreateKeyboardEvent(source: RawEventRef, key: u16, down: bool) -> RawEventRef;
    fn CGEventSetIntegerValueField(event: RawEventRef, field: u32, value: i64);
    fn CGEventSetType(event: RawEventRef, event_type: u32);
    fn CGEventGetType(event: RawEventRef) -> u32;
    fn CGEventSetFlags(event: RawEventRef, flags: u64);
    fn CGEventGetFlags(event: RawEventRef) -> u64;
    fn CGEventGetTimestamp(event: RawEventRef) -> u64;
    fn CGEventKeyboardSetUnicodeString(event: RawEventRef, length: usize, text: *const u16);
    fn CGEventKeyboardGetUnicodeString(
        event: RawEventRef,
        capacity: usize,
        length: *mut usize,
        text: *mut u16,
    );
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}

struct Keyboard {
    raw: RawEventRef,
    source: RawEventRef,
    event_type: u32,
    pid: i32,
    marker: u64,
}

impl Keyboard {
    fn new(
        event_type: u32,
        key: u16,
        flags: u64,
        pid: i32,
        marker: u64,
        text: Option<&str>,
    ) -> Self {
        let source = unsafe { CGEventSourceCreate(if pid == 0 { 1 } else { 0 }) };
        assert!(!source.is_null());
        unsafe { CGEventSourceSetUserData(source, marker as i64) };
        let raw = unsafe { CGEventCreateKeyboardEvent(source, key, event_type != 11) };
        assert!(!raw.is_null());
        unsafe {
            CGEventSetType(raw, event_type);
            CGEventSetFlags(raw, flags);
            CGEventSetIntegerValueField(raw, 41, i64::from(pid));
            CGEventSetIntegerValueField(raw, 8, 0);
        }
        if let Some(text) = text {
            let text: Vec<_> = text.encode_utf16().collect();
            unsafe { CGEventKeyboardSetUnicodeString(raw, text.len(), text.as_ptr()) };
        }
        Self {
            raw,
            source,
            event_type,
            pid,
            marker,
        }
    }

    fn input(&self) -> EventTapEvent {
        EventTapEvent::new(self.event_type, self.raw)
    }

    fn metadata(&self) -> macos_adapter::KeyboardMetadata {
        let metadata = self.input().keyboard_metadata().unwrap();
        assert_eq!(metadata.process_id, self.pid);
        assert_eq!(metadata.marker, self.marker);
        metadata
    }

    fn unicode(&self) -> Vec<u16> {
        let mut count = 0;
        unsafe { CGEventKeyboardGetUnicodeString(self.raw, 0, &mut count, std::ptr::null_mut()) };
        let mut text = vec![0; count];
        unsafe {
            CGEventKeyboardGetUnicodeString(self.raw, text.len(), &mut count, text.as_mut_ptr())
        };
        text.truncate(count);
        text
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.raw.cast_const());
            CFRelease(self.source.cast_const());
        }
    }
}

struct Harness {
    runtime: Arc<Mutex<RuntimeController>>,
    diagnostics: Arc<Diagnostics>,
    pending: Arc<AtomicBool>,
    own: u64,
    routes: KeyboardRoutingState,
    local: EventNormalizer,
    local_keys: LocalKeyboard,
    outputs: Vec<OutputEvent>,
}

impl Harness {
    fn new(passthrough: bool, map_a: bool) -> Self {
        let mut config = AppConfig::default();
        config.software_input_passthrough = passthrough;
        if map_a {
            config
                .profiles
                .get_mut(&config.active_profile.clone())
                .unwrap()
                .keyboard_rules
                .push(Rule {
                    trigger: "key:0".into(),
                    action: "key:11".into(),
                    priority: 600,
                    ..Default::default()
                });
        }
        let mut runtime = RuntimeController::from_config(config).unwrap();
        runtime.status = RuntimeStatus::Running;
        let own = runtime.own_marker();
        let diagnostics = runtime.diagnostics();
        let pending = Arc::clone(&runtime.external_keyboard_pending);
        Self {
            runtime: Arc::new(Mutex::new(runtime)),
            diagnostics,
            pending,
            own,
            routes: KeyboardRoutingState::default(),
            local: EventNormalizer::new(own),
            local_keys: LocalKeyboard::default(),
            outputs: Vec::new(),
        }
    }

    fn source(keyboard: &Keyboard, kind: KeyboardSourceKind) -> KeyboardSource {
        KeyboardSource {
            kind,
            id: KeyboardSourceId {
                process_id: keyboard.pid,
                process_start: if keyboard.pid > 0 { 10 } else { 0 },
                marker: if keyboard.pid > 0 { 0 } else { keyboard.marker },
            },
        }
    }

    fn map_to_n(&mut self, trigger: u16) {
        let mut runtime = self.runtime.lock().unwrap();
        let mut config = runtime.config().unwrap().clone();
        config
            .profiles
            .get_mut(&config.active_profile)
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: format!("key:{trigger}"),
                action: "key:45".into(),
                priority: 600,
                ..Default::default()
            });
        runtime
            .apply(RuntimeCommand::ReplaceConfig(config))
            .unwrap();
    }

    fn send(&mut self, keyboard: &Keyboard, kind: KeyboardSourceKind) -> EventTapAction {
        self.send_with_bypass(keyboard, kind, KeyboardBypass::default())
    }

    fn send_with_bypass(
        &mut self,
        keyboard: &Keyboard,
        kind: KeyboardSourceKind,
        bypass: KeyboardBypass,
    ) -> EventTapAction {
        let source = Self::source(keyboard, kind);
        let outputs = &mut self.outputs;
        let routed = route_keyboard_classified(
            keyboard.input(),
            keyboard.metadata(),
            source,
            self.own,
            &mut self.routes,
            &self.runtime,
            &self.diagnostics,
            &self.pending,
            bypass,
            |batch, _| {
                outputs.extend_from_slice(batch);
                Ok(())
            },
        );
        if let Some(action) = routed {
            return action;
        }
        let Some(event) = self.local.normalize(keyboard.input()) else {
            return EventTapAction::Pass;
        };
        let decision = self.runtime.lock().unwrap().process_local_raw(
            keyboard.input(),
            &event,
            &mut self.local_keys,
            bypass,
            || false,
            |batch, _| {
                outputs.extend_from_slice(batch);
                Ok(())
            },
        );
        if decision == Decision::Suppress {
            EventTapAction::Suppress
        } else {
            EventTapAction::Pass
        }
    }
}

fn has_key(outputs: &[OutputEvent], code: u16, phase: EventPhase, modifiers: M) -> bool {
    outputs.iter().any(|output| matches!(output,
        OutputEvent::Key { code:PhysicalCode::Key(actual),phase:actual_phase,modifiers:actual_modifiers }
        if *actual == code && *actual_phase == phase && actual_modifiers.contains(modifiers)))
}

#[test]
fn exact_ctrl_s_keeps_command_down_repeat_and_up_for_each_control_combination() {
    use EventPhase::{Down, Repeat, Up};

    let left = (59, 0x0004_0001);
    let right = (62, 0x0004_2000);
    for (source, pid, passthrough) in [
        (KeyboardSourceKind::HardwareLike, 0, true),
        (KeyboardSourceKind::Software, 501, true),
        (KeyboardSourceKind::Software, 501, false),
    ] {
        for controls in [&[left][..], &[right], &[left, right]] {
            for control_up_first in [false, true] {
                let mut h = Harness::new(passthrough, false);
                let mut flags = 0;
                for &(code, flag) in controls {
                    flags |= flag;
                    let input = Keyboard::new(12, code, flags, pid, 0, None);
                    assert_eq!(h.send(&input, source), EventTapAction::Pass);
                    assert_eq!(input.metadata().raw_flags, flags);
                }
                let down = Keyboard::new(10, 1, flags, pid, 0, None);
                let repeat = Keyboard::new(10, 1, flags, pid, 0, None);
                unsafe { CGEventSetIntegerValueField(repeat.raw, 8, 1) };
                assert_eq!(h.send(&down, source), EventTapAction::Suppress);
                assert_eq!(h.send(&repeat, source), EventTapAction::Suppress);

                if !control_up_first {
                    let up = Keyboard::new(11, 1, flags, pid, 0, None);
                    assert_eq!(h.send(&up, source), EventTapAction::Suppress);
                }
                for &(code, flag) in controls.iter().rev() {
                    flags &= !(flag & 0xffff);
                    if flags == 0x0004_0000 {
                        flags = 0;
                    }
                    let input = Keyboard::new(12, code, flags, pid, 0, None);
                    assert_eq!(h.send(&input, source), EventTapAction::Pass);
                    assert_eq!(input.metadata().raw_flags, flags);
                }
                if control_up_first {
                    let up = Keyboard::new(11, 1, flags, pid, 0, None);
                    assert_eq!(h.send(&up, source), EventTapAction::Suppress);
                }
                assert_eq!(
                    h.outputs,
                    [Down, Repeat, Up].map(|phase| OutputEvent::Key {
                        code: PhysicalCode::key(1),
                        phase,
                        modifiers: M::LEFT_COMMAND,
                    })
                );
            }
        }
    }
}

#[test]
fn exact_ctrl_s_does_not_change_plain_s_or_a_chord_with_extra_modifiers() {
    let left_control = 0x0004_0001;
    let physical = KeyboardSourceKind::HardwareLike;
    let mut h = Harness::new(true, false);
    for phase in [10, 11] {
        let input = Keyboard::new(phase, 1, 0, 0, 0, None);
        assert_eq!(h.send(&input, physical), EventTapAction::Pass);
    }
    assert!(h.outputs.is_empty());
    let ctrl = Keyboard::new(12, 59, left_control, 0, 0, None);
    assert_eq!(h.send(&ctrl, physical), EventTapAction::Pass);
    h.runtime
        .lock()
        .unwrap()
        .apply(RuntimeCommand::Disable)
        .unwrap();
    for phase in [10, 11] {
        let input = Keyboard::new(phase, 1, left_control, 0, 0, None);
        assert_eq!(h.send(&input, physical), EventTapAction::Pass);
        assert_eq!(input.metadata().raw_flags, left_control);
    }
    assert!(h.outputs.is_empty(), "paused mode converted Control+S");

    for (code, extra, mapped_flags) in [
        (56, 0x0002_0002, None),
        (58, 0x0008_0020, Some(M::LEFT_CONTROL | M::LEFT_WINDOWS)),
        (55, 0x0010_0008, Some(M::LEFT_CONTROL | M::LEFT_ALT)),
    ] {
        let mut h = Harness::new(true, false);
        assert_eq!(h.send(&ctrl, physical), EventTapAction::Pass);
        let input = Keyboard::new(12, code, left_control | extra, 0, 0, None);
        h.send(&input, physical);
        h.outputs.clear();
        for phase in [10, 11] {
            let input = Keyboard::new(phase, 1, left_control | extra, 0, 0, None);
            let decision = if mapped_flags.is_some() {
                EventTapAction::Suppress
            } else {
                EventTapAction::Pass
            };
            assert_eq!(h.send(&input, physical), decision);
        }
        let expected: Vec<_> = mapped_flags
            .into_iter()
            .flat_map(|modifiers| {
                [EventPhase::Down, EventPhase::Up].map(|phase| OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase,
                    modifiers,
                })
            })
            .collect();
        assert_eq!(h.outputs, expected);
    }
}

#[test]
fn exact_ctrl_s_keeps_its_flags_without_releasing_other_stream_modifiers() {
    use EventPhase::{Down, Repeat, Up};

    let software = KeyboardSourceKind::Software;
    let physical = KeyboardSourceKind::HardwareLike;
    let ctrl_flags = 0x0004_0001;
    let shift_flags = 0x0002_0002;

    for (saver, saver_pid, other, pid) in [
        (software, 501, physical, 0),
        (software, 501, software, 502),
        (physical, 0, software, 502),
    ] {
        let mut h = Harness::new(true, false);
        let ctrl_down = Keyboard::new(12, 59, ctrl_flags, saver_pid, 0, None);
        let save_down = Keyboard::new(10, 1, ctrl_flags, saver_pid, 0, None);
        let save_repeat = Keyboard::new(10, 1, ctrl_flags, saver_pid, 0, None);
        unsafe { CGEventSetIntegerValueField(save_repeat.raw, 8, 1) };
        let save_up = Keyboard::new(11, 1, ctrl_flags, saver_pid, 0, None);
        let shift_down = Keyboard::new(12, 56, shift_flags, pid, 0, None);
        assert_eq!(h.send(&shift_down, other), EventTapAction::Pass);
        assert_eq!(h.send(&ctrl_down, saver), EventTapAction::Pass);
        assert_eq!(h.send(&save_down, saver), EventTapAction::Suppress);

        // Another modifier arrives after the binding was frozen. Every saved
        // S event must still use only Command, with the extra holders intact.
        let option_flags = 0x0008_0020;
        let option_down = Keyboard::new(12, 58, shift_flags | option_flags, pid, 0, None);
        h.send(&option_down, other);
        let before_repeat = h.outputs.len();
        assert_eq!(h.send(&save_repeat, saver), EventTapAction::Suppress);
        assert_eq!(h.send(&save_up, saver), EventTapAction::Suppress);
        assert_eq!(
            &h.outputs[before_repeat..],
            &[Repeat, Up].map(|phase| OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase,
                modifiers: M::LEFT_COMMAND,
            })
        );
        assert_eq!(
            h.outputs.first(),
            Some(&OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: Down,
                modifiers: M::LEFT_COMMAND,
            })
        );
        let stream = if pid == 0 {
            InputStream::Local
        } else {
            InputStream::Software(Harness::source(&shift_down, other).id)
        };
        assert!(h
            .runtime
            .lock()
            .unwrap()
            .output_ownership
            .has_native(stream, PhysicalCode::key(56)));
        let shift_up = Keyboard::new(12, 56, option_flags, pid, 0, None);
        h.send(&shift_up, other);
        let option_up = Keyboard::new(12, 58, 0, pid, 0, None);
        h.send(&option_up, other);
        assert!(!h
            .runtime
            .lock()
            .unwrap()
            .output_ownership
            .has_native(stream, PhysicalCode::key(56)));
    }
}

#[test]
fn synthetic_control_does_not_make_another_software_streams_s_a_save_shortcut() {
    let mut h = Harness::new(true, false);
    let source = KeyboardSourceKind::Software;
    let ctrl = 0x0004_0001;
    assert_eq!(
        h.send(&Keyboard::new(12, 59, ctrl, 501, 0, None), source),
        EventTapAction::Pass
    );
    for phase in [10, 11] {
        let input = Keyboard::new(phase, 1, ctrl, 502, 0, None);
        assert_eq!(h.send(&input, source), EventTapAction::Pass);
    }
    assert!(h.outputs.is_empty());
}

#[test]
fn disabling_mapping_releases_a_held_save_key_without_releasing_native_control() {
    let mut h = Harness::new(true, false);
    let source = KeyboardSourceKind::HardwareLike;
    let ctrl = 0x0004_0001;
    assert_eq!(
        h.send(&Keyboard::new(12, 59, ctrl, 0, 0, None), source),
        EventTapAction::Pass
    );
    assert_eq!(
        h.send(&Keyboard::new(10, 1, ctrl, 0, 0, None), source),
        EventTapAction::Suppress
    );
    let mut runtime = h.runtime.lock().unwrap();
    assert!(matches!(
        runtime.apply(RuntimeCommand::Disable).unwrap().as_slice(),
        [OutputEvent::Key {
            code: PhysicalCode::Key(1),
            phase: EventPhase::Up,
            ..
        }]
    ));
    assert!(runtime
        .output_ownership
        .has_native(InputStream::Local, PhysicalCode::left_control()));
    assert!(runtime.apply(RuntimeCommand::Disable).unwrap().is_empty());
}

#[test]
fn caps_lock_keeps_its_native_event_and_mapped_modifier_flags() {
    const COMMAND: u64 = 0x0010_0008;
    const CAPS_LOCK: u64 = 0x0001_0000;
    for (source, pid) in [
        (KeyboardSourceKind::HardwareLike, 0),
        (KeyboardSourceKind::Software, 501),
    ] {
        let mut h = Harness::new(false, false);
        let command = Keyboard::new(12, 55, COMMAND, pid, 0, None);
        assert_eq!(h.send(&command, source), EventTapAction::Suppress);
        let caps = Keyboard::new(12, 57, COMMAND | CAPS_LOCK, pid, 0, None);
        assert_eq!(h.send(&caps, source), EventTapAction::Pass);
        assert_eq!(unsafe { CGEventGetType(caps.raw) }, 12);
        assert_eq!(caps.metadata().keycode, 57);
        assert_eq!(caps.metadata().raw_flags, CAPS_LOCK | 0x0008_0020);
        assert!(has_key(&h.outputs, 58, EventPhase::Down, M::LEFT_ALT));
        assert_eq!(h.outputs.len(), 1);
        assert_eq!(h.runtime.lock().unwrap().status(), &RuntimeStatus::Running);
        let release = Keyboard::new(12, 55, CAPS_LOCK, pid, 0, None);
        assert_eq!(h.send(&release, source), EventTapAction::Suppress);
        assert!(has_key(&h.outputs, 58, EventPhase::Up, M::NONE));
    }
}

#[test]
fn focused_clipboard_forwards_navigation_and_freezes_existing_key_sequences() {
    let focused = KeyboardBypass {
        clipboard_focused: true,
        ..Default::default()
    };
    for (source, pid) in [
        (KeyboardSourceKind::HardwareLike, 0),
        (KeyboardSourceKind::Software, 501),
    ] {
        for key in [36, 48, 53, 76, 125, 126] {
            let mut h = Harness::new(false, false);
            h.map_to_n(key);
            let down = Keyboard::new(10, key, 0, pid, 0, None);
            let repeat = Keyboard::new(10, key, 0, pid, 0, None);
            unsafe { CGEventSetIntegerValueField(repeat.raw, 8, 1) };
            let up = Keyboard::new(11, key, 0, pid, 0, None);
            assert_eq!(
                h.send_with_bypass(&down, source, focused),
                EventTapAction::Pass
            );
            assert_eq!(h.send(&repeat, source), EventTapAction::Pass);
            assert_eq!(h.send(&up, source), EventTapAction::Pass);
            assert!(h.outputs.is_empty(), "navigation key {key} was mapped");

            assert_eq!(h.send(&down, source), EventTapAction::Suppress);
            assert_eq!(
                h.send_with_bypass(&up, source, focused),
                EventTapAction::Suppress
            );
            assert!(has_key(&h.outputs, 45, EventPhase::Down, M::NONE));
            assert!(has_key(&h.outputs, 45, EventPhase::Up, M::NONE));
        }
    }
}

#[test]
fn recording_keeps_a_recognized_software_key_raw_until_release() {
    let mut h = Harness::new(false, false);
    h.map_to_n(11);
    let recording = KeyboardBypass {
        recording: true,
        ..Default::default()
    };
    let source = KeyboardSourceKind::Software;
    let down = Keyboard::new(10, 11, 0, 501, 0, Some("b"));
    let up = Keyboard::new(11, 11, 0, 501, 0, None);
    assert_eq!(
        h.send_with_bypass(&down, source, recording),
        EventTapAction::Pass
    );
    assert_eq!(down.metadata().keycode, 11);
    assert_eq!(down.unicode(), vec![u16::from(b'b')]);
    assert_eq!(h.send(&up, source), EventTapAction::Pass);
    assert!(h.outputs.is_empty());
    assert_eq!(h.send(&down, source), EventTapAction::Suppress);
    assert_eq!(
        h.send_with_bypass(&up, source, recording),
        EventTapAction::Suppress
    );
    assert!(has_key(&h.outputs, 45, EventPhase::Down, M::NONE));
    assert!(has_key(&h.outputs, 45, EventPhase::Up, M::NONE));
}

#[test]
fn deskflow_hid_down_and_cg_up_complete_one_legacy_mapping() {
    let mut h = Harness::new(true, true);
    let down = Keyboard::new(10, 0, 0, 0, 0, None);
    let up = Keyboard::new(11, 0, 0, 501, 0, None);
    assert_eq!(
        h.send(&down, KeyboardSourceKind::HardwareLike),
        EventTapAction::Suppress
    );
    assert_eq!(
        h.send(&up, KeyboardSourceKind::Deskflow),
        EventTapAction::Suppress
    );
    assert!(has_key(&h.outputs, 11, EventPhase::Down, M::NONE));
    assert!(has_key(&h.outputs, 11, EventPhase::Up, M::NONE));
    let runtime = h.runtime.lock().unwrap();
    assert!(!runtime.engine.state().physical_down(PhysicalCode::key(0)));
    assert_eq!(
        runtime
            .engine
            .state()
            .logical_down_count(PhysicalCode::key(11)),
        0
    );
    assert!(runtime.software.is_empty());
}

#[test]
fn deskflow_hid_modifier_and_cg_character_keep_windows_copy() {
    let mut h = Harness::new(true, false);
    assert_eq!(
        h.send(
            &Keyboard::new(12, 59, 0x0004_0001, 0, 0, None),
            KeyboardSourceKind::HardwareLike
        ),
        EventTapAction::Pass
    );
    assert_eq!(
        h.send(
            &Keyboard::new(10, 8, 0x0004_0001, 501, 0, None),
            KeyboardSourceKind::Deskflow
        ),
        EventTapAction::Suppress
    );
    assert_eq!(
        h.send(
            &Keyboard::new(11, 8, 0x0004_0001, 501, 0, None),
            KeyboardSourceKind::Deskflow
        ),
        EventTapAction::Suppress
    );
    assert_eq!(
        h.send(
            &Keyboard::new(12, 59, 0, 501, 0, None),
            KeyboardSourceKind::Deskflow
        ),
        EventTapAction::Pass
    );
    assert!(has_key(&h.outputs, 8, EventPhase::Down, M::LEFT_COMMAND));
    assert!(has_key(&h.outputs, 8, EventPhase::Up, M::LEFT_COMMAND));
    assert!(h.runtime.lock().unwrap().software.is_empty());
}

#[test]
fn software_enigo_and_rdev_channels_share_copy_and_release_state() {
    for passthrough in [true, false] {
        let mut h = Harness::new(passthrough, false);
        h.send(
            &Keyboard::new(12, 59, 0x0004_0001, 501, 100, None),
            KeyboardSourceKind::Software,
        );
        assert_eq!(
            h.send(
                &Keyboard::new(10, 8, 0x0004_0001, 501, 100, None),
                KeyboardSourceKind::Software
            ),
            EventTapAction::Suppress
        );
        assert_eq!(
            h.send(
                &Keyboard::new(11, 8, 0x0004_0001, 501, 0, None),
                KeyboardSourceKind::Software
            ),
            EventTapAction::Suppress
        );
        h.send(
            &Keyboard::new(12, 59, 0, 501, 100, None),
            KeyboardSourceKind::Software,
        );
        assert!(has_key(&h.outputs, 8, EventPhase::Down, M::LEFT_COMMAND));
        assert!(has_key(&h.outputs, 8, EventPhase::Up, M::LEFT_COMMAND));
        let runtime = h.runtime.lock().unwrap();
        assert_eq!(runtime.software.len(), 1);
        let id = KeyboardSourceId {
            process_id: 501,
            process_start: 10,
            marker: 0,
        };
        assert!(!runtime.software[&id]
            .engine
            .state()
            .physical_down(PhysicalCode::key(59)));
        assert!(!runtime.software[&id]
            .engine
            .state()
            .physical_down(PhysicalCode::key(8)));
    }
}

#[test]
fn all_identified_key_zero_text_stays_raw_and_does_not_own_local_a() {
    for passthrough in [true, false] {
        for marker in [0, 100] {
            let mut h = Harness::new(passthrough, true);
            for text in ["a", "b", "中", "文", "a"] {
                let raw = Keyboard::new(10, 0, 0x0004_0001, 501, marker, Some(text));
                let before = unsafe {
                    (
                        CGEventGetFlags(raw.raw),
                        CGEventGetTimestamp(raw.raw),
                        CGEventGetType(raw.raw),
                    )
                };
                assert_eq!(
                    h.send(&raw, KeyboardSourceKind::Software),
                    EventTapAction::Pass
                );
                assert_eq!(raw.unicode(), text.encode_utf16().collect::<Vec<_>>());
                assert_eq!(
                    unsafe {
                        (
                            CGEventGetFlags(raw.raw),
                            CGEventGetTimestamp(raw.raw),
                            CGEventGetType(raw.raw),
                        )
                    },
                    before
                );
            }
            assert!(h.outputs.is_empty());
            assert!(h.runtime.lock().unwrap().software.is_empty());
            assert_eq!(
                h.send(
                    &Keyboard::new(10, 0, 0, 0, 0, None),
                    KeyboardSourceKind::HardwareLike
                ),
                EventTapAction::Suppress
            );
            assert_eq!(
                h.send(
                    &Keyboard::new(11, 0, 0, 0, 0, None),
                    KeyboardSourceKind::HardwareLike
                ),
                EventTapAction::Suppress
            );
            assert!(has_key(&h.outputs, 11, EventPhase::Down, M::NONE));
            assert!(has_key(&h.outputs, 11, EventPhase::Up, M::NONE));
        }
    }
}

#[test]
fn own_text_shaped_replay_precedes_text_guard_without_interrupting_gestures() {
    let mut h = Harness::new(true, false);
    let raw = Keyboard::new(10, 0, 0, 501, h.own, Some("a"));
    assert_eq!(
        h.send(&raw, KeyboardSourceKind::OwnInjected),
        EventTapAction::Pass
    );
    assert!(h.outputs.is_empty());
    assert!(!h.pending.load(Ordering::Acquire));
    assert_eq!(h.diagnostics.snapshot().external_keyboard_events_passed, 0);
    assert!(h.runtime.lock().unwrap().software.is_empty());
}

#[test]
fn text_does_not_wait_for_a_busy_runtime_mutex() {
    let h = Harness::new(true, false);
    let runtime = Arc::clone(&h.runtime);
    let diagnostics = Arc::clone(&h.diagnostics);
    let pending = Arc::clone(&h.pending);
    let own = h.own;
    let guard = runtime.lock().unwrap();
    let worker_runtime = Arc::clone(&runtime);
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let raw = Keyboard::new(10, 0, 0, 501, 100, Some("中"));
        let mut routes = KeyboardRoutingState::default();
        let result = route_keyboard_classified(
            raw.input(),
            raw.metadata(),
            Harness::source(&raw, KeyboardSourceKind::Software),
            own,
            &mut routes,
            &worker_runtime,
            &diagnostics,
            &pending,
            KeyboardBypass::default(),
            |_, _| panic!("text cannot inject while runtime is busy"),
        );
        send.send(result).unwrap();
    });
    let received = receive.recv_timeout(std::time::Duration::from_secs(2));
    drop(guard);
    worker.join().unwrap();
    assert_eq!(received.unwrap(), Some(EventTapAction::Pass));
    assert!(h.pending.load(Ordering::Acquire));
}

#[test]
fn lost_software_continuity_does_not_guess_a_new_control_down() {
    let mut h = Harness::new(true, false);
    h.send(
        &Keyboard::new(12, 59, 0x0004_0001, 501, 100, None),
        KeyboardSourceKind::Software,
    );
    h.runtime.lock().unwrap().reset_keyboard_continuity();
    assert_eq!(
        h.send(
            &Keyboard::new(10, 8, 0x0004_0001, 501, 100, None),
            KeyboardSourceKind::Software
        ),
        EventTapAction::Pass
    );
    assert!(!has_key(&h.outputs, 8, EventPhase::Down, M::LEFT_COMMAND));
}
