use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::model::{Note, NoteId};
use crate::{Error, Result};

pub trait Trasher {
    fn trash(&self, path: &Path) -> Result<()>;
}

pub struct SystemTrasher;

impl Trasher for SystemTrasher {
    fn trash(&self, path: &Path) -> Result<()> {
        trash::delete(path).map_err(|error| Error::Trash(error.to_string()))
    }
}

/// An isolated trash backend suitable for integration tests and local fixtures.
pub struct FakeTrasher {
    pub dir: PathBuf,
}

impl Trasher for FakeTrasher {
    fn trash(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let name = path
            .file_name()
            .ok_or_else(|| Error::Trash(format!("No filename in {}", path.display())))?;
        let mut destination = self.dir.join(name);
        let mut suffix = 2_u64;
        while fs::symlink_metadata(&destination).is_ok() {
            let stem = path.file_stem().unwrap_or(name).to_string_lossy();
            let mut name = format!("{stem} {suffix}");
            if let Some(extension) = path.extension() {
                name.push('.');
                name.push_str(&extension.to_string_lossy());
            }
            destination = self.dir.join(name);
            suffix += 1;
        }
        fs::rename(path, destination)?;
        Ok(())
    }
}

/// Replace slashes, strip leading dots, trim, and cap at 200 UTF-8 bytes.
/// In particular, `.txt` becomes `txt`, not an empty title. Whitespace before
/// leading dots is also removed so the resulting file is never hidden.
pub fn sanitize_title(title: &str) -> String {
    let replaced = title.replace('/', "-");
    let cleaned = replaced.trim().trim_start_matches('.').trim();
    let mut end = cleaned.len().min(200);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let title = cleaned[..end].trim();
    if title.is_empty() {
        "Untitled Note".to_owned()
    } else {
        title.to_owned()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    modified: SystemTime,
    size: u64,
    hash: [u8; 32],
}

struct Entry {
    note: Note,
    fingerprint: Fingerprint,
}

pub struct NoteStore {
    dir: PathBuf,
    ext: String,
    trasher: Box<dyn Trasher>,
    entries: BTreeMap<NoteId, Entry>,
    next_id: NoteId,
}

fn accepted_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.starts_with('.')
        && name != "4913"
        && !name.ends_with('~')
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "txt" | "text" | "utf8" | "taskpaper" | "md"
                )
            })
}

fn title_from_path(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn snapshot(path: &Path) -> Result<(Vec<u8>, Fingerprint)> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Not a regular note file").into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let modified = after.modified()?;
    if before.modified()? != modified
        || before.len() != after.len()
        || after.len() != bytes.len() as u64
    {
        return Err(Error::ExternalChange(path.to_owned()));
    }
    let hash = Sha256::digest(&bytes).into();
    Ok((
        bytes,
        Fingerprint {
            modified,
            size: after.len(),
            hash,
        },
    ))
}

