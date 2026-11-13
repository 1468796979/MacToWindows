use diagnostics::{Diagnostics, PermissionState, TapState};
use event_model::{Decision, NormalizedEvent, OutputEvent, SystemShortcut};
use mac_to_windows_config::AppConfig;
use macos_adapter::{
    EventAdapter, InjectionError, InputSourceCommand, InputSourceService, SafeAdapter,
};
use rule_engine::{CompiledRules, Rule, RuleEngine};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

mod clipboard;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RuntimeStatus {
    Disabled,
    Running,
    PermissionRequired,
    Degraded,
    Stopped,
}

pub enum RuntimeCommand {
    Enable,
    Disable,
    ReplaceRules(CompiledRules),
    ReplaceConfig(AppConfig),
    SetProfile(String),
    ReloadPermissions,
    ReleaseAll,
    Shutdown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RuntimeError {
    PermissionDenied,
    Configuration(String),
    ProfileConfigurationUnavailable,
    Injection(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardHotkeyAction {
    Show,
    Suppress,
}

#[derive(Debug, Default)]
pub struct ClipboardHotkeyLatch {
    active: bool,
}

impl ClipboardHotkeyLatch {
    pub fn reset(&mut self) {
        self.active = false;
    }

    pub fn handle(&mut self, event: &NormalizedEvent) -> Option<ClipboardHotkeyAction> {
        if event.source != event_model::DeviceSource::Keyboard
            || event.code != event_model::PhysicalCode::key(9)
            || !matches!(event.origin, event_model::EventOrigin::Hardware)
        {
            return None;
        }
        match event.phase {
            event_model::EventPhase::Down
                if event
                    .modifiers
                    .contains(event_model::ModifierSet::LEFT_OPTION) =>
            {
                if self.active {
                    Some(ClipboardHotkeyAction::Suppress)
                } else {
                    self.active = true;
                    Some(ClipboardHotkeyAction::Show)
                }
            }
            event_model::EventPhase::Repeat if self.active => Some(ClipboardHotkeyAction::Suppress),
            event_model::EventPhase::Up if self.active => {
                self.active = false;
                Some(ClipboardHotkeyAction::Suppress)
            }
            _ => None,
        }
    }
}

pub struct RuntimeController {
    engine: RuleEngine,
    adapter: SafeAdapter,
    input_source: Option<InputSourceService>,
    clipboard: Option<Arc<clipboard::ClipboardManager>>,
    diagnostics: Arc<Diagnostics>,
    profile: String,
    config: Option<AppConfig>,
    status: RuntimeStatus,
}

pub struct AppState {
    pub runtime: Arc<Mutex<RuntimeController>>,
    pub event_runtime: EventRuntime,
    clipboard: Arc<Mutex<Option<Arc<clipboard::ClipboardManager>>>>,
}

impl AppState {
    pub fn new(rules: CompiledRules) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(RuntimeController::new(rules))),
            event_runtime: EventRuntime::default(),
            clipboard: Arc::new(Mutex::new(None)),
        }
    }

    pub fn from_config(config: AppConfig) -> Result<Self, mac_to_windows_config::ConfigError> {
        Ok(Self {
            runtime: Arc::new(Mutex::new(RuntimeController::from_config(config)?)),
            event_runtime: EventRuntime::default(),
            clipboard: Arc::new(Mutex::new(None)),
        })
    }

    pub fn set_clipboard(&self, manager: Arc<clipboard::ClipboardManager>) {
        if let Ok(mut slot) = self.clipboard.lock() {
            *slot = Some(Arc::clone(&manager));
        }
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.set_clipboard(manager);
        }
    }

    pub fn clipboard(&self) -> Option<Arc<clipboard::ClipboardManager>> {
        self.clipboard.lock().ok().and_then(|slot| slot.clone())
    }
}

