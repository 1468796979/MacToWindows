use diagnostics::{Diagnostics, PermissionState, TapState};
use event_model::{Decision, NormalizedEvent, OutputEvent, SystemShortcut};
use mac_to_windows_config::AppConfig;
use macos_adapter::{
    EventAdapter, InjectionError, InputSourceCommand, InputSourceService, SafeAdapter,
};
use rule_engine::{CompiledRules, RuleEngine};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

mod clipboard;
mod output_ownership;
mod settings;
mod software_keyboard;
#[cfg(test)]
mod software_keyboard_tests;
#[cfg(all(test, target_os = "macos"))]
mod software_native_tests;
use output_ownership::{InputStream, OutputOwner, OutputOwnership};
use software_keyboard::{SoftwareResult, SoftwareSession};

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
    left_option_down: bool,
    right_option_down: bool,
    option_state_seen: bool,
}

impl ClipboardHotkeyLatch {
    pub fn reset(&mut self) {
        self.active = false;
        self.left_option_down = false;
        self.right_option_down = false;
        self.option_state_seen = false;
    }

    pub fn handle(&mut self, event: &NormalizedEvent) -> Option<ClipboardHotkeyAction> {
        if event.kind == event_model::EventKind::FlagsChanged
            && event.source == event_model::DeviceSource::Keyboard
            && !matches!(event.origin, event_model::EventOrigin::TextInjection { .. })
            && matches!(event.code, event_model::PhysicalCode::Key(58 | 61))
        {
            self.option_state_seen = true;
            match event.code {
                event_model::PhysicalCode::Key(58) => {
                    self.left_option_down = event
                        .modifiers
                        .contains(event_model::ModifierSet::LEFT_OPTION);
                }
                event_model::PhysicalCode::Key(61) => {
                    self.right_option_down = event
                        .modifiers
                        .contains(event_model::ModifierSet::RIGHT_OPTION);
                }
                _ => {}
            }
            return None;
        }
        if event.source != event_model::DeviceSource::Keyboard
            || event.code != event_model::PhysicalCode::key(9)
            || matches!(event.origin, event_model::EventOrigin::TextInjection { .. })
        {
            return None;
        }
        let left_option_active = if self.option_state_seen {
            self.left_option_down && !self.right_option_down
        } else {
            event
                .modifiers
                .contains(event_model::ModifierSet::LEFT_OPTION)
                && !event
                    .modifiers
                    .contains(event_model::ModifierSet::RIGHT_OPTION)
        };
        match event.phase {
            event_model::EventPhase::Down if left_option_active => {
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
    active_rules: CompiledRules,
    software: HashMap<macos_adapter::KeyboardSourceId, SoftwareSession>,
    output_ownership: OutputOwnership,
    output_transaction: u64,
    adapter: SafeAdapter,
    input_source: Option<InputSourceService>,
    clipboard: Option<Arc<clipboard::ClipboardManager>>,
    diagnostics: Arc<Diagnostics>,
    external_keyboard_pending: Arc<AtomicBool>,
    profile: String,
    config: Option<AppConfig>,
    status: RuntimeStatus,
    config_revision: u64,
    pending_releases: Vec<OutputEvent>,
    last_error: Option<String>,
}

pub struct AppState {
    pub runtime: Arc<Mutex<RuntimeController>>,
    pub event_runtime: EventRuntime,
    clipboard: Arc<Mutex<Option<Arc<clipboard::ClipboardManager>>>>,
    settings: settings::SettingsStore,
    shutdown_started: AtomicBool,
    shutdown_complete: AtomicBool,
}

impl AppState {
    pub fn new(rules: CompiledRules) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(RuntimeController::new(rules))),
            event_runtime: EventRuntime::default(),
            clipboard: Arc::new(Mutex::new(None)),
            settings: settings::SettingsStore::default(),
            shutdown_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
        }
    }