impl NoteStore {
    /// Open a directory with a preferred creation extension and an owned trash backend.
    pub fn open(dir: impl AsRef<Path>, ext: &str, trasher: Box<dyn Trasher>) -> Result<Self> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        if !matches!(ext.as_str(), "txt" | "text" | "utf8" | "taskpaper" | "md") {
            return Err(Error::Config(format!("Unsupported note extension: {ext}")));
        }
        fs::create_dir_all(dir.as_ref())?;
        let dir = fs::canonicalize(dir)?;
        let mut store = Self {
            dir,
            ext,
            trasher,
            entries: BTreeMap::new(),
            next_id: 1,
        };
        for entry in fs::read_dir(&store.dir)? {
            match entry {
                Ok(entry) if accepted_path(&entry.path()) => {
                    if let Err(error) = store.reload(&entry.path()) {
                        log::warn!("Skipping note {}: {error}", entry.path().display());
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("Skipping unreadable directory entry: {error}"),
            }
        }
        Ok(store)
    }

    fn entry(&self, id: NoteId) -> Result<&Entry> {
        self.entries.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("Unknown note id {id}")).into()
        })
    }

    fn available_path(&self, title: &str, ext: &str, own_id: Option<NoteId>) -> Result<PathBuf> {
        let base = sanitize_title(title);
        let own_path = own_id.and_then(|id| self.get(id)).map(|note| &note.path);
        let disk_paths = fs::read_dir(&self.dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        for number in 1_u64.. {
            let title = if number == 1 {
                base.clone()
            } else {
                format!("{base} {number}")
            };
            let folded = title.to_lowercase();
            let path = self.dir.join(format!("{title}.{ext}"));
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            let cached_collision = self.entries.values().any(|entry| {
                Some(entry.note.id) != own_id && entry.note.title.to_lowercase() == folded
            });
            let disk_collision = disk_paths.iter().any(|other| {
                Some(other) != own_path
                    && (other
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                        == filename
                        || (accepted_path(other)
                            && title_from_path(other).to_lowercase() == folded))
            });
            if !cached_collision && !disk_collision {
                return Ok(path);
            }
        }
        Err(Error::NameCollision(base))
    }

    /// Create a note and return its stable session identifier.
    pub fn create(&mut self, title: &str, body: &str) -> Result<NoteId> {
        let path = self.available_path(title, &self.ext, None)?;
        let mut temp = NamedTempFile::new_in(&self.dir)?;
        temp.write_all(body.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(&path)
            .map_err(|error| Error::Io(error.error))?;
        File::open(&self.dir)?.sync_all()?;
        self.reload(&path)
    }

    fn check_disk(&self, id: NoteId) -> Result<()> {
        let entry = self.entry(id)?;
        match snapshot(&entry.note.path) {
            Ok((_, current)) if current == entry.fingerprint => Ok(()),
            Ok(_) => Err(Error::ExternalChange(entry.note.path.clone())),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                Err(Error::ExternalChange(entry.note.path.clone()))
            }
            Err(error) => Err(error),
        }
    }

    pub fn write(&mut self, id: NoteId, body: &str) -> Result<()> {
        let note = &self.entry(id)?.note;
        if note.read_only {
            return Err(Error::ReadOnly(note.path.clone()));
        }
        let path = note.path.clone();
        self.check_disk(id)?;
        let target = fs::canonicalize(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Error::ExternalChange(path.clone())
            } else {
                error.into()
            }
        })?;
        let dir = target
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Note has no parent"))?;
        let permissions = fs::metadata(&target)?.permissions();
        let mut temp = NamedTempFile::new_in(dir)?;
        temp.as_file().set_permissions(permissions)?;
        temp.write_all(body.as_bytes())?;
        temp.as_file().sync_all()?;
        let metadata = temp.as_file().metadata()?;
        let fingerprint = Fingerprint {
            modified: metadata.modified()?,
            size: metadata.len(),
            hash: Sha256::digest(body.as_bytes()).into(),
        };
        self.check_disk(id)?;
        let current_target = fs::canonicalize(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Error::ExternalChange(path.clone())
            } else {
                error.into()
            }
        })?;
        if current_target != target {
            return Err(Error::ExternalChange(path));
        }
        temp.persist(&target)
            .map_err(|error| Error::Io(error.error))?;
        // Cache exactly our write, even if another process edits immediately afterwards.
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.note.body = body.to_owned();
            entry.note.modified = fingerprint.modified;
            entry.fingerprint = fingerprint;
        }
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    pub fn rename(&mut self, id: NoteId, new_title: &str) -> Result<()> {
        let old = self.entry(id)?.note.path.clone();
        let ext = old
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or(&self.ext);
        let new = self.available_path(new_title, ext, Some(id))?;
        if new != old {
            // On Linux rename also handles case-only changes without an intermediate name.
            fs::rename(&old, &new)?;
            self.set_path(id, new);
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }

    pub fn trash(&mut self, id: NoteId) -> Result<()> {
        self.trasher.trash(&self.entry(id)?.note.path)?;
        self.forget(id);
        Ok(())
    }

    /// Remove a cached note without touching the filesystem.
    pub fn forget(&mut self, id: NoteId) -> Option<Note> {
        self.entries.remove(&id).map(|entry| entry.note)
    }

    /// Load or refresh a path, preserving the identifier of an existing note.
    /// Invalid or unreadable files return an error; directory loading logs and skips them.
    pub fn reload(&mut self, path: &Path) -> Result<NoteId> {
        let path = self.absolute_path(path);
        if !accepted_path(&path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Not a supported note filename",
            )
            .into());
        }
        let (bytes, fingerprint) = snapshot(&path)?;
        let text = String::from_utf8_lossy(&bytes);
        let read_only = matches!(text, std::borrow::Cow::Owned(_));
        let body = text.into_owned();
        let id = match self.id_for_path(&path) {
            Some(id) => id,
            None => {
                let id = self.next_id;
                self.next_id = id
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("Note ids exhausted"))?;
                id
            }
        };
        let note = Note {
            id,
            title: title_from_path(&path),
            body,
            path,
            modified: fingerprint.modified,
            labels: String::new(),
            read_only,
        };
        self.entries.insert(id, Entry { note, fingerprint });
        Ok(id)
    }

    fn absolute_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_owned()
        } else {
            self.dir.join(path)
        }
    }

    pub fn id_for_path(&self, path: &Path) -> Option<NoteId> {
        let path = self.absolute_path(path);
        self.entries
            .values()
            .find(|entry| entry.note.path == path)
            .map(|entry| entry.note.id)
    }

    /// Follow an external rename without changing the last known content fingerprint.
    pub fn set_path(&mut self, id: NoteId, path: PathBuf) {
        let path = self.absolute_path(&path);
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.note.title = title_from_path(&path);
            entry.note.path = path;
        }
    }

    pub fn fingerprint(&self, id: NoteId) -> Option<[u8; 32]> {
        self.entries.get(&id).map(|entry| entry.fingerprint.hash)
    }

    pub fn get(&self, id: NoteId) -> Option<&Note> {
        self.entries.get(&id).map(|entry| &entry.note)
    }

    /// Borrow all cached notes; callers choose their display order.
    pub fn all(&self) -> Vec<&Note> {
        self.entries.values().map(|entry| &entry.note).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{tempdir, TempDir};

    fn store(root: &TempDir) -> NoteStore {
        NoteStore::open(
            root.path().join("notes"),
            "txt",
            Box::new(FakeTrasher {
                dir: root.path().join("trash"),
            }),
        )
        .unwrap()
    }

    #[test]
    fn sanitizing_table_and_unicode_limit() {
        for (input, expected) in [
            ("a/b", "a-b"),
            ("...hello", "hello"),
            (".txt", "txt"),
            ("  title  ", "title"),
            ("", "Untitled Note"),
            ("...", "Untitled Note"),
            ("  .hidden", "hidden"),
        ] {
            assert_eq!(sanitize_title(input), expected);
        }
        assert_eq!(sanitize_title(&"a".repeat(201)).len(), 200);
        assert_eq!(sanitize_title(&"界".repeat(100)), "界".repeat(66));
    }

    #[test]
    fn cold_start_missing_empty_and_populated() {
        let root = tempdir().unwrap();
        assert!(store(&root).all().is_empty());
        assert!(root.path().join("notes").is_dir());
        assert!(store(&root).all().is_empty());
        let dir = root.path().join("notes");
        for name in [
            "one.txt",
            "two.TEXT",
            "three.utf8",
            "four.taskpaper",
            "five.MD",
        ] {
            fs::write(dir.join(name), "body").unwrap();
        }
        for name in [
            ".hidden.txt",
            ".tmp123.txt",
            "4913",
            "backup.txt~",
            "ignored.rs",
        ] {
            fs::write(dir.join(name), "ignored").unwrap();
        }
        fs::create_dir(dir.join("directory.txt")).unwrap();
        let loaded = store(&root);
        assert_eq!(loaded.all().len(), 5);
        assert!(loaded.all().iter().all(|note| note.body == "body"));
    }

    #[test]
    fn creation_extension_collisions_and_case_only_rename() {
        let root = tempdir().unwrap();
        let mut notes = store(&root);
        let first = notes.create("Title", "one").unwrap();
        let second = notes.create("title", "two").unwrap();
        assert_eq!(notes.get(second).unwrap().title, "title 2");
        notes.rename(first, "TITLE").unwrap();
        assert_eq!(notes.get(first).unwrap().title, "TITLE");
        notes.rename(second, "title").unwrap();
        assert_eq!(notes.get(second).unwrap().title, "title 2");
        assert_eq!(
            fs::read_to_string(notes.get(first).unwrap().path.clone()).unwrap(),
            "one"
        );
        let mut markdown = NoteStore::open(
            root.path().join("notes"),
            "md",
            Box::new(FakeTrasher {
                dir: root.path().join("trash"),
            }),
        )
        .unwrap();
        let third = markdown.create("title", "three").unwrap();
        assert_eq!(
            markdown.get(third).unwrap().path.file_name().unwrap(),
            "title 3.md"
        );
    }

    #[test]
    fn checked_write_and_no_temporary_files() {
        let root = tempdir().unwrap();
        let mut notes = store(&root);
        let id = notes.create("note", "old").unwrap();
        let old_hash = notes.fingerprint(id).unwrap();
        notes.write(id, "new").unwrap();
        assert_ne!(notes.fingerprint(id).unwrap(), old_hash);
        let path = notes.get(id).unwrap().path.clone();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        // Preserve length and mtime so only the content hash detects this edit.
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "EXT").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(matches!(
            notes.write(id, "overwrite"),
            Err(Error::ExternalChange(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "EXT");
        assert_eq!(notes.get(id).unwrap().body, "new");
        assert_eq!(notes.reload(&path).unwrap(), id);
        notes.write(id, "accepted after reload").unwrap();
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            notes.write(id, "missing"),
            Err(Error::ExternalChange(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_mode_and_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempdir().unwrap();
        let mut notes = store(&root);
        let target_dir = root.path().join("targets");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("original");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let link = root.path().join("notes/link.txt");
        symlink(&target, &link).unwrap();
        let id = notes.reload(&link).unwrap();
        notes.write(id, "new").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_dir(&target_dir).unwrap().count(), 1);
        assert_eq!(fs::read_dir(root.path().join("notes")).unwrap().count(), 1);
    }

    #[test]
    fn lossy_note_is_read_only_and_bytes_survive() {
        let root = tempdir().unwrap();
        let mut notes = store(&root);
        let path = root.path().join("notes/bad.txt");
        let bytes = [b'a', 0xff, b'b'];
        fs::write(&path, bytes).unwrap();
        let id = notes.reload(&path).unwrap();
        let note = notes.get(id).unwrap();
        assert!(note.read_only);
        assert_eq!(note.body, "a\u{fffd}b");
        assert!(matches!(
            notes.write(id, "replacement"),
            Err(Error::ReadOnly(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn fake_trash_moves_and_avoids_collisions() {
        let root = tempdir().unwrap();
        let mut notes = store(&root);
        for body in ["one", "two", "three"] {
            let id = notes.create("note", body).unwrap();
            let path = notes.get(id).unwrap().path.clone();
            notes.trash(id).unwrap();
            assert!(!path.exists());
            assert!(notes.get(id).is_none());
            assert!(notes.fingerprint(id).is_none());
        }
        for (name, body) in [
            ("note.txt", "one"),
            ("note 2.txt", "two"),
            ("note 3.txt", "three"),
        ] {
            assert_eq!(
                fs::read_to_string(root.path().join("trash").join(name)).unwrap(),
                body
            );
        }
    }

    #[test]
    fn failed_trash_preserves_file_and_cache() {
        struct FailingTrasher;
        impl Trasher for FailingTrasher {
            fn trash(&self, _path: &Path) -> Result<()> {
                Err(Error::Trash("intentional failure".to_owned()))
            }
        }
        let root = tempdir().unwrap();
        let mut notes = NoteStore::open(root.path(), "txt", Box::new(FailingTrasher)).unwrap();
        let id = notes.create("note", "keep me").unwrap();
        assert!(matches!(notes.trash(id), Err(Error::Trash(_))));
        assert_eq!(
            fs::read_to_string(&notes.get(id).unwrap().path).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn external_rename_reload_forget_and_monotonic_ids() {
        let root = tempdir().unwrap();
        let mut notes = store(&root);
        let id = notes.create("before", "body").unwrap();
        let old = notes.get(id).unwrap().path.clone();
        let new = old.with_file_name("after.txt");
        fs::rename(&old, &new).unwrap();
        let hash = notes.fingerprint(id);
        notes.set_path(id, new.clone());
        assert_eq!(notes.id_for_path(&new), Some(id));
        assert_eq!(notes.id_for_path(&old), None);
        assert_eq!(notes.get(id).unwrap().title, "after");
        assert_eq!(notes.fingerprint(id), hash);
        assert_eq!(notes.reload(&new).unwrap(), id);
        assert_eq!(notes.forget(id).unwrap().id, id);
        assert!(new.exists());
        assert!(notes.get(id).is_none());
        assert!(notes.reload(&new).unwrap() > id);
    }
}
