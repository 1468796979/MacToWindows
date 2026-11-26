use event_model::{
    AppContext, Decision, EventKind, EventPhase, ModifierSet, NormalizedEvent, OutputEvent,
    PhysicalCode, ScrollAxis, ScrollAxisTransform,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

const MAX_OUTPUT_EVENTS: usize = 16;

/// Select the rules applied to a complete keyboard stream. Text submissions
/// must be routed around the key ledger before entering either policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardMappingPolicy {
    #[default]
    Full,
    /// Preserve ordinary software input while retaining configured shortcuts
    /// and project actions. Held keys keep the binding chosen on their Down.
    ShortcutsOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    /// The target is still the physical input, with flags rewritten as needed.
    Native,
    Mapped,
    /// Mapped main keys use only the flags selected by their action.
    ExactMapped,
}

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
    #[error("rule {id}: a release-only modifier chord requires exactly one system shortcut")]
    InvalidModifierChord { id: String },
    #[error("rule {id}: scroll reversal requires a scroll trigger")]
    InvalidScrollAction { id: String },
}

#[derive(Clone, Debug)]
pub struct CompiledRules {
    rules: Vec<Rule>,
}

impl CompiledRules {
    pub fn compile(input: Vec<Rule>) -> Result<Self, EngineError> {
        let mut rules: Vec<Rule> = Vec::new();
        for mut rule in input.into_iter().filter(|rule| rule.enabled) {
            if matches!(rule.trigger, Trigger::ModifierChord { .. })
                && (!matches!(
                    rule.trigger,
                    Trigger::ModifierChord {
                        phase: EventPhase::Up,
                        ..
                    }
                ) || !matches!(&rule.action, Action::Replace(outputs) if matches!(outputs.as_slice(), [OutputEvent::SystemShortcut(_)])))
            {
                return Err(EngineError::InvalidModifierChord { id: rule.id });
            }
            let scroll_trigger = matches!(rule.trigger, Trigger::Scroll { .. });
            if matches!(rule.action, Action::InvertScroll) && !scroll_trigger {
                return Err(EngineError::InvalidScrollAction { id: rule.id });
            }
            if let Action::Replace(outputs) = &mut rule.action {
                if scroll_trigger {
                    complete_scroll_action(outputs);
                }
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
        self.find_with_policy(event, KeyboardMappingPolicy::Full)
    }

    fn find_with_policy(
        &self,
        event: &NormalizedEvent,
        policy: KeyboardMappingPolicy,
    ) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            (policy == KeyboardMappingPolicy::Full || shortcut_rule(rule))
                && scope_matches(&rule.scope, event.app_context.as_ref())
                && trigger_matches(&rule.trigger, event)
        })
    }

    fn find_modifier_chord(
        &self,
        modifiers: ModifierSet,
        app: Option<&AppContext>,
    ) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            scope_matches(&rule.scope, app)
                && matches!(
                    rule.trigger,
                    Trigger::ModifierChord {
                        modifiers: required,
                        phase: EventPhase::Up,
                    } if required == modifiers
                )
        })
    }
}

/// Wheel events have no matching key-up. Finish any targets the action still
/// holds in this same batch, releasing later presses before earlier ones.
fn complete_scroll_action(outputs: &mut Vec<OutputEvent>) {
    let mut held = Vec::new();
    for output in outputs.iter() {
        if let Some(code) = held_output_code(output) {
            if !held.iter().any(|&(target, _)| target == code) {
                held.push((code, output));
            }
        } else if let OutputEvent::Key {
            phase: EventPhase::Up,
            ..
        }
        | OutputEvent::MouseButton {
            phase: EventPhase::Up,
            ..
        } = output
        {
            let code = output_code(output);
            held.retain(|&(target, _)| Some(target) != code);
        }
    }
    let releases: Vec<_> = held
        .into_iter()
        .rev()
        .filter_map(|(_, down)| {
            let mut release = release_event(down)?;
            if let (Some(modifier), OutputEvent::Key { modifiers, .. }) =
                (output_modifier(down), &mut release)
            {
                modifiers.remove(modifier);
            }
            Some(release)
        })
        .collect();
    outputs.extend(releases);
}

fn shortcut_rule(rule: &Rule) -> bool {
    match &rule.trigger {
        Trigger::Chord { modifiers, .. } => *modifiers != ModifierSet::NONE,
        Trigger::ModifierChord { .. } => true,
        Trigger::Key { modifiers, .. } if modifiers.required != ModifierSet::NONE => true,
        Trigger::Key { .. } => matches!(
            &rule.action,
            Action::Replace(outputs)
                if !outputs.is_empty()
                    && outputs.iter().all(|output| matches!(output, OutputEvent::SystemShortcut(_)))
        ),
        // The policy controls keyboard mappings, not mouse/scroll behavior.
        Trigger::MouseButton { .. } | Trigger::Scroll { .. } => true,
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
    bindings: HashMap<PhysicalCode, SourceBinding>,
    logical_down: HashMap<PhysicalCode, u32>,
    physical_down: HashSet<PhysicalCode>,
    active_modifiers: ModifierSet,
    modifier_chord_interrupted: bool,
    modifier_chord_fired: bool,
    gestures_blocked_until_modifiers_up: bool,
    shift_taps: HashMap<PhysicalCode, ShiftTapState>,
    keyboard_policy: KeyboardMappingPolicy,
}

#[derive(Clone, Debug)]
struct SourceBinding {
    outputs: Vec<OutputEvent>,
    passthrough: bool,
    kind: BindingKind,
    // Ordinary software events must stay raw even if the policy changes while
    // the key is held. Rebuilding such Repeat/Up events can discard payloads.
    raw_passthrough: bool,
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
        let native_targets: HashSet<_> = self
            .bindings
            .values()
            .filter(|binding| binding.kind == BindingKind::Native)
            .flat_map(|binding| binding.outputs.iter().filter_map(output_code))
            .collect();
        let mut native_modifiers = self.active_modifiers;
        for (source, binding) in &self.bindings {
            if binding.kind != BindingKind::Native {
                if let Some(modifier) = modifier_for_code(*source) {
                    native_modifiers.remove(modifier);
                }
            }
        }
        for target in &native_targets {
            if let Some(modifier) = modifier_for_code(*target) {
                native_modifiers.insert(modifier);
            }
        }
        for binding in self
            .bindings
            .values()
            .filter(|binding| binding.kind != BindingKind::Native)
        {
            for event in &binding.outputs {
                if held_output_code(event).is_none() {
                    continue;
                }
                if let Some(release) = release_event(event) {
                    if let Some(code) = output_code(&release) {
                        if !native_targets.contains(&code) && released.insert(code) {
                            let release = match release {
                                OutputEvent::Key { code, phase, .. } => OutputEvent::Key {
                                    code,
                                    phase,
                                    modifiers: native_modifiers,
                                },
                                other => other,
                            };
                            output.push(release);
                        }
                    }
                }
            }
        }
        self.clear_bindings();
        output
    }

    fn clear_bindings(&mut self) {
        self.bindings.clear();
        self.logical_down.clear();
        self.physical_down.clear();
        self.active_modifiers = ModifierSet::NONE;
        self.modifier_chord_interrupted = false;
        self.modifier_chord_fired = false;
        self.gestures_blocked_until_modifiers_up = false;
        self.shift_taps.clear();
    }
}

pub struct RuleEngine {
    rules: CompiledRules,
    state: RuntimeState,
    pinyin_active: bool,
    software_input: bool,
    coalesce_targets: bool,
}

impl RuleEngine {
    pub fn new(rules: CompiledRules) -> Self {
        Self {
            rules,
            state: RuntimeState::default(),
            pinyin_active: false,
            software_input: false,
            coalesce_targets: true,
        }
    }

    /// Let a shared output dispatcher see every source binding's transition.
    /// The dispatcher can then pair holders across engines and native input.
    pub fn with_external_output_ownership(rules: CompiledRules) -> Self {
        Self {
            coalesce_targets: false,
            ..Self::new(rules)
        }
    }
    pub fn replace_rules(&mut self, rules: CompiledRules) {
        self.rules = rules;
        // Existing key bindings remain frozen. Cancel pending gestures so a
        // release cannot accidentally run an action introduced by the update.
        self.state.modifier_chord_interrupted = true;
        self.state.modifier_chord_fired = true;
        self.state.shift_taps.clear();
    }
    pub fn state(&self) -> &RuntimeState {
        &self.state
    }
    pub fn release_all(&mut self) -> Vec<OutputEvent> {
        self.state.release_all()
    }
    /// Clear rules' source bindings when the external dispatcher drains their
    /// mapped targets, leaving native targets for their actual key-up events.
    pub fn clear_bindings(&mut self) {
        self.state.clear_bindings();
    }
    pub fn checkpoint(&self) -> RuntimeState {
        self.state.clone()
    }
    pub fn restore(&mut self, checkpoint: RuntimeState) {
        self.state = checkpoint;
    }
    pub fn logical_modifiers(&self) -> ModifierSet {
        self.map_modifier_flags(self.state.active_modifiers)
    }
    pub fn modifiers_for_event(&self, event: &NormalizedEvent) -> ModifierSet {
        self.map_modifier_flags(event.modifiers)
    }

