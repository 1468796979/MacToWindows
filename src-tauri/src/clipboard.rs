use clipboard_core::{
    ClipboardEntry, ClipboardHistory, ClipboardStore, IngestOutcome, LoadOutcome,
};
use macos_adapter::{ForegroundApplication, PasteboardError, PasteboardService};
use serde::Serialize;
use std::path::PathBuf;
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

    pub fn capture_target(&mut self) -> Result<ForegroundApplication, PasteboardError> {
        self.target_bundle_id = None;
        let target = self
            .pasteboard
            .frontmost_application()?
            .ok_or(PasteboardError::ApplicationUnavailable)?;
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

    fn activate_application(&mut self, bundle_id: &str) -> Result<(), PasteboardError> {
        self.pasteboard.activate_application(bundle_id)
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
        thread::Builder::new()
            .name("mac-to-windows-clipboard".into())
            .spawn(move || worker_loop(app, history_path, receiver, worker_snapshot, marker))
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
        Ok(Arc::new(Self { sender, snapshot }))
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
        self.request(|reply| WorkerCommand::Select { id, reply })
    }

    pub fn delete(&self, id: u64) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Delete { id, reply })
    }

    pub fn clear(&self) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Clear { reply })
    }

    pub fn close_panel(&self) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Close { reply })
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

    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => {
                let should_stop = handle_command(
                    &app,
                    &history_path,
                    &mut core,
                    &mut history,
                    &mut panel_open,
                    &mut last_error,
                    &snapshot,
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
                    last_error = Some(format!("读取系统剪贴板失败：{error}"));
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

#[allow(clippy::too_many_arguments)]
fn handle_command(
    app: &AppHandle,
    history_path: &PathBuf,
    core: &mut Option<ClipboardManagerCore>,
    history: &mut ClipboardHistory,
    panel_open: &mut bool,
    last_error: &mut Option<String>,
    snapshot: &Arc<Mutex<ClipboardSnapshot>>,
    marker: u64,
    command: WorkerCommand,
) -> bool {
    match command {
        WorkerCommand::Show => {
            if !*panel_open {
                if let Some(core) = core.as_mut() {
                    if let Err(error) = core.capture_target() {
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
        WorkerCommand::Select { id, reply } => {
            let result = select_and_paste(app, core, panel_open, last_error, marker, id);
            let succeeded = result.is_ok();
            let _ = reply.send(result);
            if succeeded {
                publish_snapshot(
                    app,
                    snapshot,
                    history,
                    *panel_open,
                    last_error.as_deref(),
                    "clipboard-updated",
                );
            }
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
            *last_error = None;
            hide_window(app);
            publish_snapshot(
                app,
                snapshot,
                history,
                *panel_open,
                last_error.as_deref(),
                "clipboard-updated",
            );
            let _ = reply.send(Ok(()));
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

fn select_and_paste(
    app: &AppHandle,
    core: &mut Option<ClipboardManagerCore>,
    panel_open: &mut bool,
    last_error: &mut Option<String>,
    marker: u64,
    id: u64,
) -> Result<(), String> {
    let Some(core) = core.as_mut() else {
        let error = "系统剪贴板服务不可用".to_owned();
        *last_error = Some(error.clone());
        return Err(error);
    };
    let selection = core.select(id)?;
    core.write_text(&selection.text)
        .map_err(|error| format!("写入系统剪贴板失败：{error}"))?;
    hide_window(app);
    thread::sleep(Duration::from_millis(15));
    if let Err(error) = core.activate_application(&selection.target_bundle_id) {
        let message = format!("恢复前台应用失败：{error}");
        *last_error = Some(message.clone());
        show_window(app);
        return Err(message);
    }
    if let Err(error) = wait_for_frontmost(core, &selection.target_bundle_id) {
        *last_error = Some(error.clone());
        *panel_open = true;
        show_window(app);
        return Err(error);
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

fn wait_for_frontmost(
    core: &mut ClipboardManagerCore,
    target_bundle_id: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        match core.pasteboard.frontmost_application() {
            Ok(Some(application)) if application.bundle_id == target_bundle_id => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(format!("读取前台应用失败：{error}")),
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

fn hide_window(app: &AppHandle) {
    let handle = app.clone();
    let closure_handle = handle.clone();
    let _ = handle.run_on_main_thread(move || {
        if let Some(window) = closure_handle.get_webview_window("clipboard") {
            let _ = window.hide();
        }
    });
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
