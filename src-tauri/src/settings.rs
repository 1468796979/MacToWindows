use super::*;
use mac_to_windows_config::{import_saved_toml, save_atomic, ConfigError, Profile, SaveOutcome};
use std::path::Path;
use std::{fs, io};

#[derive(Default)]
pub(super) struct SettingsStore {
    pub path: Option<PathBuf>,
    mutation: Mutex<()>,
    pub error: Mutex<Option<String>>,
    pub recovery_notice: Option<String>,
    pub writes_blocked: bool,
}

#[derive(Debug, Serialize)]
pub struct RuleView {
    pub id: String,
    pub name: String,
    pub category: String,
    pub trigger: String,
    pub action: String,
    pub enabled: bool,
    pub priority: i32,
    pub covered_by: Option<String>,
    pub app_bundle_id: Option<String>,
    pub editable: bool,
}

#[derive(Debug, Serialize)]
pub struct SettingsSnapshot {
    pub config_revision: u64,
    pub active_profile: String,
    pub profiles: Vec<String>,
    pub rules: Vec<RuleView>,
    pub show_desktop_shortcut: String,
    pub software_input_passthrough: bool,
    pub runtime: RuntimeView,
    pub config_error: Option<String>,
}

fn categories(profile: &Profile) -> [(&str, &[mac_to_windows_config::Rule]); 5] {
    [
        ("keyboard_rules", &profile.keyboard_rules),
        ("mouse_rules", &profile.mouse_rules),
        ("global_hotkeys", &profile.global_hotkeys),
        ("app_rules", &profile.app_rules),
        ("device_rules", &profile.device_rules),
    ]
}

pub(super) fn snapshot(state: &AppState) -> Result<SettingsSnapshot, String> {
    let runtime = state.runtime.lock().map_err(|_| "runtime lock poisoned")?;
    let config = runtime.config.as_ref().ok_or("运行时没有可编辑的配置")?;
    let profile = config
        .profiles
        .get(&config.active_profile)
        .ok_or("活动预设不存在")?;
    // Full mapping uses the first enabled rule for each identical trigger and scope.
    let mut first_by_trigger = HashMap::new();
    let mut covered_rules = HashMap::new();
    for rule in runtime.active_rules.rules() {
        let first = *first_by_trigger
            .entry((&rule.scope, &rule.trigger))
            .or_insert(rule.id.as_str());
        if first != rule.id.as_str() {
            covered_rules.insert(rule.id.as_str(), first);
        }
    }
    let rules = categories(profile)
        .into_iter()
        .flat_map(|(category, rules)| {
            let covered_rules = &covered_rules;
            rules.iter().map(move |rule| RuleView {
                id: rule.id.clone(),
                name: rule.name.clone(),
                category: category.into(),
                trigger: rule.trigger.clone(),
                action: rule.action.clone(),
                enabled: rule.enabled,
                priority: rule.priority,
                covered_by: covered_rules
                    .get(rule.id.as_str())
                    .copied()
                    .map(str::to_owned),
                app_bundle_id: rule.app_bundle_id.clone(),
                editable: true,
            })
        })
        .collect();
    Ok(SettingsSnapshot {
        config_revision: runtime.config_revision,
        active_profile: config.active_profile.clone(),
        profiles: config.profiles.keys().cloned().collect(),
        rules,
        show_desktop_shortcut: config.system_shortcuts.show_desktop.clone(),
        software_input_passthrough: config.software_input_passthrough,
        runtime: runtime_view(state, &runtime),
        config_error: {
            let save_error = state
                .settings
                .error
                .lock()
                .map_err(|_| "settings error lock poisoned")?;
            match (&*save_error, &state.settings.recovery_notice) {
                (Some(error), Some(notice)) => Some(format!("{error}\n{notice}")),
                (Some(message), None) | (None, Some(message)) => Some(message.clone()),
                (None, None) => None,
            }
        },
    })
}

