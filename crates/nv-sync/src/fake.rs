//! In-memory remote for sync tests.
//!
//! Keys are `/` followed by the lowercase filename. Updates preserve the first
//! uploaded display name. Cursors encode an expiration generation and log position.
//! The default hash is a simple, non-cryptographic test hash, not Dropbox's content
//! hash. Use [`FakeRemote::with_hasher`] to inject the real content hash in tests.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::remote::{
    is_note_name, Cursor, Notifier, Remote, RemoteChange, RemoteError, RemoteFile, RemoteResult,
};

const SIZE_LIMIT: usize = 150 * 1024 * 1024;

/// Return the fake's Dropbox-style `path_lower` key.
pub fn key_for(name: &str) -> String {
    format!("/{}", name.to_lowercase())
}

fn simple_hash(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

struct StoredFile {
    metadata: RemoteFile,
    bytes: Vec<u8>,
    note: bool,
}

struct LogEntry {
    sequence: usize,
    key: String,
    change: Option<RemoteChange>,
}

/// A deterministic in-memory Dropbox substitute. External edits bypass faults,
/// revision checks, size limits, and the successful API write counter.
pub struct FakeRemote {
    files: BTreeMap<String, StoredFile>,
    log: Vec<LogEntry>,
    generation: u64,
    next_id: u64,
    revision: u64,
    hasher: fn(&[u8]) -> String,
    faults: VecDeque<RemoteError>,
    writes: usize,
    size_limit: usize,
}

impl Default for FakeRemote {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRemote {
    pub fn new() -> Self {
        Self::with_hasher(simple_hash)
    }

    /// Supply a hash function, such as `crate::content_hash::content_hash`.
    pub fn with_hasher(hasher: fn(&[u8]) -> String) -> Self {
        Self {
            files: BTreeMap::new(),
            log: Vec::new(),
            generation: 0,
            next_id: 0,
            revision: 0,
            hasher,
            faults: VecDeque::new(),
            writes: 0,
            size_limit: SIZE_LIMIT,
        }
    }

    pub fn key_for(name: &str) -> String {
        key_for(name)
    }

    /// Override the exclusive upload size limit for small test fixtures.
    pub fn set_size_limit(&mut self, bytes: usize) {
        self.size_limit = bytes;
    }

    pub fn expire_cursors(&mut self) {
        self.generation += 1;
    }

    /// Queue an error for the next `Remote` call. Notifier calls do not consume it.
    pub fn fail_next(&mut self, error: RemoteError) {
        self.faults.push_back(error);
    }

    pub fn edit_remotely(&mut self, name: &str, bytes: &[u8]) -> RemoteFile {
        self.store(name, bytes, is_note_name(name))
    }

    pub fn delete_remotely(&mut self, name: &str) -> bool {
        self.remove(&key_for(name))
    }

    /// Touching a missing file is a no-op.
    pub fn touch_remotely(&mut self, name: &str) {
        let key = key_for(name);
        if let Some(file) = self.files.get_mut(&key) {
            self.revision += 1;
            file.metadata.rev = format!("{:09x}", self.revision);
            let change = file
                .note
                .then(|| RemoteChange::Upsert(file.metadata.clone()));
            self.record(key, change);
        }
    }

    /// Store an ignored entry, even if its name looks like a note (e.g. a folder).
    pub fn add_non_note(&mut self, name: &str) {
        self.store(name, &[], false);
    }

    pub fn file(&self, name: &str) -> Option<RemoteFile> {
        self.files
            .get(&key_for(name))
            .map(|file| file.metadata.clone())
    }

    pub fn snapshot(&self) -> BTreeMap<String, (String, Vec<u8>)> {
        self.files
            .iter()
            .map(|(key, file)| {
                (
                    key.clone(),
                    (file.metadata.name.clone(), file.bytes.clone()),
                )
            })
            .collect()
    }

    pub fn write_count(&self) -> usize {
        self.writes
    }

    fn fault(&mut self) -> RemoteResult<()> {
        match self.faults.pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn cursor(&self) -> Cursor {
        Cursor(format!("fake:{}:{}", self.generation, self.log.len()))
    }

    fn position(&self, cursor: &Cursor) -> RemoteResult<usize> {
        let mut parts = cursor.0.split(':');
        let prefix = parts.next();
        let generation = parts.next().and_then(|part| part.parse::<u64>().ok());
        let sequence = parts.next().and_then(|part| part.parse::<usize>().ok());
        match (prefix, generation, sequence, parts.next()) {
            (Some("fake"), Some(generation), Some(sequence), None)
                if generation == self.generation
                    && sequence <= self.log.len()
                    && cursor.0 == format!("fake:{generation}:{sequence}") =>
            {
                Ok(sequence)
            }
            _ => Err(RemoteError::ResetCursor),
        }
    }

    fn record(&mut self, key: String, change: Option<RemoteChange>) {
        self.log.push(LogEntry {
            sequence: self.log.len() + 1,
            key,
            change,
        });
    }

    fn store(&mut self, name: &str, bytes: &[u8], note: bool) -> RemoteFile {
        let key = key_for(name);
        let (id, name) = match self.files.get(&key) {
            Some(file) => (file.metadata.id.clone(), file.metadata.name.clone()),
            None => {
                self.next_id += 1;
                (format!("id:{}", self.next_id), name.to_owned())
            }
        };
        self.revision += 1;
        let metadata = RemoteFile {
            id,
            name,
            key: key.clone(),
            rev: format!("{:09x}", self.revision),
            content_hash: (self.hasher)(bytes),
            size: bytes.len() as u64,
        };
        self.files.insert(
            key.clone(),
            StoredFile {
                metadata: metadata.clone(),
                bytes: bytes.to_vec(),
                note,
            },
        );
        self.record(key, note.then(|| RemoteChange::Upsert(metadata.clone())));
        metadata
    }

    fn remove(&mut self, key: &str) -> bool {
        if let Some(file) = self.files.remove(key) {
            self.record(
                key.to_owned(),
                file.note.then(|| RemoteChange::Deleted {
                    key: key.to_owned(),
                }),
            );
            true
        } else {
            false
        }
    }
}

impl Remote for FakeRemote {
    fn list(&mut self) -> RemoteResult<(Vec<RemoteFile>, Cursor)> {
        self.fault()?;
        let files = self
            .files
            .values()
            .filter(|file| file.note)
            .map(|file| file.metadata.clone())
            .collect();
        Ok((files, self.cursor()))
    }

    fn changes(&mut self, since: &Cursor) -> RemoteResult<(Vec<RemoteChange>, Cursor)> {
        self.fault()?;
        let position = self.position(since)?;
        let mut latest = BTreeMap::new();
        for entry in &self.log[position..] {
            debug_assert!(entry.sequence > position);
            latest.insert(entry.key.clone(), entry.change.clone());
        }
        Ok((latest.into_values().flatten().collect(), self.cursor()))
    }

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
        self.fault()?;
        self.files
            .values()
            .find(|file| file.metadata.id == id)
            .map(|file| file.bytes.clone())
            .ok_or_else(|| RemoteError::Other("not found".into()))
    }

    fn upload(
        &mut self,
        name: &str,
        bytes: &[u8],
        expect_rev: Option<&str>,
    ) -> RemoteResult<RemoteFile> {
        self.fault()?;
        if !is_note_name(name) {
            return Err(RemoteError::BadName);
        }
        if bytes.len() >= self.size_limit {
            return Err(RemoteError::TooLarge);
        }
        match (self.files.get(&key_for(name)), expect_rev) {
            (None, None) => {}
            (Some(file), Some(rev)) if file.metadata.rev == rev => {}
            _ => return Err(RemoteError::Conflict),
        }
        let file = self.store(name, bytes, true);
        self.writes += 1;
        Ok(file)
    }

    fn delete(&mut self, id: &str, parent_rev: &str) -> RemoteResult<()> {
        self.fault()?;
        let file = self
            .files
            .values()
            .find(|file| file.metadata.id == id)
            .ok_or_else(|| RemoteError::Other("not found".into()))?;
        if file.metadata.rev != parent_rev {
            return Err(RemoteError::Conflict);
        }
        let key = file.metadata.key.clone();
        self.remove(&key);
        self.writes += 1;
        Ok(())
    }
}

impl Notifier for FakeRemote {
    fn wait(&mut self, since: &Cursor, _timeout: Duration) -> RemoteResult<bool> {
        Ok(self.log.len() > self.position(since)?)
    }
}

/// Cloneable handles sharing one fake, for an engine and another-device test driver.
#[derive(Clone, Default)]
pub struct SharedFake(Arc<Mutex<FakeRemote>>);

impl SharedFake {
    pub fn new(fake: FakeRemote) -> Self {
        Self(Arc::new(Mutex::new(fake)))
    }

    /// Access the fake while holding its lock. Do not re-enter another handle here.
    pub fn with<T>(&self, f: impl FnOnce(&mut FakeRemote) -> T) -> T {
        f(&mut self.0.lock().expect("fake remote mutex poisoned"))
    }
}

impl From<FakeRemote> for SharedFake {
    fn from(fake: FakeRemote) -> Self {
        Self::new(fake)
    }
}

impl Remote for SharedFake {
    fn list(&mut self) -> RemoteResult<(Vec<RemoteFile>, Cursor)> {
        self.with(Remote::list)
    }

    fn changes(&mut self, since: &Cursor) -> RemoteResult<(Vec<RemoteChange>, Cursor)> {
        self.with(|fake| fake.changes(since))
    }

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
        self.with(|fake| fake.download(id))
    }

    fn upload(
        &mut self,
        name: &str,
        bytes: &[u8],
        expect_rev: Option<&str>,
    ) -> RemoteResult<RemoteFile> {
        self.with(|fake| fake.upload(name, bytes, expect_rev))
    }

    fn delete(&mut self, id: &str, parent_rev: &str) -> RemoteResult<()> {
        self.with(|fake| fake.delete(id, parent_rev))
    }
}

impl Notifier for SharedFake {
    fn wait(&mut self, since: &Cursor, timeout: Duration) -> RemoteResult<bool> {
        self.with(|fake| fake.wait(since, timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_update_and_case_insensitive_conflicts() {
        let mut fake = FakeRemote::new();
        let first = fake.upload("Todo.txt", b"one", None).unwrap();
        assert_eq!(first.key, "/todo.txt");
        for name in ["Todo.txt", "todo.txt", "TODO.TXT"] {
            assert_eq!(fake.upload(name, b"two", None), Err(RemoteError::Conflict));
        }
        let second = fake.upload("todo.txt", b"two", Some(&first.rev)).unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.name, "Todo.txt");
        assert!(second.rev > first.rev);
        assert_eq!(fake.download(&second.id).unwrap(), b"two");
        assert_eq!(
            fake.upload("Todo.txt", b"stale", Some(&first.rev)),
            Err(RemoteError::Conflict)
        );
        assert_eq!(
            fake.upload("missing.txt", b"x", Some(&first.rev)),
            Err(RemoteError::Conflict)
        );
        assert_eq!(fake.file("TODO.txt"), Some(second));
        assert_eq!(fake.snapshot().len(), 1);
        assert_eq!(fake.write_count(), 2);
    }

    #[test]
    fn cursor_deltas_collapse_and_delete() {
        let mut fake = FakeRemote::new();
        fake.edit_remotely("old.txt", b"old");
        let (_, cursor) = fake.list().unwrap();
        fake.edit_remotely("new.txt", b"one");
        let latest = fake.edit_remotely("new.txt", b"two");
        fake.edit_remotely("gone.txt", b"gone");
        assert!(fake.delete_remotely("GONE.txt"));
        assert!(!fake.delete_remotely("gone.txt"));
        let (changes, next) = fake.changes(&cursor).unwrap();
        assert_eq!(
            changes,
            vec![
                RemoteChange::Deleted {
                    key: key_for("gone.txt")
                },
                RemoteChange::Upsert(latest)
            ]
        );
        assert!(fake.changes(&next).unwrap().0.is_empty());
        assert_eq!(fake.write_count(), 0);
    }

    #[test]
    fn expired_and_invalid_cursors() {
        let mut fake = FakeRemote::new();
        let (_, old) = fake.list().unwrap();
        fake.expire_cursors();
        assert_eq!(fake.changes(&old), Err(RemoteError::ResetCursor));
        assert_eq!(
            fake.wait(&old, Duration::ZERO),
            Err(RemoteError::ResetCursor)
        );
        for value in ["", "fake:0", "fake:1:999", "fake:1:0:extra", "fake:1:+0"] {
            assert_eq!(
                fake.changes(&Cursor(value.into())),
                Err(RemoteError::ResetCursor)
            );
        }
        let (_, fresh) = fake.list().unwrap();
        assert!(fake.changes(&fresh).unwrap().0.is_empty());
    }

    #[test]
    fn non_notes_are_ignored() {
        let mut fake = FakeRemote::new();
        let (_, cursor) = fake.list().unwrap();
        fake.add_non_note("image.jpg");
        fake.add_non_note("folder.txt");
        fake.touch_remotely("folder.txt");
        fake.edit_remotely("another.jpg", b"image");
        fake.delete_remotely("image.jpg");
        assert!(fake.list().unwrap().0.is_empty());
        assert!(fake.changes(&cursor).unwrap().0.is_empty());
        assert!(fake.file("another.jpg").is_some());
        assert_eq!(fake.upload("bad.jpg", b"", None), Err(RemoteError::BadName));
    }

    #[test]
    fn faults_are_fifo_and_consumed_once() {
        let mut fake = FakeRemote::new();
        let (_, cursor) = fake.list().unwrap();
        fake.fail_next(RemoteError::Offline);
        fake.fail_next(RemoteError::Unauthorized);
        assert!(!fake.wait(&cursor, Duration::ZERO).unwrap());
        assert_eq!(fake.download("missing"), Err(RemoteError::Offline));
        assert_eq!(
            fake.upload("note.txt", b"x", None),
            Err(RemoteError::Unauthorized)
        );
        assert!(fake.list().unwrap().0.is_empty());
        assert!(fake.upload("note.txt", b"x", None).is_ok());
    }

    #[test]
    fn delete_checks_parent_revision() {
        let mut fake = FakeRemote::new();
        let file = fake.upload("a.txt", b"a", None).unwrap();
        let (_, cursor) = fake.list().unwrap();
        assert_eq!(fake.delete(&file.id, "wrong"), Err(RemoteError::Conflict));
        assert_eq!(fake.write_count(), 1);
        fake.delete(&file.id, &file.rev).unwrap();
        assert_eq!(fake.write_count(), 2);
        assert_eq!(
            fake.changes(&cursor).unwrap().0,
            vec![RemoteChange::Deleted { key: file.key }]
        );
        assert_eq!(
            fake.delete(&file.id, &file.rev),
            Err(RemoteError::Other("not found".into()))
        );
        assert_eq!(
            fake.download(&file.id),
            Err(RemoteError::Other("not found".into()))
        );
    }

    #[test]
    fn upload_size_limit_is_exclusive() {
        let mut fake = FakeRemote::new();
        assert_eq!(fake.size_limit, 150 * 1024 * 1024);
        fake.set_size_limit(3);
        assert_eq!(
            fake.upload("a.txt", b"123", None),
            Err(RemoteError::TooLarge)
        );
        let file = fake.upload("a.txt", b"12", None).unwrap();
        assert_eq!(
            fake.upload("a.txt", b"1234", Some(&file.rev)),
            Err(RemoteError::TooLarge)
        );
        assert_eq!(fake.file("a.txt"), Some(file));
        assert_eq!(fake.write_count(), 1);
    }

    #[test]
    fn shared_handles_touch_and_custom_hash() {
        let mut engine = SharedFake::new(FakeRemote::with_hasher(|_| "custom".into()));
        let device = engine.clone();
        let file = engine.upload("a.txt", b"body", None).unwrap();
        assert_eq!(file.content_hash, "custom");
        let (_, cursor) = engine.list().unwrap();
        assert!(!engine.wait(&cursor, Duration::ZERO).unwrap());
        device.with(|fake| fake.touch_remotely("A.txt"));
        assert!(engine.wait(&cursor, Duration::ZERO).unwrap());
        let touched = device.with(|fake| fake.file("a.txt").unwrap());
        assert_ne!(touched.rev, file.rev);
        assert_eq!(touched.content_hash, file.content_hash);
        assert_eq!(engine.download(&file.id).unwrap(), b"body");
        assert_eq!(
            engine.changes(&cursor).unwrap().0,
            vec![RemoteChange::Upsert(touched)]
        );
        assert_eq!(device.with(|fake| fake.write_count()), 1);
    }
}
