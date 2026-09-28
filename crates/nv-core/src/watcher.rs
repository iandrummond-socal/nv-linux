use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use sha2::{Digest, Sha256};

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreEvent {
    Changed(PathBuf),
    Removed(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

type ExpectedWrites = Arc<Mutex<HashMap<PathBuf, [u8; 32]>>>;

pub struct FolderWatcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    expected: ExpectedWrites,
    dir: PathBuf,
}

impl FolderWatcher {
    /// Start watching, forwarding events through a standard-library channel.
    pub fn start(dir: impl AsRef<Path>, sender: Sender<StoreEvent>) -> Result<Self> {
        let dir = fs::canonicalize(dir)?;
        let expected = Arc::new(Mutex::new(HashMap::new()));
        let handler_expected = Arc::clone(&expected);
        let handler_dir = dir.clone();
        let mut debouncer = new_debouncer(
            Duration::from_millis(200),
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let mapped = events
                        .iter()
                        .flat_map(|event| map_event(event.kind, &event.paths, &handler_dir));
                    for event in prepare_batch(mapped, &handler_expected) {
                        if sender.send(event).is_err() {
                            break;
                        }
                    }
                }
                Err(errors) => {
                    for error in errors {
                        log::warn!("Folder watcher error: {error}");
                    }
                }
            },
        )
        .map_err(std::io::Error::other)?;
        debouncer
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(std::io::Error::other)?;
        Ok(Self {
            _debouncer: debouncer,
            expected,
            dir,
        })
    }

    /// Register an expected write. Relative paths are relative to the watched folder.
    pub fn expect_write(&self, path: &Path, hash: [u8; 32]) {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dir.join(path)
        };
        match self.expected.lock() {
            Ok(mut expected) => {
                expected.insert(path, hash);
            }
            Err(error) => log::warn!("Cannot register expected write: {error}"),
        }
    }
}

/// Classify a filename without accessing disk (removed paths must also be recognized).
pub fn is_note_path(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    if name.starts_with('.') || name == "4913" || name.ends_with('~') {
        return false;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["txt", "text", "utf8", "taskpaper", "md"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

// Scope both rename endpoints: a paired rename can cross the watched boundary.
fn map_event(kind: EventKind, paths: &[PathBuf], dir: &Path) -> Vec<StoreEvent> {
    let is_note = |path: &Path| path.parent() == Some(dir) && is_note_path(path);
    match kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            let [from, to] = paths else {
                return Vec::new();
            };
            match (is_note(from), is_note(to)) {
                (true, true) => vec![StoreEvent::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                }],
                (false, true) => vec![StoreEvent::Changed(to.clone())],
                (true, false) => vec![StoreEvent::Removed(from.clone())],
                (false, false) => Vec::new(),
            }
        }
        EventKind::Create(CreateKind::File | CreateKind::Any)
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => paths
            .iter()
            .filter(|path| is_note(path))
            .cloned()
            .map(StoreEvent::Changed)
            .collect(),
        EventKind::Remove(RemoveKind::File | RemoveKind::Any)
        | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => paths
            .iter()
            .filter(|path| is_note(path))
            .cloned()
            .map(StoreEvent::Removed)
            .collect(),
        _ => Vec::new(),
    }
}

