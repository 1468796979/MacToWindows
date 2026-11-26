//! Nonblocking source hints for keyboard compatibility routing.
//!
//! CoreGraphics metadata is not a hardware identity or an authenticated sender.
//! In particular, a virtual HID can be indistinguishable from a physical device.

use super::{is_own_marker, KeyboardMetadata};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyboardSourceKind {
    OwnInjected,
    HardwareLike,
    Deskflow,
    Software,
    Unknown,
}

/// An in-memory producer key. Process starts isolate a reused PID after refresh.
/// A positive PID joins its Enigo/rdev channels; CoreGraphics cannot distinguish
/// several remote connections inside the same producer process. Markers group
/// streams only when no PID is available. These keys are not persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KeyboardSourceId {
    pub process_id: i32,
    pub process_start: u64,
    pub marker: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardSource {
    pub kind: KeyboardSourceKind,
    pub id: KeyboardSourceId,
}

/// Freeze a positive-PID key stream across nonblocking cache refreshes. Text
/// submissions must be filtered before this ledger; own replays never enter it.
#[derive(Default)]
pub struct KeyboardRoutingState {
    bindings: HashMap<(i32, u16), KeyboardSource>,
    generations: HashMap<i32, u64>,
}

impl KeyboardRoutingState {
    pub fn reset(&mut self) {
        self.bindings.clear();
        self.generations.clear();
    }

    pub fn forget_process(&mut self, process_id: i32) {
        self.bindings.retain(|(pid, _), _| *pid != process_id);
        self.generations.remove(&process_id);
    }

    /// Inspect the current producer epoch without registering a key. Callers
    /// can protect submitted text using this effective source before deciding
    /// whether the event is eligible for the Down/Up routing ledger.
    pub fn current_source(
        &self,
        metadata: &KeyboardMetadata,
        source: KeyboardSource,
    ) -> KeyboardSource {
        if metadata.process_id <= 0 || source.kind == KeyboardSourceKind::OwnInjected {
            return source;
        }
        if source.id.process_start != 0
            && self
                .generations
                .get(&metadata.process_id)
                .is_some_and(|previous| *previous != source.id.process_start)
        {
            return source;
        }
        self.bindings
            .iter()
            .find_map(|((pid, _), frozen)| (*pid == metadata.process_id).then_some(*frozen))
            .unwrap_or(source)
    }

