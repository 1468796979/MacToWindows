use diagnostics::{Diagnostics, PermissionState, TapState};
use event_model::{OutputEvent, PhysicalCode};
use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

mod clipboard;
mod input_source;
pub use clipboard::{
    activate_application, frontmost_application, ForegroundApplication, PasteboardBackend,
    PasteboardChange, PasteboardError, PasteboardService,
};
pub use input_source::{
    InputSourceCommand, InputSourceError, InputSourceService, InputSourceSnapshot,
};

static MARKER_COUNTER: AtomicU64 = AtomicU64::new(1);
pub fn next_marker() -> u64 {
    MARKER_COUNTER.fetch_add(1, Ordering::Relaxed)
}
pub fn is_own_marker(marker: u64, own_marker: u64) -> bool {
    marker != 0 && marker == own_marker
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectionError {
    PermissionDenied,
    Unavailable,
    InvalidEvent,
    Failed(String),
}

pub fn inject_outputs(outputs: &[OutputEvent], marker: u64) -> Result<(), InjectionError> {
    platform::inject_outputs(outputs, marker)
}

/// Inject outputs from inside an event-tap callback.
///
/// CoreGraphics requires replacement events posted while a tap callback is
/// running to go through that callback's proxy. Posting them with
/// `CGEventPost` and then suppressing the source event can otherwise leave the
/// replacement visible to the tap but never deliver it to the foreground app.
///
/// # Safety
/// `proxy` must be the live proxy supplied to the current CoreGraphics event
/// tap callback, or a null pointer when no callback proxy is available.
pub unsafe fn inject_outputs_at(
    outputs: &[OutputEvent],
    marker: u64,
    proxy: RawEventRef,
) -> Result<(), InjectionError> {
    unsafe { platform::inject_outputs_at(outputs, marker, proxy) }
}
impl fmt::Display for InjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for InjectionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterOutcome<T> {
    Pass(T),
    Suppress,
}

/// The raw event handle passed across the CoreGraphics callback boundary.
/// It is intentionally opaque to the platform-independent adapter.
pub type RawEventRef = *mut c_void;
pub const TAP_DISABLED_BY_TIMEOUT: u32 = u32::MAX - 1;
pub const TAP_DISABLED_BY_USER_INPUT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTapAction {
    Pass,
    Suppress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventTapEvent {
    event_type: u32,
    raw_event: RawEventRef,
    proxy: RawEventRef,
}

impl EventTapEvent {
    pub const fn new(event_type: u32, raw_event: RawEventRef) -> Self {
        Self::with_proxy(std::ptr::null_mut(), event_type, raw_event)
    }

    pub const fn with_proxy(proxy: RawEventRef, event_type: u32, raw_event: RawEventRef) -> Self {
        Self {
            event_type,
            raw_event,
            proxy,
        }
    }

    pub const fn event_type(self) -> u32 {
        self.event_type
    }

    pub const fn raw_event(self) -> RawEventRef {
        self.raw_event
    }

    pub const fn proxy(self) -> RawEventRef {
        self.proxy
    }

    pub const fn is_tap_disabled(self) -> bool {
        self.event_type == TAP_DISABLED_BY_TIMEOUT || self.event_type == TAP_DISABLED_BY_USER_INPUT
    }
}

pub trait EventAdapter {
    fn permission_state(&self) -> PermissionState;
    fn tap_state(&self) -> TapState;
    fn start(&self) -> Result<(), InjectionError>;
    fn stop(&self);
    fn restart(&self) -> Result<(), InjectionError> {
        self.stop();
        self.start()
    }
    fn own_marker(&self) -> u64;
    fn should_filter_marker(&self, marker: u64) -> bool {
        is_own_marker(marker, self.own_marker())
    }
}

pub struct SafeAdapter {
    diagnostics: Arc<Diagnostics>,
    marker: u64,
    running: AtomicBool,
}

fn permission_state(listen: bool, post: bool) -> PermissionState {
    if listen && post {
        PermissionState::Granted
    } else {
        PermissionState::Denied
    }
}

impl Default for SafeAdapter {
    fn default() -> Self {
        Self::new()
    }
}
impl SafeAdapter {
    pub fn new() -> Self {
        Self::with_diagnostics(Arc::new(Diagnostics::new()))
    }
    pub fn with_diagnostics(diagnostics: Arc<Diagnostics>) -> Self {
        Self {
            diagnostics,
            marker: next_marker(),
            running: AtomicBool::new(false),
        }
    }
    pub fn set_permission(&self, state: PermissionState) {
        self.diagnostics.set_permission(state);
    }
    pub fn set_tap_state(&self, state: TapState) {
        self.diagnostics.set_tap_state(state);
    }
    pub fn diagnostics(&self) -> Arc<Diagnostics> {
        Arc::clone(&self.diagnostics)
    }
    /// Re-read the operating system's input permissions and update diagnostics.
    ///
    /// The UI can call this without enabling the adapter. Keeping this separate
    /// from `start` makes the permission check useful when the app is paused.
    pub fn refresh_permissions(&self) -> PermissionState {
        #[cfg(target_os = "macos")]
        {
            let (listen, post) = platform::preflight_permissions();
            let state = permission_state(listen, post);
            self.diagnostics.set_permission(state);
            if state == PermissionState::Denied {
                self.running.store(false, Ordering::Release);
                self.diagnostics.set_tap_state(TapState::Degraded);
            }
            state
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.permission_state()
        }
    }
    /// Ask CoreGraphics to register this app for listen/post access, then cache
    /// the resulting state for the UI and runtime controller.
    pub fn request_permissions(&self) -> PermissionState {
        #[cfg(target_os = "macos")]
        {
            let (listen, post) = platform::request_permissions();
            let state = permission_state(listen, post);
            self.diagnostics.set_permission(state);
            if state == PermissionState::Denied {
                self.running.store(false, Ordering::Release);
                self.diagnostics.set_tap_state(TapState::Degraded);
            }
            state
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.refresh_permissions()
        }
    }

    /// Return the live CoreGraphics preflight result for each capability.
    ///
    /// The aggregate permission state is intentionally kept in diagnostics,
    /// but exposing both values lets the UI explain which macOS permission is
    /// still missing instead of reporting a generic failure.
    pub fn permission_access(&self) -> (bool, bool) {
        #[cfg(target_os = "macos")]
        {
            platform::preflight_permissions()
        }
        #[cfg(not(target_os = "macos"))]
        {
            (false, false)
        }
    }
    pub fn process<T, F>(&self, original: T, inject: F) -> AdapterOutcome<T>
    where
        F: FnOnce(&T) -> Result<(), InjectionError>,
    {
        self.diagnostics.record_seen();
        if !self.running.load(Ordering::Acquire) {
            self.diagnostics.record_passed();
            return AdapterOutcome::Pass(original);
        }
        match inject(&original) {
            Ok(()) => {
                self.diagnostics.record_injected();
                self.diagnostics.record_suppressed();
                AdapterOutcome::Suppress
            }
            Err(_) => {
                self.diagnostics.record_injection_failure();
                self.diagnostics.set_tap_state(TapState::Degraded);
                self.diagnostics.record_passed();
                AdapterOutcome::Pass(original)
            }
        }
    }
    pub fn process_injected<T>(&self, event: T, marker: u64) -> AdapterOutcome<T> {
        if self.should_filter_marker(marker) {
            self.diagnostics.record_passed();
        } else {
            self.diagnostics.record_seen();
        }
        AdapterOutcome::Pass(event)
    }
}
impl EventAdapter for SafeAdapter {
    fn permission_state(&self) -> PermissionState {
        self.diagnostics.snapshot().permission
    }
    fn tap_state(&self) -> TapState {
        self.diagnostics.snapshot().tap_state
    }
    fn start(&self) -> Result<(), InjectionError> {
        if self.permission_state() == PermissionState::Denied {
            self.request_permissions();
            if self.permission_state() == PermissionState::Denied {
                self.diagnostics.set_tap_state(TapState::Degraded);
                return Err(InjectionError::PermissionDenied);
            }
        }
        #[cfg(target_os = "macos")]
        {
            let (listen, post) = platform::preflight_permissions();
            if !listen || !post {
                self.request_permissions();
                let (listen, post) = platform::preflight_permissions();
                if !listen || !post {
                    self.diagnostics.set_permission(PermissionState::Denied);
                    self.diagnostics.set_tap_state(TapState::Degraded);
                    return Err(InjectionError::PermissionDenied);
                }
            }
            self.diagnostics.set_permission(PermissionState::Granted);
        }
        self.diagnostics.set_tap_state(TapState::Running);
        self.running.store(true, Ordering::Release);
        Ok(())
    }
    fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.diagnostics.set_tap_state(TapState::Stopped);
    }
    fn own_marker(&self) -> u64 {
        self.marker
    }

    fn restart(&self) -> Result<(), InjectionError> {
        self.diagnostics.set_tap_state(TapState::Restarting);
        self.diagnostics.record_tap_restart();
        self.running.store(false, Ordering::Release);
        self.start()
    }
}

#[cfg(target_os = "macos")]
pub mod platform {
    use super::{
        EventTapAction, EventTapEvent, InjectionError, OutputEvent, PhysicalCode, RawEventRef,
        TAP_DISABLED_BY_TIMEOUT, TAP_DISABLED_BY_USER_INPUT,
    };
    use event_model::EventPhase;
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::ptr;

    pub const CORE_GRAPHICS_BACKEND: bool = true;

    pub type EventTapRef = *mut c_void;
    pub type EventRef = RawEventRef;
    pub type RunLoopRef = *mut c_void;
    pub const EVENT_SOURCE_USER_DATA_FIELD: u32 = 42;
    type RunLoopSourceRef = *mut c_void;
    type EventTapCallback = Box<dyn FnMut(EventTapEvent) -> EventTapAction + Send + 'static>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum EventTapError {
        PermissionDenied,
        CreationFailed,
        RunLoopSourceFailed,
        Unavailable,
    }

    struct CallbackState {
        callback: EventTapCallback,
        tap: EventTapRef,
    }

    // The concrete event callback is kept behind this boundary so the rest of
    // the workspace never needs to link CoreGraphics on non-macOS systems.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGPreflightListenEventAccess() -> bool;
        pub fn CGPreflightPostEventAccess() -> bool;
        pub fn CGRequestListenEventAccess() -> bool;
        pub fn CGRequestPostEventAccess() -> bool;
        pub fn CGEventTapPostEvent(tap: *mut c_void, event: *mut c_void);
        pub fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: Option<
                unsafe extern "C" fn(*mut c_void, u32, RawEventRef, *mut c_void) -> RawEventRef,
            >,
            user_info: *mut c_void,
        ) -> EventTapRef;
        pub fn CGEventTapEnable(tap: EventTapRef, enable: bool);
        pub fn CGEventTapIsEnabled(tap: EventTapRef) -> bool;
        pub fn CGEventSetIntegerValueField(event: EventRef, field: u32, value: i64);
        pub fn CGEventGetIntegerValueField(event: EventRef, field: u32) -> i64;
        pub fn CGEventGetDoubleValueField(event: EventRef, field: u32) -> f64;
        pub fn CGEventGetFlags(event: EventRef) -> u64;
        pub fn CGEventGetTimestamp(event: EventRef) -> u64;
        pub fn CGEventPost(tap: u32, event: EventRef);
        pub fn CGEventCreateKeyboardEvent(
            source: *mut c_void,
            keycode: u16,
            keydown: bool,
        ) -> EventRef;
        pub fn CGEventSetFlags(event: EventRef, flags: u64);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: EventTapRef,
            order: isize,
        ) -> RunLoopSourceRef;
        fn CFRunLoopGetCurrent() -> RunLoopRef;
        fn CFRunLoopAddSource(run_loop: RunLoopRef, source: RunLoopSourceRef, mode: *const c_void);
        fn CFRunLoopRemoveSource(
            run_loop: RunLoopRef,
            source: RunLoopSourceRef,
            mode: *const c_void,
        );
        fn CFRunLoopRun();
        fn CFRelease(value: *mut c_void);
        static kCFRunLoopCommonModes: *const c_void;
    }

    unsafe extern "C" fn event_tap_callback(
        proxy: *mut c_void,
        event_type: u32,
        event: RawEventRef,
        user_info: *mut c_void,
    ) -> RawEventRef {
        if user_info.is_null() {
            return event;
        }
        let state = &mut *(user_info.cast::<CallbackState>());
        if event_type == TAP_DISABLED_BY_TIMEOUT || event_type == TAP_DISABLED_BY_USER_INPUT {
            if !state.tap.is_null() {
                CGEventTapEnable(state.tap, true);
            }
            return event;
        }
        let decision = catch_unwind(AssertUnwindSafe(|| {
            (state.callback)(EventTapEvent::with_proxy(proxy, event_type, event))
        }));
        match decision {
            Ok(EventTapAction::Pass) | Err(_) => event,
            Ok(EventTapAction::Suppress) => ptr::null_mut(),
        }
    }

    pub struct EventTap {
        tap: EventTapRef,
        source: RunLoopSourceRef,
        run_loop: RunLoopRef,
        callback_state: *mut CallbackState,
        running: bool,
    }

    impl EventTap {
        pub fn create<F>(events_of_interest: u64, callback: F) -> Result<Self, EventTapError>
        where
            F: FnMut(EventTapEvent) -> EventTapAction + Send + 'static,
        {
            let (listen, _) = preflight_permissions();
            if !listen {
                return Err(EventTapError::PermissionDenied);
            }
            let callback_state = Box::into_raw(Box::new(CallbackState {
                callback: Box::new(callback),
                tap: ptr::null_mut(),
            }));
            let tap = unsafe {
                CGEventTapCreate(
                    EVENT_TAP_LOCATION,
                    0, // kCGHeadInsertEventTap
                    0, // kCGEventTapOptionDefault
                    events_of_interest,
                    Some(event_tap_callback),
                    callback_state.cast(),
                )
            };
            if tap.is_null() {
                unsafe { drop(Box::from_raw(callback_state)) };
                return Err(EventTapError::CreationFailed);
            }
            unsafe {
                (*callback_state).tap = tap;
            }
            Ok(Self {
                tap,
                source: ptr::null_mut(),
                run_loop: ptr::null_mut(),
                callback_state,
                running: false,
            })
        }

        pub fn start(&mut self) -> Result<(), EventTapError> {
            if self.running {
                return Ok(());
            }
            let run_loop = unsafe { CFRunLoopGetCurrent() };
            let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), self.tap, 0) };
            if source.is_null() {
                return Err(EventTapError::RunLoopSourceFailed);
            }
            unsafe {
                CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
                CGEventTapEnable(self.tap, true);
            }
            self.run_loop = run_loop;
            self.source = source;
            self.running = true;
            Ok(())
        }

        pub fn stop(&mut self) {
            if self.tap.is_null() {
                return;
            }
            unsafe {
                CGEventTapEnable(self.tap, false);
                if !self.source.is_null() && !self.run_loop.is_null() {
                    CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopCommonModes);
                    CFRelease(self.source);
                }
            }
            self.source = ptr::null_mut();
            self.run_loop = ptr::null_mut();
            self.running = false;
        }

        pub fn restart(&mut self) -> Result<(), EventTapError> {
            self.stop();
            self.start()
        }

        pub fn is_running(&self) -> bool {
            self.running
        }

        /// Runs the current thread's CFRunLoop after [`Self::start`].
        pub fn run_loop(&self) {
            if self.running {
                unsafe { CFRunLoopRun() }
            }
        }
    }

    /// kCGSessionEventTap: the session-level tap available to ordinary user
    /// processes. kCGHIDEventTap is restricted to root processes.
    pub const EVENT_TAP_LOCATION: u32 = 1;

    impl Drop for EventTap {
        fn drop(&mut self) {
            self.stop();
            unsafe {
                if !self.tap.is_null() {
                    CFRelease(self.tap);
                }
                if !self.callback_state.is_null() {
                    drop(Box::from_raw(self.callback_state));
                }
            }
            self.tap = ptr::null_mut();
            self.callback_state = ptr::null_mut();
        }
    }

    pub fn preflight_permissions() -> (bool, bool) {
        unsafe { (CGPreflightListenEventAccess(), CGPreflightPostEventAccess()) }
    }

    pub fn request_permissions() -> (bool, bool) {
        unsafe { (CGRequestListenEventAccess(), CGRequestPostEventAccess()) }
    }

    pub fn normalize_event(input: EventTapEvent) -> Option<event_model::NormalizedEvent> {
        use event_model::{
            AppContext, DeviceSource, EventKind, EventOrigin, EventPhase, NormalizedEvent,
            PhysicalCode, ScrollAxis, ScrollDelta,
        };
        let raw = input.raw_event();
        if raw.is_null() || input.is_tap_disabled() {
            return None;
        }
        let event_type = input.event_type();
        let marker =
            unsafe { CGEventGetIntegerValueField(raw, EVENT_SOURCE_USER_DATA_FIELD) as u64 };
        let origin = if marker == 0 {
            EventOrigin::Hardware
        } else {
            EventOrigin::Injected { marker }
        };
        let flags = unsafe { CGEventGetFlags(raw) };
        let mut modifiers = modifiers_from_flags(flags);
        let key_code = unsafe { CGEventGetIntegerValueField(raw, 9) as u16 };
        let timestamp_ns = unsafe { CGEventGetTimestamp(raw) };
        let (source, kind, phase, code, scroll) = match event_type {
            10 => {
                let repeat = unsafe { CGEventGetIntegerValueField(raw, 8) != 0 };
                (
                    DeviceSource::Keyboard,
                    EventKind::Keyboard,
                    if repeat {
                        EventPhase::Repeat
                    } else {
                        EventPhase::Down
                    },
                    PhysicalCode::Key(key_code),
                    None,
                )
            }
            11 => (
                DeviceSource::Keyboard,
                EventKind::Keyboard,
                EventPhase::Up,
                PhysicalCode::Key(key_code),
                None,
            ),
            12 => {
                modifiers = modifier_for_key_code(key_code, flags);
                (
                    DeviceSource::Keyboard,
                    EventKind::FlagsChanged,
                    EventPhase::Changed,
                    PhysicalCode::Key(key_code),
                    None,
                )
            }
            1 | 2 | 3 | 4 | 25 | 26 => {
                let button = match event_type {
                    1 | 2 => event_model::MouseButton::Left,
                    3 | 4 => event_model::MouseButton::Right,
                    _ => match unsafe { CGEventGetIntegerValueField(raw, 3) } {
                        2 => event_model::MouseButton::Middle,
                        value => event_model::MouseButton::Other(value as u8),
                    },
                };
                (
                    DeviceSource::Mouse,
                    EventKind::MouseButton,
                    if matches!(event_type, 1 | 3 | 25) {
                        EventPhase::Down
                    } else {
                        EventPhase::Up
                    },
                    PhysicalCode::MouseButton(button),
                    None,
                )
            }
            22 => {
                let vertical = unsafe { CGEventGetDoubleValueField(raw, 11) };
                let horizontal = unsafe { CGEventGetDoubleValueField(raw, 12) };
                let axis = if horizontal.abs() > 0.0 && vertical.abs() == 0.0 {
                    ScrollAxis::Horizontal
                } else {
                    ScrollAxis::Vertical
                };
                (
                    DeviceSource::Mouse,
                    EventKind::Scroll,
                    EventPhase::Motion,
                    PhysicalCode::Scroll(axis),
                    Some(ScrollDelta {
                        x: horizontal,
                        y: vertical,
                    }),
                )
            }
            _ => return None,
        };
        Some(NormalizedEvent {
            source,
            kind,
            phase,
            code,
            modifiers,
            location: None,
            scroll,
            timestamp_ns,
            app_context: None::<AppContext>,
            origin,
        })
    }

    fn modifiers_from_flags(flags: u64) -> event_model::ModifierSet {
        let mut modifiers = event_model::ModifierSet::NONE;
        if flags & 0x0002_0000 != 0 {
            modifiers.insert(event_model::ModifierSet::LEFT_SHIFT);
        }
        if flags & 0x0004_0000 != 0 {
            modifiers.insert(event_model::ModifierSet::LEFT_CONTROL);
        }
        if flags & 0x0008_0000 != 0 {
            modifiers.insert(event_model::ModifierSet::LEFT_OPTION);
        }
        if flags & 0x0010_0000 != 0 {
            modifiers.insert(event_model::ModifierSet::LEFT_COMMAND);
        }
        modifiers
    }

    fn modifier_for_key_code(code: u16, flags: u64) -> event_model::ModifierSet {
        let mut modifiers = event_model::ModifierSet::NONE;
        match code {
            56 => {
                if flags & 0x0002_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::LEFT_SHIFT)
                }
            }
            60 => {
                if flags & 0x0002_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::RIGHT_SHIFT)
                }
            }
            // A FlagsChanged event carries the state of every currently
            // pressed modifier.  Test the bit belonging to the key that
            // changed instead of treating any non-zero flags as a press.
            // For example, releasing Control while Shift remains held must
            // be normalized as Control-up, not another Control-down.
            58 => {
                if flags & 0x0008_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::LEFT_OPTION)
                }
            }
            61 => {
                if flags & 0x0008_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::RIGHT_OPTION)
                }
            }
            55 => {
                if flags & 0x0010_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::LEFT_COMMAND)
                }
            }
            54 => {
                if flags & 0x0010_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::RIGHT_COMMAND)
                }
            }
            59 => {
                if flags & 0x0004_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::LEFT_CONTROL)
                }
            }
            62 => {
                if flags & 0x0004_0000 != 0 {
                    modifiers.insert(event_model::ModifierSet::RIGHT_CONTROL)
                }
            }
            _ => modifiers = modifiers_from_flags(flags),
        }
        modifiers
    }

    /// Posts a valid CoreGraphics event after attaching this process marker.
    ///
    /// # Safety
    /// `event` must be a live CGEventRef owned by the caller.
    pub unsafe fn inject_marked(event: RawEventRef, marker: u64) {
        CGEventSetIntegerValueField(event, EVENT_SOURCE_USER_DATA_FIELD, marker as i64);
        CGEventPost(1, event);
    }

    /// Reads the user-data marker from a live CoreGraphics event.
    ///
    /// # Safety
    /// `event` must be a live CGEventRef owned by the callback.
    pub unsafe fn marker(event: RawEventRef) -> u64 {
        CGEventGetIntegerValueField(event, EVENT_SOURCE_USER_DATA_FIELD) as u64
    }

    pub fn inject_outputs(outputs: &[OutputEvent], marker: u64) -> Result<(), InjectionError> {
        unsafe { inject_outputs_at(outputs, marker, std::ptr::null_mut()) }
    }

    /// # Safety
    /// `proxy` must be the live proxy supplied to the current CoreGraphics
    /// event tap callback, or a null pointer when no callback proxy exists.
    pub unsafe fn inject_outputs_at(
        outputs: &[OutputEvent],
        marker: u64,
        proxy: RawEventRef,
    ) -> Result<(), InjectionError> {
        let (_, post) = preflight_permissions();
        if !post {
            return Err(InjectionError::PermissionDenied);
        }
        for output in outputs {
            match output {
                OutputEvent::Key {
                    code: PhysicalCode::Key(code),
                    phase,
                    modifiers,
                } => {
                    let event = unsafe {
                        CGEventCreateKeyboardEvent(
                            std::ptr::null_mut(),
                            *code,
                            *phase != EventPhase::Up,
                        )
                    };
                    if event.is_null() {
                        return Err(InjectionError::Unavailable);
                    }
                    unsafe {
                        CGEventSetFlags(event, flags_for_modifiers(*modifiers));
                        CGEventSetIntegerValueField(
                            event,
                            EVENT_SOURCE_USER_DATA_FIELD,
                            marker as i64,
                        );
                        post_event(event, proxy);
                        CFRelease(event);
                    }
                }
                OutputEvent::MouseButton { .. } | OutputEvent::Scroll { .. } => {
                    return Err(InjectionError::Unavailable);
                }
                OutputEvent::SystemShortcut(shortcut) => {
                    inject_system_shortcut(*shortcut, marker, proxy)?;
                }
                _ => return Err(InjectionError::InvalidEvent),
            }
        }
        Ok(())
    }

    fn inject_system_shortcut(
        shortcut: event_model::SystemShortcut,
        marker: u64,
        proxy: RawEventRef,
    ) -> Result<(), InjectionError> {
        match shortcut {
            // macOS's native application switcher is Command+Tab. Emit a
            // complete sequence for each logical Windows Alt+Tab action so the
            // original physical gesture can be safely suppressed.
            event_model::SystemShortcut::SwitchApplication => {
                let command_down = create_keyboard_event(55, true, 0x0010_0000, marker)?;
                let tab_down = create_keyboard_event(48, true, 0x0010_0000, marker)?;
                let tab_up = create_keyboard_event(48, false, 0x0010_0000, marker)?;
                let command_up = create_keyboard_event(55, false, 0, marker)?;
                post_and_release(command_down, proxy);
                post_and_release(tab_down, proxy);
                post_and_release(tab_up, proxy);
                post_and_release(command_up, proxy);
                Ok(())
            }
            // Chrome and Firefox use Command+Shift+R for a hard refresh.
            event_model::SystemShortcut::HardRefresh => {
                let command_down = create_keyboard_event(55, true, 0x0010_0000, marker)?;
                let shift_down = create_keyboard_event(60, true, 0x0002_0000, marker)?;
                let r_down = create_keyboard_event(15, true, 0x0012_0000, marker)?;
                let r_up = create_keyboard_event(15, false, 0x0012_0000, marker)?;
                let shift_up = create_keyboard_event(60, false, 0x0010_0000, marker)?;
                let command_up = create_keyboard_event(55, false, 0, marker)?;
                post_and_release(command_down, proxy);
                post_and_release(shift_down, proxy);
                post_and_release(r_down, proxy);
                post_and_release(r_up, proxy);
                post_and_release(shift_up, proxy);
                post_and_release(command_up, proxy);
                Ok(())
            }
            event_model::SystemShortcut::LockScreen
            | event_model::SystemShortcut::ShowDesktop
            | event_model::SystemShortcut::CloseWindow
            | event_model::SystemShortcut::SwitchInputSource
            | event_model::SystemShortcut::TogglePinyinEnglish
            | event_model::SystemShortcut::ShowClipboard => Err(InjectionError::InvalidEvent),
        }
    }

    fn create_keyboard_event(
        key_code: u16,
        key_down: bool,
        flags: u64,
        marker: u64,
    ) -> Result<EventRef, InjectionError> {
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), key_code, key_down) };
        if event.is_null() {
            return Err(InjectionError::Unavailable);
        }
        unsafe {
            CGEventSetFlags(event, flags);
            CGEventSetIntegerValueField(event, EVENT_SOURCE_USER_DATA_FIELD, marker as i64);
        }
        Ok(event)
    }

    fn post_and_release(event: EventRef, proxy: RawEventRef) {
        unsafe {
            post_event(event, proxy);
            CFRelease(event);
        }
    }

    unsafe fn post_event(event: EventRef, proxy: RawEventRef) {
        if proxy.is_null() {
            CGEventPost(EVENT_TAP_LOCATION, event);
        } else {
            CGEventTapPostEvent(proxy, event);
        }
    }

    fn flags_for_modifiers(modifiers: event_model::ModifierSet) -> u64 {
        let mut flags = 0;
        if modifiers.contains(event_model::ModifierSet::LEFT_SHIFT)
            || modifiers.contains(event_model::ModifierSet::RIGHT_SHIFT)
        {
            flags |= 0x0002_0000;
        }
        if modifiers.contains(event_model::ModifierSet::LEFT_CONTROL)
            || modifiers.contains(event_model::ModifierSet::RIGHT_CONTROL)
        {
            flags |= 0x0004_0000;
        }
        if modifiers.contains(event_model::ModifierSet::LEFT_ALT)
            || modifiers.contains(event_model::ModifierSet::RIGHT_ALT)
        {
            flags |= 0x0008_0000;
        }
        if modifiers.contains(event_model::ModifierSet::LEFT_WINDOWS)
            || modifiers.contains(event_model::ModifierSet::RIGHT_WINDOWS)
            || modifiers.contains(event_model::ModifierSet::LEFT_COMMAND)
            || modifiers.contains(event_model::ModifierSet::RIGHT_COMMAND)
        {
            flags |= 0x0010_0000;
        }
        flags
    }

    #[cfg(test)]
    mod modifier_tests {
        use super::{
            modifier_for_key_code, normalize_event, CFRelease, CGEventCreateKeyboardEvent,
            CGEventSetFlags, EventTapEvent,
        };
        use event_model::{ModifierSet, PhysicalCode};

        const SHIFT_FLAG: u64 = 0x0002_0000;
        const CONTROL_FLAG: u64 = 0x0004_0000;

        fn flags_changed(key_code: u16, flags: u64) -> event_model::NormalizedEvent {
            let event =
                unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), key_code, false) };
            assert!(!event.is_null());
            unsafe { CGEventSetFlags(event, flags) };
            let normalized = normalize_event(EventTapEvent::new(12, event))
                .expect("FlagsChanged event should normalize");
            unsafe { CFRelease(event) };
            normalized
        }

        #[test]
        fn right_shift_flags_are_normalized_as_right_shift() {
            assert_eq!(
                modifier_for_key_code(60, SHIFT_FLAG),
                ModifierSet::RIGHT_SHIFT
            );
        }

        #[test]
        fn ctrl_release_while_shift_held_is_normalized_as_control_up() {
            let event = flags_changed(59, SHIFT_FLAG);
            assert_eq!(event.code, PhysicalCode::left_control());
            assert_eq!(event.modifiers, ModifierSet::NONE);
        }

        #[test]
        fn ctrl_shift_release_orders_normalize_without_stuck_modifiers() {
            // Control then Shift, release Control first while Shift remains
            // held, then release Shift.
            let ctrl_down = flags_changed(59, CONTROL_FLAG);
            let shift_down = flags_changed(56, CONTROL_FLAG | SHIFT_FLAG);
            let ctrl_up = flags_changed(59, SHIFT_FLAG);
            let shift_up = flags_changed(56, 0);
            assert_eq!(ctrl_down.modifiers, ModifierSet::LEFT_CONTROL);
            assert_eq!(shift_down.modifiers, ModifierSet::LEFT_SHIFT);
            assert_eq!(ctrl_up.modifiers, ModifierSet::NONE);
            assert_eq!(shift_up.modifiers, ModifierSet::NONE);

            // Shift then Control, release Shift first while Control remains
            // held, then release Control.
            let shift_down = flags_changed(56, SHIFT_FLAG);
            let ctrl_down = flags_changed(59, CONTROL_FLAG | SHIFT_FLAG);
            let shift_up = flags_changed(56, CONTROL_FLAG);
            let ctrl_up = flags_changed(59, 0);
            assert_eq!(shift_down.modifiers, ModifierSet::LEFT_SHIFT);
            assert_eq!(ctrl_down.modifiers, ModifierSet::LEFT_CONTROL);
            assert_eq!(shift_up.modifiers, ModifierSet::NONE);
            assert_eq!(ctrl_up.modifiers, ModifierSet::NONE);
        }
    }
}
#[cfg(not(target_os = "macos"))]
pub mod platform {
    use super::{
        EventTapAction, EventTapEvent, InjectionError, OutputEvent, TAP_DISABLED_BY_TIMEOUT,
        TAP_DISABLED_BY_USER_INPUT,
    };