    /// External keyboard activity cancels pending local tap gestures without
    /// changing any held key or its frozen output binding.
    pub fn interrupt_gestures(&mut self) {
        self.state.shift_taps.clear();
        self.state.modifier_chord_interrupted = true;
        self.state.gestures_blocked_until_modifiers_up =
            self.physical_modifier_flags(ModifierSet::NONE) != ModifierSet::NONE;
    }

    /// Enables the standalone Shift toggle while the input-source service can
    /// move between the remembered Pinyin and Latin sources.  The service
    /// keeps the toggle available for the Latin half of that pair too, so a
    /// second Shift tap can return to Pinyin.
    pub fn set_pinyin_active(&mut self, active: bool) {
        self.pinyin_active = active;
    }

    pub fn handle(&mut self, event: &NormalizedEvent) -> event_model::Decision {
        self.handle_with_policy(event, KeyboardMappingPolicy::Full)
    }

    /// A rejected one-shot action posted no output. Keep the original press
    /// and its remaining events together even if the configuration changes.
    pub fn pass_rejected_shortcut(&mut self, event: &NormalizedEvent) -> bool {
        if effective_phase(event) != EventPhase::Down {
            return false;
        }
        self.pass_event(event);
        if let Some(binding) = self.state.bindings.get_mut(&event.code) {
            binding.raw_passthrough = true;
        }
        true
    }

    /// Report whether the source keeps its native target, even when its flags
    /// need rewriting. A held source keeps this identity until release.
    pub fn handle_with_binding(
        &mut self,
        event: &NormalizedEvent,
        policy: KeyboardMappingPolicy,
    ) -> (event_model::Decision, bool) {
        let (decision, kind) = self.handle_with_binding_kind(event, policy);
        (decision, kind == BindingKind::Native)
    }

    /// Report how to deliver the binding's output. A held source keeps this
    /// identity, including the exact match, until release.
    pub fn handle_with_binding_kind(
        &mut self,
        event: &NormalizedEvent,
        policy: KeyboardMappingPolicy,
    ) -> (event_model::Decision, BindingKind) {
        let previous = self
            .state
            .bindings
            .get(&event.code)
            .map(|binding| binding.kind);
        let decision = self.handle_with_policy(event, policy);
        let kind = previous
            .or_else(|| {
                self.state
                    .bindings
                    .get(&event.code)
                    .map(|binding| binding.kind)
            })
            .unwrap_or(BindingKind::Native);
        (decision, kind)
    }

    pub fn set_keyboard_mapping_policy(&mut self, policy: KeyboardMappingPolicy) {
        if self.state.keyboard_policy != policy {
            self.interrupt_gestures();
            self.state.keyboard_policy = policy;
        }
    }

    pub fn keyboard_mapping_policy(&self) -> KeyboardMappingPolicy {
        self.state.keyboard_policy
    }

    /// Software input can contain repeated Down-only submissions. If the first
    /// event was passed through, keep later Downs raw without acquiring another
    /// holder. Mapped and explicitly consumed bindings retain duplicate guards.
    pub fn set_software_input(&mut self, software: bool) {
        self.software_input = software;
    }

    /// Inspect the highest-priority enabled shortcut without changing state.
    /// A caller can use this to resolve an explicit Alt+Tab session while
    /// still honoring the active preset's shortcut rules.
    pub fn shortcut_action(&self, event: &NormalizedEvent) -> Option<Action> {
        self.action_for_event(event, KeyboardMappingPolicy::ShortcutsOnly)
    }

    pub fn action_for_event(
        &self,
        event: &NormalizedEvent,
        policy: KeyboardMappingPolicy,
    ) -> Option<Action> {
        if matches!(event.origin, event_model::EventOrigin::TextInjection { .. }) {
            return None;
        }
        self.rules
            .find_with_policy(event, policy)
            .map(|rule| rule.action.clone())
    }

    pub fn handle_with_policy(
        &mut self,
        event: &NormalizedEvent,
        policy: KeyboardMappingPolicy,
    ) -> event_model::Decision {
        self.handle_with_scroll_default(event, policy, false)
    }

