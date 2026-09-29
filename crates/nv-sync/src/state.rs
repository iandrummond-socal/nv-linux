//! Persistent sync state.
//!
//! Empty state means a cold start: the engine treats every file as never-synced.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::error::Result;
use crate::remote::RemoteChange;

pub const STATE_FILE: &str = "sync-state.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    pub cursor: Option<String>,
    pub account_id: Option<String>,
    /// Canonical notes directory, when canonicalization succeeds.
    pub notes_dir: Option<PathBuf>,
    /// Unapplied remote delta to resume after a restart.
    pub pending: Vec<RemoteChange>,
    pub files: BTreeMap<String, Base>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub id: String,
    pub rev: String,
    /// Content hash both sides agreed on; decides equality.
    pub hash: String,
    /// Preserves the on-disk case for downloads.
    pub local_name: String,
    pub size: u64,
    /// Used only to skip re-hashing.
    pub mtime: SystemTime,
}

pub fn state_dir_with(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("NV_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|dir| dir.join("nv-linux")))
}

impl SyncState {
    pub fn load(state_dir: &Path, notes_dir: &Path, account_id: Option<&str>) -> Result<Self> {
        let path = state_dir.join(STATE_FILE);
        let mut state: Self = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(state) => state,
                Err(error) => {
                    let bad_path = state_dir.join(format!("{STATE_FILE}.bad"));
                    fs::rename(&path, &bad_path)?;
                    log::warn!(
                        "Corrupt sync state moved to {}: {error}",
                        bad_path.display()
                    );
                    Self::default()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        let notes_dir = fs::canonicalize(notes_dir).unwrap_or_else(|_| notes_dir.to_path_buf());
        let different_dir = state
            .notes_dir
            .as_ref()
            .is_some_and(|dir| dir != &notes_dir);
        let different_account = state
            .account_id
            .as_deref()
            .zip(account_id)
            .is_some_and(|(stored, current)| stored != current);
        if different_dir || different_account {
            log::warn!(
                "Sync state belongs to a different notes directory or account; starting cold"
            );
            state = Self::default();
        }
        state.notes_dir = Some(notes_dir);
        if let Some(account_id) = account_id {
            state.account_id = Some(account_id.to_owned());
        }
        Ok(state)
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        fs::create_dir_all(state_dir)?;
        let mut temp = NamedTempFile::new_in(state_dir)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut temp, self)?;
        temp.as_file().sync_all()?;
        temp.persist(state_dir.join(STATE_FILE))
            .map_err(|error| error.error)?;
        File::open(state_dir)?.sync_all()?;
        Ok(())
    }

    pub fn is_cold(&self) -> bool {
        self.cursor.is_none() && self.files.is_empty()
    }

    /// Return to a cold start while retaining the current folder and account.
    pub fn reset(&mut self) {
        self.cursor = None;
        self.pending.clear();
        self.files.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::tempdir;

    fn populated(notes_dir: &Path) -> SyncState {
        SyncState {
            cursor: Some("cursor".into()),
            account_id: Some("account".into()),
            notes_dir: Some(fs::canonicalize(notes_dir).unwrap()),
            pending: vec![RemoteChange::Deleted {
                key: "/deleted.txt".into(),
            }],
            files: BTreeMap::from([(
                "/note.txt".into(),
                Base {
                    id: "id:note".into(),
                    rev: "revision".into(),
                    hash: "hash".into(),
                    local_name: "Note.txt".into(),
                    size: 42,
                    mtime: UNIX_EPOCH + Duration::new(123, 456),
                },
            )]),
        }
    }

    #[test]
    fn missing_state_is_cold() {
        let dir = tempdir().unwrap();
        let notes = tempdir().unwrap();
        let state = SyncState::load(dir.path(), notes.path(), Some("account")).unwrap();
        assert!(state.is_cold());
        assert!(state.pending.is_empty());
        assert_eq!(
            state.notes_dir,
            Some(fs::canonicalize(notes.path()).unwrap())
        );
        assert_eq!(state.account_id.as_deref(), Some("account"));
    }

    #[test]
    fn save_load_round_trip_and_same_world() {
        let dir = tempdir().unwrap();
        let notes = tempdir().unwrap();
        let state = populated(notes.path());
        state.save(dir.path()).unwrap();
        assert_eq!(
            SyncState::load(dir.path(), notes.path(), Some("account")).unwrap(),
            state
        );
        assert_eq!(
            SyncState::load(dir.path(), notes.path(), None).unwrap(),
            state
        );
    }

    #[test]
    fn save_is_private_and_leaves_no_temp_files() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join("nested/state");
        let state = populated(dir.path());
        state.save(&state_dir).unwrap();
        state.save(&state_dir).unwrap();
        let mode = fs::metadata(state_dir.join(STATE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o600);
        let entries: Vec<_> = fs::read_dir(&state_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(STATE_FILE)]);
    }

    #[test]
    fn corrupt_state_is_preserved_as_bad() {
        let dir = tempdir().unwrap();
        let bad_path = dir.path().join(format!("{STATE_FILE}.bad"));
        let original = b"invalid JSON\xff";
        fs::write(dir.path().join(STATE_FILE), original).unwrap();
        fs::write(&bad_path, b"older corrupt state").unwrap();
        let state = SyncState::load(dir.path(), dir.path(), Some("account")).unwrap();
        assert!(state.is_cold());
        assert!(state.pending.is_empty());
        assert_eq!(fs::read(bad_path).unwrap(), original);
        assert!(!dir.path().join(STATE_FILE).exists());
        assert_eq!(state.account_id.as_deref(), Some("account"));
        assert_eq!(state.notes_dir, Some(fs::canonicalize(dir.path()).unwrap()));
    }

    #[test]
    fn different_notes_directory_starts_cold() {
        let dir = tempdir().unwrap();
        let other = tempdir().unwrap();
        populated(dir.path()).save(dir.path()).unwrap();
        let state = SyncState::load(dir.path(), other.path(), Some("account")).unwrap();
        assert!(state.is_cold());
        assert!(state.pending.is_empty());
        assert_eq!(
            state.notes_dir,
            Some(fs::canonicalize(other.path()).unwrap())
        );
    }

    #[test]
    fn different_account_starts_cold() {
        let dir = tempdir().unwrap();
        populated(dir.path()).save(dir.path()).unwrap();
        let state = SyncState::load(dir.path(), dir.path(), Some("other")).unwrap();
        assert!(state.is_cold());
        assert!(state.pending.is_empty());
        assert_eq!(state.account_id.as_deref(), Some("other"));
    }

    #[test]
    fn canonical_directory_alias_preserves_state() {
        let dir = tempdir().unwrap();
        let notes = tempdir().unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(notes.path(), &alias).unwrap();
        let state = populated(notes.path());
        state.save(dir.path()).unwrap();
        assert_eq!(SyncState::load(dir.path(), &alias, None).unwrap(), state);
    }

    #[test]
    fn missing_notes_directory_uses_given_path() {
        let dir = tempdir().unwrap();
        let notes = dir.path().join("missing");
        let state = SyncState::load(dir.path(), &notes, None).unwrap();
        assert!(state.is_cold());
        assert_eq!(state.notes_dir, Some(notes));
    }

    #[test]
    fn state_directory_uses_injected_override() {
        assert_eq!(
            state_dir_with(&|key| {
                assert_eq!(key, "NV_STATE_DIR");
                Some("/custom/state".into())
            }),
            Some(PathBuf::from("/custom/state"))
        );
    }

    #[test]
    fn reset_keeps_world_and_clears_progress() {
        let dir = tempdir().unwrap();
        let mut state = populated(dir.path());
        assert!(!state.is_cold());
        state.reset();
        assert_eq!(
            state,
            SyncState {
                notes_dir: Some(fs::canonicalize(dir.path()).unwrap()),
                account_id: Some("account".into()),
                ..SyncState::default()
            }
        );
        assert!(state.is_cold());
    }
}
