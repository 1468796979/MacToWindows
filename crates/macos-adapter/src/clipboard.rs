use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteboardError {
    Unsupported,
    ReadFailed,
    WriteFailed,
    ApplicationUnavailable,
}

impl fmt::Display for PasteboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PasteboardError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteboardChange {
    pub change_count: i64,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ForegroundApplication {
    pub bundle_id: String,
    #[cfg(target_os = "macos")]
    instance: Option<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>>,
    #[cfg(target_os = "macos")]
    pid: i32,
}

impl ForegroundApplication {
    pub fn process_id(&self) -> Option<i32> {
        #[cfg(target_os = "macos")]
        if self.pid > 0 {
            return Some(self.pid);
        }
        None
    }

    pub fn same_instance(&self, other: &Self) -> bool {
        #[cfg(target_os = "macos")]
        {
            use objc2_foundation::NSObjectProtocol;
            self.pid > 0
                && self.pid == other.pid
                && self
                    .instance
                    .as_ref()
                    .zip(other.instance.as_ref())
                    .is_some_and(|(left, right)| !left.isTerminated() && left.isEqual(Some(right)))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = other;
            false
        }
    }

    /// Requests activation of the captured application instance on the main thread.
    pub fn activate(&self) -> Result<(), PasteboardError> {
        platform::activate_instance(self)
    }

    /// Posts the shortcut only while this instance is frontmost and the
    /// pasteboard still has the ownership count from the selected write.
    pub fn paste_if_frontmost(
        &self,
        marker: u64,
        expected_change_count: i64,
        selection_is_current: impl FnOnce() -> bool,
    ) -> Result<(), String> {
        platform::paste_if_frontmost(self, marker, expected_change_count, selection_is_current)
    }
}

/// Reads the current application from the shared NSWorkspace. Unlike
/// activation, this property can be read from the Event Tap thread.
pub fn frontmost_application() -> Result<Option<ForegroundApplication>, PasteboardError> {
    platform::frontmost_application()
}

/// Activates an application using the platform's application APIs.
/// Callers that need AppKit thread affinity should invoke this on the main
/// thread.
pub fn activate_application(bundle_id: &str) -> Result<(), PasteboardError> {
    platform::activate_application(bundle_id)
}

pub trait PasteboardBackend {
    fn change_count(&mut self) -> Result<i64, PasteboardError>;
    fn read_text(&mut self) -> Result<Option<String>, PasteboardError>;
    fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError>;
    fn frontmost_bundle_id(&mut self) -> Result<Option<String>, PasteboardError>;
    fn activate_bundle_id(&mut self, bundle_id: &str) -> Result<(), PasteboardError>;
}

pub struct PasteboardService {
    backend: Box<dyn PasteboardBackend>,
    last_change_count: Option<i64>,
    ignored_change_count: Option<i64>,
    pending_change_count: Option<i64>,
}

impl PasteboardService {
    pub fn with_backend(backend: Box<dyn PasteboardBackend>) -> Self {
        Self {
            backend,
            last_change_count: None,
            ignored_change_count: None,
            pending_change_count: None,
        }
    }

    pub fn start() -> Result<Self, PasteboardError> {
        Ok(Self::with_backend(Box::new(
            platform::PlatformPasteboard::new()?,
        )))
    }

    pub fn poll(&mut self) -> Result<Option<PasteboardChange>, PasteboardError> {
        let change_count = self.backend.change_count()?;
        if self.ignored_change_count == Some(change_count) {
            self.ignored_change_count = None;
            self.pending_change_count = None;
            self.last_change_count = Some(change_count);
            return Ok(None);
        }
        if self.ignored_change_count.is_some() {
            self.ignored_change_count = None;
        }
        if self.last_change_count.is_none() {
            self.last_change_count = Some(change_count);
            return Ok(None);
        }
        if self.pending_change_count.is_none() && self.last_change_count == Some(change_count) {
            return Ok(None);
        }
        if self.pending_change_count != Some(change_count) {
            self.pending_change_count = Some(change_count);
        }
        let text = self.backend.read_text()?;
        // A pasteboard change can be published before its string
        // representation is ready. Keep the change pending until the text
        // becomes readable; retiring it after a fixed number of attempts
        // would permanently lose a slow copy operation.
        if text.is_some() {
            self.last_change_count = Some(change_count);
            self.pending_change_count = None;
        }
        Ok(text.map(|text| PasteboardChange {
            change_count,
            text: Some(text),
        }))
    }

    pub fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError> {
        let change_count = self.backend.write_text(text)?;
        self.last_change_count = Some(change_count);
        self.ignored_change_count = Some(change_count);
        self.pending_change_count = None;
        Ok(change_count)
    }

