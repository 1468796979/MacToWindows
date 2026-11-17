use clipboard_core::{
    ClipboardEntry, ClipboardHistory, ClipboardStore, IngestOutcome, LoadOutcome, StoreError,
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
    pub persistence_pending: bool,
    pub storage_error: Option<String>,
    pub read_error: Option<String>,
    pub operation_error: Option<String>,
}

#[derive(Default, Debug, PartialEq, Eq, Clone)]
struct ClipboardErrors {
    storage: Option<String>,
    read: Option<String>,
    operation: Option<String>,
}

impl ClipboardErrors {
    fn message(&self) -> Option<String> {
        let messages: Vec<&str> = [&self.storage, &self.read, &self.operation]
            .into_iter()
            .filter_map(|message| message.as_deref())
            .collect();
        (!messages.is_empty()).then(|| messages.join("\n"))
    }
}

/// The worker owns the current history. Disk is loaded once; a failed save must
/// never turn a UI refresh into a replacement with an older persisted snapshot.
struct HistoryState {
    history: ClipboardHistory,
    dirty: bool,
    next_retry_at: Option<Instant>,
    retry_step: usize,
    storage_ready: bool,
    errors: ClipboardErrors,
}

impl HistoryState {
    fn from_load(result: Result<LoadOutcome, StoreError>) -> Self {
        let (history, storage_error, storage_ready) = match result {
            Ok(LoadOutcome::Loaded(history)) => (history, None, true),
            Ok(LoadOutcome::RecoveredCorrupt { history, path }) => (
                history,
                Some(format!("历史文件损坏，已备份到 {}", path.display())),
                true,
            ),
            Err(error) => (
                ClipboardHistory::default(),
                Some(format!(
                    "加载剪贴板历史失败：{error}；已暂停写入，请修复文件后重启应用"
                )),
                false,
            ),
        };
        Self {
            history,
            dirty: false,
            next_retry_at: None,
            retry_step: 0,
            storage_ready,
            errors: ClipboardErrors {
                storage: storage_error,
                ..ClipboardErrors::default()
            },
        }
    }

    fn snapshot(&self, panel_open: bool) -> ClipboardSnapshot {
        ClipboardSnapshot {
            entries: self.history.entries().to_vec(),
            panel_open,
            last_error: self.errors.message(),
            persistence_pending: self.dirty,
            storage_error: self.errors.storage.clone(),
            read_error: self.errors.read.clone(),
            operation_error: self.errors.operation.clone(),
        }
    }

    fn collected(&mut self, history: ClipboardHistory, now: Instant) {
        self.history = history;
        if !self.dirty {
            self.next_retry_at = Some(now);
        }
        self.dirty = true;
    }

    fn saved(&mut self) {
        self.dirty = false;
        self.next_retry_at = None;
        self.retry_step = 0;
        self.errors.storage = None;
    }

    fn save_current<F>(&mut self, now: Instant, save: F) -> Result<(), String>
    where
        F: FnOnce(&ClipboardHistory) -> Result<(), String>,
    {
        if !self.dirty {
            return Ok(());
        }
        self.ensure_storage_ready()?;
        match save(&self.history) {
            Ok(()) => {
                self.saved();
                Ok(())
            }
            Err(error) => {
                const RETRY_SECONDS: [u64; 6] = [1, 2, 4, 8, 16, 30];
                let delay = RETRY_SECONDS[self.retry_step.min(RETRY_SECONDS.len() - 1)];
                self.retry_step = (self.retry_step + 1).min(RETRY_SECONDS.len() - 1);
                self.next_retry_at = Some(now + Duration::from_secs(delay));
                self.errors.storage = Some(error.clone());
                Err(error)
            }
        }
    }

    fn retry_if_due<F>(&mut self, now: Instant, save: F) -> bool
    where
        F: FnOnce(&ClipboardHistory) -> Result<(), String>,
    {
        if !self.storage_ready
            || !self.dirty
            || self.next_retry_at.is_some_and(|deadline| now < deadline)
        {
            return false;
        }
        let before = (self.dirty, self.errors.clone());
        let _ = self.save_current(now, save);
        before != (self.dirty, self.errors.clone())
    }

    fn ensure_storage_ready(&self) -> Result<(), String> {
        if self.storage_ready {
            Ok(())
        } else {
            Err(self.errors.storage.clone().unwrap_or_else(|| {
                "历史文件未成功加载，已暂停写入以保护原文件；请修复文件后重启应用".to_owned()
            }))
        }
    }

