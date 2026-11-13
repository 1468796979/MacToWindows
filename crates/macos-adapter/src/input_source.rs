use std::sync::{Arc, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSourceCommand {
    Next,
    TogglePinyinEnglish,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputSourceSnapshot {
    pub current_id: Option<String>,
    pub current_is_pinyin: bool,
    pub last_latin_id: Option<String>,
    pub last_pinyin_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputSourceError {
    Unsupported,
    ServiceStopped,
    Api(String),
}

pub struct InputSourceService {
    snapshot: Arc<RwLock<InputSourceSnapshot>>,
    last_error: Arc<RwLock<Option<InputSourceError>>>,
    #[cfg(target_os = "macos")]
    tx: std::sync::mpsc::Sender<InputSourceRequest>,
}

#[cfg(target_os = "macos")]
struct InputSourceRequest {
    command: InputSourceCommand,
}

impl InputSourceService {
    pub fn start() -> Result<Self, InputSourceError> {
        platform::start()
    }

    pub fn snapshot(&self) -> InputSourceSnapshot {
        self.snapshot
            .read()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    pub fn take_last_error(&self) -> Option<InputSourceError> {
        self.last_error.write().ok()?.take()
    }

    pub fn request(&self, command: InputSourceCommand) -> Result<(), InputSourceError> {
        #[cfg(target_os = "macos")]
        {
            self.tx
                .send(InputSourceRequest { command })
                .map_err(|_| InputSourceError::ServiceStopped)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = command;
            Err(InputSourceError::Unsupported)
        }
    }
}

fn is_pinyin_identifier(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("scim") || lower.contains("pinyin")
}

fn next_index(current: usize, count: usize) -> Option<usize> {
    if count == 0 {
        None
    } else {
        Some((current + 1) % count)
    }
}

/// Keep a just-selected source visible until TIS reports the same source.
///
/// `TISSelectInputSource` can return before `TISCopyCurrentKeyboardInputSource`
/// observes the new source. Replacing the optimistic snapshot with that stale
/// result would make the next Shift decision use the source from before the
/// command. Once TIS reports the requested target, its snapshot is authoritative.
fn reconcile_selection_snapshot(
    optimistic: InputSourceSnapshot,
    refreshed: InputSourceSnapshot,
    target: &str,
) -> InputSourceSnapshot {
    if refreshed.current_id.as_deref() == Some(target) {
        refreshed
    } else {
        optimistic
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{
        is_pinyin_identifier, next_index, reconcile_selection_snapshot, InputSourceCommand,
        InputSourceError, InputSourceService, InputSourceSnapshot,
    };
    use std::ffi::{c_char, c_void, CStr};
    use std::ptr;
    use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
    use std::thread;
    use std::time::Duration;

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type TISInputSourceRef = *const c_void;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
        fn TISCreateInputSourceList(
            properties: CFDictionaryRef,
            include_all_installed: bool,
        ) -> CFArrayRef;
        fn TISGetInputSourceProperty(
            input_source: TISInputSourceRef,
            property: CFStringRef,
        ) -> CFTypeRef;
        fn TISSelectInputSource(input_source: TISInputSourceRef) -> i32;
        static kTISPropertyInputSourceID: CFStringRef;
        static kTISPropertyInputSourceCategory: CFStringRef;
        static kTISPropertyInputSourceType: CFStringRef;
        static kTISPropertyInputSourceIsASCIICapable: CFStringRef;
        static kTISCategoryKeyboardInputSource: CFStringRef;
        static kTISTypeKeyboardLayout: CFStringRef;
        static kTISTypeKeyboardInputMethodWithoutModes: CFStringRef;
        static kTISTypeKeyboardInputMode: CFStringRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
        fn CFBooleanGetValue(value: CFTypeRef) -> bool;
        fn CFEqual(left: CFTypeRef, right: CFTypeRef) -> bool;
        fn CFRelease(value: *mut c_void);
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> bool;
    }

    static TIS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub fn start() -> Result<InputSourceService, InputSourceError> {
        // HIToolbox requires TIS queries on the application main queue. The
        // first query runs here during controller construction; later worker
        // requests dispatch through `run_on_main` and never block the event
        // tap callback because `request` only enqueues a command.
        let initial = {
            let _guard = TIS_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .map_err(|_| InputSourceError::Api("输入源服务锁已损坏".into()))?;
            refresh_snapshot_unlocked(&InputSourceSnapshot::default())?
        };
        let snapshot = Arc::new(RwLock::new(initial));
        let last_error = Arc::new(RwLock::new(None));
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker_snapshot = Arc::clone(&snapshot);
        let worker_error = Arc::clone(&last_error);
        thread::Builder::new()
            .name("mac-to-windows-input-source".into())
            .spawn(move || run_worker(rx, worker_snapshot, worker_error, ready_tx))
            .map_err(|error| InputSourceError::Api(error.to_string()))?;
        ready_rx
            .recv_timeout(Duration::from_millis(500))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    InputSourceError::Api("输入源服务启动超时".into())
                }
                mpsc::RecvTimeoutError::Disconnected => InputSourceError::ServiceStopped,
            })??;
        Ok(InputSourceService {
            snapshot,
            last_error,
            tx,
        })
    }

    fn run_worker(
        rx: mpsc::Receiver<super::InputSourceRequest>,
        snapshot: Arc<RwLock<InputSourceSnapshot>>,
        last_error: Arc<RwLock<Option<InputSourceError>>>,
        ready: mpsc::SyncSender<Result<(), InputSourceError>>,
    ) {
        const SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
        let mut current = snapshot
            .read()
            .map(|value| value.clone())
            .unwrap_or_default();
        let _ = ready.send(Ok(()));
        loop {
            let request = match rx.recv_timeout(SNAPSHOT_REFRESH_INTERVAL) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Keep the snapshot in sync with changes made by macOS or
                    // another input-source client. This runs on the service
                    // worker, never on the event-tap callback path.
                    match refresh_snapshot(&current) {
                        Ok(refreshed) => {
                            current = refreshed;
                            if let Ok(mut target) = snapshot.write() {
                                *target = current.clone();
                            }
                        }
                        Err(error) => {
                            if let Ok(mut target) = last_error.write() {
                                *target = Some(error);
                            }
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // The user or another input-source client may have changed the
            // source since the previous command. Refresh on this serial
            // worker before choosing the target so the event-tap path never
            // calls TIS and never uses a stale source.
            match refresh_snapshot(&current) {
                Ok(refreshed) => {
                    current = refreshed;
                    if let Ok(mut target) = snapshot.write() {
                        *target = current.clone();
                    }
                }
                Err(error) => {
                    if let Ok(mut target) = last_error.write() {
                        *target = Some(error);
                    }
                }
            }
            let selected_target = match request.command {
                InputSourceCommand::Next => select_next(&current).map(Some),
                InputSourceCommand::TogglePinyinEnglish => toggle_pinyin_english(&current),
            };
            if let Err(error) = &selected_target {
                if let Ok(mut target) = last_error.write() {
                    *target = Some(error.clone());
                }
                continue;
            }
            if let Ok(Some(target)) = selected_target {
                // TISSelectInputSource returns before the selected source is
                // observable through TISCopyCurrentKeyboardInputSource on
                // recent macOS releases.  Keep the event-tap snapshot
                // optimistic immediately, then reconcile it once macOS has
                // published the new source.
                let mut optimistic = current.clone();
                optimistic.current_id = Some(target.clone());
                optimistic.current_is_pinyin = is_pinyin_identifier(&target);
                if optimistic.current_is_pinyin {
                    optimistic.last_pinyin_id = Some(target.clone());
                } else {
                    optimistic.last_latin_id = Some(target.clone());
                }
                current = optimistic.clone();
                if let Ok(mut target_snapshot) = snapshot.write() {
                    *target_snapshot = optimistic;
                }
                match refresh_snapshot_until(&current, &target) {
                    Ok(updated) => {
                        current = updated;
                        if let Ok(mut target_snapshot) = snapshot.write() {
                            *target_snapshot = current.clone();
                        }
                    }
                    Err(error) => {
                        if let Ok(mut target_snapshot) = last_error.write() {
                            *target_snapshot = Some(error);
                        }
                    }
                }
            }
        }
    }

    fn refresh_snapshot(
        previous: &InputSourceSnapshot,
    ) -> Result<InputSourceSnapshot, InputSourceError> {
        let _guard = TIS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| InputSourceError::Api("输入源服务锁已损坏".into()))?;
        let previous = previous.clone();
        dispatch2::run_on_main(move |_| refresh_snapshot_unlocked(&previous))
    }

    fn refresh_snapshot_until(
        previous: &InputSourceSnapshot,
        target: &str,
    ) -> Result<InputSourceSnapshot, InputSourceError> {
        const POLL_INTERVAL: Duration = Duration::from_millis(25);
        const POLL_ATTEMPTS: usize = 48;
        for _ in 0..POLL_ATTEMPTS {
            let refreshed = refresh_snapshot(previous)?;
            if refreshed.current_id.as_deref() == Some(target) {
                return Ok(refreshed);
            }
            thread::sleep(POLL_INTERVAL);
        }
        // Preserve the optimistic target when TIS still reports the old
        // source. A later idle refresh can reconcile the snapshot once macOS
        // publishes the selection.
        let refreshed = refresh_snapshot(previous)?;
        Ok(reconcile_selection_snapshot(
            previous.clone(),
            refreshed,
            target,
        ))
    }

    fn refresh_snapshot_unlocked(
        previous: &InputSourceSnapshot,
    ) -> Result<InputSourceSnapshot, InputSourceError> {
        let current_source = unsafe { TISCopyCurrentKeyboardInputSource() };
        if current_source.is_null() {
            return Err(InputSourceError::Api("无法读取当前输入源".into()));
        }
        let current_id = unsafe { property_string(current_source, kTISPropertyInputSourceID) };
        let current_is_pinyin = current_id.as_deref().is_some_and(is_pinyin_identifier);
        let is_ascii =
            unsafe { bool_property(current_source, kTISPropertyInputSourceIsASCIICapable) };
        unsafe { CFRelease(current_source as *mut c_void) };
        let mut snapshot = previous.clone();
        snapshot.current_id = current_id.clone();
        snapshot.current_is_pinyin = current_is_pinyin;
        if current_is_pinyin {
            snapshot.last_pinyin_id = current_id;
        } else if is_ascii {
            snapshot.last_latin_id = current_id;
        }
        Ok(snapshot)
    }

    fn select_next(current: &InputSourceSnapshot) -> Result<String, InputSourceError> {
        let _guard = TIS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| InputSourceError::Api("输入源服务锁已损坏".into()))?;
        let current = current.clone();
        dispatch2::run_on_main(move |_| select_next_unlocked(&current))
    }

    fn select_next_unlocked(current: &InputSourceSnapshot) -> Result<String, InputSourceError> {
        let sources = enabled_sources()?;
        if sources.is_empty() {
            return Err(InputSourceError::Api("没有可用的键盘输入源".into()));
        }
        let current_index = current
            .current_id
            .as_deref()
            .and_then(|id| sources.iter().position(|source| source.0 == id))
            .unwrap_or(0);
        let index = next_index(current_index, sources.len()).unwrap_or(0);
        let target = sources[index].0.clone();
        select_source_id(&target)?;
        Ok(target)
    }

    fn toggle_pinyin_english(
        current: &InputSourceSnapshot,
    ) -> Result<Option<String>, InputSourceError> {
        let _guard = TIS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| InputSourceError::Api("输入源服务锁已损坏".into()))?;
        let current = current.clone();
        dispatch2::run_on_main(move |_| toggle_pinyin_english_unlocked(&current))
    }

    fn toggle_pinyin_english_unlocked(
        current: &InputSourceSnapshot,
    ) -> Result<Option<String>, InputSourceError> {
        let sources = enabled_sources()?;
        let fallback_latin = sources
            .iter()
            .find(|(id, is_ascii)| *is_ascii && !is_pinyin_identifier(id))
            .map(|(id, _)| id.as_str());
        let target = if current.current_is_pinyin {
            current.last_latin_id.as_deref().or(fallback_latin)
        } else if current.current_id.as_deref() == current.last_latin_id.as_deref() {
            current.last_pinyin_id.as_deref()
        } else {
            None
        };
        let Some(target) = target else {
            return Ok(None);
        };
        if !sources.iter().any(|(id, _)| id == target) {
            return Err(InputSourceError::Api("目标输入源未启用".into()));
        }
        select_source_id(target)?;
        Ok(Some(target.to_string()))
    }

    fn select_source_id(target: &str) -> Result<(), InputSourceError> {
        let list = unsafe { TISCreateInputSourceList(ptr::null(), false) };
        if list.is_null() {
            return Err(InputSourceError::Api("无法枚举键盘输入源".into()));
        }
        let count = unsafe { CFArrayGetCount(list) };
        let mut selected = None;
        for index in 0..count {
            let source = unsafe { CFArrayGetValueAtIndex(list, index) } as TISInputSourceRef;
            if !unsafe { is_selectable_keyboard_source(source) } {
                continue;
            }
            let Some(id) = (unsafe { property_string(source, kTISPropertyInputSourceID) }) else {
                continue;
            };
            if id == target {
                selected = Some(unsafe { TISSelectInputSource(source) });
                break;
            }
        }
        unsafe { CFRelease(list as *mut c_void) };
        match selected {
            Some(0) => Ok(()),
            Some(status) => Err(InputSourceError::Api(format!(
                "TISSelectInputSource 返回 {status}"
            ))),
            None => Err(InputSourceError::Api("目标输入源未启用".into())),
        }
    }

    fn enabled_sources() -> Result<Vec<(String, bool)>, InputSourceError> {
        let list = unsafe { TISCreateInputSourceList(ptr::null(), false) };
        if list.is_null() {
            return Err(InputSourceError::Api("无法枚举键盘输入源".into()));
        }
        let count = unsafe { CFArrayGetCount(list) };
        let mut sources = Vec::new();
        for index in 0..count {
            let source = unsafe { CFArrayGetValueAtIndex(list, index) } as TISInputSourceRef;
            if !unsafe { is_selectable_keyboard_source(source) } {
                continue;
            }
            let Some(id) = (unsafe { property_string(source, kTISPropertyInputSourceID) }) else {
                continue;
            };
            let is_ascii = unsafe { bool_property(source, kTISPropertyInputSourceIsASCIICapable) };
            sources.push((id, is_ascii));
        }
        unsafe { CFRelease(list as *mut c_void) };
        Ok(sources)
    }

    /// TIS exposes both switchable keyboard sources and implementation-detail
    /// entries for input-method modes. The latter (for example the parent
    /// `com.apple.inputmethod.SCIM` entry) are returned by
    /// `TISCreateInputSourceList` but `TISSelectInputSource` rejects them with
    /// `paramErr` (-50). Only these three keyboard source types represent
    /// selectable entries.
    unsafe fn is_selectable_keyboard_source(source: TISInputSourceRef) -> bool {
        let category = TISGetInputSourceProperty(source, kTISPropertyInputSourceCategory);
        if category.is_null() || !CFEqual(category, kTISCategoryKeyboardInputSource) {
            return false;
        }
        let source_type = TISGetInputSourceProperty(source, kTISPropertyInputSourceType);
        if source_type.is_null() {
            return false;
        }
        CFEqual(source_type, kTISTypeKeyboardLayout)
            || CFEqual(source_type, kTISTypeKeyboardInputMethodWithoutModes)
            || CFEqual(source_type, kTISTypeKeyboardInputMode)
    }

    unsafe fn bool_property(source: TISInputSourceRef, property: CFStringRef) -> bool {
        let value = TISGetInputSourceProperty(source, property);
        !value.is_null() && CFBooleanGetValue(value)
    }

    unsafe fn property_string(source: TISInputSourceRef, property: CFStringRef) -> Option<String> {
        let value = TISGetInputSourceProperty(source, property) as CFStringRef;
        if value.is_null() {
            return None;
        }
        let mut buffer = [0 as c_char; 512];
        if CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        ) {
            Some(
                CStr::from_ptr(buffer.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{InputSourceError, InputSourceService, InputSourceSnapshot};
    use std::sync::{Arc, RwLock};

    pub fn start() -> Result<InputSourceService, InputSourceError> {
        Ok(InputSourceService {
            snapshot: Arc::new(RwLock::new(InputSourceSnapshot::default())),
            last_error: Arc::new(RwLock::new(None)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_apple_pinyin_identifiers() {
        assert!(is_pinyin_identifier("com.apple.inputmethod.SCIM.ITABC"));
        assert!(is_pinyin_identifier("Pinyin - Simplified"));
        assert!(!is_pinyin_identifier("com.apple.keylayout.ABC"));
    }

    #[test]
    fn next_index_wraps_over_enabled_sources() {
        assert_eq!(next_index(0, 3), Some(1));
        assert_eq!(next_index(2, 3), Some(0));
        assert_eq!(next_index(0, 0), None);
    }

    #[test]
    fn stale_snapshot_does_not_replace_pending_selection() {
        let mut pending = InputSourceSnapshot {
            current_id: Some("com.apple.inputmethod.SCIM.ITABC".into()),
            current_is_pinyin: true,
            last_latin_id: Some("com.apple.keylayout.ABC".into()),
            last_pinyin_id: Some("com.apple.inputmethod.SCIM.ITABC".into()),
        };
        let stale = InputSourceSnapshot {
            current_id: Some("com.apple.keylayout.ABC".into()),
            current_is_pinyin: false,
            ..pending.clone()
        };

        pending = reconcile_selection_snapshot(pending, stale, "com.apple.inputmethod.SCIM.ITABC");

        assert_eq!(
            pending.current_id.as_deref(),
            Some("com.apple.inputmethod.SCIM.ITABC")
        );
        assert!(pending.current_is_pinyin);
    }
}
