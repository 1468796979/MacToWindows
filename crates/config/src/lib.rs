use event_model::{
    EventPhase, ModifierSet, MouseButton, OutputEvent, PhysicalCode, ScrollAxis, SystemShortcut,
};
use rule_engine::{
    Action, CompiledRules, EngineError, ModifierMatcher, Rule as EngineRule, RuleScope, Trigger,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
};
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

const BUILT_IN_PROFILE_IDS: [&str; 3] = ["windows_physical", "shortcut", "custom"];
const DEFAULT_SAVE_TRIGGERS: [&str; 3] = [
    "exact_chord:left_control+key:1",
    "exact_chord:right_control+key:1",
    "exact_chord:left_control|right_control+key:1",
];
const DEFAULT_REDO_TRIGGERS: [&str; 3] = [
    "exact_chord:left_control+key:16",
    "exact_chord:right_control+key:16",
    "exact_chord:left_control|right_control+key:16",
];
const DEFAULT_SAVE_ACTION: &str = "command:key:1";
const DEFAULT_REDO_ACTION: &str = "modified:left_command|left_shift+key:6";
const CONTROL_SIDES: [&str; 3] = [
    "left_control",
    "right_control",
    "left_control|right_control",
];
const SHIFT_VARIANTS: [&str; 3] = ["left_shift", "right_shift", "left_shift|right_shift"];
const LEGACY_STOCK_MODIFIERS: [(&str, &str); 4] = [
    ("left_option", "left_windows"),
    ("right_option", "right_windows"),
    ("left_command", "left_alt"),
    ("right_command", "right_alt"),
];
const LEGACY_STOCK_UNDO: (&str, &str) = ("chord:left_control+key:6", "command:key:6");
const LEGACY_STOCK_EDIT_SHORTCUTS: [(&str, &str); 5] = [
    ("chord:left_control+key:0", "command:key:0"),
    ("chord:left_control+key:8", "command:key:8"),
    ("chord:left_control+key:9", "command:key:9"),
    ("chord:left_control+key:7", "command:key:7"),
    LEGACY_STOCK_UNDO,
];

/// A deliberately small, extensible rule representation. Additional fields can be
/// added without coupling this crate to the rule engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub action: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub app_bundle_id: Option<String>,
    #[serde(default)]
    pub device_identifier: Option<String>,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "自定义规则".into(),
            trigger: String::new(),
            action: String::new(),
            enabled: true,
            priority: 0,
            app_bundle_id: None,
            device_identifier: None,
        }
    }
}

fn default_true() -> bool {
    true
}

pub type KeyboardRule = Rule;
pub type MouseRule = Rule;
pub type GlobalHotkey = Rule;
pub type AppRule = Rule;
pub type DeviceRule = Rule;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseConfig {
    #[serde(default = "default_scroll_mode")]
    pub scroll_mode: String,
    #[serde(default)]
    scroll_mode_initialized: bool,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            scroll_mode: default_scroll_mode(),
            scroll_mode_initialized: true,
        }
    }
}

fn default_scroll_mode() -> String {
    "system".into()
}

impl MouseConfig {
    fn valid_mode(&self) -> bool {
        matches!(
            self.scroll_mode.as_str(),
            "system" | "natural" | "non_natural"
        )
    }

    fn migrate_scroll_mode(&mut self) -> bool {
        if self.scroll_mode_initialized && self.valid_mode() {
            return false;
        }
        self.scroll_mode = default_scroll_mode();
        self.scroll_mode_initialized = true;
        true
    }

