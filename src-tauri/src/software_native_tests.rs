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

    fn send(&mut self, keyboard: &Keyboard, kind: KeyboardSourceKind) -> EventTapAction {
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
        let decision = self
            .runtime
            .lock()
            .unwrap()
            .process_event_marked(&event, |batch, _| {
                outputs.extend_from_slice(batch);
                Ok(())
            });
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
