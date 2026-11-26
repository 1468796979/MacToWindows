use event_model::{
    EventPhase, ModifierSet, MouseButton, OutputEvent, PhysicalCode, ScrollAxis, SystemShortcut,
};
use rule_engine::{
    Action, CompiledRules, EngineError, ModifierMatcher, Rule as EngineRule, RuleScope, Trigger,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{self, Write},
    path::Path,
};
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

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
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            scroll_mode: default_scroll_mode(),
        }
    }
}

fn default_scroll_mode() -> String {
    "non_natural".into()
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

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        if !self.profiles.contains_key(&self.active_profile) {
            return Err(ConfigError::UnknownProfile(self.active_profile.clone()));
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

    pub fn show_desktop_output(&self) -> Result<Vec<OutputEvent>, ConfigError> {
        let (key, modifiers) = match parse_trigger(&self.system_shortcuts.show_desktop) {
            Some(Trigger::Key {
                code,
                phase: EventPhase::Down,
                ..
            }) => (code, ModifierSet::NONE),
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
        let trigger = parse_trigger(&input.trigger).ok_or_else(|| {
            ConfigError::InvalidRule(format!("profiles.{profile_id}.{category}[{index}].trigger"))
        })?;
        let action = parse_action(&input.action).ok_or_else(|| {
            ConfigError::InvalidRule(format!("profiles.{profile_id}.{category}[{index}].action"))
        })?;
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
            scope: scope(input)?,
            trigger,
            action,
        });
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
    let mut profiles = BTreeMap::new();
    profiles.insert("windows_physical".into(), profile.clone());
    for id in ["shortcut", "custom"] {
        let mut independent = profile.clone();
        for rule in &mut independent.keyboard_rules {
            rule.id = uuid::Uuid::new_v4().to_string();
        }
        profiles.insert(id.into(), independent);
    }
    AppConfig {
        schema_version: CURRENT_SCHEMA_VERSION,
        active_profile: "windows_physical".into(),
        enabled: false,
        software_input_passthrough: true,
        panic_hotkey: default_panic_hotkey(),
        mouse: MouseConfig::default(),
        system_shortcuts: SystemShortcutsConfig::default(),
        keyboard_rules: Vec::new(),
        mouse_rules: Vec::new(),
        global_hotkeys: Vec::new(),
        app_rules: Vec::new(),
        device_rules: Vec::new(),
        profiles,
    }
}

pub fn import_toml(input: &str) -> Result<AppConfig, ConfigError> {
    let config = parse_toml(input)?;
    config.validate()?;
    Ok(config)
}

/// Load saved settings from releases that accepted scroll reversal on a key
/// or button. Preserve those entries for editing without enabling them.
pub fn import_saved_toml(input: &str) -> Result<(AppConfig, Vec<String>), ConfigError> {
    let mut config = parse_toml(input)?;
    let mut disabled = Vec::new();
    for (profile_id, profile) in &mut config.profiles {
        for (category, rules) in [
            ("keyboard_rules", &mut profile.keyboard_rules),
            ("mouse_rules", &mut profile.mouse_rules),
            ("global_hotkeys", &mut profile.global_hotkeys),
            ("app_rules", &mut profile.app_rules),
            ("device_rules", &mut profile.device_rules),
        ] {
            for (index, rule) in rules.iter_mut().enumerate() {
                if rule.enabled
                    && rule.action == "invert_scroll"
                    && parse_trigger(&rule.trigger)
                        .is_some_and(|trigger| !is_scroll_trigger(&trigger))
                {
                    rule.enabled = false;
                    disabled.push(format!("profiles.{profile_id}.{category}[{index}]"));
                }
            }
        }
    }
    config.validate()?;
    Ok((config, disabled))
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
        for (trigger, action) in [
            ("left_option", "left_windows"),
            ("right_option", "right_windows"),
            ("left_command", "left_alt"),
            ("right_command", "right_alt"),
        ] {
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

pub fn save_atomic(path: impl AsRef<Path>, config: &AppConfig) -> Result<(), ConfigError> {
    let path = path.as_ref();
    let data = export_toml(config)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), unique_suffix()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(data.as_bytes())?;
        file.sync_all()?;
        let backup = path.with_extension("toml.bak");
        if path.exists() {
            let backup_tmp = path.with_extension(format!("bak.tmp.{}", unique_suffix()));
            fs::copy(path, &backup_tmp)?;
            File::open(&backup_tmp)?.sync_all()?;
            fs::rename(&backup_tmp, &backup)?;
        }
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok::<(), io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(ConfigError::Io)
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
        assert_eq!(config.mouse.scroll_mode, "non_natural");
    }

    #[test]
    fn active_profile_compiles_to_rule_snapshot() {
        let config = default_config();
        let compiled = config.compile_active_profile().unwrap();
        assert_eq!(compiled.rules().len(), 15);
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
        assert_eq!(config.compile_active_profile().unwrap().rules().len(), 16);
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
