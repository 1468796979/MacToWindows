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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundApplication {
    pub bundle_id: String,
}

/// Reads the frontmost application using the platform's application APIs.
/// Callers that need AppKit thread affinity should invoke this on the main
/// thread.
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
}

impl PasteboardService {
    pub fn with_backend(backend: Box<dyn PasteboardBackend>) -> Self {
        Self {
            backend,
            last_change_count: None,
            ignored_change_count: None,
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
        if self.last_change_count == Some(change_count) {
            return Ok(None);
        }
        self.last_change_count = Some(change_count);
        Ok(Some(PasteboardChange {
            change_count,
            text: self.backend.read_text()?,
        }))
    }

    pub fn write_text(&mut self, text: &str) -> Result<i64, PasteboardError> {
        let change_count = self.backend.write_text(text)?;
        self.last_change_count = Some(change_count);
        self.ignored_change_count = Some(change_count);
        Ok(change_count)
    }

    pub fn frontmost_application(
        &mut self,
    ) -> Result<Option<ForegroundApplication>, PasteboardError> {
        Ok(self
            .backend
            .frontmost_bundle_id()?
            .map(|bundle_id| ForegroundApplication { bundle_id }))
    }

    pub fn activate_application(&mut self, bundle_id: &str) -> Result<(), PasteboardError> {
        self.backend.activate_bundle_id(bundle_id)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{PasteboardBackend, PasteboardError};
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationOptions, NSPasteboard, NSPasteboardType,
        NSPasteboardTypeString, NSRunningApplication, NSWorkspace,
    };
    use objc2_foundation::NSString;

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
            self.pasteboard.clearContents();
            let value = NSString::from_str(text);
            if !self
                .pasteboard
                .setString_forType(&value, Self::pasteboard_type())
            {
                return Err(PasteboardError::WriteFailed);
            }
            Ok(self.pasteboard.changeCount() as i64)
        }

        fn frontmost_bundle_id(&mut self) -> Result<Option<String>, PasteboardError> {
            Ok(frontmost_application()?.map(|application| application.bundle_id))
        }

        fn activate_bundle_id(&mut self, bundle_id: &str) -> Result<(), PasteboardError> {
            activate_application(bundle_id)
        }
    }

    pub fn frontmost_application() -> Result<Option<super::ForegroundApplication>, PasteboardError>
    {
        Ok(NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|application| application.bundleIdentifier())
            .map(|bundle_id| super::ForegroundApplication {
                bundle_id: bundle_id.to_string(),
            }))
    }

    pub fn activate_application(bundle_id: &str) -> Result<(), PasteboardError> {
        let bundle_id = NSString::from_str(bundle_id);
        let applications =
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
        let Some(application) = applications.firstObject() else {
            return Err(PasteboardError::ApplicationUnavailable);
        };
        if application.isTerminated() {
            return Err(PasteboardError::ApplicationUnavailable);
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(PasteboardError::ApplicationUnavailable);
        };
        NSApplication::sharedApplication(mtm).yieldActivationToApplication(&application);
        let _ = application.unhide();
        #[allow(deprecated)]
        let options = NSApplicationActivationOptions::ActivateAllWindows
            | NSApplicationActivationOptions::ActivateIgnoringOtherApps;
        if application.activateWithOptions(options) || application.isActive() {
            return Ok(());
        }
        let _ = application.unhide();
        let _ = application.activateWithOptions(options);
        // Activation can complete asynchronously. The caller polls the
        // frontmost application before injecting Cmd+V, so a false return
        // here is not by itself an unavailable-application error.
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{PasteboardBackend, PasteboardError};

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
}

#[cfg(test)]
mod tests {
    use super::{PasteboardBackend, PasteboardError, PasteboardService};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockState {
        change_count: i64,
        text: Option<String>,
    }

    struct MockBackend {
        state: Arc<Mutex<MockState>>,
    }

    #[test]
    fn first_poll_only_primes_change_count_and_later_change_is_visible() {
        let state = Arc::new(Mutex::new(MockState {
            change_count: 1,
            text: Some("alpha".into()),
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
            let state = self.state.lock().unwrap();
            if state.change_count < 0 {
                Err(PasteboardError::ReadFailed)
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