    pub fn frontmost_application(
        &mut self,
    ) -> Result<Option<ForegroundApplication>, PasteboardError> {
        Ok(self
            .backend
            .frontmost_bundle_id()?
            .map(|bundle_id| ForegroundApplication {
                bundle_id,
                #[cfg(target_os = "macos")]
                instance: None,
                #[cfg(target_os = "macos")]
                pid: 0,
            }))
    }

    pub fn activate_application(&mut self, bundle_id: &str) -> Result<(), PasteboardError> {
        self.backend.activate_bundle_id(bundle_id)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ForegroundApplication, PasteboardBackend, PasteboardError};
    use event_model::{EventPhase, ModifierSet, OutputEvent, PhysicalCode};
    use objc2::{sel, MainThreadMarker};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationOptions, NSPasteboard, NSPasteboardType,
        NSPasteboardTypeString, NSRunningApplication, NSWorkspace,
    };
    use objc2_foundation::{NSObjectProtocol, NSString};

    pub struct PlatformPasteboard {
        pasteboard: objc2::rc::Retained<NSPasteboard>,
    }

    impl PlatformPasteboard {
        pub fn new() -> Result<Self, PasteboardError> {
            Ok(Self {
                pasteboard: NSPasteboard::generalPasteboard(),
            })
        }

