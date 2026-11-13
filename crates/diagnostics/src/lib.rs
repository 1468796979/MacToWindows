use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PermissionState {
    Unknown = 0,
    Denied = 1,
    Granted = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TapState {
    Stopped = 0,
    Running = 1,
    Disabled = 2,
    Restarting = 3,
    Degraded = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub events_seen: u64,
    pub events_passed: u64,
    pub events_suppressed: u64,
    pub events_injected: u64,
    pub injection_failures: u64,
    pub tap_restarts: u64,
    pub permission: PermissionState,
    pub tap_state: TapState,
}

pub struct Diagnostics {
    events_seen: AtomicU64,
    events_passed: AtomicU64,
    events_suppressed: AtomicU64,
    events_injected: AtomicU64,
    injection_failures: AtomicU64,
    tap_restarts: AtomicU64,
    permission: AtomicU8,
    tap_state: AtomicU8,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}
impl Diagnostics {
    pub const fn new() -> Self {
        Self {
            events_seen: AtomicU64::new(0),
            events_passed: AtomicU64::new(0),
            events_suppressed: AtomicU64::new(0),
            events_injected: AtomicU64::new(0),
            injection_failures: AtomicU64::new(0),
            tap_restarts: AtomicU64::new(0),
            permission: AtomicU8::new(PermissionState::Unknown as u8),
            tap_state: AtomicU8::new(TapState::Stopped as u8),
        }
    }
    fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn increment_events_seen(&self) {
        self.record_seen();
    }
    pub fn increment_events_passed(&self) {
        self.record_passed();
    }
    pub fn increment_events_suppressed(&self) {
        self.record_suppressed();
    }
    pub fn increment_events_injected(&self) {
        self.record_injected();
    }
    pub fn increment_injection_failures(&self) {
        self.record_injection_failure();
    }
    pub fn record_seen(&self) {
        Self::inc(&self.events_seen);
    }
    pub fn record_passed(&self) {
        Self::inc(&self.events_passed);
    }
    pub fn record_suppressed(&self) {
        Self::inc(&self.events_suppressed);
    }
    pub fn record_injected(&self) {
        Self::inc(&self.events_injected);
    }
    pub fn record_injection_failure(&self) {
        Self::inc(&self.injection_failures);
    }
    pub fn record_tap_restart(&self) {
        Self::inc(&self.tap_restarts);
    }
    pub fn set_permission(&self, state: PermissionState) {
        self.permission.store(state as u8, Ordering::Release);
    }
    pub fn set_tap_state(&self, state: TapState) {
        self.tap_state.store(state as u8, Ordering::Release);
    }
    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            events_seen: self.events_seen.load(Ordering::Relaxed),
            events_passed: self.events_passed.load(Ordering::Relaxed),
            events_suppressed: self.events_suppressed.load(Ordering::Relaxed),
            events_injected: self.events_injected.load(Ordering::Relaxed),
            injection_failures: self.injection_failures.load(Ordering::Relaxed),
            tap_restarts: self.tap_restarts.load(Ordering::Relaxed),
            permission: match self.permission.load(Ordering::Acquire) {
                1 => PermissionState::Denied,
                2 => PermissionState::Granted,
                _ => PermissionState::Unknown,
            },
            tap_state: match self.tap_state.load(Ordering::Acquire) {
                1 => TapState::Running,
                2 => TapState::Disabled,
                3 => TapState::Restarting,
                4 => TapState::Degraded,
                _ => TapState::Stopped,
            },
        }
    }
}

#[cfg(test)]
mod lib_test;