    fn commit_candidate<F>(&mut self, candidate: ClipboardHistory, save: F) -> Result<(), String>
    where
        F: FnOnce(&ClipboardHistory) -> Result<(), String>,
    {
        self.ensure_storage_ready()?;
        match save(&candidate) {
            Ok(()) => {
                self.history = candidate;
                self.saved();
                self.errors.operation = None;
                Ok(())
            }
            Err(error) => {
                // Keep the original current history and retry schedule. A
                // rejected delete/clear must not become a deferred mutation.
                self.errors.storage = Some(error.clone());
                Err(error)
            }
        }
    }

    fn delete<F>(&mut self, id: u64, save: F) -> Result<(), String>
    where
        F: FnOnce(&ClipboardHistory) -> Result<(), String>,
    {
        let mut candidate = self.history.clone();
        if !candidate.remove(id) {
            let message = "剪贴板条目不存在".to_owned();
            self.errors.operation = Some(message.clone());
            return Err(message);
        }
        self.commit_candidate(candidate, save)
    }

    fn clear<F>(&mut self, save: F) -> Result<(), String>
    where
        F: FnOnce(&ClipboardHistory) -> Result<(), String>,
    {
        let mut candidate = self.history.clone();
        candidate.clear();
        self.commit_candidate(candidate, save)
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
    Refresh {
        reply: Sender<Result<(), String>>,
    },
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
    Shutdown {
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
            persistence_pending: false,
            storage_error: None,
            read_error: None,
            operation_error: None,
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
                persistence_pending: false,
                storage_error: None,
                read_error: None,
                operation_error: None,
            })
    }

    pub fn last_error(&self) -> Option<String> {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.last_error.clone())
            .unwrap_or_else(|_| Some("剪贴板状态锁不可用".into()))
    }

    pub fn try_show(&self) -> bool {
        self.sender.try_send(WorkerCommand::Show).is_ok()
    }

    pub fn refresh(&self) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Refresh { reply })
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

    /// Cancel selection, attempt one final save, and stop the worker. Invoke
    /// from a background cleanup task so AppKit work can finish on the UI thread.
    pub fn shutdown(&self) -> Result<(), ClipboardManagerError> {
        self.select_generation.fetch_add(1, Ordering::AcqRel);
        self.request(|reply| WorkerCommand::Shutdown { reply })
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

fn save_history(path: &PathBuf, history: &ClipboardHistory) -> Result<(), String> {
    ClipboardStore::save_atomic(path, history)
        .map_err(|error| format!("保存剪贴板历史失败：{error}"))
}

fn worker_loop(
    app: AppHandle,
    history_path: PathBuf,
    receiver: Receiver<WorkerCommand>,
    snapshot: Arc<Mutex<ClipboardSnapshot>>,
    select_generation: Arc<AtomicU64>,
    marker: u64,
) {
    let mut state = HistoryState::from_load(ClipboardStore::load_or_recover(&history_path));
    let pasteboard = match PasteboardService::start() {
        Ok(service) => Some(service),
        Err(error) => {
            state.errors.read = Some(format!("macOS 剪贴板服务不可用：{error}"));
            None
        }
    };
    let own_bundle_id = app.config().identifier.clone();
    let mut core = pasteboard.map(|service| {
        ClipboardManagerCore::new_with_own_bundle(
            state.history.clone(),
            service,
            Some(own_bundle_id),
        )
    });
    let mut panel_open = false;
    publish_snapshot(&app, &snapshot, &state, panel_open, "clipboard-updated");

    let poll_interval = Duration::from_millis(25);
    let command_wakeup = Duration::from_millis(20);
    let mut next_poll = Instant::now();
    loop {
        let wait = next_poll
            .saturating_duration_since(Instant::now())
            .min(command_wakeup);
        match receiver.recv_timeout(wait) {
            Ok(command) => {
                if handle_command(
                    &app,
                    &history_path,
                    &mut core,
                    &mut state,
                    &mut panel_open,
                    &snapshot,
                    &select_generation,
                    marker,
                    command,
                ) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = state.save_current(Instant::now(), |history| {
                    save_history(&history_path, history)
                });
                return;
            }
        }

        if Instant::now() < next_poll {
            continue;
        }
        next_poll = Instant::now() + poll_interval;
        if refresh_memory_history(&mut core, &mut state, Instant::now(), |history| {
            save_history(&history_path, history)
        }) {
            publish_snapshot(&app, &snapshot, &state, panel_open, "clipboard-updated");
        }
    }
}

fn refresh_memory_history<F>(
    core: &mut Option<ClipboardManagerCore>,
    state: &mut HistoryState,
    now: Instant,
    save: F,
) -> bool
where
    F: FnOnce(&ClipboardHistory) -> Result<(), String>,
{
    let mut changed = false;
    if let Some(core) = core.as_mut() {
        match core.poll_once() {
            Ok(outcome) => {
                changed |= state.errors.read.take().is_some();
                if matches!(
                    outcome,
                    Some(IngestOutcome::Added(_) | IngestOutcome::Moved(_))
                ) {
                    state.collected(core.history_clone(), now);
                    changed = true;
                }
            }
            Err(error) => {
                let message = format!("读取系统剪贴板失败：{error}");
                changed |= state.errors.read.as_deref() != Some(message.as_str());
                state.errors.read = Some(message);
            }
        }
    }
    state.retry_if_due(now, save) || changed
}

fn sync_core_history(core: &mut Option<ClipboardManagerCore>, state: &HistoryState) {
    if let Some(core) = core.as_mut() {
        core.history = state.history.clone();
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    app: &AppHandle,
    history_path: &PathBuf,
    core: &mut Option<ClipboardManagerCore>,
    state: &mut HistoryState,
    panel_open: &mut bool,
    snapshot: &Arc<Mutex<ClipboardSnapshot>>,
    select_generation: &Arc<AtomicU64>,
    marker: u64,
    command: WorkerCommand,
) -> bool {
    match command {
        WorkerCommand::Show => {
            refresh_memory_history(core, state, Instant::now(), |history| {
                save_history(history_path, history)
            });
            if !*panel_open {
                state.errors.operation = None;
                if let Some(core) = core.as_mut() {
                    core.target_bundle_id = None;
                    let result = frontmost_application_on_main_thread(app).and_then(|target| {
                        core.capture_target_value(target)
                            .map_err(|error| error.to_string())
                    });
                    if let Err(error) = result {
                        if should_report_show_capture_error(&error) {
                            state.errors.operation = Some(format!("无法记录前台应用：{error}"));
                        }
                    }
                }
                *panel_open = true;
            }
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-show");
            show_window(app);
        }
        WorkerCommand::Refresh { reply } => {
            refresh_memory_history(core, state, Instant::now(), |history| {
                save_history(history_path, history)
            });
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-updated");
            // A readable in-memory snapshot remains useful while disk is failing.
            let _ = reply.send(Ok(()));
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
                &mut state.errors.operation,
                select_generation,
                generation,
                marker,
                id,
            );
            if let Err(error) = result.as_ref() {
                state.errors.operation = (error != "操作已取消").then(|| error.clone());
            }
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-updated");
            let _ = reply.send(result);
        }
        WorkerCommand::Delete { id, reply } => {
            let result = state.delete(id, |history| save_history(history_path, history));
            if result.is_ok() {
                sync_core_history(core, state);
            }
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-updated");
            let _ = reply.send(result);
        }
        WorkerCommand::Clear { reply } => {
            let result = state.clear(|history| save_history(history_path, history));
            if result.is_ok() {
                sync_core_history(core, state);
            }
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-updated");
            let _ = reply.send(result);
        }
        WorkerCommand::Close { reply } => {
            dismiss_panel(core, panel_open);
            let result = hide_window_and_wait(app);
            state.errors.operation = result.as_ref().err().cloned();
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-updated");
            let _ = reply.send(result);
        }
        WorkerCommand::Shutdown { reply } => {
            dismiss_panel(core, panel_open);
            let result = state.save_current(Instant::now(), |history| {
                save_history(history_path, history)
            });
            publish_snapshot(app, snapshot, state, *panel_open, "clipboard-updated");
            let _ = reply.send(result);
            return true;
        }
    }
    false
}

fn should_report_show_capture_error(error: &str) -> bool {
    error != PasteboardError::ApplicationUnavailable.to_string()
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
    state: &HistoryState,
    panel_open: bool,
    event: &str,
) {
    let value = state.snapshot(panel_open);
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
    use super::{
        dismiss_panel, refresh_memory_history, sync_core_history, ClipboardManagerCore,
        ClipboardSelection, HistoryState,
    };
    use clipboard_core::{ClipboardHistory, IngestOutcome};
    use macos_adapter::{PasteboardBackend, PasteboardError, PasteboardService};
    use std::cell::{Cell, RefCell};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

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
    fn showing_without_external_frontmost_application_does_not_report_error() {
        assert!(!super::should_report_show_capture_error(
            "ApplicationUnavailable"
        ));
        assert!(super::should_report_show_capture_error(
            "读取前台应用失败：主线程操作超时"
        ));
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

    fn history_state_with(texts: &[&str]) -> HistoryState {
        let mut history = ClipboardHistory::default();
        for (index, text) in texts.iter().enumerate() {
            history.ingest((*text).into(), index as u64);
        }
        HistoryState::from_load(Ok(clipboard_core::LoadOutcome::Loaded(history)))
    }

    fn mock_core(
        history: &ClipboardHistory,
    ) -> (Option<ClipboardManagerCore>, Arc<Mutex<MockState>>) {
        let backend = Arc::new(Mutex::new(MockState::default()));
        let service = PasteboardService::with_backend(Box::new(MockBackend {
            state: Arc::clone(&backend),
        }));
        (
            Some(ClipboardManagerCore::new_with_own_bundle(
                history.clone(),
                service,
                None,
            )),
            backend,
        )
    }

    #[test]
    fn refresh_keeps_unsaved_entries_and_retries_without_another_copy() {
        let mut state = history_state_with(&["alpha"]);
        let (mut core, backend) = mock_core(&state.history);
        let now = Instant::now();
        let attempts = Cell::new(0);
        refresh_memory_history(&mut core, &mut state, now, |_| Ok(()));
        {
            let mut backend = backend.lock().unwrap();
            backend.count += 1;
            backend.text = Some("beta".into());
        }
        refresh_memory_history(&mut core, &mut state, now, |_| {
            attempts.set(attempts.get() + 1);
            Err("disk unavailable".into())
        });
        assert_eq!(state.snapshot(true).entries.len(), 2);
        assert!(state.snapshot(true).persistence_pending);
        assert_eq!(state.errors.storage.as_deref(), Some("disk unavailable"));
        // Both Show and Refresh use this same in-memory refresh operation.
        for elapsed in [100, 500, 999] {
            refresh_memory_history(
                &mut core,
                &mut state,
                now + Duration::from_millis(elapsed),
                |_| {
                    attempts.set(attempts.get() + 1);
                    Ok(())
                },
            );
            assert_eq!(state.history.entries().len(), 2);
            assert!(state.dirty);
        }
        assert_eq!(attempts.get(), 1);
        let saved = RefCell::new(None);
        refresh_memory_history(
            &mut core,
            &mut state,
            now + Duration::from_secs(1),
            |history| {
                attempts.set(attempts.get() + 1);
                *saved.borrow_mut() = Some(history.clone());
                Ok(())
            },
        );
        assert_eq!(attempts.get(), 2);
        assert_eq!(saved.borrow().as_ref().unwrap().entries()[0].text, "beta");
        assert!(!state.dirty);
        assert!(state.errors.storage.is_none());
        assert_eq!(core.as_ref().unwrap().history(), &state.history);
    }

    #[test]
    fn retry_backoff_is_capped_and_always_saves_latest_history() {
        let mut state = history_state_with(&["alpha"]);
        let start = Instant::now();
        state.collected(state.history.clone(), start);
        let mut now = start;
        for delay in [1, 2, 4, 8, 16, 30, 30] {
            state.retry_if_due(now, |_| Err("disk unavailable".into()));
            assert_eq!(state.next_retry_at, Some(now + Duration::from_secs(delay)));
            let called = Cell::new(false);
            state.retry_if_due(now + Duration::from_millis(delay * 1000 - 1), |_| {
                called.set(true);
                Ok(())
            });
            assert!(!called.get());
            now += Duration::from_secs(delay);
        }
        let mut latest = state.history.clone();
        latest.ingest("beta".into(), 2);
        latest.ingest("gamma".into(), 3);
        state.collected(latest, now);
        state.retry_if_due(now, |history| {
            let texts: Vec<_> = history
                .entries()
                .iter()
                .map(|entry| entry.text.as_str())
                .collect();
            assert_eq!(texts, ["gamma", "beta", "alpha"]);
            Ok(())
        });
        assert!(!state.dirty);
        assert_eq!(state.retry_step, 0);
    }

    #[test]
    fn delete_and_clear_commit_latest_dirty_history_only_after_save_succeeds() {
        let mut state = history_state_with(&["alpha", "beta"]);
        let now = Instant::now();
        state.collected(state.history.clone(), now);
        state
            .save_current(now, |_| Err("offline".into()))
            .unwrap_err();
        let original = state.history.clone();
        let beta_id = original.entries()[0].id;
        let retry_at = state.next_retry_at;
        state
            .delete(beta_id, |_| Err("offline delete".into()))
            .unwrap_err();
        assert_eq!(state.history, original);
        assert_eq!(state.next_retry_at, retry_at);
        assert!(state.dirty);
        // An automatic retry saves the original history, not the rejected delete.
        state.retry_if_due(retry_at.unwrap(), |history| {
            assert_eq!(history, &original);
            Err("offline".into())
        });
        state.clear(|_| Err("offline clear".into())).unwrap_err();
        assert_eq!(state.history, original);
        state
            .delete(beta_id, |candidate| {
                assert_eq!(candidate.entries().len(), 1);
                assert_eq!(candidate.entries()[0].text, "alpha");
                Ok(())
            })
            .unwrap();
        assert!(!state.dirty);
        let (mut core, _) = mock_core(&original);
        sync_core_history(&mut core, &state);
        assert_eq!(core.as_ref().unwrap().history(), &state.history);
        let next_id = state.history.next_id;
        state
            .clear(|candidate| {
                assert!(candidate.entries().is_empty());
                assert_eq!(candidate.next_id, next_id);
                Ok(())
            })
            .unwrap();
        let called = Cell::new(false);
        state.retry_if_due(now + Duration::from_secs(100), |_| {
            called.set(true);
            Ok(())
        });
        assert!(!called.get());
        assert_eq!(
            state.history.ingest("new".into(), 3),
            IngestOutcome::Added(next_id)
        );
    }

    #[test]
    fn failed_clean_delete_is_not_queued_for_retry() {
        let mut state = history_state_with(&["alpha"]);
        let id = state.history.entries()[0].id;
        state.delete(id, |_| Err("offline".into())).unwrap_err();
        let called = Cell::new(false);
        state.retry_if_due(Instant::now() + Duration::from_secs(100), |_| {
            called.set(true);
            Ok(())
        });
        assert!(!called.get());
        assert_eq!(state.history.entries()[0].id, id);
    }

    #[test]
    fn error_categories_survive_unrelated_success_and_refresh() {
        let mut state = history_state_with(&["alpha"]);
        let now = Instant::now();
        state.collected(state.history.clone(), now);
        state
            .save_current(now, |_| Err("save error".into()))
            .unwrap_err();
        state.errors.operation = Some("paste error".into());
        // Successful paste, cancelled selection, opening and closing only clear
        // the operation slot; storage errors remain visible in snapshots.
        state.errors.operation = None;
        assert!(state
            .snapshot(false)
            .last_error
            .unwrap()
            .contains("save error"));
        let (mut core, backend) = mock_core(&state.history);
        backend.lock().unwrap().count = -1;
        refresh_memory_history(&mut core, &mut state, now, |_| Ok(()));
        assert!(state.errors.read.is_some());
        assert_eq!(state.errors.storage.as_deref(), Some("save error"));
        state.save_current(now, |_| Ok(())).unwrap();
        assert!(state.errors.storage.is_none());
        assert!(state.errors.read.is_some());
        backend.lock().unwrap().count = 0;
        refresh_memory_history(&mut core, &mut state, now, |_| Ok(()));
        assert!(state.errors.read.is_none());
    }

    #[test]
    fn startup_load_failure_blocks_overwriting_unknown_existing_history() {
        let mut state =
            HistoryState::from_load(Err(clipboard_core::StoreError::UnsupportedSchema(2)));
        let now = Instant::now();
        let mut pending = state.history.clone();
        pending.ingest("new".into(), 1);
        state.collected(pending, now);
        let called = Cell::new(false);
        assert!(state
            .save_current(now, |_| {
                called.set(true);
                Ok(())
            })
            .is_err());
        assert!(!called.get());
        assert!(state.dirty);
        assert!(state
            .snapshot(true)
            .last_error
            .unwrap()
            .contains("unsupported schema"));
    }

    #[test]
    fn final_save_ignores_backoff_and_persists_pending_history() {
        let mut state = history_state_with(&["alpha"]);
        let now = Instant::now();
        state.collected(state.history.clone(), now);
        state
            .save_current(now, |_| Err("offline".into()))
            .unwrap_err();
        state
            .save_current(now, |history| {
                assert_eq!(history.entries()[0].text, "alpha");
                Ok(())
            })
            .unwrap();
        assert!(!state.dirty);
    }

    impl PasteboardBackend for MockBackend {
        fn change_count(&mut self) -> Result<i64, PasteboardError> {
            let count = self.state.lock().unwrap().count;
            if count < 0 {
                Err(PasteboardError::ReadFailed)
            } else {
                Ok(count)
            }
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