        fn pasteboard_type() -> &'static NSPasteboardType {
            unsafe { NSPasteboardTypeString }
        }
    }

    impl PasteboardBackend for PlatformPasteboard {
        fn change_count(&mut self) -> Result<i64, PasteboardError> {
            Ok(self.pasteboard.changeCount() as i64)
        }

        fn read_text(&mut self) -> Result<Option<String>, PasteboardError> {
            Ok(self
                .pasteboard
                .stringForType(Self::pasteboard_type())
                .map(|value| value.to_string()))
        }

        fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError> {
            let value = NSString::from_str(text);
            let owned_change_count = self.pasteboard.clearContents() as i64;
            if !self
                .pasteboard
                .setString_forType(&value, Self::pasteboard_type())
            {
                return Err(PasteboardError::WriteFailed);
            }
            // A later read could return the count of another writer.
            Ok(owned_change_count)
        }

        fn frontmost_bundle_id(&mut self) -> Result<Option<String>, PasteboardError> {
            Ok(frontmost_application()?.map(|application| application.bundle_id))
        }

        fn activate_bundle_id(&mut self, bundle_id: &str) -> Result<(), PasteboardError> {
            activate_application(bundle_id)
        }
    }

    pub fn frontmost_application() -> Result<Option<ForegroundApplication>, PasteboardError> {
        Ok(objc2::rc::autoreleasepool(|_| {
            NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .and_then(|instance| {
                    let pid = instance.processIdentifier();
                    if pid <= 0 || instance.isTerminated() {
                        return None;
                    }
                    let bundle_id = instance.bundleIdentifier()?;
                    Some(ForegroundApplication {
                        bundle_id: bundle_id.to_string(),
                        pid,
                        instance: Some(instance),
                    })
                })
        }))
    }

    pub fn activate_application(bundle_id: &str) -> Result<(), PasteboardError> {
        let bundle_id = NSString::from_str(bundle_id);
        let applications =
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
        let Some(application) = applications.firstObject() else {
            return Err(PasteboardError::ApplicationUnavailable);
        };
        activate_running_application(&application)
    }

    pub fn activate_instance(target: &ForegroundApplication) -> Result<(), PasteboardError> {
        let application = target
            .instance
            .as_deref()
            .ok_or(PasteboardError::ApplicationUnavailable)?;
        if target.pid <= 0 || target.pid != application.processIdentifier() {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        activate_running_application(application)
    }

    fn activate_running_application(
        application: &NSRunningApplication,
    ) -> Result<(), PasteboardError> {
        if application.isTerminated() {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(PasteboardError::ApplicationUnavailable);
        };
        let _ = application.unhide();
        #[allow(deprecated)]
        let options = NSApplicationActivationOptions::ActivateAllWindows
            | NSApplicationActivationOptions::ActivateIgnoringOtherApps;
        let owner = NSApplication::sharedApplication(mtm);
        if owner.respondsToSelector(sel!(yieldActivationToApplication:))
            && application.respondsToSelector(sel!(activateFromApplication:options:))
        {
            owner.yieldActivationToApplication(application);
            let current = NSRunningApplication::currentApplication();
            if application.activateFromApplication_options(&current, options) {
                return Ok(());
            }
        }
        let _ = application.activateWithOptions(options);
        // Activation can complete asynchronously. The caller polls the
        // frontmost application before injecting Cmd+V, so a false return
        // here is not by itself an unavailable-application error.
        Ok(())
    }

    pub fn paste_if_frontmost(
        target: &ForegroundApplication,
        marker: u64,
        expected_change_count: i64,
        selection_is_current: impl FnOnce() -> bool,
    ) -> Result<(), String> {
        let foreground_changed = || FOREGROUND_CHANGED.to_owned();
        if MainThreadMarker::new().is_none() {
            return Err(foreground_changed());
        }
        let application = target.instance.as_deref().ok_or_else(foreground_changed)?;
        let outputs = [EventPhase::Down, EventPhase::Up].map(|phase| OutputEvent::Key {
            code: PhysicalCode::key(9),
            phase,
            modifiers: ModifierSet::LEFT_COMMAND,
        });
        let mut refusal = None;
        crate::platform::inject_outputs_to_application(&outputs, marker, || {
            let frontmost_pid = NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .filter(|frontmost| {
                    target.pid > 0
                        && target.pid == application.processIdentifier()
                        && application.isEqual(Some(frontmost))
                        && !application.isTerminated()
                })
                .map(|_| target.pid);
            match confirm_paste_target(
                frontmost_pid,
                selection_is_current,
                expected_change_count,
                || NSPasteboard::generalPasteboard().changeCount() as i64,
            ) {
                Ok(pid) => Some(pid),
                Err(error) => {
                    refusal = Some(error);
                    None
                }
            }
        })
        .map_err(|error| match (error, refusal) {
            (crate::InjectionError::InvalidEvent, Some(reason)) => reason.to_owned(),
            (error, _) => format!("自动粘贴失败：{error}"),
        })
    }

    const FOREGROUND_CHANGED: &str = "目标应用已离开前台，已取消自动粘贴";
    const PASTEBOARD_CHANGED: &str = "系统剪贴板内容已变化，已取消自动粘贴";

    fn confirm_paste_target(
        frontmost_pid: Option<i32>,
        selection_is_current: impl FnOnce() -> bool,
        expected_change_count: i64,
        current_change_count: impl FnOnce() -> i64,
    ) -> Result<i32, &'static str> {
        let pid = frontmost_pid.ok_or(FOREGROUND_CHANGED)?;
        let pasteboard_unchanged = current_change_count() == expected_change_count;
        if !selection_is_current() {
            return Err("操作已取消");
        }
        if !pasteboard_unchanged {
            return Err(PASTEBOARD_CHANGED);
        }
        Ok(pid)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn paste_uses_only_the_selected_pasteboard_ownership() {
            let mut private_board = PlatformPasteboard {
                pasteboard: NSPasteboard::pasteboardWithUniqueName(),
            };
            let expected = private_board.write_text("selected").unwrap();
            assert_eq!(
                private_board.read_text().unwrap().as_deref(),
                Some("selected")
            );
            assert_eq!(
                confirm_paste_target(
                    Some(42),
                    || true,
                    expected,
                    || private_board.change_count().unwrap()
                ),
                Ok(42)
            );
            assert_eq!(
                confirm_paste_target(None, || true, expected, || panic!("wrong target")),
                Err(FOREGROUND_CHANGED)
            );
            assert_eq!(
                confirm_paste_target(Some(42), || false, expected, || expected),
                Err("操作已取消")
            );
            let newer = private_board.write_text("replacement").unwrap();
            assert_ne!(expected, newer);
            assert_eq!(
                confirm_paste_target(
                    Some(42),
                    || true,
                    expected,
                    || private_board.change_count().unwrap()
                ),
                Err(PASTEBOARD_CHANGED)
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{ForegroundApplication, PasteboardBackend, PasteboardError};

    pub struct PlatformPasteboard;

    impl PlatformPasteboard {
        pub fn new() -> Result<Self, PasteboardError> {
            Err(PasteboardError::Unsupported)
        }
    }

    impl PasteboardBackend for PlatformPasteboard {
        fn change_count(&mut self) -> Result<i64, PasteboardError> {
            Err(PasteboardError::Unsupported)
        }
        fn read_text(&mut self) -> Result<Option<String>, PasteboardError> {
            Err(PasteboardError::Unsupported)
        }
        fn write_text(&mut self, _text: &str) -> Result<i64, PasteboardError> {
            Err(PasteboardError::Unsupported)
        }
        fn frontmost_bundle_id(&mut self) -> Result<Option<String>, PasteboardError> {
            frontmost_application().map(|application| application.map(|value| value.bundle_id))
        }
        fn activate_bundle_id(&mut self, _bundle_id: &str) -> Result<(), PasteboardError> {
            activate_application(_bundle_id)
        }
    }

    pub fn frontmost_application() -> Result<Option<super::ForegroundApplication>, PasteboardError>
    {
        Err(PasteboardError::Unsupported)
    }

    pub fn activate_application(_bundle_id: &str) -> Result<(), PasteboardError> {
        Err(PasteboardError::Unsupported)
    }

    pub fn activate_instance(_target: &ForegroundApplication) -> Result<(), PasteboardError> {
        Err(PasteboardError::Unsupported)
    }

    pub fn paste_if_frontmost(
        _target: &ForegroundApplication,
        _marker: u64,
        _expected_change_count: i64,
        _selection_is_current: impl FnOnce() -> bool,
    ) -> Result<(), String> {
        Err(PasteboardError::Unsupported.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{PasteboardBackend, PasteboardError, PasteboardService};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockState {
        change_count: i64,
        text: Option<String>,
        read_none_count: usize,
    }

    struct MockBackend {
        state: Arc<Mutex<MockState>>,
    }

    #[test]
    fn first_poll_only_primes_change_count_and_later_change_is_visible() {
        let state = Arc::new(Mutex::new(MockState {
            change_count: 1,
            text: Some("alpha".into()),
            read_none_count: 0,
        }));
        let backend = MockBackend {
            state: Arc::clone(&state),
        };
        let mut service = PasteboardService::with_backend(Box::new(backend));
        assert!(service.poll().unwrap().is_none());
        {
            let mut state = state.lock().unwrap();
            state.change_count = 2;
            state.text = Some("beta".into());
        }
        let change = service.poll().unwrap().unwrap();
        assert_eq!(change.change_count, 2);
        assert_eq!(change.text.as_deref(), Some("beta"));
    }

    #[test]
    fn delayed_text_representation_is_retried_for_the_same_change_count() {
        let state = Arc::new(Mutex::new(MockState {
            change_count: 1,
            text: Some("alpha".into()),
            read_none_count: 0,
        }));
        let backend = MockBackend {
            state: Arc::clone(&state),
        };
        let mut service = PasteboardService::with_backend(Box::new(backend));
        assert!(service.poll().unwrap().is_none());
        {
            let mut state = state.lock().unwrap();
            state.change_count = 2;
            state.text = Some("beta".into());
            state.read_none_count = 41;
        }
        for _ in 0..41 {
            assert!(service.poll().unwrap().is_none());
        }
        let change = service
            .poll()
            .unwrap()
            .expect("text should be retried after the representation is ready");
        assert_eq!(change.text.as_deref(), Some("beta"));
    }

    #[test]
    fn own_write_is_ignored_on_next_poll() {
        let backend = MockBackend {
            state: Arc::new(Mutex::new(MockState::default())),
        };
        let mut service = PasteboardService::with_backend(Box::new(backend));
        assert!(service.poll().unwrap().is_none());
        service.write_text("owned").unwrap();
        assert!(service.poll().unwrap().is_none());
    }

    #[test]
    fn own_write_marker_is_cleared_before_later_external_changes() {
        let state = Arc::new(Mutex::new(MockState::default()));
        let backend = MockBackend {
            state: Arc::clone(&state),
        };
        let mut service = PasteboardService::with_backend(Box::new(backend));
        assert!(service.poll().unwrap().is_none());
        service.write_text("owned").unwrap();
        assert!(service.poll().unwrap().is_none());
        {
            let mut state = state.lock().unwrap();
            state.change_count += 1;
            state.text = Some("external".into());
        }
        let change = service
            .poll()
            .unwrap()
            .expect("external change should be visible");
        assert_eq!(change.text.as_deref(), Some("external"));
    }

    #[test]
    fn backend_errors_are_returned_without_being_swallowed() {
        let mut service = PasteboardService::with_backend(Box::new(MockBackend {
            state: Arc::new(Mutex::new(MockState {
                change_count: -1,
                text: None,
                read_none_count: 0,
            })),
        }));
        assert_eq!(service.poll().unwrap_err(), PasteboardError::ReadFailed);
    }

    impl PasteboardBackend for MockBackend {
        fn change_count(&mut self) -> Result<i64, PasteboardError> {
            let state = self.state.lock().unwrap();
            if state.change_count < 0 {
                Err(PasteboardError::ReadFailed)
            } else {
                Ok(state.change_count)
            }
        }

        fn read_text(&mut self) -> Result<Option<String>, PasteboardError> {
            let mut state = self.state.lock().unwrap();
            if state.change_count < 0 {
                Err(PasteboardError::ReadFailed)
            } else if state.read_none_count > 0 {
                state.read_none_count -= 1;
                Ok(None)
            } else {
                Ok(state.text.clone())
            }
        }

        fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError> {
            let mut state = self.state.lock().unwrap();
            state.change_count += 1;
            state.text = Some(text.to_owned());
            Ok(state.change_count)
        }

        fn frontmost_bundle_id(&mut self) -> Result<Option<String>, PasteboardError> {
            Ok(Some("com.example.target".into()))
        }

        fn activate_bundle_id(&mut self, _bundle_id: &str) -> Result<(), PasteboardError> {
            Ok(())
        }
    }
}
