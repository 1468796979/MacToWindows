use clipboard_core::{
    ClipboardEntry, ClipboardHistory, ClipboardStore, IngestOutcome, LoadOutcome,
};
use macos_adapter::{ForegroundApplication, PasteboardError, PasteboardService};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSelection {
    pub text: String,
    pub target_bundle_id: String,
}

pub struct ClipboardManagerCore {
    history: ClipboardHistory,
    pasteboard: PasteboardService,
    target_bundle_id: Option<String>,
    own_bundle_id: Option<String>,
}

impl ClipboardManagerCore {
    fn new_with_own_bundle(
        history: ClipboardHistory,
        pasteboard: PasteboardService,
        own_bundle_id: Option<String>,
    ) -> Self {
        Self {
            history,
            pasteboard,
            target_bundle_id: None,
            own_bundle_id,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(pasteboard: PasteboardService) -> Self {
        Self::new_with_own_bundle(ClipboardHistory::default(), pasteboard, None)
    }

    #[cfg(test)]
    pub fn history(&self) -> &ClipboardHistory {
        &self.history
    }

    pub fn poll_once(&mut self) -> Result<Option<IngestOutcome>, PasteboardError> {
        let Some(change) = self.pasteboard.poll()? else {
            return Ok(None);
        };
        let Some(text) = change.text else {
            return Ok(None);
        };
        let outcome = self.history.ingest(text, unix_ms());
        Ok(Some(outcome))
    }

    #[cfg(test)]
    pub fn capture_target(&mut self) -> Result<ForegroundApplication, PasteboardError> {
        let target = self.pasteboard.frontmost_application()?;
        self.capture_target_value(target)
    }

    fn capture_target_value(
        &mut self,
        target: Option<ForegroundApplication>,
    ) -> Result<ForegroundApplication, PasteboardError> {
        self.target_bundle_id = None;
        let target = target.ok_or(PasteboardError::ApplicationUnavailable)?;
        if self.own_bundle_id.as_deref() == Some(target.bundle_id.as_str()) {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        self.target_bundle_id = Some(target.bundle_id.clone());
        Ok(target)
    }

    pub fn select(&self, id: u64) -> Result<ClipboardSelection, String> {
        let entry = self
            .history
            .entries()
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| "剪贴板条目不存在".to_owned())?;
        let target_bundle_id = self
            .target_bundle_id
            .clone()
            .ok_or_else(|| "没有可恢复的目标应用".to_owned())?;
        Ok(ClipboardSelection {
            text: entry.text.clone(),
            target_bundle_id,
        })
    }

    fn write_text(&mut self, text: &str) -> Result<(), PasteboardError> {
        self.pasteboard.write_text(text).map(|_| ())
    }

    fn history_clone(&self) -> ClipboardHistory {
        self.history.clone()
    }

    #[cfg(test)]
    fn ingest_for_test(&mut self, text: &str) {
        self.history.ingest(text.to_owned(), unix_ms());
    }

    #[cfg(test)]
    fn set_target_for_test(&mut self, target: Option<String>) {
        self.target_bundle_id = target;
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipboardSnapshot {
    pub entries: Vec<ClipboardEntry>,
    pub panel_open: bool,
    pub last_error: Option<String>,
}

impl ClipboardSnapshot {
    fn from_history(
        history: &ClipboardHistory,
        panel_open: bool,
        last_error: Option<String>,
    ) -> Self {
        Self {
            entries: history.entries().to_vec(),
            panel_open,
            last_error,
        }
    }
}

#[derive(Debug)]
pub enum ClipboardManagerError {
    WorkerUnavailable,
    RequestFailed(String),
}

impl std::fmt::Display for ClipboardManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerUnavailable => write!(f, "剪贴板服务线程不可用"),
            Self::RequestFailed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ClipboardManagerError {}

enum WorkerCommand {
    Show,
    Select {
        id: u64,
        generation: u64,
        reply: Sender<Result<(), String>>,
    },
    Delete {
        id: u64,
        reply: Sender<Result<(), String>>,
    },
    Clear {
        reply: Sender<Result<(), String>>,
    },
    Close {
        reply: Sender<Result<(), String>>,
    },
}

pub struct ClipboardManager {
    sender: SyncSender<WorkerCommand>,
    snapshot: Arc<Mutex<ClipboardSnapshot>>,
    select_generation: Arc<AtomicU64>,
}

impl ClipboardManager {
    pub fn start(
        app: AppHandle,
        history_path: PathBuf,
    ) -> Result<Arc<Self>, ClipboardManagerError> {
        Self::start_with_marker(app, history_path, macos_adapter::next_marker())
    }

    pub fn start_with_marker(
        app: AppHandle,
        history_path: PathBuf,
        marker: u64,
    ) -> Result<Arc<Self>, ClipboardManagerError> {
        let (sender, receiver) = mpsc::sync_channel(32);
        let snapshot = Arc::new(Mutex::new(ClipboardSnapshot {
            entries: Vec::new(),
            panel_open: false,
            last_error: None,
        }));
        let worker_snapshot = Arc::clone(&snapshot);
        let select_generation = Arc::new(AtomicU64::new(0));
        let worker_select_generation = Arc::clone(&select_generation);
        thread::Builder::new()
            .name("mac-to-windows-clipboard".into())
            .spawn(move || {
                worker_loop(
                    app,
                    history_path,
                    receiver,
                    worker_snapshot,
                    worker_select_generation,
                    marker,
                )
            })
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
        Ok(Arc::new(Self {
            sender,
            snapshot,
            select_generation,
        }))
    }

    pub fn snapshot(&self) -> ClipboardSnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or(ClipboardSnapshot {
                entries: Vec::new(),
                panel_open: false,
                last_error: Some("剪贴板状态锁不可用".into()),
            })
    }

    pub fn try_show(&self) -> bool {
        self.sender.try_send(WorkerCommand::Show).is_ok()
    }

    pub fn select(&self, id: u64) -> Result<(), ClipboardManagerError> {
        let generation = self.select_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let result = self.request(|reply| WorkerCommand::Select {
            id,
            generation,
            reply,
        });
        if result.is_err() {
            self.select_generation.fetch_add(1, Ordering::AcqRel);
        }
        result
    }

    pub fn delete(&self, id: u64) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Delete { id, reply })
    }

    pub fn clear(&self) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Clear { reply })
    }

    pub fn close_panel(&self) -> Result<(), ClipboardManagerError> {
        self.select_generation.fetch_add(1, Ordering::AcqRel);
        let (reply, _response) = mpsc::channel();
        self.sender
            .try_send(WorkerCommand::Close { reply })
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)
    }

    fn request<F>(&self, build: F) -> Result<(), ClipboardManagerError>
    where
        F: FnOnce(Sender<Result<(), String>>) -> WorkerCommand,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.sender
            .try_send(build(reply_tx))
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?
            .map_err(ClipboardManagerError::RequestFailed)?;
        Ok(())
    }
}

