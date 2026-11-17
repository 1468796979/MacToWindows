use super::*;
use mac_to_windows_config::{import_toml, save_atomic, Profile};
use std::path::Path;

#[derive(Default)]
pub(super) struct SettingsStore {
    pub path: Option<PathBuf>,
    mutation: Mutex<()>,
    pub error: Mutex<Option<String>>,
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
    let rules = categories(profile)
        .into_iter()
        .flat_map(|(category, rules)| {
            rules.iter().map(move |rule| RuleView {
                id: rule.id.clone(),
                name: rule.name.clone(),
                category: category.into(),
                trigger: rule.trigger.clone(),
                action: rule.action.clone(),
                enabled: rule.enabled,
                priority: rule.priority,
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
        runtime: runtime_view(state, &runtime),
        config_error: state
            .settings
            .error
            .lock()
            .map_err(|_| "settings error lock poisoned")?
            .clone(),
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
    let _writer = state
        .settings
        .mutation
        .lock()
        .map_err(|_| "settings lock poisoned")?;
    if state.shutdown_started.load(Ordering::Acquire) {
        return Err("应用正在退出".into());
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
    if let Some(path) = &state.settings.path {
        if let Err(error) = save_atomic(path, &candidate) {
            let message = format!("保存配置失败，原配置继续生效：{error}");
            *state
                .settings
                .error
                .lock()
                .map_err(|_| "settings error lock poisoned")? = Some(message.clone());
            return Err(message);
        }
    }
    state
        .runtime
        .lock()
        .map_err(|_| "runtime lock poisoned")?
        .install_config(candidate, rules);
    *state
        .settings
        .error
        .lock()
        .map_err(|_| "settings error lock poisoned")? = None;
    snapshot(state)
}

pub(super) fn set_enabled(
    config: &mut AppConfig,
    profile_id: &str,
    rule_id: &str,
    enabled: bool,
) -> Result<(), String> {
    let profile = config.profiles.get_mut(profile_id).ok_or("预设不存在")?;
    for rules in [
        &mut profile.keyboard_rules,
        &mut profile.mouse_rules,
        &mut profile.global_hotkeys,
        &mut profile.app_rules,
        &mut profile.device_rules,
    ] {
        if let Some(rule) = rules.iter_mut().find(|r| r.id == rule_id) {
            rule.enabled = enabled;
            return Ok(());
        }
    }
    Err("规则不存在，请刷新配置".into())
}

fn read_valid(path: &Path) -> Result<(AppConfig, bool), String> {
    let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let document: toml::Value = contents.parse::<toml::Value>().map_err(|e| e.to_string())?;
    let legacy = document
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .unwrap_or(1)
        == 1;
    let config = import_toml(&contents).map_err(|e| e.to_string())?;
    config.validate_desktop().map_err(|e| e.to_string())?;
    Ok((config, legacy))
}

pub(super) fn load_startup(path: &Path) -> (AppConfig, Option<String>) {
    if !path.exists() {
        let backup = path.with_extension("toml.bak");
        if backup.exists() {
            return match read_valid(&backup) {
                Ok((config, _)) => {
                    let error = match save_atomic(path, &config) {
                        Ok(()) => "主配置缺失，已恢复有效备份".into(),
                        Err(e) => format!("已在内存恢复配置备份，但写回失败：{e}"),
                    };
                    (config, Some(error))
                }
                Err(error) => (
                    AppConfig::default(),
                    Some(format!(
                        "主配置缺失且备份无法读取，已暂停并使用默认配置；备份保留：{error}"
                    )),
                ),
            };
        }
        let config = AppConfig::default();
        let error = save_atomic(path, &config)
            .err()
            .map(|e| format!("无法创建本地配置：{e}"));
        return (config, error);
    }
    match read_valid(path) {
        Ok((config, legacy)) => {
            let error = if legacy {
                save_atomic(path, &config)
                    .err()
                    .map(|e| format!("配置已迁移但保存失败：{e}"))
            } else {
                None
            };
            (config, error)
        }
        Err(error) => {
            let backup = path.with_extension("toml.bak");
            if let Ok((config, _)) = read_valid(&backup) {
                let suffix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let corrupt = path.with_extension(format!("corrupt.{suffix}.toml"));
                let restored = std::fs::rename(path, &corrupt)
                    .map_err(|e| e.to_string())
                    .and_then(|_| save_atomic(path, &config).map_err(|e| e.to_string()));
                return (
                    config,
                    Some(match restored {
                        Ok(()) => format!("配置读取失败，已恢复有效备份并保留损坏文件：{error}"),
                        Err(e) => format!("已在内存恢复配置备份，但写回失败：{e}"),
                    }),
                );
            }
            (
                AppConfig::default(),
                Some(format!(
                    "配置及备份无法加载，已暂停并使用默认配置；原文件保留：{error}"
                )),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mac_to_windows_config::export_toml;

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
        let (config, error) = load_startup(&path);
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
        let (loaded, error) = load_startup(state.settings.path.as_ref().unwrap());
        assert!(error.is_none());
        assert!(!loaded.profiles["windows_physical"].keyboard_rules[0].enabled);
        assert!(!loaded.enabled);
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
        let (loaded, error) = load_startup(&path);
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
        let (loaded, error) = load_startup(&path);
        assert_eq!(loaded.active_profile, "custom");
        assert!(error.unwrap().contains("恢复"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), contents);
        assert!(read_valid(&path).is_ok());
    }
}