#[derive(Default)]
pub struct EventRuntime {
    started: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl EventRuntime {
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub fn start(
        &self,
        runtime: Arc<Mutex<RuntimeController>>,
        clipboard: Option<Arc<clipboard::ClipboardManager>>,
    ) -> Result<(), String> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let thread = std::thread::Builder::new()
                .name("mac-to-windows-event-tap".into())
                .spawn(move || {
                    let mask = event_mask();
                    let mut clipboard_hotkey = ClipboardHotkeyLatch::default();
                    let tap = macos_adapter::EventTap::create(mask, move |raw| {
                        if raw.is_tap_disabled() {
                            return macos_adapter::EventTapAction::Pass;
                        }
                        let proxy = raw.proxy();
                        let Some(event) = macos_adapter::normalize_event(raw) else {
                            return macos_adapter::EventTapAction::Pass;
                        };
                        if let Some(action) = clipboard_hotkey.handle(&event) {
                            if matches!(action, ClipboardHotkeyAction::Show) {
                                if let Some(manager) = &clipboard {
                                    if !manager.try_show() {
                                        clipboard_hotkey.reset();
                                        return macos_adapter::EventTapAction::Pass;
                                    }
                                } else {
                                    clipboard_hotkey.reset();
                                    return macos_adapter::EventTapAction::Pass;
                                }
                            }
                            return macos_adapter::EventTapAction::Suppress;
                        }
                        let decision = runtime
                            .lock()
                            .map(|mut controller| {
                                controller.process_event_marked(&event, |outputs, marker| unsafe {
                                    macos_adapter::inject_outputs_at(outputs, marker, proxy)
                                })
                            })
                            .unwrap_or(Decision::Pass);
                        match decision {
                            Decision::Suppress => macos_adapter::EventTapAction::Suppress,
                            Decision::Pass | Decision::Emit(_) => {
                                macos_adapter::EventTapAction::Pass
                            }
                        }
                    });
                    let Ok(mut tap) = tap else {
                        let _ = ready_tx.send(Err(
                            "CGEventTap 创建失败，请检查辅助功能和输入监控权限".into(),
                        ));
                        return;
                    };
                    if let Err(error) = tap.start() {
                        let _ = ready_tx.send(Err(format!("Event Tap 启动失败: {error:?}")));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));
                    tap.run_loop();
                })
                .map_err(|error| error.to_string())?;
            match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Ok(())) => {
                    *self
                        .thread
                        .lock()
                        .map_err(|_| "event runtime lock poisoned")? = Some(thread);
                    Ok(())
                }
                Ok(Err(error)) => {
                    self.started.store(false, Ordering::Release);
                    let _ = thread.join();
                    Err(error)
                }
                Err(error) => {
                    self.started.store(false, Ordering::Release);
                    Err(format!("Event Tap 启动超时: {error}"))
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (runtime, clipboard);
            self.started.store(false, Ordering::Release);
            Err("Event Tap 仅支持 macOS".into())
        }
    }
}

#[cfg(target_os = "macos")]
fn event_mask() -> u64 {
    [1u32, 2, 3, 4, 5, 6, 7, 10, 11, 12, 22, 25, 26, 27]
        .into_iter()
        .fold(0u64, |mask, event_type| mask | (1u64 << event_type))
}

#[derive(Debug, Serialize)]
pub struct RuntimeView {
    pub status: RuntimeStatus,
    pub profile: String,
    pub events_seen: u64,
    pub injection_failures: u64,
    pub tap_state: String,
    pub permission: String,
    pub listen_access: bool,
    pub post_access: bool,
    pub event_runtime_started: bool,
    pub input_source: String,
    pub clipboard_status: String,
}

impl RuntimeController {
    pub fn new(rules: CompiledRules) -> Self {
        let adapter = SafeAdapter::new();
        let diagnostics = adapter.diagnostics();
        #[cfg(test)]
        let input_source = None;
        #[cfg(not(test))]
        let input_source = InputSourceService::start().ok();
        Self {
            engine: RuleEngine::new(rules),
            adapter,
            input_source,
            clipboard: None,
            diagnostics,
            profile: "windows_physical".into(),
            config: None,
            status: RuntimeStatus::Disabled,
        }
    }

    pub fn from_config(config: AppConfig) -> Result<Self, mac_to_windows_config::ConfigError> {
        let rules = config.compile_active_profile()?;
        let profile = config.active_profile.clone();
        let mut controller = Self::new(rules);
        controller.profile = profile;
        controller.config = Some(config);
        Ok(controller)
    }

    pub fn new_with_config(config: AppConfig) -> Result<Self, mac_to_windows_config::ConfigError> {
        Self::from_config(config)
    }

