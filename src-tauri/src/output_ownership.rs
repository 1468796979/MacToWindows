//! Coordinate the target keys shared by independent input streams.
//!
//! Each rule engine keeps its own source bindings. This table records only the
//! resulting target holders, including raw input and transient system actions.
//! Callers checkpoint this value together with the engines before injection.

use event_model::{EventKind, EventPhase, ModifierSet, NormalizedEvent, OutputEvent, PhysicalCode};
use macos_adapter::KeyboardSourceId;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum InputStream {
    Local,
    Software(KeyboardSourceId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OutputOwner {
    // A stream can map several physical inputs to the same target.
    Mapped(InputStream, PhysicalCode),
    // Native input targets its own key or button, even when flags are rewritten.
    Native(InputStream),
    Transient(InputStream, u64),
    ApplicationSwitch(InputStream),
}

impl OutputOwner {
    pub(crate) fn for_binding(stream: InputStream, source: PhysicalCode, native: bool) -> Self {
        if native {
            Self::Native(stream)
        } else {
            Self::Mapped(stream, source)
        }
    }

    fn stream(self) -> InputStream {
        match self {
            Self::Mapped(stream, _)
            | Self::Native(stream)
            | Self::Transient(stream, _)
            | Self::ApplicationSwitch(stream) => stream,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OutputOwnership {
    held: HashMap<PhysicalCode, HashSet<OutputOwner>>,
}

impl OutputOwnership {
    pub(crate) fn has_native(&self, stream: InputStream, code: PhysicalCode) -> bool {
        self.held
            .get(&code)
            .is_some_and(|owners| owners.contains(&OutputOwner::Native(stream)))
    }

    /// Filter primitive outputs in order. Expand system key sequences before
    /// calling this; non-key actions pass through without acquiring holders.
    pub(crate) fn filter(
        &mut self,
        owner: OutputOwner,
        outputs: &[OutputEvent],
    ) -> Vec<OutputEvent> {
        let mut filtered = Vec::with_capacity(outputs.len());
        for output in outputs {
            let target = match output {
                OutputEvent::Key { code, phase, .. } => Some((*code, *phase)),
                OutputEvent::MouseButton { button, phase } => {
                    Some((PhysicalCode::MouseButton(*button), *phase))
                }
                _ => None,
            };
            if let Some((code, phase)) = target {
                if !self.transition(owner, code, phase, false) {
                    continue;
                }
            }
            let mut output = output.clone();
            if let OutputEvent::Key {
                code, modifiers, ..
            } = &mut output
            {
                let preserved = if modifier_bits(*code).is_some() {
                    self.preserved_modifiers(owner.stream())
                } else {
                    // A shortcut's main key deliberately selects its own
                    // stream's flags. A mapped Control may only represent a
                    // previous flags rewrite, not an extra requested Ctrl+C.
                    self.other_stream_modifiers(owner.stream())
                };
                *modifiers = merge_modifiers(*modifiers, preserved);
            }
            filtered.push(output);
        }
        filtered
    }

    /// Register a raw event and decide whether its target transition may pass.
    /// An already passed modifier's final Up remains eligible to pass. Unknown
    /// Up/Repeat events never create holders. The caller is responsible for
    /// preserving other held modifier flags on the original native event.
    pub(crate) fn native_pass(&mut self, stream: InputStream, event: &NormalizedEvent) -> bool {
        if !matches!(
            event.kind,
            EventKind::Keyboard | EventKind::FlagsChanged | EventKind::MouseButton
        ) {
            return true;
        }
        let phase = if event.kind == EventKind::FlagsChanged && event.phase == EventPhase::Changed {
            match modifier_bits(event.code) {
                Some((_, candidates)) if event.modifiers.bits() & candidates.bits() != 0 => {
                    EventPhase::Down
                }
                Some(_) => EventPhase::Up,
                None => EventPhase::Changed,
            }
        } else {
            event.phase
        };
        self.transition(OutputOwner::Native(stream), event.code, phase, true)
    }

    /// Modifier state that this stream must not accidentally release. Its own
    /// native Control/Option is excluded so Ctrl+C can become Command+C rather
    /// than Control+Command+C. All mapped/transient holders remain included.
    pub(crate) fn preserved_modifiers(&self, stream: InputStream) -> ModifierSet {
        self.held
            .iter()
            .filter_map(|(code, owners)| {
                let (physical, _) = modifier_bits(*code)?;
                owners
                    .iter()
                    .any(|owner| *owner != OutputOwner::Native(stream))
                    .then_some(physical)
            })
            .fold(ModifierSet::NONE, |flags, modifier| flags | modifier)
    }

    /// Keep an existing semantic alias rather than adding a second spelling of
    /// the same native modifier (Command/Windows or Option/Alt).
    pub(crate) fn modifiers_with_preserved(
        &self,
        stream: InputStream,
        modifiers: ModifierSet,
    ) -> ModifierSet {
        merge_modifiers(modifiers, self.preserved_modifiers(stream))
    }

    fn other_stream_modifiers(&self, stream: InputStream) -> ModifierSet {
        self.held
            .iter()
            .filter_map(|(code, owners)| {
                let (physical, _) = modifier_bits(*code)?;
                owners
                    .iter()
                    .any(|owner| owner.stream() != stream)
                    .then_some(physical)
            })
            .fold(ModifierSet::NONE, |flags, modifier| flags | modifier)
    }

    /// Finish the targets still owned by a retired stream or transaction.
    /// Native holders are released only when the caller has established that
    /// their source has retired; normal pause must not release physical input.
    pub(crate) fn release_owner(&mut self, owner: OutputOwner) -> Vec<OutputEvent> {
        self.release_matching(|candidate| candidate == owner)
    }

    pub(crate) fn release_mapped(&mut self, stream: InputStream) -> Vec<OutputEvent> {
        self.release_matching(|owner| matches!(owner, OutputOwner::Mapped(s, _) if s == stream))
    }

    pub(crate) fn release_stream(&mut self, stream: InputStream) -> Vec<OutputEvent> {
        self.release_matching(|owner| owner.stream() == stream)
    }

    fn release_matching(
        &mut self,
        matches_owner: impl Fn(OutputOwner) -> bool,
    ) -> Vec<OutputEvent> {
        let mut targets: Vec<_> = self
            .held
            .iter()
            .flat_map(|(&code, owners)| owners.iter().copied().map(move |owner| (code, owner)))
            .filter(|&(_, owner)| matches_owner(owner))
            .collect();
        // Finish main keys before their modifiers. Stable order makes cleanup
        // repeatable without depending on HashMap iteration order.
        targets.sort_by_key(|(code, _)| match code {
            PhysicalCode::Key(value) => (modifier_bits(*code).is_some(), 0_u8, *value),
            PhysicalCode::MouseButton(button) => {
                use event_model::MouseButton;
                let button = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Other(number) => u16::from(*number) + 3,
                };
                (false, 1, button)
            }
            PhysicalCode::Scroll(_) => (false, 2, 0),
        });
        let mut releases = Vec::new();
        for (code, owner) in targets {
            let output = match code {
                PhysicalCode::MouseButton(button) => OutputEvent::MouseButton {
                    button,
                    phase: EventPhase::Up,
                },
                _ => OutputEvent::Key {
                    code,
                    phase: EventPhase::Up,
                    // Main-key cleanup retains the current session flags
                    // until its following modifier releases finish them.
                    modifiers: if modifier_bits(code).is_some() {
                        ModifierSet::NONE
                    } else {
                        self.preserved_modifiers(owner.stream())
                    },
                },
            };
            releases.extend(self.filter(owner, &[output]));
        }
        releases
    }

    fn transition(
        &mut self,
        owner: OutputOwner,
        code: PhysicalCode,
        phase: EventPhase,
        raw: bool,
    ) -> bool {
        match phase {
            EventPhase::Down => {
                let owners = self.held.entry(code).or_default();
                let first = owners.is_empty();
                let already_held = owners.contains(&owner);
                owners.insert(owner);
                // A repeated raw Down can be a new software submission. Keep
                // it usable when it does not interfere with another holder.
                first || (raw && already_held && owners.len() == 1)
            }
            EventPhase::Up => {
                let Some(owners) = self.held.get_mut(&code) else {
                    return true;
                };
                owners.remove(&owner);
                if !owners.is_empty() {
                    return false;
                }
                self.held.remove(&code);
                true
            }
            EventPhase::Repeat => self
                .held
                .get(&code)
                .is_none_or(|owners| owners.contains(&owner)),
            _ => true,
        }
    }
}

fn merge_modifiers(mut modifiers: ModifierSet, preserved: ModifierSet) -> ModifierSet {
    for code in [56, 60, 59, 62, 58, 61, 55, 54] {
        let (physical, candidates) = modifier_bits(PhysicalCode::key(code)).unwrap();
        if preserved.contains(physical) && modifiers.bits() & candidates.bits() == 0 {
            modifiers.insert(physical);
        }
    }
    modifiers
}

fn modifier_bits(code: PhysicalCode) -> Option<(ModifierSet, ModifierSet)> {
    let PhysicalCode::Key(code) = code else {
        return None;
    };
    let (physical, candidates) = match code {
        56 => (ModifierSet::LEFT_SHIFT, ModifierSet::LEFT_SHIFT),
        60 => (ModifierSet::RIGHT_SHIFT, ModifierSet::RIGHT_SHIFT),
        59 => (ModifierSet::LEFT_CONTROL, ModifierSet::LEFT_CONTROL),
        62 => (ModifierSet::RIGHT_CONTROL, ModifierSet::RIGHT_CONTROL),
        58 => (
            ModifierSet::LEFT_OPTION,
            ModifierSet::LEFT_OPTION | ModifierSet::LEFT_ALT,
        ),
        61 => (
            ModifierSet::RIGHT_OPTION,
            ModifierSet::RIGHT_OPTION | ModifierSet::RIGHT_ALT,
        ),
        55 => (
            ModifierSet::LEFT_COMMAND,
            ModifierSet::LEFT_COMMAND | ModifierSet::LEFT_WINDOWS,
        ),
        54 => (
            ModifierSet::RIGHT_COMMAND,
            ModifierSet::RIGHT_COMMAND | ModifierSet::RIGHT_WINDOWS,
        ),
        _ => return None,
    };
    Some((physical, candidates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_model::{DeviceSource, EventOrigin, MouseButton};

    fn software(pid: i32) -> InputStream {
        InputStream::Software(KeyboardSourceId {
            process_id: pid,
            process_start: 1,
            marker: 0,
        })
    }

    fn mapped(stream: InputStream) -> OutputOwner {
        OutputOwner::Mapped(stream, PhysicalCode::key(11))
    }

    fn key(code: u16, phase: EventPhase, modifiers: ModifierSet) -> OutputEvent {
        OutputEvent::Key {
            code: PhysicalCode::key(code),
            phase,
            modifiers,
        }
    }

    fn native(code: u16, phase: EventPhase, modifiers: ModifierSet) -> NormalizedEvent {
        NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind: EventKind::Keyboard,
            phase,
            code: PhysicalCode::key(code),
            modifiers,
            location: None,
            scroll: None,
            timestamp_ns: 1,
            app_context: None,
            origin: EventOrigin::Hardware,
        }
    }

    #[test]
    fn target_is_released_only_after_all_sources_release() {
        let mut ownership = OutputOwnership::default();
        let local = mapped(InputStream::Local);
        let remote = mapped(software(10));
        let down = key(8, EventPhase::Down, ModifierSet::NONE);
        let up = key(8, EventPhase::Up, ModifierSet::NONE);
        assert_eq!(ownership.filter(local, &[down.clone()]), [down.clone()]);
        assert!(ownership.filter(remote, &[down]).is_empty());
        assert!(ownership.filter(local, &[up.clone()]).is_empty());
        assert_eq!(ownership.filter(remote, &[up.clone()]), [up]);
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn native_command_survives_a_remote_transient_shortcut() {
        let mut ownership = OutputOwnership::default();
        assert!(ownership.native_pass(
            InputStream::Local,
            &native(55, EventPhase::Down, ModifierSet::LEFT_COMMAND)
        ));
        let owner = OutputOwner::Transient(software(10), 1);
        assert_eq!(
            ownership.filter(
                owner,
                &[
                    key(55, EventPhase::Down, ModifierSet::LEFT_COMMAND),
                    key(12, EventPhase::Down, ModifierSet::LEFT_COMMAND),
                    key(12, EventPhase::Up, ModifierSet::LEFT_COMMAND),
                    key(55, EventPhase::Up, ModifierSet::NONE),
                ]
            ),
            [
                key(12, EventPhase::Down, ModifierSet::LEFT_COMMAND),
                key(12, EventPhase::Up, ModifierSet::LEFT_COMMAND),
            ]
        );
        assert_eq!(
            ownership.preserved_modifiers(software(10)),
            ModifierSet::LEFT_COMMAND
        );
        assert!(ownership.native_pass(
            InputStream::Local,
            &native(55, EventPhase::Up, ModifierSet::NONE)
        ));
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn shortcut_flags_exclude_own_native_control_and_keep_other_holders() {
        let mut ownership = OutputOwnership::default();
        let remote = software(10);
        ownership.native_pass(
            remote,
            &native(59, EventPhase::Down, ModifierSet::LEFT_CONTROL),
        );
        ownership.native_pass(
            InputStream::Local,
            &native(56, EventPhase::Down, ModifierSet::LEFT_SHIFT),
        );
        ownership.filter(
            mapped(remote),
            &[key(58, EventPhase::Down, ModifierSet::LEFT_OPTION)],
        );
        let expected = ModifierSet::LEFT_COMMAND | ModifierSet::LEFT_SHIFT;
        assert_eq!(
            ownership.filter(
                mapped(remote),
                &[key(8, EventPhase::Down, ModifierSet::LEFT_COMMAND)]
            ),
            [key(8, EventPhase::Down, expected)]
        );
        assert!(!expected.contains(ModifierSet::LEFT_CONTROL));
        assert_eq!(
            ownership.preserved_modifiers(remote),
            ModifierSet::LEFT_SHIFT | ModifierSet::LEFT_OPTION
        );
    }

    #[test]
    fn native_up_waits_for_mapped_holder_and_unknown_up_cannot_release_it() {
        let mut ownership = OutputOwnership::default();
        let remote = software(10);
        let down = key(55, EventPhase::Down, ModifierSet::LEFT_COMMAND);
        ownership.filter(mapped(remote), &[down]);
        assert!(!ownership.native_pass(
            InputStream::Local,
            &native(55, EventPhase::Down, ModifierSet::LEFT_COMMAND)
        ));
        assert!(
            !ownership.native_pass(software(11), &native(55, EventPhase::Up, ModifierSet::NONE))
        );
        assert!(!ownership.native_pass(
            InputStream::Local,
            &native(55, EventPhase::Up, ModifierSet::NONE)
        ));
        assert_eq!(
            ownership.filter(
                mapped(remote),
                &[key(55, EventPhase::Up, ModifierSet::NONE)]
            ),
            [key(55, EventPhase::Up, ModifierSet::NONE)]
        );
    }

    #[test]
    fn repeated_output_and_raw_down_do_not_acquire_extra_holders() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        let down = native(8, EventPhase::Down, ModifierSet::NONE);
        assert!(ownership.native_pass(stream, &down));
        assert!(ownership.native_pass(stream, &down));
        assert!(ownership.native_pass(stream, &native(8, EventPhase::Repeat, ModifierSet::NONE)));
        assert_eq!(ownership.held[&PhysicalCode::key(8)].len(), 1);
        assert!(ownership.native_pass(stream, &native(8, EventPhase::Up, ModifierSet::NONE)));
        let owner = mapped(stream);
        assert_eq!(
            ownership
                .filter(owner, &[key(8, EventPhase::Down, ModifierSet::NONE)])
                .len(),
            1
        );
        assert!(ownership
            .filter(owner, &[key(8, EventPhase::Down, ModifierSet::NONE)])
            .is_empty());
        assert_eq!(
            ownership.filter(owner, &[key(8, EventPhase::Repeat, ModifierSet::NONE)]),
            [key(8, EventPhase::Repeat, ModifierSet::NONE)]
        );
        assert_eq!(ownership.held[&PhysicalCode::key(8)].len(), 1);
        assert_eq!(
            ownership
                .filter(owner, &[key(8, EventPhase::Up, ModifierSet::NONE)])
                .len(),
            1
        );
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn unknown_up_and_repeat_do_not_create_target_holders() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        for phase in [EventPhase::Up, EventPhase::Repeat] {
            assert!(ownership.native_pass(stream, &native(8, phase, ModifierSet::NONE)));
            assert_eq!(
                ownership.filter(mapped(stream), &[key(8, phase, ModifierSet::NONE)]),
                [key(8, phase, ModifierSet::NONE)]
            );
            assert!(ownership.held.is_empty());
        }
    }

    #[test]
    fn changed_modifier_events_pair_and_support_semantic_aliases() {
        let mut ownership = OutputOwnership::default();
        let mut event = native(55, EventPhase::Changed, ModifierSet::LEFT_WINDOWS);
        event.kind = EventKind::FlagsChanged;
        assert!(ownership.native_pass(InputStream::Local, &event));
        assert!(ownership.has_native(InputStream::Local, PhysicalCode::key(55)));
        assert!(!ownership.has_native(software(10), PhysicalCode::key(55)));
        assert_eq!(
            ownership.preserved_modifiers(software(10)),
            ModifierSet::LEFT_COMMAND
        );
        event.modifiers = ModifierSet::NONE;
        assert!(ownership.native_pass(InputStream::Local, &event));
        assert!(!ownership.has_native(InputStream::Local, PhysicalCode::key(55)));
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn mouse_targets_share_holders_without_reordering_other_outputs() {
        let mut ownership = OutputOwnership::default();
        let mouse = NormalizedEvent {
            source: DeviceSource::Mouse,
            kind: EventKind::MouseButton,
            code: PhysicalCode::MouseButton(MouseButton::Left),
            ..native(0, EventPhase::Down, ModifierSet::NONE)
        };
        assert!(ownership.native_pass(InputStream::Local, &mouse));
        let owner = mapped(software(10));
        let down = OutputEvent::MouseButton {
            button: MouseButton::Left,
            phase: EventPhase::Down,
        };
        let up = OutputEvent::MouseButton {
            button: MouseButton::Left,
            phase: EventPhase::Up,
        };
        let scroll = OutputEvent::Scroll {
            delta_x: 1.0,
            delta_y: 2.0,
        };
        assert_eq!(
            ownership.filter(owner, &[down, scroll.clone(), up]),
            [scroll]
        );
        assert!(ownership.native_pass(
            InputStream::Local,
            &NormalizedEvent {
                phase: EventPhase::Up,
                ..mouse
            }
        ));
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn transient_ids_and_application_switch_keep_distinct_holders() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        let first = OutputOwner::Transient(stream, 1);
        let second = OutputOwner::Transient(stream, 2);
        let switch = OutputOwner::ApplicationSwitch(stream);
        let down = key(55, EventPhase::Down, ModifierSet::LEFT_COMMAND);
        let up = key(55, EventPhase::Up, ModifierSet::NONE);
        assert_eq!(ownership.filter(switch, &[down.clone()]).len(), 1);
        assert!(ownership.filter(first, &[down.clone()]).is_empty());
        assert!(ownership.filter(second, &[down]).is_empty());
        assert!(ownership.filter(first, &[up.clone()]).is_empty());
        assert!(ownership.filter(switch, &[up.clone()]).is_empty());
        assert_eq!(ownership.filter(second, &[up.clone()]), [up]);
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn existing_semantic_modifier_aliases_are_not_duplicated() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        let owner = mapped(stream);
        for (code, semantic, native) in [
            (55, ModifierSet::LEFT_WINDOWS, ModifierSet::LEFT_COMMAND),
            (54, ModifierSet::RIGHT_WINDOWS, ModifierSet::RIGHT_COMMAND),
            (58, ModifierSet::LEFT_ALT, ModifierSet::LEFT_OPTION),
            (61, ModifierSet::RIGHT_ALT, ModifierSet::RIGHT_OPTION),
        ] {
            let down = key(code, EventPhase::Down, semantic);
            assert_eq!(ownership.filter(owner, &[down.clone()]), [down]);
            assert_eq!(
                ownership.modifiers_with_preserved(stream, semantic),
                semantic
            );
            assert_eq!(ownership.preserved_modifiers(stream), native);
            assert_eq!(
                ownership.release_owner(owner),
                [key(code, EventPhase::Up, ModifierSet::NONE)]
            );
        }
    }

    #[test]
    fn retiring_native_source_releases_only_its_final_targets() {
        let mut ownership = OutputOwnership::default();
        let retired = software(10);
        let current = software(11);
        ownership.native_pass(
            retired,
            &native(59, EventPhase::Down, ModifierSet::LEFT_CONTROL),
        );
        ownership.native_pass(
            retired,
            &native(55, EventPhase::Down, ModifierSet::LEFT_COMMAND),
        );
        ownership.filter(
            mapped(current),
            &[key(55, EventPhase::Down, ModifierSet::LEFT_COMMAND)],
        );
        assert_eq!(
            ownership.release_owner(OutputOwner::Native(retired)),
            [key(59, EventPhase::Up, ModifierSet::LEFT_COMMAND)]
        );
        assert!(!ownership.has_native(retired, PhysicalCode::key(59)));
        assert!(!ownership.has_native(retired, PhysicalCode::key(55)));
        assert_eq!(
            ownership.preserved_modifiers(retired),
            ModifierSet::LEFT_COMMAND
        );
        assert_eq!(
            ownership.release_owner(mapped(current)),
            [key(55, EventPhase::Up, ModifierSet::NONE)]
        );
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn retiring_owner_finishes_main_keys_before_modifiers_and_is_idempotent() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        let owner = OutputOwner::ApplicationSwitch(stream);
        ownership.filter(
            owner,
            &[
                key(55, EventPhase::Down, ModifierSet::LEFT_COMMAND),
                key(48, EventPhase::Down, ModifierSet::LEFT_COMMAND),
            ],
        );
        assert_eq!(
            ownership.release_owner(owner),
            [
                key(48, EventPhase::Up, ModifierSet::LEFT_COMMAND),
                key(55, EventPhase::Up, ModifierSet::NONE),
            ]
        );
        assert!(ownership.release_owner(owner).is_empty());
        assert!(ownership.held.is_empty());
    }

    #[test]
    fn explicit_command_copy_does_not_inherit_its_own_mapped_control() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        let owner = mapped(stream);
        ownership.filter(
            owner,
            &[
                key(55, EventPhase::Down, ModifierSet::LEFT_COMMAND),
                key(
                    59,
                    EventPhase::Down,
                    ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_COMMAND,
                ),
            ],
        );
        for phase in [EventPhase::Down, EventPhase::Repeat, EventPhase::Up] {
            assert_eq!(
                ownership.filter(owner, &[key(8, phase, ModifierSet::LEFT_COMMAND)]),
                [key(8, phase, ModifierSet::LEFT_COMMAND)]
            );
        }
        assert_eq!(
            ownership.preserved_modifiers(stream),
            ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_COMMAND
        );
    }

    #[test]
    fn explicit_shortcut_keeps_other_streams_control_even_with_own_mapped_control() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        ownership.filter(
            mapped(stream),
            &[key(59, EventPhase::Down, ModifierSet::LEFT_CONTROL)],
        );
        ownership.native_pass(
            InputStream::Local,
            &native(59, EventPhase::Down, ModifierSet::LEFT_CONTROL),
        );
        assert_eq!(
            ownership.filter(
                mapped(stream),
                &[key(8, EventPhase::Down, ModifierSet::LEFT_COMMAND)]
            ),
            [key(
                8,
                EventPhase::Down,
                ModifierSet::LEFT_COMMAND | ModifierSet::LEFT_CONTROL
            )]
        );
    }

    #[test]
    fn transient_main_key_uses_explicit_flags_without_same_stream_mapped_option() {
        let mut ownership = OutputOwnership::default();
        let stream = software(10);
        ownership.filter(
            mapped(stream),
            &[key(58, EventPhase::Down, ModifierSet::LEFT_OPTION)],
        );
        assert_eq!(
            ownership.filter(
                OutputOwner::Transient(stream, 1),
                &[key(12, EventPhase::Down, ModifierSet::LEFT_COMMAND)]
            ),
            [key(12, EventPhase::Down, ModifierSet::LEFT_COMMAND)]
        );
        assert_eq!(
            ownership.preserved_modifiers(stream),
            ModifierSet::LEFT_OPTION
        );
    }
}
