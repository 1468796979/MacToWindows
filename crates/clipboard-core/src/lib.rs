use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_ENTRIES: usize = 50;
pub const MAX_CHARS: usize = 1_048_576;
const SCHEMA_VERSION: u32 = 1;
const RECOVERY_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEntry {
    pub id: u64,
    pub text: String,
    pub copied_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardHistory {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    pub next_id: u64,
    pub entries: Vec<ClipboardEntry>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for ClipboardHistory {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    IgnoredEmpty,
    IgnoredTooLarge,
    Added(u64),
    Moved(u64),
}

impl ClipboardHistory {
    pub fn entries(&self) -> &[ClipboardEntry] {
        &self.entries
    }

    pub fn ingest(&mut self, text: String, now_ms: u64) -> IngestOutcome {
        if text.is_empty() {
            return IngestOutcome::IgnoredEmpty;
        }
        if text.chars().count() > MAX_CHARS {
            return IngestOutcome::IgnoredTooLarge;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.text == text) {
            let mut entry = self.entries.remove(index);
            entry.copied_at_unix_ms = now_ms;
            let id = entry.id;
            self.entries.insert(0, entry);
            return IngestOutcome::Moved(id);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.insert(
            0,
            ClipboardEntry {
                id,
                text,
                copied_at_unix_ms: now_ms,
            },
        );
        self.entries.truncate(MAX_ENTRIES);
        IngestOutcome::Added(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return false;
        };
        self.entries.remove(index);
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn sanitize(mut self) -> Self {
        let mut ids = HashSet::new();
        let mut texts = HashSet::new();
        self.entries.retain(|entry| {
            entry.id > 0
                && !entry.text.is_empty()
                && entry.text.chars().count() <= MAX_CHARS
                && ids.insert(entry.id)
                && texts.insert(entry.text.clone())
        });
        self.entries.truncate(MAX_ENTRIES);
        let max_id = self.entries.iter().map(|entry| entry.id).max().unwrap_or(0);
        if self.next_id == 0 || self.next_id <= max_id {
            self.next_id = max_id.saturating_add(1);
        }
        self
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Json(serde_json::Error),
    UnsupportedSchema(u32),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::UnsupportedSchema(version) => write!(f, "unsupported schema version {version}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug)]
pub enum LoadOutcome {
    Loaded(ClipboardHistory),
    RecoveredCorrupt {
        history: ClipboardHistory,
        path: PathBuf,
    },
    RecoveredBackup(ClipboardHistory),
    RecoveredRemoval {
        history: ClipboardHistory,
        pending: Option<io::Error>,
    },
}

impl LoadOutcome {
    pub fn history(self) -> ClipboardHistory {
        match self {
            Self::Loaded(history)
            | Self::RecoveredBackup(history)
            | Self::RecoveredRemoval { history, .. }
            | Self::RecoveredCorrupt { history, .. } => history,
        }
    }
}

#[derive(Debug)]
pub enum RemovalOutcome {
    Saved,
    Pending(io::Error),
}

// Flatten the history so a manually restored backup remains a normal history file.
#[derive(Serialize, Deserialize)]
struct RecoveryBackup<T> {
    recovery_version: u32,
    #[serde(flatten)]
    history: T,
}

pub struct ClipboardStore;

impl ClipboardStore {
    pub fn manual_recovery_dir(path: &Path) -> PathBuf {
        path.with_extension("manual-recovery")
    }

    pub fn load_or_recover(path: impl AsRef<Path>) -> Result<LoadOutcome, StoreError> {
        let path = path.as_ref();
        if let Some((history, data)) = read_removal(path)? {
            let pending = finish_removal(path, &history, &data, &mut sync_directory).err();
            return Ok(LoadOutcome::RecoveredRemoval { history, pending });
        }
        let corrupt = match fs::read(path) {
            Ok(contents) => match decode_history(&contents) {
                Ok(history) => return Ok(LoadOutcome::Loaded(history)),
                Err(StoreError::Json(_)) => {
                    Some(path.with_extension(format!("corrupt.{}", suffix())))
                }
                Err(error) => return Err(error),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(StoreError::Io(error)),
        };

        let backup_path = path.with_extension("bak");
        let backup = match fs::read(&backup_path) {
            Ok(contents) => {
                let backup =
                    serde_json::from_slice::<RecoveryBackup<ClipboardHistory>>(&contents).ok();
                match backup {
                    Some(backup)
                        if backup.recovery_version == RECOVERY_VERSION
                            && backup.history.schema_version == SCHEMA_VERSION =>
                    {
                        Some(backup.history.sanitize())
                    }
                    _ => {
                        // A legacy backup might contain the history immediately before a clear.
                        // Preserve its bytes for manual recovery and keep it out of rotation.
                        let directory = Self::manual_recovery_dir(path);
                        fs::create_dir_all(&directory)?;
                        let saved = directory.join(format!("history.{}.bak", suffix()));
                        fs::rename(&backup_path, &saved)?;
                        sync_directory(&saved)?;
                        sync_directory(&backup_path)?;
                        None
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(StoreError::Io(error)),
        };
        if let Some(corrupt_path) = &corrupt {
            fs::rename(path, corrupt_path)?;
            sync_directory(path)?;
        }
        if let Some(history) = backup {
            write_atomic(path, &serde_json::to_vec_pretty(&history)?)?;
            return Ok(LoadOutcome::RecoveredBackup(history));
        }
        let history = ClipboardHistory::default();
        Ok(match corrupt {
            Some(path) => LoadOutcome::RecoveredCorrupt { history, path },
            None => LoadOutcome::Loaded(history),
        })
    }

    pub fn save_atomic(
        path: impl AsRef<Path>,
        history: &ClipboardHistory,
    ) -> Result<(), StoreError> {
        let path = path.as_ref();
        complete_previous_removal(path)?;
        let previous = read_history(path)?;
        let backup_data = encode_backup(previous.as_ref().unwrap_or(history))?;
        let data = serde_json::to_vec_pretty(history)?;
        let staged = stage_file(path, &data)?;
        let result = (|| {
            write_atomic(&path.with_extension("bak"), &backup_data)?;
            fs::rename(&staged, path)?;
            sync_directory(path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staged);
        }
        Ok(result?)
    }

    pub fn save_after_removal(
        path: impl AsRef<Path>,
        history: &ClipboardHistory,
    ) -> Result<RemovalOutcome, StoreError> {
        Self::save_removal(path.as_ref(), history, sync_directory)
    }

    fn save_removal(
        path: &Path,
        history: &ClipboardHistory,
        mut sync: impl FnMut(&Path) -> io::Result<()>,
    ) -> Result<RemovalOutcome, StoreError> {
        complete_previous_removal(path)?;
        read_history(path)?;
        let data = serde_json::to_vec_pretty(history)?;
        let pending = path.with_extension("removal");
        let staged = stage_file(&pending, &data)?;
        if let Err(error) = fs::rename(&staged, &pending) {
            let _ = fs::remove_file(&staged);
            return Err(error.into());
        }
        // Once published, the journal wins over either historical file.
        // The directory sync must succeed before either file is replaced.
        Ok(match finish_removal(path, history, &data, &mut sync) {
            Ok(()) => RemovalOutcome::Saved,
            Err(error) => RemovalOutcome::Pending(error),
        })
    }
}

fn read_history(path: &Path) -> Result<Option<ClipboardHistory>, StoreError> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(decode_history(&contents)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn encode_backup(history: &ClipboardHistory) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec_pretty(&RecoveryBackup {
        recovery_version: RECOVERY_VERSION,
        history,
    })
}

fn read_removal(path: &Path) -> Result<Option<(ClipboardHistory, Vec<u8>)>, StoreError> {
    match fs::read(path.with_extension("removal")) {
        Ok(data) => Ok(Some((decode_history(&data)?, data))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn complete_previous_removal(path: &Path) -> Result<(), StoreError> {
    if let Some((history, data)) = read_removal(path)? {
        finish_removal(path, &history, &data, &mut sync_directory)?;
    }
    Ok(())
}

fn finish_removal(
    path: &Path,
    history: &ClipboardHistory,
    data: &[u8],
    sync: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let pending = path.with_extension("removal");
    // A previous attempt may have published the record without syncing it.
    sync(&pending)?;
    write_atomic_with_sync(&path.with_extension("bak"), &encode_backup(history)?, sync)?;
    write_atomic_with_sync(path, data, sync)?;
    fs::remove_file(&pending)?;
    sync(&pending)
}

fn decode_history(contents: &[u8]) -> Result<ClipboardHistory, StoreError> {
    let history: ClipboardHistory = serde_json::from_slice(contents)?;
    if history.schema_version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(history.schema_version));
    }
    Ok(history.sanitize())
}

fn stage_file(path: &Path, data: &[u8]) -> io::Result<PathBuf> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let staged = path.with_extension(format!("tmp.{}", suffix()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    if let Err(error) = file.write_all(data).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(staged)
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    write_atomic_with_sync(path, data, &mut sync_directory)
}

fn write_atomic_with_sync(
    path: &Path,
    data: &[u8],
    sync: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let staged = stage_file(path, data)?;
    let result = fs::rename(&staged, path).and_then(|_| sync(path));
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn sync_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

fn suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}.{}", std::process::id(), now)
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardHistory, ClipboardStore, IngestOutcome, LoadOutcome, StoreError, MAX_CHARS,
        MAX_ENTRIES,
    };
    use std::fs;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mactowindows-clipboard-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn removal_commit_is_recovered_at_each_directory_sync_boundary() {
        use super::RemovalOutcome;
        for fail_at in 1..=4 {
            let path = temp_path(&format!("remove-sync-failure-{fail_at}.json"));
            let mut original = ClipboardHistory::default();
            original.ingest("old".into(), 1);
            ClipboardStore::save_atomic(&path, &original).unwrap();
            let mut removed = original.clone();
            removed.clear();
            let mut syncs = 0;
            let result = ClipboardStore::save_removal(&path, &removed, |file| {
                syncs += 1;
                if syncs == fail_at {
                    Err(std::io::Error::other("simulated directory sync failure"))
                } else {
                    super::sync_directory(file)
                }
            })
            .unwrap();
            assert!(matches!(result, RemovalOutcome::Pending(_)));
            if fail_at == 1 {
                assert_eq!(super::read_history(&path).unwrap(), Some(original));
            }
            if fail_at == 2 {
                fs::write(&path, "damaged current history").unwrap();
            }
            assert_eq!(
                ClipboardStore::load_or_recover(&path).unwrap().history(),
                removed
            );
            assert!(!path.with_extension("removal").exists());
            let mut later = removed.clone();
            later.ingest("new".into(), 2);
            ClipboardStore::save_atomic(&path, &later).unwrap();
            fs::remove_file(&path).unwrap();
            assert_eq!(
                ClipboardStore::load_or_recover(&path).unwrap().history(),
                removed
            );
            fs::remove_file(&path).unwrap();
            fs::remove_file(path.with_extension("bak")).unwrap();
        }
    }

    #[test]
    fn ingest_rejects_empty_and_over_character_limit() {
        let mut history = ClipboardHistory::default();
        assert_eq!(history.ingest("".into(), 1), IngestOutcome::IgnoredEmpty);
        assert_eq!(
            history.ingest("x".repeat(MAX_CHARS + 1), 2),
            IngestOutcome::IgnoredTooLarge
        );
        assert!(history.entries().is_empty());
    }

    #[test]
    fn duplicate_moves_to_front_and_refreshes_timestamp() {
        let mut history = ClipboardHistory::default();
        let first = history.ingest("alpha".into(), 1);
        let second = history.ingest("beta".into(), 2);
        assert!(matches!(first, IngestOutcome::Added(_)));
        assert!(matches!(second, IngestOutcome::Added(_)));
        let duplicate = history.ingest("alpha".into(), 3);
        assert!(matches!(duplicate, IngestOutcome::Moved(_)));
        assert_eq!(history.entries()[0].text, "alpha");
        assert_eq!(history.entries()[0].copied_at_unix_ms, 3);
        assert_eq!(history.entries().len(), 2);
    }

    #[test]
    fn history_evicts_oldest_after_max_entries() {
        let mut history = ClipboardHistory::default();
        for index in 0..(MAX_ENTRIES + 1) {
            history.ingest(format!("item-{index}"), index as u64);
        }
        assert_eq!(history.entries().len(), MAX_ENTRIES);
        assert_eq!(history.entries().last().unwrap().text, "item-1");
    }

    #[test]
    fn delete_and_clear_update_history() {
        let mut history = ClipboardHistory::default();
        let id = match history.ingest("alpha".into(), 1) {
            IngestOutcome::Added(id) => id,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(history.remove(id));
        assert!(!history.remove(id));
        history.ingest("beta".into(), 2);
        history.clear();
        assert!(history.entries().is_empty());
    }

    #[test]
    fn corrupt_file_is_renamed_and_returns_empty_history() {
        let path = temp_path("corrupt");
        let _ = fs::remove_file(&path);
        fs::write(&path, b"not-json").unwrap();
        let outcome = ClipboardStore::load_or_recover(&path).unwrap();
        assert!(matches!(outcome, LoadOutcome::RecoveredCorrupt { .. }));
        assert!(matches!(outcome.history(), ClipboardHistory { .. }));
        assert!(!path.exists());
        let parent = path.parent().unwrap();
        let corrupt_count = fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("mactowindows-clipboard-corrupt-")
            })
            .count();
        assert!(corrupt_count >= 1);
        for entry in fs::read_dir(parent).unwrap().filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("mactowindows-clipboard-corrupt-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);
        let mut history = ClipboardHistory::default();
        history.ingest("alpha".into(), 1);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        let loaded = ClipboardStore::load_or_recover(&path).unwrap().history();
        assert_eq!(loaded.entries(), history.entries());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("bak"));
    }

    #[test]
    fn recovers_only_valid_current_backups_without_rotating_them() {
        let directory = temp_path("current-recovery");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("history.json");
        let mut history = ClipboardHistory::default();
        history.ingest("earlier".into(), 1);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        history.ingest("latest".into(), 2);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        let backup = fs::read(path.with_extension("bak")).unwrap();
        fs::write(&path, b"invalid json").unwrap();

        let loaded = ClipboardStore::load_or_recover(&path).unwrap();
        assert!(matches!(loaded, LoadOutcome::RecoveredBackup(_)));
        assert_eq!(loaded.history().entries()[0].text, "earlier");
        assert_eq!(fs::read(path.with_extension("bak")).unwrap(), backup);
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            ClipboardStore::load_or_recover(&path).unwrap(),
            LoadOutcome::RecoveredBackup(_)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_backup_is_preserved_without_auto_restoring_old_history() {
        let directory = temp_path("legacy-backup");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("history.json");
        let mut history = ClipboardHistory::default();
        history.ingest("before clear".into(), 1);
        let legacy = serde_json::to_vec(&history).unwrap();
        fs::write(path.with_extension("bak"), &legacy).unwrap();
        fs::write(&path, b"invalid json").unwrap();

        let mut history = ClipboardStore::load_or_recover(&path).unwrap().history();
        assert!(history.entries().is_empty());
        assert!(!path.with_extension("bak").exists());
        let recovery = ClipboardStore::manual_recovery_dir(&path);
        let legacy_path = fs::read_dir(&recovery)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(&legacy_path).unwrap(), legacy);

        history.ingest("new entry".into(), 2);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        history.ingest("another new entry".into(), 3);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        assert_eq!(fs::read(legacy_path).unwrap(), legacy);
        assert_eq!(
            ClipboardStore::load_or_recover(&path).unwrap().history(),
            history
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn backup_recovery_respects_delete_clear_and_subsequent_copy() {
        let directory = temp_path("recovery-removals");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("history.json");
        let mut history = ClipboardHistory::default();
        history.ingest("delete me".into(), 1);
        history.ingest("keep me".into(), 2);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        assert!(history.remove(1));
        ClipboardStore::save_after_removal(&path, &history).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            ClipboardStore::load_or_recover(&path).unwrap().history(),
            history
        );

        history.clear();
        ClipboardStore::save_after_removal(&path, &history).unwrap();
        fs::write(&path, b"invalid json").unwrap();
        assert_eq!(
            ClipboardStore::load_or_recover(&path).unwrap().history(),
            history
        );
        history.ingest("first after clear".into(), 3);
        ClipboardStore::save_atomic(&path, &history).unwrap();
        fs::remove_file(&path).unwrap();
        let recovered = ClipboardStore::load_or_recover(&path).unwrap().history();
        assert!(recovered.entries().is_empty());
        assert_eq!(recovered.next_id, 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn preserves_both_files_when_manually_recoverable_backup_cannot_be_moved() {
        let directory = temp_path("blocked-recovery");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("history.json");
        let legacy = serde_json::to_vec(&ClipboardHistory::default()).unwrap();
        let backup = path.with_extension("bak");
        fs::write(&backup, &legacy).unwrap();
        fs::write(&path, b"invalid json").unwrap();
        fs::write(ClipboardStore::manual_recovery_dir(&path), b"collision").unwrap();

        assert!(matches!(
            ClipboardStore::load_or_recover(&path),
            Err(StoreError::Io(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"invalid json");
        assert_eq!(fs::read(backup).unwrap(), legacy);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn load_sanitizes_semantically_invalid_history() {
        let path = temp_path("sanitize");
        let _ = fs::remove_file(&path);
        let mut entries = Vec::new();
        entries.push(serde_json::json!({
            "id": 0,
            "text": "zero-id",
            "copied_at_unix_ms": 1
        }));
        entries.push(serde_json::json!({
            "id": 1,
            "text": "",
            "copied_at_unix_ms": 2
        }));
        entries.push(serde_json::json!({
            "id": 2,
            "text": "duplicate",
            "copied_at_unix_ms": 3
        }));
        entries.push(serde_json::json!({
            "id": 3,
            "text": "duplicate",
            "copied_at_unix_ms": 4
        }));
        entries.push(serde_json::json!({
            "id": 4,
            "text": "x".repeat(MAX_CHARS + 1),
            "copied_at_unix_ms": 5
        }));
        for id in 5..=(MAX_ENTRIES as u64 + 10) {
            entries.push(serde_json::json!({
                "id": id,
                "text": format!("item-{id}"),
                "copied_at_unix_ms": id
            }));
        }
        fs::write(
            &path,
            serde_json::json!({
                "schema_version": 1,
                "next_id": 1,
                "entries": entries,
            })
            .to_string(),
        )
        .unwrap();

        let history = ClipboardStore::load_or_recover(&path).unwrap().history();
        assert!(history.entries().len() <= MAX_ENTRIES);
        assert!(history.entries().iter().all(|entry| {
            entry.id > 0 && !entry.text.is_empty() && entry.text.chars().count() <= MAX_CHARS
        }));
        assert!(history
            .entries()
            .windows(2)
            .all(|pair| pair[0].id != pair[1].id && pair[0].text != pair[1].text));
        let max_id = history
            .entries()
            .iter()
            .map(|entry| entry.id)
            .max()
            .unwrap_or(0);
        assert!(history.next_id > max_id);
        let _ = fs::remove_file(&path);
    }
}
