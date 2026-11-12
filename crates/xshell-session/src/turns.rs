//! Durable records of finished agent turns (H5).
//!
//! A turn's events live in the daemon's in-memory journal, which a restart
//! loses, and a session that is not durable does not outlive the daemon
//! either. So each agent turn's outcome is also written here, under the
//! daemon's state directory, as one file per turn named by its turn ID.
//! `xshell result --turn` and `xshell wait --turn` read it when the journal
//! no longer has the turn. Records are bounded: the oldest are removed
//! beyond [`MAX_TURN_RECORDS`].

use crate::{TurnAuditRef, TurnFailureReason};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;
use xshell_execution::TurnUsage;

/// Most turn records kept. The oldest are removed when a new one is written.
pub const MAX_TURN_RECORDS: usize = 256;
/// Longest answer kept in a record; longer answers are cut at a character
/// boundary. The session's history keeps the whole answer.
pub const MAX_RECORDED_ANSWER_BYTES: usize = 64 * 1024;
/// Directory under the state directory that holds the records.
const TURNS_DIRECTORY: &str = "turns";

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// What a finished agent turn did, kept after the daemon restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn_id: String,
    pub session_id: String,
    pub session_name: String,
    pub finished_at_unix_ms: u64,
    pub outcome: TurnOutcome,
    /// Why the turn failed, when that is a cause clients act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TurnFailureReason>,
    /// The failure message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The main agent's last non-empty response.
    #[serde(default)]
    pub answer: String,
    /// Tool calls the model asked for, across every agent in the turn.
    #[serde(default)]
    pub tool_calls: usize,
    /// Prompted calls that were denied.
    #[serde(default)]
    pub approvals_denied: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<TurnAuditRef>,
}

/// Where a turn is: running in a live session, finished with a record, or
/// both (the record is written just before the turn is released).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLocation {
    pub turn_id: String,
    /// The session running the turn, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    /// The turn's record, once it has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<TurnRecord>,
}

/// The records directory of one daemon.
#[derive(Debug, Clone)]
pub struct TurnStore {
    directory: PathBuf,
    max_records: usize,
}

impl TurnStore {
    /// Open (creating if needed) the records directory under
    /// `state_directory`.
    pub fn open(state_directory: &Path) -> Result<Self> {
        let directory = state_directory.join(TURNS_DIRECTORY);
        xshell_platform::ensure_secure_directory(&directory, "turn records")?;
        Ok(Self {
            directory,
            max_records: MAX_TURN_RECORDS,
        })
    }

    /// Write `record` durably (a new file, synced, renamed into place, then
    /// the directory synced), then remove the oldest records beyond the
    /// limit.
    pub fn record(&self, record: &TurnRecord) -> Result<()> {
        let name = record_name(&record.turn_id)?;
        let mut record = record.clone();
        truncate_at_boundary(&mut record.answer, MAX_RECORDED_ANSWER_BYTES);
        let encoded = serde_json::to_vec(&record)?;
        let final_path = self.directory.join(&name);
        let temporary = self.directory.join(format!(".{name}.tmp"));
        let written = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            fs::rename(&temporary, &final_path)?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error.context("cannot write the turn record"));
        }
        crate::registry::sync_directory(&self.directory)
            .context("cannot sync the turn records directory")?;
        self.prune();
        Ok(())
    }

    /// The record of `turn_id`, if one was kept.
    pub fn get(&self, turn_id: &str) -> Result<Option<TurnRecord>> {
        let path = self.directory.join(record_name(turn_id)?);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let record: TurnRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid turn record {}", path.display()))?;
        if record.turn_id != turn_id {
            bail!("turn record {} names another turn", path.display());
        }
        Ok(Some(record))
    }

    /// Remove the oldest records beyond the limit. Best-effort: a record
    /// that cannot be removed is tried again after the next write.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        let mut records: Vec<(SystemTime, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|entry| {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.path()))
            })
            .collect();
        if records.len() <= self.max_records {
            return;
        }
        records.sort();
        let excess = records.len() - self.max_records;
        for (_, path) in records.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }
}

/// The file name of a turn's record. Turn IDs are UUIDs; anything else is
/// refused, so a client cannot name a path.
fn record_name(turn_id: &str) -> Result<String> {
    let parsed =
        Uuid::parse_str(turn_id).with_context(|| format!("{turn_id:?} is not a turn ID"))?;
    Ok(format!("{}.json", parsed.hyphenated()))
}

fn truncate_at_boundary(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record(turn_id: &str) -> TurnRecord {
        TurnRecord {
            turn_id: turn_id.into(),
            session_id: "session".into(),
            session_name: "job".into(),
            finished_at_unix_ms: 1,
            outcome: TurnOutcome::Completed,
            reason: None,
            message: None,
            answer: "done".into(),
            tool_calls: 2,
            approvals_denied: 0,
            usage: None,
            audit: None,
        }
    }

    #[test]
    fn records_survive_reopening_and_unknown_turns_have_none() {
        let temp = TempDir::new().unwrap();
        let turn = Uuid::new_v4().to_string();
        TurnStore::open(temp.path())
            .unwrap()
            .record(&record(&turn))
            .unwrap();
        let reopened = TurnStore::open(temp.path()).unwrap();
        assert_eq!(reopened.get(&turn).unwrap(), Some(record(&turn)));
        assert_eq!(reopened.get(&Uuid::new_v4().to_string()).unwrap(), None);
    }

    #[test]
    fn only_turn_ids_name_records() {
        let temp = TempDir::new().unwrap();
        let store = TurnStore::open(temp.path()).unwrap();
        for bad in ["../sessions", "", "not-a-uuid", "/etc/passwd"] {
            assert!(store.get(bad).is_err(), "{bad:?}");
            assert!(store.record(&record(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_oldest_records_are_removed_beyond_the_limit() {
        let temp = TempDir::new().unwrap();
        let mut store = TurnStore::open(temp.path()).unwrap();
        store.max_records = 3;
        let turns: Vec<String> = (0..5).map(|_| Uuid::new_v4().to_string()).collect();
        for turn in &turns {
            store.record(&record(turn)).unwrap();
            // Distinct modification times, so age is unambiguous.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(store.get(&turns[0]).unwrap(), None);
        assert_eq!(store.get(&turns[1]).unwrap(), None);
        for turn in &turns[2..] {
            assert!(store.get(turn).unwrap().is_some());
        }
    }

    #[test]
    fn long_answers_are_cut_at_a_character_boundary() {
        let temp = TempDir::new().unwrap();
        let store = TurnStore::open(temp.path()).unwrap();
        let turn = Uuid::new_v4().to_string();
        let mut long = record(&turn);
        long.answer = "é".repeat(MAX_RECORDED_ANSWER_BYTES);
        store.record(&long).unwrap();
        let kept = store.get(&turn).unwrap().unwrap().answer;
        assert!(kept.len() <= MAX_RECORDED_ANSWER_BYTES);
        assert!(kept.chars().all(|c| c == 'é'));
    }
}