    pub fn status(&self) -> &RuntimeStatus {
        &self.status
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn diagnostics(&self) -> Arc<Diagnostics> {
        Arc::clone(&self.diagnostics)
    }

    pub fn config(&self) -> Option<&AppConfig> {
        self.config.as_ref()
    }

    pub fn set_clipboard(&mut self, manager: Arc<clipboard::ClipboardManager>) {
        self.clipboard = Some(manager);
    }

    pub fn apply(&mut self, command: RuntimeCommand) -> Result<Vec<OutputEvent>, RuntimeError> {
        match command {
            RuntimeCommand::Enable => {
                if let Err(error) = self.adapter.start() {
                    if matches!(error, InjectionError::PermissionDenied) {
                        self.status = RuntimeStatus::PermissionRequired;
                    }
                    return Err(RuntimeError::PermissionDenied);
                }
                self.status = RuntimeStatus::Running;
                if let Some(config) = self.config.as_mut() {
                    config.enabled = true;
                }
            }
            RuntimeCommand::Disable => {
                let released = self.engine.release_all();
                self.adapter.stop();
                self.status = RuntimeStatus::Disabled;
                if let Some(config) = self.config.as_mut() {
                    config.enabled = false;
                }
                return Ok(released);
            }
            RuntimeCommand::ReplaceRules(rules) => {
                let released = self.engine.release_all();
                self.engine.replace_rules(rules);
                return Ok(released);
            }
            RuntimeCommand::ReplaceConfig(config) => {
                let rules = config
                    .compile_active_profile()
                    .map_err(|error| RuntimeError::Configuration(error.to_string()))?;
                let released = self.engine.release_all();
                self.engine.replace_rules(rules);
                self.profile = config.active_profile.clone();
                self.config = Some(config);
                return Ok(released);
            }
            RuntimeCommand::SetProfile(profile) => {
                let mut config = self
                    .config
                    .clone()
                    .ok_or(RuntimeError::ProfileConfigurationUnavailable)?;
                config.active_profile = profile.clone();
                let rules = config
                    .compile_active_profile()
                    .map_err(|error| RuntimeError::Configuration(error.to_string()))?;
                let released = self.engine.release_all();
                self.engine.replace_rules(rules);
                self.config = Some(config);
                self.profile = profile;
                return Ok(released);
            }
            RuntimeCommand::ReloadPermissions => {
                let permission = self.adapter.request_permissions();
                if permission == PermissionState::Granted {
                    if self.status == RuntimeStatus::PermissionRequired {
                        self.status = RuntimeStatus::Disabled;
                    }
                } else {
                    self.adapter.stop();
                    self.status = RuntimeStatus::PermissionRequired;
                }
            }
            RuntimeCommand::ReleaseAll => return Ok(self.engine.release_all()),
            RuntimeCommand::Shutdown => {
                let released = self.engine.release_all();
                self.adapter.stop();
                self.status = RuntimeStatus::Stopped;
                if let Some(config) = self.config.as_mut() {
                    config.enabled = false;
                }
                return Ok(released);
            }
        }
        Ok(Vec::new())
    }

    pub fn process_event<F>(&mut self, event: &NormalizedEvent, inject: F) -> Decision
    where
        F: FnOnce(&[OutputEvent]) -> Result<(), InjectionError>,
    {
        self.process_event_marked(event, |outputs, _marker| inject(outputs))
    }

    pub fn process_event_marked<F>(&mut self, event: &NormalizedEvent, inject: F) -> Decision
    where
        F: FnOnce(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        self.diagnostics.record_seen();
        if let event_model::EventOrigin::Injected { marker } = event.origin {
            if self.adapter.should_filter_marker(marker) {
                self.diagnostics.record_passed();
                return Decision::Pass;
            }
        }
        if self.status != RuntimeStatus::Running {
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        if self
            .input_source
            .as_ref()
            .and_then(|service| service.take_last_error())
            .is_some()
        {
            self.diagnostics.record_injection_failure();
            self.diagnostics.set_tap_state(TapState::Degraded);
            self.status = RuntimeStatus::Degraded;
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        if is_emergency_hotkey(event) {
            if let Ok(outputs) = self.apply(RuntimeCommand::Disable) {
                let _ = macos_adapter::inject_outputs(&outputs, self.adapter.own_marker());
            }
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        let pinyin_toggle_available = self
            .input_source
            .as_ref()
            .map(|service| {
                let snapshot = service.snapshot();
                snapshot.current_is_pinyin
                    || (snapshot.current_id.is_some()
                        && snapshot.current_id == snapshot.last_latin_id
                        && snapshot.last_pinyin_id.is_some())
            })
            .unwrap_or(false);
        self.engine.set_pinyin_active(pinyin_toggle_available);
        let decision = self.engine.handle(event);
        match decision {
            Decision::Pass => {
                self.diagnostics.record_passed();
                Decision::Pass
            }
            Decision::Suppress => {
                self.diagnostics.record_suppressed();
                Decision::Suppress
            }
            Decision::Emit(outputs) => match self.dispatch_outputs(&outputs, inject) {
                Ok(()) => {
                    self.diagnostics.record_injected();
                    self.diagnostics.record_suppressed();
                    Decision::Suppress
                }
                Err(_) => {
                    let released = self.engine.release_all();
                    let _ = macos_adapter::inject_outputs(&released, self.adapter.own_marker());
                    self.diagnostics.record_injection_failure();
                    self.diagnostics.set_tap_state(TapState::Degraded);
                    self.status = RuntimeStatus::Degraded;
                    self.diagnostics.record_passed();
                    Decision::Pass
                }
            },
        }
    }

    fn dispatch_outputs<F>(&self, outputs: &[OutputEvent], inject: F) -> Result<(), InjectionError>
    where
        F: FnOnce(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let mut regular = Vec::new();
        let mut input_source_error = None;
        for output in outputs {
            match output {
                OutputEvent::SystemShortcut(SystemShortcut::SwitchInputSource) => {
                    if let Some(service) = &self.input_source {
                        if let Err(error) = service.request(InputSourceCommand::Next) {
                            input_source_error = Some(InjectionError::Failed(format!(
                                "input source switch failed: {error:?}"
                            )));
                        }
                    } else {
                        input_source_error = Some(InjectionError::Unavailable);
                    }
                }
                OutputEvent::SystemShortcut(SystemShortcut::TogglePinyinEnglish) => {
                    if let Some(service) = &self.input_source {
                        if let Err(error) = service.request(InputSourceCommand::TogglePinyinEnglish)
                        {
                            input_source_error = Some(InjectionError::Failed(format!(
                                "pinyin toggle failed: {error:?}"
                            )));
                        }
                    } else {
                        input_source_error = Some(InjectionError::Unavailable);
                    }
                }
                OutputEvent::SystemShortcut(SystemShortcut::ShowClipboard) => {
                    if let Some(manager) = &self.clipboard {
                        if !manager.try_show() {
                            input_source_error = Some(InjectionError::Unavailable);
                        }
                    } else {
                        input_source_error = Some(InjectionError::Unavailable);
                    }
                }
                other => regular.push(other.clone()),
            }
        }
        if let Some(error) = input_source_error {
            // The event tap must fall back to the original event when the
            // worker cannot complete the TIS request. Do not inject the
            // synthetic release collected alongside the shortcut, otherwise
            // returning Pass below would duplicate that release.
            Err(error)
        } else if !regular.is_empty() {
            inject(&regular, self.adapter.own_marker())
        } else {
            Ok(())
        }
    }
}

fn flush_outputs(runtime: &RuntimeController, outputs: &[OutputEvent]) -> Result<(), RuntimeError> {
    if outputs.is_empty() {
        return Ok(());
    }
    macos_adapter::inject_outputs(outputs, runtime.adapter.own_marker())
        .map_err(|error| RuntimeError::Injection(error.to_string()))
}

fn is_emergency_hotkey(event: &NormalizedEvent) -> bool {
    event.phase == event_model::EventPhase::Down
        && event.code == event_model::PhysicalCode::key(53)
        && event.modifiers.contains(
            event_model::ModifierSet::LEFT_CONTROL
                | event_model::ModifierSet::LEFT_OPTION
                | event_model::ModifierSet::LEFT_COMMAND,
        )
}

fn append_keyboard_rule(
    config: &mut AppConfig,
    rule: mac_to_windows_config::Rule,
) -> Result<(), String> {
    let mut candidate = config.clone();
    let profile_name = candidate.active_profile.clone();
    let profile = candidate
        .profiles
        .get_mut(&profile_name)
        .ok_or_else(|| format!("active profile does not exist: {profile_name}"))?;
    profile.keyboard_rules.push(rule);
    candidate
        .compile_active_profile()
        .map_err(|error| error.to_string())?;
    *config = candidate;
    Ok(())
}

fn clipboard_history_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Application Support/MacToWindows/clipboard-history.json")
}

pub fn run() {
    let config = AppConfig::default();
    let app_state = AppState::from_config(config).unwrap_or_else(|_| {
        AppState::new(CompiledRules::compile(Vec::<Rule>::new()).expect("empty rules"))
    });
    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::toggle_mode,
            commands::runtime_status,
            commands::set_profile,
            commands::release_all,
            commands::emergency_disable,
            commands::add_rule,
            commands::export_config,
            commands::import_config,
            commands::reload_permissions,
            commands::open_privacy_settings,
            commands::clipboard_history,
            commands::select_clipboard_item,
            commands::delete_clipboard_item,
            commands::clear_clipboard_history,
            commands::close_clipboard_panel
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            WebviewWindowBuilder::new(app, "clipboard", WebviewUrl::App("clipboard.html".into()))
                .title("剪贴板历史")
                .inner_size(420.0, 520.0)
                .visible(false)
                .decorations(false)
                .resizable(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .center()
                .build()?;
            let manager =
                clipboard::ClipboardManager::start(app.handle().clone(), clipboard_history_path())
                    .map_err(|error| error.to_string())?;
            let state = app.state::<AppState>();
            state.set_clipboard(Arc::clone(&manager));
            let _ = state
                .event_runtime
                .start(Arc::clone(&state.runtime), Some(manager));
            let open = MenuItem::with_id(app, "open", "打开设置", true, None::<&str>)?;
            let toggle = MenuItem::with_id(app, "toggle", "启用 Windows 模式", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &toggle, &quit])?;
            TrayIconBuilder::new()
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "toggle" => {
                        let _ = commands::toggle_mode(app.state::<AppState>());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running MacToWindows");
}

pub mod commands {
    use super::*;

    #[tauri::command]
    pub fn toggle_mode(state: tauri::State<'_, AppState>) -> Result<bool, String> {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        match runtime.status() {
            RuntimeStatus::Running => {
                let outputs = runtime
                    .apply(RuntimeCommand::Disable)
                    .map_err(|error| format!("{error:?}"))?;
                flush_outputs(&runtime, &outputs).map_err(|error| format!("{error:?}"))?;
                Ok(false)
            }
            RuntimeStatus::Disabled
            | RuntimeStatus::Degraded
            | RuntimeStatus::PermissionRequired => {
                runtime
                    .apply(RuntimeCommand::Enable)
                    .map_err(|error| format!("{error:?}"))?;
                drop(runtime);
                if let Err(error) = state
                    .event_runtime
                    .start(Arc::clone(&state.runtime), state.clipboard())
                {
                    let mut runtime = state
                        .runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_owned())?;
                    let _ = runtime.apply(RuntimeCommand::Disable);
                    return Err(error);
                }
                Ok(true)
            }
            RuntimeStatus::Stopped => Err("runtime has stopped".into()),
        }
    }

    #[tauri::command]
    pub fn runtime_status(state: tauri::State<'_, AppState>) -> Result<RuntimeView, String> {
        let runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let snapshot = runtime.diagnostics().snapshot();
        let (listen_access, post_access) = runtime.adapter.permission_access();
        let input_source = runtime
            .input_source
            .as_ref()
            .and_then(|service| service.snapshot().current_id)
            .unwrap_or_else(|| "未知".into());
        let clipboard_status = state
            .clipboard()
            .and_then(|manager| manager.snapshot().last_error)
            .unwrap_or_else(|| "正常".into());
        let tap_state = if state.event_runtime.is_started()
            && matches!(snapshot.tap_state, TapState::Stopped)
        {
            "Running".to_owned()
        } else {
            format!("{:?}", snapshot.tap_state)
        };
        Ok(RuntimeView {
            status: runtime.status().clone(),
            profile: runtime.profile().to_owned(),
            events_seen: snapshot.events_seen,
            injection_failures: snapshot.injection_failures,
            tap_state,
            permission: format!("{:?}", snapshot.permission),
            listen_access,
            post_access,
            event_runtime_started: state.event_runtime.is_started(),
            input_source,
            clipboard_status,
        })
    }

    #[tauri::command]
    pub fn set_profile(state: tauri::State<'_, AppState>, profile: String) -> Result<(), String> {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let outputs = runtime
            .apply(RuntimeCommand::SetProfile(profile))
            .map_err(|error| format!("{error:?}"))?;
        flush_outputs(&runtime, &outputs).map_err(|error| format!("{error:?}"))?;
        Ok(())
    }

    #[tauri::command]
    pub fn release_all(state: tauri::State<'_, AppState>) -> Result<usize, String> {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let outputs = runtime
            .apply(RuntimeCommand::ReleaseAll)
            .map_err(|error| format!("{error:?}"))?;
        flush_outputs(&runtime, &outputs).map_err(|error| format!("{error:?}"))?;
        Ok(outputs.len())
    }

    #[tauri::command]
    pub fn emergency_disable(state: tauri::State<'_, AppState>) -> Result<(), String> {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let outputs = runtime
            .apply(RuntimeCommand::Disable)
            .map_err(|error| format!("{error:?}"))?;
        flush_outputs(&runtime, &outputs).map_err(|error| format!("{error:?}"))?;
        Ok(())
    }

    #[tauri::command]
    pub fn add_rule(
        state: tauri::State<'_, AppState>,
        trigger: String,
        action: String,
        enabled: bool,
        priority: i32,
    ) -> Result<(), String> {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let mut config = runtime
            .config()
            .cloned()
            .ok_or_else(|| "运行时没有可编辑的配置".to_owned())?;
        append_keyboard_rule(
            &mut config,
            mac_to_windows_config::Rule {
                trigger,
                action,
                enabled,
                priority,
                ..mac_to_windows_config::Rule::default()
            },
        )?;
        let outputs = runtime
            .apply(RuntimeCommand::ReplaceConfig(config))
            .map_err(|error| format!("{error:?}"))?;
        flush_outputs(&runtime, &outputs).map_err(|error| format!("{error:?}"))?;
        Ok(())
    }

    #[tauri::command]
    pub fn reload_permissions(state: tauri::State<'_, AppState>) -> Result<RuntimeStatus, String> {
        let status = {
            let mut runtime = state
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_owned())?;
            runtime
                .apply(RuntimeCommand::ReloadPermissions)
                .map_err(|error| format!("{error:?}"))?;
            runtime.status().clone()
        };
        if !state.event_runtime.is_started() {
            let _ = state
                .event_runtime
                .start(Arc::clone(&state.runtime), state.clipboard());
        }
        Ok(status)
    }

    #[tauri::command]
    pub fn clipboard_history(
        state: tauri::State<'_, AppState>,
    ) -> Result<clipboard::ClipboardSnapshot, String> {
        state
            .clipboard()
            .map(|manager| manager.snapshot())
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())
    }