    pub fn desired_scroll_inversion(&self) -> Option<bool> {
        match self.scroll_mode.as_str() {
            "natural" => Some(true),
            "non_natural" => Some(false),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub keyboard_rules: Vec<KeyboardRule>,
    #[serde(default)]
    pub mouse_rules: Vec<MouseRule>,
    #[serde(default)]
    pub global_hotkeys: Vec<GlobalHotkey>,
    #[serde(default)]
    pub app_rules: Vec<AppRule>,
    #[serde(default)]
    pub device_rules: Vec<DeviceRule>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemShortcutsConfig {
    #[serde(default = "default_show_desktop")]
    pub show_desktop: String,
}

fn default_show_desktop() -> String {
    "key:103".into()
}

impl Default for SystemShortcutsConfig {
    fn default() -> Self {
        Self {
            show_desktop: default_show_desktop(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_active_profile")]
    pub active_profile: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_software_input_passthrough")]
    pub software_input_passthrough: bool,
    #[serde(default)]
    pub default_save_shortcuts_initialized: bool,
    #[serde(default)]
    pub default_redo_shortcuts_initialized: bool,
    #[serde(default)]
    pub default_edit_control_sides_initialized: bool,
    #[serde(default = "default_panic_hotkey")]
    pub panic_hotkey: Vec<String>,
    #[serde(default)]
    pub mouse: MouseConfig,
    #[serde(default)]
    pub system_shortcuts: SystemShortcutsConfig,
    #[serde(default)]
    pub keyboard_rules: Vec<KeyboardRule>,
    #[serde(default)]
    pub mouse_rules: Vec<MouseRule>,
    #[serde(default)]
    pub global_hotkeys: Vec<GlobalHotkey>,
    #[serde(default)]
    pub app_rules: Vec<AppRule>,
    #[serde(default)]
    pub device_rules: Vec<DeviceRule>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Default for AppConfig {
    fn default() -> Self {
        default_config()
    }
}

// Missing versions belong to the legacy schema and must run migration.
fn default_schema_version() -> u32 {
    1
}
fn default_active_profile() -> String {
    "windows_physical".to_owned()
}

fn default_software_input_passthrough() -> bool {
    true
}

fn default_panic_hotkey() -> Vec<String> {
    vec![
        "left_control".into(),
        "left_option".into(),
        "left_command".into(),
        "escape".into(),
    ]
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("TOML serialization error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("unsupported schema version {0}")]
    UnsupportedSchema(u32),
    #[error("active profile does not exist: {0}")]
    UnknownProfile(String),
    #[error("invalid rule: {0}")]
    InvalidRule(String),
    #[error("unsupported rule scope: {0}")]
    UnsupportedScope(String),
    #[error("rule compilation failed: {0}")]
    RuleCompile(#[from] EngineError),
}

#[derive(Debug)]
pub enum SaveOutcome {
    Saved,
    DirectorySyncFailed(io::Error),
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        if !self.profiles.contains_key(&self.active_profile) {
            return Err(ConfigError::UnknownProfile(self.active_profile.clone()));
        }
        if !self.mouse.valid_mode() {
            return Err(ConfigError::InvalidRule(
                "mouse.scroll_mode must be system, natural, or non_natural".into(),
            ));
        }
        for (category, rules) in [
            ("keyboard_rules", &self.keyboard_rules),
            ("mouse_rules", &self.mouse_rules),
            ("global_hotkeys", &self.global_hotkeys),
            ("app_rules", &self.app_rules),
            ("device_rules", &self.device_rules),
        ] {
            if !rules.is_empty() {
                return Err(ConfigError::InvalidRule(format!(
                    "{category}: schema 2 requires rules inside an explicit profile; import legacy rules as schema 1"
                )));
            }
        }
        let mut ids = HashSet::new();
        for (profile_id, profile) in &self.profiles {
            for (category, rules) in profile_rule_lists(profile) {
                for (index, rule) in rules.iter().enumerate() {
                    let path = format!("profiles.{profile_id}.{category}[{index}]");
                    let id = uuid::Uuid::parse_str(&rule.id)
                        .ok()
                        .filter(|id| id.get_version_num() == 4)
                        .ok_or_else(|| {
                            ConfigError::InvalidRule(format!("{path}.id must be a UUIDv4"))
                        })?;
                    if !ids.insert(id) {
                        return Err(ConfigError::InvalidRule(format!("{path}.id is duplicated")));
                    }
                    if rule.name.trim().is_empty() {
                        return Err(ConfigError::InvalidRule(format!(
                            "{path}.name must not be empty"
                        )));
                    }
                }
            }
            compile_profile(profile_id, profile)?;
        }
        self.show_desktop_output()?;
        Ok(())
    }

    /// Desktop releases do not yet supply a reliable foreground application
    /// context. Reject enabled scoped rules rather than silently globalizing them.
    pub fn validate_desktop(&self) -> Result<(), ConfigError> {
        self.validate()?;
        for (profile_id, profile) in &self.profiles {
            if let Some((index, _)) = profile
                .app_rules
                .iter()
                .enumerate()
                .find(|(_, rule)| rule.enabled)
            {
                return Err(ConfigError::UnsupportedScope(format!("profiles.{profile_id}.app_rules[{index}]: desktop application context is unavailable")));
            }
        }
        Ok(())
    }

    pub fn compile_active_profile(&self) -> Result<CompiledRules, ConfigError> {
        self.validate()?;
        compile_profile(&self.active_profile, &self.profiles[&self.active_profile])
    }

    /// Mark old saved settings as evaluated and add Save defaults only to
    /// built-in profiles still recognizably based on the old keyboard preset.
    pub fn upgrade_default_save_shortcuts(&mut self) -> bool {
        if self.default_save_shortcuts_initialized {
            return false;
        }
        self.default_save_shortcuts_initialized = true;
        for id in BUILT_IN_PROFILE_IDS {
            if let Some(profile) = self
                .profiles
                .get_mut(id)
                .filter(|profile| has_legacy_stock_keyboard_rules(profile))
            {
                add_missing_default_chords(profile, DEFAULT_SAVE_TRIGGERS, DEFAULT_SAVE_ACTION);
            }
        }
        true
    }

    /// Add Redo to older built-in profiles that still use the default Undo.
    pub fn upgrade_default_redo_shortcuts(&mut self) -> bool {
        if self.default_redo_shortcuts_initialized {
            return false;
        }
        self.default_redo_shortcuts_initialized = true;
        for id in BUILT_IN_PROFILE_IDS {
            if let Some(profile) = self
                .profiles
                .get_mut(id)
                .filter(|profile| has_stock_keyboard_rule(profile, LEGACY_STOCK_UNDO, true))
            {
                add_missing_default_chords(profile, DEFAULT_REDO_TRIGGERS, DEFAULT_REDO_ACTION);
            }
        }
        true
    }

    pub fn upgrade_default_edit_shortcuts(&mut self) -> bool {
        let mut upgraded = false;
        for id in BUILT_IN_PROFILE_IDS {
            let Some(profile) = self.profiles.get_mut(id) else {
                continue;
            };
            for (trigger, action) in LEGACY_STOCK_EDIT_SHORTCUTS {
                let key = trigger.strip_prefix("chord:left_control+").unwrap();
                if !matches!(key, "key:6" | "key:9") {
                    continue;
                }
                let name = format!("{trigger} → {action}");
                let Some(index) = profile.keyboard_rules.iter().position(|rule| {
                    rule.name == name && rule.trigger == trigger && rule.action == action
                }) else {
                    continue;
                };
                let exact = format!("exact_chord:left_control+{key}");
                let original_id = profile.keyboard_rules[index].id.clone();
                let explicit_override = |trigger: &str| {
                    let Some(Trigger::Key {
                        code,
                        modifiers,
                        phase,
                    }) = parse_trigger(trigger)
                    else {
                        return false;
                    };
                    global_profile_rules(profile).any(|rule| {
                        rule.id != original_id
                            && parse_trigger(&rule.trigger).is_some_and(|existing| {
                                !matches!(&existing, Trigger::Key { modifiers, .. } if modifiers.required == ModifierSet::NONE)
                                    && matches_key_input(&existing, code, modifiers.required, phase)
                            })
                    })
                };
                let original = &profile.keyboard_rules[index];
                let exact_trigger = parse_trigger(&exact);
                let plain_conflict = global_profile_rules(profile).any(|rule| {
                    rule.enabled
                        && rule.id != original_id
                        && rule.priority == original.priority
                        && parse_trigger(&rule.trigger) == exact_trigger
                        && parse_action(&rule.action) != parse_action(action)
                });
                let mut variants = Vec::new();
                for shift in SHIFT_VARIANTS {
                    let trigger = format!("exact_chord:left_control|{shift}+{key}");
                    if !explicit_override(&trigger) {
                        let action = format!("modified:left_command|{shift}+{key}");
                        variants.push(Rule {
                            name: format!("{trigger} → {action}"),
                            trigger,
                            action,
                            priority: original.priority,
                            enabled: original.enabled,
                            ..Rule::default()
                        });
                    }
                }
                let original = &mut profile.keyboard_rules[index];
                original.trigger = exact;
                original.name = format!("{} → {}", original.trigger, original.action);
                original.enabled &= !plain_conflict;
                profile.keyboard_rules.extend(variants);
                upgraded = true;
            }
        }
        upgraded
    }

    /// Extend the existing stock rules without touching a user's original
    /// shortcut, including any deliberately disabled variant.
    pub fn upgrade_default_edit_control_sides(&mut self) -> bool {
        if self.default_edit_control_sides_initialized {
            return false;
        }
        self.default_edit_control_sides_initialized = true;
        for id in BUILT_IN_PROFILE_IDS {
            let Some(profile) = self.profiles.get_mut(id) else {
                continue;
            };
            let uses_stock_edits = LEGACY_STOCK_EDIT_SHORTCUTS
                .into_iter()
                .any(|rule| has_stock_keyboard_rule(profile, rule, true));
            for (trigger, action) in LEGACY_STOCK_EDIT_SHORTCUTS {
                let key = trigger.strip_prefix("chord:left_control+").unwrap();
                mirror_control_rule(profile, trigger, "", key, action);
                if matches!(key, "key:6" | "key:9") {
                    for shift in SHIFT_VARIANTS {
                        let source = format!("exact_chord:left_control|{shift}+{key}");
                        let output = format!("modified:left_command|{shift}+{key}");
                        mirror_control_rule(profile, &source, &format!("|{shift}"), key, &output);
                    }
                }
            }
            if uses_stock_edits {
                for control in CONTROL_SIDES {
                    let trigger = format!("exact_chord:{control}+key:3");
                    if !has_matching_global_rule(profile, &trigger) {
                        profile
                            .keyboard_rules
                            .push(default_chord_rule(&trigger, "command:key:3"));
                    }
                }
            }
        }
        true
    }

    pub fn show_desktop_output(&self) -> Result<Vec<OutputEvent>, ConfigError> {
        let (key, modifiers) = match parse_trigger(&self.system_shortcuts.show_desktop) {
            Some(Trigger::Key {
                code,
                modifiers,
                phase: EventPhase::Down,
            }) => (code, modifiers.required),
            Some(Trigger::Chord {
                key,
                modifiers,
                phase: EventPhase::Down,
            }) => (key, modifiers),
            _ => {
                return Err(ConfigError::InvalidRule(
                    "system_shortcuts.show_desktop must be a key or modifier+key chord".into(),
                ))
            }
        };
        if modifier_codes()
            .iter()
            .any(|(code, flag)| *code == key && modifiers.contains(*flag))
        {
            return Err(ConfigError::InvalidRule(
                "system_shortcuts.show_desktop repeats the main modifier key".into(),
            ));
        }
        let mut outputs = Vec::new();
        let mut flags = ModifierSet::NONE;
        let held: Vec<_> = modifier_codes()
            .into_iter()
            .filter(|(_, flag)| modifiers.contains(*flag))
            .collect();
        for (code, flag) in &held {
            flags.insert(*flag);
            outputs.push(OutputEvent::Key {
                code: *code,
                phase: EventPhase::Down,
                modifiers: flags,
            });
        }
        outputs.push(OutputEvent::Key {
            code: key,
            phase: EventPhase::Down,
            modifiers: flags
                | modifier_codes()
                    .into_iter()
                    .find(|(code, _)| *code == key)
                    .map(|(_, modifier)| modifier)
                    .unwrap_or(ModifierSet::NONE),
        });
        outputs.push(OutputEvent::Key {
            code: key,
            phase: EventPhase::Up,
            modifiers: flags,
        });
        for (code, flag) in held.into_iter().rev() {
            flags.remove(flag);
            outputs.push(OutputEvent::Key {
                code,
                phase: EventPhase::Up,
                modifiers: flags,
            });
        }
        Ok(outputs)
    }
}

fn global_profile_rules(profile: &Profile) -> impl Iterator<Item = &Rule> {
    profile
        .keyboard_rules
        .iter()
        .chain(&profile.mouse_rules)
        .chain(&profile.global_hotkeys)
}

fn has_legacy_stock_keyboard_rules(profile: &Profile) -> bool {
    LEGACY_STOCK_MODIFIERS
        .into_iter()
        .chain(LEGACY_STOCK_EDIT_SHORTCUTS)
        .all(|rule| has_stock_keyboard_rule(profile, rule, false))
        && LEGACY_STOCK_EDIT_SHORTCUTS
            .into_iter()
            .any(|rule| has_stock_keyboard_rule(profile, rule, true))
}

fn has_stock_keyboard_rule(
    profile: &Profile,
    (trigger, action): (&str, &str),
    enabled_only: bool,
) -> bool {
    let exact = trigger
        .strip_prefix("chord:")
        .and_then(|chord| parse_trigger(&format!("exact_chord:{chord}")));
    let trigger = parse_trigger(trigger);
    let action = parse_action(action);
    global_profile_rules(profile).any(|rule| {
        let existing = parse_trigger(&rule.trigger);
        (!enabled_only || rule.enabled)
            && (existing == trigger || exact.is_some() && existing == exact)
            && parse_action(&rule.action) == action
    })
}

fn add_missing_default_chords(profile: &mut Profile, triggers: [&str; 3], action: &str) {
    for trigger in triggers {
        if !has_matching_global_rule(profile, trigger) {
            profile
                .keyboard_rules
                .push(default_chord_rule(trigger, action));
        }
    }
}

fn has_matching_global_rule(profile: &Profile, trigger: &str) -> bool {
    let Some(Trigger::Key {
        code,
        modifiers,
        phase,
    }) = parse_trigger(trigger)
    else {
        return false;
    };
    // A disabled or catch-all rule may be an intentional override.
    global_profile_rules(profile)
        .filter_map(|rule| parse_trigger(&rule.trigger))
        .any(|rule| matches_key_input(&rule, code, modifiers.required, phase))
}

fn mirror_control_rule(profile: &mut Profile, source: &str, suffix: &str, key: &str, action: &str) {
    let exact = format!("exact_chord:left_control{suffix}+{key}");
    let Some((priority, enabled)) = profile.keyboard_rules.iter().find_map(|rule| {
        ((rule.trigger == source || rule.trigger == exact) && rule.action == action)
            .then_some((rule.priority, rule.enabled))
    }) else {
        return;
    };
    for control in CONTROL_SIDES.into_iter().skip(1) {
        let trigger = format!("exact_chord:{control}{suffix}+{key}");
        if !has_matching_global_rule(profile, &trigger) {
            profile.keyboard_rules.push(Rule {
                priority,
                enabled,
                ..default_chord_rule(&trigger, action)
            });
        }
    }
}

fn matches_key_input(
    trigger: &Trigger,
    code: PhysicalCode,
    modifiers: ModifierSet,
    phase: EventPhase,
) -> bool {
    match trigger {
        Trigger::Key {
            code: rule_code,
            modifiers: matcher,
            phase: rule_phase,
        } => *rule_code == code && *rule_phase == phase && matcher.matches(modifiers),
        Trigger::Chord {
            key: rule_code,
            modifiers: required,
            phase: rule_phase,
        } => *rule_code == code && *rule_phase == phase && modifiers.contains(*required),
        _ => false,
    }
}

fn default_chord_rule(trigger: &str, action: &str) -> Rule {
    Rule {
        name: format!("{trigger} → {action}"),
        trigger: trigger.into(),
        action: action.into(),
        priority: 200,
        ..Rule::default()
    }
}

fn modifier_codes() -> [(PhysicalCode, ModifierSet); 8] {
    [
        (PhysicalCode::key(56), ModifierSet::LEFT_SHIFT),
        (PhysicalCode::key(60), ModifierSet::RIGHT_SHIFT),
        (PhysicalCode::key(59), ModifierSet::LEFT_CONTROL),
        (PhysicalCode::key(62), ModifierSet::RIGHT_CONTROL),
        (PhysicalCode::key(58), ModifierSet::LEFT_OPTION),
        (PhysicalCode::key(61), ModifierSet::RIGHT_OPTION),
        (PhysicalCode::key(55), ModifierSet::LEFT_COMMAND),
        (PhysicalCode::key(54), ModifierSet::RIGHT_COMMAND),
    ]
}

fn profile_rule_lists(profile: &Profile) -> [(&str, &[Rule]); 5] {
    [
        ("keyboard_rules", &profile.keyboard_rules),
        ("mouse_rules", &profile.mouse_rules),
        ("global_hotkeys", &profile.global_hotkeys),
        ("app_rules", &profile.app_rules),
        ("device_rules", &profile.device_rules),
    ]
}

fn compile_profile(profile_id: &str, profile: &Profile) -> Result<CompiledRules, ConfigError> {
    let mut rules = Vec::new();
    for (category, inputs) in [
        ("keyboard_rules", &profile.keyboard_rules),
        ("mouse_rules", &profile.mouse_rules),
        ("global_hotkeys", &profile.global_hotkeys),
    ] {
        compile_rules(&mut rules, profile_id, category, inputs, |_| {
            Ok(RuleScope::Global)
        })?;
    }
    compile_rules(
        &mut rules,
        profile_id,
        "app_rules",
        &profile.app_rules,
        |input| {
            let bundle_id = input
                .app_bundle_id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    ConfigError::InvalidRule(format!(
                        "profiles.{profile_id}.app_rules.app_bundle_id is required"
                    ))
                })?;
            Ok(RuleScope::App { bundle_id })
        },
    )?;
    if profile.device_rules.iter().any(|rule| rule.enabled) {
        return Err(ConfigError::UnsupportedScope("device_rules".into()));
    }
    Ok(CompiledRules::compile(rules)?)
}

fn compile_rules<F>(
    output: &mut Vec<EngineRule>,
    profile_id: &str,
    category: &str,
    inputs: &[Rule],
    scope: F,
) -> Result<(), ConfigError>
where
    F: Fn(&Rule) -> Result<RuleScope, ConfigError>,
{
    for (index, input) in inputs.iter().enumerate() {
        let rule_scope = scope(input)?;
        if matches!(rule_scope, RuleScope::Global) {
            validate_global_scope_fields(profile_id, category, index, input)?;
        }
        let trigger = parse_trigger(&input.trigger).ok_or_else(|| {
            ConfigError::InvalidRule(format!("profiles.{profile_id}.{category}[{index}].trigger"))
        })?;
        let action = parse_action(&input.action).ok_or_else(|| {
            ConfigError::InvalidRule(format!("profiles.{profile_id}.{category}[{index}].action"))
        })?;
        if let Some(reason) = unsupported_keyboard_rule(input, &trigger, &action) {
            return Err(ConfigError::InvalidRule(format!(
                "profiles.{profile_id}.{category}[{index}].{reason}"
            )));
        }
        if input.enabled && matches!(action, Action::InvertScroll) && !is_scroll_trigger(&trigger) {
            return Err(ConfigError::InvalidRule(format!(
                "profiles.{profile_id}.{category}[{index}].action: scroll reversal requires a scroll trigger"
            )));
        }
        if input.enabled
            && matches!(trigger, Trigger::ModifierChord { .. })
            && !matches!(&action, Action::Replace(outputs) if matches!(outputs.as_slice(), [OutputEvent::SystemShortcut(_)]))
        {
            return Err(ConfigError::InvalidRule(format!(
                "profiles.{profile_id}.{category}[{index}].action: a release-only modifier chord requires exactly one system shortcut"
            )));
        }
        output.push(EngineRule {
            id: input.id.clone(),
            enabled: input.enabled,
            priority: input.priority,
            scope: rule_scope,
            trigger,
            action,
        });
    }
    Ok(())
}

fn unsupported_keyboard_rule(
    rule: &Rule,
    trigger: &Trigger,
    action: &Action,
) -> Option<&'static str> {
    if !rule.enabled {
        return None;
    }
    match trigger {
        Trigger::Key {
            code: PhysicalCode::Key(57),
            ..
        }
        | Trigger::Chord {
            key: PhysicalCode::Key(57),
            ..
        } => {
            return Some("trigger: Caps Lock 保留给 macOS，不能作为映射触发键");
        }
        _ => {}
    }
    if (rule.action.starts_with("modified:") || rule.action.starts_with("command:"))
        && matches!(action, Action::Replace(outputs)
            if matches!(outputs.as_slice(), [OutputEvent::Key { code, .. }]
                if modifier_codes().iter().any(|(modifier, _)| code == modifier)))
    {
        return Some("action: 带修饰键输出的主键必须是普通键");
    }
    None
}

fn validate_global_scope_fields(
    profile_id: &str,
    category: &str,
    index: usize,
    rule: &Rule,
) -> Result<(), ConfigError> {
    for (field, present) in [
        ("app_bundle_id", rule.app_bundle_id.is_some()),
        ("device_identifier", rule.device_identifier.is_some()),
    ] {
        if rule.enabled && present {
            return Err(ConfigError::InvalidRule(format!(
                "profiles.{profile_id}.{category}[{index}].{field}: an enabled global rule cannot be restricted to an application or device"
            )));
        }
    }
    Ok(())
}

fn parse_trigger(value: &str) -> Option<Trigger> {
    if let Some(modifier_chord) = value.strip_prefix("modifier_chord:") {
        return Some(Trigger::ModifierChord {
            modifiers: parse_modifiers(modifier_chord)?,
            phase: EventPhase::Up,
        });
    }
    if let Some(chord) = value.strip_prefix("exact_chord:") {
        let (modifiers, key) = chord.split_once('+')?;
        return Some(Trigger::Key {
            code: parse_code(key)?,
            modifiers: ModifierMatcher {
                required: parse_modifiers(modifiers)?,
                exact: true,
            },
            phase: EventPhase::Down,
        });
    }
    if let Some(chord) = value.strip_prefix("chord:") {
        let (modifiers, key) = chord.split_once('+')?;
        return Some(Trigger::Chord {
            modifiers: parse_modifiers(modifiers)?,
            key: parse_code(key)?,
            phase: EventPhase::Down,
        });
    }
    if let Some(button) = value.strip_prefix("mouse:") {
        return Some(Trigger::MouseButton {
            button: parse_mouse_button(button)?,
            phase: EventPhase::Down,
        });
    }
    if let Some(axis) = value.strip_prefix("scroll:") {
        return Some(Trigger::Scroll {
            axis: match axis {
                "horizontal" => ScrollAxis::Horizontal,
                "vertical" => ScrollAxis::Vertical,
                _ => return None,
            },
        });
    }
    Some(Trigger::Key {
        code: parse_code(value)?,
        modifiers: ModifierMatcher::default(),
        phase: EventPhase::Down,
    })
}

fn is_scroll_trigger(trigger: &Trigger) -> bool {
    matches!(trigger, Trigger::Scroll { .. })
}

fn parse_action(value: &str) -> Option<Action> {
    if value == "pass" {
        return Some(Action::Pass);
    }
    if value == "suppress" {
        return Some(Action::Suppress);
    }
    if value == "invert_scroll" {
        return Some(Action::InvertScroll);
    }
    if let Some(chord) = value.strip_prefix("modified:") {
        let (modifiers, key) = chord.split_once('+')?;
        return Some(Action::Replace(vec![OutputEvent::Key {
            code: parse_code(key)?,
            phase: EventPhase::Down,
            modifiers: parse_modifiers(modifiers)?,
        }]));
    }
    if let Some(key) = value.strip_prefix("command:") {
        return Some(Action::Replace(vec![OutputEvent::Key {
            code: parse_code(key)?,
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_COMMAND,
        }]));
    }
    if let Some(shortcut) = parse_system_shortcut(value) {
        return Some(Action::Replace(vec![OutputEvent::SystemShortcut(shortcut)]));
    }
    if let Some(button) = value.strip_prefix("mouse:") {
        return Some(Action::Replace(vec![OutputEvent::MouseButton {
            button: parse_mouse_button(button)?,
            phase: EventPhase::Down,
        }]));
    }
    Some(Action::Replace(vec![OutputEvent::Key {
        code: match value {
            "left_windows" => PhysicalCode::left_command(),
            "right_windows" => PhysicalCode::right_command(),
            "left_alt" => PhysicalCode::left_option(),
            "right_alt" => PhysicalCode::right_option(),
            _ => parse_code(value)?,
        },
        phase: EventPhase::Down,
        modifiers: match value {
            "left_windows" => ModifierSet::LEFT_WINDOWS,
            "right_windows" => ModifierSet::RIGHT_WINDOWS,
            "left_alt" => ModifierSet::LEFT_ALT,
            "right_alt" => ModifierSet::RIGHT_ALT,
            "left_option" => ModifierSet::LEFT_OPTION,
            "right_option" => ModifierSet::RIGHT_OPTION,
            "left_command" => ModifierSet::LEFT_COMMAND,
            "right_command" => ModifierSet::RIGHT_COMMAND,
            "left_control" => ModifierSet::LEFT_CONTROL,
            "right_control" => ModifierSet::RIGHT_CONTROL,
            "left_shift" => ModifierSet::LEFT_SHIFT,
            "right_shift" => ModifierSet::RIGHT_SHIFT,
            _ => ModifierSet::NONE,
        },
    }]))
}

fn parse_modifiers(value: &str) -> Option<ModifierSet> {
    let mut modifiers = ModifierSet::NONE;
    for name in value.split('|') {
        modifiers.insert(match name {
            "left_control" => ModifierSet::LEFT_CONTROL,
            "right_control" => ModifierSet::RIGHT_CONTROL,
            "left_option" => ModifierSet::LEFT_OPTION,
            "right_option" => ModifierSet::RIGHT_OPTION,
            "left_command" => ModifierSet::LEFT_COMMAND,
            "right_command" => ModifierSet::RIGHT_COMMAND,
            "left_shift" => ModifierSet::LEFT_SHIFT,
            "right_shift" => ModifierSet::RIGHT_SHIFT,
            _ => return None,
        });
    }
    Some(modifiers)
}

fn parse_mouse_button(value: &str) -> Option<MouseButton> {
    Some(match value {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        value => {
            let button = value.strip_prefix("other:")?.parse::<u8>().ok()?;
            match button {
                0 => MouseButton::Left,
                1 => MouseButton::Right,
                2 => MouseButton::Middle,
                3..=31 => MouseButton::Other(button),
                _ => return None,
            }
        }
    })
}

fn parse_system_shortcut(value: &str) -> Option<SystemShortcut> {
    Some(match value.strip_prefix("shortcut:").unwrap_or(value) {
        "lock_screen" => SystemShortcut::LockScreen,
        "switch_application" => SystemShortcut::SwitchApplication,
        "show_desktop" => SystemShortcut::ShowDesktop,
        "close_window" => SystemShortcut::CloseWindow,
        "hard_refresh" => SystemShortcut::HardRefresh,
        "switch_input_source" => SystemShortcut::SwitchInputSource,
        "toggle_pinyin_english" => SystemShortcut::TogglePinyinEnglish,
        "show_clipboard" => SystemShortcut::ShowClipboard,
        _ => return None,
    })
}

fn parse_code(value: &str) -> Option<PhysicalCode> {
    match value {
        "left_shift" => Some(PhysicalCode::key(56)),
        "right_shift" => Some(PhysicalCode::key(60)),
        "left_control" => Some(PhysicalCode::left_control()),
        "left_option" | "left_windows" => Some(PhysicalCode::left_option()),
        "left_command" | "left_alt" => Some(PhysicalCode::left_command()),
        "right_control" => Some(PhysicalCode::right_control()),
        "right_option" | "right_windows" => Some(PhysicalCode::right_option()),
        "right_command" | "right_alt" => Some(PhysicalCode::right_command()),
        "escape" => Some(PhysicalCode::key(53)),
        raw => {
            let code = raw.strip_prefix("key:")?.parse::<u16>().ok()?;
            (code <= 127).then_some(PhysicalCode::key(code))
        }
    }
}

pub fn default_config() -> AppConfig {
    let mut profile = Profile {
        keyboard_rules: vec![
            Rule {
                trigger: "left_option".into(),
                action: "left_windows".into(),
                ..Rule::default()
            },
            Rule {
                trigger: "left_command".into(),
                action: "left_alt".into(),
                ..Rule::default()
            },
            Rule {
                trigger: "right_option".into(),
                action: "right_windows".into(),
                ..Rule::default()
            },
            Rule {
                trigger: "right_command".into(),
                action: "right_alt".into(),
                ..Rule::default()
            },
            Rule {
                trigger: "chord:left_control+key:0".into(),
                action: "command:key:0".into(),
                priority: 200,
                ..Rule::default()
            },
            Rule {
                trigger: "chord:left_control+key:8".into(),
                action: "command:key:8".into(),
                priority: 200,
                ..Rule::default()
            },
            Rule {
                trigger: "chord:left_control+key:9".into(),
                action: "command:key:9".into(),
                priority: 200,
                ..Rule::default()
            },
            Rule {
                trigger: "chord:left_control+key:7".into(),
                action: "command:key:7".into(),
                priority: 200,
                ..Rule::default()
            },
            Rule {
                trigger: "chord:left_control+key:6".into(),
                action: "command:key:6".into(),
                priority: 200,
                ..Rule::default()
            },
            Rule {
                trigger: "chord:left_control+key:96".into(),
                action: "shortcut:hard_refresh".into(),
                priority: 300,
                ..Rule::default()
            },
            Rule {
                // Windows Alt+Tab has no direct macOS Alt equivalent. Use the
                // native application switcher while preserving the physical
                // Command+Tab gesture users perform on a Mac keyboard.
                trigger: "chord:left_command+key:48".into(),
                action: "shortcut:switch_application".into(),
                priority: 300,
                ..Rule::default()
            },
            Rule {
                trigger: "modifier_chord:left_control|left_shift".into(),
                action: "shortcut:switch_input_source".into(),
                priority: 300,
                ..Rule::default()
            },
            Rule {
                trigger: "modifier_chord:left_control|right_shift".into(),
                action: "shortcut:switch_input_source".into(),
                priority: 300,
                ..Rule::default()
            },
            Rule {
                trigger: "modifier_chord:right_control|left_shift".into(),
                action: "shortcut:switch_input_source".into(),
                priority: 300,
                ..Rule::default()
            },
            Rule {
                trigger: "modifier_chord:right_control|right_shift".into(),
                action: "shortcut:switch_input_source".into(),
                priority: 300,
                ..Rule::default()
            },
        ],
        ..Profile::default()
    };
    for rule in &mut profile.keyboard_rules {
        rule.name = format!("{} → {}", rule.trigger, rule.action);
    }
    for (triggers, action) in [
        (DEFAULT_SAVE_TRIGGERS, DEFAULT_SAVE_ACTION),
        (DEFAULT_REDO_TRIGGERS, DEFAULT_REDO_ACTION),
    ] {
        profile.keyboard_rules.extend(
            triggers
                .into_iter()
                .map(|trigger| default_chord_rule(trigger, action)),
        );
    }
    let mut profiles = BTreeMap::new();
    profiles.insert(BUILT_IN_PROFILE_IDS[0].into(), profile.clone());
    for id in BUILT_IN_PROFILE_IDS.into_iter().skip(1) {
        let mut independent = profile.clone();
        for rule in &mut independent.keyboard_rules {
            rule.id = uuid::Uuid::new_v4().to_string();
        }
        profiles.insert(id.into(), independent);
    }
    let mut config = AppConfig {
        schema_version: CURRENT_SCHEMA_VERSION,
        active_profile: "windows_physical".into(),
        enabled: false,
        software_input_passthrough: true,
        default_save_shortcuts_initialized: true,
        default_redo_shortcuts_initialized: true,
        default_edit_control_sides_initialized: false,
        panic_hotkey: default_panic_hotkey(),
        mouse: MouseConfig::default(),
        system_shortcuts: SystemShortcutsConfig::default(),
        keyboard_rules: Vec::new(),
        mouse_rules: Vec::new(),
        global_hotkeys: Vec::new(),
        app_rules: Vec::new(),
        device_rules: Vec::new(),
        profiles,
    };
    config.upgrade_default_edit_shortcuts();
    config.upgrade_default_edit_control_sides();
    config
}

pub fn import_toml(input: &str) -> Result<AppConfig, ConfigError> {
    let mut config = parse_toml(input)?;
    config.validate()?;
    config.mouse.migrate_scroll_mode();
    Ok(config)
}

/// Preserve unsupported entries from older saved settings for editing.
pub fn import_saved_toml(input: &str) -> Result<(AppConfig, bool, Vec<String>), ConfigError> {
    let mut config = parse_toml(input)?;
    let invalid_scroll_mode = !config.mouse.valid_mode();
    let upgraded_scroll = config.mouse.migrate_scroll_mode();
    let mut disabled = Vec::new();
    let mut unsupported = Vec::new();
    for (profile_id, profile) in &mut config.profiles {
        for (category, rules) in [
            ("keyboard_rules", &mut profile.keyboard_rules),
            ("mouse_rules", &mut profile.mouse_rules),
            ("global_hotkeys", &mut profile.global_hotkeys),
            ("app_rules", &mut profile.app_rules),
            ("device_rules", &mut profile.device_rules),
        ] {
            for (index, rule) in rules.iter_mut().enumerate() {
                if matches!(
                    category,
                    "keyboard_rules" | "mouse_rules" | "global_hotkeys"
                ) {
                    validate_global_scope_fields(profile_id, category, index, rule)?;
                }
                if rule.enabled
                    && rule.action == "invert_scroll"
                    && parse_trigger(&rule.trigger)
                        .is_some_and(|trigger| !is_scroll_trigger(&trigger))
                {
                    rule.enabled = false;
                    disabled.push(format!("profiles.{profile_id}.{category}[{index}]"));
                }
                if let Some((trigger, action)) =
                    parse_trigger(&rule.trigger).zip(parse_action(&rule.action))
                {
                    if let Some(reason) = unsupported_keyboard_rule(rule, &trigger, &action) {
                        rule.enabled = false;
                        unsupported.push(format!(
                            "profiles.{profile_id}.{category}[{index}].{reason}"
                        ));
                    }
                }
            }
        }
    }
    config.validate()?;
    let mut warnings = Vec::new();
    if !disabled.is_empty() {
        warnings.push(format!(
            "已停用触发键无效的滚轮反转规则（{}）",
            disabled.join("、")
        ));
    }
    if invalid_scroll_mode {
        warnings.push("mouse.scroll_mode 无效，滚动方向已恢复为跟随系统".into());
    }
    let upgraded_unsupported = !unsupported.is_empty();
    if upgraded_unsupported {
        warnings.push(format!(
            "已停用系统不支持的键盘规则（{}）",
            unsupported.join("、")
        ));
    }
    Ok((config, upgraded_scroll || upgraded_unsupported, warnings))
}

fn parse_toml(input: &str) -> Result<AppConfig, ConfigError> {
    let mut config: AppConfig = toml::from_str(input)?;
    if config.schema_version == 1 {
        migrate_v1(&mut config);
    }
    Ok(config)
}

fn migrate_v1(config: &mut AppConfig) {
    let legacy = Profile {
        keyboard_rules: std::mem::take(&mut config.keyboard_rules),
        mouse_rules: std::mem::take(&mut config.mouse_rules),
        global_hotkeys: std::mem::take(&mut config.global_hotkeys),
        app_rules: std::mem::take(&mut config.app_rules),
        device_rules: std::mem::take(&mut config.device_rules),
    };
    let mut ids = HashSet::new();
    for profile in config.profiles.values_mut() {
        if profile_rule_lists(profile)
            .iter()
            .all(|(_, rules)| rules.is_empty())
        {
            *profile = legacy.clone();
        }
        // Materialize the old engine's hidden conversion as editable rules.
        // An explicit rule, including a disabled one, always wins over the baseline.
        for (trigger, action) in LEGACY_STOCK_MODIFIERS {
            let source = parse_code(trigger).unwrap();
            let explicit = [
                &profile.keyboard_rules,
                &profile.mouse_rules,
                &profile.global_hotkeys,
            ]
            .into_iter()
            .flatten()
            .any(|rule| {
                matches!(parse_trigger(&rule.trigger),
                    Some(Trigger::Key {code, modifiers, phase: EventPhase::Down})
                        if code == source && modifiers.required == ModifierSet::NONE)
            });
            if !explicit {
                profile.keyboard_rules.push(Rule {
                    trigger: trigger.into(),
                    action: action.into(),
                    priority: i32::MIN,
                    name: format!("{trigger} → {action}"),
                    ..Rule::default()
                });
            }
        }
        for rules in [
            &mut profile.keyboard_rules,
            &mut profile.mouse_rules,
            &mut profile.global_hotkeys,
            &mut profile.app_rules,
            &mut profile.device_rules,
        ] {
            for rule in rules {
                let valid_id = uuid::Uuid::parse_str(&rule.id)
                    .ok()
                    .filter(|id| id.get_version_num() == 4);
                if valid_id.is_none() || !ids.insert(valid_id.unwrap()) {
                    rule.id = uuid::Uuid::new_v4().to_string();
                    ids.insert(uuid::Uuid::parse_str(&rule.id).unwrap());
                }
                if rule.name.trim().is_empty() {
                    rule.name = format!("{} → {}", rule.trigger, rule.action);
                }
            }
        }
    }
    config.schema_version = CURRENT_SCHEMA_VERSION;
}

pub fn export_toml(config: &AppConfig) -> Result<String, ConfigError> {
    config.compile_active_profile()?;
    Ok(toml::to_string_pretty(config)?)
}

pub fn load(path: impl AsRef<Path>) -> Result<AppConfig, ConfigError> {
    import_toml(&fs::read_to_string(path)?)
}

pub fn save_atomic(path: impl AsRef<Path>, config: &AppConfig) -> Result<SaveOutcome, ConfigError> {
    save_with_directory_sync(path.as_ref(), config, File::sync_all)
}

fn save_with_directory_sync(
    path: &Path,
    config: &AppConfig,
    sync_directory: impl FnOnce(&File) -> io::Result<()>,
) -> Result<SaveOutcome, ConfigError> {
    let data = export_toml(config)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let directory = File::open(parent)?;
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), unique_suffix()));
    let backup_tmp = path.with_extension(format!(
        "bak.tmp.{}.{}",
        std::process::id(),
        unique_suffix()
    ));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(data.as_bytes())?;
        file.sync_all()?;
        let backup = path.with_extension("toml.bak");
        if path.try_exists()? {
            fs::copy(path, &backup_tmp)?;
            File::open(&backup_tmp)?.sync_all()?;
            fs::rename(&backup_tmp, &backup)?;
        }
        fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(&backup_tmp);
        return Err(error.into());
    }
    Ok(match sync_directory(&directory) {
        Ok(()) => SaveOutcome::Saved,
        Err(error) => SaveOutcome::DirectorySyncFailed(error),
    })
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_has_windows_profile() {
        let config = default_config();
        assert_eq!(config.schema_version, 2);
        assert_eq!(config.active_profile, "windows_physical");
        assert!(config.profiles.contains_key("windows_physical"));
        config.validate().unwrap();
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = default_config();
        let text = export_toml(&config).unwrap();
        let decoded = import_toml(&text).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn imports_reject_enabled_global_rules_with_scope_fields() {
        for (category, field) in [
            ("keyboard_rules", "app_bundle_id"),
            ("mouse_rules", "device_identifier"),
            ("global_hotkeys", "app_bundle_id"),
        ] {
            let input = format!(
                "schema_version = 2\nactive_profile = 'custom'\n\
                 [[profiles.custom.{category}]]\n\
                 id = '00000000-0000-4000-8000-000000000001'\nname = 'scoped'\n\
                 trigger = 'key:11'\naction = 'key:45'\n{field} = 'example'\n"
            );
            let location = format!("profiles.custom.{category}[0].{field}");
            assert!(import_toml(&input)
                .unwrap_err()
                .to_string()
                .contains(&location));
            assert!(import_saved_toml(&input)
                .unwrap_err()
                .to_string()
                .contains(&location));
            let disabled = format!("{input}enabled = false\n");
            assert!(import_toml(&disabled).is_ok());
            assert!(import_saved_toml(&disabled).is_ok());
        }
    }

    #[test]
    fn directory_sync_failure_identifies_already_replaced_primary() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let baseline = AppConfig::default();
        let mut updated = baseline.clone();
        updated.active_profile = "custom".into();
        save_atomic(&path, &baseline).unwrap();
        let outcome = save_with_directory_sync(&path, &updated, |_| {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "synthetic directory sync failure",
            ))
        })
        .unwrap();
        assert!(matches!(outcome, SaveOutcome::DirectorySyncFailed(_)));
        assert_eq!(load(&path).unwrap(), updated);
        assert_eq!(load(path.with_extension("toml.bak")).unwrap(), baseline);
    }

    #[test]
    fn software_passthrough_defaults_on_for_existing_configs() {
        let config = default_config();
        assert!(config.software_input_passthrough);
        let text = export_toml(&config)
            .unwrap()
            .replace("software_input_passthrough = true\n", "");
        let decoded = import_toml(&text).unwrap();
        assert!(decoded.software_input_passthrough);
        assert!(!decoded.enabled);
        let legacy =
            import_toml("enabled = false\nactive_profile = \"custom\"\n[profiles.custom]\n")
                .unwrap();
        assert!(legacy.software_input_passthrough);
    }

    #[test]
    fn disabled_software_passthrough_survives_export_and_profile_change() {
        let mut config = default_config();
        config.software_input_passthrough = false;
        config.active_profile = "custom".into();
        let decoded = import_toml(&export_toml(&config).unwrap()).unwrap();
        assert!(!decoded.software_input_passthrough);
        assert_eq!(decoded.active_profile, "custom");
        assert_eq!(decoded, config);
    }

    #[test]
    fn validation_rejects_unknown_active_profile() {
        let mut config = default_config();
        config.active_profile = "missing".into();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnknownProfile(_))
        ));
    }

    #[test]
    fn atomic_save_keeps_backup_of_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let first = default_config();
        save_atomic(&path, &first).unwrap();
        let mut second = first.clone();
        second.active_profile = "custom".into();
        second.profiles.insert("custom".into(), Profile::default());
        save_atomic(&path, &second).unwrap();
        assert_eq!(load(&path).unwrap(), second);
        assert_eq!(load(path.with_extension("toml.bak")).unwrap(), first);
    }

    #[test]
    fn default_config_includes_runtime_safety_settings() {
        let config = default_config();
        assert!(!config.enabled);
        assert_eq!(
            config.panic_hotkey,
            vec!["left_control", "left_option", "left_command", "escape"]
        );
        assert_eq!(config.mouse.scroll_mode, "system");
    }

    #[test]
    fn active_profile_compiles_to_rule_snapshot() {
        let config = default_config();
        let compiled = config.compile_active_profile().unwrap();
        assert_eq!(compiled.rules().len(), 49);
    }

    #[test]
    fn stock_find_and_edit_shortcuts_support_each_control_side() {
        use event_model::{Decision, DeviceSource, EventKind, EventOrigin, NormalizedEvent};
        use rule_engine::RuleEngine;

        let config = default_config();
        for id in BUILT_IN_PROFILE_IDS {
            let rules = compile_profile(id, &config.profiles[id]).unwrap();
            for control in [
                ModifierSet::LEFT_CONTROL,
                ModifierSet::RIGHT_CONTROL,
                ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_CONTROL,
            ] {
                for (code, shift) in [
                    (0, ModifierSet::NONE),
                    (3, ModifierSet::NONE),
                    (7, ModifierSet::NONE),
                    (8, ModifierSet::NONE),
                    (6, ModifierSet::NONE),
                    (9, ModifierSet::NONE),
                    (6, ModifierSet::LEFT_SHIFT),
                    (9, ModifierSet::RIGHT_SHIFT),
                    (6, ModifierSet::LEFT_SHIFT | ModifierSet::RIGHT_SHIFT),
                ] {
                    let mut engine = RuleEngine::new(rules.clone());
                    let code = PhysicalCode::key(code);
                    assert_eq!(
                        engine.handle(&NormalizedEvent {
                            source: DeviceSource::Keyboard,
                            kind: EventKind::Keyboard,
                            phase: EventPhase::Down,
                            code,
                            modifiers: control | shift,
                            location: None,
                            scroll: None,
                            timestamp_ns: 0,
                            app_context: None,
                            origin: EventOrigin::Hardware,
                        }),
                        Decision::Emit(vec![OutputEvent::Key {
                            code,
                            phase: EventPhase::Down,
                            modifiers: ModifierSet::LEFT_COMMAND | shift,
                        }]),
                        "{id} {control:?} {code:?} {shift:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn control_side_migration_extends_stock_rules_and_keeps_existing_overrides() {
        let mut config = default_config();
        config.default_edit_control_sides_initialized = false;
        let profile = config.profiles.get_mut("windows_physical").unwrap();
        profile.keyboard_rules.retain(|rule| {
            !rule.trigger.starts_with("exact_chord:right_control")
                && !rule
                    .trigger
                    .starts_with("exact_chord:left_control|right_control")
                && !rule.trigger.ends_with("+key:3")
        });
        let disabled = profile
            .keyboard_rules
            .iter_mut()
            .find(|rule| rule.trigger == "chord:left_control+key:8")
            .unwrap();
        disabled.priority = 140;
        disabled.enabled = false;
        let custom = Rule {
            name: "Custom right undo".into(),
            trigger: "chord:right_control+key:6".into(),
            action: "pass".into(),
            ..Rule::default()
        };
        profile.global_hotkeys.push(custom.clone());
        let original = profile.keyboard_rules.clone();
        assert!(config.upgrade_default_edit_control_sides());
        assert!(!config.upgrade_default_edit_control_sides());
        let profile = &config.profiles["windows_physical"];
        assert_eq!(&profile.keyboard_rules[..original.len()], &original);
        assert!(profile.global_hotkeys.contains(&custom));
        assert!(!profile
            .keyboard_rules
            .iter()
            .any(|rule| rule.trigger == "exact_chord:right_control+key:6"
                || rule.trigger == "exact_chord:left_control|right_control+key:6"));
        assert!(profile
            .keyboard_rules
            .iter()
            .any(|rule| rule.trigger == "exact_chord:right_control+key:8"
                && !rule.enabled
                && rule.priority == 140));
        assert!(profile
            .keyboard_rules
            .iter()
            .any(|rule| rule.trigger == "exact_chord:right_control+key:3" && rule.enabled));
        config.validate().unwrap();
    }

    #[test]
    fn unsupported_keyboard_rules_are_rejected_on_import_and_preserved_disabled_on_upgrade() {
        for (trigger, action, expected) in [
            (
                "key:96",
                "modified:left_command+left_shift",
                "主键必须是普通键",
            ),
            ("key:96", "command:key:56", "主键必须是普通键"),
            ("key:057", "key:11", "Caps Lock"),
            ("exact_chord:left_control+key:57", "key:11", "Caps Lock"),
        ] {
            let mut config = default_config();
            let rule = Rule {
                trigger: trigger.into(),
                action: action.into(),
                ..Rule::default()
            };
            config
                .profiles
                .get_mut("windows_physical")
                .unwrap()
                .keyboard_rules
                .push(rule.clone());
            let contents = toml::to_string_pretty(&config).unwrap();
            assert!(import_toml(&contents)
                .unwrap_err()
                .to_string()
                .contains(expected));
            let (mut recovered, changed, warnings) = import_saved_toml(&contents).unwrap();
            assert!(changed && warnings.iter().any(|warning| warning.contains(expected)));
            let upgraded = recovered
                .profiles
                .get_mut("windows_physical")
                .unwrap()
                .keyboard_rules
                .last_mut()
                .unwrap();
            assert_eq!(
                *upgraded,
                Rule {
                    enabled: false,
                    ..rule
                }
            );
            upgraded.enabled = true;
            assert!(recovered
                .validate_desktop()
                .unwrap_err()
                .to_string()
                .contains(expected));
        }
    }

    #[test]
    fn edit_upgrade_keeps_stock_identity_and_respects_shift_override() {
        let mut config = default_config();
        let profile = config.profiles.get_mut("custom").unwrap();
        profile
            .keyboard_rules
            .retain(|rule| !rule.action.starts_with("modified:"));
        let old = profile
            .keyboard_rules
            .iter_mut()
            .find(|rule| rule.trigger == "exact_chord:left_control+key:6")
            .unwrap();
        old.trigger = "chord:left_control+key:6".into();
        old.name = "chord:left_control+key:6 → command:key:6".into();
        old.priority = 198;
        let id = old.id.clone();
        let override_rule = Rule {
            trigger: "chord:left_control|right_shift+key:6".into(),
            action: "suppress".into(),
            ..Rule::default()
        };
        profile.keyboard_rules.push(override_rule.clone());
        assert!(config.upgrade_default_edit_shortcuts());
        assert!(!config.upgrade_default_edit_shortcuts());
        config.validate().unwrap();
        let rules = &config.profiles["custom"].keyboard_rules;
        assert!(rules.contains(&override_rule));
        let migrated = rules.iter().find(|rule| rule.id == id).unwrap();
        assert_eq!(migrated.trigger, "exact_chord:left_control+key:6");
        assert_eq!(migrated.priority, 198);
        assert!(rules
            .iter()
            .any(|rule| rule.action == "modified:left_command|left_shift+key:6"));
        assert!(!rules
            .iter()
            .any(|rule| rule.action == "modified:left_command|right_shift+key:6"));
    }

    #[test]
    fn old_scroll_configuration_follows_system_and_explicit_modes_round_trip() {
        let mut config = default_config();
        config.mouse.scroll_mode = "non_natural".into();
        let old = export_toml(&config)
            .unwrap()
            .replace("scroll_mode_initialized = true\n", "");
        let (upgraded, changed, warnings) = import_saved_toml(&old).unwrap();
        assert!(changed && warnings.is_empty());
        assert_eq!(upgraded.mouse.desired_scroll_inversion(), None);
        assert_eq!(upgraded.profiles, config.profiles);
        assert_eq!(import_toml(&old).unwrap(), upgraded);
        for (mode, inversion) in [("natural", true), ("non_natural", false)] {
            config.mouse.scroll_mode = mode.into();
            let saved = export_toml(&config).unwrap();
            assert_eq!(
                import_toml(&saved)
                    .unwrap()
                    .mouse
                    .desired_scroll_inversion(),
                Some(inversion)
            );
            let invalid = saved.replace(
                &format!("scroll_mode = \"{mode}\""),
                "scroll_mode = \"invalid\"",
            );
            assert!(import_toml(&invalid)
                .unwrap_err()
                .to_string()
                .contains("mouse.scroll_mode"));
            let (recovered, changed, warnings) = import_saved_toml(&invalid).unwrap();
            assert!(changed && !warnings.is_empty());
            assert_eq!(recovered.mouse.desired_scroll_inversion(), None);
            assert_eq!(recovered.profiles, config.profiles);
        }
    }

    #[test]
    fn default_save_shortcuts_require_control_without_additional_modifiers() {
        use event_model::{Decision, DeviceSource, EventKind, EventOrigin, NormalizedEvent};
        use rule_engine::{KeyboardMappingPolicy, RuleEngine};

        let config = default_config();
        assert!(config.default_save_shortcuts_initialized);
        for id in BUILT_IN_PROFILE_IDS {
            let rules = compile_profile(id, &config.profiles[id]).unwrap();
            let save_rules: Vec<_> = rules
                .rules()
                .iter()
                .filter(|rule| {
                    matches!(rule.trigger, Trigger::Key { code: PhysicalCode::Key(1), modifiers, .. }
                        if modifiers.exact)
                })
                .collect();
            assert_eq!(save_rules.len(), 3);
            assert!(save_rules.iter().all(|rule| rule.priority == 200));
            for policy in [
                KeyboardMappingPolicy::Full,
                KeyboardMappingPolicy::ShortcutsOnly,
            ] {
                for (modifiers, saves) in [
                    (ModifierSet::LEFT_CONTROL, true),
                    (ModifierSet::RIGHT_CONTROL, true),
                    (ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_CONTROL, true),
                    (ModifierSet::NONE, false),
                    (ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_SHIFT, false),
                    (
                        ModifierSet::RIGHT_CONTROL | ModifierSet::RIGHT_OPTION,
                        false,
                    ),
                    (ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_COMMAND, false),
                ] {
                    let mut engine = RuleEngine::new(rules.clone());
                    let decision = engine.handle_with_policy(
                        &NormalizedEvent {
                            source: DeviceSource::Keyboard,
                            kind: EventKind::Keyboard,
                            phase: EventPhase::Down,
                            code: PhysicalCode::key(1),
                            modifiers,
                            location: None,
                            scroll: None,
                            timestamp_ns: 0,
                            app_context: None,
                            origin: EventOrigin::Hardware,
                        },
                        policy,
                    );
                    let expected = if saves {
                        Decision::Emit(vec![OutputEvent::Key {
                            code: PhysicalCode::key(1),
                            phase: EventPhase::Down,
                            modifiers: ModifierSet::LEFT_COMMAND,
                        }])
                    } else {
                        Decision::Pass
                    };
                    assert_eq!(decision, expected, "{id} {policy:?} {modifiers:?}");
                }
            }
        }
    }

    #[test]
    fn exact_chord_is_opt_in_and_ordinary_chords_still_allow_additional_modifiers() {
        let inclusive = parse_trigger("chord:left_control+key:1").unwrap();
        let exact = parse_trigger("exact_chord:left_control+key:1").unwrap();
        let key = PhysicalCode::key(1);
        let control = ModifierSet::LEFT_CONTROL;
        let control_shift = control | ModifierSet::LEFT_SHIFT;
        assert!(matches_key_input(
            &inclusive,
            key,
            control,
            EventPhase::Down
        ));
        assert!(matches_key_input(
            &inclusive,
            key,
            control_shift,
            EventPhase::Down
        ));
        assert!(matches_key_input(&exact, key, control, EventPhase::Down));
        assert!(!matches_key_input(
            &exact,
            key,
            control_shift,
            EventPhase::Down
        ));
        assert!(parse_trigger("exact_chord:+key:1").is_none());
    }

    fn legacy_save_config() -> AppConfig {
        let mut config = default_config();
        config.default_save_shortcuts_initialized = false;
        for profile in config.profiles.values_mut() {
            profile
                .keyboard_rules
                .retain(|rule| !DEFAULT_SAVE_TRIGGERS.contains(&rule.trigger.as_str()));
        }
        config
    }

    #[test]
    fn save_upgrade_keeps_global_overrides_and_touches_only_recognized_presets() {
        let mut config = legacy_save_config();
        let custom = config.profiles.get_mut("custom").unwrap();
        custom.keyboard_rules[0].priority = i32::MIN;
        custom
            .keyboard_rules
            .iter_mut()
            .find(|rule| rule.trigger == "chord:left_control+key:8")
            .unwrap()
            .enabled = false;
        custom.keyboard_rules.push(Rule {
            trigger: "chord:left_control|left_shift+key:1".into(),
            action: "suppress".into(),
            priority: 900,
            ..Rule::default()
        });
        config
            .profiles
            .get_mut("windows_physical")
            .unwrap()
            .global_hotkeys
            .push(Rule {
                trigger: "chord:left_control+key:01".into(),
                action: "suppress".into(),
                enabled: false,
                priority: -10,
                ..Rule::default()
            });
        config
            .profiles
            .get_mut("shortcut")
            .unwrap()
            .mouse_rules
            .push(Rule {
                trigger: "key:01".into(),
                action: "pass".into(),
                priority: -20,
                ..Rule::default()
            });
        config.profiles.insert(
            "imported-profile".into(),
            legacy_save_config().profiles.remove("custom").unwrap(),
        );
        let previous = config.clone();
        assert!(config.upgrade_default_save_shortcuts());
        config.validate_desktop().unwrap();
        assert_eq!(config.schema_version, previous.schema_version);
        assert_eq!(config.profiles["shortcut"], previous.profiles["shortcut"]);
        assert_eq!(
            config.profiles["imported-profile"],
            previous.profiles["imported-profile"]
        );
        for (id, expected_triggers) in [
            ("windows_physical", &DEFAULT_SAVE_TRIGGERS[1..2]),
            ("custom", &DEFAULT_SAVE_TRIGGERS[..]),
        ] {
            let old = &previous.profiles[id];
            let updated = &config.profiles[id];
            let (unchanged, inserted) = updated.keyboard_rules.split_at(old.keyboard_rules.len());
            assert_eq!(unchanged, old.keyboard_rules);
            assert_eq!(updated.global_hotkeys, old.global_hotkeys);
            assert_eq!(
                inserted
                    .iter()
                    .map(|rule| rule.trigger.as_str())
                    .collect::<Vec<_>>(),
                expected_triggers
            );
        }
        let saved = export_toml(&config).unwrap();
        assert!(saved.contains("default_save_shortcuts_initialized = true"));
        assert!(!config.upgrade_default_save_shortcuts());

        let mut reloaded = import_toml(&saved).unwrap();
        assert_eq!(reloaded, config);
        reloaded
            .profiles
            .get_mut("custom")
            .unwrap()
            .keyboard_rules
            .pop();
        assert!(!reloaded.upgrade_default_save_shortcuts());
        assert_eq!(
            reloaded.profiles["custom"].keyboard_rules.len(),
            config.profiles["custom"].keyboard_rules.len() - 1
        );
    }

    #[test]
    fn save_upgrade_preserves_empty_and_custom_profiles_without_stock_bindings() {
        let mut config = legacy_save_config();
        config
            .profiles
            .insert("windows_physical".into(), Profile::default());
        config.profiles.insert(
            "custom".into(),
            Profile {
                keyboard_rules: vec![Rule {
                    trigger: "key:11".into(),
                    action: "key:45".into(),
                    ..Rule::default()
                }],
                ..Profile::default()
            },
        );
        for rule in &mut config.profiles.get_mut("shortcut").unwrap().keyboard_rules {
            rule.enabled = false;
        }
        let previous = config.profiles.clone();
        assert!(config.upgrade_default_save_shortcuts());
        assert_eq!(config.profiles, previous);
        assert!(!config.upgrade_default_save_shortcuts());
    }

    #[test]
    fn config_serialization_rejects_unknown_keycodes() {
        let mut config = default_config();
        config
            .profiles
            .get_mut("windows_physical")
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: "key:999".into(),
                action: "pass".into(),
                ..Rule::default()
            });
        assert!(matches!(
            export_toml(&config),
            Err(ConfigError::InvalidRule(_))
        ));
    }

    #[test]
    fn default_profile_ids_match_the_ui() {
        let config = default_config();
        assert_eq!(config.active_profile, "windows_physical");
        for id in ["windows_physical", "shortcut", "custom"] {
            assert!(config.profiles.contains_key(id), "missing profile {id}");
        }
    }

    #[test]
    fn compile_active_profile_includes_all_supported_rule_categories() {
        let mut config = default_config();
        let profile = config.profiles.get_mut("custom").unwrap();
        *profile = Profile::default();
        profile.mouse_rules.push(Rule {
            trigger: "scroll:vertical".into(),
            action: "invert_scroll".into(),
            ..Rule::default()
        });
        profile.global_hotkeys.push(Rule {
            trigger: "key:12".into(),
            action: "lock_screen".into(),
            ..Rule::default()
        });
        profile.app_rules.push(Rule {
            trigger: "key:13".into(),
            action: "pass".into(),
            app_bundle_id: Some("com.example.Editor".into()),
            ..Rule::default()
        });
        config.active_profile = "custom".into();

        let compiled = config.compile_active_profile().unwrap();
        assert_eq!(compiled.rules().len(), 3);
        assert!(compiled.rules().iter().any(|rule| matches!(
            rule.action,
            Action::Replace(ref outputs)
                if outputs.iter().any(|output| matches!(
                    output,
                    OutputEvent::SystemShortcut(event_model::SystemShortcut::LockScreen)
                ))
        )));
    }

    #[test]
    fn device_scoped_rules_are_rejected_explicitly() {
        let mut config = default_config();
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .device_rules
            .push(Rule {
                trigger: "key:12".into(),
                action: "pass".into(),
                device_identifier: Some("keyboard-1".into()),
                ..Rule::default()
            });
        config.active_profile = "custom".into();

        assert!(matches!(
            config.compile_active_profile(),
            Err(ConfigError::UnsupportedScope(scope)) if scope == "device_rules"
        ));
    }

    #[test]
    fn v2_rejects_legacy_top_level_rules_instead_of_implicit_fallback() {
        let mut config = default_config();
        config.active_profile = "custom".into();
        config.profiles.insert("custom".into(), Profile::default());
        config.keyboard_rules = vec![Rule {
            trigger: "key:12".into(),
            action: "key:13".into(),
            ..Rule::default()
        }];
        assert!(matches!(
            config.compile_active_profile(),
            Err(ConfigError::InvalidRule(_))
        ));
    }

    #[test]
    fn v2_empty_profile_is_explicitly_empty() {
        let mut config = default_config();
        config.active_profile = "custom".into();
        config.profiles.insert("custom".into(), Profile::default());
        assert!(config.compile_active_profile().unwrap().rules().is_empty());
    }

    #[test]
    fn default_profile_contains_windows_edit_chords() {
        let config = default_config();
        let compiled = config.compile_active_profile().unwrap();
        assert!(compiled.rules().iter().any(|rule| {
            matches!(rule.trigger, Trigger::Chord { modifiers, key, .. } if modifiers.contains(ModifierSet::LEFT_CONTROL) && key == PhysicalCode::key(8))
        }));
    }

    #[test]
    fn default_profile_contains_select_all_and_hard_refresh_chords() {
        let config = default_config();
        let compiled = config.compile_active_profile().unwrap();
        assert!(compiled.rules().iter().any(|rule| {
            matches!(
                rule.trigger,
                Trigger::Chord { modifiers, key, .. }
                    if modifiers.contains(ModifierSet::LEFT_CONTROL)
                        && key == PhysicalCode::key(0)
            ) && matches!(
                rule.action,
                Action::Replace(ref outputs)
                    if outputs.contains(&OutputEvent::Key {
                        code: PhysicalCode::key(0),
                        phase: EventPhase::Down,
                        modifiers: ModifierSet::LEFT_COMMAND,
                    })
            )
        }));
        assert!(config.profiles["windows_physical"]
            .keyboard_rules
            .iter()
            .any(|rule| {
                rule.trigger == "chord:left_control+key:96"
                    && rule.action == "shortcut:hard_refresh"
            }));
    }

    #[test]
    fn default_profile_contains_application_switch_shortcut() {
        let config = default_config();
        let compiled = config.compile_active_profile().unwrap();
        assert!(compiled.rules().iter().any(|rule| {
            matches!(
                rule.trigger,
                Trigger::Chord { modifiers, key, .. }
                    if modifiers.contains(ModifierSet::LEFT_COMMAND)
                        && key == PhysicalCode::key(48)
            ) && matches!(
                rule.action,
                Action::Replace(ref outputs)
                    if outputs.contains(&OutputEvent::SystemShortcut(SystemShortcut::SwitchApplication))
            )
        }));
    }

    #[test]
    fn parses_modifier_chord_input_source_rule() {
        let mut config = default_config();
        config.active_profile = "custom".into();
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: "modifier_chord:left_control|left_shift".into(),
                action: "shortcut:switch_input_source".into(),
                ..Rule::default()
            });
        let compiled = config.compile_active_profile().unwrap();
        assert!(compiled.rules().iter().any(|rule| {
            matches!(
                rule.trigger,
                Trigger::ModifierChord { modifiers, phase: EventPhase::Up }
                    if modifiers == (ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_SHIFT)
            ) && matches!(
                rule.action,
                Action::Replace(ref outputs)
                    if outputs.contains(&OutputEvent::SystemShortcut(SystemShortcut::SwitchInputSource))
            )
        }));
    }

    #[test]
    fn parses_show_clipboard_shortcut() {
        let mut config = default_config();
        config.active_profile = "custom".into();
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: "chord:left_option+key:9".into(),
                action: "shortcut:show_clipboard".into(),
                ..Rule::default()
            });
        let compiled = config.compile_active_profile().unwrap();
        assert!(compiled.rules().iter().any(|rule| {
            matches!(
                rule.action,
                Action::Replace(ref outputs)
                    if outputs.contains(&OutputEvent::SystemShortcut(SystemShortcut::ShowClipboard))
            )
        }));
    }

    #[test]
    fn default_profile_contains_all_ctrl_shift_input_source_chords() {
        let config = default_config();
        let compiled = config.compile_active_profile().unwrap();
        let combinations = [
            ModifierSet::LEFT_CONTROL | ModifierSet::LEFT_SHIFT,
            ModifierSet::LEFT_CONTROL | ModifierSet::RIGHT_SHIFT,
            ModifierSet::RIGHT_CONTROL | ModifierSet::LEFT_SHIFT,
            ModifierSet::RIGHT_CONTROL | ModifierSet::RIGHT_SHIFT,
        ];
        for combination in combinations {
            assert!(compiled.rules().iter().any(|rule| {
                matches!(
                    rule.trigger,
                    Trigger::ModifierChord { modifiers, phase: EventPhase::Up }
                        if modifiers == combination
                )
            }));
        }
    }

    #[test]
    fn custom_profile_starts_as_an_independent_copy_and_keeps_defaults_on_add() {
        let mut config = default_config();
        config.active_profile = "custom".into();
        let default_rule_count = config.compile_active_profile().unwrap().rules().len();
        let original_windows = config.profiles["windows_physical"].clone();
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .keyboard_rules
            .push(Rule {
                trigger: "key:12".into(),
                action: "key:13".into(),
                ..Rule::default()
            });
        assert_eq!(
            config.compile_active_profile().unwrap().rules().len(),
            default_rule_count + 1
        );
        assert_eq!(config.profiles["windows_physical"], original_windows);
        let windows_ids: HashSet<_> = original_windows
            .keyboard_rules
            .iter()
            .map(|rule| &rule.id)
            .collect();
        assert!(config.profiles["custom"]
            .keyboard_rules
            .iter()
            .all(|rule| !windows_ids.contains(&rule.id)));
    }

    #[test]
    fn migrate_legacy_empty_profiles_materializes_fallback_and_hidden_modifiers_once() {
        let legacy = r#"
            schema_version = 1
            active_profile = "custom"
            [[keyboard_rules]]
            trigger = "key:12"
            action = "key:13"
            [profiles.custom]
            [profiles.other]
        "#;
        let migrated = import_toml(legacy).unwrap();
        assert_eq!(migrated.schema_version, 2);
        assert!(migrated.keyboard_rules.is_empty());
        for profile in migrated.profiles.values() {
            assert_eq!(profile.keyboard_rules.len(), 5);
            assert!(profile
                .keyboard_rules
                .iter()
                .all(|rule| !rule.name.is_empty()
                    && uuid::Uuid::parse_str(&rule.id).unwrap().get_version_num() == 4));
        }
        assert_eq!(
            import_toml(&export_toml(&migrated).unwrap()).unwrap(),
            migrated
        );
    }

    #[test]
    fn migrate_legacy_nonempty_profiles_does_not_merge_top_level_rules() {
        let migrated = import_toml(
            r#"
            schema_version = 1
            active_profile = "custom"
            [[keyboard_rules]]
            trigger = "key:12"
            action = "key:13"
            [[profiles.custom.mouse_rules]]
            trigger = "scroll:vertical"
            action = "invert_scroll"
        "#,
        )
        .unwrap();
        assert!(!migrated.profiles["custom"]
            .keyboard_rules
            .iter()
            .any(|rule| rule.trigger == "key:12"));
        assert_eq!(migrated.profiles["custom"].mouse_rules.len(), 1);
    }

    #[test]
    fn v1_migration_respects_explicit_disabled_modifier_rule() {
        let migrated = import_toml(
            r#"
            schema_version = 1
            active_profile = "custom"
            [[profiles.custom.keyboard_rules]]
            trigger = "left_option"
            action = "left_windows"
            enabled = false
        "#,
        )
        .unwrap();
        let matching: Vec<_> = migrated.profiles["custom"]
            .keyboard_rules
            .iter()
            .filter(|rule| rule.trigger == "left_option")
            .collect();
        assert_eq!(matching.len(), 1);
        assert!(!matching[0].enabled);
    }

    #[test]
    fn validates_inactive_profiles_and_rejects_duplicate_ids() {
        let mut config = default_config();
        config.profiles.get_mut("custom").unwrap().keyboard_rules[0].trigger = "key:999".into();
        assert!(config.compile_active_profile().is_err());
        let mut config = default_config();
        let duplicate = config.profiles["windows_physical"].keyboard_rules[0]
            .id
            .clone();
        config.profiles.get_mut("custom").unwrap().keyboard_rules[0].id = duplicate;
        assert!(config.validate().is_err());
    }

    #[test]
    fn v2_requires_stable_uuid_and_name() {
        let mut config = default_config();
        config
            .profiles
            .get_mut("windows_physical")
            .unwrap()
            .keyboard_rules[0]
            .id
            .clear();
        assert!(config.validate().is_err());
        let mut config = default_config();
        config
            .profiles
            .get_mut("windows_physical")
            .unwrap()
            .keyboard_rules[0]
            .name
            .clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn desktop_rejects_enabled_application_rules_in_any_profile() {
        let mut config = default_config();
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .app_rules
            .push(Rule {
                trigger: "key:12".into(),
                action: "pass".into(),
                app_bundle_id: Some("com.example.Editor".into()),
                ..Rule::default()
            });
        assert!(config.validate().is_ok());
        assert!(matches!(
            config.validate_desktop(),
            Err(ConfigError::UnsupportedScope(_))
        ));
        config.profiles.get_mut("custom").unwrap().app_rules[0].enabled = false;
        config.validate_desktop().unwrap();
    }

    #[test]
    fn show_desktop_configuration_generates_complete_key_and_chord_pairs() {
        let mut config = default_config();
        assert_eq!(
            config.show_desktop_output().unwrap(),
            vec![
                OutputEvent::Key {
                    code: PhysicalCode::key(103),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::NONE
                },
                OutputEvent::Key {
                    code: PhysicalCode::key(103),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::NONE
                },
            ]
        );
        config.system_shortcuts.show_desktop = "chord:left_control|left_shift+key:103".into();
        let outputs = config.show_desktop_output().unwrap();
        assert_eq!(outputs.len(), 6);
        assert!(matches!(
            outputs.last(),
            Some(OutputEvent::Key {
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE,
                ..
            })
        ));
        config.system_shortcuts.show_desktop = "exact_chord:left_control|left_shift+key:103".into();
        assert_eq!(config.show_desktop_output().unwrap(), outputs);
        config.system_shortcuts.show_desktop = "modifier_chord:left_shift".into();
        assert!(config.validate().is_err());
        config.system_shortcuts.show_desktop = "left_shift".into();
        assert_eq!(
            config.show_desktop_output().unwrap(),
            vec![
                OutputEvent::Key {
                    code: PhysicalCode::key(56),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::LEFT_SHIFT
                },
                OutputEvent::Key {
                    code: PhysicalCode::key(56),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::NONE
                },
            ]
        );
    }

    #[test]
    fn disabled_default_option_mapping_has_no_hidden_modifier_fallback() {
        use event_model::{DeviceSource, EventKind, EventOrigin, NormalizedEvent};
        let mut config = default_config();
        config
            .profiles
            .get_mut("windows_physical")
            .unwrap()
            .keyboard_rules
            .iter_mut()
            .find(|rule| rule.trigger == "left_option")
            .unwrap()
            .enabled = false;
        let mut engine = rule_engine::RuleEngine::new(config.compile_active_profile().unwrap());
        let press = NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind: EventKind::FlagsChanged,
            phase: EventPhase::Changed,
            code: PhysicalCode::left_option(),
            modifiers: ModifierSet::LEFT_OPTION,
            location: None,
            scroll: None,
            timestamp_ns: 1,
            app_context: None,
            origin: EventOrigin::Hardware,
        };
        assert_eq!(engine.handle(&press), event_model::Decision::Pass);
        let letter = NormalizedEvent {
            kind: EventKind::Keyboard,
            phase: EventPhase::Down,
            code: PhysicalCode::key(4),
            ..press.clone()
        };
        assert_eq!(engine.handle(&letter), event_model::Decision::Pass);
        assert_eq!(engine.logical_modifiers(), ModifierSet::LEFT_OPTION);
        let release = NormalizedEvent {
            modifiers: ModifierSet::NONE,
            ..press
        };
        assert_eq!(engine.handle(&release), event_model::Decision::Pass);
    }

    #[test]
    fn modifier_actions_use_native_target_codes_and_mouse_buttons_are_bounded() {
        for (action, code) in [
            ("left_windows", 55),
            ("right_windows", 54),
            ("left_alt", 58),
            ("right_alt", 61),
        ] {
            assert!(
                matches!(parse_action(action), Some(Action::Replace(ref outputs)) if matches!(outputs.as_slice(), [OutputEvent::Key {code: PhysicalCode::Key(target), ..}] if *target == code))
            );
        }
        assert!(parse_action("mouse:other:255").is_none());
        assert_eq!(parse_mouse_button("other:0"), Some(MouseButton::Left));
        assert_eq!(parse_mouse_button("other:31"), Some(MouseButton::Other(31)));
    }

    #[test]
    fn all_eight_recorded_modifier_names_compile_and_shift_outputs_keep_flags() {
        let mut config = default_config();
        let profile = config.profiles.get_mut("custom").unwrap();
        *profile = Profile::default();
        for trigger in [
            "left_shift",
            "right_shift",
            "left_control",
            "right_control",
            "left_option",
            "right_option",
            "left_command",
            "right_command",
        ] {
            profile.keyboard_rules.push(Rule {
                trigger: trigger.into(),
                action: "pass".into(),
                ..Rule::default()
            });
        }
        config.active_profile = "custom".into();
        assert_eq!(config.compile_active_profile().unwrap().rules().len(), 8);
        for (action, code, flag) in [
            ("left_shift", 56, ModifierSet::LEFT_SHIFT),
            ("right_shift", 60, ModifierSet::RIGHT_SHIFT),
        ] {
            assert_eq!(
                parse_action(action),
                Some(Action::Replace(vec![OutputEvent::Key {
                    code: PhysicalCode::key(code),
                    phase: EventPhase::Down,
                    modifiers: flag,
                }]))
            );
        }
    }

    #[test]
    fn modifier_chord_action_error_identifies_profile_category_and_index() {
        let mut config = default_config();
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .global_hotkeys
            .push(Rule {
                trigger: "modifier_chord:left_control|left_shift".into(),
                action: "suppress".into(),
                ..Rule::default()
            });
        assert!(
            matches!(config.validate(), Err(ConfigError::InvalidRule(message)) if message.contains("profiles.custom.global_hotkeys[0].action"))
        );
    }
}