// The mutation gate serializes writers, while all validation and disk I/O occur
// without holding the Event Tap's runtime mutex. Installation is an infallible swap.
pub(super) fn mutate<F>(
    state: &AppState,
    expected_revision: u64,
    edit: F,
) -> Result<SettingsSnapshot, String>
where
    F: FnOnce(&mut AppConfig) -> Result<(), String>,
{
    mutate_with_save(state, expected_revision, edit, |path, candidate| {
        save_atomic(path, candidate)
    })
}

fn mutate_with_save<F>(
    state: &AppState,
    expected_revision: u64,
    edit: F,
    save: impl FnOnce(&Path, &AppConfig) -> Result<SaveOutcome, ConfigError>,
) -> Result<SettingsSnapshot, String>
where
    F: FnOnce(&mut AppConfig) -> Result<(), String>,
{
    let _writer = state
        .settings
        .mutation
        .lock()
        .map_err(|_| "settings lock poisoned")?;
    if state.shutdown_started.load(Ordering::Acquire) {
        return Err("应用正在退出".into());
    }
    if state.settings.writes_blocked {
        return Err("无法保全原配置，已暂停保存。请检查恢复提示后重新启动应用。".into());
    }
    let mut candidate = {
        let runtime = state.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        if runtime.config_revision != expected_revision {
            return Err("配置已更新，请刷新后重试".into());
        }
        runtime.config.clone().ok_or("运行时没有可编辑的配置")?
    };
    edit(&mut candidate)?;
    candidate.enabled = false; // Persist rules, never automatically resume interception.
    candidate.validate_desktop().map_err(|e| e.to_string())?;
    let rules = candidate
        .compile_active_profile()
        .map_err(|e| e.to_string())?;
    let warning = match state
        .settings
        .path
        .as_deref()
        .map(|path| save(path, &candidate))
    {
        Some(Ok(SaveOutcome::DirectorySyncFailed(error))) => Some(save_sync_warning(&error)),
        Some(Ok(SaveOutcome::Saved)) | None => None,
        Some(Err(error)) => {
            let message = format!("保存配置失败，原配置继续生效：{error}");
            *state
                .settings
                .error
                .lock()
                .map_err(|_| "settings error lock poisoned")? = Some(message.clone());
            return Err(message);
        }
    };
    state
        .runtime
        .lock()
        .map_err(|_| "runtime lock poisoned")?
        .install_config(candidate, rules);
    *state
        .settings
        .error
        .lock()
        .map_err(|_| "settings error lock poisoned")? = warning;
    snapshot(state)
}

pub(super) fn set_software_input_passthrough(
    state: &AppState,
    expected_revision: u64,
    enabled: bool,
) -> Result<SettingsSnapshot, String> {
    mutate(state, expected_revision, |config| {
        config.software_input_passthrough = enabled;
        Ok(())
    })
}

pub(super) fn set_enabled(
    config: &mut AppConfig,
    profile_id: &str,
    rule_id: &str,
    enabled: bool,
) -> Result<(), String> {
    find_rule_mut(config, profile_id, rule_id)?.enabled = enabled;
    Ok(())
}

pub(super) fn set_priority(
    config: &mut AppConfig,
    profile_id: &str,
    rule_id: &str,
    priority: i32,
) -> Result<(), String> {
    find_rule_mut(config, profile_id, rule_id)?.priority = priority;
    Ok(())
}

fn find_rule_mut<'a>(
    config: &'a mut AppConfig,
    profile_id: &str,
    rule_id: &str,
) -> Result<&'a mut mac_to_windows_config::Rule, String> {
    let profile = config.profiles.get_mut(profile_id).ok_or("预设不存在")?;
    for rules in [
        &mut profile.keyboard_rules,
        &mut profile.mouse_rules,
        &mut profile.global_hotkeys,
        &mut profile.app_rules,
        &mut profile.device_rules,
    ] {
        if let Some(rule) = rules.iter_mut().find(|r| r.id == rule_id) {
            return Ok(rule);
        }
    }
    Err("规则不存在，请刷新配置".into())
}

fn read_valid(path: &Path) -> Result<(AppConfig, bool, Vec<String>), String> {
    let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let document: toml::Value = contents.parse::<toml::Value>().map_err(|e| e.to_string())?;
    let legacy = document
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .unwrap_or(1)
        == 1;
    let (mut config, disabled_rules) = import_saved_toml(&contents).map_err(|e| e.to_string())?;
    let upgraded_save_shortcuts = config.upgrade_default_save_shortcuts();
    config.validate_desktop().map_err(|e| e.to_string())?;
    Ok((config, legacy || upgraded_save_shortcuts, disabled_rules))
}

