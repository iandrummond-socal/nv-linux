//! Apply one planned sync action, preserving displaced local edits.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Once;
use std::time::Duration;

use chrono::{DateTime, Local};
use tempfile::NamedTempFile;

use crate::content_hash::content_hash;
use crate::plan::{Action, LocalFile, SkipReason};
use crate::remote::{Remote, RemoteError, RemoteFile};
use crate::state::{Base, SyncState};
use crate::{Error, Result};

pub struct Item {
    pub key: String,
    pub base: Option<Base>,
    pub local: Option<LocalFile>,
    pub remote: Option<RemoteFile>,
}

pub struct Ctx<'a> {
    pub dir: &'a Path,
    pub remote: &'a mut dyn Remote,
    pub trasher: &'a dyn nv_core::storage::Trasher,
    pub state: &'a mut SyncState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Replan,
    Retry,
    Downloaded { bytes_hash: String },
    ConflictCopy { name: String },
    Skipped(SkipReason),
}

/// Read through a single descriptor, rejecting observable in-place writes.
pub fn read_stable(path: &Path) -> Result<Vec<u8>> {
    for attempt in 0..5 {
        let mut file = File::open(path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file").into());
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        if before.len() == after.len()
            && before.modified()? == after.modified()?
            && after.len() == bytes.len() as u64
        {
            return Ok(bytes);
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    Err(io::Error::new(io::ErrorKind::WouldBlock, "file kept changing").into())
}

pub fn conflict_name(dir: &Path, name: &str, now: DateTime<Local>) -> String {
    let path = Path::new(name);
    let title = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    let stamp = now.format("%Y-%m-%d %H%M");
    for suffix in 1_u64.. {
        let number = if suffix == 1 {
            String::new()
        } else {
            format!(" {suffix}")
        };
        let name = format!("{title} (conflict {stamp}{number}){ext}");
        // A dangling symlink also occupies a name. Other errors are surfaced by
        // the subsequent no-clobber operation, rather than spinning here.
        if fs::symlink_metadata(dir.join(&name)).is_err() {
            return name;
        }
    }
    unreachable!("conflict suffix space exhausted")
}

/// Whether the canonical file still holds the scanned bytes; a missing file
/// counts as changed.
fn local_unchanged(path: &Path, local: &LocalFile) -> Result<bool> {
    match read_stable(path) {
        Ok(bytes) => Ok(content_hash(&bytes) == local.hash),
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Find a conflict copy of `name` in `dir` already holding exactly `bytes`.
fn existing_copy(dir: &Path, name: &str, hash: &str, bytes: &[u8]) -> Result<Option<String>> {
    let path = Path::new(name);
    let title = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    let prefix = format!("{title} (conflict ");
    let suffix = format!("){ext}");
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(candidate) = entry.file_name().into_string() else {
            continue;
        };
        if candidate.len() < prefix.len() + suffix.len()
            || !candidate.starts_with(&prefix)
            || !candidate.ends_with(&suffix)
        {
            continue;
        }
        // symlink_metadata: never follow a link out of the folder.
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_file() || meta.len() != bytes.len() as u64 {
            continue;
        }
        match read_stable(&entry.path()) {
            Ok(existing) if content_hash(&existing) == hash => return Ok(Some(candidate)),
            _ => continue,
        }
    }
    Ok(None)
}

fn invalid(message: &str) -> Error {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

fn local(item: &Item) -> Result<&LocalFile> {
    item.local
        .as_ref()
        .ok_or_else(|| invalid("action requires local metadata"))
}

fn remote(item: &Item) -> Result<&RemoteFile> {
    item.remote
        .as_ref()
        .ok_or_else(|| invalid("action requires remote metadata"))
}

fn name(item: &Item) -> Result<&str> {
    item.local
        .as_ref()
        .map(|file| file.name.as_str())
        .or_else(|| item.base.as_ref().map(|base| base.local_name.as_str()))
        .or_else(|| item.remote.as_ref().map(|file| file.name.as_str()))
        .ok_or_else(|| invalid("action requires a local filename"))
}

fn record(item: &Item, ctx: &mut Ctx<'_>, file: &RemoteFile, name: &str) -> Result<()> {
    let mtime = fs::metadata(ctx.dir.join(name))?.modified()?;
    ctx.state.files.insert(
        item.key.clone(),
        Base {
            id: file.id.clone(),
            rev: file.rev.clone(),
            hash: file.content_hash.clone(),
            local_name: name.to_owned(),
            size: file.size,
            mtime,
        },
    );
    Ok(())
}

fn download(ctx: &mut Ctx<'_>, file: &RemoteFile) -> Result<Vec<u8>> {
    let bytes = ctx.remote.download(&file.id)?;
    if content_hash(&bytes) != file.content_hash {
        return Err(RemoteError::Other("download hash mismatch".into()).into());
    }
    Ok(bytes)
}

fn temporary(dir: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let mut temp = tempfile::Builder::new()
        .prefix(".nv-sync-")
        .tempfile_in(dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    Ok(temp)
}

fn upload(item: &Item, ctx: &mut Ctx<'_>, bytes: &[u8], rev: Option<&str>) -> Result<Outcome> {
    let local = local(item)?;
    match ctx.remote.upload(&local.name, bytes, rev) {
        Ok(file) => {
            if file.content_hash != content_hash(bytes) {
                return Err(RemoteError::Other("upload hash mismatch".into()).into());
            }
            record(item, ctx, &file, &local.name)?;
            Ok(Outcome::Done)
        }
        Err(RemoteError::Conflict) => Ok(Outcome::Retry),
        Err(error) => Err(error.into()),
    }
}

/// Keep ownership of the displaced path outside tempfile's automatic cleanup:
/// any failure after exchange must leave the user's text recoverable on disk.
fn replace(temp: NamedTempFile, target: &Path, dir: &Path, local: &LocalFile) -> Result<Outcome> {
    let tmp = temp.into_temp_path().keep().map_err(|error| error.error)?;
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        &tmp,
        rustix::fs::CWD,
        target,
        rustix::fs::RenameFlags::EXCHANGE,
    ) {
        Ok(()) => {
            let bytes = read_stable(&tmp)?;
            if content_hash(&bytes) == local.hash {
                fs::remove_file(&tmp)?;
                Ok(Outcome::Done)
            } else {
                let now = Local::now();
                loop {
                    let name = conflict_name(dir, &local.name, now);
                    match fs::hard_link(&tmp, dir.join(&name)) {
                        Ok(()) => {
                            // Make the recovery link durable before unlinking tmp.
                            File::open(dir)?.sync_all()?;
                            fs::remove_file(&tmp)?;
                            return Ok(Outcome::ConflictCopy { name });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        Err(error) => {
            // Exchange failed, so tmp still contains only downloaded bytes.
            let result = (|| {
                if error == rustix::io::Errno::INVAL {
                    static WARN: Once = Once::new();
                    WARN.call_once(|| {
                        log::warn!("filesystem lacks rename exchange; using checked replacement");
                    });
                    if content_hash(&read_stable(target)?) != local.hash {
                        return Ok(Outcome::Replan);
                    }
                    // Equivalent to TempPath::persist: atomically replace the target.
                    fs::rename(&tmp, target)?;
                    Ok(Outcome::Done)
                } else if io::Error::from(error).kind() == io::ErrorKind::NotFound {
                    Ok(Outcome::Replan)
                } else {
                    Err(io::Error::from(error).into())
                }
            })();
            match fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            result
        }
    }
}

pub fn apply(action: &Action, item: &Item, ctx: &mut Ctx<'_>) -> Result<Outcome> {
    match action {
        Action::Nothing { .. } => return Ok(Outcome::Done),
        Action::Skip { reason, .. } => return Ok(Outcome::Skipped(reason.clone())),
        Action::ForgetBase { .. } => {
            ctx.state.files.remove(&item.key);
            return Ok(Outcome::Done);
        }
        _ => {}
    }
    let name = name(item)?;
    let path = ctx.dir.join(name);
    match action {
        Action::Adopt { .. } | Action::Upload { .. } | Action::TrashLocal { .. } => {
            let bytes = match read_stable(&path) {
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Outcome::Replan);
                }
                result => result?,
            };
            if content_hash(&bytes) != local(item)?.hash {
                return Ok(Outcome::Replan);
            }
            match action {
                Action::Adopt { .. } => record(item, ctx, remote(item)?, name)?,
                Action::Upload { expect_rev, .. } => {
                    return upload(item, ctx, &bytes, expect_rev.as_deref());
                }
                Action::TrashLocal { .. } => {
                    ctx.trasher.trash(&path)?;
                    ctx.state.files.remove(&item.key);
                }
                _ => unreachable!(),
            }
            Ok(Outcome::Done)
        }
        Action::Download { .. } => {
            let file = remote(item)?;
            let bytes = download(ctx, file)?;
            let temp = temporary(ctx.dir, &bytes)?;
            let outcome = if path.try_exists()? {
                let Some(local) = &item.local else {
                    return Ok(Outcome::Replan);
                };
                replace(temp, &path, ctx.dir, local)?
            } else {
                match temp.persist_noclobber(&path) {
                    Ok(_) => Outcome::Done,
                    Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                        return Ok(Outcome::Replan);
                    }
                    Err(error) => return Err(error.error.into()),
                }
            };
            File::open(ctx.dir)?.sync_all()?;
            if outcome == Outcome::Replan {
                return Ok(outcome);
            }
            record(item, ctx, file, name)?;
            match outcome {
                Outcome::ConflictCopy { .. } => Ok(outcome),
                _ => Ok(Outcome::Downloaded {
                    bytes_hash: file.content_hash.clone(),
                }),
            }
        }
        Action::Conflict { .. } => {
            let local = local(item)?;
            let file = remote(item)?;
            // Check the canonical file before writing anything: a replan here
            // must leave no copy behind for the next round to duplicate.
            if !local_unchanged(&path, local)? {
                return Ok(Outcome::Replan);
            }
            let bytes = download(ctx, file)?;
            // A copy of these exact bytes may survive from an earlier round that
            // stopped after writing it (replan, retry or error); reuse it.
            let copy = match existing_copy(ctx.dir, &local.name, &file.content_hash, &bytes)? {
                Some(copy) => copy,
                None => {
                    let mut temp = temporary(ctx.dir, &bytes)?;
                    let now = Local::now();
                    let copy = loop {
                        let copy = conflict_name(ctx.dir, &local.name, now);
                        match temp.persist_noclobber(ctx.dir.join(&copy)) {
                            Ok(_) => break copy,
                            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                                temp = error.file;
                            }
                            Err(error) => return Err(error.error.into()),
                        }
                    };
                    File::open(ctx.dir)?.sync_all()?;
                    copy
                }
            };
            let bytes = match read_stable(&path) {
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Outcome::Replan);
                }
                result => result?,
            };
            if content_hash(&bytes) != local.hash {
                return Ok(Outcome::Replan);
            }
            match upload(item, ctx, &bytes, Some(&file.rev))? {
                Outcome::Done => Ok(Outcome::ConflictCopy { name: copy }),
                outcome => Ok(outcome),
            }
        }
        Action::DeleteRemote { rev, .. } => {
            if path.try_exists()? {
                return Ok(Outcome::Replan);
            }
            match ctx.remote.delete(&remote(item)?.id, rev) {
                Ok(()) => {
                    ctx.state.files.remove(&item.key);
                    Ok(Outcome::Done)
                }
                Err(RemoteError::Conflict) => Ok(Outcome::Retry),
                Err(error) => Err(error.into()),
            }
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeRemote;
    use nv_core::storage::{FakeTrasher, NoteStore};
    use std::os::unix::fs::MetadataExt;
    use tempfile::{tempdir, TempDir};

    struct Fixture {
        dir: TempDir,
        trash: TempDir,
        remote: FakeRemote,
        state: SyncState,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempdir().unwrap(),
                trash: tempdir().unwrap(),
                remote: FakeRemote::with_hasher(content_hash),
                state: SyncState::default(),
            }
        }

        fn scan(&self) -> Item {
            let path = self.dir.path().join("Note.txt");
            let local = path.exists().then(|| {
                let bytes = read_stable(&path).unwrap();
                LocalFile {
                    name: "Note.txt".into(),
                    hash: content_hash(&bytes),
                    size: bytes.len() as u64,
                }
            });
            Item {
                key: "/note.txt".into(),
                base: self.state.files.get("/note.txt").cloned(),
                local,
                remote: self.remote.file("Note.txt"),
            }
        }

        fn apply(&mut self, action: Action, item: &Item) -> Result<Outcome> {
            apply(
                &action,
                item,
                &mut Ctx {
                    dir: self.dir.path(),
                    remote: &mut self.remote,
                    trasher: &FakeTrasher {
                        dir: self.trash.path().to_owned(),
                    },
                    state: &mut self.state,
                },
            )
        }

        fn write(&self, bytes: &[u8]) {
            fs::write(self.dir.path().join("Note.txt"), bytes).unwrap();
        }

        fn read(&self, name: &str) -> Vec<u8> {
            fs::read(self.dir.path().join(name)).unwrap()
        }

        fn download(&mut self, item: &Item) -> Result<Outcome> {
            self.apply(
                Action::Download {
                    key: item.key.clone(),
                },
                item,
            )
        }
    }

    #[test]
    fn download_new_records_base_and_cleans_temps() {
        let mut f = Fixture::new();
        f.remote.edit_remotely("Note.txt", b"remote\0bytes");
        let item = f.scan();
        assert_eq!(
            f.download(&item).unwrap(),
            Outcome::Downloaded {
                bytes_hash: content_hash(b"remote\0bytes"),
            }
        );
        assert_eq!(f.read("Note.txt"), b"remote\0bytes");
        let base = &f.state.files[&item.key];
        assert_eq!(base.hash, item.remote.unwrap().content_hash);
        assert_eq!(base.local_name, "Note.txt");
        assert_eq!(base.size, 12);
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn download_bad_hash_writes_nothing() {
        let mut f = Fixture::new();
        f.remote.edit_remotely("Note.txt", b"remote");
        let mut item = f.scan();
        item.remote.as_mut().unwrap().content_hash = "wrong".into();
        assert!(matches!(
            f.download(&item),
            Err(Error::Remote(RemoteError::Other(message)))
                if message == "download hash mismatch"
        ));
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 0);
        assert!(f.state.files.is_empty());
    }

    #[test]
    fn download_existing_unchanged_removes_displaced_temp() {
        let mut f = Fixture::new();
        f.write(b"old");
        f.remote.edit_remotely("Note.txt", b"remote");
        let item = f.scan();
        assert!(matches!(
            f.download(&item).unwrap(),
            Outcome::Downloaded { .. }
        ));
        assert_eq!(f.read("Note.txt"), b"remote");
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn download_preserves_edit_after_scan() {
        let mut f = Fixture::new();
        f.write(b"old");
        f.remote.edit_remotely("Note.txt", b"remote");
        let item = f.scan();
        f.write(b"newer local text");
        let Outcome::ConflictCopy { name } = f.download(&item).unwrap() else {
            panic!("expected preserved local edit");
        };
        assert_eq!(f.read("Note.txt"), b"remote");
        assert_eq!(f.read(&name), b"newer local text");
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 2);
        assert_eq!(f.state.files[&item.key].hash, content_hash(b"remote"));
    }

    #[test]
    fn download_replans_if_local_appeared() {
        let mut f = Fixture::new();
        f.remote.edit_remotely("Note.txt", b"remote");
        let item = f.scan();
        f.write(b"appeared");
        assert_eq!(f.download(&item).unwrap(), Outcome::Replan);
        assert_eq!(f.read("Note.txt"), b"appeared");
        assert!(f.state.files.is_empty());
    }

    #[test]
    fn conflict_keeps_canonical_inode_and_uploads_local() {
        let mut f = Fixture::new();
        f.write(b"local");
        f.remote.edit_remotely("Note.txt", b"remote");
        let item = f.scan();
        let inode = fs::metadata(f.dir.path().join("Note.txt")).unwrap().ino();
        let action = Action::Conflict {
            key: item.key.clone(),
        };
        let Outcome::ConflictCopy { name } = f.apply(action, &item).unwrap() else {
            panic!("expected remote copy");
        };
        assert_eq!(f.read(&name), b"remote");
        assert_eq!(f.read("Note.txt"), b"local");
        assert_eq!(
            fs::metadata(f.dir.path().join("Note.txt")).unwrap().ino(),
            inode
        );
        assert_eq!(
            f.remote.download(&item.remote.unwrap().id).unwrap(),
            b"local"
        );
        assert_eq!(f.state.files[&item.key].hash, content_hash(b"local"));
    }

    #[test]
    fn conflict_replan_before_copy_writes_nothing() {
        let mut f = Fixture::new();
        f.write(b"local");
        f.remote.edit_remotely("Note.txt", b"remote");
        let item = f.scan();
        let original = f.state.clone();
        let writes = f.remote.write_count();
        let action = Action::Conflict {
            key: item.key.clone(),
        };
        // The user saves again between the scan and the apply.
        f.write(b"typed during the pass");
        assert_eq!(f.apply(action.clone(), &item).unwrap(), Outcome::Replan);
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 1);
        assert_eq!(f.read("Note.txt"), b"typed during the pass");
        assert_eq!(f.remote.write_count(), writes);
        assert_eq!(f.state, original);
        // The replanned round preserves the remote text exactly once.
        let item = f.scan();
        let Outcome::ConflictCopy { name } = f.apply(action, &item).unwrap() else {
            panic!("expected remote copy");
        };
        assert_eq!(f.read(&name), b"remote");
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn conflict_reuses_identical_copy_left_by_earlier_round() {
        let mut f = Fixture::new();
        f.write(b"local");
        f.remote.edit_remotely("Note.txt", b"remote");
        // An earlier round wrote the copy, then stopped before uploading.
        fs::write(
            f.dir.path().join("Note (conflict 2026-09-28 2150).txt"),
            b"remote",
        )
        .unwrap();
        // A copy of different text is left alone and never reused.
        fs::write(
            f.dir.path().join("Note (conflict 2026-09-28 2149).txt"),
            b"older remote",
        )
        .unwrap();
        let item = f.scan();
        let action = Action::Conflict {
            key: item.key.clone(),
        };
        assert_eq!(
            f.apply(action, &item).unwrap(),
            Outcome::ConflictCopy {
                name: "Note (conflict 2026-09-28 2150).txt".into()
            }
        );
        assert_eq!(fs::read_dir(f.dir.path()).unwrap().count(), 3);
        assert_eq!(
            f.read("Note (conflict 2026-09-28 2149).txt"),
            b"older remote"
        );
        assert_eq!(f.state.files[&item.key].hash, content_hash(b"local"));
    }

    #[test]
    fn upload_retries_stale_revision_and_replans_changed_local() {
        let mut f = Fixture::new();
        f.write(b"same");
        f.remote.edit_remotely("Note.txt", b"same");
        let item = f.scan();
        f.apply(
            Action::Adopt {
                key: item.key.clone(),
            },
            &item,
        )
        .unwrap();
        let original = f.state.clone();
        f.remote.touch_remotely("Note.txt");
        let action = Action::Upload {
            key: item.key.clone(),
            expect_rev: Some(item.remote.as_ref().unwrap().rev.clone()),
        };
        assert_eq!(f.apply(action.clone(), &item).unwrap(), Outcome::Retry);
        assert_eq!(f.state, original);
        let writes = f.remote.write_count();
        f.write(b"changed");
        assert_eq!(f.apply(action, &item).unwrap(), Outcome::Replan);
        assert_eq!(f.remote.write_count(), writes);
        assert_eq!(f.state, original);
    }

    #[test]
    fn trash_checks_local_before_moving_to_trash() {
        let mut f = Fixture::new();
        f.write(b"old");
        let item = f.scan();
        let action = Action::TrashLocal {
            key: item.key.clone(),
        };
        f.write(b"new");
        assert_eq!(f.apply(action.clone(), &item).unwrap(), Outcome::Replan);
        assert_eq!(f.read("Note.txt"), b"new");
        let item = f.scan();
        assert_eq!(f.apply(action, &item).unwrap(), Outcome::Done);
        assert!(!f.dir.path().join("Note.txt").exists());
        assert_eq!(fs::read(f.trash.path().join("Note.txt")).unwrap(), b"new");
    }

    #[test]
    fn delete_checks_revision_and_local_recreation() {
        let mut f = Fixture::new();
        f.remote.edit_remotely("Note.txt", b"remote");
        let item = f.scan();
        let action = Action::DeleteRemote {
            key: item.key.clone(),
            rev: item.remote.as_ref().unwrap().rev.clone(),
        };
        f.remote.touch_remotely("Note.txt");
        assert_eq!(f.apply(action.clone(), &item).unwrap(), Outcome::Retry);
        f.write(b"recreated");
        assert_eq!(f.apply(action, &item).unwrap(), Outcome::Replan);
        assert_eq!(f.remote.write_count(), 0);
    }

    #[test]
    fn passive_actions_preserve_or_forget_base() {
        let mut f = Fixture::new();
        f.write(b"same");
        f.remote.edit_remotely("Note.txt", b"same");
        let item = f.scan();
        let key = item.key.clone();
        f.apply(Action::Adopt { key: key.clone() }, &item).unwrap();
        let original = f.state.clone();
        f.write(b"changed");
        assert_eq!(
            f.apply(Action::Adopt { key: key.clone() }, &item).unwrap(),
            Outcome::Replan
        );
        assert_eq!(
            f.apply(Action::Nothing { key: key.clone() }, &item)
                .unwrap(),
            Outcome::Done
        );
        assert_eq!(
            f.apply(
                Action::Skip {
                    key: key.clone(),
                    reason: SkipReason::TooLarge
                },
                &item,
            )
            .unwrap(),
            Outcome::Skipped(SkipReason::TooLarge)
        );
        assert_eq!(f.state, original);
        f.apply(Action::ForgetBase { key }, &item).unwrap();
        assert!(f.state.files.is_empty());
        assert_eq!(f.read("Note.txt"), b"changed");
        assert_eq!(f.remote.write_count(), 0);
    }

    #[test]
    fn upload_success_and_hash_mismatch() {
        let mut f = Fixture::new();
        f.write(b"local");
        let item = f.scan();
        let action = Action::Upload {
            key: item.key.clone(),
            expect_rev: None,
        };
        assert_eq!(f.apply(action.clone(), &item).unwrap(), Outcome::Done);
        assert_eq!(f.state.files[&item.key].hash, content_hash(b"local"));
        f.remote = FakeRemote::with_hasher(|_| "incorrect".into());
        let original = f.state.clone();
        assert!(matches!(
            f.apply(action, &item),
            Err(Error::Remote(RemoteError::Other(_)))
        ));
        assert_eq!(f.state, original);
    }

    #[test]
    fn conflict_names_match_library_date_and_suffixes() {
        use chrono::TimeZone;
        let dir = tempdir().unwrap();
        let now = Local.with_ymd_and_hms(2026, 9, 28, 21, 50, 0).unwrap();
        for suffix in ["", " 2", " 3"] {
            let expected = format!("Note (conflict 2026-09-28 2150{suffix}).txt");
            assert_eq!(conflict_name(dir.path(), "Note.txt", now), expected);
            fs::write(dir.path().join(expected), b"occupied").unwrap();
        }
    }

    #[test]
    fn read_stable_with_concurrent_append() {
        use std::sync::{Arc, Barrier};
        let dir = tempdir().unwrap();
        let path = dir.path().join("growing.txt");
        fs::write(&path, vec![b'x'; 1024 * 1024]).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let writer = {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut file = File::options().append(true).open(path).unwrap();
                barrier.wait();
                for _ in 0..1000 {
                    file.write_all(&[b'x'; 1024]).unwrap();
                }
            })
        };
        barrier.wait();
        match read_stable(&path) {
            Ok(bytes) => {
                assert!(bytes.len() >= 1024 * 1024);
                assert!(bytes.len() <= 1024 * 1024 + 1000 * 1024);
                assert!(bytes.iter().all(|byte| *byte == b'x'));
            }
            Err(Error::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
            Err(error) => panic!("unexpected error: {error}"),
        }
        writer.join().unwrap();
        assert_eq!(read_stable(&path).unwrap().len(), 1024 * 1024 + 1000 * 1024);
    }

    /// Coordinate each save with one exchange so that every acknowledged write
    /// is displaced by sync, rather than overwritten by the writer itself.
    #[test]
    fn concurrent_note_store_saves_displaced_by_download_survive() {
        use std::sync::mpsc;
        let mut f = Fixture::new();
        f.write(b"initial");
        f.remote.edit_remotely("Note.txt", b"initial remote");
        let (start_tx, start_rx) = mpsc::channel();
        let (saved_tx, saved_rx) = mpsc::channel();
        let dir = f.dir.path().to_owned();
        let trash = f.trash.path().to_owned();
        let writer = std::thread::spawn(move || {
            let mut store =
                NoteStore::open(&dir, "txt", Box::new(FakeTrasher { dir: trash })).unwrap();
            let path = dir.join("Note.txt");
            let id = store.id_for_path(&path).unwrap();
            let mut saved = Vec::new();
            for i in 0..50 {
                start_rx.recv().unwrap();
                let text = format!("writer {i}");
                match store.write(id, &text) {
                    Ok(()) => {}
                    Err(nv_core::Error::ExternalChange(_)) => {
                        store.reload(&path).unwrap();
                        store.write(id, &text).unwrap();
                    }
                    Err(error) => panic!("writer failed: {error}"),
                }
                saved.push(text);
                saved_tx.send(()).unwrap();
            }
            saved
        });
        for i in 0..50 {
            f.remote
                .edit_remotely("Note.txt", format!("remote {i}").as_bytes());
            let item = f.scan();
            start_tx.send(()).unwrap();
            saved_rx.recv().unwrap();
            assert!(matches!(
                f.download(&item).unwrap(),
                Outcome::ConflictCopy { .. }
            ));
        }
        let saved = writer.join().unwrap();
        let contents: Vec<Vec<u8>> = fs::read_dir(f.dir.path())
            .unwrap()
            .map(|entry| fs::read(entry.unwrap().path()).unwrap())
            .collect();
        for text in saved {
            assert!(
                contents.iter().any(|bytes| bytes == text.as_bytes()),
                "lost {text}"
            );
        }
    }
}