    pub fn from_config(config: AppConfig) -> Result<Self, mac_to_windows_config::ConfigError> {
        Ok(Self {
            runtime: Arc::new(Mutex::new(RuntimeController::from_config(config)?)),
            event_runtime: EventRuntime::default(),
            clipboard: Arc::new(Mutex::new(None)),
            settings: settings::SettingsStore::default(),
            shutdown_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
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

    pub fn runtime_marker(&self) -> u64 {
        self.runtime
            .lock()
            .map(|runtime| runtime.own_marker())
            .unwrap_or_else(|_| macos_adapter::next_marker())
    }
}

#[derive(Default)]
pub struct EventRuntime {
    started: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
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
        self.stop.store(false, Ordering::Release);
        #[cfg(target_os = "macos")]
        {
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let stopped = Arc::clone(&self.stop);
            let started = Arc::clone(&self.started);
            let thread = std::thread::Builder::new()
                .name("mac-to-windows-event-tap".into())
                .spawn(move || {
                    let callback_runtime = Arc::clone(&runtime);
                    let (marker, diagnostics, external_keyboard_pending) = runtime
                        .lock()
                        .map(|r| {
                            (
                                r.own_marker(),
                                r.diagnostics(),
                                Arc::clone(&r.external_keyboard_pending),
                            )
                        })
                        .unwrap_or_else(|_| {
                            (
                                0,
                                Arc::new(Diagnostics::new()),
                                Arc::new(AtomicBool::new(false)),
                            )
                        });
                    let mut normalizer = macos_adapter::EventNormalizer::new(marker);
                    let mut clipboard_hotkey = ClipboardHotkeyLatch::default();
                    let keyboard_sources = macos_adapter::KeyboardSourceCache::start();
                    let mut keyboard_routes = macos_adapter::KeyboardRoutingState::default();
                    let tap = macos_adapter::EventTap::create(event_mask(), move |raw| {
                        if raw.is_tap_disabled() {
                            normalizer.reset();
                            clipboard_hotkey.reset();
                            keyboard_routes.reset();
                            if let Ok(mut controller) = callback_runtime.lock() {
                                controller.reset_keyboard_continuity();
                                if controller.status == RuntimeStatus::Running {
                                    controller.degrade_with(
                                        "Event Tap 已被系统禁用，请重新启用映射".into(),
                                        &mut macos_adapter::inject_outputs,
                                    );
                                }
                            }
                            return macos_adapter::EventTapAction::Pass;
                        }
                        if let Some(action) = route_keyboard_source(
                            raw,
                            marker,
                            &keyboard_sources,
                            &mut keyboard_routes,
                            &callback_runtime,
                            &diagnostics,
                            &external_keyboard_pending,
                            |outputs, own| unsafe {
                                macos_adapter::inject_outputs_with_context(
                                    outputs,
                                    own,
                                    raw.proxy(),
                                    raw.raw_event(),
                                )
                            },
                        ) {
                            return action;
                        }
                        let Some(event) = normalizer.normalize(raw) else {
                            return macos_adapter::EventTapAction::Pass;
                        };
                        if let Some(action) = clipboard_hotkey.handle(&event) {
                            if action == ClipboardHotkeyAction::Show {
                                if let Some(manager) = &clipboard {
                                    let _ = manager.try_show();
                                }
                            }
                            return macos_adapter::EventTapAction::Suppress;
                        }
                        let decision = callback_runtime
                            .lock()
                            .map(|mut controller| {
                                let decision = controller.process_event_marked(
                                    &event,
                                    |outputs, marker| unsafe {
                                        macos_adapter::inject_outputs_with_context(
                                            outputs,
                                            marker,
                                            raw.proxy(),
                                            raw.raw_event(),
                                        )
                                    },
                                );
                                if decision == Decision::Pass {
                                    if let Some(metadata) = raw.keyboard_metadata() {
                                        let flags =
                                            controller.output_ownership.modifiers_with_preserved(
                                                InputStream::Local,
                                                metadata.modifiers,
                                            );
                                        if flags != metadata.modifiers {
                                            let _ = unsafe {
                                                macos_adapter::set_keyboard_modifiers(raw, flags)
                                            };
                                        }
                                    }
                                }
                                decision
                            })
                            .unwrap_or(Decision::Pass);
                        if decision == Decision::Suppress {
                            macos_adapter::EventTapAction::Suppress
                        } else {
                            macos_adapter::EventTapAction::Pass
                        }
                    });
                    match tap {
                        Ok(mut tap) => match tap.start() {
                            Ok(()) => {
                                let _ = ready_tx.send(Ok(()));
                                tap.run_loop_until(&stopped);
                                // Do not queue global cleanup behind a tap whose run loop has stopped.
                                tap.stop();
                                if let Ok(mut controller) = runtime.lock() {
                                    if let Ok(outputs) = controller.apply(RuntimeCommand::Shutdown)
                                    {
                                        let _ = flush_outputs(&mut controller, &outputs);
                                    }
                                }
                            }
                            Err(error) => {
                                let _ =
                                    ready_tx.send(Err(format!("Event Tap 启动失败：{error:?}")));
                            }
                        },
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!("Event Tap 创建失败：{error:?}")));
                        }
                    }
                    started.store(false, Ordering::Release);
                })
                .map_err(|error| {
                    self.started.store(false, Ordering::Release);
                    error.to_string()
                })?;
            *self
                .thread
                .lock()
                .map_err(|_| "event runtime lock poisoned")? = Some(thread);
            match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(result) => result,
                Err(error) => {
                    self.stop.store(true, Ordering::Release);
                    Err(format!("Event Tap 启动超时：{error}"))
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

    pub fn shutdown(&self, runtime: &Mutex<RuntimeController>) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        let thread = self
            .thread
            .lock()
            .map_err(|_| "event runtime lock poisoned")?
            .take();
        if let Some(thread) = thread {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !thread.is_finished() {
                if std::time::Instant::now() >= deadline {
                    *self
                        .thread
                        .lock()
                        .map_err(|_| "event runtime lock poisoned")? = Some(thread);
                    return Err("等待事件线程退出超时".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            thread.join().map_err(|_| "事件线程异常退出".to_owned())?;
        }
        let mut controller = runtime.lock().map_err(|_| "runtime lock poisoned")?;
        let outputs = controller
            .apply(RuntimeCommand::Shutdown)
            .map_err(|e| format!("{e:?}"))?;
        flush_outputs(&mut controller, &outputs).map_err(|e| format!("{e:?}"))
    }
}

#[cfg(target_os = "macos")]
fn route_keyboard_source<F>(
    raw: macos_adapter::EventTapEvent,
    marker: u64,
    sources: &macos_adapter::KeyboardSourceCache,
    routes: &mut macos_adapter::KeyboardRoutingState,
    runtime: &Mutex<RuntimeController>,
    diagnostics: &Diagnostics,
    pending: &AtomicBool,
    inject: F,
) -> Option<macos_adapter::EventTapAction>
where
    F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
{
    if !matches!(raw.event_type(), 10..=12) {
        return None;
    }
    let Some(metadata) = raw.keyboard_metadata() else {
        return route_keyboard_input(raw, marker, runtime, diagnostics, pending);
    };
    route_keyboard_classified(
        raw,
        metadata,
        sources.classify(&metadata, marker),
        marker,
        routes,
        runtime,
        diagnostics,
        pending,
        inject,
    )
}

#[cfg(target_os = "macos")]
fn route_keyboard_classified<F>(
    raw: macos_adapter::EventTapEvent,
    metadata: macos_adapter::KeyboardMetadata,
    source: macos_adapter::KeyboardSource,
    marker: u64,
    routes: &mut macos_adapter::KeyboardRoutingState,
    runtime: &Mutex<RuntimeController>,
    diagnostics: &Diagnostics,
    pending: &AtomicBool,
    mut inject: F,
) -> Option<macos_adapter::EventTapAction>
where
    F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
{
    use macos_adapter::{EventTapAction, KeyboardSourceKind};
    if source.kind == KeyboardSourceKind::OwnInjected {
        return route_keyboard_input(raw, marker, runtime, diagnostics, pending);
    }
    let effective = routes.current_source(&metadata, source);
    if matches!(
        effective.kind,
        KeyboardSourceKind::Software | KeyboardSourceKind::Unknown
    ) && software_keyboard::protects_text(&metadata)
    {
        pass_external_keyboard(runtime, diagnostics, pending);
        if let Ok(mut controller) = runtime.try_lock() {
            controller.observe_text_source(source.id, &mut inject);
        }
        return Some(EventTapAction::Pass);
    }
    let observed_identity = source.id;
    let source = routes.route(&metadata, source);
    match source.kind {
        KeyboardSourceKind::HardwareLike | KeyboardSourceKind::OwnInjected => {
            route_keyboard_input(raw, marker, runtime, diagnostics, pending)
        }
        // Deskflow can change injection paths between the Down and Up of
        // one key. Both paths must share the legacy HID/local key ledger.
        KeyboardSourceKind::Deskflow | KeyboardSourceKind::Unknown => None,
        KeyboardSourceKind::Software => {
            let result = runtime
                .lock()
                .map(|mut controller| {
                    let mut identity = source.id;
                    if identity.process_start == 0 {
                        identity.process_start = observed_identity.process_start;
                    }
                    let mut result =
                        controller.process_software_raw(identity, raw, false, &mut inject);
                    if result.decision == Decision::Pass {
                        let base = result.original_modifiers.unwrap_or(metadata.modifiers);
                        let flags = base | controller.software_preserved_modifiers(identity);
                        if flags != metadata.modifiers {
                            result.original_modifiers = Some(flags);
                        }
                    }
                    result
                })
                .unwrap_or(SoftwareResult {
                    decision: Decision::Pass,
                    original_modifiers: None,
                });
            if let Some(modifiers) = result.original_modifiers {
                // The original event keeps its keycode, phase and text payload.
                let _ = unsafe { macos_adapter::set_keyboard_modifiers(raw, modifiers) };
            }
            Some(if result.decision == Decision::Suppress {
                EventTapAction::Suppress
            } else {
                EventTapAction::Pass
            })
        }
    }
}

fn pass_external_keyboard(
    runtime: &Mutex<RuntimeController>,
    diagnostics: &Diagnostics,
    pending: &AtomicBool,
) {
    diagnostics.record_seen();
    diagnostics.record_passed();
    diagnostics.record_external_keyboard_passed();
    pending.store(true, Ordering::Release);
    if let Ok(mut controller) = runtime.try_lock() {
        // An entry with an injector must also finish pending switch sessions.
        controller.interrupt_keyboard_gestures();
    }
}

/// Keep known text-injection channels out of every physical-key state machine.
/// Ordinary keycode streams (including Deskflow) keep their existing mappings.
/// Returning Pass preserves the original CGEvent and its Unicode payload.
#[cfg(target_os = "macos")]
fn route_keyboard_input(
    raw: macos_adapter::EventTapEvent,
    marker: u64,
    runtime: &Mutex<RuntimeController>,
    diagnostics: &Diagnostics,
    external_keyboard_pending: &AtomicBool,
) -> Option<macos_adapter::EventTapAction> {
    use macos_adapter::{EventTapAction, KeyboardEventRoute};
    match raw.keyboard_route(marker)? {
        KeyboardEventRoute::Mappable => None,
        KeyboardEventRoute::OwnInjected => {
            diagnostics.record_seen();
            diagnostics.record_passed();
            Some(EventTapAction::Pass)
        }
        KeyboardEventRoute::TextInjection | KeyboardEventRoute::Unknown => {
            pass_external_keyboard(runtime, diagnostics, external_keyboard_pending);
            // Neither a busy nor a poisoned runtime may delay remote text.
            Some(EventTapAction::Pass)
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
    pub external_keyboard_events_passed: u64,
    pub injection_failures: u64,
    pub tap_state: String,
    pub permission: String,
    pub listen_access: bool,
    pub post_access: bool,
    pub event_runtime_started: bool,
    pub input_source: String,
    pub clipboard_status: String,
    pub last_error: Option<String>,
    pub pending_releases: usize,
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
            engine: RuleEngine::new(rules.clone()),
            active_rules: rules,
            software: HashMap::new(),
            output_ownership: OutputOwnership::default(),
            output_transaction: 0,
            adapter,
            input_source,
            clipboard: None,
            diagnostics,
            external_keyboard_pending: Arc::new(AtomicBool::new(false)),
            profile: "windows_physical".into(),
            config: None,
            status: RuntimeStatus::Disabled,
            config_revision: 0,
            pending_releases: Vec::new(),
            last_error: None,
        }
    }

    pub fn from_config(config: AppConfig) -> Result<Self, mac_to_windows_config::ConfigError> {
        config.validate_desktop()?;
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

    pub fn own_marker(&self) -> u64 {
        self.adapter.own_marker()
    }

    pub fn config(&self) -> Option<&AppConfig> {
        self.config.as_ref()
    }

    pub fn set_clipboard(&mut self, manager: Arc<clipboard::ClipboardManager>) {
        self.clipboard = Some(manager);
    }

    fn install_config(&mut self, config: AppConfig, rules: CompiledRules) {
        self.replace_active_rules(rules);
        self.profile = config.active_profile.clone();
        self.config = Some(config);
        self.config_revision += 1;
    }

    fn transition(&mut self, status: RuntimeStatus) -> Vec<OutputEvent> {
        if matches!(
            status,
            RuntimeStatus::PermissionRequired | RuntimeStatus::Stopped
        ) {
            self.reset_keyboard_continuity();
        }
        let mut releases = std::mem::take(&mut self.pending_releases);
        let local = self.engine.release_all();
        let mut all = self
            .output_ownership
            .filter(OutputOwner::Mapped(InputStream::Local), &local);
        for (id, session) in &mut self.software {
            let stream = InputStream::Software(*id);
            all.extend(
                self.output_ownership
                    .filter(OutputOwner::Mapped(stream), &session.engine.release_all()),
            );
            all.extend(self.output_ownership.filter(
                OutputOwner::ApplicationSwitch(stream),
                &session.switcher.release(event_model::ModifierSet::NONE),
            ));
        }
        for output in all {
            if !releases.contains(&output) {
                releases.push(output);
            }
        }
        self.adapter.stop();
        self.status = status;
        if matches!(
            self.status,
            RuntimeStatus::Degraded | RuntimeStatus::PermissionRequired
        ) {
            self.diagnostics.set_tap_state(TapState::Degraded);
        }
        releases
    }

    fn flush_with<F>(
        &mut self,
        outputs: &[OutputEvent],
        mut inject: F,
    ) -> Result<(), InjectionError>
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        if outputs.is_empty() {
            return Ok(());
        }
        #[cfg(not(test))]
        let batch = prepare_cleanup_outputs(
            outputs,
            macos_adapter::physical_modifier_snapshot(),
            macos_adapter::physical_code_is_down,
        );
        #[cfg(test)]
        let batch = prepare_cleanup_outputs(outputs, event_model::ModifierSet::NONE, |_| false);
        if batch.is_empty() {
            self.pending_releases.clear();
            return Ok(());
        }
        match inject(&batch, self.own_marker()) {
            Ok(()) => {
                self.pending_releases.clear();
                Ok(())
            }
            Err(error) => {
                for output in outputs {
                    if !self.pending_releases.contains(output) {
                        self.pending_releases.push(output.clone());
                    }
                }
                self.last_error = Some(format!("按键释放尚未完成：{error}"));
                self.diagnostics.record_injection_failure();
                Err(error)
            }
        }
    }

    fn degrade_with<F>(&mut self, message: String, inject: &mut F)
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let outputs = self.transition(RuntimeStatus::Degraded);
        self.last_error = Some(message);
        self.diagnostics.record_injection_failure();
        let _ = self.flush_with(&outputs, inject);
    }

    fn permissions_refreshed(&mut self, permission: PermissionState) -> Vec<OutputEvent> {
        self.adapter.set_permission(permission);
        if permission != PermissionState::Granted {
            self.last_error = Some("输入权限未就绪，映射已暂停".into());
            return self.transition(RuntimeStatus::PermissionRequired);
        }
        if self.status == RuntimeStatus::PermissionRequired {
            self.status = RuntimeStatus::Disabled;
        }
        std::mem::take(&mut self.pending_releases)
    }

    pub fn apply(&mut self, command: RuntimeCommand) -> Result<Vec<OutputEvent>, RuntimeError> {
        match command {
            RuntimeCommand::Enable => {
                if !self.pending_releases.is_empty() {
                    let outputs = self.pending_releases.clone();
                    self.flush_with(&outputs, macos_adapter::inject_outputs)
                        .map_err(|e| RuntimeError::Injection(e.to_string()))?;
                }
                if self.adapter.start().is_err() {
                    let outputs = self.transition(RuntimeStatus::PermissionRequired);
                    self.pending_releases = outputs;
                    return Err(RuntimeError::PermissionDenied);
                }
                self.last_error = None;
                self.status = RuntimeStatus::Running;
            }
            RuntimeCommand::Disable => return Ok(self.transition(RuntimeStatus::Disabled)),
            RuntimeCommand::ReplaceRules(rules) => self.replace_active_rules(rules),
            RuntimeCommand::ReplaceConfig(config) => {
                config
                    .validate_desktop()
                    .map_err(|e| RuntimeError::Configuration(e.to_string()))?;
                let rules = config
                    .compile_active_profile()
                    .map_err(|e| RuntimeError::Configuration(e.to_string()))?;
                self.install_config(config, rules);
            }
            RuntimeCommand::SetProfile(profile) => {
                let mut config = self
                    .config
                    .clone()
                    .ok_or(RuntimeError::ProfileConfigurationUnavailable)?;
                config.active_profile = profile;
                config
                    .validate_desktop()
                    .map_err(|e| RuntimeError::Configuration(e.to_string()))?;
                let rules = config
                    .compile_active_profile()
                    .map_err(|e| RuntimeError::Configuration(e.to_string()))?;
                self.install_config(config, rules);
            }
            RuntimeCommand::ReloadPermissions => {
                let permission = self.adapter.request_permissions();
                return Ok(self.permissions_refreshed(permission));
            }
            RuntimeCommand::ReleaseAll => {
                let status = self.status.clone();
                let outputs = self.transition(status.clone());
                if status == RuntimeStatus::Running {
                    let _ = self.adapter.start();
                }
                return Ok(outputs);
            }
            RuntimeCommand::Shutdown => return Ok(self.transition(RuntimeStatus::Stopped)),
        }
        Ok(Vec::new())
    }

    pub fn process_event<F>(&mut self, event: &NormalizedEvent, mut inject: F) -> Decision
    where
        F: FnMut(&[OutputEvent]) -> Result<(), InjectionError>,
    {
        self.process_event_marked(event, |outputs, _marker| inject(outputs))
    }

    pub fn process_event_marked<F>(&mut self, event: &NormalizedEvent, mut inject: F) -> Decision
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        self.diagnostics.record_seen();
        if let Some(marker) = event.origin.injected_marker() {
            if self.adapter.should_filter_marker(marker) {
                self.diagnostics.record_passed();
                return Decision::Pass;
            }
        }
        if matches!(event.origin, event_model::EventOrigin::TextInjection { .. })
            && matches!(
                event.kind,
                event_model::EventKind::Keyboard | event_model::EventKind::FlagsChanged
            )
        {
            self.engine.interrupt_gestures();
            self.diagnostics.record_external_keyboard_passed();
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        self.consume_external_keyboard_activity();
        for session in self.software.values_mut() {
            session.engine.interrupt_gestures();
        }
        if !self.close_software_switchers(None, &mut inject) {
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        if is_emergency_hotkey(event) {
            let outputs = self.transition(RuntimeStatus::Disabled);
            let _ = self.flush_with(&outputs, &mut inject);
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        if self.status != RuntimeStatus::Running {
            self.output_ownership.native_pass(InputStream::Local, event);
            self.diagnostics.record_passed();
            return Decision::Pass;
        }
        if let Some(error) = self
            .input_source
            .as_ref()
            .and_then(|service| service.take_last_error())
        {
            self.degrade_with(format!("输入源服务异常：{error:?}"), &mut inject);
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
        let checkpoint = self.engine.checkpoint();
        let decision = self.engine.handle(event);
        match decision {
            Decision::Pass => {
                if !self.output_ownership.native_pass(InputStream::Local, event) {
                    self.diagnostics.record_suppressed();
                    return Decision::Suppress;
                }
                self.diagnostics.record_passed();
                Decision::Pass
            }
            Decision::Suppress => {
                self.diagnostics.record_suppressed();
                Decision::Suppress
            }
            Decision::Emit(outputs) => match self.dispatch_outputs_for(
                InputStream::Local,
                OutputOwner::Mapped(InputStream::Local),
                &outputs,
                self.engine.modifiers_for_event(event),
                &mut inject,
            ) {
                Ok(()) => {
                    self.diagnostics.record_injected();
                    self.diagnostics.record_suppressed();
                    Decision::Suppress
                }
                Err(error) => {
                    self.engine.restore(checkpoint);
                    self.degrade_with(format!("事件注入失败：{error}"), &mut inject);
                    self.diagnostics.record_passed();
                    Decision::Pass
                }
            },
        }
    }

    fn consume_external_keyboard_activity(&mut self) -> bool {
        if self.external_keyboard_pending.swap(false, Ordering::AcqRel) {
            self.engine.interrupt_gestures();
            for session in self.software.values_mut() {
                session.engine.interrupt_gestures();
            }
            true
        } else {
            false
        }
    }

    fn dispatch_outputs_for<F>(
        &mut self,
        stream: InputStream,
        owner: OutputOwner,
        outputs: &[OutputEvent],
        restore_flags: event_model::ModifierSet,
        mut inject: F,
    ) -> Result<(), InjectionError>
    where
        F: FnMut(&[OutputEvent], u64) -> Result<(), InjectionError>,
    {
        let mut regular = Vec::new();
        let mut ownership = self.output_ownership.clone();
        let mut restore_modifiers = false;
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
                OutputEvent::SystemShortcut(SystemShortcut::ShowDesktop) => {
                    restore_modifiers = true;
                    let config = self.config.clone().unwrap_or_default();
                    match config.show_desktop_output() {
                        Ok(outputs) => {
                            self.output_transaction = self.output_transaction.wrapping_add(1);
                            regular.extend(ownership.filter(
                                OutputOwner::Transient(stream, self.output_transaction),
                                &outputs,
                            ));
                        }
                        Err(error) => {
                            input_source_error = Some(InjectionError::Failed(error.to_string()))
                        }
                    }
                }
                OutputEvent::SystemShortcut(shortcut) => {
                    restore_modifiers = true;
                    self.output_transaction = self.output_transaction.wrapping_add(1);
                    match macos_adapter::expand_system_shortcut(*shortcut) {
                        Ok(outputs) => regular.extend(ownership.filter(
                            OutputOwner::Transient(stream, self.output_transaction),
                            &outputs,
                        )),
                        Err(error) => input_source_error = Some(error),
                    }
                }
                other => {
                    regular.extend(ownership.filter(owner, std::slice::from_ref(other)));
                }
            }
        }
        if restore_modifiers {
            regular.extend(event_model::modifier_restore_outputs(
                ownership.modifiers_with_preserved(stream, restore_flags),
            ));
        }
        if let Some(error) = input_source_error {
            // The event tap must fall back to the original event when the
            // worker cannot complete the TIS request. Do not inject the
            // synthetic release collected alongside the shortcut, otherwise
            // returning Pass below would duplicate that release.
            Err(error)
        } else if !regular.is_empty() {
            inject(&regular, self.adapter.own_marker())?;
            self.output_ownership = ownership;
            Ok(())
        } else {
            self.output_ownership = ownership;
            Ok(())
        }
    }
}

fn prepare_cleanup_outputs<F>(
    outputs: &[OutputEvent],
    native_modifiers: event_model::ModifierSet,
    mut physically_held: F,
) -> Vec<OutputEvent>
where
    F: FnMut(event_model::PhysicalCode) -> bool,
{
    let mut batch = Vec::new();
    for output in outputs {
        match output {
            OutputEvent::Key {
                code,
                phase: event_model::EventPhase::Up,
                ..
            } => {
                if !physically_held(*code) {
                    batch.push(OutputEvent::Key {
                        code: *code,
                        phase: event_model::EventPhase::Up,
                        modifiers: native_modifiers,
                    });
                }
            }
            OutputEvent::MouseButton {
                button,
                phase: event_model::EventPhase::Up,
            } => {
                if !physically_held(event_model::PhysicalCode::MouseButton(*button)) {
                    batch.push(output.clone());
                }
            }
            _ => batch.push(output.clone()),
        }
    }
    if !batch.is_empty() {
        batch.extend(event_model::modifier_restore_outputs(native_modifiers));
    }
    batch
}

fn flush_outputs(
    runtime: &mut RuntimeController,
    outputs: &[OutputEvent],
) -> Result<(), RuntimeError> {
    if outputs.is_empty() {
        return Ok(());
    }
    runtime
        .flush_with(outputs, macos_adapter::inject_outputs)
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

#[cfg(test)]
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

fn should_restore_main_window(has_visible_windows: bool) -> bool {
    !has_visible_windows
}

fn start_status_notifications(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut previous = None;
        loop {
            let state = app.state::<AppState>();
            if state.shutdown_started.load(Ordering::Acquire) {
                break;
            }
            // Emit away from the Event Tap callback; event counters are refreshed on demand.
            let current = state.runtime.lock().ok().map(|r| {
                (
                    r.status.clone(),
                    r.config_revision,
                    r.last_error.clone(),
                    r.pending_releases.len(),
                )
            });
            if current != previous {
                previous = current;
                let _ = app.emit("runtime-changed", ());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

fn begin_shutdown(app: &tauri::AppHandle, code: i32) {
    let state = app.state::<AppState>();
    if state.shutdown_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let worker_app = app.clone();
    std::thread::spawn(move || {
        let state = worker_app.state::<AppState>();
        if let Err(error) = state.event_runtime.shutdown(&state.runtime) {
            eprintln!("退出清理：{error}");
        }
        if let Some(manager) = state.clipboard() {
            if let Err(error) = manager.shutdown() {
                eprintln!("剪贴板退出清理：{error}");
            }
        }
        state.shutdown_complete.store(true, Ordering::Release);
        worker_app.exit(code);
    });
    let timer_app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if !timer_app
            .state::<AppState>()
            .shutdown_complete
            .swap(true, Ordering::AcqRel)
        {
            eprintln!("退出清理超时，按键释放或剪贴板持久化可能尚未完成");
            timer_app.exit(code);
        }
    });
}

pub fn run() {
    let config_path = clipboard_history_path().with_file_name("config.toml");
    let (config, error) = settings::load_startup(&config_path);
    let mut app_state = AppState::from_config(config).expect("validated startup configuration");
    app_state.settings.path = Some(config_path);
    *app_state.settings.error.lock().expect("new settings lock") = error;
    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::toggle_mode,
            commands::runtime_status,
            commands::settings_snapshot,
            commands::set_rule_enabled,
            commands::set_show_desktop_shortcut,
            commands::set_software_input_passthrough,
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
            if let Some(main_window) = app.get_webview_window("main") {
                let close_target = main_window.clone();
                main_window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = close_target.hide();
                    }
                });
            }
            WebviewWindowBuilder::new(app, "clipboard", WebviewUrl::App("clipboard.html".into()))
                .title("剪贴板历史")
                .inner_size(360.0, 460.0)
                .min_inner_size(320.0, 360.0)
                .visible(false)
                .decorations(false)
                .resizable(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .center()
                .build()?;
            start_status_notifications(app.handle().clone());
            let state = app.state::<AppState>();
            let manager = clipboard::ClipboardManager::start_with_marker(
                app.handle().clone(),
                clipboard_history_path(),
                state.runtime_marker(),
            )
            .map_err(|error| error.to_string())?;
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
                        let app = app.clone();
                        std::thread::spawn(move || {
                            let _ = commands::toggle_mode_impl(&app.state::<AppState>());
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building MacToWindows")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = &event {
                if !app
                    .state::<AppState>()
                    .shutdown_complete
                    .load(Ordering::Acquire)
                {
                    api.prevent_exit();
                    begin_shutdown(app, code.unwrap_or(0));
                }
            }
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = event
            {
                // The clipboard panel intentionally hides the settings
                // window while it is active. A Dock click must still reopen
                // the settings window when the app has no visible windows.
                if should_restore_main_window(has_visible_windows) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

fn runtime_view(state: &AppState, runtime: &RuntimeController) -> RuntimeView {
    let snapshot = runtime.diagnostics().snapshot();
    let (listen_access, post_access) = runtime.adapter.permission_access();
    let input_source = runtime
        .input_source
        .as_ref()
        .and_then(|service| service.snapshot().current_id)
        .unwrap_or_else(|| "未知".into());
    let clipboard_status = state
        .clipboard()
        .and_then(|manager| manager.last_error())
        .unwrap_or_else(|| "正常".into());
    let tap_state =
        if state.event_runtime.is_started() && matches!(snapshot.tap_state, TapState::Stopped) {
            "Running".to_owned()
        } else {
            format!("{:?}", snapshot.tap_state)
        };
    RuntimeView {
        status: runtime.status().clone(),
        profile: runtime.profile().to_owned(),
        events_seen: snapshot.events_seen,
        external_keyboard_events_passed: snapshot.external_keyboard_events_passed,
        injection_failures: snapshot.injection_failures,
        tap_state,
        permission: format!("{:?}", snapshot.permission),
        listen_access,
        post_access,
        event_runtime_started: state.event_runtime.is_started(),
        input_source,
        clipboard_status,
        last_error: runtime.last_error.clone(),
        pending_releases: runtime.pending_releases.len(),
    }
}

pub mod commands {
    use super::*;

    #[tauri::command]
    pub async fn toggle_mode(app: tauri::AppHandle) -> Result<bool, String> {
        tauri::async_runtime::spawn_blocking(move || toggle_mode_impl(&app.state::<AppState>()))
            .await
            .map_err(|e| e.to_string())?
    }

    pub(super) fn toggle_mode_impl(state: &AppState) -> Result<bool, String> {
        if state.shutdown_started.load(Ordering::Acquire) {
            return Err("应用正在退出".into());
        }
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_owned())?;
        match runtime.status() {
            RuntimeStatus::Running => {
                let outputs = runtime
                    .apply(RuntimeCommand::Disable)
                    .map_err(|error| format!("{error:?}"))?;
                flush_outputs(&mut runtime, &outputs).map_err(|error| format!("{error:?}"))?;
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
        let runtime = state.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        Ok(runtime_view(&state, &runtime))
    }

    #[tauri::command]
    pub fn settings_snapshot(
        state: tauri::State<'_, AppState>,
    ) -> Result<settings::SettingsSnapshot, String> {
        settings::snapshot(&state)
    }

    #[tauri::command]
    pub async fn set_profile(
        app: tauri::AppHandle,
        profile: String,
        expected_revision: u64,
    ) -> Result<settings::SettingsSnapshot, String> {
        tauri::async_runtime::spawn_blocking(move || {
            settings::mutate(&app.state::<AppState>(), expected_revision, |config| {
                config.active_profile = profile;
                Ok(())
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tauri::command]
    pub async fn set_rule_enabled(
        app: tauri::AppHandle,
        profile_id: String,
        rule_id: String,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<settings::SettingsSnapshot, String> {
        tauri::async_runtime::spawn_blocking(move || {
            settings::mutate(&app.state::<AppState>(), expected_revision, |config| {
                settings::set_enabled(config, &profile_id, &rule_id, enabled)
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tauri::command]
    pub async fn set_software_input_passthrough(
        app: tauri::AppHandle,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<settings::SettingsSnapshot, String> {
        tauri::async_runtime::spawn_blocking(move || {
            settings::set_software_input_passthrough(
                &app.state::<AppState>(),
                expected_revision,
                enabled,
            )
        })
        .await
        .map_err(|error| error.to_string())?
    }

    #[tauri::command]
    pub async fn set_show_desktop_shortcut(
        app: tauri::AppHandle,
        trigger: String,
        expected_revision: u64,
    ) -> Result<settings::SettingsSnapshot, String> {
        tauri::async_runtime::spawn_blocking(move || {
            settings::mutate(&app.state::<AppState>(), expected_revision, |config| {
                config.system_shortcuts.show_desktop = trigger;
                Ok(())
            })
        })
        .await
        .map_err(|e| e.to_string())?
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
        flush_outputs(&mut runtime, &outputs).map_err(|error| format!("{error:?}"))?;
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
        flush_outputs(&mut runtime, &outputs).map_err(|error| format!("{error:?}"))?;
        Ok(())
    }

    #[tauri::command]
    pub async fn add_rule(
        app: tauri::AppHandle,
        name: String,
        trigger: String,
        action: String,
        enabled: bool,
        priority: i32,
        expected_revision: u64,
    ) -> Result<settings::SettingsSnapshot, String> {
        tauri::async_runtime::spawn_blocking(move || {
            settings::mutate(&app.state::<AppState>(), expected_revision, |config| {
                let rule = mac_to_windows_config::Rule {
                    name,
                    trigger,
                    action,
                    enabled,
                    priority,
                    ..Default::default()
                };
                let profile = config
                    .profiles
                    .get_mut(&config.active_profile)
                    .ok_or("预设不存在")?;
                if rule.trigger.starts_with("mouse:") || rule.trigger.starts_with("scroll:") {
                    profile.mouse_rules.push(rule);
                } else {
                    profile.keyboard_rules.push(rule);
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tauri::command]
    pub async fn reload_permissions(app: tauri::AppHandle) -> Result<RuntimeStatus, String> {
        tauri::async_runtime::spawn_blocking(move || {
            #[cfg(target_os = "macos")]
            let permission = {
                let (listen, post) = macos_adapter::platform::request_permissions();
                if listen && post {
                    PermissionState::Granted
                } else {
                    PermissionState::Denied
                }
            };
            #[cfg(not(target_os = "macos"))]
            let permission = PermissionState::Denied;
            let state = app.state::<AppState>();
            let status = {
                let mut runtime = state.runtime.lock().map_err(|_| "runtime lock poisoned")?;
                let outputs = runtime.permissions_refreshed(permission);
                flush_outputs(&mut runtime, &outputs).map_err(|e| format!("{e:?}"))?;
                runtime.status().clone()
            };
            if permission == PermissionState::Granted && !state.event_runtime.is_started() {
                state
                    .event_runtime
                    .start(Arc::clone(&state.runtime), state.clipboard())?;
            }
            Ok(status)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tauri::command]
    pub async fn clipboard_history(
        state: tauri::State<'_, AppState>,
    ) -> Result<clipboard::ClipboardSnapshot, String> {
        let manager = state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?;
        tauri::async_runtime::spawn_blocking(move || {
            manager.refresh().map_err(|error| error.to_string())?;
            Ok(manager.snapshot())
        })
        .await
        .map_err(|error| format!("剪贴板刷新线程失败：{error}"))?
    }

    #[tauri::command]
    pub async fn select_clipboard_item(
        state: tauri::State<'_, AppState>,
        id: u64,
    ) -> Result<(), String> {
        let manager = state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?;
        tauri::async_runtime::spawn_blocking(move || {
            manager.select(id).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("剪贴板操作线程失败：{error}"))?
    }

    #[tauri::command]
    pub async fn delete_clipboard_item(
        state: tauri::State<'_, AppState>,
        id: u64,
    ) -> Result<(), String> {
        let manager = state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?;
        tauri::async_runtime::spawn_blocking(move || {
            manager.delete(id).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("剪贴板操作线程失败：{error}"))?
    }

    #[tauri::command]
    pub async fn clear_clipboard_history(state: tauri::State<'_, AppState>) -> Result<(), String> {
        let manager = state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?;
        tauri::async_runtime::spawn_blocking(move || {
            manager.clear().map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("剪贴板操作线程失败：{error}"))?
    }

    #[tauri::command]
    pub async fn close_clipboard_panel(
        app: tauri::AppHandle,
        state: tauri::State<'_, AppState>,
    ) -> Result<(), String> {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(window) = handle.get_webview_window("clipboard") {
                let _ = window.hide();
            }
            // Hiding the last window alone leaves the Tauri application
            // active. macOS can then keep MacToWindows as the frontmost app,
            // which makes the next clipboard capture lose its real target.
            // Hide the application as well so the previous app remains
            // frontmost after dismissing the panel.
            #[cfg(target_os = "macos")]
            let _ = handle.hide();
        });
        let manager = state
            .clipboard()
            .ok_or_else(|| "剪贴板服务尚未启动".to_owned())?;
        tauri::async_runtime::spawn_blocking(move || {
            manager.close_panel().map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("剪贴板操作线程失败：{error}"))?
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
    pub async fn import_config(
        app: tauri::AppHandle,
        contents: String,
        expected_revision: u64,
    ) -> Result<settings::SettingsSnapshot, String> {
        tauri::async_runtime::spawn_blocking(move || {
            settings::mutate(&app.state::<AppState>(), expected_revision, |candidate| {
                *candidate =
                    mac_to_windows_config::import_toml(&contents).map_err(|e| e.to_string())?;
                Ok(())
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_model::{
        DeviceSource, EventKind, EventOrigin, EventPhase, ModifierSet, PhysicalCode,
    };
    use rule_engine::{Action, ModifierMatcher, Rule, RuleScope, Trigger};

    #[test]
    fn reopen_only_restores_main_window_when_no_window_is_visible() {
        assert!(should_restore_main_window(false));
        assert!(!should_restore_main_window(true));
    }

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

    fn keyboard_event(
        code: u16,
        phase: EventPhase,
        modifiers: ModifierSet,
        kind: EventKind,
    ) -> NormalizedEvent {
        NormalizedEvent {
            source: DeviceSource::Keyboard,
            kind,
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
    fn retried_cleanup_uses_current_native_flags_and_does_not_release_physical_holders() {
        let stale = OutputEvent::Key {
            code: PhysicalCode::key(8),
            phase: EventPhase::Up,
            modifiers: ModifierSet::LEFT_COMMAND,
        };
        let now_released = prepare_cleanup_outputs(&[stale.clone()], ModifierSet::NONE, |_| false);
        assert_eq!(
            now_released,
            vec![OutputEvent::Key {
                code: PhysicalCode::key(8),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE
            }]
        );
        let still_control =
            prepare_cleanup_outputs(&[stale.clone()], ModifierSet::RIGHT_CONTROL, |_| false);
        assert!(matches!(
            &still_control[0],
            OutputEvent::Key {
                modifiers: ModifierSet::RIGHT_CONTROL,
                ..
            }
        ));
        assert!(still_control.iter().any(|output| matches!(
            output,
            OutputEvent::Key {
                code: PhysicalCode::Key(62),
                phase: EventPhase::Down,
                ..
            }
        )));
        let handed_back = prepare_cleanup_outputs(&[stale], ModifierSet::NONE, |code| {
            code == PhysicalCode::key(8)
        });
        assert!(handed_back.is_empty());
    }

    #[test]
    fn permission_loss_collects_release_and_stops_processing_mapped_keys() {
        let mut controller = controller_with_rule();
        controller.status = RuntimeStatus::Running;
        controller.process_event(&key(), |_| Ok(()));
        let releases = controller.permissions_refreshed(PermissionState::Denied);
        assert_eq!(controller.status, RuntimeStatus::PermissionRequired);
        assert!(matches!(
            &releases[..],
            [OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Up,
                ..
            }]
        ));
        let mut delivered = Vec::new();
        controller
            .flush_with(&releases, |outputs, _| {
                delivered.extend_from_slice(outputs);
                Ok(())
            })
            .unwrap();
        assert_eq!(delivered, releases);
        let up = NormalizedEvent {
            phase: EventPhase::Up,
            ..key()
        };
        assert_eq!(
            controller.process_event(&up, |_| panic!("paused runtime must pass")),
            Decision::Pass
        );
        assert_eq!(
            controller
                .engine
                .state()
                .logical_down_count(PhysicalCode::key(1)),
            0
        );
    }

    #[test]
    fn failed_cleanup_is_retained_until_successful_retry() {
        let mut controller = controller_with_rule();
        controller.status = RuntimeStatus::Running;
        controller.process_event(&key(), |_| Ok(()));
        let outputs = controller.transition(RuntimeStatus::Degraded);
        assert!(controller
            .flush_with(&outputs, |_, _| Err(InjectionError::PermissionDenied))
            .is_err());
        assert_eq!(controller.pending_releases, outputs);
        controller.flush_with(&[], |_, _| Ok(())).unwrap();
        assert_eq!(controller.pending_releases, outputs);
        let retry = controller.permissions_refreshed(PermissionState::Granted);
        assert_eq!(retry, outputs);
        assert!(controller
            .flush_with(&retry, |_, _| Err(InjectionError::Unavailable))
            .is_err());
        assert_eq!(controller.pending_releases, outputs);
        controller.flush_with(&outputs, |_, _| Ok(())).unwrap();
        assert!(controller.pending_releases.is_empty());
    }

    #[test]
    fn failed_output_rolls_back_before_cleanup_and_does_not_release_an_unposted_key() {
        let mut controller = controller_with_rule();
        controller.status = RuntimeStatus::Running;
        let mut attempts = Vec::new();
        let decision = controller.process_event_marked(&key(), |outputs, _| {
            attempts.push(outputs.to_vec());
            Err(InjectionError::Unavailable)
        });
        assert_eq!(decision, Decision::Pass);
        assert_eq!(attempts.len(), 1);
        assert!(matches!(
            &attempts[0][..],
            [OutputEvent::Key {
                phase: EventPhase::Down,
                ..
            }]
        ));
        assert!(controller.pending_releases.is_empty());
        assert_eq!(
            controller
                .engine
                .state()
                .logical_down_count(PhysicalCode::key(1)),
            0
        );
    }

    #[test]
    fn asynchronous_failure_releases_prior_binding_through_supplied_injector() {
        let mut controller = controller_with_rule();
        controller.status = RuntimeStatus::Running;
        controller.process_event(&key(), |_| Ok(()));
        let mut released = Vec::new();
        controller.degrade_with(
            "input source failed".into(),
            &mut |outputs: &[OutputEvent], _| {
                released.extend_from_slice(outputs);
                Ok(())
            },
        );
        assert_eq!(controller.status, RuntimeStatus::Degraded);
        assert!(matches!(
            &released[..],
            [OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Up,
                ..
            }]
        ));
        assert!(controller.pending_releases.is_empty());
    }

    #[test]
    fn configuration_swap_preserves_a_held_keys_original_release_target() {
        let mut controller = controller_with_rule();
        controller.status = RuntimeStatus::Running;
        controller.process_event(&key(), |_| Ok(()));
        let mut config = AppConfig::default();
        config.active_profile = "custom".into();
        config.profiles.get_mut("custom").unwrap().keyboard_rules =
            vec![mac_to_windows_config::Rule {
                trigger: "key:0".into(),
                action: "key:2".into(),
                ..Default::default()
            }];
        assert!(controller
            .apply(RuntimeCommand::ReplaceConfig(config))
            .unwrap()
            .is_empty());
        let mut delivered = Vec::new();
        controller.process_event(
            &NormalizedEvent {
                phase: EventPhase::Up,
                ..key()
            },
            |outputs| {
                delivered.extend_from_slice(outputs);
                Ok(())
            },
        );
        assert!(matches!(
            &delivered[..],
            [OutputEvent::Key {
                code: PhysicalCode::Key(1),
                phase: EventPhase::Up,
                ..
            }]
        ));
        delivered.clear();
        controller.process_event(&key(), |outputs| {
            delivered.extend_from_slice(outputs);
            Ok(())
        });
        assert!(matches!(
            &delivered[..],
            [OutputEvent::Key {
                code: PhysicalCode::Key(2),
                phase: EventPhase::Down,
                ..
            }]
        ));
    }

    #[test]
    fn shutdown_releases_once_and_does_not_reenable_mapping() {
        let mut controller = controller_with_rule();
        controller.status = RuntimeStatus::Running;
        controller.process_event(&key(), |_| Ok(()));
        let release = controller.apply(RuntimeCommand::Shutdown).unwrap();
        assert_eq!(release.len(), 1);
        controller.flush_with(&release, |_, _| Ok(())).unwrap();
        assert!(controller
            .apply(RuntimeCommand::Shutdown)
            .unwrap()
            .is_empty());
        assert_eq!(controller.status, RuntimeStatus::Stopped);
    }

    #[test]
    fn shortcut_restores_a_modifier_held_before_mapping_was_enabled() {
        let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
        controller.status = RuntimeStatus::Running;
        let f5 = keyboard_event(
            96,
            EventPhase::Down,
            ModifierSet::LEFT_CONTROL,
            EventKind::Keyboard,
        );
        let mut delivered = Vec::new();
        assert_eq!(
            controller.process_event(&f5, |outputs| {
                delivered.extend_from_slice(outputs);
                Ok(())
            }),
            Decision::Suppress
        );
        assert!(matches!(
            delivered.first(),
            Some(OutputEvent::Key {
                code: PhysicalCode::Key(55),
                phase: EventPhase::Down,
                ..
            })
        ));
        assert!(delivered.iter().any(|event| matches!(event,
            OutputEvent::Key { code: PhysicalCode::Key(15), phase: EventPhase::Down, modifiers }
                if modifiers.contains(ModifierSet::LEFT_COMMAND | ModifierSet::LEFT_SHIFT)
                    && !modifiers.contains(ModifierSet::LEFT_CONTROL))));
        assert!(delivered.iter().any(|event| matches!(
            event,
            OutputEvent::Key {
                code: PhysicalCode::Key(59),
                phase: EventPhase::Down,
                modifiers: ModifierSet::LEFT_CONTROL
            }
        )));
    }

    #[test]
    fn show_desktop_uses_the_configured_complete_shortcut() {
        let mut config = AppConfig::default();
        config.system_shortcuts.show_desktop = "chord:left_control|left_shift+key:11".into();
        config
            .profiles
            .get_mut("windows_physical")
            .unwrap()
            .keyboard_rules
            .push(mac_to_windows_config::Rule {
                trigger: "key:0".into(),
                action: "shortcut:show_desktop".into(),
                ..Default::default()
            });
        let expected = config.show_desktop_output().unwrap();
        let mut controller = RuntimeController::from_config(config).unwrap();
        controller.status = RuntimeStatus::Running;
        let mut delivered = Vec::new();
        assert_eq!(
            controller.process_event(&key(), |outputs| {
                delivered.extend_from_slice(outputs);
                Ok(())
            }),
            Decision::Suppress
        );
        assert_eq!(delivered, expected);
        assert!(delivered.iter().any(|e| matches!(
            e,
            OutputEvent::Key {
                code: PhysicalCode::Key(11),
                phase: EventPhase::Up,
                ..
            }
        )));
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
    fn default_control_copy_mapping_injects_only_command_copy() {
        let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let mut injected = Vec::new();

        assert_eq!(
            controller.process_event(
                &keyboard_event(
                    59,
                    EventPhase::Changed,
                    ModifierSet::LEFT_CONTROL,
                    EventKind::FlagsChanged,
                ),
                |outputs| {
                    injected.extend_from_slice(outputs);
                    Ok(())
                },
            ),
            Decision::Pass
        );
        assert_eq!(
            controller.process_event(
                &keyboard_event(
                    8,
                    EventPhase::Down,
                    ModifierSet::LEFT_CONTROL,
                    EventKind::Keyboard,
                ),
                |outputs| {
                    injected.extend_from_slice(outputs);
                    Ok(())
                },
            ),
            Decision::Suppress
        );
        assert_eq!(
            controller.process_event(
                &keyboard_event(
                    8,
                    EventPhase::Up,
                    ModifierSet::LEFT_CONTROL,
                    EventKind::Keyboard,
                ),
                |outputs| {
                    injected.extend_from_slice(outputs);
                    Ok(())
                },
            ),
            Decision::Suppress
        );
        assert_eq!(
            controller.process_event(
                &keyboard_event(
                    59,
                    EventPhase::Changed,
                    ModifierSet::NONE,
                    EventKind::FlagsChanged,
                ),
                |outputs| {
                    injected.extend_from_slice(outputs);
                    Ok(())
                },
            ),
            Decision::Pass
        );
        assert_eq!(
            injected,
            vec![
                OutputEvent::Key {
                    code: PhysicalCode::key(8),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::LEFT_COMMAND,
                },
                OutputEvent::Key {
                    code: PhysicalCode::key(8),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::LEFT_COMMAND,
                },
            ]
        );
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
    fn own_outputs_and_text_injection_bypass_mapping_without_injection() {
        let mut controller = controller_with_rule();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let own = controller.adapter.own_marker();
        for origin in [
            EventOrigin::Injected { marker: own },
            EventOrigin::TextInjection { marker: own },
        ] {
            let own_event = NormalizedEvent { origin, ..key() };
            assert_eq!(
                controller.process_event(&own_event, |_| panic!("own replay must bypass mapping")),
                Decision::Pass
            );
        }
        let foreign_event = NormalizedEvent {
            origin: event_model::EventOrigin::TextInjection {
                marker: own.wrapping_add(1),
            },
            ..key()
        };
        assert_eq!(
            controller.process_event(&foreign_event, |_| panic!("external input must pass")),
            Decision::Pass
        );
        for _ in 0..4 {
            let unmarked = NormalizedEvent {
                origin: EventOrigin::TextInjection { marker: 0 },
                ..key()
            };
            assert_eq!(
                controller.process_event(&unmarked, |_| panic!("text must not be rebuilt")),
                Decision::Pass
            );
        }
        assert!(!controller
            .engine
            .state()
            .physical_down(PhysicalCode::key(0)));
        assert_eq!(
            controller
                .engine
                .state()
                .logical_down_count(PhysicalCode::key(1)),
            0
        );
        let snapshot = controller.diagnostics().snapshot();
        assert_eq!(snapshot.events_seen, 7);
        assert_eq!(snapshot.events_passed, 7);
        assert_eq!(snapshot.external_keyboard_events_passed, 5);
        assert_eq!(snapshot.events_injected, 0);
        assert_eq!(controller.status(), &RuntimeStatus::Running);
    }

    #[test]
    fn ordinary_injected_keyboard_streams_keep_windows_copy_paste_undo_and_select_all() {
        for origin in [
            EventOrigin::Hardware,
            EventOrigin::Injected { marker: 0 },
            EventOrigin::Injected { marker: 77 },
        ] {
            let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
            controller.apply(RuntimeCommand::Enable).unwrap();
            let mut delivered = Vec::new();
            for (code, phase, modifiers, kind) in std::iter::once((
                59,
                EventPhase::Changed,
                ModifierSet::LEFT_CONTROL,
                EventKind::FlagsChanged,
            ))
            .chain([8, 9, 6, 0].into_iter().flat_map(|code| {
                [
                    (
                        code,
                        EventPhase::Down,
                        ModifierSet::LEFT_CONTROL,
                        EventKind::Keyboard,
                    ),
                    (
                        code,
                        EventPhase::Up,
                        ModifierSet::LEFT_CONTROL,
                        EventKind::Keyboard,
                    ),
                ]
            }))
            .chain(std::iter::once((
                59,
                EventPhase::Changed,
                ModifierSet::NONE,
                EventKind::FlagsChanged,
            ))) {
                let event = NormalizedEvent {
                    origin,
                    ..keyboard_event(code, phase, modifiers, kind)
                };
                let decision = controller.process_event(&event, |outputs| {
                    delivered.extend_from_slice(outputs);
                    Ok(())
                });
                assert_eq!(
                    decision,
                    if code == 59 {
                        Decision::Pass
                    } else {
                        Decision::Suppress
                    }
                );
            }
            let expected: Vec<_> = [8, 9, 6, 0]
                .into_iter()
                .flat_map(|code| {
                    [EventPhase::Down, EventPhase::Up].map(|phase| OutputEvent::Key {
                        code: PhysicalCode::key(code),
                        phase,
                        modifiers: ModifierSet::LEFT_COMMAND,
                    })
                })
                .collect();
            assert_eq!(delivered, expected);
            assert_eq!(
                controller
                    .diagnostics()
                    .snapshot()
                    .external_keyboard_events_passed,
                0
            );
            assert_eq!(controller.status(), &RuntimeStatus::Running);
            assert!(!controller
                .engine
                .state()
                .physical_down(PhysicalCode::key(59)));
        }
    }

    #[test]
    fn external_same_key_events_cannot_release_a_local_frozen_mapping() {
        let mut controller = controller_with_rule();
        controller.apply(RuntimeCommand::Enable).unwrap();
        assert_eq!(
            controller.process_event(&key(), |_| Ok(())),
            Decision::Suppress
        );
        for phase in [EventPhase::Down, EventPhase::Down, EventPhase::Up] {
            let remote = NormalizedEvent {
                origin: EventOrigin::TextInjection { marker: 0 },
                phase,
                ..key()
            };
            assert_eq!(
                controller.process_event(&remote, |_| panic!("remote event injected")),
                Decision::Pass
            );
            assert!(controller
                .engine
                .state()
                .physical_down(PhysicalCode::key(0)));
            assert_eq!(
                controller
                    .engine
                    .state()
                    .logical_down_count(PhysicalCode::key(1)),
                1
            );
        }
        let mut released = Vec::new();
        let up = NormalizedEvent {
            phase: EventPhase::Up,
            ..key()
        };
        assert_eq!(
            controller.process_event(&up, |outputs| {
                released.extend_from_slice(outputs);
                Ok(())
            }),
            Decision::Suppress
        );
        assert_eq!(
            released,
            vec![OutputEvent::Key {
                code: PhysicalCode::key(1),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE,
            }]
        );
        assert!(!controller
            .engine
            .state()
            .physical_down(PhysicalCode::key(0)));
    }

    #[test]
    fn external_input_preserves_local_mapped_option_and_ignores_emergency_hotkey() {
        let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let option = keyboard_event(
            58,
            EventPhase::Changed,
            ModifierSet::LEFT_OPTION,
            EventKind::FlagsChanged,
        );
        assert_eq!(
            controller.process_event(&option, |_| Ok(())),
            Decision::Suppress
        );
        for code in [0, 9, 53] {
            let remote = NormalizedEvent {
                code: PhysicalCode::key(code),
                origin: EventOrigin::TextInjection { marker: 0 },
                modifiers: ModifierSet::LEFT_CONTROL
                    | ModifierSet::LEFT_OPTION
                    | ModifierSet::LEFT_COMMAND,
                ..key()
            };
            assert_eq!(
                controller.process_event(&remote, |_| panic!("remote event injected")),
                Decision::Pass
            );
            assert_eq!(controller.status(), &RuntimeStatus::Running);
            assert!(controller
                .engine
                .state()
                .physical_down(PhysicalCode::key(58)));
            assert_eq!(
                controller
                    .engine
                    .state()
                    .logical_down_count(PhysicalCode::key(55)),
                1
            );
        }
        let mut released = Vec::new();
        let up = NormalizedEvent {
            modifiers: ModifierSet::NONE,
            ..option
        };
        assert_eq!(
            controller.process_event(&up, |outputs| {
                released.extend_from_slice(outputs);
                Ok(())
            }),
            Decision::Suppress
        );
        assert_eq!(
            released,
            vec![OutputEvent::Key {
                code: PhysicalCode::key(55),
                phase: EventPhase::Up,
                modifiers: ModifierSet::NONE,
            }]
        );
    }

    #[test]
    fn foreign_input_cancels_pending_modifier_gesture_without_degrading_runtime() {
        let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
        controller.apply(RuntimeCommand::Enable).unwrap();
        let control = keyboard_event(
            59,
            EventPhase::Changed,
            ModifierSet::LEFT_CONTROL,
            EventKind::FlagsChanged,
        );
        assert_eq!(
            controller.process_event(&control, |_| Ok(())),
            Decision::Pass
        );
        let remote = NormalizedEvent {
            origin: EventOrigin::TextInjection { marker: 0 },
            ..key()
        };
        assert_eq!(
            controller.process_event(&remote, |_| panic!("remote event injected")),
            Decision::Pass
        );
        let shift = keyboard_event(
            56,
            EventPhase::Changed,
            ModifierSet::LEFT_SHIFT,
            EventKind::FlagsChanged,
        );
        assert_eq!(controller.process_event(&shift, |_| Ok(())), Decision::Pass);
        for down in [shift, control] {
            let up = NormalizedEvent {
                modifiers: ModifierSet::NONE,
                ..down
            };
            // Without cancellation the Ctrl+Shift gesture requests an input
            // source worker (absent in tests) and degrades the controller.
            assert_eq!(controller.process_event(&up, |_| Ok(())), Decision::Pass);
            assert_eq!(controller.status(), &RuntimeStatus::Running);
        }
        assert!(controller.last_error.is_none());
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
        let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
        controller.config = Some(config);
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

    #[test]
    fn text_injection_cannot_activate_or_release_local_clipboard_hotkey_latch() {
        let mut latch = ClipboardHotkeyLatch::default();
        let option = keyboard_event(
            58,
            EventPhase::Changed,
            ModifierSet::LEFT_OPTION,
            EventKind::FlagsChanged,
        );
        assert_eq!(latch.handle(&option), None);
        let v = NormalizedEvent {
            code: PhysicalCode::key(9),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key()
        };
        for marker in [0, 100] {
            let remote = NormalizedEvent {
                origin: EventOrigin::TextInjection { marker },
                ..v.clone()
            };
            assert_eq!(latch.handle(&remote), None);
            assert!(!latch.active);
        }
        assert_eq!(latch.handle(&v), Some(ClipboardHotkeyAction::Show));
        let remote_up = NormalizedEvent {
            phase: EventPhase::Up,
            origin: EventOrigin::TextInjection { marker: 0 },
            ..v.clone()
        };
        assert_eq!(latch.handle(&remote_up), None);
        assert!(latch.active);
        let local_up = NormalizedEvent {
            phase: EventPhase::Up,
            ..v
        };
        assert_eq!(
            latch.handle(&local_up),
            Some(ClipboardHotkeyAction::Suppress)
        );
        assert!(!latch.active);
    }

    #[test]
    fn clipboard_hotkey_uses_flags_changed_to_distinguish_right_option() {
        let mut latch = ClipboardHotkeyLatch::default();
        let left_down = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            code: PhysicalCode::key(58),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key()
        };
        let right_down = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            code: PhysicalCode::key(61),
            modifiers: ModifierSet::RIGHT_OPTION,
            ..key()
        };
        let right_up = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            code: PhysicalCode::key(61),
            modifiers: ModifierSet::NONE,
            ..key()
        };
        let left_up = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            code: PhysicalCode::key(58),
            modifiers: ModifierSet::NONE,
            ..key()
        };
        let v = NormalizedEvent {
            code: PhysicalCode::key(9),
            modifiers: ModifierSet::LEFT_OPTION | ModifierSet::RIGHT_OPTION,
            ..key()
        };
        assert_eq!(latch.handle(&left_down), None);
        assert_eq!(latch.handle(&right_down), None);
        assert_eq!(latch.handle(&v), None);
        assert_eq!(latch.handle(&right_up), None);
        assert_eq!(latch.handle(&v), Some(ClipboardHotkeyAction::Show));
        assert_eq!(latch.handle(&left_up), None);
        let plain = NormalizedEvent {
            code: PhysicalCode::key(9),
            modifiers: ModifierSet::NONE,
            ..key()
        };
        assert_eq!(latch.handle(&plain), None);
        latch.reset();
        assert_eq!(latch.handle(&right_down), None);
        let right_v = NormalizedEvent {
            code: PhysicalCode::key(9),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key()
        };
        assert_eq!(latch.handle(&right_v), None);
        assert_eq!(latch.handle(&right_up), None);
    }

    #[test]
    fn clipboard_hotkey_repeated_option_flags_do_not_flip_state() {
        let mut latch = ClipboardHotkeyLatch::default();
        let left_down = NormalizedEvent {
            kind: EventKind::FlagsChanged,
            code: PhysicalCode::key(58),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key()
        };
        let v = NormalizedEvent {
            code: PhysicalCode::key(9),
            modifiers: ModifierSet::LEFT_OPTION,
            ..key()
        };

        assert_eq!(latch.handle(&left_down), None);
        assert_eq!(latch.handle(&left_down), None);
        assert_eq!(latch.handle(&v), Some(ClipboardHotkeyAction::Show));
    }

    #[cfg(target_os = "macos")]
    mod keyboard_route_tests {
        use super::*;
        use macos_adapter::{EventTapAction, EventTapEvent, RawEventRef};
        use std::ffi::c_void;

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool)
                -> RawEventRef;
            fn CGEventSetIntegerValueField(event: RawEventRef, field: u32, value: i64);
            fn CGEventGetIntegerValueField(event: RawEventRef, field: u32) -> i64;
            fn CGEventSetType(event: RawEventRef, event_type: u32);
            fn CGEventGetType(event: RawEventRef) -> u32;
            fn CGEventSetFlags(event: RawEventRef, flags: u64);
            fn CGEventGetFlags(event: RawEventRef) -> u64;
            fn CGEventSetTimestamp(event: RawEventRef, time: u64);
            fn CGEventGetTimestamp(event: RawEventRef) -> u64;
            fn CGEventKeyboardSetUnicodeString(event: RawEventRef, length: usize, text: *const u16);
            fn CGEventKeyboardGetUnicodeString(
                event: RawEventRef,
                capacity: usize,
                length: *mut usize,
                text: *mut u16,
            );
        }

        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFRelease(value: *const c_void);
        }

        struct Keyboard(RawEventRef);

        impl Keyboard {
            fn new(event_type: u32, marker: u64, pid: i64, state: i64, text: &str) -> Self {
                let raw = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), 0, true) };
                assert!(!raw.is_null());
                let text: Vec<u16> = text.encode_utf16().collect();
                unsafe {
                    CGEventSetType(raw, event_type);
                    CGEventSetIntegerValueField(raw, 42, marker as i64);
                    CGEventSetIntegerValueField(raw, 41, pid);
                    CGEventSetIntegerValueField(raw, 45, state);
                    CGEventSetIntegerValueField(raw, 8, 0);
                    CGEventSetFlags(raw, 0x0004_0001);
                    CGEventSetTimestamp(raw, 123456789);
                    CGEventKeyboardSetUnicodeString(raw, text.len(), text.as_ptr());
                }
                Self(raw)
            }

            fn stroke(event_type: u32, code: u16, pid: i64, state: i64, flags: u64) -> Self {
                let raw = Self::new(event_type, 0, pid, state, "");
                unsafe {
                    CGEventSetIntegerValueField(raw.0, 9, i64::from(code));
                    CGEventSetFlags(raw.0, flags);
                }
                raw
            }

            fn text(&self) -> Vec<u16> {
                let mut text = [0; 128];
                let mut length = 0;
                unsafe {
                    CGEventKeyboardGetUnicodeString(
                        self.0,
                        text.len(),
                        &mut length,
                        text.as_mut_ptr(),
                    )
                };
                text[..length].to_vec()
            }
        }

        impl Drop for Keyboard {
            fn drop(&mut self) {
                unsafe { CFRelease(self.0) };
            }
        }

        #[test]
        fn deskflow_keycode_paths_keep_windows_shortcuts_while_enigo_text_passes() {
            // Metadata-shaped objects cover both the HID-like route and the
            // positive-PID CGEvent fallback. This does not post an HID event.
            for (pid, state) in [(0, 1), (4321, 0), (4321, 1)] {
                let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
                controller.apply(RuntimeCommand::Enable).unwrap();
                let own = controller.own_marker();
                let diagnostics = controller.diagnostics();
                let pending = Arc::clone(&controller.external_keyboard_pending);
                let runtime = Mutex::new(controller);
                let mut normalizer = macos_adapter::EventNormalizer::new(own);
                let mut delivered = Vec::new();
                let steps = std::iter::once((12, 59, 0x0004_0001))
                    .chain(
                        [8, 9, 6, 0]
                            .into_iter()
                            .flat_map(|code| [(10, code, 0x0004_0001), (11, code, 0x0004_0001)]),
                    )
                    .chain(std::iter::once((12, 59, 0)));
                for (event_type, code, flags) in steps {
                    let raw = Keyboard::stroke(event_type, code, pid, state, flags);
                    let input = EventTapEvent::new(event_type, raw.0);
                    assert_eq!(
                        route_keyboard_input(input, own, &runtime, &diagnostics, &pending),
                        None
                    );
                    let event = normalizer
                        .normalize(input)
                        .expect("Deskflow keycode stream must normalize");
                    assert_eq!(event.origin, EventOrigin::Hardware);
                    let decision = runtime.lock().unwrap().process_event(&event, |outputs| {
                        delivered.extend_from_slice(outputs);
                        Ok(())
                    });
                    assert_eq!(
                        decision,
                        if code == 59 {
                            Decision::Pass
                        } else {
                            Decision::Suppress
                        }
                    );
                    if event_type == 12 && flags != 0 {
                        for text in ["a", "b", "中"] {
                            let text_raw = Keyboard::new(10, 100, 9876, 0, text);
                            assert_eq!(
                                route_keyboard_input(
                                    EventTapEvent::new(10, text_raw.0),
                                    own,
                                    &runtime,
                                    &diagnostics,
                                    &pending
                                ),
                                Some(EventTapAction::Pass)
                            );
                            assert_eq!(text_raw.text(), text.encode_utf16().collect::<Vec<_>>());
                            let controller = runtime.lock().unwrap();
                            assert!(controller
                                .engine
                                .state()
                                .physical_down(PhysicalCode::key(59)));
                            assert!(!controller
                                .engine
                                .state()
                                .physical_down(PhysicalCode::key(0)));
                        }
                    }
                }
                let expected: Vec<_> = [8, 9, 6, 0]
                    .into_iter()
                    .flat_map(|code| {
                        [EventPhase::Down, EventPhase::Up].map(|phase| OutputEvent::Key {
                            code: PhysicalCode::key(code),
                            phase,
                            modifiers: ModifierSet::LEFT_COMMAND,
                        })
                    })
                    .collect();
                assert_eq!(delivered, expected);
                assert_eq!(diagnostics.snapshot().external_keyboard_events_passed, 3);
                assert_eq!(runtime.lock().unwrap().status(), &RuntimeStatus::Running);
            }
        }

        #[test]
        fn deskflow_fallback_keeps_alt_tab_modifier_mapping_and_clipboard_hotkey() {
            let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
            controller.apply(RuntimeCommand::Enable).unwrap();
            let own = controller.own_marker();
            let mut normalizer = macos_adapter::EventNormalizer::new(own);
            let mut delivered = Vec::new();
            for (event_type, code, flags) in [
                (12, 58, 0x0008_0020),
                (10, 48, 0x0008_0020),
                (11, 48, 0x0008_0020),
                (12, 58, 0),
            ] {
                let raw = Keyboard::stroke(event_type, code, 4321, 0, flags);
                let event = normalizer
                    .normalize(EventTapEvent::new(event_type, raw.0))
                    .unwrap();
                assert_eq!(event.origin, EventOrigin::Hardware);
                controller.process_event(&event, |outputs| {
                    delivered.extend_from_slice(outputs);
                    Ok(())
                });
            }
            assert_eq!(
                delivered,
                vec![
                    OutputEvent::Key {
                        code: PhysicalCode::key(55),
                        phase: EventPhase::Down,
                        modifiers: ModifierSet::LEFT_WINDOWS
                    },
                    OutputEvent::Key {
                        code: PhysicalCode::key(48),
                        phase: EventPhase::Down,
                        modifiers: ModifierSet::LEFT_WINDOWS
                    },
                    OutputEvent::Key {
                        code: PhysicalCode::key(48),
                        phase: EventPhase::Up,
                        modifiers: ModifierSet::LEFT_WINDOWS
                    },
                    OutputEvent::Key {
                        code: PhysicalCode::key(55),
                        phase: EventPhase::Up,
                        modifiers: ModifierSet::NONE
                    },
                ]
            );
            let mut latch = ClipboardHotkeyLatch::default();
            for (event_type, code, expected) in [
                (12, 58, None),
                (10, 9, Some(ClipboardHotkeyAction::Show)),
                (11, 9, Some(ClipboardHotkeyAction::Suppress)),
            ] {
                let raw = Keyboard::stroke(event_type, code, 4321, 0, 0x0008_0020);
                let event = normalizer
                    .normalize(EventTapEvent::new(event_type, raw.0))
                    .unwrap();
                assert_eq!(latch.handle(&event), expected);
            }
            assert_eq!(
                controller
                    .diagnostics()
                    .snapshot()
                    .external_keyboard_events_passed,
                0
            );
        }

        #[test]
        fn raw_enigo_text_route_preserves_payload_and_local_binding() {
            let mut controller = controller_with_rule();
            controller.apply(RuntimeCommand::Enable).unwrap();
            controller.process_event(&key(), |_| Ok(()));
            let own = controller.own_marker();
            let diagnostics = controller.diagnostics();
            let pending = Arc::clone(&controller.external_keyboard_pending);
            let runtime = Mutex::new(controller);
            let foreign_marker = 100;
            let mut count = 0;
            for (marker, pid, state) in [
                (foreign_marker, 4321, 0),
                (foreign_marker, 4321, 1),
                (foreign_marker, 0, 0),
                (foreign_marker, 0, 1),
                (foreign_marker, -1, 1),
            ] {
                for event_type in [10, 11, 12] {
                    for text in ["a", "b", "中", "e\u{301}", "👨‍👩‍👧‍👦"] {
                        let raw = Keyboard::new(event_type, marker, pid, state, text);
                        let original_text = raw.text();
                        assert_eq!(
                            route_keyboard_input(
                                EventTapEvent::new(event_type, raw.0),
                                own,
                                &runtime,
                                &diagnostics,
                                &pending,
                            ),
                            Some(EventTapAction::Pass)
                        );
                        assert_eq!(raw.text(), original_text);
                        unsafe {
                            assert_eq!(CGEventGetType(raw.0), event_type);
                            assert_eq!(CGEventGetFlags(raw.0), 0x0004_0001);
                            assert_eq!(CGEventGetTimestamp(raw.0), 123456789);
                            for (field, value) in
                                [(42, marker as i64), (41, pid), (45, state), (8, 0), (9, 0)]
                            {
                                assert_eq!(CGEventGetIntegerValueField(raw.0, field), value);
                            }
                        }
                        count += 1;
                    }
                }
            }
            let mut controller = runtime.lock().unwrap();
            assert!(controller
                .engine
                .state()
                .physical_down(PhysicalCode::key(0)));
            assert_eq!(
                controller
                    .engine
                    .state()
                    .logical_down_count(PhysicalCode::key(1)),
                1
            );
            assert_eq!(
                diagnostics.snapshot().external_keyboard_events_passed,
                count
            );
            let up = NormalizedEvent {
                phase: EventPhase::Up,
                ..key()
            };
            let mut released = Vec::new();
            assert_eq!(
                controller.process_event(&up, |outputs| {
                    released.extend_from_slice(outputs);
                    Ok(())
                }),
                Decision::Suppress
            );
            assert_eq!(
                released,
                vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Up,
                    modifiers: ModifierSet::NONE
                }]
            );
        }

        #[test]
        fn raw_hardware_enters_mapping_and_own_output_does_not_cancel_shift_gesture() {
            let mut controller = controller_with_rule();
            controller.apply(RuntimeCommand::Enable).unwrap();
            let own = controller.own_marker();
            let diagnostics = controller.diagnostics();
            let pending = Arc::clone(&controller.external_keyboard_pending);
            let runtime = Mutex::new(controller);
            let hardware = Keyboard::new(10, 0, 0, 1, "a");
            assert_eq!(
                route_keyboard_input(
                    EventTapEvent::new(10, hardware.0),
                    own,
                    &runtime,
                    &diagnostics,
                    &pending,
                ),
                None
            );
            unsafe { CGEventSetFlags(hardware.0, 0) };
            let mut normalizer = macos_adapter::EventNormalizer::new(own);
            let event = normalizer
                .normalize(EventTapEvent::new(10, hardware.0))
                .unwrap();
            assert_eq!(event.origin, EventOrigin::Hardware);
            let mut emitted = Vec::new();
            assert_eq!(
                runtime.lock().unwrap().process_event(&event, |outputs| {
                    emitted.extend_from_slice(outputs);
                    Ok(())
                }),
                Decision::Suppress
            );
            assert_eq!(
                emitted,
                vec![OutputEvent::Key {
                    code: PhysicalCode::key(1),
                    phase: EventPhase::Down,
                    modifiers: ModifierSet::NONE,
                }]
            );
            runtime.lock().unwrap().process_event(
                &NormalizedEvent {
                    phase: EventPhase::Up,
                    ..key()
                },
                |_| Ok(()),
            );
            let shift = keyboard_event(
                56,
                EventPhase::Changed,
                ModifierSet::LEFT_SHIFT,
                EventKind::FlagsChanged,
            );
            {
                let mut controller = runtime.lock().unwrap();
                controller.engine.set_pinyin_active(true);
                assert_eq!(controller.engine.handle(&shift), Decision::Pass);
            }
            let own_raw = Keyboard::new(10, own, std::process::id() as i64, 0, "a");
            assert_eq!(
                route_keyboard_input(
                    EventTapEvent::new(10, own_raw.0),
                    own,
                    &runtime,
                    &diagnostics,
                    &pending,
                ),
                Some(EventTapAction::Pass)
            );
            let up = NormalizedEvent {
                modifiers: ModifierSet::NONE,
                ..shift
            };
            assert!(
                matches!(runtime.lock().unwrap().engine.handle(&up), Decision::Emit(outputs) if outputs.contains(&OutputEvent::SystemShortcut(SystemShortcut::TogglePinyinEnglish)))
            );
            assert_eq!(diagnostics.snapshot().external_keyboard_events_passed, 0);
        }

        #[test]
        fn raw_text_input_does_not_wait_for_busy_runtime_and_defers_gesture_cancellation() {
            let mut controller = RuntimeController::from_config(AppConfig::default()).unwrap();
            controller.apply(RuntimeCommand::Enable).unwrap();
            let control = keyboard_event(
                59,
                EventPhase::Changed,
                ModifierSet::LEFT_CONTROL,
                EventKind::FlagsChanged,
            );
            assert_eq!(
                controller.process_event(&control, |_| Ok(())),
                Decision::Pass
            );
            let marker = controller.own_marker();
            let diagnostics = controller.diagnostics();
            let pending = Arc::clone(&controller.external_keyboard_pending);
            let runtime = Arc::new(Mutex::new(controller));
            let guard = runtime.lock().unwrap();
            let callback_runtime = Arc::clone(&runtime);
            let callback_pending = Arc::clone(&pending);
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let thread = std::thread::spawn(move || {
                let raw = Keyboard::new(10, 100, 4321, 0, "中文");
                let action = route_keyboard_input(
                    EventTapEvent::new(10, raw.0),
                    marker,
                    &callback_runtime,
                    &diagnostics,
                    &callback_pending,
                );
                tx.send((action, raw.text())).unwrap();
            });
            let result = rx.recv_timeout(std::time::Duration::from_secs(2));
            // Always unblock/join the worker, even if a regression made routing
            // wait on this guard, so a failed test cannot hang the test suite.
            drop(guard);
            thread.join().unwrap();
            let (action, text) = result.expect("software input must pass while runtime is busy");
            assert_eq!(action, Some(EventTapAction::Pass));
            assert_eq!(text, "中文".encode_utf16().collect::<Vec<_>>());
            assert!(pending.load(Ordering::Acquire));
            let mut controller = runtime.lock().unwrap();
            let shift = keyboard_event(
                56,
                EventPhase::Changed,
                ModifierSet::LEFT_SHIFT,
                EventKind::FlagsChanged,
            );
            assert_eq!(controller.process_event(&shift, |_| Ok(())), Decision::Pass);
            assert!(!pending.load(Ordering::Acquire));
            for down in [shift, control] {
                let up = NormalizedEvent {
                    modifiers: ModifierSet::NONE,
                    ..down
                };
                assert_eq!(controller.process_event(&up, |_| Ok(())), Decision::Pass);
            }
            assert_eq!(controller.status(), &RuntimeStatus::Running);
        }

        #[test]
        fn raw_text_input_passes_even_when_runtime_lock_is_poisoned() {
            let controller = controller_with_rule();
            let marker = controller.own_marker();
            let diagnostics = controller.diagnostics();
            let pending = Arc::clone(&controller.external_keyboard_pending);
            let runtime = Mutex::new(controller);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = runtime.lock().unwrap();
                panic!("poison test runtime");
            }));
            let raw = Keyboard::new(10, 100, 4321, 0, "中文");
            assert_eq!(
                route_keyboard_input(
                    EventTapEvent::new(10, raw.0),
                    marker,
                    &runtime,
                    &diagnostics,
                    &pending,
                ),
                Some(EventTapAction::Pass)
            );
            assert_eq!(raw.text(), "中文".encode_utf16().collect::<Vec<_>>());
        }
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
        state
            .event_runtime
            .shutdown(&state.runtime)
            .expect("Event Tap should stop and join cleanly");
        assert!(!state.event_runtime.is_started());
        assert_eq!(
            *state.runtime.lock().unwrap().status(),
            RuntimeStatus::Stopped
        );
    }
}