    pub fn handle_with_scroll_default(
        &mut self,
        event: &NormalizedEvent,
        policy: KeyboardMappingPolicy,
        invert_unmapped_scroll: bool,
    ) -> event_model::Decision {
        if matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged)
            && matches!(event.origin, event_model::EventOrigin::TextInjection { .. })
        {
            return event_model::Decision::Pass;
        }
        self.set_keyboard_mapping_policy(policy);
        if event.kind == EventKind::MouseMove {
            for tap in self.state.shift_taps.values_mut() {
                tap.interrupted = true;
            }
            if self.state.active_modifiers != ModifierSet::NONE {
                self.state.modifier_chord_interrupted = true;
            }
            return match self.state.bindings.get(&event.code) {
                Some(binding) if !binding.passthrough => binding
                    .outputs
                    .iter()
                    .find_map(|output| {
                        if let OutputEvent::MouseButton { button, .. } = output {
                            Some(event_model::Decision::Emit(vec![OutputEvent::MouseDrag {
                                button: *button,
                            }]))
                        } else {
                            None
                        }
                    })
                    .unwrap_or(event_model::Decision::Suppress),
                _ => event_model::Decision::Pass,
            };
        }
        let phase = effective_phase(event);
        if event.kind == EventKind::Keyboard {
            self.state.active_modifiers = self.physical_modifier_flags(event.modifiers);
        }
        if phase == EventPhase::Down {
            if let Some(binding) = self.state.bindings.get(&event.code) {
                if self.software_input && binding.passthrough {
                    self.interrupt_gestures();
                    return event_model::Decision::Pass;
                }
                return event_model::Decision::Suppress;
            }
        }
        let modifier = modifier_for_code(event.code);
        let modifiers_before = self.state.active_modifiers;
        if let Some(modifier) = modifier {
            if phase == EventPhase::Down {
                self.state.active_modifiers.insert(modifier);
                if !self.state.gestures_blocked_until_modifiers_up
                    && self
                        .rules
                        .find_modifier_chord(
                            self.state.active_modifiers,
                            event.app_context.as_ref(),
                        )
                        .is_some()
                    && self
                        .rules
                        .find_modifier_chord(modifiers_before, event.app_context.as_ref())
                        .is_none()
                    && self
                        .state
                        .physical_down
                        .iter()
                        .all(|code| modifier_for_code(*code).is_some())
                {
                    self.state.modifier_chord_interrupted = false;
                    self.state.modifier_chord_fired = false;
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
            if self.state.active_modifiers != ModifierSet::NONE {
                self.state.modifier_chord_interrupted = true;
            }
            for state in self.state.shift_taps.values_mut() {
                state.interrupted = true;
            }
        }
        if let Some(shift_modifier) = modifier.filter(|_| {
            phase == EventPhase::Down
                && is_shift_code(event.code)
                && !self.state.gestures_blocked_until_modifiers_up
        }) {
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
                .map(|binding| self.release_binding(binding, event));
            let modifier_chord = modifier.is_some()
                && modifiers_before.bits() != 0
                && !self.state.modifier_chord_fired
                && !self.state.modifier_chord_interrupted
                && !self.state.gestures_blocked_until_modifiers_up
                && self
                    .rules
                    .find_modifier_chord(modifiers_before, event.app_context.as_ref())
                    .map(|_| true)
                    .unwrap_or(false);
            if modifier_chord {
                self.state.modifier_chord_fired = true;
                let mut outputs = self
                    .rules
                    .find_modifier_chord(modifiers_before, event.app_context.as_ref())
                    .and_then(|rule| match &rule.action {
                        Action::Replace(outputs) => Some(outputs.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                append_release_outputs(
                    &mut outputs,
                    binding_decision,
                    event,
                    self.modifiers_for_event(event),
                );
                return event_model::Decision::Emit(outputs);
            }
            if self.state.active_modifiers.bits() == 0 {
                self.state.modifier_chord_interrupted = false;
                self.state.modifier_chord_fired = false;
                self.state.gestures_blocked_until_modifiers_up = false;
            }
            if is_shift_code(event.code) {
                if let Some(tap) = self.state.shift_taps.remove(&event.code) {
                    let duration = event.timestamp_ns.saturating_sub(tap.started_ns);
                    if self.pinyin_active && !tap.interrupted && duration <= 300_000_000 {
                        let mut outputs = vec![OutputEvent::SystemShortcut(
                            event_model::SystemShortcut::TogglePinyinEnglish,
                        )];
                        append_release_outputs(
                            &mut outputs,
                            binding_decision,
                            event,
                            self.modifiers_for_event(event),
                        );
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
                return if binding.kind == BindingKind::Native {
                    if binding.raw_passthrough {
                        return event_model::Decision::Pass;
                    }
                    let modifiers = self.modifiers_for_event(event);
                    if binding.passthrough
                        && modifiers == self.physical_modifier_flags(event.modifiers)
                    {
                        event_model::Decision::Pass
                    } else if matches!(event.code, PhysicalCode::Key(_)) {
                        event_model::Decision::Emit(vec![OutputEvent::Key {
                            code: event.code,
                            phase: EventPhase::Repeat,
                            modifiers,
                        }])
                    } else {
                        event_model::Decision::Suppress
                    }
                } else if binding.outputs.is_empty() {
                    event_model::Decision::Suppress
                } else {
                    let repeated = repeat_outputs(&binding.outputs);
                    if repeated.is_empty() {
                        event_model::Decision::Suppress
                    } else {
                        event_model::Decision::Emit(repeated)
                    }
                };
            }
            return event_model::Decision::Pass;
        }
        if event.kind == EventKind::Scroll {
            return self.handle_scroll(event, invert_unmapped_scroll);
        }
        let rule = self
            .rules
            .find_with_policy(event, self.state.keyboard_policy)
            .or_else(|| {
                if phase == event.phase {
                    None
                } else {
                    let mut adjusted = event.clone();
                    adjusted.phase = phase;
                    self.rules
                        .find_with_policy(&adjusted, self.state.keyboard_policy)
                }
            });
        let exact = rule.is_some_and(|rule| {
            matches!(
                rule.trigger,
                Trigger::Key {
                    modifiers: ModifierMatcher { exact: true, .. },
                    ..
                }
            )
        });
        let action = rule.map(|rule| rule.action.clone());
        match action {
            Some(Action::Pass) => self.pass_event(event),
            Some(Action::Suppress) => {
                if is_press_phase(phase) {
                    self.state.bindings.insert(
                        event.code,
                        SourceBinding {
                            outputs: Vec::new(),
                            passthrough: false,
                            kind: BindingKind::Mapped,
                            raw_passthrough: false,
                        },
                    );
                }
                event_model::Decision::Suppress
            }
            Some(Action::Replace(outputs)) => {
                if is_press_phase(phase) {
                    return self.acquire_binding(
                        event,
                        outputs,
                        false,
                        if exact {
                            BindingKind::ExactMapped
                        } else {
                            BindingKind::Mapped
                        },
                    );
                }
                let outputs = if phase == EventPhase::Repeat {
                    repeat_outputs(&outputs)
                } else {
                    outputs
                };
                event_model::Decision::Emit(outputs)
            }
            None if self.state.keyboard_policy == KeyboardMappingPolicy::ShortcutsOnly
                && matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged) =>
            {
                self.pass_event(event)
            }
            Some(Action::InvertScroll) | None => self.default_modifier_decision(event),
        }
    }

    fn handle_scroll(&self, event: &NormalizedEvent, invert_unmapped_scroll: bool) -> Decision {
        use ScrollAxisTransform::{Invert, Keep, Remove};

        let moved = |axis| match &event.scroll {
            Some(delta) => match axis {
                ScrollAxis::Horizontal => delta.x != 0.0,
                ScrollAxis::Vertical => delta.y != 0.0,
            },
            None => event.code == PhysicalCode::Scroll(axis),
        };
        let mut outputs = Vec::new();
        let mut keep_scroll = false;
        let has_gesture_phase = event.phase == EventPhase::ScrollGesture;
        let mut has_native_axis = false;
        let mut transform = |axis| {
            let moving = moved(axis);
            if !moving && !has_gesture_phase {
                return Keep;
            }
            let mut adjusted = event.clone();
            adjusted.code = PhysicalCode::Scroll(axis);
            let transform = match self.rules.find(&adjusted).map(|rule| &rule.action) {
                Some(Action::Suppress) => Remove,
                Some(Action::Replace(action)) => {
                    if moving {
                        outputs.extend(action.iter().cloned());
                    }
                    Remove
                }
                Some(Action::InvertScroll) => Invert,
                None if invert_unmapped_scroll => Invert,
                Some(Action::Pass) | None => Keep,
            };
            has_native_axis |= transform != Remove;
            if moving {
                keep_scroll |= transform != Remove;
                transform
            } else {
                Keep
            }
        };
        let x = transform(ScrollAxis::Horizontal);
        let y = transform(ScrollAxis::Vertical);
        if x == Keep && y == Keep {
            // Also pass zero-delta packets carrying native gesture phases.
            return Decision::Pass;
        }
        if keep_scroll {
            outputs.push(if x == Remove || y == Remove {
                OutputEvent::ScrollTransformAxes { x, y }
            } else {
                OutputEvent::ScrollTransform {
                    invert_x: x == Invert,
                    invert_y: y == Invert,
                }
            });
        } else if has_gesture_phase && has_native_axis {
            // Another axis may scroll later in this gesture, or may have
            // scrolled earlier. Preserve the phase without the consumed delta.
            outputs.push(OutputEvent::ScrollTransformAxes {
                x: Remove,
                y: Remove,
            });
        }
        if outputs.is_empty() {
            Decision::Suppress
        } else {
            Decision::Emit(outputs)
        }
    }

    fn default_modifier_decision(&mut self, event: &NormalizedEvent) -> event_model::Decision {
        let phase = effective_phase(event);
        // Unmanaged flagsChanged events (including Caps Lock and Fn) cannot
        // be rebuilt as key presses. Let the adapter rewrite their flags in place.
        if !matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged)
            || phase == EventPhase::Changed
        {
            return self.pass_event(event);
        }
        let mapped = self.map_modifier_flags(event.modifiers);
        if mapped == self.physical_modifier_flags(event.modifiers) {
            return self.pass_event(event);
        }
        let output = OutputEvent::Key {
            code: event.code,
            phase,
            modifiers: mapped,
        };
        if is_press_phase(phase) {
            return self.acquire_binding(event, vec![output], false, BindingKind::Native);
        }
        event_model::Decision::Emit(vec![output])
    }

    fn pass_event(&mut self, event: &NormalizedEvent) -> event_model::Decision {
        if effective_phase(event) != EventPhase::Down {
            return event_model::Decision::Pass;
        }
        let output = match event.code {
            PhysicalCode::Key(_)
                if matches!(event.kind, EventKind::Keyboard | EventKind::FlagsChanged) =>
            {
                OutputEvent::Key {
                    code: event.code,
                    phase: EventPhase::Down,
                    modifiers: event.modifiers,
                }
            }
            PhysicalCode::MouseButton(button) if event.kind == EventKind::MouseButton => {
                OutputEvent::MouseButton {
                    button,
                    phase: EventPhase::Down,
                }
            }
            _ => return event_model::Decision::Pass,
        };
        self.acquire_binding(event, vec![output], true, BindingKind::Native)
    }

    fn acquire_binding(
        &mut self,
        event: &NormalizedEvent,
        outputs: Vec<OutputEvent>,
        passthrough: bool,
        kind: BindingKind,
    ) -> event_model::Decision {
        self.state.bindings.insert(
            event.code,
            SourceBinding {
                outputs: outputs.clone(),
                passthrough,
                kind,
                raw_passthrough: passthrough
                    && self.state.keyboard_policy == KeyboardMappingPolicy::ShortcutsOnly,
            },
        );
        let modifiers = self.map_modifier_flags(event.modifiers);
        let mut emitted = Vec::new();
        for mut output in outputs {
            if let Some(code) = held_output_code(&output) {
                let count = self.state.logical_down.entry(code).or_default();
                let first = *count == 0;
                *count += 1;
                if self.coalesce_targets && !first {
                    continue;
                }
                if let OutputEvent::Key {
                    code,
                    modifiers: flags,
                    ..
                } = &mut output
                {
                    if modifier_for_code(*code).is_some() {
                        *flags = modifiers;
                    }
                }
            }
            emitted.push(output);
        }
        if emitted.is_empty() {
            event_model::Decision::Suppress
        } else if passthrough {
            event_model::Decision::Pass
        } else {
            event_model::Decision::Emit(emitted)
        }
    }

    fn release_binding(
        &mut self,
        binding: SourceBinding,
        event: &NormalizedEvent,
    ) -> event_model::Decision {
        if binding.outputs.is_empty() {
            return event_model::Decision::Suppress;
        }
        let mut output = Vec::new();
        for held in binding.outputs {
            if let Some(code) = held_output_code(&held) {
                let count = self.state.logical_down.entry(code).or_default();
                *count = count.saturating_sub(1);
                let last = *count == 0;
                if last || !self.coalesce_targets {
                    if let Some(mut release) = release_event(&held) {
                        if let OutputEvent::Key { modifiers, .. } = &mut release {
                            if binding.kind == BindingKind::Native
                                || modifier_for_code(code).is_some()
                            {
                                *modifiers = self.map_modifier_flags(event.modifiers);
                            }
                        }
                        output.push(release);
                    }
                }
                if last {
                    self.state.logical_down.remove(&code);
                }
            }
        }
        if output.is_empty() {
            event_model::Decision::Suppress
        } else if binding.passthrough
            && (binding.raw_passthrough
                || self.modifiers_for_event(event) == self.physical_modifier_flags(event.modifiers))
        {
            event_model::Decision::Pass
        } else {
            event_model::Decision::Emit(output)
        }
    }

    fn physical_modifier_flags(&self, observed: ModifierSet) -> ModifierSet {
        let mut physical = observed | self.state.active_modifiers;
        for source in self.state.bindings.keys() {
            if let Some(modifier) = modifier_for_code(*source) {
                physical.insert(modifier);
            }
        }
        physical
    }

    fn map_modifier_flags(&self, physical: ModifierSet) -> ModifierSet {
        let mut logical = self.physical_modifier_flags(physical);
        for (code, binding) in &self.state.bindings {
            if binding.kind != BindingKind::Native {
                if let Some(source) = modifier_for_code(*code) {
                    logical.remove(source);
                }
            }
        }
        for binding in self.state.bindings.values() {
            if binding.kind != BindingKind::Native {
                for output in &binding.outputs {
                    if let Some(modifier) = output_modifier(output) {
                        logical.insert(modifier);
                    }
                }
            }
        }
        logical
    }
}

fn append_release_outputs(
    outputs: &mut Vec<OutputEvent>,
    decision: Option<event_model::Decision>,
    event: &NormalizedEvent,
    modifiers: ModifierSet,
) {
    match decision {
        Some(event_model::Decision::Emit(releases)) => outputs.extend(releases),
        Some(event_model::Decision::Suppress) => {}
        Some(event_model::Decision::Pass) | None => outputs.push(OutputEvent::Key {
            code: event.code,
            phase: EventPhase::Up,
            modifiers,
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
        .filter_map(|output| match output {
            OutputEvent::Key {
                code, modifiers, ..
            } => Some(OutputEvent::Key {
                code: *code,
                phase: EventPhase::Repeat,
                modifiers: *modifiers,
            }),
            OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchApplication) => {
                Some(output.clone())
            }
            OutputEvent::MouseButton { .. } | OutputEvent::SystemShortcut(_) => None,
            other => Some(other.clone()),
        })
        .collect()
}

fn output_code(output: &OutputEvent) -> Option<PhysicalCode> {
    match output {
        OutputEvent::Key { code, .. } => Some(*code),
        OutputEvent::MouseButton { button, .. } => Some(PhysicalCode::MouseButton(*button)),
        _ => None,
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
        _ => None,
    }
}

fn held_output_code(output: &OutputEvent) -> Option<PhysicalCode> {
    match output {
        OutputEvent::Key {
            phase: EventPhase::Down,
            ..
        }
        | OutputEvent::MouseButton {
            phase: EventPhase::Down,
            ..
        } => output_code(output),
        _ => None,
    }
}

fn output_modifier(output: &OutputEvent) -> Option<ModifierSet> {
    let OutputEvent::Key {
        code,
        modifiers,
        phase: EventPhase::Down,
    } = output
    else {
        return None;
    };
    let physical = modifier_for_code(*code)?;
    for (target, semantic) in [
        (55, ModifierSet::LEFT_WINDOWS),
        (54, ModifierSet::RIGHT_WINDOWS),
        (58, ModifierSet::LEFT_ALT),
        (61, ModifierSet::RIGHT_ALT),
    ] {
        if *code == PhysicalCode::key(target) && modifiers.contains(semantic) {
            return Some(semantic);
        }
    }
    Some(physical)
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
            event_model::Decision::Emit(vec![OutputEvent::ScrollTransform {
                invert_x: false,
                invert_y: true,
            }])
        );
        let pass_vertical = Rule {
            id: "vertical-pass".into(),
            enabled: true,
            priority: 1,
            scope: RuleScope::Global,
            trigger: Trigger::Scroll {
                axis: ScrollAxis::Vertical,
            },
            action: Action::Pass,
        };
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![pass_vertical]).unwrap());
        assert_eq!(
            engine.handle_with_scroll_default(&event, KeyboardMappingPolicy::Full, true),
            Decision::Emit(vec![OutputEvent::ScrollTransform {
                invert_x: true,
                invert_y: false
            }])
        );
        assert_eq!(engine.handle(&event), Decision::Pass);
    }

    #[test]
    fn display_desktop_is_one_action_until_the_source_key_is_released() {
        use event_model::SystemShortcut;
        let mut rule = replacement("show-desktop", 96, 1, 1);
        rule.action = Action::Replace(vec![OutputEvent::SystemShortcut(
            SystemShortcut::ShowDesktop,
        )]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        let run = Decision::Emit(vec![OutputEvent::SystemShortcut(
            SystemShortcut::ShowDesktop,
        )]);
        assert_eq!(engine.handle(&key(96, EventPhase::Down)), run);
        for phase in [EventPhase::Repeat, EventPhase::Repeat, EventPhase::Up] {
            assert_eq!(engine.handle(&key(96, phase)), Decision::Suppress);
        }
        assert_eq!(engine.handle(&key(96, EventPhase::Down)), run);
    }

    #[test]
    fn modifier_can_be_the_main_key_of_a_chord_in_both_input_paths() {
        let control = ModifierSet::LEFT_CONTROL;
        let shift = ModifierSet::LEFT_SHIFT;
        let mut rule = replacement("modifier-main-key", 56, 11, 1);
        rule.trigger = Trigger::Key {
            code: PhysicalCode::key(56),
            modifiers: ModifierMatcher {
                required: control | shift,
                exact: true,
            },
            phase: EventPhase::Down,
        };
        let rules = CompiledRules::compile(vec![rule]).unwrap();
        for normalized_phases in [false, true] {
            let mut engine = RuleEngine::new(rules.clone());
            let mut event = modifier(59, control, 1);
            if normalized_phases {
                event.phase = EventPhase::Down;
            }
            engine.handle(&event);
            event.code = PhysicalCode::key(56);
            event.modifiers = control | shift;
            assert!(matches!(engine.handle(&event), Decision::Emit(outputs)
                if matches!(outputs.as_slice(), [OutputEvent::Key { code: PhysicalCode::Key(11), phase: EventPhase::Down, .. }])));
            event.modifiers = control;
            if normalized_phases {
                event.phase = EventPhase::Up;
            }
            assert!(matches!(engine.handle(&event), Decision::Emit(outputs)
                if matches!(outputs.as_slice(), [OutputEvent::Key { code: PhysicalCode::Key(11), phase: EventPhase::Up, .. }])));
        }
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
    fn empty_rules_leave_native_option_unchanged() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        let mut event = key(4, EventPhase::Down);
        event.modifiers = ModifierSet::LEFT_OPTION;
        assert_eq!(engine.handle(&event), event_model::Decision::Pass);
        assert_eq!(
            engine.handle(&key(4, EventPhase::Up)),
            event_model::Decision::Pass
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
    fn flags_changed_native_modifier_pairs_changed_press_and_release() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        let press = modifier(58, ModifierSet::LEFT_OPTION, 1);
        assert_eq!(engine.handle(&press), event_model::Decision::Pass);
        assert!(engine.state().physical_down(PhysicalCode::left_option()));
        assert_eq!(
            engine
                .state()
                .logical_down_count(PhysicalCode::left_option()),
            1
        );
        assert_eq!(
            engine.handle(&modifier(58, ModifierSet::NONE, 2)),
            event_model::Decision::Pass
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

    #[test]
    fn mapped_and_native_sources_share_target_in_every_order() {
        for native_first in [false, true] {
            for native_released_first in [false, true] {
                let mut engine = RuleEngine::new(
                    CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap(),
                );
                let presses = if native_first { [1, 0] } else { [0, 1] };
                engine.handle(&key(presses[0], EventPhase::Down));
                assert_eq!(
                    engine.handle(&key(presses[1], EventPhase::Down)),
                    event_model::Decision::Suppress
                );
                assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 2);
                let releases = if native_released_first {
                    [1, 0]
                } else {
                    [0, 1]
                };
                assert_eq!(
                    engine.handle(&key(releases[0], EventPhase::Up)),
                    event_model::Decision::Suppress
                );
                assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 1);
                let final_release = engine.handle(&key(releases[1], EventPhase::Up));
                if releases[1] == 1 {
                    assert_eq!(final_release, event_model::Decision::Pass);
                } else {
                    assert_eq!(
                        final_release,
                        event_model::Decision::Emit(vec![OutputEvent::Key {
                            code: PhysicalCode::key(1),
                            phase: EventPhase::Up,
                            modifiers: ModifierSet::NONE,
                        }])
                    );
                }
                assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 0);
            }
        }
    }

    #[test]
    fn repeats_and_duplicate_down_do_not_acquire_extra_holds() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap());
        engine.handle(&key(0, EventPhase::Down));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Suppress
        );
        engine.handle(&key(0, EventPhase::Repeat));
        engine.handle(&key(0, EventPhase::Repeat));
        assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 1);
        engine.handle(&key(0, EventPhase::Up));
        assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 0);
    }

    #[test]
    fn release_all_preserves_native_holders_and_releases_mapped_only() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap());
        engine.handle(&key(0, EventPhase::Down));
        engine.handle(&key(1, EventPhase::Down));
        engine.handle(&key(2, EventPhase::Down));
        assert!(engine.release_all().is_empty());
        engine.handle(&key(0, EventPhase::Down));
        assert_eq!(engine.release_all().len(), 1);
        assert!(engine.release_all().is_empty());
    }

    #[test]
    fn restore_checkpoint_discards_uncommitted_mapping() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![replacement("a", 0, 1, 10)]).unwrap());
        let checkpoint = engine.checkpoint();
        engine.handle(&key(0, EventPhase::Down));
        engine.restore(checkpoint);
        assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 0);
        assert!(!engine.state().physical_down(PhysicalCode::key(0)));
        assert!(engine.release_all().is_empty());
    }

    #[test]
    fn modifier_chords_reject_non_atomic_actions() {
        for action in [
            Action::Pass,
            Action::Suppress,
            Action::InvertScroll,
            Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::key(7),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE,
            }]),
            Action::Replace(vec![
                OutputEvent::SystemShortcut(
                    event_model::SystemShortcut::SwitchInputSource
                );
                2
            ]),
        ] {
            let mut rule = modifier_chord_rule();
            rule.action = action;
            assert!(matches!(
                CompiledRules::compile(vec![rule]),
                Err(EngineError::InvalidModifierChord { .. })
            ));
        }
    }

    #[test]
    fn modifier_chords_respect_application_scope_and_unknown_context() {
        let mut rule = modifier_chord_rule();
        rule.scope = RuleScope::App {
            bundle_id: "com.example.Editor".into(),
        };
        for bundle in [None, Some("com.apple.finder"), Some("com.example.Editor")] {
            let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule.clone()]).unwrap());
            let mut event = modifier(59, ModifierSet::LEFT_CONTROL, 1);
            event.app_context = bundle.map(|id| AppContext {
                bundle_id: id.into(),
            });
            engine.handle(&event);
            event.code = PhysicalCode::key(56);
            event.modifiers = ModifierSet::LEFT_SHIFT;
            engine.handle(&event);
            event.modifiers = ModifierSet::NONE;
            let result = engine.handle(&event);
            assert_eq!(
                matches!(result, event_model::Decision::Emit(ref outputs) if outputs.iter().any(|output| matches!(output, OutputEvent::SystemShortcut(_)))),
                bundle == Some("com.example.Editor")
            );
        }
    }

    #[test]
    fn explicit_modifier_rules_drive_character_flags_and_release() {
        let mut rule = replacement("option", 58, 55, 10);
        rule.action = Action::Replace(vec![OutputEvent::Key {
            code: PhysicalCode::key(55),
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_WINDOWS,
        }]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        assert!(
            matches!(engine.handle(&modifier(58, ModifierSet::LEFT_OPTION, 1)), event_model::Decision::Emit(ref outputs) if outputs == &vec![OutputEvent::Key {code: PhysicalCode::key(55), phase: EventPhase::Down, modifiers: ModifierSet::LEFT_WINDOWS}])
        );
        assert_eq!(engine.logical_modifiers(), ModifierSet::LEFT_WINDOWS);
        let mut letter = key(4, EventPhase::Down);
        letter.modifiers = ModifierSet::LEFT_OPTION;
        assert!(
            matches!(engine.handle(&letter), event_model::Decision::Emit(ref outputs) if outputs == &vec![OutputEvent::Key {code: PhysicalCode::key(4), phase: EventPhase::Down, modifiers: ModifierSet::LEFT_WINDOWS}])
        );
        engine.handle(&key(4, EventPhase::Up));
        assert_eq!(
            engine.handle(&modifier(58, ModifierSet::NONE, 2)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }])
        );
        assert_eq!(engine.logical_modifiers(), ModifierSet::NONE);
    }

    #[test]
    fn diagonal_scroll_combines_axis_rules_and_honors_axis_pass() {
        let horizontal = Rule {
            id: "horizontal".into(),
            enabled: true,
            priority: 1,
            scope: RuleScope::Global,
            trigger: Trigger::Scroll {
                axis: ScrollAxis::Horizontal,
            },
            action: Action::InvertScroll,
        };
        let vertical = Rule {
            id: "vertical".into(),
            trigger: Trigger::Scroll {
                axis: ScrollAxis::Vertical,
            },
            ..horizontal.clone()
        };
        let event = NormalizedEvent {
            source: DeviceSource::Mouse,
            kind: EventKind::Scroll,
            phase: EventPhase::Motion,
            code: PhysicalCode::Scroll(ScrollAxis::Vertical),
            scroll: Some(event_model::ScrollDelta { x: 0.25, y: -2.5 }),
            ..key(0, EventPhase::Motion)
        };
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![horizontal.clone(), vertical.clone()]).unwrap(),
        );
        assert_eq!(
            engine.handle(&event),
            event_model::Decision::Emit(vec![OutputEvent::ScrollTransform {
                invert_x: true,
                invert_y: true
            }])
        );
        let mut pass = vertical;
        pass.action = Action::Pass;
        engine.replace_rules(CompiledRules::compile(vec![horizontal, pass]).unwrap());
        assert_eq!(
            engine.handle(&event),
            event_model::Decision::Emit(vec![OutputEvent::ScrollTransform {
                invert_x: true,
                invert_y: false
            }])
        );
    }

    #[test]
    fn scroll_actions_apply_per_moving_axis_without_losing_the_other_axis() {
        use event_model::ScrollDelta;
        use ScrollAxisTransform::{Invert, Keep, Remove};

        let output = |phase| OutputEvent::Key {
            code: PhysicalCode::key(3),
            phase,
            modifiers: ModifierSet::NONE,
        };
        let scroll_rule = |axis, action| Rule {
            id: format!("{axis:?}"),
            enabled: true,
            priority: 0,
            scope: RuleScope::Global,
            trigger: Trigger::Scroll { axis },
            action,
        };
        let rules = |vertical| {
            CompiledRules::compile(vec![
                scroll_rule(
                    ScrollAxis::Horizontal,
                    Action::Replace(vec![output(EventPhase::Down)]),
                ),
                scroll_rule(ScrollAxis::Vertical, vertical),
            ])
            .unwrap()
        };
        let mut engine = RuleEngine::new(rules(Action::InvertScroll));
        let scroll = |x, y, axis| NormalizedEvent {
            source: DeviceSource::Mouse,
            kind: EventKind::Scroll,
            code: PhysicalCode::Scroll(axis),
            phase: EventPhase::Motion,
            scroll: Some(ScrollDelta { x, y }),
            ..key(0, EventPhase::Motion)
        };
        let horizontal = scroll(1.0, 0.0, ScrollAxis::Horizontal);
        let horizontal_gesture = NormalizedEvent {
            phase: EventPhase::ScrollGesture,
            ..horizontal.clone()
        };
        let diagonal = scroll(1.0, 0.125, ScrollAxis::Vertical);
        let vertical = scroll(0.0, 0.125, ScrollAxis::Vertical);
        let phase_only = scroll(0.0, 0.0, ScrollAxis::Vertical);
        let press = output(EventPhase::Down);
        let release = output(EventPhase::Up);
        assert_eq!(
            engine.handle(&horizontal),
            Decision::Emit(vec![press.clone(), release.clone()])
        );
        assert_eq!(
            engine.handle(&horizontal_gesture),
            Decision::Emit(vec![
                press.clone(),
                release.clone(),
                OutputEvent::ScrollTransformAxes {
                    x: Remove,
                    y: Remove,
                },
            ])
        );
        assert_eq!(
            engine.handle(&diagonal),
            Decision::Emit(vec![
                press.clone(),
                release.clone(),
                OutputEvent::ScrollTransformAxes {
                    x: Remove,
                    y: Invert,
                },
            ])
        );
        assert_eq!(
            engine.handle(&vertical),
            Decision::Emit(vec![OutputEvent::ScrollTransform {
                invert_x: false,
                invert_y: true,
            }])
        );
        assert_eq!(engine.handle(&phase_only), Decision::Pass);

        engine.replace_rules(rules(Action::Pass));
        assert_eq!(
            engine.handle(&diagonal),
            Decision::Emit(vec![
                press.clone(),
                release.clone(),
                OutputEvent::ScrollTransformAxes { x: Remove, y: Keep },
            ])
        );
        engine.replace_rules(rules(Action::Suppress));
        assert_eq!(
            engine.handle(&horizontal_gesture),
            Decision::Emit(vec![press.clone(), release.clone()])
        );
        assert_eq!(
            engine.handle(&diagonal),
            Decision::Emit(vec![press, release])
        );
        assert_eq!(engine.handle(&vertical), Decision::Suppress);
        assert_eq!(engine.handle(&phase_only), Decision::Pass);
    }

    #[test]
    fn unmanaged_flags_changes_pass_with_a_mapped_modifier_held() {
        let mut command = replacement("left_command_to_option", 55, 58, 1);
        command.action = Action::Replace(vec![OutputEvent::Key {
            code: PhysicalCode::left_option(),
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_OPTION,
        }]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![command]).unwrap());
        assert!(matches!(
            engine.handle(&modifier(55, ModifierSet::LEFT_COMMAND, 1)),
            Decision::Emit(_)
        ));
        for code in [57, 63] {
            let event = modifier(code, ModifierSet::LEFT_COMMAND, 2);
            assert_eq!(engine.handle(&event), Decision::Pass);
            assert_eq!(engine.modifiers_for_event(&event), ModifierSet::LEFT_OPTION);
            assert!(!engine.state().physical_down(event.code));
        }
    }

    #[test]
    fn mapped_modifier_releases_and_shutdown_preserve_native_control() {
        for drain in [false, true] {
            let mut rule = replacement("option", 58, 55, 10);
            rule.action = Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_WINDOWS,
            }]);
            let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
            assert_eq!(
                engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 1)),
                event_model::Decision::Pass
            );
            engine.handle(&modifier(58, ModifierSet::LEFT_OPTION, 2));
            assert_eq!(
                engine.logical_modifiers(),
                ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_WINDOWS
            );
            let outputs = if drain {
                engine.release_all()
            } else {
                match engine.handle(&modifier(58, ModifierSet::NONE, 3)) {
                    event_model::Decision::Emit(outputs) => outputs,
                    other => panic!("{other:?}"),
                }
            };
            assert_eq!(
                outputs,
                vec![OutputEvent::Key {
                    code: PhysicalCode::key(55),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::LEFT_CONTROL
                }]
            );
        }
    }

    #[test]
    fn keyboard_to_mouse_holds_do_not_repeat_mouse_down() {
        let mut rule = replacement("mouse", 0, 1, 10);
        rule.action = Action::Replace(vec![OutputEvent::MouseButton {
            button: event_model::MouseButton::Right,
            phase: EventPhase::Down,
        }]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        engine.handle(&key(0, EventPhase::Down));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Repeat)),
            event_model::Decision::Suppress
        );
        assert_eq!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Emit(vec![OutputEvent::MouseButton {
                button: event_model::MouseButton::Right,
                phase: EventPhase::Up
            }])
        );
    }

    #[test]
    fn modifier_gesture_release_retains_other_native_modifier_flags() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 1));
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 2));
        assert_eq!(
            engine.handle(&modifier(56, ModifierSet::NONE, 3)),
            event_model::Decision::Emit(vec![
                OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchInputSource),
                OutputEvent::Key {
                    code: PhysicalCode::key(56),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::LEFT_CONTROL
                },
            ])
        );
    }

    #[test]
    fn native_flags_rewrites_follow_current_modifiers_without_new_synthetic_holds() {
        let mut rule = replacement("option", 58, 55, 10);
        rule.action = Action::Replace(vec![OutputEvent::Key {
            code: PhysicalCode::key(55),
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_WINDOWS,
        }]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        engine.handle(&modifier(58, ModifierSet::LEFT_OPTION, 1));
        let mut letter = key(1, EventPhase::Down);
        letter.modifiers = ModifierSet::LEFT_OPTION;
        engine.handle(&letter);
        engine.handle(&modifier(58, ModifierSet::NONE, 2));
        assert_eq!(
            engine.handle(&key(1, EventPhase::Repeat)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Repeat,
                modifiers: ModifierSet::NONE
            }])
        );
        assert!(engine.release_all().is_empty());
    }

    #[test]
    fn native_modifier_pressed_during_mapping_survives_drain() {
        let mut rule = replacement("option", 58, 55, 10);
        rule.action = Action::Replace(vec![OutputEvent::Key {
            code: PhysicalCode::key(55),
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_WINDOWS,
        }]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        engine.handle(&modifier(58, ModifierSet::LEFT_OPTION, 1));
        assert_eq!(
            engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 2)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(59),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_WINDOWS
            }])
        );
        assert_eq!(
            engine.release_all(),
            vec![OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: EventPhase::Up,
                modifiers: ModifierSet::LEFT_CONTROL
            }]
        );
    }

    #[test]
    fn ordinary_event_bootstraps_modifiers_held_before_enable() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        let mut event = key(96, EventPhase::Down);
        event.modifiers = ModifierSet::LEFT_CONTROL;
        assert_eq!(
            engine.modifiers_for_event(&event),
            ModifierSet::LEFT_CONTROL
        );
        engine.handle(&event);
        assert_eq!(engine.logical_modifiers(), ModifierSet::LEFT_CONTROL);
        let mut rule = replacement("option", 58, 55, 10);
        rule.action = Action::Replace(vec![OutputEvent::Key {
            code: PhysicalCode::key(55),
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_WINDOWS,
        }]);
        engine.replace_rules(CompiledRules::compile(vec![rule]).unwrap());
        engine.handle(&modifier(58, ModifierSet::LEFT_OPTION, 1));
        assert_eq!(
            engine.release_all(),
            vec![OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: EventPhase::Up,
                modifiers: ModifierSet::LEFT_CONTROL
            }]
        );
    }

    #[test]
    fn mouse_drag_uses_the_frozen_mapped_button() {
        let mut rule = replacement("mouse", 0, 1, 10);
        rule.trigger = Trigger::MouseButton {
            button: event_model::MouseButton::Right,
            phase: EventPhase::Down,
        };
        rule.action = Action::Replace(vec![OutputEvent::MouseButton {
            button: event_model::MouseButton::Left,
            phase: EventPhase::Down,
        }]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        let down = NormalizedEvent {
            source: DeviceSource::Mouse,
            kind: EventKind::MouseButton,
            phase: EventPhase::Down,
            code: PhysicalCode::MouseButton(event_model::MouseButton::Right),
            ..key(0, EventPhase::Down)
        };
        engine.handle(&down);
        engine.replace_rules(CompiledRules::compile(vec![]).unwrap());
        let drag = NormalizedEvent {
            kind: EventKind::MouseMove,
            phase: EventPhase::Motion,
            ..down.clone()
        };
        assert_eq!(
            engine.handle(&drag),
            event_model::Decision::Emit(vec![OutputEvent::MouseDrag {
                button: event_model::MouseButton::Left
            }])
        );
        let up = NormalizedEvent {
            phase: EventPhase::Up,
            ..down
        };
        assert_eq!(
            engine.handle(&up),
            event_model::Decision::Emit(vec![OutputEvent::MouseButton {
                button: event_model::MouseButton::Left,
                phase: EventPhase::Up
            }])
        );
    }

    #[test]
    fn rule_update_cancels_pending_gesture_without_releasing_native_modifiers() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 1));
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 2));
        engine.replace_rules(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        assert_eq!(
            engine.handle(&modifier(56, ModifierSet::NONE, 3)),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle(&modifier(59, ModifierSet::NONE, 4)),
            event_model::Decision::Pass
        );
    }

    fn assert_same_key_holds(actual: &RuntimeState, expected: &RuntimeState) {
        assert_eq!(actual.physical_down, expected.physical_down);
        assert_eq!(actual.logical_down, expected.logical_down);
        assert_eq!(actual.active_modifiers, expected.active_modifiers);
        assert_eq!(actual.bindings.len(), expected.bindings.len());
        for (code, binding) in &expected.bindings {
            let actual = &actual.bindings[code];
            assert_eq!(actual.outputs, binding.outputs);
            assert_eq!(actual.passthrough, binding.passthrough);
            assert_eq!(actual.kind, binding.kind);
            assert_eq!(actual.raw_passthrough, binding.raw_passthrough);
        }
    }

    fn copy_rule(priority: i32) -> Rule {
        Rule {
            id: "copy".into(),
            enabled: true,
            priority,
            scope: RuleScope::Global,
            trigger: Trigger::Chord {
                modifiers: ModifierSet::LEFT_CONTROL,
                key: PhysicalCode::key(8),
                phase: EventPhase::Down,
            },
            action: Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::key(8),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_COMMAND,
            }]),
        }
    }

    #[test]
    fn shortcuts_only_skips_single_keys_before_matching_lower_priority_shortcuts() {
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![
                replacement("plain-c", 8, 12, 500),
                copy_rule(200),
                replacement("option-to-command", 58, 55, 100),
            ])
            .unwrap(),
        );
        let policy = KeyboardMappingPolicy::ShortcutsOnly;
        assert_eq!(
            engine.handle_with_policy(&modifier(58, ModifierSet::LEFT_OPTION, 1), policy),
            event_model::Decision::Pass
        );
        let plain = NormalizedEvent {
            modifiers: ModifierSet::LEFT_OPTION,
            ..key(8, EventPhase::Down)
        };
        assert_eq!(
            engine.handle_with_policy(&plain, policy),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(
                &NormalizedEvent {
                    phase: EventPhase::Up,
                    ..plain
                },
                policy
            ),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(&modifier(58, ModifierSet::NONE, 2), policy),
            event_model::Decision::Pass
        );
        let copy = NormalizedEvent {
            modifiers: ModifierSet::LEFT_CONTROL,
            ..key(8, EventPhase::Down)
        };
        assert_eq!(
            engine.handle_with_policy(&copy, policy),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(8),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_COMMAND,
            }])
        );
        assert_eq!(
            engine.handle_with_policy(
                &NormalizedEvent {
                    phase: EventPhase::Up,
                    ..copy
                },
                policy
            ),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(8),
                phase: EventPhase::Up,
                modifiers: ModifierSet::LEFT_COMMAND,
            }])
        );
        assert!(engine.state.bindings.is_empty());
    }

    #[test]
    fn policy_changes_keep_old_mapped_and_native_bindings_until_release() {
        let full = KeyboardMappingPolicy::Full;
        let native = KeyboardMappingPolicy::ShortcutsOnly;
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![replacement("a-to-b", 0, 1, 100)]).unwrap(),
        );
        assert!(matches!(
            engine.handle_with_policy(&key(0, EventPhase::Down), full),
            event_model::Decision::Emit(_)
        ));
        assert_eq!(
            engine.handle_with_policy(&key(0, EventPhase::Repeat), native),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Repeat,
                modifiers: ModifierSet::NONE,
            }])
        );
        assert_eq!(
            engine.handle_with_policy(&key(0, EventPhase::Up), native),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE,
            }])
        );
        assert_eq!(
            engine.handle_with_policy(&key(0, EventPhase::Down), native),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(&key(0, EventPhase::Repeat), full),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(&key(0, EventPhase::Up), full),
            event_model::Decision::Pass
        );
        assert!(engine.state.bindings.is_empty());
        assert_eq!(engine.state().logical_down_count(PhysicalCode::key(0)), 0);
        assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 0);
    }

    #[test]
    fn software_pass_binding_stays_raw_when_a_later_modifier_is_mapped() {
        let full = KeyboardMappingPolicy::Full;
        let native = KeyboardMappingPolicy::ShortcutsOnly;
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![replacement("option-to-command", 58, 55, 100)]).unwrap(),
        );
        assert_eq!(
            engine.handle_with_policy(&key(0, EventPhase::Down), native),
            event_model::Decision::Pass
        );
        assert!(matches!(
            engine.handle_with_policy(&modifier(58, ModifierSet::LEFT_OPTION, 1), full),
            event_model::Decision::Emit(_)
        ));
        let repeat = NormalizedEvent {
            modifiers: ModifierSet::LEFT_OPTION,
            ..key(0, EventPhase::Repeat)
        };
        assert_eq!(
            engine.handle_with_policy(&repeat, full),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(
                &NormalizedEvent {
                    phase: EventPhase::Up,
                    ..repeat
                },
                full
            ),
            event_model::Decision::Pass
        );
        assert!(matches!(
            engine.handle_with_policy(&modifier(58, ModifierSet::NONE, 2), full),
            event_model::Decision::Emit(_)
        ));
        assert!(engine.state.bindings.is_empty());
    }

    #[test]
    fn shortcut_lookup_respects_priority_disabled_rules_and_application_scope() {
        let mut high = copy_rule(300);
        high.id = "disabled".into();
        high.enabled = false;
        high.action = Action::Suppress;
        let mut scoped = copy_rule(250);
        scoped.id = "scoped".into();
        scoped.scope = RuleScope::App {
            bundle_id: "editor".into(),
        };
        scoped.action = Action::Suppress;
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![high, scoped, copy_rule(100)]).unwrap());
        let event = NormalizedEvent {
            modifiers: ModifierSet::LEFT_CONTROL,
            ..key(8, EventPhase::Down)
        };
        assert_eq!(engine.shortcut_action(&event), Some(copy_rule(100).action));
        assert_eq!(
            engine.shortcut_action(&NormalizedEvent {
                app_context: Some(AppContext {
                    bundle_id: "editor".into()
                }),
                ..event.clone()
            }),
            Some(Action::Suppress)
        );
        assert!(engine.state.bindings.is_empty());
        let decision = engine.handle_with_policy(&event, KeyboardMappingPolicy::ShortcutsOnly);
        assert!(matches!(decision, event_model::Decision::Emit(_)));
    }

    #[test]
    fn shortcuts_only_preserves_project_function_bound_to_a_single_key() {
        let mut rule = replacement("clipboard", 96, 97, 100);
        rule.action = Action::Replace(vec![OutputEvent::SystemShortcut(
            event_model::SystemShortcut::ShowClipboard,
        )]);
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![rule]).unwrap());
        assert_eq!(
            engine.handle_with_policy(
                &key(96, EventPhase::Down),
                KeyboardMappingPolicy::ShortcutsOnly
            ),
            event_model::Decision::Emit(vec![OutputEvent::SystemShortcut(
                event_model::SystemShortcut::ShowClipboard
            )])
        );
        assert_eq!(
            engine.handle_with_policy(
                &key(96, EventPhase::Up),
                KeyboardMappingPolicy::ShortcutsOnly
            ),
            event_model::Decision::Suppress
        );
    }

    #[test]
    fn software_shift_and_control_shift_functions_keep_the_original_release() {
        let policy = KeyboardMappingPolicy::ShortcutsOnly;
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        engine.set_pinyin_active(true);
        assert_eq!(
            engine.handle_with_policy(&modifier(56, ModifierSet::LEFT_SHIFT, 1), policy),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(&modifier(56, ModifierSet::NONE, 2), policy),
            event_model::Decision::Emit(vec![
                OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish),
                OutputEvent::Key {
                    code: PhysicalCode::key(56),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::NONE
                },
            ])
        );
        assert_eq!(
            engine.handle_with_policy(&modifier(59, ModifierSet::LEFT_CONTROL, 3), policy),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(&modifier(56, ModifierSet::LEFT_SHIFT, 4), policy),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.handle_with_policy(&modifier(56, ModifierSet::NONE, 5), policy),
            event_model::Decision::Emit(vec![
                OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchInputSource),
                OutputEvent::Key {
                    code: PhysicalCode::key(56),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::LEFT_CONTROL
                },
            ])
        );
        assert_eq!(
            engine.handle_with_policy(&modifier(59, ModifierSet::NONE, 6), policy),
            event_model::Decision::Pass
        );
        assert!(engine.state.bindings.is_empty());
    }

    #[test]
    fn policy_update_cancels_pending_gestures_without_losing_the_release() {
        let mut engine =
            RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle_with_policy(
            &modifier(56, ModifierSet::LEFT_SHIFT, 1),
            KeyboardMappingPolicy::ShortcutsOnly,
        );
        assert_eq!(
            engine.handle_with_policy(
                &modifier(56, ModifierSet::NONE, 2),
                KeyboardMappingPolicy::Full
            ),
            event_model::Decision::Pass
        );
        assert!(engine.state.bindings.is_empty());
    }

    #[test]
    fn checkpoint_restores_policy_with_its_pending_gestures() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        let checkpoint = engine.checkpoint();
        engine.set_keyboard_mapping_policy(KeyboardMappingPolicy::ShortcutsOnly);
        engine.restore(checkpoint);
        assert_eq!(
            engine.keyboard_mapping_policy(),
            KeyboardMappingPolicy::Full
        );
        assert_eq!(
            engine.handle_with_policy(
                &modifier(56, ModifierSet::NONE, 2),
                KeyboardMappingPolicy::ShortcutsOnly
            ),
            event_model::Decision::Pass
        );
    }

    #[test]
    fn known_text_does_not_change_policy_or_enter_shortcut_matching() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![copy_rule(100)]).unwrap());
        engine.set_keyboard_mapping_policy(KeyboardMappingPolicy::ShortcutsOnly);
        let text = NormalizedEvent {
            origin: EventOrigin::TextInjection { marker: 100 },
            modifiers: ModifierSet::LEFT_CONTROL,
            ..key(8, EventPhase::Down)
        };
        assert_eq!(
            engine.handle_with_policy(&text, KeyboardMappingPolicy::Full),
            event_model::Decision::Pass
        );
        assert_eq!(
            engine.keyboard_mapping_policy(),
            KeyboardMappingPolicy::ShortcutsOnly
        );
        assert_eq!(engine.shortcut_action(&text), None);
        assert!(engine.state.bindings.is_empty());
    }

    #[test]
    fn software_down_only_passthrough_stays_usable_without_extra_holders() {
        for policy in [
            KeyboardMappingPolicy::Full,
            KeyboardMappingPolicy::ShortcutsOnly,
        ] {
            let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
            engine.set_software_input(true);
            for _ in 0..5 {
                assert_eq!(
                    engine.handle_with_policy(&key(12, EventPhase::Down), policy),
                    event_model::Decision::Pass
                );
                assert_eq!(engine.state().logical_down_count(PhysicalCode::key(12)), 1);
            }
            assert_eq!(
                engine.handle_with_policy(&key(12, EventPhase::Up), policy),
                event_model::Decision::Pass
            );
            assert_eq!(engine.state().logical_down_count(PhysicalCode::key(12)), 0);
            assert!(engine.state.bindings.is_empty());
        }
    }

    #[test]
    fn software_duplicate_down_does_not_leak_a_mapped_or_consumed_source() {
        let mut suppressed = replacement("suppress", 8, 9, 100);
        suppressed.action = Action::Suppress;
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![replacement("a-to-b", 0, 1, 100), suppressed]).unwrap(),
        );
        engine.set_software_input(true);
        assert!(matches!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Emit(_)
        ));
        assert_eq!(
            engine.handle(&key(0, EventPhase::Down)),
            event_model::Decision::Suppress
        );
        assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 1);
        assert_eq!(
            engine.handle(&key(8, EventPhase::Down)),
            event_model::Decision::Suppress
        );
        assert_eq!(
            engine.handle(&key(8, EventPhase::Down)),
            event_model::Decision::Suppress
        );
        assert_eq!(
            engine.handle(&key(8, EventPhase::Up)),
            event_model::Decision::Suppress
        );
        assert!(matches!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Emit(_)
        ));
        assert!(engine.state.bindings.is_empty());
    }

    #[test]
    fn text_injection_down_only_events_pass_without_registering_or_matching_a_key() {
        for marker in [0, 100] {
            let mut engine = RuleEngine::new(
                CompiledRules::compile(vec![replacement("local-a", 0, 1, 10)]).unwrap(),
            );
            let external = NormalizedEvent {
                origin: EventOrigin::TextInjection { marker },
                ..key(0, EventPhase::Down)
            };
            for _ in 0..4 {
                assert_eq!(engine.handle(&external), event_model::Decision::Pass);
                assert!(!engine.state().physical_down(PhysicalCode::key(0)));
                assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 0);
                assert!(engine.state.bindings.is_empty());
            }
            assert_eq!(
                engine.handle(&key(0, EventPhase::Down)),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::NONE
                }])
            );
            assert_eq!(
                engine.handle(&key(0, EventPhase::Down)),
                event_model::Decision::Suppress
            );
            assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 1);
            assert_eq!(
                engine.handle(&key(0, EventPhase::Up)),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::NONE
                }])
            );
        }
    }

    #[test]
    fn text_injection_cannot_change_local_holds_even_with_the_same_keycode() {
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![replacement("local-a", 0, 1, 10)]).unwrap(),
        );
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        engine.handle(&key(0, EventPhase::Down));
        let checkpoint = engine.checkpoint();
        let taps = engine.state.shift_taps.len();
        let interrupted = engine.state.modifier_chord_interrupted;
        for marker in [0, 100] {
            for event in [
                key(0, EventPhase::Down),
                key(0, EventPhase::Up),
                key(0, EventPhase::Repeat),
                modifier(56, ModifierSet::NONE, 2),
                modifier(59, ModifierSet::LEFT_CONTROL, 3),
            ] {
                let external = NormalizedEvent {
                    origin: EventOrigin::TextInjection { marker },
                    ..event
                };
                assert_eq!(engine.handle(&external), event_model::Decision::Pass);
                assert_same_key_holds(engine.state(), &checkpoint);
                assert_eq!(engine.state.shift_taps.len(), taps);
                assert_eq!(engine.state.modifier_chord_interrupted, interrupted);
            }
        }
        assert_eq!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }])
        );
    }

    #[test]
    fn text_injection_entry_guard_does_not_cancel_a_pending_local_shift_tap() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        let external = NormalizedEvent {
            origin: EventOrigin::TextInjection { marker: 100 },
            ..key(0, EventPhase::Down)
        };
        assert_eq!(engine.handle(&external), event_model::Decision::Pass);
        assert!(
            matches!(engine.handle(&modifier(56, ModifierSet::NONE, 100)), event_model::Decision::Emit(ref outputs)
            if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish)))
        );
    }

    #[test]
    fn interrupt_gestures_cancels_shift_without_releasing_any_local_hold() {
        let mut engine = RuleEngine::new(
            CompiledRules::compile(vec![replacement("local-a", 0, 1, 10)]).unwrap(),
        );
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        engine.handle(&key(0, EventPhase::Down));
        let checkpoint = engine.checkpoint();
        engine.interrupt_gestures();
        assert_same_key_holds(engine.state(), &checkpoint);
        assert!(engine.state.shift_taps.is_empty());
        assert!(engine.state.modifier_chord_interrupted);
        assert!(engine.state.gestures_blocked_until_modifiers_up);
        assert_eq!(
            engine.handle(&key(0, EventPhase::Up)),
            event_model::Decision::Emit(vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }])
        );
        assert_eq!(
            engine.handle(&modifier(56, ModifierSet::NONE, 100)),
            event_model::Decision::Pass
        );
        assert!(!engine.state.gestures_blocked_until_modifiers_up);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 200));
        assert!(
            matches!(engine.handle(&modifier(56, ModifierSet::NONE, 300)), event_model::Decision::Emit(ref outputs)
            if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish)))
        );
    }

    #[test]
    fn interrupted_modifier_chords_stay_blocked_until_all_local_modifiers_release() {
        for already_formed in [false, true] {
            let mut engine =
                RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
            engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 1));
            if already_formed {
                engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 2));
            }
            engine.interrupt_gestures();
            if !already_formed {
                engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 3));
            }
            assert!(engine.state.shift_taps.is_empty());
            assert!(engine.state.modifier_chord_interrupted);
            assert_eq!(
                engine.handle(&modifier(56, ModifierSet::NONE, 4)),
                event_model::Decision::Pass
            );
            assert!(engine.state.gestures_blocked_until_modifiers_up);
            engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 5));
            assert!(engine.state.shift_taps.is_empty());
            assert_eq!(
                engine.handle(&modifier(56, ModifierSet::NONE, 6)),
                event_model::Decision::Pass
            );
            assert!(engine.state.gestures_blocked_until_modifiers_up);
            assert_eq!(
                engine.handle(&modifier(59, ModifierSet::NONE, 7)),
                event_model::Decision::Pass
            );
            assert!(!engine.state.gestures_blocked_until_modifiers_up);
            engine.handle(&modifier(59, ModifierSet::LEFT_CONTROL, 8));
            engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 9));
            assert!(
                matches!(engine.handle(&modifier(56, ModifierSet::NONE, 10)), event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchInputSource)))
            );
        }
    }

    #[test]
    fn interrupt_without_held_modifiers_does_not_block_the_next_shift_tap() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.interrupt_gestures();
        assert!(!engine.state.gestures_blocked_until_modifiers_up);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        assert!(
            matches!(engine.handle(&modifier(56, ModifierSet::NONE, 100)), event_model::Decision::Emit(ref outputs)
            if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish)))
        );
    }

    #[test]
    fn release_all_resets_the_interrupted_gesture_latch() {
        let mut engine = RuleEngine::new(CompiledRules::compile(vec![]).unwrap());
        engine.set_pinyin_active(true);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 1));
        engine.interrupt_gestures();
        assert!(engine.state.gestures_blocked_until_modifiers_up);
        assert!(engine.release_all().is_empty());
        assert!(!engine.state.gestures_blocked_until_modifiers_up);
        engine.handle(&modifier(56, ModifierSet::LEFT_SHIFT, 2));
        assert!(
            matches!(engine.handle(&modifier(56, ModifierSet::NONE, 100)), event_model::Decision::Emit(ref outputs)
            if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish)))
        );
    }

    #[test]
    fn ordinary_injected_keycode_streams_still_map_and_pair_releases() {
        for marker in [0, 123] {
            let mut engine = RuleEngine::new(
                CompiledRules::compile(vec![replacement("deskflow-a", 0, 1, 10)]).unwrap(),
            );
            let down = NormalizedEvent {
                origin: EventOrigin::Injected { marker },
                ..key(0, EventPhase::Down)
            };
            assert_eq!(
                engine.handle(&down),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::NONE,
                }])
            );
            assert!(engine.state().physical_down(PhysicalCode::key(0)));
            assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 1);
            let repeat = NormalizedEvent {
                phase: EventPhase::Repeat,
                ..down.clone()
            };
            assert_eq!(
                engine.handle(&repeat),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Repeat,
                    modifiers: ModifierSet::NONE,
                }])
            );
            let up = NormalizedEvent {
                phase: EventPhase::Up,
                ..down
            };
            assert_eq!(
                engine.handle(&up),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::NONE,
                }])
            );
            assert!(!engine.state().physical_down(PhysicalCode::key(0)));
            assert_eq!(engine.state().logical_down_count(PhysicalCode::key(1)), 0);
        }
    }

    #[test]
    fn ordinary_injected_control_copy_still_matches_shortcut_rule() {
        for marker in [0, 123] {
            let mut copy = replacement("deskflow-copy", 8, 8, 200);
            copy.trigger = Trigger::Chord {
                modifiers: ModifierSet::LEFT_CONTROL,
                key: PhysicalCode::key(8),
                phase: EventPhase::Down,
            };
            copy.action = Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::key(8),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_COMMAND,
            }]);
            let mut engine = RuleEngine::new(CompiledRules::compile(vec![copy]).unwrap());
            let down = NormalizedEvent {
                origin: EventOrigin::Injected { marker },
                modifiers: ModifierSet::LEFT_CONTROL,
                ..key(8, EventPhase::Down)
            };
            assert_eq!(
                engine.handle(&down),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(8),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::LEFT_COMMAND,
                }])
            );
            let up = NormalizedEvent {
                phase: EventPhase::Up,
                ..down
            };
            assert_eq!(
                engine.handle(&up),
                event_model::Decision::Emit(vec![OutputEvent::Key {
                    code: PhysicalCode::key(8),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::LEFT_COMMAND,
                }])
            );
        }
    }

    #[test]
    fn ordinary_injected_modifiers_can_form_input_source_gestures() {
        for marker in [0, 123] {
            let mut engine =
                RuleEngine::new(CompiledRules::compile(vec![modifier_chord_rule()]).unwrap());
            for event in [
                modifier(59, ModifierSet::LEFT_CONTROL, 1),
                modifier(56, ModifierSet::LEFT_SHIFT, 2),
            ] {
                let event = NormalizedEvent {
                    origin: EventOrigin::Injected { marker },
                    ..event
                };
                assert_eq!(engine.handle(&event), event_model::Decision::Pass);
            }
            let shift_up = NormalizedEvent {
                origin: EventOrigin::Injected { marker },
                ..modifier(56, ModifierSet::NONE, 3)
            };
            assert!(
                matches!(engine.handle(&shift_up), event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::SwitchInputSource)))
            );
            let control_up = NormalizedEvent {
                origin: EventOrigin::Injected { marker },
                ..modifier(59, ModifierSet::NONE, 4)
            };
            assert_eq!(engine.handle(&control_up), event_model::Decision::Pass);
            engine.set_pinyin_active(true);
            let shift_down = NormalizedEvent {
                origin: EventOrigin::Injected { marker },
                ..modifier(56, ModifierSet::LEFT_SHIFT, 5)
            };
            engine.handle(&shift_down);
            let shift_up = NormalizedEvent {
                origin: EventOrigin::Injected { marker },
                ..modifier(56, ModifierSet::NONE, 6)
            };
            assert!(
                matches!(engine.handle(&shift_up), event_model::Decision::Emit(ref outputs)
                if outputs.contains(&OutputEvent::SystemShortcut(event_model::SystemShortcut::TogglePinyinEnglish)))
            );
        }
    }
}