    pub const CORE_GRAPHICS_BACKEND: bool = false;

    pub type EventTapRef = *mut std::ffi::c_void;
    pub type EventRef = super::RawEventRef;
    pub type RunLoopRef = *mut std::ffi::c_void;
    pub const EVENT_SOURCE_USER_DATA_FIELD: u32 = 42;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum EventTapError {
        PermissionDenied,
        CreationFailed,
        RunLoopSourceFailed,
        Unavailable,
    }

    pub struct EventTap;

    pub const EVENT_TAP_LOCATION: u32 = 1;

    impl EventTap {
        pub fn create<F>(_events_of_interest: u64, _callback: F) -> Result<Self, EventTapError>
        where
            F: FnMut(EventTapEvent) -> EventTapAction + Send + 'static,
        {
            Err(EventTapError::Unavailable)
        }

        pub fn start(&mut self) -> Result<(), EventTapError> {
            Err(EventTapError::Unavailable)
        }

        pub fn stop(&mut self) {}

        pub fn restart(&mut self) -> Result<(), EventTapError> {
            Err(EventTapError::Unavailable)
        }

        pub fn is_running(&self) -> bool {
            false
        }

        pub fn run_loop(&self) {}
    }

    pub fn preflight_permissions() -> (bool, bool) {
        (false, false)
    }

    pub fn request_permissions() -> (bool, bool) {
        (false, false)
    }

    pub unsafe fn inject_marked(_event: super::RawEventRef, _marker: u64) {}

    pub unsafe fn marker(_event: super::RawEventRef) -> u64 {
        0
    }

    pub unsafe fn inject_outputs_at(
        _outputs: &[OutputEvent],
        _marker: u64,
        _proxy: super::RawEventRef,
    ) -> Result<(), InjectionError> {
        Err(InjectionError::Unavailable)
    }

    pub fn inject_outputs(_outputs: &[OutputEvent], _marker: u64) -> Result<(), InjectionError> {
        Err(InjectionError::Unavailable)
    }
}

#[cfg(target_os = "macos")]
pub use platform::normalize_event;
pub use platform::{preflight_permissions, EventTap, EventTapError};

#[cfg(test)]
mod lib_test;