fn startup_warning(message: Option<String>, disabled_rules: &[String]) -> Option<String> {
    if disabled_rules.is_empty() {
        return message;
    }
    let disabled = format!(
        "已停用触发键无效的滚轮反转规则（{}）",
        disabled_rules.join("、")
    );
    Some(match message {
        Some(message) => format!("{message}。{disabled}"),
        None => disabled,
    })
}

pub(super) struct StartupSettings {
    config: AppConfig,
    error: Option<String>,
    recovery_notice: Option<String>,
    writes_blocked: bool,
}

impl StartupSettings {
    pub(super) fn into_state(self, path: PathBuf) -> AppState {
        let mut state =
            AppState::from_config(self.config).expect("validated startup configuration");
        state.settings.path = Some(path);
        state.settings.writes_blocked = self.writes_blocked;
        state.settings.recovery_notice = self.recovery_notice;
        *state.settings.error.lock().expect("new settings lock") = self.error;
        state
    }
}

fn save_sync_warning(error: &io::Error) -> String {
    format!("配置已生效并写入文件，但目录同步失败，突然断电时可能丢失本次更改：{error}")
}

fn startup_save(
    path: &Path,
    config: &AppConfig,
    success: Option<String>,
    failure: &str,
) -> Option<String> {
    match save_atomic(path, config) {
        Ok(SaveOutcome::Saved) => success,
        Ok(SaveOutcome::DirectorySyncFailed(error)) => Some(save_sync_warning(&error)),
        Err(error) => Some(format!("{failure}：{error}")),
    }
}

fn recovery_directory(path: &Path) -> PathBuf {
    path.with_extension("manual-recovery")
}

fn archive_unreadable(path: &Path, sources: &[&Path]) -> io::Result<()> {
    // Check every source before moving any of them. In particular, leave an
    // unexpected directory or symbolic link in place and stop writes.
    for source in sources {
        if !fs::symlink_metadata(source)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} 不是普通文件", source.display()),
            ));
        }
    }
    let root = recovery_directory(path);
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.recursive(true).create(&root)?;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let archive = root.join(format!("{}-{suffix}", std::process::id()));
    builder.recursive(false).create(&archive)?;
    // Sync the new directory entries before relocating the originals.
    fs::File::open(&root)?.sync_all()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::File::open(parent)?;
    parent.sync_all()?;
    let archive_dir = fs::File::open(&archive)?;
    for source in sources {
        let file_name = source
            .file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        fs::rename(source, archive.join(file_name))?;
        archive_dir.sync_all()?;
        parent.sync_all()?;
    }
    Ok(())
}