    #[tauri::command]
    pub fn select_clipboard_item(state: tauri::State<'_, AppState>, id: u64) -> Result<(), String> {
        state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?
            .select(id)
            .map_err(|error| error.to_string())
    }

    #[tauri::command]
    pub fn delete_clipboard_item(state: tauri::State<'_, AppState>, id: u64) -> Result<(), String> {
        state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?
            .delete(id)
            .map_err(|error| error.to_string())
    }

    #[tauri::command]
    pub fn clear_clipboard_history(state: tauri::State<'_, AppState>) -> Result<(), String> {
        state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?
            .clear()
            .map_err(|error| error.to_string())
    }

    #[tauri::command]
    pub fn close_clipboard_panel(state: tauri::State<'_, AppState>) -> Result<(), String> {
        state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?
            .close_panel()
            .map_err(|error| error.to_string())
    }

    #[tauri::command]
    pub fn open_privacy_settings() -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg("x-apple.systempreferences:com.apple.preference.security")
                .status()
                .map_err(|error| format!("无法打开系统设置: {error}"))?
                .success()
                .then_some(())
                .ok_or_else(|| "系统设置未能打开".to_owned())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err("打开系统设置仅支持 macOS".to_owned())
        }
    }

    #[tauri::command]
    pub fn export_config(state: tauri::State<'_, AppState>) -> Result<String, String> {
        let runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let config = runtime.config().cloned().unwrap_or_default();
        mac_to_windows_config::export_toml(&config).map_err(|error| error.to_string())
    }

    #[tauri::command]
    pub fn import_config(
        state: tauri::State<'_, AppState>,
        contents: String,
    ) -> Result<(), String> {
        let config =
            mac_to_windows_config::import_toml(&contents).map_err(|error| error.to_string())?;
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        let outputs = runtime
            .apply(RuntimeCommand::ReplaceConfig(config))
            .map_err(|error| format!("{error:?}"))?;
        flush_outputs(&runtime, &outputs).map_err(|error| format!("{error:?}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_model::{
        DeviceSource, EventKind, EventOrigin, EventPhase, ModifierSet, PhysicalCode,
    };
    use rule_engine::{Action, ModifierMatcher, RuleScope, Trigger};

    fn key() -> NormalizedEvent {
        NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind: EventKind::Keyboard,
            phase: EventPhase::Down,
            code: PhysicalCode::Key(0),
            modifiers: ModifierSet::NONE,
            location: None,
            scroll: None,
            timestamp_ns: 1,
            app_context: None,
            origin: EventOrigin::Hardware,
        }
    }

    fn controller_with_rule() -> RuntimeController {
        let rule = Rule {
            id: "a".into(),
            enabled: true,
            priority: 1,
            scope: RuleScope::Global,
            trigger: Trigger::Key {
                code: PhysicalCode::Key(0),
                modifiers: ModifierMatcher::default(),
                phase: EventPhase::Down,
            },
            action: Action::Replace(vec![OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE,
            }]),
        };
        RuntimeController::new(CompiledRules::compile(vec![rule]).unwrap())
    }

    #[test]
    fn enable_disable_release_and_shutdown_follow_safe_transitions() {
        let mut controller = controller_with_rule();
        assert_eq!(controller.status(), &RuntimeStatus::Disabled);
        controller.apply(RuntimeCommand::Enable).unwrap();
        assert_eq!(controller.status(), &RuntimeStatus::Running);
        controller.process_event(&key(), |_| Ok(()));
        let released = controller.apply(RuntimeCommand::Disable).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(controller.status(), &RuntimeStatus::Disabled);
        controller.apply(RuntimeCommand::Shutdown).unwrap();
        assert_eq!(controller.status(), &RuntimeStatus::Stopped);
    }

    #[test]
    fn injection_failure_passes_original_event_and_degrades_runtime() {
        let mut controller = controller_with_rule();
        controller.apply(RuntimeCommand::Enable).unwrap();
        assert_eq!(
            controller.process_event(&key(), |_| Err(InjectionError::Unavailable)),
            Decision::Pass
        );
        assert_eq!(controller.status(), &RuntimeStatus::Degraded);
        assert_eq!(controller.diagnostics().snapshot().injection_failures, 1);
    }

    #[test]
    fn emergency_hotkey_disables_mapping_before_rules_run() {
        let mut controller = controller_with_rule();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let panic = NormalizedEvent {
            phase: EventPhase::Down,
            code: PhysicalCode::key(53),
            modifiers: ModifierSet::LEFT_CONTROL
                | ModifierSet::LEFT_OPTION
                | ModifierSet::LEFT_COMMAND,
            ..key()
        };
        assert_eq!(controller.process_event(&panic, |_| Ok(())), Decision::Pass);
        assert_eq!(controller.status(), &RuntimeStatus::Disabled);
    }

    #[test]
    fn only_own_injected_marker_bypasses_rule_engine() {
        let mut controller = controller_with_rule();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let own = controller.adapter.own_marker();
        let own_event = NormalizedEvent {
            origin: event_model::EventOrigin::Injected { marker: own },
            ..key()
        };
        assert_eq!(
            controller.process_event(&own_event, |_| Err(InjectionError::Unavailable)),
            Decision::Pass
        );
        let foreign_event = NormalizedEvent {
            origin: event_model::EventOrigin::Injected {
                marker: own.wrapping_add(1),
            },
            ..key()
        };
        assert_eq!(
            controller.process_event(&foreign_event, |_| Ok(())),
            Decision::Suppress
        );
    }

    #[test]
    fn set_profile_compiles_and_switches_the_active_profile() {
        let mut config = AppConfig::default();
        config.profiles.insert(
            "custom".into(),
            mac_to_windows_config::Profile {
                keyboard_rules: vec![mac_to_windows_config::Rule {
                    trigger: "key:0".into(),
                    action: "key:2".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let mut controller = RuntimeController::from_config(config).unwrap();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let released = controller
            .apply(RuntimeCommand::SetProfile("custom".into()))
            .unwrap();
        assert!(released.is_empty());
        assert_eq!(controller.profile(), "custom");
        let mut injected = Vec::new();
        assert_eq!(
            controller.process_event(&key(), |outputs| {
                injected.extend_from_slice(outputs);
                Ok(())
            }),
            Decision::Suppress
        );
        assert_eq!(
            injected,
            vec![OutputEvent::Key {
                code: PhysicalCode::key(2),
                phase: EventPhase::Down,
                modifiers: ModifierSet::NONE,
            }]
        );
    }

    #[test]
    fn set_profile_keeps_previous_rules_when_compilation_fails() {
        let mut config = AppConfig::default();
        config.profiles.insert(
            "broken".into(),
            mac_to_windows_config::Profile {
                keyboard_rules: vec![mac_to_windows_config::Rule {
                    trigger: "not-a-code".into(),
                    action: "key:2".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let mut controller = RuntimeController::from_config(config).unwrap();
        let error = controller
            .apply(RuntimeCommand::SetProfile("broken".into()))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::Configuration(_)));
        assert_eq!(controller.profile(), "windows_physical");
    }

    #[test]
    fn adding_a_custom_rule_updates_the_active_profile() {
        let mut config = AppConfig::default();
        append_keyboard_rule(
            &mut config,
            mac_to_windows_config::Rule {
                trigger: "chord:left_control+key:8".into(),
                action: "command:key:8".into(),
                ..mac_to_windows_config::Rule::default()
            },
        )
        .unwrap();

        let profile = config.profiles.get(&config.active_profile).unwrap();
        assert!(profile
            .keyboard_rules
            .iter()
            .any(|rule| rule.trigger == "chord:left_control+key:8"));
        assert!(config.compile_active_profile().is_ok());
    }

    #[test]
    fn app_state_exposes_an_event_runtime_lifecycle() {
        let config = AppConfig::default();
        let state = AppState::from_config(config).unwrap();
        assert!(!state.event_runtime.is_started());
    }

    #[test]
    fn clipboard_hotkey_suppresses_left_option_v_down_up_and_repeat() {
        let mut latch = ClipboardHotkeyLatch::default();
        let down = NormalizedEvent {
            code: PhysicalCode::key(9),
            phase: EventPhase::Down,
            modifiers: ModifierSet::LEFT_OPTION,
            ..key()
        };
        let repeat = NormalizedEvent {
            phase: EventPhase::Repeat,
            ..down.clone()
        };
        let up = NormalizedEvent {
            phase: EventPhase::Up,
            ..down.clone()
        };
        assert_eq!(latch.handle(&down), Some(ClipboardHotkeyAction::Show));
        assert_eq!(latch.handle(&repeat), Some(ClipboardHotkeyAction::Suppress));
        assert_eq!(latch.handle(&up), Some(ClipboardHotkeyAction::Suppress));
    }

    #[test]
    fn clipboard_hotkey_ignores_right_option_v_and_plain_option() {
        let mut latch = ClipboardHotkeyLatch::default();
        let right = NormalizedEvent {
            code: PhysicalCode::key(9),
            phase: EventPhase::Down,
            modifiers: ModifierSet::RIGHT_OPTION,
            ..key()
        };
        let plain = NormalizedEvent {
            code: PhysicalCode::key(9),
            phase: EventPhase::Down,
            modifiers: ModifierSet::NONE,
            ..key()
        };
        assert_eq!(latch.handle(&right), None);
        assert_eq!(latch.handle(&plain), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires exclusive macOS Event Tap access; run explicitly for hardware validation"]
    fn event_runtime_starts_a_real_core_graphics_tap() {
        let state = AppState::from_config(AppConfig::default()).unwrap();
        state
            .event_runtime
            .start(Arc::clone(&state.runtime), None)
            .expect("Event Tap should start with macOS input permissions");
        assert!(state.event_runtime.is_started());
    }
}
