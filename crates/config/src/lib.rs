use event_model::{
    EventPhase, ModifierSet, MouseButton, OutputEvent, PhysicalCode, ScrollAxis, SystemShortcut,
};
use rule_engine::{
    Action, CompiledRules, EngineError, ModifierMatcher, Rule as EngineRule, RuleScope, Trigger,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Write},
    path::Path,
};
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// A deliberately small, extensible rule representation. Additional fields can be
/// added without coupling this crate to the rule engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
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
pub struct AppConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_active_profile")]
    pub active_profile: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_panic_hotkey")]
    pub panic_hotkey: Vec<String>,
    #[serde(default)]
    pub mouse: MouseConfig,
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

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}
fn default_active_profile() -> String {
    "windows_physical".to_owned()
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
        Ok(())
    }

    pub fn compile_active_profile(&self) -> Result<CompiledRules, ConfigError> {
        self.validate()?;
        let base_profile = self
            .profiles
            .get(&self.active_profile)
            .ok_or_else(|| ConfigError::UnknownProfile(self.active_profile.clone()))?;
        let mut profile = base_profile.clone();
        let profile_is_empty = profile.keyboard_rules.is_empty()
            && profile.mouse_rules.is_empty()
            && profile.global_hotkeys.is_empty()
            && profile.app_rules.is_empty()
            && profile.device_rules.is_empty();
        if profile_is_empty {
            profile.keyboard_rules = self.keyboard_rules.clone();
            profile.mouse_rules = self.mouse_rules.clone();
            profile.global_hotkeys = self.global_hotkeys.clone();
            profile.app_rules = self.app_rules.clone();
            profile.device_rules = self.device_rules.clone();
        }
        let mut rules = Vec::new();
        compile_rules(
            &mut rules,
            &self.active_profile,
            "keyboard_rules",
            &profile.keyboard_rules,
            |_| Ok(RuleScope::Global),
        )?;
        compile_rules(
            &mut rules,
            &self.active_profile,
            "mouse_rules",
            &profile.mouse_rules,
            |_| Ok(RuleScope::Global),
        )?;
        compile_rules(
            &mut rules,
            &self.active_profile,
            "global_hotkeys",
            &profile.global_hotkeys,
            |_| Ok(RuleScope::Global),
        )?;
        compile_rules(
            &mut rules,
            &self.active_profile,
            "app_rules",
            &profile.app_rules,
            |input| {
                let bundle_id = input.app_bundle_id.clone().ok_or_else(|| {
                    ConfigError::InvalidRule("app_rules.app_bundle_id is required".into())
                })?;
                if bundle_id.trim().is_empty() {
                    return Err(ConfigError::InvalidRule(
                        "app_rules.app_bundle_id must not be empty".into(),
                    ));
                }
                Ok(RuleScope::App { bundle_id })
            },
        )?;
        if !profile.device_rules.is_empty() {
            return Err(ConfigError::UnsupportedScope("device_rules".into()));
        }
        Ok(CompiledRules::compile(rules)?)
    }
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
        let trigger = parse_trigger(&input.trigger)
            .ok_or_else(|| ConfigError::InvalidRule(format!("{category}[{index}].trigger")))?;
        let action = parse_action(&input.action)
            .ok_or_else(|| ConfigError::InvalidRule(format!("{category}[{index}].action")))?;
        output.push(EngineRule {
            id: format!("{profile_id}-{category}-{index}"),
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
        code: parse_code(value)?,
        phase: EventPhase::Down,
        modifiers: match value {
            "left_windows" => ModifierSet::LEFT_WINDOWS,
            "right_windows" => ModifierSet::RIGHT_WINDOWS,
            "left_alt" => ModifierSet::LEFT_ALT,
            "right_alt" => ModifierSet::RIGHT_ALT,
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
        value => MouseButton::Other(value.strip_prefix("other:")?.parse().ok()?),
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
    let profile = Profile {
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
    let mut profiles = BTreeMap::new();
    profiles.insert("windows_physical".into(), profile.clone());
    profiles.insert("shortcut".into(), Profile::default());
    profiles.insert("custom".into(), Profile::default());
    AppConfig {
        schema_version: CURRENT_SCHEMA_VERSION,
        active_profile: "windows_physical".into(),
        enabled: false,
        panic_hotkey: default_panic_hotkey(),
        mouse: MouseConfig::default(),
        keyboard_rules: profile.keyboard_rules.clone(),
        mouse_rules: profile.mouse_rules.clone(),
        global_hotkeys: profile.global_hotkeys.clone(),
        app_rules: profile.app_rules.clone(),
        device_rules: profile.device_rules.clone(),
        profiles,
    }
}

pub fn import_toml(input: &str) -> Result<AppConfig, ConfigError> {
    let config: AppConfig = toml::from_str(input)?;
    config.compile_active_profile()?;
    Ok(config)
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
        assert_eq!(config.schema_version, 1);
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
        assert_eq!(compiled.rules().len(), 13);
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
    fn legacy_top_level_rules_are_used_when_profile_rules_are_empty() {
        let mut config = default_config();
        config.active_profile = "custom".into();
        config.keyboard_rules = vec![Rule {
            trigger: "key:12".into(),
            action: "key:13".into(),
            ..Rule::default()
        }];
        let compiled = config.compile_active_profile().unwrap();
        assert_eq!(compiled.rules().len(), 1);
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
}
