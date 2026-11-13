use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_ENTRIES: usize = 50;
pub const MAX_CHARS: usize = 1_048_576;
const SCHEMA_VERSION: u32 = 1;

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
}

impl LoadOutcome {
    pub fn history(self) -> ClipboardHistory {
        match self {
            Self::Loaded(history) | Self::RecoveredCorrupt { history, .. } => history,
        }
    }
}

pub struct ClipboardStore;

impl ClipboardStore {
    pub fn load_or_recover(path: impl AsRef<Path>) -> Result<LoadOutcome, StoreError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(LoadOutcome::Loaded(ClipboardHistory::default()));
        }
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) => return Err(StoreError::Io(error)),
        };
        match serde_json::from_str::<ClipboardHistory>(&contents) {
            Ok(history) if history.schema_version == SCHEMA_VERSION => {
                Ok(LoadOutcome::Loaded(history.sanitize()))
            }
            Ok(history) => Err(StoreError::UnsupportedSchema(history.schema_version)),
            Err(_) => {
                let corrupt_path = path.with_extension(format!("corrupt.{}", unix_ms()));
                fs::rename(path, &corrupt_path)?;
                Ok(LoadOutcome::RecoveredCorrupt {
                    history: ClipboardHistory::default(),
                    path: corrupt_path,
                })
            }
        }
    }

    pub fn save_atomic(
        path: impl AsRef<Path>,
        history: &ClipboardHistory,
    ) -> Result<(), StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(history)?;
        let tmp_path = path.with_extension(format!("tmp.{}.{}", std::process::id(), unix_ms()));
        let mut file = File::create(&tmp_path)?;
        use std::io::Write;
        file.write_all(&data)?;
        file.sync_all()?;
        if path.exists() {
            let backup = path.with_extension("bak");
            let backup_tmp = backup.with_extension(format!("tmp.{}", unix_ms()));
            fs::copy(path, &backup_tmp)?;
            File::open(&backup_tmp)?.sync_all()?;
            fs::rename(backup_tmp, backup)?;
        }
        fs::rename(&tmp_path, path)?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardHistory, ClipboardStore, IngestOutcome, LoadOutcome, MAX_CHARS, MAX_ENTRIES,
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
        let _ = fs::remove_file(path.with_extension("toml.bak"));
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