fn read_if_present(path: &Path) -> Result<Option<(AppConfig, bool, Vec<String>)>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_valid(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn read_or_recover(path: &Path) -> (AppConfig, Option<String>, bool) {
    let primary_error = match read_if_present(path) {
        Ok(Some((config, needs_writeback, disabled_rules))) => {
            let error = if needs_writeback {
                startup_save(path, &config, None, "配置已在内存迁移，但保存失败")
            } else {
                None
            };
            return (config, startup_warning(error, &disabled_rules), false);
        }
        Ok(None) => None,
        Err(error) => Some(error),
    };
    let backup = path.with_extension("toml.bak");
    let backup_error = match read_if_present(&backup) {
        Ok(Some((config, _, disabled_rules))) => {
            let result = if let Some(error) = primary_error {
                archive_unreadable(path, &[path])
                    .map(|()| format!("主配置读取失败，已恢复有效备份：{error}"))
            } else {
                Ok("主配置缺失，已恢复有效备份".into())
            };
            return match result {
                Ok(message) => {
                    let error = startup_save(
                        path,
                        &config,
                        Some(message),
                        "已在内存恢复配置备份，但写回失败",
                    );
                    (config, startup_warning(error, &disabled_rules), false)
                }
                Err(error) => (
                    config,
                    startup_warning(
                        Some(format!(
                            "已在内存恢复配置备份，但无法保全主配置，已暂停保存：{error}"
                        )),
                        &disabled_rules,
                    ),
                    true,
                ),
            };
        }
        Ok(None) => None,
        Err(error) => Some(error),
    };
    let config = AppConfig::default();
    let mut sources = Vec::with_capacity(2);
    let mut errors = Vec::with_capacity(2);
    if let Some(error) = primary_error {
        sources.push(path);
        errors.push(format!("主配置：{error}"));
    }
    if let Some(error) = backup_error {
        sources.push(backup.as_path());
        errors.push(format!("备份：{error}"));
    }
    if sources.is_empty() {
        let error = startup_save(path, &config, None, "无法创建本地配置");
        return (config, error, false);
    }
    let error = format!(
        "无法读取配置，已暂停映射并使用默认配置。{}",
        errors.join("。")
    );
    match archive_unreadable(path, &sources) {
        Ok(()) => (config, Some(error), false),
        Err(archive_error) => (
            config,
            Some(format!(
                "{error} 无法保全原文件，已暂停保存：{archive_error}"
            )),
            true,
        ),
    }
}

pub(super) fn load_startup(path: &Path) -> StartupSettings {
    let (config, error, writes_blocked) = read_or_recover(path);
    let directory = recovery_directory(path);
    let recovery_notice = fs::read_dir(&directory)
        .is_ok_and(|mut entries| entries.next().is_some())
        .then(|| format!("旧配置恢复文件位于 {}。", directory.display()));
    StartupSettings {
        config,
        error,
        recovery_notice,
        writes_blocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mac_to_windows_config::export_toml;

    #[test]
    fn startup_keeps_previous_rules_and_disables_only_invalid_scroll_reversal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = AppConfig::default();
        config.active_profile = "custom".into();
        let rule = mac_to_windows_config::Rule {
            name: "旧版错误滚轮反转规则".into(),
            trigger: "key:3".into(),
            action: "invert_scroll".into(),
            ..Default::default()
        };
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .keyboard_rules
            .push(rule.clone());
        let old_file = toml::to_string(&config).unwrap();
        std::fs::write(&path, &old_file).unwrap();

        assert!(
            matches!(mac_to_windows_config::import_toml(&old_file), Err(error)
            if error.to_string().contains("profiles.custom.keyboard_rules"))
        );
        let StartupSettings {
            config: loaded,
            error: warning,
            ..
        } = load_startup(&path);
        assert!(warning.unwrap().contains("已停用触发键无效的滚轮反转规则"));
        config
            .profiles
            .get_mut("custom")
            .unwrap()
            .keyboard_rules
            .last_mut()
            .unwrap()
            .enabled = false;
        assert_eq!(loaded, config);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old_file);
    }

    fn state(path: PathBuf) -> AppState {
        let mut state = AppState::from_config(AppConfig::default()).unwrap();
        state.settings.path = Some(path);
        state
    }

    #[test]
    fn startup_migrates_legacy_file_and_keeps_original_backup_without_enabling() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let legacy = "schema_version = 1\nactive_profile = \"custom\"\nenabled = true\n[[keyboard_rules]]\ntrigger = \"key:0\"\naction = \"key:1\"\n[profiles.custom]\n";
        std::fs::write(&path, legacy).unwrap();
        let StartupSettings { config, error, .. } = load_startup(&path);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(config.schema_version, 2);
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            legacy
        );
        assert!(!read_valid(&path).unwrap().1);
        assert_eq!(
            RuntimeController::from_config(config).unwrap().status,
            RuntimeStatus::Disabled
        );
    }

    #[test]
    fn startup_persists_save_shortcuts_once_and_keeps_the_pre_upgrade_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let backup = path.with_extension("toml.bak");
        let mut previous = AppConfig::default();
        previous.software_input_passthrough = false;
        for profile in previous.profiles.values_mut() {
            profile
                .keyboard_rules
                .retain(|rule| !rule.trigger.starts_with("exact_chord:"));
        }
        let mut old_document = toml::Value::try_from(&previous).unwrap();
        old_document
            .as_table_mut()
            .unwrap()
            .remove("default_save_shortcuts_initialized");
        let original = toml::to_string_pretty(&old_document).unwrap();
        fs::write(&path, &original).unwrap();

        let upgraded = load_startup(&path);
        assert!(upgraded.error.is_none(), "{:?}", upgraded.error);
        assert!(!upgraded.config.software_input_passthrough);
        for (id, old_profile) in &previous.profiles {
            let rules = &upgraded.config.profiles[id].keyboard_rules;
            assert_eq!(
                &rules[..old_profile.keyboard_rules.len()],
                old_profile.keyboard_rules
            );
            assert_eq!(rules.len(), old_profile.keyboard_rules.len() + 3);
        }
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("default_save_shortcuts_initialized = true"));
        assert_eq!(load_startup(&path).config, upgraded.config);
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    }

    #[test]
    fn saved_rule_toggle_survives_restart_and_rejects_stale_revision() {
        let temp = tempfile::tempdir().unwrap();
        let state = state(temp.path().join("config.toml"));
        let before = snapshot(&state).unwrap();
        let id = before.rules[0].id.clone();
        let updated = mutate(&state, 0, |c| {
            set_enabled(c, "windows_physical", &id, false)
        })
        .unwrap();
        assert!(!updated.rules[0].enabled);
        assert_eq!(updated.config_revision, 1);
        assert!(mutate(&state, 0, |_| Ok(())).is_err());
        let StartupSettings {
            config: loaded,
            error,
            ..
        } = load_startup(state.settings.path.as_ref().unwrap());
        assert!(error.is_none());
        assert!(!loaded.profiles["windows_physical"].keyboard_rules[0].enabled);
        assert!(!loaded.enabled);
    }

    #[test]
    fn rule_coverage_follows_compiled_triggers_and_explicit_priority_changes() {
        use event_model::{
            DeviceSource, EventKind, EventOrigin, EventPhase, ModifierSet, PhysicalCode,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let state = state(path.clone());
        let custom = mac_to_windows_config::Rule {
            name: "自定义复制规则".into(),
            // A different spelling of the same compiled trigger.
            trigger: "chord:left_control+key:08".into(),
            action: "suppress".into(),
            priority: 100,
            ..Default::default()
        };
        let custom_id = custom.id.clone();
        let initial = mutate(&state, 0, |config| {
            config
                .profiles
                .get_mut("windows_physical")
                .unwrap()
                .keyboard_rules
                .push(custom);
            Ok(())
        })
        .unwrap();
        let default_id = initial
            .rules
            .iter()
            .find(|rule| rule.trigger == "chord:left_control+key:8")
            .unwrap()
            .id
            .clone();
        let coverage = |snapshot: &SettingsSnapshot, id: &str| {
            snapshot
                .rules
                .iter()
                .find(|rule| rule.id == id)
                .unwrap()
                .covered_by
                .clone()
        };
        let press_copy = || {
            let rules = state.runtime.lock().unwrap().active_rules.clone();
            RuleEngine::new(rules).handle(&NormalizedEvent {
                source: DeviceSource::Keyboard,
                kind: EventKind::Keyboard,
                phase: EventPhase::Down,
                code: PhysicalCode::key(8),
                modifiers: ModifierSet::LEFT_CONTROL,
                location: None,
                scroll: None,
                timestamp_ns: 0,
                app_context: None,
                origin: EventOrigin::Hardware,
            })
        };
        assert_eq!(coverage(&initial, &custom_id), Some(default_id.clone()));
        assert!(matches!(press_copy(), Decision::Emit(_)));

        let same = mutate(&state, 1, |config| {
            set_priority(config, "windows_physical", &custom_id, 200)
        })
        .unwrap_err();
        assert!(same.contains("conflicting rules"));
        let promoted = mutate(&state, 1, |config| {
            set_priority(config, "windows_physical", &custom_id, 201)
        })
        .unwrap();
        assert_eq!(promoted.config_revision, 2);
        assert_eq!(coverage(&promoted, &default_id), Some(custom_id.clone()));
        assert_eq!(coverage(&promoted, &custom_id), None);
        assert_eq!(press_copy(), Decision::Suppress);
        let loaded = mac_to_windows_config::load(&path).unwrap();
        assert_eq!(
            loaded.profiles["windows_physical"]
                .keyboard_rules
                .iter()
                .find(|rule| rule.id == custom_id)
                .unwrap()
                .priority,
            201
        );
    }

    #[test]
    fn software_passthrough_setting_persists_without_resuming_windows_mode() {
        let temp = tempfile::tempdir().unwrap();
        let state = state(temp.path().join("config.toml"));
        assert!(snapshot(&state).unwrap().software_input_passthrough);
        let updated = set_software_input_passthrough(&state, 0, false).unwrap();
        assert!(!updated.software_input_passthrough);
        assert_eq!(updated.config_revision, 1);
        assert_eq!(updated.runtime.status, RuntimeStatus::Disabled);
        let changed_profile = mutate(&state, 1, |config| {
            config.active_profile = "custom".into();
            Ok(())
        })
        .unwrap();
        assert!(!changed_profile.software_input_passthrough);
        let StartupSettings {
            config: loaded,
            error,
            ..
        } = load_startup(state.settings.path.as_ref().unwrap());
        assert!(error.is_none());
        assert!(!loaded.software_input_passthrough);
        assert!(!loaded.enabled);
        assert_eq!(loaded.active_profile, "custom");
    }

    #[test]
    fn stale_software_passthrough_write_preserves_the_confirmed_choice() {
        let temp = tempfile::tempdir().unwrap();
        let state = state(temp.path().join("config.toml"));
        set_software_input_passthrough(&state, 0, false).unwrap();
        let error = set_software_input_passthrough(&state, 0, true).unwrap_err();
        assert!(error.contains("配置已更新"));
        let current = snapshot(&state).unwrap();
        assert!(!current.software_input_passthrough);
        assert_eq!(current.config_revision, 1);
    }

    #[test]
    fn failed_software_passthrough_save_keeps_the_live_policy() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("file");
        std::fs::write(&blocker, "not a directory").unwrap();
        let state = state(blocker.join("config.toml"));
        assert!(set_software_input_passthrough(&state, 0, false).is_err());
        let current = snapshot(&state).unwrap();
        assert!(current.software_input_passthrough);
        assert_eq!(current.config_revision, 0);
        assert_eq!(current.runtime.status, RuntimeStatus::Disabled);
        assert!(current.config_error.is_some());
    }

    #[test]
    fn disk_failure_leaves_live_config_and_revision_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("file");
        std::fs::write(&blocker, "not a directory").unwrap();
        let state = state(blocker.join("config.toml"));
        assert!(mutate(&state, 0, |c| {
            c.active_profile = "custom".into();
            Ok(())
        })
        .is_err());
        let snapshot = snapshot(&state).unwrap();
        assert_eq!(snapshot.active_profile, "windows_physical");
        assert_eq!(snapshot.config_revision, 0);
        assert!(snapshot.config_error.is_some());
    }

    #[test]
    fn invalid_import_does_not_replace_config_or_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let state = state(path.clone());
        let valid = snapshot(&state).unwrap();
        assert!(mutate(&state, 0, |c| {
            c.active_profile = "missing".into();
            Ok(())
        })
        .is_err());
        assert_eq!(
            snapshot(&state).unwrap().active_profile,
            valid.active_profile
        );
        assert!(!path.exists());
    }

    #[test]
    fn missing_primary_restores_valid_backup_instead_of_creating_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let backup = path.with_extension("toml.bak");
        let mut config = AppConfig::default();
        config.active_profile = "custom".into();
        let contents = export_toml(&config).unwrap();
        std::fs::write(&backup, &contents).unwrap();
        let StartupSettings {
            config: loaded,
            error,
            ..
        } = load_startup(&path);
        assert_eq!(loaded.active_profile, "custom");
        assert!(error.unwrap().contains("恢复"));
        assert_eq!(std::fs::read_to_string(backup).unwrap(), contents);
        assert_eq!(read_valid(&path).unwrap().0.active_profile, "custom");
    }

    #[test]
    fn corrupt_primary_restores_backup_without_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let backup = path.with_extension("toml.bak");
        let mut config = AppConfig::default();
        config.active_profile = "custom".into();
        let contents = export_toml(&config).unwrap();
        std::fs::write(&backup, &contents).unwrap();
        std::fs::write(&path, "[broken").unwrap();
        let StartupSettings {
            config: loaded,
            error,
            ..
        } = load_startup(&path);
        assert_eq!(loaded.active_profile, "custom");
        assert!(error.unwrap().contains("恢复"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), contents);
        assert!(read_valid(&path).is_ok());
    }

    #[test]
    fn unreadable_primary_and_backup_survive_two_saves_and_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let backup = path.with_extension("toml.bak");
        let old_primary = b"schema_version = 90\n# synthetic future format\n";
        let old_backup = b"[synthetic damaged backup";
        fs::write(&path, old_primary).unwrap();
        fs::write(&backup, old_backup).unwrap();

        let startup = load_startup(&path);
        assert!(!startup.writes_blocked);
        assert!(startup.recovery_notice.is_some());
        let current = startup.into_state(path.clone());
        set_software_input_passthrough(&current, 0, false).unwrap();
        mutate(&current, 1, |config| {
            config.active_profile = "custom".into();
            Ok(())
        })
        .unwrap();

        let restarted = load_startup(&path);
        assert!(restarted.error.is_none());
        assert!(restarted.recovery_notice.is_some());
        assert_eq!(restarted.config.active_profile, "custom");
        assert!(!restarted.config.software_input_passthrough);
        let archive = fs::read_dir(recovery_directory(&path))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(archive.join("config.toml")).unwrap(), old_primary);
        assert_eq!(
            fs::read(archive.join("config.toml.bak")).unwrap(),
            old_backup
        );
    }

    #[test]
    fn failure_to_archive_unreadable_config_prevents_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let backup = path.with_extension("toml.bak");
        fs::write(&path, b"broken").unwrap();
        fs::write(&backup, b"old").unwrap();
        fs::write(recovery_directory(&path), b"block directory creation").unwrap();

        let startup = load_startup(&path);
        assert!(startup.writes_blocked);
        let current = startup.into_state(path.clone());
        assert!(set_software_input_passthrough(&current, 0, false).is_err());
        assert_eq!(fs::read(path).unwrap(), b"broken");
        assert_eq!(fs::read(backup).unwrap(), b"old");
        assert_eq!(snapshot(&current).unwrap().config_revision, 0);
    }

    #[test]
    fn committed_sync_warning_keeps_live_settings_in_step_with_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let current = state(path.clone());
        let updated = mutate_with_save(
            &current,
            0,
            |config| {
                config.active_profile = "custom".into();
                Ok(())
            },
            |path, config| {
                save_atomic(path, config)?;
                Ok(SaveOutcome::DirectorySyncFailed(io::Error::new(
                    io::ErrorKind::Other,
                    "synthetic directory sync failure",
                )))
            },
        )
        .unwrap();
        assert_eq!(updated.active_profile, "custom");
        assert_eq!(updated.config_revision, 1);
        assert!(updated.config_error.unwrap().contains("目录同步失败"));
        assert_eq!(load_startup(&path).config.active_profile, "custom");

        let next = set_software_input_passthrough(&current, 1, false).unwrap();
        assert_eq!(next.config_revision, 2);
        assert!(next.config_error.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_directory_fails_before_replacing_config_or_backup() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let current = state(path.clone());
        save_atomic(&path, &AppConfig::default()).unwrap();
        let primary_before = fs::read(&path).unwrap();
        let backup = path.with_extension("toml.bak");
        fs::write(&backup, b"synthetic original backup").unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o300)).unwrap();
        if fs::File::open(temp.path()).is_ok() {
            // A process with elevated privileges can bypass directory mode checks.
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let result = set_software_input_passthrough(&current, 0, false);
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), primary_before);
        assert_eq!(fs::read(backup).unwrap(), b"synthetic original backup");
        let current = snapshot(&current).unwrap();
        assert!(current.software_input_passthrough);
        assert_eq!(current.config_revision, 0);
    }
}