    pub fn route(&mut self, metadata: &KeyboardMetadata, source: KeyboardSource) -> KeyboardSource {
        use event_model::EventPhase;
        if metadata.process_id <= 0 || source.kind == KeyboardSourceKind::OwnInjected {
            return source;
        }
        let pid = metadata.process_id;
        if source.id.process_start != 0 {
            if self
                .generations
                .insert(pid, source.id.process_start)
                .is_some_and(|previous| previous != source.id.process_start)
            {
                // A verified new process generation cannot complete an old
                // process's Down, even if the operating system reused its PID.
                self.bindings.retain(|(owner, _), _| *owner != pid);
            }
        }
        let epoch = self.current_source(metadata, source);
        let phase = if metadata.phase == EventPhase::Changed {
            let modifier = match metadata.keycode {
                56 => Some((event_model::ModifierSet::LEFT_SHIFT, 0x0002_0000, 0x06)),
                60 => Some((event_model::ModifierSet::RIGHT_SHIFT, 0x0002_0000, 0x06)),
                59 => Some((event_model::ModifierSet::LEFT_CONTROL, 0x0004_0000, 0x2001)),
                62 => Some((event_model::ModifierSet::RIGHT_CONTROL, 0x0004_0000, 0x2001)),
                58 => Some((event_model::ModifierSet::LEFT_OPTION, 0x0008_0000, 0x60)),
                61 => Some((event_model::ModifierSet::RIGHT_OPTION, 0x0008_0000, 0x60)),
                55 => Some((event_model::ModifierSet::LEFT_COMMAND, 0x0010_0000, 0x18)),
                54 => Some((event_model::ModifierSet::RIGHT_COMMAND, 0x0010_0000, 0x18)),
                _ => None,
            };
            match modifier {
                Some((modifier, _, _)) if metadata.modifiers.contains(modifier) => EventPhase::Down,
                Some((_, aggregate, sides))
                    if metadata.raw_flags & aggregate != 0
                        && metadata.raw_flags & sides == 0
                        && !self.bindings.contains_key(&(pid, metadata.keycode)) =>
                {
                    EventPhase::Down
                }
                Some(_) => EventPhase::Up,
                None => return epoch,
            }
        } else {
            metadata.phase
        };
        let key = (pid, metadata.keycode);
        match phase {
            EventPhase::Down => *self.bindings.entry(key).or_insert(epoch),
            EventPhase::Repeat => self.bindings.get(&key).copied().unwrap_or(epoch),
            EventPhase::Up => {
                let frozen = self.bindings.remove(&key).unwrap_or(epoch);
                if !self.bindings.keys().any(|(owner, _)| *owner == pid) {
                    self.generations.remove(&pid);
                }
                frozen
            }
            _ => epoch,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessIdentity {
    started: u64,
    deskflow: bool,
}

struct SourceSnapshot {
    refreshed: Instant,
    processes: HashMap<i32, ProcessIdentity>,
    listed_pids: Option<HashSet<i32>>,
}

impl Default for SourceSnapshot {
    fn default() -> Self {
        Self {
            refreshed: Instant::now(),
            processes: HashMap::new(),
            listed_pids: None,
        }
    }
}

/// Refreshes process information away from the event callback. Lookups use
/// `try_read` and never wait for the worker, access files, or inspect processes.
/// The periodic cache is a compatibility hint: identity changes can take one
/// refresh interval to appear, so a cached PID must not be treated as authority.
pub struct KeyboardSourceCache {
    snapshot: Arc<RwLock<SourceSnapshot>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl KeyboardSourceCache {
    pub fn start() -> Self {
        let snapshot = Arc::new(RwLock::new(process_snapshot()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_snapshot = Arc::clone(&snapshot);
        let worker_stopped = Arc::clone(&stopped);
        let worker = std::thread::Builder::new()
            .name("mac-to-windows-keyboard-sources".into())
            .spawn(move || {
                while !worker_stopped.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_secs(1));
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let next = process_snapshot();
                    if let Ok(mut current) = worker_snapshot.write() {
                        *current = next;
                    }
                }
            })
            .ok();
        Self {
            snapshot,
            stopped,
            worker,
        }
    }

    pub fn classify(&self, metadata: &KeyboardMetadata, own_marker: u64) -> KeyboardSource {
        let snapshot = self.snapshot.try_read().ok();
        // Never indefinitely trust a stale Deskflow PID if the refresh worker
        // failed. A changed generation is reflected in the returned stream key.
        let identity = snapshot.as_ref().and_then(|snapshot| {
            (snapshot.refreshed.elapsed() <= Duration::from_secs(3))
                .then(|| snapshot.processes.get(&metadata.process_id).copied())
                .flatten()
        });
        classify(metadata, own_marker, identity)
    }

    /// A missing classification is inconclusive. Confirm disappearance from a
    /// complete process listing, or a different generation of a known PID.
    pub fn process_has_ended(&self, id: KeyboardSourceId, last_input: Instant) -> bool {
        if id.process_id <= 0 {
            return false;
        }
        self.snapshot.try_read().is_ok_and(|snapshot| {
            snapshot.refreshed > last_input
                && snapshot.refreshed.elapsed() <= Duration::from_secs(3)
                && (snapshot
                    .listed_pids
                    .as_ref()
                    .is_some_and(|pids| !pids.contains(&id.process_id))
                    || (id.process_start != 0
                        && snapshot
                            .processes
                            .get(&id.process_id)
                            .is_some_and(|entry| entry.started != id.process_start)))
        })
    }
}

impl Drop for KeyboardSourceCache {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn classify(
    metadata: &KeyboardMetadata,
    own_marker: u64,
    identity: Option<ProcessIdentity>,
) -> KeyboardSource {
    let id = KeyboardSourceId {
        process_id: metadata.process_id,
        process_start: identity.map_or(0, |process| process.started),
        marker: if metadata.process_id > 0 {
            0
        } else {
            metadata.marker
        },
    };
    let kind = if is_own_marker(metadata.marker, own_marker) {
        KeyboardSourceKind::OwnInjected
    } else if identity.is_some_and(|process| process.deskflow) {
        KeyboardSourceKind::Deskflow
    } else if metadata.process_id > 0 && metadata.marker == 0 && identity.is_none() {
        // Preserve legacy routing while a newly started process, a busy
        // snapshot, or a failed refresh has not established its identity.
        KeyboardSourceKind::Unknown
    } else if metadata.process_id > 0 || metadata.marker != 0 {
        KeyboardSourceKind::Software
    } else {
        // A source-state value is an event state table, not a device identity.
        // Preserve legacy mappings for unmarked hardware-shaped events.
        KeyboardSourceKind::HardwareLike
    };
    KeyboardSource { kind, id }
}

fn is_deskflow_executable(path: &str) -> bool {
    path.ends_with("/Deskflow.app/Contents/MacOS/Deskflow")
        || path.ends_with("/Deskflow.app/Contents/MacOS/deskflow-core")
}

#[cfg(target_os = "macos")]
fn process_snapshot() -> SourceSnapshot {
    use std::ffi::c_void;
    use std::mem::size_of;

    #[repr(C)]
    struct BsdInfo {
        fields: [u32; 12],
        command: [u8; 16],
        name: [u8; 32],
        other: [u32; 6],
        started_seconds: u64,
        started_microseconds: u64,
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, size: i32) -> i32;
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32) -> i32;
    }

    let mut snapshot = SourceSnapshot::default();
    let needed = unsafe { proc_listpids(1, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return snapshot;
    }
    let mut pids = vec![0_i32; needed as usize / size_of::<i32>() + 256];
    let bytes = unsafe {
        proc_listpids(
            1,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    let count = (bytes.max(0) as usize / size_of::<i32>()).min(pids.len());
    if bytes > 0 && (bytes as usize) < pids.len() * size_of::<i32>() {
        snapshot.listed_pids = Some(
            pids[..count]
                .iter()
                .copied()
                .filter(|&pid| pid > 0)
                .collect(),
        );
    }
    for &pid in &pids[..count] {
        if pid <= 0 {
            continue;
        }
        let mut info: BsdInfo = unsafe { std::mem::zeroed() };
        if unsafe {
            proc_pidinfo(
                pid,
                3,
                0,
                (&mut info as *mut BsdInfo).cast(),
                size_of::<BsdInfo>() as i32,
            )
        } != size_of::<BsdInfo>() as i32
        {
            continue;
        }
        let mut path = [0_u8; 4096];
        let length = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if length <= 0 {
            // An unreadable executable must not be labelled "not Deskflow".
            continue;
        }
        let end = path
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(path.len());
        let Ok(path) = std::str::from_utf8(&path[..end]) else {
            continue;
        };
        let mut current: BsdInfo = unsafe { std::mem::zeroed() };
        if unsafe {
            proc_pidinfo(
                pid,
                3,
                0,
                (&mut current as *mut BsdInfo).cast(),
                size_of::<BsdInfo>() as i32,
            )
        } != size_of::<BsdInfo>() as i32
            || current.started_seconds != info.started_seconds
            || current.started_microseconds != info.started_microseconds
        {
            // Do not combine an old process generation with a reused PID's
            // executable path observed during this refresh.
            continue;
        }
        let deskflow = is_deskflow_executable(path);
        snapshot.processes.insert(
            pid,
            ProcessIdentity {
                started: info
                    .started_seconds
                    .saturating_mul(1_000_000)
                    .saturating_add(info.started_microseconds),
                deskflow,
            },
        );
    }
    snapshot
}

#[cfg(not(target_os = "macos"))]
fn process_snapshot() -> SourceSnapshot {
    SourceSnapshot::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(pid: i32, marker: u64, state: i64) -> KeyboardMetadata {
        KeyboardMetadata {
            marker,
            process_id: pid,
            source_state: state,
            keycode: 8,
            phase: event_model::EventPhase::Down,
            modifiers: event_model::ModifierSet::NONE,
            raw_flags: 0,
            unicode_len: 0,
            unicode_is_ascii_a: false,
        }
    }

    #[test]
    fn process_termination_requires_a_fresh_complete_list_or_verified_generation() {
        let now = Instant::now();
        let prior_input = now - Duration::from_millis(1);
        let cache = KeyboardSourceCache {
            snapshot: Arc::new(RwLock::new(SourceSnapshot {
                refreshed: now,
                ..SourceSnapshot::default()
            })),
            stopped: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        let id = KeyboardSourceId {
            process_id: 501,
            process_start: 12,
            marker: 0,
        };
        assert!(!cache.process_has_ended(id, prior_input));
        cache.snapshot.write().unwrap().listed_pids = Some(HashSet::from([501]));
        assert!(!cache.process_has_ended(id, prior_input));

        cache.snapshot.write().unwrap().processes.insert(
            501,
            ProcessIdentity {
                started: 13,
                deskflow: false,
            },
        );
        assert!(cache.process_has_ended(id, prior_input));
        assert!(!cache.process_has_ended(id, now));
        let unidentified = KeyboardSourceId {
            process_start: 0,
            ..id
        };
        assert!(!cache.process_has_ended(unidentified, prior_input));
        cache.snapshot.write().unwrap().listed_pids = Some(HashSet::from([502]));
        assert!(cache.process_has_ended(unidentified, prior_input));
        assert!(!cache.process_has_ended(
            KeyboardSourceId {
                process_id: 0,
                ..id
            },
            prior_input
        ));
        cache.snapshot.write().unwrap().refreshed = now - Duration::from_secs(4);
        assert!(!cache.process_has_ended(id, now - Duration::from_secs(5)));
    }

    #[test]
    fn source_hints_preserve_unknown_hid_but_except_confirmed_deskflow() {
        let deskflow = Some(ProcessIdentity {
            started: 123,
            deskflow: true,
        });
        assert_eq!(
            classify(&metadata(42, 99, 0), 99, deskflow).kind,
            KeyboardSourceKind::OwnInjected
        );
        assert_eq!(
            classify(&metadata(42, 100, 0), 99, deskflow).kind,
            KeyboardSourceKind::Deskflow
        );
        assert_eq!(
            classify(&metadata(42, 0, 1), 99, None).kind,
            KeyboardSourceKind::Unknown
        );
        assert_eq!(
            classify(&metadata(0, 100, 1), 99, None).kind,
            KeyboardSourceKind::Software
        );
        assert_eq!(
            classify(&metadata(42, 100, 0), 99, None).kind,
            KeyboardSourceKind::Software
        );
        for state in [-1, 0, 1, 123] {
            assert_eq!(
                classify(&metadata(0, 0, state), 99, None).kind,
                KeyboardSourceKind::HardwareLike
            );
        }
    }

    #[test]
    fn restarted_process_uses_a_new_in_memory_source_id() {
        let first = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: true,
            }),
        );
        let reused = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 2,
                deskflow: false,
            }),
        );
        assert_ne!(first.id, reused.id);
        assert_eq!(reused.kind, KeyboardSourceKind::Software);
    }

    #[test]
    fn one_process_joins_its_enigo_and_rdev_channels() {
        let process = Some(ProcessIdentity {
            started: 123,
            deskflow: false,
        });
        assert_eq!(
            classify(&metadata(42, 100, 0), 99, process).id,
            classify(&metadata(42, 0, 0), 99, process).id
        );
        assert_ne!(
            classify(&metadata(0, 100, 0), 99, None).id,
            classify(&metadata(0, 101, 0), 99, None).id
        );
    }

    #[test]
    fn deskflow_identity_covers_gui_and_core_but_not_lookalike_names() {
        for path in [
            "/Applications/Deskflow.app/Contents/MacOS/Deskflow",
            "/Users/test/Apps/Deskflow.app/Contents/MacOS/deskflow-core",
        ] {
            assert!(is_deskflow_executable(path));
        }
        for path in [
            "/tmp/deskflow-core",
            "/Applications/FakeDeskflow.app/Contents/MacOS/Deskflow",
            "/Applications/Deskflow.app/Contents/MacOS/Deskflow-helper",
        ] {
            assert!(!is_deskflow_executable(path));
        }
    }

    #[test]
    fn a_busy_snapshot_never_blocks_classification() {
        let cache = KeyboardSourceCache {
            snapshot: Arc::new(RwLock::new(SourceSnapshot::default())),
            stopped: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        let _guard = cache.snapshot.write().unwrap();
        assert_eq!(
            cache.classify(&metadata(42, 0, 0), 99).kind,
            KeyboardSourceKind::Unknown
        );
    }

    #[test]
    fn cache_refresh_does_not_move_an_existing_down_to_another_route() {
        use event_model::EventPhase;
        let mut routing = KeyboardRoutingState::default();
        let unknown = classify(&metadata(42, 0, 0), 99, None);
        let software = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: false,
            }),
        );
        let deskflow = KeyboardSource {
            kind: KeyboardSourceKind::Deskflow,
            ..software
        };
        let mut event = metadata(42, 0, 0);
        assert_eq!(routing.route(&event, unknown), unknown);
        event.phase = EventPhase::Repeat;
        assert_eq!(routing.route(&event, software), unknown);
        event.phase = EventPhase::Up;
        assert_eq!(routing.route(&event, deskflow), unknown);
        event.phase = EventPhase::Down;
        assert_eq!(routing.route(&event, deskflow), deskflow);
    }

    #[test]
    fn producer_epoch_keeps_a_chord_together_until_every_key_is_up() {
        use event_model::{EventPhase, ModifierSet};
        let mut routing = KeyboardRoutingState::default();
        let unknown = classify(&metadata(42, 0, 0), 99, None);
        let software = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: false,
            }),
        );
        let mut event = metadata(42, 0, 0);
        event.keycode = 59;
        event.phase = EventPhase::Changed;
        event.modifiers = ModifierSet::LEFT_CONTROL;
        assert_eq!(routing.route(&event, unknown), unknown);
        event.keycode = 8;
        event.phase = EventPhase::Down;
        assert_eq!(routing.current_source(&event, software), unknown);
        assert_eq!(routing.route(&event, software), unknown);
        event.phase = EventPhase::Up;
        assert_eq!(routing.route(&event, software), unknown);
        event.keycode = 59;
        event.phase = EventPhase::Changed;
        event.modifiers = ModifierSet::NONE;
        assert_eq!(routing.route(&event, software), unknown);
        event.keycode = 8;
        event.phase = EventPhase::Down;
        assert_eq!(routing.route(&event, software), software);
    }

    #[test]
    fn text_source_inspection_does_not_acquire_another_down() {
        let mut routing = KeyboardRoutingState::default();
        let software = classify(
            &metadata(42, 100, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: false,
            }),
        );
        let deskflow = KeyboardSource {
            kind: KeyboardSourceKind::Deskflow,
            ..software
        };
        let event = metadata(42, 100, 0);
        routing.route(&event, software);
        let mut text = event;
        text.keycode = 0;
        text.unicode_len = 1;
        assert_eq!(routing.current_source(&text, deskflow), software);
        assert_eq!(routing.bindings.len(), 1);
        assert!(!routing.bindings.contains_key(&(42, 0)));
    }

    #[test]
    fn routing_ignores_own_replays_and_clears_a_reused_pid_generation() {
        use event_model::EventPhase;
        let mut routing = KeyboardRoutingState::default();
        let first = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: false,
            }),
        );
        let second = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 2,
                deskflow: true,
            }),
        );
        let mut event = metadata(42, 0, 0);
        routing.route(&event, first);
        let own = KeyboardSource {
            kind: KeyboardSourceKind::OwnInjected,
            ..second
        };
        assert_eq!(routing.route(&event, own), own);
        assert_eq!(routing.bindings.len(), 1);
        event.keycode = 9;
        routing.route(&event, second);
        event.keycode = 8;
        event.phase = EventPhase::Up;
        assert_eq!(routing.route(&event, second), second);
        assert!(routing.bindings.keys().all(|(_, code)| *code != 8));
    }

    #[test]
    fn routing_freezes_modifier_flags_changed_until_its_up() {
        use event_model::{EventPhase, ModifierSet};
        let mut routing = KeyboardRoutingState::default();
        let first = classify(&metadata(42, 0, 0), 99, None);
        let second = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: false,
            }),
        );
        let mut event = metadata(42, 0, 0);
        event.keycode = 58;
        event.phase = EventPhase::Changed;
        event.modifiers = ModifierSet::LEFT_OPTION;
        assert_eq!(routing.route(&event, first), first);
        event.modifiers = ModifierSet::NONE;
        assert_eq!(routing.route(&event, second), first);
        assert!(routing.bindings.is_empty());
        routing.route(&metadata(42, 0, 0), first);
        routing.reset();
        assert!(routing.bindings.is_empty());
        assert!(routing.generations.is_empty());
    }

    #[test]
    fn aggregate_only_modifier_flags_freeze_down_and_up_during_refresh() {
        use event_model::{EventPhase, ModifierSet};
        let mut routing = KeyboardRoutingState::default();
        let first = classify(&metadata(42, 0, 0), 99, None);
        let second = classify(
            &metadata(42, 0, 0),
            99,
            Some(ProcessIdentity {
                started: 1,
                deskflow: false,
            }),
        );
        let mut event = metadata(42, 0, 0);
        event.keycode = 59;
        event.phase = EventPhase::Changed;
        event.raw_flags = 0x0004_0000;
        assert_eq!(routing.route(&event, first), first);
        event.raw_flags = 0;
        assert_eq!(routing.route(&event, second), first);
        assert!(routing.bindings.is_empty());
        event.raw_flags = 0x0004_0000;
        routing.route(&event, first);
        // Left Control Up while Right Control remains down must finish Left.
        event.modifiers = ModifierSet::RIGHT_CONTROL;
        event.raw_flags = 0x0004_2000;
        assert_eq!(routing.route(&event, second), first);
        assert!(routing.bindings.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_process_snapshot_reads_current_process_generation() {
        let snapshot = process_snapshot().processes;
        let current = snapshot.get(&(std::process::id() as i32)).unwrap();
        assert!(current.started > 0);
        assert!(!current.deskflow);
    }
}
