use clipboard_core::{
    ClipboardEntry, ClipboardHistory, ClipboardStore, IngestOutcome, LoadOutcome, RemovalOutcome,
    StoreError,
};
use macos_adapter::{ForegroundApplication, PasteboardError, PasteboardService};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const REOPEN_FOR_PASTE: &str = "请从目标应用重新打开剪贴板历史";
const QUEUE_TIMEOUT: Duration = Duration::from_secs(2);
const REMOVAL_CANCELLED: &str = "剪贴板操作排队超时，已取消";

#[derive(Debug, Clone)]
pub struct ClipboardSelection {
    pub text: String,
    pub target: ForegroundApplication,
}

pub struct ClipboardManagerCore {
    history: ClipboardHistory,
    pasteboard: PasteboardService,
    target: Option<ForegroundApplication>,
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
            target: None,
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
        self.target = None;
        let target = target.ok_or(PasteboardError::ApplicationUnavailable)?;
        if self.own_bundle_id.as_deref() == Some(target.bundle_id.as_str()) {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        self.target = Some(target.clone());
        Ok(target)
    }

    pub fn select(&self, id: u64) -> Result<ClipboardSelection, String> {
        let entry = self
            .history
            .entries()
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| "剪贴板条目不存在".to_owned())?;
        let target = self
            .target
            .clone()
            .ok_or_else(|| REOPEN_FOR_PASTE.to_owned())?;
        Ok(ClipboardSelection {
            text: entry.text.clone(),
            target,
        })
    }

    fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError> {
        self.pasteboard.write_text(text)
    }

    fn history_clone(&self) -> ClipboardHistory {
        self.history.clone()
    }

    #[cfg(test)]
    fn ingest_for_test(&mut self, text: &str) {
        self.history.ingest(text.to_owned(), unix_ms());
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipboardPreview {
    id: u64,
    preview: String,
    copied_at_unix_ms: u64,
}

impl From<&ClipboardEntry> for ClipboardPreview {
    fn from(entry: &ClipboardEntry) -> Self {
        const LIMIT: usize = 120;
        let mut preview = String::new();
        let mut length = 0;
        let mut needs_space = false;
        for character in entry.text.chars() {
            if character.is_whitespace() {
                needs_space = length > 0;
                continue;
            }
            if length == LIMIT {
                preview.push('…');
                break;
            }
            if needs_space {
                preview.push(' ');
                length += 1;
                needs_space = false;
                if length == LIMIT {
                    preview.push('…');
                    break;
                }
            }
            preview.push(character);
            length += 1;
        }
        Self {
            id: entry.id,
            preview,
            copied_at_unix_ms: entry.copied_at_unix_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipboardSnapshot {
    pub entries: Vec<ClipboardPreview>,
    pub panel_open: bool,
    pub can_paste: bool,
    pub panel_session: u64,
    pub revision: u64,
    pub last_error: Option<String>,
    pub persistence_pending: bool,
    pub storage_error: Option<String>,
    pub read_error: Option<String>,
    pub operation_error: Option<String>,
}

#[derive(Default, Debug, PartialEq, Eq, Clone)]
struct ClipboardErrors {
    storage: Option<String>,
    manual_recovery: Option<String>,
    read: Option<String>,
    operation: Option<String>,
}

impl ClipboardErrors {
    fn message(&self) -> Option<String> {
        let messages: Vec<&str> = [
            &self.storage,
            &self.manual_recovery,
            &self.read,
            &self.operation,
        ]
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
    panel_session: u64,
}

impl HistoryState {
    fn load(path: &Path) -> Self {
        let mut state = Self::from_load(ClipboardStore::load_or_recover(path));
        let saved = ClipboardStore::manual_recovery_dir(path);
        if std::fs::read_dir(&saved).is_ok_and(|mut entries| entries.next().is_some()) {
            state.errors.manual_recovery = Some(format!(
                "旧历史备份已保留到 {}，请确认备份内容后手动恢复。",
                saved.display()
            ));
        }
        state
    }

    fn from_load(result: Result<LoadOutcome, StoreError>) -> Self {
        let mut dirty = false;
        let (history, storage_error, storage_ready) = match result {
            Ok(LoadOutcome::Loaded(history)) => (history, None, true),
            Ok(LoadOutcome::RecoveredCorrupt { history, path }) => (
                history,
                Some(format!("历史文件损坏，已备份到 {}", path.display())),
                true,
            ),
            Ok(LoadOutcome::RecoveredBackup(history)) => {
                (history, Some("已从有效备份恢复剪贴板历史".into()), true)
            }
            Ok(LoadOutcome::RecoveredRemoval { history, pending }) => {
                dirty = pending.is_some();
                (history, pending.map(|error| removal_warning(&error)), true)
            }
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
            dirty,
            next_retry_at: None,
            retry_step: 0,
            storage_ready,
            panel_session: 0,
            errors: ClipboardErrors {
                storage: storage_error,
                ..ClipboardErrors::default()
            },
        }
    }

    fn snapshot(&self, panel_open: bool, core: Option<&ClipboardManagerCore>) -> ClipboardSnapshot {
        ClipboardSnapshot {
            entries: self
                .history
                .entries()
                .iter()
                .map(ClipboardPreview::from)
                .collect(),
            panel_open,
            can_paste: panel_open && core.is_some_and(|core| core.target.is_some()),
            panel_session: self.panel_session,
            revision: 0,
            last_error: self.errors.message(),
            persistence_pending: self.dirty,
            storage_error: self.errors.storage.clone(),
            read_error: self.errors.read.clone(),
            operation_error: self.errors.operation.clone(),
        }
    }

    fn open_panel(
        &mut self,
        core: &mut Option<ClipboardManagerCore>,
        panel_open: &mut bool,
        from_panel: bool,
        target: Option<ForegroundApplication>,
    ) {
        if *panel_open && from_panel {
            return;
        }
        self.panel_session = self.panel_session.wrapping_add(1);
        self.errors.operation = None;
        if let Some(core) = core.as_mut() {
            let _ = core.capture_target_value(target);
        }
        *panel_open = true;
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
        F: FnOnce(&ClipboardHistory) -> Result<RemovalOutcome, String>,
    {
        self.ensure_storage_ready()?;
        match save(&candidate) {
            Ok(outcome) => {
                self.history = candidate;
                self.saved();
                self.errors.operation = None;
                if let RemovalOutcome::Pending(error) = outcome {
                    self.dirty = true;
                    self.next_retry_at = Some(Instant::now() + Duration::from_secs(1));
                    self.errors.storage = Some(removal_warning(&error));
                }
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
        F: FnOnce(&ClipboardHistory) -> Result<RemovalOutcome, String>,
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
        F: FnOnce(&ClipboardHistory) -> Result<RemovalOutcome, String>,
    {
        let mut candidate = self.history.clone();
        candidate.clear();
        self.commit_candidate(candidate, save)
    }
}

fn removal_warning(error: &std::io::Error) -> String {
    format!("剪贴板历史已更新，磁盘同步尚未完成，将自动重试：{error}")
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
    Show {
        from_panel: bool,
        target: Option<ForegroundApplication>,
        verify_foreground: bool,
    },
    Refresh {
        reply: Sender<Result<(), String>>,
    },
    Select {
        id: u64,
        session: u64,
        generation: u64,
        reply: Sender<Result<(), String>>,
    },
    Remove(RemovalRequest),
    Close {
        session: u64,
        reply: Sender<Result<(), String>>,
    },
    Shutdown {
        reply: Sender<Result<(), String>>,
    },
}

#[derive(Clone, Copy)]
enum Removal {
    Entry(u64),
    All,
}

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

struct RemovalRequest {
    action: Removal,
    deadline: Instant,
    status: Arc<AtomicU8>,
    reply: Sender<Result<(), String>>,
}

impl RemovalRequest {
    fn run(
        &self,
        state: &mut HistoryState,
        save: impl FnOnce(&ClipboardHistory) -> Result<RemovalOutcome, String>,
    ) -> Option<Result<(), String>> {
        self.status
            .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        if Instant::now() >= self.deadline {
            self.status.store(CANCELLED, Ordering::Release);
            let _ = self.reply.send(Err(REMOVAL_CANCELLED.into()));
            return None;
        }
        Some(match self.action {
            Removal::Entry(id) => state.delete(id, save),
            Removal::All => state.clear(save),
        })
    }
}

pub struct ClipboardManager {
    sender: SyncSender<WorkerCommand>,
    snapshot: Arc<Mutex<ClipboardSnapshot>>,
    select_generation: Arc<AtomicU64>,
    panel_focused: Arc<AtomicBool>,
    admission: Arc<Mutex<()>>,
}

impl ClipboardManager {
    #[cfg(test)]
    pub(crate) fn with_full_test_queue() -> (Arc<Self>, impl FnOnce()) {
        let (sender, receiver) = mpsc::sync_channel(1);
        assert!(sender
            .try_send(WorkerCommand::Show {
                from_panel: false,
                target: None,
                verify_foreground: false,
            })
            .is_ok());
        let manager = Arc::new(Self {
            sender,
            snapshot: Arc::new(Mutex::new(
                HistoryState::from_load(Ok(LoadOutcome::Loaded(ClipboardHistory::default())))
                    .snapshot(false, None),
            )),
            select_generation: Arc::new(AtomicU64::new(0)),
            panel_focused: Arc::new(AtomicBool::new(false)),
            admission: Arc::new(Mutex::new(())),
        });
        (manager, move || drop(receiver))
    }

    pub fn start(
        app: AppHandle,
        history_path: PathBuf,
    ) -> Result<Arc<Self>, ClipboardManagerError> {
        Self::start_with_marker(
            app,
            history_path,
            macos_adapter::next_marker(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub fn start_with_marker(
        app: AppHandle,
        history_path: PathBuf,
        marker: u64,
        panel_focused: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, ClipboardManagerError> {
        let (sender, receiver) = mpsc::sync_channel(32);
        let snapshot = Arc::new(Mutex::new(ClipboardSnapshot {
            entries: Vec::new(),
            panel_open: false,
            can_paste: false,
            panel_session: 0,
            revision: 0,
            last_error: None,
            persistence_pending: false,
            storage_error: None,
            read_error: None,
            operation_error: None,
        }));
        let worker_snapshot = Arc::clone(&snapshot);
        let select_generation = Arc::new(AtomicU64::new(0));
        let worker_select_generation = Arc::clone(&select_generation);
        let admission = Arc::new(Mutex::new(()));
        let worker_admission = Arc::clone(&admission);
        thread::Builder::new()
            .name("mac-to-windows-clipboard".into())
            .spawn(move || {
                worker_loop(
                    app,
                    history_path,
                    receiver,
                    worker_snapshot,
                    worker_select_generation,
                    worker_admission,
                    marker,
                )
            })
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
        Ok(Arc::new(Self {
            sender,
            snapshot,
            select_generation,
            panel_focused,
            admission,
        }))
    }

    pub fn snapshot(&self) -> ClipboardSnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or(ClipboardSnapshot {
                entries: Vec::new(),
                panel_open: false,
                can_paste: false,
                panel_session: 0,
                revision: 0,
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

    pub fn try_show(&self, target_pid: Option<i32>) -> bool {
        self.try_show_with_capture(target_pid, || {
            macos_adapter::frontmost_application().ok().flatten()
        })
    }

    fn try_show_with_capture(
        &self,
        target_pid: Option<i32>,
        capture: impl FnOnce() -> Option<ForegroundApplication>,
    ) -> bool {
        let Ok(_admission) = self.admission.try_lock() else {
            return false;
        };
        // This snapshot belongs to the hotkey. A queued Show cannot query a
        // new foreground app after a slow history operation or focus change.
        let target = capture()
            .filter(|target| target_pid.is_none_or(|pid| target.process_id() == Some(pid)));
        let from_panel = self.panel_focused.load(Ordering::Acquire)
            && target
                .as_ref()
                .and_then(ForegroundApplication::process_id)
                .is_some_and(|pid| u32::try_from(pid) == Ok(std::process::id()));
        if self
            .sender
            .try_send(WorkerCommand::Show {
                from_panel,
                target,
                verify_foreground: target_pid.is_none(),
            })
            .is_err()
        {
            return false;
        }
        if !from_panel {
            self.select_generation.fetch_add(1, Ordering::AcqRel);
        }
        true
    }

    pub fn refresh(&self) -> Result<(), ClipboardManagerError> {
        self.request(|reply| WorkerCommand::Refresh { reply })
    }

    pub fn select(&self, id: u64, session: u64) -> Result<(), ClipboardManagerError> {
        let (reply, response) = mpsc::channel();
        let generation = {
            let _admission = self
                .admission
                .lock()
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            let snapshot = self
                .snapshot
                .lock()
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            if !snapshot.panel_open || snapshot.panel_session != session {
                return Err(ClipboardManagerError::RequestFailed("操作已取消".into()));
            }
            if !snapshot.can_paste {
                return Err(ClipboardManagerError::RequestFailed(
                    REOPEN_FOR_PASTE.into(),
                ));
            }
            let generation = self
                .select_generation
                .load(Ordering::Acquire)
                .wrapping_add(1);
            self.sender
                .try_send(WorkerCommand::Select {
                    id,
                    session,
                    generation,
                    reply,
                })
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            self.select_generation.store(generation, Ordering::Release);
            generation
        };
        match response.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result.map_err(ClipboardManagerError::RequestFailed),
            Err(_) => {
                if let Ok(_admission) = self.admission.lock() {
                    let _ = self.select_generation.compare_exchange(
                        generation,
                        generation.wrapping_add(1),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                Err(ClipboardManagerError::WorkerUnavailable)
            }
        }
    }

    pub fn delete(&self, id: u64) -> Result<(), ClipboardManagerError> {
        self.remove(Removal::Entry(id), QUEUE_TIMEOUT)
    }

    pub fn clear(&self) -> Result<(), ClipboardManagerError> {
        self.remove(Removal::All, QUEUE_TIMEOUT)
    }

    fn remove(&self, action: Removal, timeout: Duration) -> Result<(), ClipboardManagerError> {
        let (reply, response) = mpsc::channel();
        let status = Arc::new(AtomicU8::new(QUEUED));
        self.sender
            .try_send(WorkerCommand::Remove(RemovalRequest {
                action,
                deadline: Instant::now() + timeout,
                status: Arc::clone(&status),
                reply,
            }))
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
        wait_for_removal(&status, response, timeout)
    }

    pub fn close_panel(&self, session: u64) -> Result<(), ClipboardManagerError> {
        let (reply, response) = mpsc::channel();
        {
            let _admission = self
                .admission
                .lock()
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            let snapshot = self
                .snapshot
                .lock()
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            if !snapshot.panel_open || snapshot.panel_session != session {
                return Ok(());
            }
            self.sender
                .try_send(WorkerCommand::Close { session, reply })
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            self.select_generation.fetch_add(1, Ordering::AcqRel);
        }
        response
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| ClipboardManagerError::WorkerUnavailable)?
            .map_err(ClipboardManagerError::RequestFailed)
    }

    /// Cancel selection, attempt one final save, and stop the worker. Invoke
    /// from a background cleanup task so AppKit work can finish on the UI thread.
    pub fn shutdown(&self) -> Result<(), ClipboardManagerError> {
        {
            let _admission = self
                .admission
                .lock()
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?;
            self.select_generation.fetch_add(1, Ordering::AcqRel);
        }
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

fn wait_for_removal(
    status: &AtomicU8,
    response: Receiver<Result<(), String>>,
    timeout: Duration,
) -> Result<(), ClipboardManagerError> {
    let result = match response.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if status
                .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Err(ClipboardManagerError::RequestFailed(
                    REMOVAL_CANCELLED.into(),
                ));
            }
            // The worker already owns the request. Return its actual
            // outcome, including a committed removal pending disk sync.
            response
                .recv()
                .map_err(|_| ClipboardManagerError::WorkerUnavailable)?
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(ClipboardManagerError::WorkerUnavailable);
        }
    };
    result.map_err(ClipboardManagerError::RequestFailed)
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
    admission: Arc<Mutex<()>>,
    marker: u64,
) {
    let mut state = HistoryState::load(&history_path);
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
    publish_snapshot(
        &app,
        &snapshot,
        &state,
        core.as_ref(),
        panel_open,
        Some("clipboard-updated"),
    );

    let poll_interval = Duration::from_millis(25);
    let command_wakeup = Duration::from_millis(20);
    let mut next_poll = Instant::now();
    loop {
        let wait = next_poll
            .saturating_duration_since(Instant::now())
            .min(command_wakeup);
        match receiver.recv_timeout(wait) {
            Ok(command) => {
                // Wait for the sender to publish any generation change
                // associated with this queue entry.
                drop(admission.lock());
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
            publish_snapshot(
                &app,
                &snapshot,
                &state,
                core.as_ref(),
                panel_open,
                Some("clipboard-updated"),
            );
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
    let save_after_removal = |history: &ClipboardHistory| {
        ClipboardStore::save_after_removal(history_path, history)
            .map_err(|error| format!("保存剪贴板历史失败：{error}"))
    };
    match command {
        WorkerCommand::Show {
            from_panel,
            target,
            verify_foreground,
        } => {
            // A session event may lack a window-server target PID. A later
            // read can confirm the captured app, but cannot supply a new one.
            let target = verify_show_target(target, verify_foreground && !from_panel, |target| {
                frontmost_application_on_main_thread(app).is_ok_and(|current| {
                    current.is_some_and(|current| target.same_instance(&current))
                })
            });
            state.open_panel(core, panel_open, from_panel, target);
            refresh_memory_history(core, state, Instant::now(), |history| {
                save_history(history_path, history)
            });
            publish_snapshot(
                app,
                snapshot,
                state,
                core.as_ref(),
                *panel_open,
                Some("clipboard-show"),
            );
            show_window(app);
        }
        WorkerCommand::Refresh { reply } => {
            if refresh_memory_history(core, state, Instant::now(), |history| {
                save_history(history_path, history)
            }) {
                // The invoking window receives the snapshot in the IPC reply.
                publish_snapshot(app, snapshot, state, core.as_ref(), *panel_open, None);
            }
            // A readable in-memory snapshot remains useful while disk is failing.
            let _ = reply.send(Ok(()));
        }
        WorkerCommand::Select {
            id,
            session,
            generation,
            reply,
        } => {
            let current = || select_generation.load(Ordering::Acquire) == generation;
            if !*panel_open || state.panel_session != session || !current() {
                let _ = reply.send(Err("操作已取消".into()));
                return false;
            }
            let result = select_and_paste(app, core, select_generation, generation, marker, id);
            if current() {
                state.errors.operation = result.as_ref().err().cloned();
                if result.is_ok() {
                    dismiss_panel(core, panel_open);
                } else {
                    show_window(app);
                }
                publish_snapshot(
                    app,
                    snapshot,
                    state,
                    core.as_ref(),
                    *panel_open,
                    Some("clipboard-updated"),
                );
            }
            let _ = reply.send(result);
        }
        WorkerCommand::Remove(request) => {
            if let Some(result) = request.run(state, save_after_removal) {
                if result.is_ok() {
                    sync_core_history(core, state);
                }
                publish_snapshot(
                    app,
                    snapshot,
                    state,
                    core.as_ref(),
                    *panel_open,
                    Some("clipboard-updated"),
                );
                let _ = request.reply.send(result);
            }
        }
        WorkerCommand::Close { session, reply } => {
            if state.panel_session != session || !*panel_open {
                let _ = reply.send(Ok(()));
                return false;
            }
            let result = hide_window_and_wait(app, true);
            if result.is_ok() {
                dismiss_panel(core, panel_open);
            }
            state.errors.operation = result.as_ref().err().cloned();
            publish_snapshot(
                app,
                snapshot,
                state,
                core.as_ref(),
                *panel_open,
                Some("clipboard-updated"),
            );
            let _ = reply.send(result);
        }
        WorkerCommand::Shutdown { reply } => {
            dismiss_panel(core, panel_open);
            let result = state.save_current(Instant::now(), |history| {
                save_history(history_path, history)
            });
            publish_snapshot(
                app,
                snapshot,
                state,
                core.as_ref(),
                *panel_open,
                Some("clipboard-updated"),
            );
            let _ = reply.send(result);
            return true;
        }
    }
    false
}

fn verify_show_target(
    target: Option<ForegroundApplication>,
    require_confirmation: bool,
    confirms_target: impl FnOnce(&ForegroundApplication) -> bool,
) -> Option<ForegroundApplication> {
    target.filter(|target| !require_confirmation || confirms_target(target))
}

fn dismiss_panel(core: &mut Option<ClipboardManagerCore>, panel_open: &mut bool) {
    if let Some(core) = core.as_mut() {
        core.target = None;
    }
    *panel_open = false;
}

fn select_and_paste(
    app: &AppHandle,
    core: &mut Option<ClipboardManagerCore>,
    select_generation: &Arc<AtomicU64>,
    generation: u64,
    marker: u64,
    id: u64,
) -> Result<(), String> {
    let check_current = || {
        if select_generation.load(Ordering::Acquire) == generation {
            Ok(())
        } else {
            Err("操作已取消".to_owned())
        }
    };
    check_current()?;
    let core = core.as_mut().ok_or("系统剪贴板服务不可用")?;
    let selection = core.select(id)?;
    hide_window_and_wait(app, false)?;
    check_current()?;
    activate_application_on_main_thread(
        app,
        &selection.target,
        Arc::clone(select_generation),
        generation,
    )?;
    wait_for_frontmost(app, &selection.target, select_generation, generation)?;
    thread::sleep(Duration::from_millis(50));
    check_current()?;
    if !frontmost_application_on_main_thread(app)?
        .is_some_and(|current| selection.target.same_instance(&current))
    {
        return Err(REOPEN_FOR_PASTE.into());
    }
    check_current()?;
    let expected_change_count = core
        .write_text(&selection.text)
        .map_err(|error| format!("写入系统剪贴板失败：{error}"))?;
    let select_generation = Arc::clone(select_generation);
    run_on_main_thread_result(app, move |timed_out| {
        selection
            .target
            .paste_if_frontmost(marker, expected_change_count, || {
                !timed_out.load(Ordering::Acquire)
                    && select_generation.load(Ordering::Acquire) == generation
            })
    })?
}

fn run_on_main_thread_result<T, F>(app: &AppHandle, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> T + Send + 'static,
{
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let closure_cancelled = Arc::clone(&cancelled);
    app.run_on_main_thread(move || {
        if closure_cancelled.load(Ordering::Acquire) {
            return;
        }
        let _ = result_tx.send(operation(&closure_cancelled));
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
    run_on_main_thread_result(app, |_| macos_adapter::frontmost_application())?
        .map_err(|error| format!("读取前台应用失败：{error}"))
}

fn activate_application_on_main_thread(
    app: &AppHandle,
    target: &ForegroundApplication,
    select_generation: Arc<AtomicU64>,
    generation: u64,
) -> Result<(), String> {
    let target = target.clone();
    run_on_main_thread_result(app, move |timed_out| {
        if timed_out.load(Ordering::Acquire)
            || select_generation.load(Ordering::Acquire) != generation
        {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        target.activate()
    })?
    .map_err(|error| format!("恢复前台应用失败：{error}"))
}

fn wait_for_frontmost(
    app: &AppHandle,
    target: &ForegroundApplication,
    select_generation: &Arc<AtomicU64>,
    generation: u64,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(1_000);
    loop {
        if select_generation.load(Ordering::Acquire) != generation {
            return Err("操作已取消".into());
        }
        match frontmost_application_on_main_thread(app)? {
            Some(application) if target.same_instance(&application) => return Ok(()),
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
    core: Option<&ClipboardManagerCore>,
    panel_open: bool,
    event: Option<&str>,
) {
    let Ok(mut current) = snapshot.lock() else {
        return;
    };
    let mut value = state.snapshot(panel_open, core);
    value.revision = current.revision.wrapping_add(1);
    if let Some(event) = event {
        *current = value.clone();
        drop(current);
        let _ = app.emit_to("clipboard", event, value);
    } else {
        *current = value;
    }
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

fn hide_window_and_wait(app: &AppHandle, hide_app: bool) -> Result<(), String> {
    let handle = app.clone();
    run_on_main_thread_result(app, move |_| {
        if let Some(window) = handle.get_webview_window("clipboard") {
            window.hide().map_err(|error| error.to_string())?;
        }
        // The app otherwise remains frontmost after hiding its last window.
        #[cfg(target_os = "macos")]
        if hide_app {
            handle.hide().map_err(|error| error.to_string())?;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = hide_app;
        Ok(())
    })?
}

#[cfg(test)]
mod tests {
    use super::{
        dismiss_panel, mpsc, refresh_memory_history, sync_core_history, wait_for_removal,
        AtomicBool, AtomicU64, AtomicU8, ClipboardManager, ClipboardManagerCore,
        ClipboardManagerError, ClipboardSnapshot, HistoryState, Removal, RemovalRequest,
        WorkerCommand, QUEUED,
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

    fn manager_with_snapshot(
        snapshot: ClipboardSnapshot,
    ) -> (ClipboardManager, mpsc::Receiver<WorkerCommand>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let manager = ClipboardManager {
            sender,
            snapshot: Arc::new(Mutex::new(snapshot)),
            select_generation: Arc::new(AtomicU64::new(0)),
            panel_focused: Arc::new(AtomicBool::new(false)),
            admission: Arc::new(Mutex::new(())),
        };
        (manager, receiver)
    }

    #[test]
    fn queued_open_uses_the_target_captured_before_the_foreground_changes() {
        let mut state = history_state_with(&["sample"]);
        let id = state.history.entries()[0].id;
        let (manager, receiver) = manager_with_snapshot(state.snapshot(false, None));
        manager.panel_focused.store(true, super::Ordering::Release);
        let (mut core, mock) = mock_core(&state.history);
        let mut source = PasteboardService::with_backend(Box::new(MockBackend {
            state: Arc::clone(&mock),
        }));
        mock.lock().unwrap().frontmost = Some("com.example.first".into());
        assert!(manager.try_show_with_capture(None, || source.frontmost_application().unwrap()));
        mock.lock().unwrap().frontmost = Some("com.example.second".into());
        let WorkerCommand::Show {
            from_panel,
            target,
            verify_foreground,
        } = receiver.try_recv().unwrap()
        else {
            panic!("expected queued show");
        };
        assert!(!from_panel);
        assert!(verify_foreground);
        let mut open = false;
        state.open_panel(&mut core, &mut open, from_panel, target.clone());
        assert!(state.snapshot(open, core.as_ref()).can_paste);
        assert_eq!(
            core.unwrap().select(id).unwrap().target.bundle_id,
            "com.example.first"
        );
        let unverified = super::verify_show_target(target, verify_foreground, |captured| {
            let current = source.frontmost_application().unwrap().unwrap();
            captured.bundle_id == current.bundle_id
        });
        assert!(unverified.is_none());
    }

    #[test]
    fn unknown_target_prevents_selection_before_it_reaches_the_worker() {
        let mut state = history_state_with(&["sample"]);
        let mut open = false;
        let (mut core, _) = mock_core(&state.history);
        state.open_panel(&mut core, &mut open, false, None);
        let (manager, receiver) = manager_with_snapshot(state.snapshot(open, core.as_ref()));
        let error = manager
            .select(state.history.entries()[0].id, state.panel_session)
            .unwrap_err();
        assert_eq!(error.to_string(), super::REOPEN_FOR_PASTE);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn queued_clear_and_delete_are_cancelled_on_timeout_without_writing() {
        let mut state = history_state_with(&["sample"]);
        let (manager, receiver) = manager_with_snapshot(state.snapshot(true, None));
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        super::ClipboardStore::save_atomic(&path, &state.history).unwrap();
        let previous = state.history.clone();
        for action in [Removal::Entry(previous.entries()[0].id), Removal::All] {
            let error = manager.remove(action, Duration::ZERO).unwrap_err();
            assert!(error.to_string().contains("排队超时，已取消"));
            assert!(
                matches!(receiver.try_recv().unwrap(), WorkerCommand::Remove(request)
                if request.run(&mut state, |_| panic!("cancelled command must not write")).is_none())
            );
        }
        assert_eq!(state.history, previous);
        assert_eq!(
            super::ClipboardStore::load_or_recover(&path)
                .unwrap()
                .history(),
            previous
        );
    }

    #[test]
    fn started_removal_returns_the_actual_result_after_the_queue_deadline() {
        let status = Arc::new(AtomicU8::new(QUEUED));
        let (reply, response) = mpsc::channel();
        let request = RemovalRequest {
            action: Removal::All,
            deadline: Instant::now() + Duration::from_secs(30),
            status: Arc::clone(&status),
            reply,
        };
        let (started, claimed) = mpsc::channel();
        let (finish, proceed) = mpsc::channel();
        std::thread::scope(|tasks| {
            tasks.spawn(move || {
                let mut state = history_state_with(&["sample"]);
                let result = request
                    .run(&mut state, |_| {
                        started.send(()).unwrap();
                        proceed.recv().unwrap();
                        Err("disk rejected the removal".into())
                    })
                    .unwrap();
                request.reply.send(result).unwrap();
                assert_eq!(state.history.entries().len(), 1);
            });
            claimed.recv().unwrap();
            let (completed, observed) = mpsc::channel();
            tasks.spawn(move || {
                completed
                    .send(wait_for_removal(&status, response, Duration::ZERO))
                    .unwrap();
            });
            let while_active = observed.recv_timeout(Duration::from_millis(20));
            finish.send(()).unwrap();
            assert!(matches!(while_active, Err(mpsc::RecvTimeoutError::Timeout)));
            assert!(
                matches!(observed.recv().unwrap(), Err(ClipboardManagerError::RequestFailed(error)) if error == "disk rejected the removal")
            );
        });
    }

    #[test]
    fn worker_can_expire_a_queued_removal_before_the_requester_wakes() {
        let mut state = history_state_with(&["sample"]);
        let status = Arc::new(AtomicU8::new(QUEUED));
        let (reply, response) = mpsc::channel();
        let request = RemovalRequest {
            action: Removal::All,
            deadline: Instant::now(),
            status: Arc::clone(&status),
            reply,
        };
        assert!(request
            .run(&mut state, |_| panic!("expired command must not write"))
            .is_none());
        let error = wait_for_removal(&status, response, Duration::ZERO).unwrap_err();
        assert_eq!(error.to_string(), super::REMOVAL_CANCELLED);
        assert_eq!(state.history.entries().len(), 1);
    }

    #[test]
    fn snapshot_sends_only_a_bounded_unicode_preview() {
        let large = format!(
            "  alpha\n  beta  {}",
            "😀".repeat(clipboard_core::MAX_CHARS - 20)
        );
        let state = history_state_with(&[&large]);
        let snapshot = state.snapshot(true, None);
        let preview = &snapshot.entries[0].preview;
        assert!(preview.starts_with("alpha beta 😀"));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 121);
        let tauri::ipc::InvokeResponseBody::Json(encoded) =
            tauri::ipc::IpcResponse::body(snapshot).unwrap()
        else {
            panic!("expected JSON");
        };
        assert!(encoded.len() < 1024);
        assert_eq!(state.history.entries()[0].text, large);
    }

    #[test]
    fn external_reopen_refreshes_the_target_and_panel_reopen_reuses_it() {
        let mut state = history_state_with(&["sample"]);
        let id = state.history.entries()[0].id;
        let (mut core, mock) = mock_core(&state.history);
        let mut open = false;
        for (session, target) in [(1, "com.example.first"), (2, "com.example.second")] {
            mock.lock().unwrap().frontmost = Some(target.into());
            let source = PasteboardService::with_backend(Box::new(MockBackend {
                state: Arc::clone(&mock),
            }));
            let mut source = source;
            let captured = source.frontmost_application().unwrap();
            state.open_panel(&mut core, &mut open, false, captured);
            assert_eq!(state.snapshot(open, core.as_ref()).panel_session, session);
            assert_eq!(
                core.as_ref().unwrap().select(id).unwrap().target.bundle_id,
                target
            );
        }
        state.open_panel(&mut core, &mut open, true, None);
        assert_eq!(state.snapshot(open, core.as_ref()).panel_session, 2);
        assert!(state.snapshot(open, core.as_ref()).can_paste);
        state.open_panel(&mut core, &mut open, false, None);
        assert!(!state.snapshot(open, core.as_ref()).can_paste);
        assert!(core.as_ref().unwrap().select(id).is_err());
    }

    #[test]
    fn a_completed_older_selection_cannot_invalidate_the_newer_generation() {
        use super::Ordering;
        let mut state = history_state_with(&["sample"]);
        state.panel_session = 1;
        let (core, mock) = mock_core(&state.history);
        mock.lock().unwrap().frontmost = Some("com.example.editor".into());
        let mut core = core.unwrap();
        core.capture_target().unwrap();
        let (manager, receiver) = manager_with_snapshot(state.snapshot(true, Some(&core)));
        std::thread::scope(|tasks| {
            let first = tasks.spawn(|| manager.select(1, 1));
            let WorkerCommand::Select { reply: old, .. } = receiver.recv().unwrap() else {
                panic!("expected first selection");
            };
            let newer = tasks.spawn(|| manager.select(2, 1));
            let WorkerCommand::Select {
                reply, generation, ..
            } = receiver.recv().unwrap()
            else {
                panic!("expected second selection");
            };
            old.send(Err("操作已取消".into())).unwrap();
            assert!(first.join().unwrap().is_err());
            assert_eq!(
                manager.select_generation.load(Ordering::Acquire),
                generation
            );
            reply.send(Ok(())).unwrap();
            assert!(newer.join().unwrap().is_ok());
        });
    }

    #[test]
    fn a_committed_removal_updates_memory_while_storage_is_retried() {
        let mut state = history_state_with(&["sample"]);
        state
            .clear(|_| {
                Ok(clipboard_core::RemovalOutcome::Pending(
                    std::io::Error::other("sync"),
                ))
            })
            .unwrap();
        let snapshot = state.snapshot(true, None);
        assert!(snapshot.entries.is_empty() && snapshot.persistence_pending);
        assert!(snapshot.storage_error.unwrap().contains("磁盘同步尚未完成"));
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
        core.capture_target().unwrap();
        let selection = core.select(id).unwrap();
        assert_eq!(selection.text, "alpha");
        assert_eq!(selection.target.bundle_id, "com.example.editor");
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
        let state = Arc::new(Mutex::new(MockState {
            frontmost: Some("com.example.editor".into()),
            ..MockState::default()
        }));
        let service = PasteboardService::with_backend(Box::new(MockBackend { state }));
        let mut core = ClipboardManagerCore::new_for_test(service);
        core.capture_target().unwrap();
        let mut panel_open = true;

        let mut core = Some(core);
        dismiss_panel(&mut core, &mut panel_open);

        assert!(!panel_open);
        assert!(core.as_ref().unwrap().target.is_none());
    }

    #[test]
    fn manual_recovery_notice_survives_saving_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        std::fs::write(
            path.with_extension("bak"),
            br#"{"schema_version":1,"next_id":2,"entries":[{"id":1,"text":"previous history","copied_at_unix_ms":1}]}"#,
        )
        .unwrap();
        std::fs::write(&path, b"invalid json").unwrap();

        let mut state = HistoryState::load(&path);
        assert!(state.history.entries().is_empty());
        let notice = state.errors.manual_recovery.clone().unwrap();
        assert!(state
            .snapshot(false, None)
            .last_error
            .unwrap()
            .contains(&notice));

        let mut current = ClipboardHistory::default();
        current.ingest("later history".into(), 2);
        let now = Instant::now();
        state.collected(current.clone(), now);
        state
            .save_current(now, |history| super::save_history(&path, history))
            .unwrap();
        assert_eq!(
            state.snapshot(false, None).last_error.as_deref(),
            Some(notice.as_str())
        );
        let reloaded = HistoryState::load(&path);
        assert_eq!(reloaded.history, current);
        assert_eq!(
            reloaded.snapshot(false, None).last_error.as_deref(),
            Some(notice.as_str())
        );
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
        assert_eq!(state.snapshot(true, None).entries.len(), 2);
        assert!(state.snapshot(true, None).persistence_pending);
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
                Ok(clipboard_core::RemovalOutcome::Saved)
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
                Ok(clipboard_core::RemovalOutcome::Saved)
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
            .snapshot(false, None)
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
            .snapshot(true, None)
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