fn prepare_batch(
    events: impl IntoIterator<Item = StoreEvent>,
    expected: &ExpectedWrites,
) -> Vec<StoreEvent> {
    let mut output = Vec::new();
    let mut previous = None;
    // A consumed expectation suppresses all matching Changed events in this batch.
    let mut suppressed = HashMap::new();
    for event in events {
        if previous.as_ref() == Some(&event) {
            continue;
        }
        previous = Some(event.clone());
        match &event {
            StoreEvent::Changed(path) => {
                match fs::metadata(path) {
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => continue,
                    Err(error) => {
                        log::warn!("Skipping changed note {}: {error}", path.display());
                        continue;
                    }
                }
                let mut expectations = match expected.lock() {
                    Ok(expectations) => expectations,
                    Err(error) => {
                        log::warn!("Cannot check expected writes: {error}");
                        output.push(event);
                        continue;
                    }
                };
                if let Some(hash) = expectations.get(path).or_else(|| suppressed.get(path)) {
                    let bytes = match fs::read(path) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            log::warn!("Skipping unreadable note {}: {error}", path.display());
                            continue;
                        }
                    };
                    let actual: [u8; 32] = Sha256::digest(&bytes).into();
                    if actual == *hash {
                        suppressed.insert(path.clone(), actual);
                        expectations.remove(path);
                        continue;
                    }
                    // A foreign write supersedes the pending expectation.
                    expectations.remove(path);
                    suppressed.remove(path);
                }
            }
            StoreEvent::Removed(path) => match fs::symlink_metadata(path) {
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    log::warn!("Cannot check removed note {}: {error}", path.display());
                    continue;
                }
            },
            StoreEvent::Renamed { to, .. } => match fs::metadata(to) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => continue,
                Err(error) => {
                    log::warn!("Skipping renamed note {}: {error}", to.display());
                    continue;
                }
            },
        }
        if output.last() != Some(&event) {
            output.push(event);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, MetadataKind};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::Instant;
    use tempfile::{tempdir, TempDir};

    fn fixture() -> (TempDir, PathBuf, FolderWatcher, Receiver<StoreEvent>) {
        let temp = tempdir().unwrap();
        let dir = fs::canonicalize(temp.path()).unwrap();
        let note = dir.join("note.txt");
        fs::write(&note, b"old").unwrap();
        let (sender, receiver) = mpsc::channel();
        let watcher = FolderWatcher::start(&dir, sender).unwrap();
        (temp, note, watcher, receiver)
    }

    // Wait up to three seconds for the first event, then drain until quiet.
    fn collect(receiver: &Receiver<StoreEvent>) -> Vec<StoreEvent> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let timeout = if events.is_empty() {
                remaining
            } else {
                remaining.min(Duration::from_millis(600))
            };
            match receiver.recv_timeout(timeout) {
                Ok(event) => events.push(event),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => panic!("watcher disconnected"),
            }
        }
        for event in &events {
            if let StoreEvent::Removed(path) = event {
                assert!(!path.exists(), "stale removal: {event:?}");
            }
        }
        events
    }

    #[test]
    fn note_filenames() {
        for name in ["a.txt", "a.TEXT", "a.UtF8", "a.taskpaper", "a.MD"] {
            assert!(is_note_path(Path::new(name)), "{name}");
        }
        for name in [".a.txt", "4913", "a.txt~", "a.pdf", "a", ".md"] {
            assert!(!is_note_path(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn pure_event_mapping() {
        let dir = Path::new("notes");
        let note = dir.join("a.txt");
        let other = dir.join("b.md");
        let ignored = dir.join(".save");
        let outside = PathBuf::from("elsewhere/a.txt");
        let both = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        for from in [&ignored, &outside] {
            assert_eq!(
                map_event(both, &[from.clone(), note.clone()], dir),
                vec![StoreEvent::Changed(note.clone())]
            );
            assert_eq!(
                map_event(both, &[note.clone(), from.clone()], dir),
                vec![StoreEvent::Removed(note.clone())]
            );
        }
        assert_eq!(
            map_event(both, &[note.clone(), other.clone()], dir),
            vec![StoreEvent::Renamed {
                from: note.clone(),
                to: other,
            }]
        );
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        ] {
            assert_eq!(
                map_event(kind, std::slice::from_ref(&note), dir),
                vec![StoreEvent::Changed(note.clone())]
            );
        }
        for kind in [
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        ] {
            assert_eq!(
                map_event(kind, std::slice::from_ref(&note), dir),
                vec![StoreEvent::Removed(note.clone())]
            );
        }
        for kind in [
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Create(CreateKind::Folder),
            EventKind::Remove(RemoveKind::Folder),
        ] {
            assert!(map_event(kind, std::slice::from_ref(&note), dir).is_empty());
        }
        assert!(map_event(both, &[], dir).is_empty());
    }

    #[test]
    fn suppression_and_deduplication() {
        let temp = tempdir().unwrap();
        let note = temp.path().join("a.txt");
        fs::write(&note, b"ours").unwrap();
        let expected = Arc::new(Mutex::new(HashMap::from([(
            note.clone(),
            Sha256::digest(b"ours").into(),
        )])));
        let changed = StoreEvent::Changed(note.clone());
        assert!(prepare_batch([changed.clone(), changed.clone()], &expected).is_empty());
        assert!(expected.lock().unwrap().is_empty());
        assert_eq!(
            prepare_batch([changed.clone(), changed.clone()], &expected),
            vec![changed.clone()]
        );
        expected
            .lock()
            .unwrap()
            .insert(note.clone(), Sha256::digest(b"ours").into());
        fs::write(&note, b"foreign").unwrap();
        assert_eq!(prepare_batch([changed.clone()], &expected), vec![changed]);
        assert!(expected.lock().unwrap().is_empty());
        assert!(prepare_batch([StoreEvent::Removed(note)], &expected).is_empty());
    }

    #[test]
    fn our_atomic_save_is_suppressed() {
        let (_temp, note, watcher, receiver) = fixture();
        watcher.expect_write(&note, Sha256::digest(b"ours").into());
        let pending = note.with_file_name(".pending");
        fs::write(&pending, b"ours").unwrap();
        fs::rename(pending, &note).unwrap();
        assert!(collect(&receiver).is_empty());
        assert!(watcher.expected.lock().unwrap().is_empty());
    }

    #[test]
    fn foreign_atomic_save_changes_note() {
        let (_temp, note, _watcher, receiver) = fixture();
        let pending = note.with_file_name(".pending");
        fs::write(&pending, b"foreign").unwrap();
        fs::rename(pending, &note).unwrap();
        assert!(collect(&receiver).contains(&StoreEvent::Changed(note)));
    }

    #[test]
    fn vim_style_save_changes_note() {
        let (_temp, note, _watcher, receiver) = fixture();
        let probe = note.with_file_name("4913");
        let backup = note.with_file_name("note.txt~");
        fs::write(&probe, b"probe").unwrap();
        fs::remove_file(probe).unwrap();
        fs::rename(&note, &backup).unwrap();
        fs::write(&note, b"vim").unwrap();
        fs::remove_file(backup).unwrap();
        let events = collect(&receiver);
        assert!(events.contains(&StoreEvent::Changed(note.clone())));
        assert_eq!(events.last(), Some(&StoreEvent::Changed(note)));
    }

    #[test]
    fn move_in_changes_note() {
        let (temp, note, _watcher, receiver) = fixture();
        let source_dir = tempfile::tempdir_in(temp.path().parent().unwrap()).unwrap();
        let source = source_dir.path().join("incoming.md");
        let destination = note.with_file_name("incoming.md");
        fs::write(&source, b"incoming").unwrap();
        fs::rename(source, &destination).unwrap();
        assert!(collect(&receiver).contains(&StoreEvent::Changed(destination)));
    }

    #[test]
    fn rename_is_forwarded() {
        let (_temp, note, _watcher, receiver) = fixture();
        let destination = note.with_file_name("renamed.md");
        fs::rename(&note, &destination).unwrap();
        assert!(collect(&receiver).contains(&StoreEvent::Renamed {
            from: note,
            to: destination,
        }));
    }

    #[test]
    fn delete_is_forwarded() {
        let (_temp, note, _watcher, receiver) = fixture();
        fs::remove_file(&note).unwrap();
        assert!(collect(&receiver).contains(&StoreEvent::Removed(note)));
    }
}
