use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::{BitOr, BitOrAssign};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModifierSet(u16);

impl ModifierSet {
    pub const NONE: Self = Self(0);
    pub const LEFT_SHIFT: Self = Self(1 << 0);
    pub const RIGHT_SHIFT: Self = Self(1 << 1);
    pub const LEFT_CONTROL: Self = Self(1 << 2);
    pub const RIGHT_CONTROL: Self = Self(1 << 3);
    pub const LEFT_OPTION: Self = Self(1 << 4);
    pub const RIGHT_OPTION: Self = Self(1 << 5);
    pub const LEFT_COMMAND: Self = Self(1 << 6);
    pub const RIGHT_COMMAND: Self = Self(1 << 7);
    pub const LEFT_WINDOWS: Self = Self(1 << 8);
    pub const RIGHT_WINDOWS: Self = Self(1 << 9);
    pub const LEFT_ALT: Self = Self(1 << 10);
    pub const RIGHT_ALT: Self = Self(1 << 11);

    pub const fn bits(self) -> u16 {
        self.0
    }
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl BitOr for ModifierSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for ModifierSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Modifier {
    LeftShift,
    RightShift,
    LeftControl,
    RightControl,
    LeftOption,
    RightOption,
    LeftCommand,
    RightCommand,
    LeftWindows,
    RightWindows,
    LeftAlt,
    RightAlt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhysicalCode {
    Key(u16),
    MouseButton(MouseButton),
    Scroll(ScrollAxis),
}

impl PhysicalCode {
    pub const fn key(code: u16) -> Self {
        Self::Key(code)
    }
    pub const fn left_control() -> Self {
        Self::Key(59)
    }
    pub const fn left_option() -> Self {
        Self::Key(58)
    }
    pub const fn left_command() -> Self {
        Self::Key(55)
    }
    pub const fn right_control() -> Self {
        Self::Key(62)
    }
    pub const fn right_option() -> Self {
        Self::Key(61)
    }
    pub const fn right_command() -> Self {
        Self::Key(54)
    }
    pub const fn caps_lock() -> Self {
        Self::Key(57)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceSource {
    Keyboard,
    Mouse,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    Keyboard,
    MouseButton,
    MouseMove,
    Scroll,
    FlagsChanged,
    TapLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventPhase {
    Down,
    Up,
    Repeat,
    Changed,
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollDelta {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppContext {
    pub bundle_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventOrigin {
    /// Legacy unmarked key stream, including injectors such as Deskflow.
    /// This is mapping provenance, not proof of a physical device identity.
    Hardware,
    /// A marked event that remains eligible for keycode mapping. The runtime
    /// separately filters its own injected marker to prevent feedback loops.
    Injected { marker: u64 },
    /// An upstream text channel explicitly known to be unsafe for the physical
    /// key ledger. This classification does not infer whether Unicode is present.
    TextInjection { marker: u64 },
}

impl EventOrigin {
    pub fn injected_marker(self) -> Option<u64> {
        match self {
            Self::Injected { marker } | Self::TextInjection { marker } => Some(marker),
            Self::Hardware => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub source: DeviceSource,
    pub kind: EventKind,
    pub phase: EventPhase,
    pub code: PhysicalCode,
    pub modifiers: ModifierSet,
    pub location: Option<Point>,
    pub scroll: Option<ScrollDelta>,
    pub timestamp_ns: u64,
    pub app_context: Option<AppContext>,
    pub origin: EventOrigin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OutputEvent {
    Key {
        code: PhysicalCode,
        phase: EventPhase,
        modifiers: ModifierSet,
    },
    MouseButton {
        button: MouseButton,
        phase: EventPhase,
    },
    /// Continue a mapped mouse-button hold using the source event's position.
    MouseDrag {
        button: MouseButton,
    },
    Scroll {
        delta_x: f64,
        delta_y: f64,
    },
    /// Transform the original scroll event without discarding its precision or phases.
    ScrollTransform {
        invert_x: bool,
        invert_y: bool,
    },
    SystemShortcut(SystemShortcut),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    Pass,
    Suppress,
    Emit(Vec<OutputEvent>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SystemShortcut {
    LockScreen,
    SwitchApplication,
    ShowDesktop,
    CloseWindow,
    HardRefresh,
    SwitchInputSource,
    TogglePinyinEnglish,
    ShowClipboard,
}

/// Reassert held logical modifiers after a complete transient shortcut. The
/// keycodes are macOS target keys; these snapshots do not create new holders.
pub fn modifier_restore_outputs(modifiers: ModifierSet) -> Vec<OutputEvent> {
    [
        (56, ModifierSet::LEFT_SHIFT),
        (60, ModifierSet::RIGHT_SHIFT),
        (59, ModifierSet::LEFT_CONTROL),
        (62, ModifierSet::RIGHT_CONTROL),
        (58, ModifierSet::LEFT_OPTION | ModifierSet::LEFT_ALT),
        (61, ModifierSet::RIGHT_OPTION | ModifierSet::RIGHT_ALT),
        (55, ModifierSet::LEFT_COMMAND | ModifierSet::LEFT_WINDOWS),
        (54, ModifierSet::RIGHT_COMMAND | ModifierSet::RIGHT_WINDOWS),
    ]
    .into_iter()
    .filter(|(_, candidates)| modifiers.bits() & candidates.bits() != 0)
    .map(|(code, _)| OutputEvent::Key {
        code: PhysicalCode::key(code),
        phase: EventPhase::Down,
        modifiers,
    })
    .collect()
}

/// Returns the default Windows physical-key semantic conversion for modifiers.
pub const fn left_control() -> PhysicalCode {
    PhysicalCode::left_control()
}
pub const fn left_option() -> PhysicalCode {
    PhysicalCode::left_option()
}
pub const fn left_command() -> PhysicalCode {
    PhysicalCode::left_command()
}
pub const fn right_control() -> PhysicalCode {
    PhysicalCode::right_control()
}
pub const fn right_option() -> PhysicalCode {
    PhysicalCode::right_option()
}
pub const fn right_command() -> PhysicalCode {
    PhysicalCode::right_command()
}

pub fn windows_physical_modifier_map() -> BTreeMap<Modifier, Modifier> {
    BTreeMap::from([
        (Modifier::LeftShift, Modifier::LeftShift),
        (Modifier::RightShift, Modifier::RightShift),
        (Modifier::LeftControl, Modifier::LeftControl),
        (Modifier::RightControl, Modifier::RightControl),
        (Modifier::LeftOption, Modifier::LeftWindows),
        (Modifier::RightOption, Modifier::RightWindows),
        (Modifier::LeftCommand, Modifier::LeftAlt),
        (Modifier::RightCommand, Modifier::RightAlt),
    ])
}

pub fn default_windows_modifier_map() -> BTreeMap<Modifier, Modifier> {
    windows_physical_modifier_map()
}

pub fn windows_physical_modifier_preset() -> BTreeMap<Modifier, Modifier> {
    windows_physical_modifier_map()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_key_event_round_trips_through_json() {
        let event = NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind: EventKind::Keyboard,
            phase: EventPhase::Down,
            code: PhysicalCode::Key(0),
            modifiers: ModifierSet::LEFT_COMMAND,
            location: None,
            scroll: None,
            timestamp_ns: 42,
            app_context: None,
            origin: EventOrigin::Hardware,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: NormalizedEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn modifier_set_tracks_left_and_right_keys_independently() {
        let modifiers = ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_OPTION;
        assert!(modifiers.contains(ModifierSet::LEFT_CONTROL));
        assert!(modifiers.contains(ModifierSet::RIGHT_OPTION));
        assert!(!modifiers.contains(ModifierSet::RIGHT_CONTROL));
    }

    #[test]
    fn decision_and_output_events_are_serializable() {
        let decision = Decision::Emit(vec![OutputEvent::Key {
            code: PhysicalCode::Key(12),
            phase: EventPhase::Up,
            modifiers: ModifierSet::NONE,
        }]);
        let decoded: Decision =
            serde_json::from_str(&serde_json::to_string(&decision).unwrap()).unwrap();
        assert_eq!(decoded, decision);
    }

    #[test]
    fn input_source_shortcuts_round_trip_through_json() {
        for shortcut in [
            SystemShortcut::SwitchInputSource,
            SystemShortcut::TogglePinyinEnglish,
            SystemShortcut::ShowClipboard,
        ] {
            let encoded = serde_json::to_string(&OutputEvent::SystemShortcut(shortcut)).unwrap();
            let decoded: OutputEvent = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, OutputEvent::SystemShortcut(shortcut));
        }
    }

    #[test]
    fn contextual_mouse_and_scroll_outputs_round_trip() {
        for output in [
            OutputEvent::MouseDrag {
                button: MouseButton::Other(4),
            },
            OutputEvent::ScrollTransform {
                invert_x: true,
                invert_y: false,
            },
        ] {
            let encoded = serde_json::to_string(&output).unwrap();
            assert_eq!(
                serde_json::from_str::<OutputEvent>(&encoded).unwrap(),
                output
            );
        }
    }

    #[test]
    fn modifier_restoration_uses_actual_target_codes_and_deduplicates_aliases() {
        let held = ModifierSet::LEFT_WINDOWS | ModifierSet::LEFT_COMMAND | ModifierSet::RIGHT_ALT;
        let output = modifier_restore_outputs(held);
        assert_eq!(output.len(), 2);
        assert!(output.contains(&OutputEvent::Key {
            code: PhysicalCode::key(55),
            phase: EventPhase::Down,
            modifiers: held
        }));
        assert!(output.contains(&OutputEvent::Key {
            code: PhysicalCode::key(61),
            phase: EventPhase::Down,
            modifiers: held
        }));
        assert!(modifier_restore_outputs(ModifierSet::NONE).is_empty());
    }

    #[test]
    fn caps_lock_uses_macos_keycode_57() {
        assert_eq!(PhysicalCode::caps_lock(), PhysicalCode::key(57));
    }

    #[test]
    fn windows_physical_preset_maps_mac_modifier_semantics() {
        let map = windows_physical_modifier_map();
        assert_eq!(
            map.get(&Modifier::LeftControl),
            Some(&Modifier::LeftControl)
        );
        assert_eq!(map.get(&Modifier::LeftOption), Some(&Modifier::LeftWindows));
        assert_eq!(map.get(&Modifier::LeftCommand), Some(&Modifier::LeftAlt));
        assert_eq!(
            map.get(&Modifier::RightOption),
            Some(&Modifier::RightWindows)
        );
        assert_eq!(map.get(&Modifier::RightCommand), Some(&Modifier::RightAlt));
    }

    #[test]
    fn event_origin_marker_helper_and_text_channel_round_trip() {
        for (origin, marker) in [
            (EventOrigin::Hardware, None),
            (EventOrigin::Injected { marker: 0 }, Some(0)),
            (EventOrigin::Injected { marker: 123 }, Some(123)),
            (EventOrigin::TextInjection { marker: 0 }, Some(0)),
            (EventOrigin::TextInjection { marker: 100 }, Some(100)),
        ] {
            assert_eq!(origin.injected_marker(), marker);
            let json = serde_json::to_string(&origin).unwrap();
            assert_eq!(serde_json::from_str::<EventOrigin>(&json).unwrap(), origin);
        }
        // Existing serialized ordinary injections retain their previous meaning.
        assert_eq!(
            serde_json::from_str::<EventOrigin>(r#"{"Injected":{"marker":100}}"#).unwrap(),
            EventOrigin::Injected { marker: 100 }
        );
    }
}