fn worker_loop(
    app: AppHandle,
    history_path: PathBuf,
    receiver: Receiver<WorkerCommand>,
    snapshot: Arc<Mutex<ClipboardSnapshot>>,
    select_generation: Arc<AtomicU64>,
    marker: u64,
) {
    let (mut history, mut last_error) = match ClipboardStore::load_or_recover(&history_path) {
        Ok(LoadOutcome::Loaded(history)) => (history, None),
        Ok(LoadOutcome::RecoveredCorrupt { history, path }) => (
            history,
            Some(format!("历史文件损坏，已备份到 {}", path.display())),
        ),
        Err(error) => (
            ClipboardHistory::default(),
            Some(format!("加载剪贴板历史失败：{error}")),
        ),
    };
    let mut pasteboard = PasteboardService::start().ok();
    if pasteboard.is_none() && last_error.is_none() {
        last_error = Some("macOS 剪贴板服务不可用".into());
    }
    let own_bundle_id = app.config().identifier.clone();
    let mut core = pasteboard.take().map(|service| {
        ClipboardManagerCore::new_with_own_bundle(history.clone(), service, Some(own_bundle_id))
    });
    let mut panel_open = false;
    publish_snapshot(
        &app,
        &snapshot,
        &history,
        panel_open,
        last_error.as_deref(),
        "clipboard-updated",
    );

    let poll_interval = Duration::from_millis(100);
    let command_wakeup = Duration::from_millis(20);
    let mut next_poll = Instant::now();
    loop {
        let wait = next_poll
            .saturating_duration_since(Instant::now())
            .min(command_wakeup);
        match receiver.recv_timeout(wait) {
            Ok(command) => {
                let should_stop = handle_command(
                    &app,
                    &history_path,
                    &mut core,
                    &mut history,
                    &mut panel_open,
                    &mut last_error,
                    &snapshot,
                    &select_generation,
                    marker,
                    command,
                );
                if should_stop {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        if Instant::now() < next_poll {
            continue;
        }
        next_poll = Instant::now() + poll_interval;
        if let Some(core) = core.as_mut() {
            match core.poll_once() {
                Ok(Some(IngestOutcome::Added(_) | IngestOutcome::Moved(_))) => {
                    history = core.history_clone();
                    if let Err(error) = ClipboardStore::save_atomic(&history_path, &history) {
                        last_error = Some(format!("保存剪贴板历史失败：{error}"));
                    } else {
                        last_error = None;
                    }
                    publish_snapshot(
                        &app,
                        &snapshot,
                        &history,
                        panel_open,
                        last_error.as_deref(),
                        "clipboard-updated",
                    );
                }
                Ok(Some(IngestOutcome::IgnoredEmpty | IngestOutcome::IgnoredTooLarge))
                | Ok(None) => {}
                Err(error) => {
                    let message = format!("读取系统剪贴板失败：{error}");
                    let changed = last_error.as_deref() != Some(message.as_str());
                    last_error = Some(message);
                    if changed {
                        publish_snapshot(
                            &app,
                            &snapshot,
                            &history,
                            panel_open,
                            last_error.as_deref(),
                            "clipboard-updated",
                        );
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    app: &AppHandle,
    history_path: &PathBuf,
    core: &mut Option<ClipboardManagerCore>,
    history: &mut ClipboardHistory,
    panel_open: &mut bool,
    last_error: &mut Option<String>,
    snapshot: &Arc<Mutex<ClipboardSnapshot>>,
    select_generation: &Arc<AtomicU64>,
    marker: u64,
    command: WorkerCommand,
) -> bool {
    match command {
        WorkerCommand::Show => {
            if !*panel_open {
                *last_error = None;
                if let Some(core) = core.as_mut() {
                    core.target_bundle_id = None;
                    let result = frontmost_application_on_main_thread(app)
                        .map_err(|error| error.to_owned())
                        .and_then(|target| {
                            core.capture_target_value(target)
                                .map_err(|error| error.to_string())
                        });
                    if let Err(error) = result {
                        *last_error = Some(format!("无法记录前台应用：{error}"));
                    }
                }
                *panel_open = true;
            }
            publish_snapshot(
                app,
                snapshot,
                history,
                *panel_open,
                last_error.as_deref(),
                "clipboard-show",
            );
            show_window(app);
        }
        WorkerCommand::Select {
            id,
            generation,
            reply,
        } => {
            let result = select_and_paste(
                app,
                core,
                panel_open,
                last_error,
                select_generation,
                generation,
                marker,
                id,
            );
            if let Err(error) = result.as_ref() {
                if error == "操作已取消" {
                    *last_error = None;
                } else {
                    *last_error = Some(error.clone());
                }
            }
            publish_snapshot(
                app,
                snapshot,
                history,
                *panel_open,
                last_error.as_deref(),
                "clipboard-updated",
            );
            let _ = reply.send(result);
        }
        WorkerCommand::Delete { id, reply } => {
            let result = if history.entries().iter().any(|entry| entry.id == id) {
                let mut candidate = history.clone();
                candidate.remove(id);
                match ClipboardStore::save_atomic(history_path, &candidate) {
                    Ok(()) => {
                        *history = candidate;
                        if let Some(core) = core.as_mut() {
                            core.history = history.clone();
                        }
                        *last_error = None;
                        Ok(())
                    }
                    Err(error) => {
                        let message = format!("保存剪贴板历史失败：{error}");
                        *last_error = Some(message.clone());
                        Err(message)
                    }
                }
            } else {
                let message = "剪贴板条目不存在".to_owned();
                *last_error = Some(message.clone());
                Err(message)
            };
            publish_snapshot(
                app,
                snapshot,
                history,
                *panel_open,
                last_error.as_deref(),
                "clipboard-updated",
            );
            let _ = reply.send(result);
        }
        WorkerCommand::Clear { reply } => {
            let candidate = ClipboardHistory::default();
            let result = match ClipboardStore::save_atomic(history_path, &candidate) {
                Ok(()) => {
                    *history = candidate;
                    if let Some(core) = core.as_mut() {
                        core.history = history.clone();
                    }
                    *last_error = None;
                    Ok(())
                }
                Err(error) => {
                    let message = format!("保存剪贴板历史失败：{error}");
                    *last_error = Some(message.clone());
                    Err(message)
                }
            };
            publish_snapshot(
                app,
                snapshot,
                history,
                *panel_open,
                last_error.as_deref(),
                "clipboard-updated",
            );
            let _ = reply.send(result);
        }
        WorkerCommand::Close { reply } => {
            dismiss_panel(core, panel_open);
            let result = hide_window_and_wait(app);
            if let Err(error) = &result {
                *last_error = Some(error.clone());
            } else {
                *last_error = None;
            }
            publish_snapshot(
                app,
                snapshot,
                history,
                *panel_open,
                last_error.as_deref(),
                "clipboard-updated",
            );
            let _ = reply.send(result);
        }
    }
    false
}

fn dismiss_panel(core: &mut Option<ClipboardManagerCore>, panel_open: &mut bool) {
    if let Some(core) = core.as_mut() {
        core.target_bundle_id = None;
    }
    *panel_open = false;
}

#[allow(clippy::too_many_arguments)]
fn select_and_paste(
    app: &AppHandle,
    core: &mut Option<ClipboardManagerCore>,
    panel_open: &mut bool,
    last_error: &mut Option<String>,
    select_generation: &Arc<AtomicU64>,
    generation: u64,
    marker: u64,
    id: u64,
) -> Result<(), String> {
    if select_generation.load(Ordering::Acquire) != generation {
        *panel_open = false;
        return Err("操作已取消".into());
    }
    let Some(core) = core.as_mut() else {
        let error = "系统剪贴板服务不可用".to_owned();
        *last_error = Some(error.clone());
        return Err(error);
    };
    let selection = core.select(id)?;
    core.write_text(&selection.text)
        .map_err(|error| format!("写入系统剪贴板失败：{error}"))?;
    if select_generation.load(Ordering::Acquire) != generation {
        *panel_open = false;
        return Err("操作已取消".into());
    }
    if let Err(error) = hide_window_and_wait(app) {
        if select_generation.load(Ordering::Acquire) != generation {
            *panel_open = false;
            return Err("操作已取消".into());
        }
        *last_error = Some(error.clone());
        *panel_open = true;
        show_window(app);
        return Err(error);
    }
    if select_generation.load(Ordering::Acquire) != generation {
        *panel_open = false;
        return Err("操作已取消".into());
    }
    if let Err(message) = activate_application_on_main_thread(
        app,
        &selection.target_bundle_id,
        Arc::clone(select_generation),
        generation,
    ) {
        if select_generation.load(Ordering::Acquire) != generation {
            *panel_open = false;
            return Err("操作已取消".into());
        }
        *last_error = Some(message.clone());
        show_window(app);
        return Err(message);
    }
    if select_generation.load(Ordering::Acquire) != generation {
        *panel_open = false;
        return Err("操作已取消".into());
    }
    if let Err(error) = wait_for_frontmost(
        app,
        &selection.target_bundle_id,
        select_generation,
        generation,
    ) {
        if select_generation.load(Ordering::Acquire) != generation {
            *panel_open = false;
            return Err("操作已取消".into());
        }
        *last_error = Some(error.clone());
        *panel_open = true;
        show_window(app);
        return Err(error);
    }
    if select_generation.load(Ordering::Acquire) != generation {
        *panel_open = false;
        return Err("操作已取消".into());
    }
    thread::sleep(Duration::from_millis(50));
    if select_generation.load(Ordering::Acquire) != generation {
        *panel_open = false;
        return Err("操作已取消".into());
    }
    let outputs = [
        event_model::OutputEvent::Key {
            code: event_model::PhysicalCode::key(9),
            phase: event_model::EventPhase::Down,
            modifiers: event_model::ModifierSet::LEFT_COMMAND,
        },
        event_model::OutputEvent::Key {
            code: event_model::PhysicalCode::key(9),
            phase: event_model::EventPhase::Up,
            modifiers: event_model::ModifierSet::LEFT_COMMAND,
        },
    ];
    if let Err(error) = macos_adapter::inject_outputs(&outputs, marker) {
        if select_generation.load(Ordering::Acquire) != generation {
            *panel_open = false;
            return Err("操作已取消".into());
        }
        let message = format!("自动粘贴失败：{error}");
        *last_error = Some(message.clone());
        *panel_open = true;
        show_window(app);
        return Err(message);
    }
    *panel_open = false;
    *last_error = None;
    core.target_bundle_id = None;
    Ok(())
}

fn run_on_main_thread_result<T, F>(app: &AppHandle, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let closure_cancelled = Arc::clone(&cancelled);
    app.run_on_main_thread(move || {
        if closure_cancelled.load(Ordering::Acquire) {
            return;
        }
        let _ = result_tx.send(operation());
    })
    .map_err(|error| format!("主线程操作失败：{error}"))?;
    match result_rx.recv_timeout(Duration::from_millis(500)) {
        Ok(value) => Ok(value),
        Err(error) => {
            cancelled.store(true, Ordering::Release);
            Err(format!("主线程操作超时：{error}"))
        }
    }
}

fn frontmost_application_on_main_thread(
    app: &AppHandle,
) -> Result<Option<ForegroundApplication>, String> {
    run_on_main_thread_result(app, macos_adapter::frontmost_application)?
        .map_err(|error| format!("读取前台应用失败：{error}"))
}

fn activate_application_on_main_thread(
    app: &AppHandle,
    bundle_id: &str,
    select_generation: Arc<AtomicU64>,
    generation: u64,
) -> Result<(), String> {
    let bundle_id = bundle_id.to_owned();
    run_on_main_thread_result(app, move || {
        if select_generation.load(Ordering::Acquire) != generation {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        macos_adapter::activate_application(&bundle_id)
    })?
    .map_err(|error| format!("恢复前台应用失败：{error}"))
}

fn wait_for_frontmost(
    app: &AppHandle,
    target_bundle_id: &str,
    select_generation: &Arc<AtomicU64>,
    generation: u64,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(1_000);
    loop {
        if select_generation.load(Ordering::Acquire) != generation {
            return Err("操作已取消".into());
        }
        match frontmost_application_on_main_thread(app)? {
            Some(application) if application.bundle_id == target_bundle_id => return Ok(()),
            Some(_) | None => {}
        }
        if Instant::now() >= deadline {
            return Err("目标应用未能成为前台应用".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn publish_snapshot(
    app: &AppHandle,
    snapshot: &Arc<Mutex<ClipboardSnapshot>>,
    history: &ClipboardHistory,
    panel_open: bool,
    last_error: Option<&str>,
    event: &str,
) {
    let value = ClipboardSnapshot::from_history(history, panel_open, last_error.map(str::to_owned));
    if let Ok(mut current) = snapshot.lock() {
        *current = value.clone();
    }
    let _ = app.emit_to("clipboard", event, value);
}

fn show_window(app: &AppHandle) {
    let handle = app.clone();
    let closure_handle = handle.clone();
    let _ = handle.run_on_main_thread(move || {
        // Keep the settings window out of the clipboard flow. The clipboard
        // service remains alive in its worker thread; only its own window is
        // shown for the duration of the interaction.
        if let Some(main_window) = closure_handle.get_webview_window("main") {
            let _ = main_window.hide();
        }
        if let Some(window) = closure_handle.get_webview_window("clipboard") {
            let _ = window.show();
            let _ = window.center();
            let _ = window.set_focus();
        }
    });
}

fn hide_window_and_wait(app: &AppHandle) -> Result<(), String> {
    let handle = app.clone();
    let closure_handle = handle.clone();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    handle
        .run_on_main_thread(move || {
            if let Some(window) = closure_handle.get_webview_window("clipboard") {
                let _ = window.hide();
            }
            let _ = done_tx.send(());
        })
        .map_err(|error| format!("主线程隐藏剪贴板窗口失败：{error}"))?;
    done_rx
        .recv_timeout(Duration::from_millis(200))
        .map_err(|error| format!("主线程隐藏剪贴板窗口超时：{error}"))
}

#[cfg(test)]
mod tests {
    use super::{dismiss_panel, ClipboardManagerCore, ClipboardSelection};
    use clipboard_core::{ClipboardHistory, IngestOutcome};
    use macos_adapter::{PasteboardBackend, PasteboardError, PasteboardService};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockState {
        count: i64,
        text: Option<String>,
        frontmost: Option<String>,
        activated: Vec<String>,
    }

    struct MockBackend {
        state: Arc<Mutex<MockState>>,
    }

    #[test]
    fn poll_once_ingests_changed_text_and_tracks_frontmost_app() {
        let state = Arc::new(Mutex::new(MockState {
            count: 1,
            text: Some("alpha".into()),
            frontmost: Some("com.example.editor".into()),
            activated: Vec::new(),
        }));
        let backend = MockBackend {
            state: Arc::clone(&state),
        };
        let service = PasteboardService::with_backend(Box::new(backend));
        let mut core = ClipboardManagerCore::new_for_test(service);
        assert!(core.poll_once().unwrap().is_none());
        {
            let mut state = state.lock().unwrap();
            state.count = 2;
            state.text = Some("alpha".into());
        }
        assert!(matches!(
            core.poll_once().unwrap(),
            Some(IngestOutcome::Added(_))
        ));
        assert_eq!(core.history().entries()[0].text, "alpha");
        let target = core.capture_target().unwrap();
        assert_eq!(target.bundle_id, "com.example.editor");
    }

    #[test]
    fn selecting_item_returns_text_and_target_without_mutating_history() {
        let state = Arc::new(Mutex::new(MockState {
            frontmost: Some("com.example.editor".into()),
            ..Default::default()
        }));
        let service = PasteboardService::with_backend(Box::new(MockBackend { state }));
        let mut core = ClipboardManagerCore::new_for_test(service);
        core.ingest_for_test("alpha");
        let id = core.history().entries()[0].id;
        core.set_target_for_test(Some("com.example.editor".into()));
        assert_eq!(
            core.select(id).unwrap(),
            ClipboardSelection {
                text: "alpha".into(),
                target_bundle_id: "com.example.editor".into()
            }
        );
        assert_eq!(core.history().entries().len(), 1);
    }

    #[test]
    fn selecting_unknown_item_returns_error() {
        let service = PasteboardService::with_backend(Box::new(MockBackend {
            state: Arc::new(Mutex::new(MockState::default())),
        }));
        let core = ClipboardManagerCore::new_for_test(service);
        assert!(core.select(99).is_err());
    }

    #[test]
    fn failed_target_capture_invalidates_previous_target() {
        let state = Arc::new(Mutex::new(MockState {
            frontmost: Some("com.example.editor".into()),
            ..Default::default()
        }));
        let service = PasteboardService::with_backend(Box::new(MockBackend {
            state: Arc::clone(&state),
        }));
        let mut core = ClipboardManagerCore::new_for_test(service);
        core.ingest_for_test("alpha");
        let id = core.history().entries()[0].id;
        assert!(core.capture_target().is_ok());
        state.lock().unwrap().frontmost = None;

        assert!(core.capture_target().is_err());
        assert!(core.select(id).is_err());
    }

    #[test]
    fn own_application_is_never_captured_as_paste_target() {
        let state = Arc::new(Mutex::new(MockState {
            frontmost: Some("com.mactowindows.desktop".into()),
            ..Default::default()
        }));
        let service = PasteboardService::with_backend(Box::new(MockBackend { state }));
        let mut core = ClipboardManagerCore::new_with_own_bundle(
            ClipboardHistory::default(),
            service,
            Some("com.mactowindows.desktop".into()),
        );

        assert_eq!(
            core.capture_target().unwrap_err(),
            PasteboardError::ApplicationUnavailable
        );
    }

    #[test]
    fn dismissing_panel_only_closes_and_drops_target_application() {
        let state = Arc::new(Mutex::new(MockState::default()));
        let service = PasteboardService::with_backend(Box::new(MockBackend { state }));
        let mut core = ClipboardManagerCore::new_for_test(service);
        core.set_target_for_test(Some("com.example.editor".into()));
        let mut panel_open = true;

        let mut core = Some(core);
        dismiss_panel(&mut core, &mut panel_open);

        assert!(!panel_open);
        assert!(core.as_ref().unwrap().target_bundle_id.is_none());
    }

    impl PasteboardBackend for MockBackend {
        fn change_count(&mut self) -> Result<i64, PasteboardError> {
            Ok(self.state.lock().unwrap().count)
        }
        fn read_text(&mut self) -> Result<Option<String>, PasteboardError> {
            Ok(self.state.lock().unwrap().text.clone())
        }
        fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError> {
            let mut state = self.state.lock().unwrap();
            state.count += 1;
            state.text = Some(text.to_owned());
            Ok(state.count)
        }
        fn frontmost_bundle_id(&mut self) -> Result<Option<String>, PasteboardError> {
            Ok(self.state.lock().unwrap().frontmost.clone())
        }
        fn activate_bundle_id(&mut self, bundle_id: &str) -> Result<(), PasteboardError> {
            self.state
                .lock()
                .unwrap()
                .activated
                .push(bundle_id.to_owned());
            Ok(())
        }
    }
}
