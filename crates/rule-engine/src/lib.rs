use event_model::{
    AppContext, EventKind, EventPhase, ModifierSet, NormalizedEvent, OutputEvent, PhysicalCode,
    ScrollAxis,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

const MAX_OUTPUT_EVENTS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuleScope {
    Global,
    App { bundle_id: String },
    Device { identifier: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModifierMatcher {
    pub required: ModifierSet,
    pub exact: bool,
}

impl ModifierMatcher {
    pub fn matches(self, actual: ModifierSet) -> bool {
        if self.exact {
            actual == self.required
        } else {
            actual.contains(self.required)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Trigger {
    Key {
        code: PhysicalCode,
        modifiers: ModifierMatcher,
        phase: EventPhase,
    },
    Chord {
        modifiers: ModifierSet,
        key: PhysicalCode,
        phase: EventPhase,
    },
    ModifierChord {
        modifiers: ModifierSet,
        phase: EventPhase,
    },
    MouseButton {
        button: event_model::MouseButton,
        phase: EventPhase,
    },
    Scroll {
        axis: ScrollAxis,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Pass,
    Suppress,
    Replace(Vec<OutputEvent>),
    InvertScroll,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub enabled: bool,
    pub priority: i32,
    pub scope: RuleScope,
    pub trigger: Trigger,
    pub action: Action,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EngineError {
    #[error("conflicting rules {first} and {second}")]
    Conflict { first: String, second: String },
    #[error("rule {id} creates a direct mapping loop")]
    Loop { id: String },
    #[error("rule {id} emits {count} events, maximum is {max}")]
    TooManyOutputs {
        id: String,
        count: usize,
        max: usize,
    },
}

#[derive(Clone, Debug)]
pub struct CompiledRules {
    rules: Vec<Rule>,
}

impl CompiledRules {
    pub fn compile(input: Vec<Rule>) -> Result<Self, EngineError> {
        let mut rules: Vec<Rule> = Vec::new();
        for rule in input.into_iter().filter(|rule| rule.enabled) {
            if let Action::Replace(outputs) = &rule.action {
                if outputs.len() > MAX_OUTPUT_EVENTS {
                    return Err(EngineError::TooManyOutputs {
                        id: rule.id,
                        count: outputs.len(),
                        max: MAX_OUTPUT_EVENTS,
                    });
                }
                if let Trigger::Key { code, phase, .. }
                | Trigger::Chord {
                    key: code, phase, ..
                } = &rule.trigger
                {
                    if outputs.iter().any(|output| matches!(output, OutputEvent::Key { code: output_code, phase: output_phase, modifiers } if output_code == code && output_phase == phase && *modifiers == ModifierSet::NONE)) {
                        return Err(EngineError::Loop { id: rule.id });
                    }
                }
            }
            for previous in &rules {
                if previous.priority == rule.priority
                    && previous.scope == rule.scope
                    && previous.trigger == rule.trigger
                    && format!("{:?}", previous.action) != format!("{:?}", rule.action)
                {
                    return Err(EngineError::Conflict {
                        first: previous.id.clone(),
                        second: rule.id.clone(),
                    });
                }
            }
            rules.push(rule);
        }
        rules.sort_by(|a, b| {
            scope_rank(&b.scope)
                .cmp(&scope_rank(&a.scope))
                .then_with(|| b.priority.cmp(&a.priority))
        });
        detect_cycles(&rules)?;
        Ok(Self { rules })
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    fn find(&self, event: &NormalizedEvent) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            scope_matches(&rule.scope, event.app_context.as_ref())
                && trigger_matches(&rule.trigger, event)
        })
    }

    fn find_modifier_chord(&self, modifiers: ModifierSet) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            matches!(
                rule.trigger,
                Trigger::ModifierChord {
                    modifiers: required,
                    phase: EventPhase::Up,
                } if required == modifiers
            )
        })
    }
}

fn detect_cycles(rules: &[Rule]) -> Result<(), EngineError> {
    let mut graph: HashMap<PhysicalCode, Vec<(PhysicalCode, String)>> = HashMap::new();
    for rule in rules {
        let from = match &rule.trigger {
            Trigger::Key { code, .. } | Trigger::Chord { key: code, .. } => *code,
            Trigger::ModifierChord { .. } => continue,
            Trigger::MouseButton { button, .. } => PhysicalCode::MouseButton(*button),
            Trigger::Scroll { .. } => continue,
        };
        if let Action::Replace(outputs) = &rule.action {
            for output in outputs {
                let is_plain_key = matches!(
                    output,
                    OutputEvent::Key { modifiers, .. } if *modifiers == ModifierSet::NONE
                );
                if is_plain_key {
                    if let Some(to) = output_code(output) {
                        graph.entry(from).or_default().push((to, rule.id.clone()));
                    }
                }
            }
        }
    }
    for start in graph.keys().copied().collect::<Vec<_>>() {
        let mut visited = HashSet::new();
        if let Some(rule_id) = find_cycle(start, start, &graph, &mut visited) {
            return Err(EngineError::Loop { id: rule_id });
        }
    }
    Ok(())
}

fn find_cycle(
    start: PhysicalCode,
    current: PhysicalCode,
    graph: &HashMap<PhysicalCode, Vec<(PhysicalCode, String)>>,
    visited: &mut HashSet<PhysicalCode>,
) -> Option<String> {
    if !visited.insert(current) {
        return None;
    }
    for (next, rule_id) in graph.get(&current).into_iter().flatten() {
        if *next == start {
            return Some(rule_id.clone());
        }
        if let Some(found) = find_cycle(start, *next, graph, visited) {
            return Some(found);
        }
    }
    None
}

fn scope_rank(scope: &RuleScope) -> u8 {
    match scope {
        RuleScope::Global => 1,
        RuleScope::Device { .. } => 2,
        RuleScope::App { .. } => 3,
    }
}

fn scope_matches(scope: &RuleScope, app: Option<&AppContext>) -> bool {
    match scope {
        RuleScope::Global | RuleScope::Device { .. } => true,
        RuleScope::App { bundle_id } => app.is_some_and(|value| value.bundle_id == *bundle_id),
    }
}

fn trigger_matches(trigger: &Trigger, event: &NormalizedEvent) -> bool {
    match trigger {
        Trigger::Key {
            code,
            modifiers,
            phase,
        } => {
            matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged)
                && event.code == *code
                && event.phase == *phase
                && modifiers.matches(event.modifiers)
        }
        Trigger::Chord {
            modifiers,
            key,
            phase,
        } => {
            matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged)
                && event.code == *key
                && event.phase == *phase
                && event.modifiers.contains(*modifiers)
        }
        Trigger::ModifierChord { .. } => false,
        Trigger::MouseButton { button, phase } => {
            event.kind == EventKind::MouseButton
                && event.code == PhysicalCode::MouseButton(*button)
                && event.phase == *phase
        }
        Trigger::Scroll { axis } => {
            event.kind == EventKind::Scroll && event.code == PhysicalCode::Scroll(*axis)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeState {
    bindings: HashMap<PhysicalCode, Vec<OutputEvent>>,
    logical_down: HashMap<PhysicalCode, u32>,
    physical_down: HashSet<PhysicalCode>,
    active_modifiers: ModifierSet,
    modifier_chord_interrupted: bool,
    modifier_chord_fired: bool,
    shift_taps: HashMap<PhysicalCode, ShiftTapState>,
}

#[derive(Clone, Copy, Debug)]
struct ShiftTapState {
    started_ns: u64,
    interrupted: bool,
}

impl RuntimeState {
    pub fn logical_down_count(&self, code: PhysicalCode) -> u32 {
        self.logical_down.get(&code).copied().unwrap_or(0)
    }

    pub fn physical_down(&self, code: PhysicalCode) -> bool {
        self.physical_down.contains(&code)
    }

    pub fn release_all(&mut self) -> Vec<OutputEvent> {
        let mut output = Vec::new();
        let mut released = HashSet::new();
        for events in self.bindings.values() {
            for event in events {
                if let Some(release) = release_event(event) {
                    if let Some(code) = output_code(&release) {
                        if released.insert(code) {
                            output.push(release);
                        }
                    }
                }
            }
        }
        self.bindings.clear();
        self.logical_down.clear();
        self.physical_down.clear();
        self.active_modifiers = ModifierSet::NONE;
        self.modifier_chord_interrupted = false;
        self.modifier_chord_fired = false;
        self.shift_taps.clear();
        output
    }
}

pub struct RuleEngine {
    rules: CompiledRules,
    state: RuntimeState,
    pinyin_active: bool,
}

impl RuleEngine {
    pub fn new(rules: CompiledRules) -> Self {
        Self {
            rules,
            state: RuntimeState::default(),
            pinyin_active: false,
        }
    }
    pub fn replace_rules(&mut self, rules: CompiledRules) {
        self.rules = rules;
    }
    pub fn state(&self) -> &RuntimeState {
        &self.state
    }
    pub fn release_all(&mut self) -> Vec<OutputEvent> {
        self.state.release_all()
    }

    /// Enables the standalone Shift toggle while the input-source service can
    /// move between the remembered Pinyin and Latin sources.  The service
    /// keeps the toggle available for the Latin half of that pair too, so a
    /// second Shift tap can return to Pinyin.
    pub fn set_pinyin_active(&mut self, active: bool) {
        self.pinyin_active = active;
    }

    pub fn handle(&mut self, event: &NormalizedEvent) -> event_model::Decision {
        let phase = effective_phase(event);
        let modifier = modifier_for_code(event.code);
        let modifiers_before = self.state.active_modifiers;
        if let Some(modifier) = modifier {
            if phase == EventPhase::Down {
                self.state.active_modifiers.insert(modifier);
                if contains_any(
                    self.state.active_modifiers,
                    ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_CONTROL,
                ) && contains_any(
                    self.state.active_modifiers,
                    ModifierSet::LEFT_SHIFT | ModifierSet::RIGHT_SHIFT,
                ) {
                    self.state.modifier_chord_interrupted = false;
                }
                for (shift_code, state) in &mut self.state.shift_taps {
                    if *shift_code != event.code {
                        state.interrupted = true;
                    }
                }
                if self
                    .state
                    .active_modifiers
                    .contains(ModifierSet::LEFT_CONTROL)
                    || self
                        .state
                        .active_modifiers
                        .contains(ModifierSet::RIGHT_CONTROL)
                {
                    for state in self.state.shift_taps.values_mut() {
                        state.interrupted = true;
                    }
                }
            } else if phase == EventPhase::Up {
                self.state.active_modifiers.remove(modifier);
            }
        } else {
            if contains_any(
                self.state.active_modifiers,
                ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_CONTROL,
            ) && contains_any(
                self.state.active_modifiers,
                ModifierSet::LEFT_SHIFT | ModifierSet::RIGHT_SHIFT,
            ) {
                self.state.modifier_chord_interrupted = true;
            }
            for state in self.state.shift_taps.values_mut() {
                state.interrupted = true;
            }
        }
        if let Some(shift_modifier) =
            modifier.filter(|_| phase == EventPhase::Down && is_shift_code(event.code))
        {
            self.state.shift_taps.insert(
                event.code,
                ShiftTapState {
                    started_ns: event.timestamp_ns,
                    interrupted: self.state.active_modifiers.bits() != shift_modifier.bits()
                        || contains_any(
                            self.state.active_modifiers,
                            ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_CONTROL,
                        ),
                },
            );
        }
        if is_press_phase(phase) {
            self.state.physical_down.insert(event.code);
        }
        if phase == EventPhase::Up {
            self.state.physical_down.remove(&event.code);
            let binding_decision = self
                .state
                .bindings
                .remove(&event.code)
                .map(|binding| self.release_binding(binding));
            let modifier_chord = modifier.is_some()
                && modifiers_before.bits() != 0
                && !self.state.modifier_chord_fired
                && !self.state.modifier_chord_interrupted
                && self
                    .rules
                    .find_modifier_chord(modifiers_before)
                    .map(|_| true)
                    .unwrap_or(false);
            if modifier_chord {
                self.state.modifier_chord_fired = true;
                let mut outputs = self
                    .rules
                    .find_modifier_chord(modifiers_before)
                    .and_then(|rule| match &rule.action {
                        Action::Replace(outputs) => Some(outputs.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        vec![OutputEvent::SystemShortcut(
                            event_model::SystemShortcut::SwitchInputSource,
                        )]
                    });
                append_release_outputs(&mut outputs, binding_decision, event);
                return event_model::Decision::Emit(outputs);
            }
            if self.state.active_modifiers.bits() == 0 {
                self.state.modifier_chord_interrupted = false;
                self.state.modifier_chord_fired = false;
            }
            if is_shift_code(event.code) {
                if let Some(tap) = self.state.shift_taps.remove(&event.code) {
                    let duration = event.timestamp_ns.saturating_sub(tap.started_ns);
                    if self.pinyin_active && !tap.interrupted && duration <= 300_000_000 {
                        let mut outputs = vec![OutputEvent::SystemShortcut(
                            event_model::SystemShortcut::TogglePinyinEnglish,
                        )];
                        append_release_outputs(&mut outputs, binding_decision, event);
                        return event_model::Decision::Emit(outputs);
                    }
                }
            }
            if let Some(decision) = binding_decision {
                return decision;
            }
        }
        if phase == EventPhase::Repeat {
            if let Some(binding) = self.state.bindings.get(&event.code) {
                return if binding.is_empty() {
                    event_model::Decision::Suppress
                } else {
                    event_model::Decision::Emit(repeat_outputs(binding))
                };
            }
        }
        let action = self
            .rules
            .find(event)
            .or_else(|| {
                if phase == event.phase {
                    None
                } else {
                    let mut adjusted = event.clone();
                    adjusted.phase = phase;
                    self.rules.find(&adjusted)
                }
            })
            .map(|rule| rule.action.clone());
        match action {
            Some(Action::Pass) => event_model::Decision::Pass,
            Some(Action::Suppress) => {
                if is_press_phase(phase) {
                    self.state.bindings.insert(event.code, Vec::new());
                }
                event_model::Decision::Suppress
            }
            Some(Action::Replace(outputs)) => {
                if is_press_phase(phase) {
                    self.state.bindings.insert(event.code, outputs.clone());
                    self.track_down(&outputs);
                }
                let outputs = if phase == EventPhase::Repeat {
                    repeat_outputs(&outputs)
                } else {
                    outputs
                };
                event_model::Decision::Emit(outputs)
            }
            Some(Action::InvertScroll) => {
                let scroll = event
                    .scroll
                    .unwrap_or(event_model::ScrollDelta { x: 0.0, y: 0.0 });
                event_model::Decision::Emit(vec![OutputEvent::Scroll {
                    delta_x: if matches!(event.code, PhysicalCode::Scroll(ScrollAxis::Horizontal)) {
                        -scroll.x
                    } else {
                        scroll.x
                    },
                    delta_y: if matches!(event.code, PhysicalCode::Scroll(ScrollAxis::Vertical)) {
                        -scroll.y
                    } else {
                        scroll.y
                    },
                }])
            }
            None => self.default_modifier_decision(event),
        }
    }

    fn default_modifier_decision(&mut self, event: &NormalizedEvent) -> event_model::Decision {
        if !matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged) {
            return event_model::Decision::Pass;
        }
        let mapped = map_modifiers(event.modifiers);
        if mapped == event.modifiers {
            return event_model::Decision::Pass;
        }
        let output = OutputEvent::Key {
            code: event.code,
            phase: effective_phase(event),
            modifiers: mapped,
        };
        if is_press_phase(effective_phase(event)) {
            self.state.bindings.insert(event.code, vec![output.clone()]);
            self.track_down(std::slice::from_ref(&output));
        }
        event_model::Decision::Emit(vec![output])
    }

    fn track_down(&mut self, outputs: &[OutputEvent]) {
        for output in outputs {
            if let Some(code) = output_code(output) {
                *self.state.logical_down.entry(code).or_default() += 1;
            }
        }
    }

    fn release_binding(&mut self, binding: Vec<OutputEvent>) -> event_model::Decision {
        if binding.is_empty() {
            return event_model::Decision::Suppress;
        }
        let mut output = Vec::new();
        for event in binding {
            if let Some(code) = output_code(&event) {
                let count = self.state.logical_down.entry(code).or_default();
                *count = count.saturating_sub(1);
                if *count == 0 {
                    if let Some(release) = release_event(&event) {
                        output.push(release);
                    }
                    self.state.logical_down.remove(&code);
                }
            }
        }
        if output.is_empty() {
            event_model::Decision::Suppress
        } else {
            event_model::Decision::Emit(output)
        }
    }
}

fn append_release_outputs(
    outputs: &mut Vec<OutputEvent>,
    decision: Option<event_model::Decision>,
    event: &NormalizedEvent,
) {
    match decision {
        Some(event_model::Decision::Emit(releases)) => outputs.extend(releases),
        Some(event_model::Decision::Suppress) => {}
        Some(event_model::Decision::Pass) | None => outputs.push(OutputEvent::Key {
            code: event.code,
            phase: EventPhase::Up,
            modifiers: ModifierSet::NONE,
        }),
    }
}

fn is_press_phase(phase: EventPhase) -> bool {
    matches!(phase, EventPhase::Down | EventPhase::Repeat)
}

fn effective_phase(event: &NormalizedEvent) -> EventPhase {
    if event.kind == EventKind::FlagsChanged && event.phase == EventPhase::Changed {
        if modifier_for_code(event.code).is_some_and(|modifier| event.modifiers.contains(modifier))
        {
            EventPhase::Down
        } else if modifier_for_code(event.code).is_some() {
            EventPhase::Up
        } else {
            EventPhase::Changed
        }
    } else {
        event.phase
    }
}

fn modifier_for_code(code: PhysicalCode) -> Option<ModifierSet> {
    let PhysicalCode::Key(code) = code else {
        return None;
    };
    Some(match code {
        56 => ModifierSet::LEFT_SHIFT,
        60 => ModifierSet::RIGHT_SHIFT,
        59 => ModifierSet::LEFT_CONTROL,
        62 => ModifierSet::RIGHT_CONTROL,
        58 => ModifierSet::LEFT_OPTION,
        61 => ModifierSet::RIGHT_OPTION,
        55 => ModifierSet::LEFT_COMMAND,
        54 => ModifierSet::RIGHT_COMMAND,
        _ => return None,
    })
}

fn is_shift_code(code: PhysicalCode) -> bool {
    modifier_for_code(code).is_some_and(|modifier| {
        modifier == ModifierSet::LEFT_SHIFT || modifier == ModifierSet::RIGHT_SHIFT
    })
}

fn contains_any(value: ModifierSet, candidates: ModifierSet) -> bool {
    [
        ModifierSet::LEFT_SHIFT,
        ModifierSet::RIGHT_SHIFT,
        ModifierSet::LEFT_CONTROL,
        ModifierSet::RIGHT_CONTROL,
        ModifierSet::LEFT_OPTION,
        ModifierSet::RIGHT_OPTION,
        ModifierSet::LEFT_COMMAND,
        ModifierSet::RIGHT_COMMAND,
    ]
    .iter()
    .any(|bit| candidates.contains(*bit) && value.contains(*bit))
}

fn repeat_outputs(outputs: &[OutputEvent]) -> Vec<OutputEvent> {
    outputs
        .iter()
        .map(|output| match output {
            OutputEvent::Key {
                code, modifiers, ..
            } => OutputEvent::Key {
                code: *code,
                phase: EventPhase::Repeat,
                modifiers: *modifiers,
            },
            OutputEvent::MouseButton { button, .. } => OutputEvent::MouseButton {
                button: *button,
                phase: EventPhase::Repeat,
            },
            other => other.clone(),
        })
        .collect()
}

fn output_code(output: &OutputEvent) -> Option<PhysicalCode> {
    match output {
        OutputEvent::Key { code, .. } => Some(*code),
        OutputEvent::MouseButton { button, .. } => Some(PhysicalCode::MouseButton(*button)),
        OutputEvent::Scroll { .. } | OutputEvent::SystemShortcut(_) => None,
    }
}

fn release_event(output: &OutputEvent) -> Option<OutputEvent> {
    match output {
        OutputEvent::Key {
            code, modifiers, ..
        } => Some(OutputEvent::Key {
            code: *code,
            phase: EventPhase::Up,
            modifiers: *modifiers,
        }),
        OutputEvent::MouseButton { button, .. } => Some(OutputEvent::MouseButton {
            button: *button,
            phase: EventPhase::Up,
        }),
        OutputEvent::Scroll { .. } | OutputEvent::SystemShortcut(_) => None,
    }
}

fn map_modifiers(input: ModifierSet) -> ModifierSet {
    let mut output = ModifierSet::NONE;
    for (from, to) in [
        (ModifierSet::LEFT_SHIFT, ModifierSet::LEFT_SHIFT),
        (ModifierSet::RIGHT_SHIFT, ModifierSet::RIGHT_SHIFT),
        (ModifierSet::LEFT_CONTROL, ModifierSet::LEFT_CONTROL),
        (ModifierSet::RIGHT_CONTROL, ModifierSet::RIGHT_CONTROL),
        (ModifierSet::LEFT_OPTION, ModifierSet::LEFT_WINDOWS),
        (ModifierSet::RIGHT_OPTION, ModifierSet::RIGHT_WINDOWS),
        (ModifierSet::LEFT_COMMAND, ModifierSet::LEFT_ALT),
        (ModifierSet::RIGHT_COMMAND, ModifierSet::RIGHT_ALT),
    ] {
        if input.contains(from) {
            output.insert(to);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_model::{DeviceSource, EventKind, EventOrigin, EventPhase, ModifierSet, ScrollAxis};

    fn key(code: u16, phase: EventPhase) -> NormalizedEvent {
        NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind: EventKind::Keyboard,
            phase,
            code: PhysicalCode::Key(code),
            modifiers: ModifierSet::NONE,
            location: None,
            scroll: None,
            timestamp_ns: 1,
            app_context: None,
            origin: EventOrigin::Hardware,
        }
    }

    fn replacement(id: &str, code: u16, output: u16, priority: i32) -> Rule {
        Rule {
            id: id.into(),
            enabled: true,
            priority,
            scope: RuleScope::Global,
            trigger: Trigger::Key {
                code: PhysicalCode::Key(code),
                modifiers: ModifierMatcher::default(),
                phase: EventPhase::Down,
            },
            action: Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::Key(output),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE,
            }]),
        }
    }

    #[test]
    fn replaces_key_and_freezes_release_mapping() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap());
        assert!(matches!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Emit(_)
        ));
        engine.replace_rules(CompiledRules::compile(vec![replacement("b", 0, 2, 10)]).unwrap());
        assert_eq!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }])
        );
    }

    #[test]
    fn unmatched_key_passes_through() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        assert_eq!(
            engine.handle(&key(99, EventPhase::Down)),
            event_model::Decision::Pass
        );
        assert!(engine.state().physical_down(PhysicalCode::key(99)));
        assert_eq!(
            engine.handle(&key(99, EventPhase::Up)),
            event_model::Decision::Pass
        );
        assert!(!engine.state().physical_down(PhysicalCode::key(99)));
    }

    #[test]
    fn higher_priority_rule_wins() {
        let rules = CompiledRules::compile(vec![
            replacement("low", 0, 1, 1),
            replacement("high", 0, 2, 100),
        ])
        .unwrap();
        let mut engine = RuleEngine::new(rules);
        assert_eq!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::Key(2),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE
            }])
        );
    }

    #[test]
    fn shared_output_is_released_only_after_last_source() {
        let rules =
            CompiledRules::compile(vec![replacement("a", 0, 7, 10), replacement("b", 1, 7, 10)])
                .unwrap();
        let mut engine = RuleEngine::new(rules);
        engine.handle(&key(0, EventPhase::Down));
        engine.handle(&key(1, EventPhase::Down));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Suppress
        );
        assert!(matches!(
            engine.handle(&key(1, EventPhase::Up)),
            event_model::Decision::Emit(_)
        ));
    }

    #[test]
    fn scroll_inversion_preserves_axis_and_precision() {
        let rule = Rule {
            id: "scroll".into(),
            enabled: true,
            priority: 1,
            scope: RuleScope::Global,
            trigger: Trigger::Scroll {
                axis: ScrollAxis::Vertical,
            },
            action: Action::InvertScroll,
        };
        let event = NormalizedEvent {
            source: DeviceSource::Mouse,
            kind: EventKind::Scroll,
            phase: EventPhase::Motion,
            code: PhysicalCode::Scroll(ScrollAxis::Vertical),
            modifiers: ModifierSet::NONE,
            location: None,
            scroll: Some(event_model::ScrollDelta { x: 0.25, y: -2.5 }),
            timestamp_ns: 2,
            app_context: None,
            origin: EventOrigin::Hardware,
        };
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        assert_eq!(
            engine.handle(&event),
            event_model::Decision::Emit(vec![OutputEvent::Scroll {
                delta_x: 0.25,
                delta_y: 2.5
            }])
        );
    }

    #[test]
    fn rejects_conflicting_rules_and_direct_loops() {
        let conflict =
            CompiledRules::compile(vec![replacement("a", 0, 1, 10), replacement("b", 0, 2, 10)]);
        assert!(matches!(conflict, Err(EngineError::Conflict { .. })));
        let looped = CompiledRules::compile(vec![replacement("loop", 0, 0, 10)]);
        assert!(matches!(looped, Err(EngineError::Loop { .. })));
    }

    #[test]
    fn rejects_multi_hop_mapping_cycles() {
        let rules =
            CompiledRules::compile(vec![replacement("a", 0, 1, 10), replacement("b", 1, 0, 10)]);
        assert!(matches!(rules, Err(EngineError::Loop { .. })));
    }

    #[test]
    fn release_all_emits_missing_releases() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap());
        engine.handle(&key(0, EventPhase::Down));
        assert_eq!(
            engine.release_all(),
            vec![OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }]
        );
    }

    #[test]
    fn release_all_deduplicates_shared_logical_targets() {
        let rules =
            CompiledRules::compile(vec![replacement("a", 0, 7, 10), replacement("b", 1, 7, 10)])
                .unwrap();
        let mut engine = RuleEngine::new(rules);
        engine.handle(&key(0, EventPhase::Down));
        engine.handle(&key(1, EventPhase::Down));
        assert_eq!(
            engine.release_all(),
            vec![OutputEvent::Key {
                code: PhysicalCode::Key(7),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }]
        );
    }

    #[test]
    fn default_physical_modifier_mapping_emits_windows_semantics() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        let mut event = key(4, EventPhase::Down);
        event.modifiers = ModifierSet::LEFT_OPTION;
        assert_eq!(
            engine.handle(&event),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::Key(4),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_WINDOWS,
            }])
        );
        assert_eq!(
            engine.handle(&key(4, EventPhase::Up)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::Key(4),
                phase: EventPhase::Up,
                modifiers: ModifierSet::LEFT_WINDOWS,
            }])
        );
    }

    #[test]
    fn mouse_button_replacement_keeps_drag_pairing() {
        let rule = Rule {
            id: "mouse".into(),
            enabled: true,
            priority: 1,
            scope: RuleScope::Global,
            trigger: Trigger::MouseButton {
                button: event_model::MouseButton::Right,
                phase: EventPhase::Down,
            },
            action: Action::Replace(vec![OutputEvent::MouseButton {
                button: event_model::MouseButton::Left,
                phase: EventPhase::Down,
            }]),
        };
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        let event = NormalizedEvent {
            source: DeviceSource::Mouse,
            kind: EventKind::MouseButton,
            phase: EventPhase::Down,
            code: PhysicalCode::MouseButton(event_model::MouseButton::Right),
            modifiers: ModifierSet::NONE,
            location: None,
            scroll: None,
            timestamp_ns: 3,
            app_context: None,
            origin: EventOrigin::Hardware,
        };
        assert_eq!(
            engine.handle(&event),
            event_model::Decision::Emit(vec![OutputEvent::MouseButton {
                button: event_model::MouseButton::Left,
                phase: EventPhase::Down,
            }])
        );
        let up = NormalizedEvent {
            phase: EventPhase::Up,
            ..event
        };
        assert_eq!(
            engine.handle(&up),
            event_model::Decision::Emit(vec![OutputEvent::MouseButton {
                button: event_model::MouseButton::Left,
                phase: EventPhase::Up,
            }])
        );
    }

    #[test]
    fn flags_changed_modifier_pairs_changed_press_and_release() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        let press = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            phase: EventPhase::Changed,
            code: PhysicalCode::left_option(),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key(58, EventPhase::Changed)
        };
        assert_eq!(
            engine.handle(&press),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::left_option(),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_WINDOWS,
            }])
        );
        assert!(engine.state().physical_down(PhysicalCode::left_option()));
        assert_eq!(
            engine
                .state()
                .logical_down_count(PhysicalCode::left_option()),
            1
        );
        let release = NormalizedEvent {
            modifiers: ModifierSet::NONE,
            ..press
        };
        assert_eq!(
            engine.handle(&release),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::left_option(),
                phase: EventPhase::Up,
                modifiers: ModifierSet::LEFT_WINDOWS,
            }])
        );
        assert_eq!(
            engine
                .state()
                .logical_down_count(PhysicalCode::left_option()),
            0
        );
        assert!(!engine.state().physical_down(PhysicalCode::left_option()));
    }

    #[test]
    fn repeat_replays_mapping_with_repeat_phase() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap());
        engine.handle(&key(0, EventPhase::Down));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Repeat)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Repeat,
                modifiers: ModifierSet::NONE,
            }])
        );
    }

    #[test]
    fn flags_changed_rule_freezes_modifier_mapping_until_release() {
        let rule = Rule {
            id: "option".into(),
            enabled: true,
            priority: 1,
            scope: RuleScope::Global,
            trigger: Trigger::Key {
                code: PhysicalCode::left_option(),
                modifiers: ModifierMatcher::default(),
                phase: EventPhase::Down,
            },
            action: Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE,
            }]),
        };
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        let press = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            phase: EventPhase::Changed,
            code: PhysicalCode::left_option(),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key(58, EventPhase::Changed)
        };
        assert_eq!(
            engine.handle(&press),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE,
            }])
        );
        let release = NormalizedEvent {
            modifiers: ModifierSet::NONE,
            ..press
        };
        assert_eq!(
            engine.handle(&release),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE,
            }])
        );
    }

    fn modifier(code: u16, modifiers: ModifierSet, timestamp_ns: u64) -> NormalizedEvent {
        NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind: EventKind::FlagsChanged,
            phase: EventPhase::Changed,
            code: PhysicalCode::key(code),
            modifiers,
            location: None,
            scroll: None,
            timestamp_ns,
            app_context: None,
            origin: EventOrigin::Hardware,
        }
    }

    fn modifier_chord_rule() -> Rule {
        Rule {
            id: "switch-input-source".into(),
            enabled: true,
            priority: 300,
            scope: RuleScope::Global,
            trigger: Trigger::ModifierChord {
                modifiers: ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_SHIFT,
                phase: EventPhase::Up,
            },
            action: Action::Replace(vec![OutputEvent::SystemShortcut(
                event_model::SystemShortcut::SwitchInputSource,
            )]),
        }
    }

    #[test]
    fn modifier_chord_fires_for_clean_ctrl_shift() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        assert_eq!(
            engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 1)),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 2)),
            event_model::Decision::Pass
        );
        let shift_up = engine.handle(&modifier(56, ModifierSet::NONE, 3));
        assert!(matches!(
            shift_up,
            event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchInputSource))
        ));
        assert_eq!(
            engine.handle(&modifier(59, ModifierSet::NONE, 4)),
            event_model::Decision::Pass
        );
    }

    #[test]
    fn modifier_chord_supports_both_press_orders() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 2));
        assert!(matches!(
            engine.handle(&modifier(59, ModifierSet::NONE, 3)),
            event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchInputSource))
        ));
    }

    #[test]
    fn ctrl_shift_plus_letter_passes_original_events() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 1));
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 2));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle(&modifier(56, ModifierSet::NONE, 4)),
            event_model::Decision::Pass
        );
    }

    #[test]
    fn standalone_shift_tap_emits_toggle_pinyin() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        let result = engine.handle(&modifier(56, ModifierSet::NONE, 100));
        assert!(matches!(
            result,
            event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish))
        ));
    }

    #[test]
    fn shift_with_letter_preserves_shift() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Pass
        );
        let result = engine.handle(&modifier(56, ModifierSet::NONE, 100));
        assert!(!matches!(
            result,
            event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish))
        ));
    }

    #[test]
    fn shift_with_other_modifier_preserves_shift() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        engine.handle(&modifier(58, ModifierSet::LEFT_OPTION, 2));
        engine.handle(&modifier(58, ModifierSet::NONE, 3));
        let result = engine.handle(&modifier(56, ModifierSet::NONE, 100));
        assert!(!matches!(
            result,
            event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish))
        ));
    }

    #[test]
    fn long_shift_hold_preserves_shift() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        let result = engine.handle(&modifier(56, ModifierSet::NONE, 301_000_001));
        assert!(!matches!(
            result,
            event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish))
        ));
    }

    #[test]
    fn caps_lock_is_not_treated_as_shift() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        assert_eq!(
            engine.handle(&modifier(57, ModifierSet::NONE, 1)),
            event_model::Decision::Pass
        );
    }
}
