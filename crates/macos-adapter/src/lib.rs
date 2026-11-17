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

/// Inject a batch while retaining the source event's mouse and scroll metadata.
///
/// # Safety
/// `proxy` and `source_raw` must belong to the current event-tap callback, or be
/// null. The source is only borrowed for this call and is never retained.
pub unsafe fn inject_outputs_with_context(
    outputs: &[OutputEvent],
    marker: u64,
    proxy: RawEventRef,
    source_raw: RawEventRef,
) -> Result<(), InjectionError> {
    unsafe { platform::inject_outputs_with_context(outputs, marker, proxy, source_raw) }
}

/// Current hardware modifiers in native macOS semantics, independent of the
/// active profile and of events injected at the session tap.
pub fn physical_modifier_snapshot() -> event_model::ModifierSet {
    #[cfg(target_os = "macos")]
    {
        modifier_snapshot_from_key_state(|code| unsafe { platform::CGEventSourceKeyState(1, code) })
    }
    #[cfg(not(target_os = "macos"))]
    {
        event_model::ModifierSet::NONE
    }
}

/// Whether a keyboard key or mouse button is currently held by hardware.
/// Scroll axes and unsupported mouse button numbers are never held.
pub fn physical_code_is_down(code: PhysicalCode) -> bool {
    #[cfg(target_os = "macos")]
    {
        physical_code_is_down_with(
            code,
            |key| unsafe { platform::CGEventSourceKeyState(1, key) },
            |button| unsafe { platform::CGEventSourceButtonState(1, button) },
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = code;
        false
    }
}

#[cfg(any(target_os = "macos", test))]
fn modifier_snapshot_from_key_state<F>(mut key_state: F) -> event_model::ModifierSet
where
    F: FnMut(u16) -> bool,
{
    use event_model::ModifierSet as M;
    let mut modifiers = M::NONE;
    for (code, modifier) in [
        (56, M::LEFT_SHIFT),
        (60, M::RIGHT_SHIFT),
        (59, M::LEFT_CONTROL),
        (62, M::RIGHT_CONTROL),
        (58, M::LEFT_OPTION),
        (61, M::RIGHT_OPTION),
        (55, M::LEFT_COMMAND),
        (54, M::RIGHT_COMMAND),
    ] {
        if key_state(code) {
            modifiers.insert(modifier);
        }
    }
    modifiers
}

#[cfg(any(target_os = "macos", test))]
fn physical_code_is_down_with<K, B>(
    code: PhysicalCode,
    mut key_state: K,
    mut button_state: B,
) -> bool
where
    K: FnMut(u16) -> bool,
    B: FnMut(u32) -> bool,
{
    use event_model::MouseButton;
    match code {
        PhysicalCode::Key(key) => key_state(key),
        PhysicalCode::MouseButton(button) => {
            let number = match button {
                MouseButton::Left => 0,
                MouseButton::Right => 1,
                MouseButton::Middle => 2,
                MouseButton::Other(value) => u32::from(value),
            };
            number < 32 && button_state(number)
        }
        PhysicalCode::Scroll(_) => false,
    }
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

/// Physical modifier state belongs to one tap and is never updated by this
/// process's own replacement events.
pub struct EventNormalizer {
    own_marker: u64,
    modifiers: event_model::ModifierSet,
}

impl EventNormalizer {
    pub fn new(own_marker: u64) -> Self {
        Self {
            own_marker,
            modifiers: event_model::ModifierSet::NONE,
        }
    }

    pub fn reset(&mut self) {
        self.modifiers = event_model::ModifierSet::NONE;
    }

    pub fn normalize(&mut self, input: EventTapEvent) -> Option<event_model::NormalizedEvent> {
        if input.is_tap_disabled() {
            self.reset();
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            platform::normalize_with_state(
                input,
                self.own_marker,
                &mut self.modifiers,
                |code| unsafe { platform::CGEventSourceKeyState(1, code) },
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (input, self.own_marker);
            None
        }
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
        #[cfg(not(target_os = "macos"))]
        if self.permission_state() == PermissionState::Denied {
            self.diagnostics.set_tap_state(TapState::Degraded);
            return Err(InjectionError::PermissionDenied);
        }
        #[cfg(target_os = "macos")]
        {
            let (listen, post) = platform::preflight_permissions();
            if !listen || !post {
                self.diagnostics.set_permission(PermissionState::Denied);
                self.diagnostics.set_tap_state(TapState::Degraded);
                return Err(InjectionError::PermissionDenied);
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
    use std::sync::atomic::{AtomicBool, Ordering};

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

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
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
        pub fn CGEventSetDoubleValueField(event: EventRef, field: u32, value: f64);
        pub fn CGEventGetFlags(event: EventRef) -> u64;
        pub fn CGEventGetType(event: EventRef) -> u32;
        pub fn CGEventSetType(event: EventRef, event_type: u32);
        pub fn CGEventGetLocation(event: EventRef) -> CGPoint;
        pub fn CGEventCreate(source: *mut c_void) -> EventRef;
        pub fn CGEventCreateCopy(event: EventRef) -> EventRef;
        pub fn CGEventCreateMouseEvent(
            source: *mut c_void,
            event_type: u32,
            position: CGPoint,
            button: u32,
        ) -> EventRef;
        pub fn CGEventCreateScrollWheelEvent2(
            source: *mut c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> EventRef;
        pub fn CGEventSourceKeyState(state: i32, key_code: u16) -> bool;
        pub fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
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
        fn CFRunLoopRunInMode(
            mode: *const c_void,
            seconds: f64,
            return_after_source_handled: bool,
        ) -> i32;
        fn CFRelease(value: *mut c_void);
        static kCFRunLoopCommonModes: *const c_void;
        static kCFRunLoopDefaultMode: *const c_void;
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
            // Notify the owner before recovery so its latches and bindings can
            // be reset while the tap is still disabled.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                (state.callback)(EventTapEvent::with_proxy(proxy, event_type, event))
            }));
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

        /// Process callbacks on the owning thread, checking shutdown at least
        /// every 50ms when no callback is running. No run-loop pointer crosses
        /// threads, and a stop requested before startup cannot be lost.
        pub fn run_loop_until(&self, stop: &AtomicBool) {
            while self.running && !stop.load(Ordering::Acquire) {
                unsafe {
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, false);
                }
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
            1 | 2 | 3 | 4 | 6 | 7 | 25 | 26 | 27 => {
                let button = match event_type {
                    1 | 2 | 6 => event_model::MouseButton::Left,
                    3 | 4 | 7 => event_model::MouseButton::Right,
                    _ => match unsafe { CGEventGetIntegerValueField(raw, 3) } {
                        2 => event_model::MouseButton::Middle,
                        value => event_model::MouseButton::Other(value as u8),
                    },
                };
                (
                    DeviceSource::Mouse,
                    if matches!(event_type, 6 | 7 | 27) {
                        EventKind::MouseMove
                    } else {
                        EventKind::MouseButton
                    },
                    if matches!(event_type, 6 | 7 | 27) {
                        EventPhase::Motion
                    } else if matches!(event_type, 1 | 3 | 25) {
                        EventPhase::Down
                    } else {
                        EventPhase::Up
                    },
                    PhysicalCode::MouseButton(button),
                    None,
                )
            }
            22 => {
                let precise_delta = |line, fixed, pixel| {
                    let fixed_value = unsafe { CGEventGetDoubleValueField(raw, fixed) };
                    let pixel_value = unsafe { CGEventGetIntegerValueField(raw, pixel) } as f64;
                    if fixed_value != 0.0 {
                        fixed_value
                    } else if pixel_value != 0.0 {
                        pixel_value
                    } else {
                        (unsafe { CGEventGetIntegerValueField(raw, line) }) as f64
                    }
                };
                let vertical = precise_delta(11, 93, 96);
                let horizontal = precise_delta(12, 94, 97);
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
            location: if source == DeviceSource::Mouse {
                let point = unsafe { CGEventGetLocation(raw) };
                Some(event_model::Point {
                    x: point.x,
                    y: point.y,
                })
            } else {
                None
            },
            scroll,
            timestamp_ns,
            app_context: None::<AppContext>,
            origin,
        })
    }

    // Aggregate flags cannot distinguish the side that was released while its
    // counterpart remains held. These are the public SDK's device-side bits.
    const MODIFIER_KEYS: [(u64, u64, u16, event_model::ModifierSet); 8] = [
        (0x0002_0000, 0x02, 56, event_model::ModifierSet::LEFT_SHIFT),
        (0x0002_0000, 0x04, 60, event_model::ModifierSet::RIGHT_SHIFT),
        (
            0x0004_0000,
            0x01,
            59,
            event_model::ModifierSet::LEFT_CONTROL,
        ),
        (
            0x0004_0000,
            0x2000,
            62,
            event_model::ModifierSet::RIGHT_CONTROL,
        ),
        (0x0008_0000, 0x20, 58, event_model::ModifierSet::LEFT_OPTION),
        (
            0x0008_0000,
            0x40,
            61,
            event_model::ModifierSet::RIGHT_OPTION,
        ),
        (
            0x0010_0000,
            0x08,
            55,
            event_model::ModifierSet::LEFT_COMMAND,
        ),
        (
            0x0010_0000,
            0x10,
            54,
            event_model::ModifierSet::RIGHT_COMMAND,
        ),
    ];

    fn modifiers_from_flags(flags: u64) -> event_model::ModifierSet {
        let mut modifiers = event_model::ModifierSet::NONE;
        for (aggregate, side, _, modifier) in MODIFIER_KEYS {
            if flags & aggregate != 0 && flags & side != 0 {
                modifiers.insert(modifier);
            }
        }
        modifiers
    }

    fn modifier_for_key_code(code: u16, flags: u64) -> event_model::ModifierSet {
        let all = modifiers_from_flags(flags);
        MODIFIER_KEYS
            .iter()
            .find(|(_, _, key, _)| *key == code)
            .map(|(_, _, _, modifier)| {
                if all.contains(*modifier) {
                    *modifier
                } else {
                    event_model::ModifierSet::NONE
                }
            })
            .unwrap_or(all)
    }

    pub(super) fn normalize_with_state<F>(
        input: EventTapEvent,
        own_marker: u64,
        state: &mut event_model::ModifierSet,
        key_state: F,
    ) -> Option<event_model::NormalizedEvent>
    where
        F: Fn(u16) -> bool,
    {
        use event_model::{EventKind, EventOrigin, ModifierSet, PhysicalCode};
        let mut event = normalize_event(input)?;
        if matches!(event.origin, EventOrigin::Injected { marker } if marker == own_marker && marker != 0)
        {
            return Some(event);
        }
        let flags = unsafe { CGEventGetFlags(input.raw_event()) };
        let mut complete = modifiers_from_flags(flags);
        for group in MODIFIER_KEYS.chunks_exact(2) {
            let aggregate = group[0].0;
            if flags & aggregate != 0 && group.iter().all(|(_, side, _, _)| flags & side == 0) {
                // Hardware snapshots handle keys held before this tap started.
                // Foreign synthetic events with no side information are passed
                // through rather than being silently treated as left-handed.
                if !matches!(event.origin, EventOrigin::Hardware) {
                    return None;
                }
                let mut resolved = false;
                for (_, _, code, modifier) in group {
                    if key_state(*code) {
                        complete.insert(*modifier);
                        resolved = true;
                    }
                }
                if !resolved {
                    return None;
                }
            }
        }
        *state = complete;
        event.modifiers = if event.kind == EventKind::FlagsChanged {
            if let PhysicalCode::Key(code) = event.code {
                MODIFIER_KEYS
                    .iter()
                    .find(|(_, _, key, _)| *key == code)
                    .map(|(_, _, _, modifier)| {
                        if complete.contains(*modifier) {
                            *modifier
                        } else {
                            ModifierSet::NONE
                        }
                    })
                    .unwrap_or(complete)
            } else {
                complete
            }
        } else {
            complete
        };
        Some(event)
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
        unsafe { inject_outputs_with_context(outputs, marker, ptr::null_mut(), ptr::null_mut()) }
    }

    /// # Safety
    /// `proxy` must be the live callback proxy or null.
    pub unsafe fn inject_outputs_at(
        outputs: &[OutputEvent],
        marker: u64,
        proxy: RawEventRef,
    ) -> Result<(), InjectionError> {
        unsafe { inject_outputs_with_context(outputs, marker, proxy, ptr::null_mut()) }
    }

    /// # Safety
    /// Non-null `proxy` and `source_raw` must be borrowed from the current
    /// callback. The source is copied when needed and never retained.
    pub unsafe fn inject_outputs_with_context(
        outputs: &[OutputEvent],
        marker: u64,
        proxy: RawEventRef,
        source_raw: RawEventRef,
    ) -> Result<(), InjectionError> {
        if !preflight_permissions().1 {
            return Err(InjectionError::PermissionDenied);
        }
        // Construct the whole batch before posting any event. RAII drops every
        // previously created object if a later output cannot be constructed.
        let batch = unsafe { build_output_batch(outputs, marker, source_raw) }?;
        for event in batch {
            unsafe {
                post_event(event.0, proxy);
            }
        }
        Ok(())
    }

    struct OwnedEvent(EventRef);
    impl OwnedEvent {
        fn new(raw: EventRef) -> Result<Self, InjectionError> {
            if raw.is_null() {
                Err(InjectionError::Unavailable)
            } else {
                Ok(Self(raw))
            }
        }
        fn mark(&self, marker: u64) {
            unsafe {
                CGEventSetIntegerValueField(self.0, EVENT_SOURCE_USER_DATA_FIELD, marker as i64);
            }
        }
    }
    impl Drop for OwnedEvent {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.0);
            }
        }
    }

    unsafe fn build_output_batch(
        outputs: &[OutputEvent],
        marker: u64,
        source_raw: RawEventRef,
    ) -> Result<Vec<OwnedEvent>, InjectionError> {
        let mut batch = Vec::new();
        for output in outputs {
            match output {
                OutputEvent::Key {
                    code: PhysicalCode::Key(code),
                    phase,
                    modifiers,
                } => {
                    batch.push(create_keyboard_event(*code, *phase, *modifiers, marker)?);
                }
                OutputEvent::MouseButton { button, phase } => {
                    if !matches!(phase, EventPhase::Down | EventPhase::Up) {
                        return Err(InjectionError::InvalidEvent);
                    }
                    batch.push(unsafe {
                        create_mouse_event(*button, *phase, false, source_raw, marker)
                    }?);
                }
                OutputEvent::MouseDrag { button } => {
                    batch.push(unsafe {
                        create_mouse_event(*button, EventPhase::Motion, true, source_raw, marker)
                    }?);
                }
                OutputEvent::Scroll { delta_x, delta_y } => {
                    if !delta_x.is_finite() || !delta_y.is_finite() {
                        return Err(InjectionError::InvalidEvent);
                    }
                    let event = OwnedEvent::new(unsafe {
                        CGEventCreateScrollWheelEvent2(ptr::null_mut(), 0, 2, 0, 0, 0)
                    })?;
                    for (line, fixed, pixel, value) in
                        [(11, 93, 96, *delta_y), (12, 94, 97, *delta_x)]
                    {
                        unsafe {
                            CGEventSetIntegerValueField(event.0, line, value.round() as i64);
                            CGEventSetDoubleValueField(event.0, fixed, value);
                            CGEventSetIntegerValueField(event.0, pixel, value.round() as i64);
                        }
                    }
                    event.mark(marker);
                    batch.push(event);
                }
                OutputEvent::ScrollTransform { invert_x, invert_y } => {
                    if source_raw.is_null() || unsafe { CGEventGetType(source_raw) } != 22 {
                        return Err(InjectionError::InvalidEvent);
                    }
                    let event = OwnedEvent::new(unsafe { CGEventCreateCopy(source_raw) })?;
                    for (invert, line, fixed, pixel, accelerated, raw) in [
                        (*invert_y, 11, 93, 96, 176, 178),
                        (*invert_x, 12, 94, 97, 175, 177),
                    ] {
                        if invert {
                            for field in [line, pixel] {
                                let value =
                                    unsafe { CGEventGetIntegerValueField(source_raw, field) }
                                        .checked_neg()
                                        .ok_or(InjectionError::InvalidEvent)?;
                                unsafe {
                                    CGEventSetIntegerValueField(event.0, field, value);
                                }
                            }
                            for field in [fixed, accelerated, raw] {
                                let value =
                                    unsafe { CGEventGetDoubleValueField(source_raw, field) };
                                unsafe {
                                    CGEventSetDoubleValueField(event.0, field, -value);
                                }
                            }
                        }
                    }
                    event.mark(marker);
                    batch.push(event);
                }
                OutputEvent::SystemShortcut(shortcut) => {
                    for (code, phase, modifiers) in shortcut_sequence(*shortcut)? {
                        batch.push(create_keyboard_event(code, phase, modifiers, marker)?);
                    }
                }
                _ => return Err(InjectionError::InvalidEvent),
            }
        }
        Ok(batch)
    }

    fn create_keyboard_event(
        code: u16,
        phase: EventPhase,
        modifiers: event_model::ModifierSet,
        marker: u64,
    ) -> Result<OwnedEvent, InjectionError> {
        if !matches!(
            phase,
            EventPhase::Down | EventPhase::Up | EventPhase::Repeat
        ) {
            return Err(InjectionError::InvalidEvent);
        }
        let event = OwnedEvent::new(unsafe {
            CGEventCreateKeyboardEvent(ptr::null_mut(), code, phase != EventPhase::Up)
        })?;
        unsafe {
            CGEventSetFlags(event.0, flags_for_modifiers(modifiers));
            if phase == EventPhase::Repeat {
                CGEventSetIntegerValueField(event.0, 8, 1);
            }
        }
        event.mark(marker);
        Ok(event)
    }

    unsafe fn create_mouse_event(
        button: event_model::MouseButton,
        phase: EventPhase,
        drag: bool,
        source: EventRef,
        marker: u64,
    ) -> Result<OwnedEvent, InjectionError> {
        use event_model::MouseButton;
        let number = match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Other(value) => u32::from(value),
        };
        if number >= 32 {
            return Err(InjectionError::InvalidEvent);
        }
        let event_type = match (number, drag, phase) {
            (0, true, _) => 6,
            (1, true, _) => 7,
            (_, true, _) => 27,
            (0, false, EventPhase::Down) => 1,
            (0, false, _) => 2,
            (1, false, EventPhase::Down) => 3,
            (1, false, _) => 4,
            (_, false, EventPhase::Down) => 25,
            (_, false, _) => 26,
        };
        let source_is_mouse =
            !source.is_null() && matches!(unsafe { CGEventGetType(source) }, 1..=7 | 25..=27);
        if drag && !source_is_mouse {
            return Err(InjectionError::InvalidEvent);
        }
        let event = if source_is_mouse {
            let copy = OwnedEvent::new(unsafe { CGEventCreateCopy(source) })?;
            unsafe {
                CGEventSetType(copy.0, event_type);
            }
            copy
        } else {
            let cursor = OwnedEvent::new(unsafe { CGEventCreate(ptr::null_mut()) })?;
            let position = unsafe { CGEventGetLocation(cursor.0) };
            OwnedEvent::new(unsafe {
                CGEventCreateMouseEvent(ptr::null_mut(), event_type, position, number)
            })?
        };
        unsafe {
            CGEventSetIntegerValueField(event.0, 3, i64::from(number));
        }
        event.mark(marker);
        Ok(event)
    }

    fn shortcut_sequence(
        shortcut: event_model::SystemShortcut,
    ) -> Result<Vec<(u16, EventPhase, event_model::ModifierSet)>, InjectionError> {
        use event_model::{ModifierSet as M, SystemShortcut as S};
        use EventPhase::{Down, Up};
        let cmd = M::LEFT_COMMAND;
        let ctrl = M::LEFT_CONTROL;
        let shift = M::LEFT_SHIFT;
        Ok(match shortcut {
            S::SwitchApplication => vec![
                (55, Down, cmd),
                (48, Down, cmd),
                (48, Up, cmd),
                (55, Up, M::NONE),
            ],
            S::HardRefresh => vec![
                (55, Down, cmd),
                (56, Down, cmd | shift),
                (15, Down, cmd | shift),
                (15, Up, cmd | shift),
                (56, Up, cmd),
                (55, Up, M::NONE),
            ],
            S::LockScreen => vec![
                (59, Down, ctrl),
                (55, Down, ctrl | cmd),
                (12, Down, ctrl | cmd),
                (12, Up, ctrl | cmd),
                (55, Up, ctrl),
                (59, Up, M::NONE),
            ],
            S::CloseWindow => vec![
                (55, Down, cmd),
                (13, Down, cmd),
                (13, Up, cmd),
                (55, Up, M::NONE),
            ],
            S::ShowDesktop => vec![(103, Down, M::NONE), (103, Up, M::NONE)],
            S::SwitchInputSource | S::TogglePinyinEnglish | S::ShowClipboard => {
                return Err(InjectionError::InvalidEvent)
            }
        })
    }

    unsafe fn post_event(event: EventRef, proxy: RawEventRef) {
        if proxy.is_null() {
            unsafe {
                CGEventPost(EVENT_TAP_LOCATION, event);
            }
        } else {
            unsafe {
                CGEventTapPostEvent(proxy, event);
            }
        }
    }

    fn flags_for_modifiers(modifiers: event_model::ModifierSet) -> u64 {
        use event_model::ModifierSet as M;
        let mut flags = 0;
        for (semantic, aggregate, side) in [
            (M::LEFT_SHIFT, 0x0002_0000, 0x02),
            (M::RIGHT_SHIFT, 0x0002_0000, 0x04),
            (M::LEFT_CONTROL, 0x0004_0000, 0x01),
            (M::RIGHT_CONTROL, 0x0004_0000, 0x2000),
            (M::LEFT_OPTION | M::LEFT_ALT, 0x0008_0000, 0x20),
            (M::RIGHT_OPTION | M::RIGHT_ALT, 0x0008_0000, 0x40),
            (M::LEFT_COMMAND | M::LEFT_WINDOWS, 0x0010_0000, 0x08),
            (M::RIGHT_COMMAND | M::RIGHT_WINDOWS, 0x0010_0000, 0x10),
        ] {
            if modifiers.bits() & semantic.bits() != 0 {
                flags |= aggregate | side;
            }
        }
        flags
    }

    #[cfg(test)]
    mod modifier_tests {
        use super::*;
        use event_model::{EventKind, ModifierSet as M, MouseButton, PhysicalCode, SystemShortcut};

        fn keyboard_raw(code: u16, flags: u64) -> OwnedEvent {
            let raw = OwnedEvent::new(unsafe {
                CGEventCreateKeyboardEvent(ptr::null_mut(), code, false)
            })
            .unwrap();
            unsafe {
                CGEventSetFlags(raw.0, flags);
            }
            raw
        }

        #[test]
        fn every_physical_modifier_side_is_preserved() {
            for (aggregate, side, code, modifier) in MODIFIER_KEYS {
                assert_eq!(modifiers_from_flags(aggregate | side), modifier);
                assert_eq!(modifier_for_key_code(code, aggregate | side), modifier);
                let raw = keyboard_raw(8, aggregate | side);
                assert_eq!(
                    normalize_event(EventTapEvent::new(10, raw.0))
                        .unwrap()
                        .modifiers,
                    modifier
                );
            }
        }

        #[test]
        fn releasing_either_side_while_counterpart_is_held_is_an_up() {
            for pair in MODIFIER_KEYS.chunks_exact(2) {
                for order in [[0, 1], [1, 0]] {
                    let (aggregate, first_bit, first_code, first_modifier) = pair[order[0]];
                    let (_, second_bit, second_code, second_modifier) = pair[order[1]];
                    let mut state = M::NONE;
                    for (code, flags, expected) in [
                        (first_code, aggregate | first_bit, first_modifier),
                        (
                            second_code,
                            aggregate | first_bit | second_bit,
                            second_modifier,
                        ),
                        (first_code, aggregate | second_bit, M::NONE),
                        (second_code, 0, M::NONE),
                    ] {
                        let raw = keyboard_raw(code, flags);
                        let event = normalize_with_state(
                            EventTapEvent::new(12, raw.0),
                            99,
                            &mut state,
                            |_| panic!("side flags should be authoritative"),
                        )
                        .unwrap();
                        assert_eq!(event.modifiers, expected, "code {code}, flags {flags:x}");
                    }
                    assert_eq!(state, M::NONE);
                }
            }
        }

        #[test]
        fn releasing_control_while_shift_held_does_not_repress_control() {
            let raw = keyboard_raw(59, 0x0002_0002);
            let event = normalize_event(EventTapEvent::new(12, raw.0)).unwrap();
            assert_eq!(event.modifiers, M::NONE);
        }

        #[test]
        fn missing_side_flags_use_hardware_state_instead_of_guessing_left() {
            let raw = keyboard_raw(8, 0x0004_0000);
            let mut state = M::NONE;
            let event =
                normalize_with_state(EventTapEvent::new(10, raw.0), 99, &mut state, |code| {
                    code == 62
                })
                .unwrap();
            assert_eq!(event.modifiers, M::RIGHT_CONTROL);
            assert_eq!(state, M::RIGHT_CONTROL);
        }

        #[test]
        fn own_injected_events_cannot_pollute_physical_modifier_state() {
            let raw = keyboard_raw(58, 0x0008_0020);
            raw.mark(99);
            let mut state = M::RIGHT_CONTROL;
            normalize_with_state(EventTapEvent::new(12, raw.0), 99, &mut state, |_| {
                panic!("own markers must skip hardware state")
            })
            .unwrap();
            assert_eq!(state, M::RIGHT_CONTROL);
        }

        #[test]
        fn ambiguous_foreign_synthetic_modifiers_are_passed_through() {
            let raw = keyboard_raw(8, 0x0004_0000);
            raw.mark(100);
            let mut state = M::LEFT_OPTION;
            assert!(normalize_with_state(
                EventTapEvent::new(10, raw.0),
                99,
                &mut state,
                |_| panic!("foreign events must not query hardware")
            )
            .is_none());
            assert_eq!(state, M::LEFT_OPTION);
        }

        #[test]
        fn native_option_and_alt_flags_encode_to_option_with_correct_sides() {
            assert_eq!(flags_for_modifiers(M::LEFT_OPTION), 0x0008_0020);
            assert_eq!(flags_for_modifiers(M::RIGHT_ALT), 0x0008_0040);
            assert_eq!(
                flags_for_modifiers(M::LEFT_WINDOWS | M::RIGHT_COMMAND),
                0x0010_0018
            );
        }

        fn source_mouse(event_type: u32) -> OwnedEvent {
            let event = OwnedEvent::new(unsafe {
                CGEventCreateMouseEvent(
                    ptr::null_mut(),
                    event_type,
                    CGPoint {
                        x: 123.25,
                        y: 456.5,
                    },
                    0,
                )
            })
            .unwrap();
            unsafe {
                CGEventSetIntegerValueField(event.0, 1, 2); // double-click
                CGEventSetIntegerValueField(event.0, 4, 7); // movement delta
                CGEventSetDoubleValueField(event.0, 2, 0.75); // pressure
            }
            event
        }

        #[test]
        fn remapped_mouse_buttons_and_drag_preserve_source_position_and_metadata() {
            let source = source_mouse(1);
            let outputs = [
                OutputEvent::MouseButton {
                    button: MouseButton::Right,
                    phase: EventPhase::Down,
                },
                OutputEvent::MouseButton {
                    button: MouseButton::Right,
                    phase: EventPhase::Up,
                },
                OutputEvent::MouseDrag {
                    button: MouseButton::Other(4),
                },
            ];
            let batch = unsafe { build_output_batch(&outputs, 77, source.0) }.unwrap();
            for (event, expected_type, button) in
                [(&batch[0], 3, 1), (&batch[1], 4, 1), (&batch[2], 27, 4)]
            {
                unsafe {
                    assert_eq!(CGEventGetType(event.0), expected_type);
                    assert_eq!(CGEventGetLocation(event.0), CGEventGetLocation(source.0));
                    assert_eq!(CGEventGetIntegerValueField(event.0, 3), button);
                    assert_eq!(CGEventGetIntegerValueField(event.0, 1), 2);
                    assert_eq!(
                        CGEventGetIntegerValueField(event.0, EVENT_SOURCE_USER_DATA_FIELD),
                        77
                    );
                }
            }
            assert_eq!(unsafe { CGEventGetType(source.0) }, 1);
            let normalized = normalize_event(EventTapEvent::new(27, batch[2].0)).unwrap();
            assert_eq!(normalized.kind, EventKind::MouseMove);
            assert_eq!(
                normalized.code,
                PhysicalCode::MouseButton(MouseButton::Other(4))
            );
            assert_eq!(
                normalized.location,
                Some(event_model::Point {
                    x: 123.25,
                    y: 456.5
                })
            );
        }

        #[test]
        fn scroll_transform_preserves_precision_and_phase_for_each_axis() {
            let source = OwnedEvent::new(unsafe {
                CGEventCreateScrollWheelEvent2(ptr::null_mut(), 0, 2, -2, 3, 0)
            })
            .unwrap();
            unsafe {
                for (field, value) in [
                    (11, -2),
                    (12, 3),
                    (96, -15),
                    (97, 11),
                    (88, 1),
                    (99, 2),
                    (123, 4),
                ] {
                    CGEventSetIntegerValueField(source.0, field, value);
                }
                CGEventSetDoubleValueField(source.0, 93, -0.625);
                CGEventSetDoubleValueField(source.0, 94, 0.125);
            }
            for (invert_x, invert_y) in [(true, false), (false, true), (true, true)] {
                let batch = unsafe {
                    build_output_batch(
                        &[OutputEvent::ScrollTransform { invert_x, invert_y }],
                        42,
                        source.0,
                    )
                }
                .unwrap();
                let raw = batch[0].0;
                unsafe {
                    for (field, original, invert) in [
                        (11, -2, invert_y),
                        (12, 3, invert_x),
                        (96, -15, invert_y),
                        (97, 11, invert_x),
                    ] {
                        assert_eq!(
                            CGEventGetIntegerValueField(raw, field),
                            if invert { -original } else { original }
                        );
                    }
                    assert_eq!(
                        CGEventGetDoubleValueField(raw, 93),
                        if invert_y { 0.625 } else { -0.625 }
                    );
                    assert_eq!(
                        CGEventGetDoubleValueField(raw, 94),
                        if invert_x { -0.125 } else { 0.125 }
                    );
                    for (field, value) in [(88, 1), (99, 2), (123, 4)] {
                        assert_eq!(CGEventGetIntegerValueField(raw, field), value);
                    }
                    assert_eq!(CGEventGetTimestamp(raw), CGEventGetTimestamp(source.0));
                }
            }
            assert_eq!(unsafe { CGEventGetIntegerValueField(source.0, 11) }, -2);
        }

        #[test]
        fn subline_horizontal_scroll_is_not_misclassified_as_vertical() {
            let source = OwnedEvent::new(unsafe {
                CGEventCreateScrollWheelEvent2(ptr::null_mut(), 0, 2, 0, 0, 0)
            })
            .unwrap();
            unsafe {
                CGEventSetDoubleValueField(source.0, 94, 0.125);
            }
            let event = normalize_event(EventTapEvent::new(22, source.0)).unwrap();
            assert_eq!(
                event.code,
                PhysicalCode::Scroll(event_model::ScrollAxis::Horizontal)
            );
            assert_eq!(event.scroll.unwrap().x, 0.125);
        }

        #[test]
        fn system_shortcuts_build_complete_marked_batches_without_posting() {
            for shortcut in [
                SystemShortcut::LockScreen,
                SystemShortcut::CloseWindow,
                SystemShortcut::ShowDesktop,
                SystemShortcut::HardRefresh,
                SystemShortcut::SwitchApplication,
            ] {
                let batch = unsafe {
                    build_output_batch(
                        &[OutputEvent::SystemShortcut(shortcut)],
                        88,
                        ptr::null_mut(),
                    )
                }
                .unwrap();
                assert!(!batch.is_empty());
                assert_eq!(unsafe { CGEventGetFlags(batch.last().unwrap().0) }, 0);
                for event in batch {
                    assert_eq!(
                        unsafe {
                            CGEventGetIntegerValueField(event.0, EVENT_SOURCE_USER_DATA_FIELD)
                        },
                        88
                    );
                }
            }
            assert_eq!(
                shortcut_sequence(SystemShortcut::LockScreen).unwrap()[2],
                (12, EventPhase::Down, M::LEFT_CONTROL | M::LEFT_COMMAND)
            );
            assert_eq!(
                shortcut_sequence(SystemShortcut::CloseWindow).unwrap()[1].0,
                13
            );
            assert_eq!(
                shortcut_sequence(SystemShortcut::ShowDesktop).unwrap()[0].0,
                103
            );
        }

        #[test]
        fn invalid_later_output_rejects_entire_unposted_batch() {
            let outputs = [
                OutputEvent::Key {
                    code: PhysicalCode::key(8),
                    phase: EventPhase::Down,
                    modifiers: M::LEFT_COMMAND,
                },
                OutputEvent::ScrollTransform {
                    invert_x: true,
                    invert_y: false,
                },
            ];
            assert!(matches!(
                unsafe { build_output_batch(&outputs, 1, ptr::null_mut()) },
                Err(InjectionError::InvalidEvent)
            ));
        }

        #[test]
        fn tap_disabled_notification_reaches_owner_before_recovery() {
            let seen = std::sync::Arc::new(AtomicBool::new(false));
            let observed = std::sync::Arc::clone(&seen);
            let mut state = CallbackState {
                callback: Box::new(move |event| {
                    observed.store(event.is_tap_disabled(), Ordering::Release);
                    EventTapAction::Pass
                }),
                tap: ptr::null_mut(),
            };
            unsafe {
                event_tap_callback(
                    ptr::null_mut(),
                    TAP_DISABLED_BY_TIMEOUT,
                    ptr::null_mut(),
                    (&mut state as *mut CallbackState).cast(),
                );
            }
            assert!(seen.load(Ordering::Acquire));
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

    #[derive(Debug)]
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

        pub fn run_loop_until(&self, _stop: &std::sync::atomic::AtomicBool) {}
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

    pub unsafe fn inject_outputs_with_context(
        _outputs: &[OutputEvent],
        _marker: u64,
        _proxy: super::RawEventRef,
        _source_raw: super::RawEventRef,
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
