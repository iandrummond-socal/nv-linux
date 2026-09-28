use std::collections::{HashMap, HashSet};
use std::io;

use crate::config::SortOrder;
use crate::model::{Note, NoteId, SearchFields};
use crate::search::SearchState;
use crate::storage::NoteStore;
use crate::watcher::StoreEvent;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Conflict { copy_id: NoteId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Updated(NoteId),
    Removed(NoteId),
    Renamed(NoteId),
    Added(NoteId),
    None,
}

pub struct Library {
    store: NoteStore,
    search: SearchState,
    sort: SortOrder,
    fields: HashMap<NoteId, SearchFields>,
    deleted_on_disk: HashSet<NoteId>,
    dirty: bool,
}

impl Library {
    /// Combine an opened store with the configured display order.
    pub fn new(store: NoteStore, sort: SortOrder) -> Self {
        let fields = store
            .all()
            .into_iter()
            .map(|note| (note.id, note.search_fields()))
            .collect();
        Self {
            store,
            search: SearchState::new(),
            sort,
            fields,
            deleted_on_disk: HashSet::new(),
            dirty: true,
        }
    }

    pub fn search(&mut self, query: &str) -> Vec<NoteId> {
        let mut notes: Vec<_> = self
            .store
            .all()
            .into_iter()
            .filter_map(|note| self.fields.get(&note.id).map(|fields| (note, fields)))
            .collect();
        notes.sort_by(|(left, left_fields), (right, right_fields)| {
            let title_order = left_fields
                .title
                .cmp(&right_fields.title)
                .then_with(|| left.id.cmp(&right.id));
            match self.sort {
                SortOrder::Modified => right.modified.cmp(&left.modified).then(title_order),
                SortOrder::Title => title_order,
            }
        });
        let fields: Vec<_> = notes
            .iter()
            .map(|(note, fields)| (note.id, *fields))
            .collect();
        let result = self.search.filter(query, &fields, self.dirty);
        self.dirty = false;
        result
    }

    /// Choose a completion from filtered identifiers in display order.
    pub fn autocomplete(&self, query: &str, filtered: &[NoteId]) -> Option<NoteId> {
        let fields: Vec<_> = filtered
            .iter()
            .filter_map(|id| self.fields.get(id).map(|fields| (*id, fields)))
            .collect();
        self.search.autocomplete(query, &fields)
    }

    pub fn create(&mut self, title: &str, body: &str) -> Result<NoteId> {
        let id = self.store.create(title, body)?;
        self.refresh(id);
        Ok(id)
    }

    pub fn save(&mut self, id: NoteId, body: &str) -> Result<SaveOutcome> {
        self.require_present(id)?;
        if self.note(id)?.body == body {
            return Ok(SaveOutcome::Saved);
        }
        let result = self.store.write(id, body);
        // A directory sync can fail after the store has committed the write.
        self.refresh(id);
        match result {
            Ok(()) => Ok(SaveOutcome::Saved),
            Err(Error::ExternalChange(path)) => {
                if !path.try_exists()? {
                    self.deleted_on_disk.insert(id);
                    return Err(Error::ExternalChange(path));
                }
                let title = format!(
                    "{} (conflict {})",
                    self.note(id)?.title,
                    chrono::Local::now().format("%Y-%m-%d %H%M")
                );
                let copy_id = self.create(&title, body)?;
                self.store.reload(&path)?;
                self.deleted_on_disk.remove(&id);
                self.refresh(id);
                Ok(SaveOutcome::Conflict { copy_id })
            }
            Err(error) => Err(error),
        }
    }

    pub fn rename(&mut self, id: NoteId, new_title: &str) -> Result<()> {
        self.require_present(id)?;
        let result = self.store.rename(id, new_title);
        self.refresh(id);
        result
    }

    pub fn trash(&mut self, id: NoteId) -> Result<()> {
        if !self.is_deleted_on_disk(id) {
            self.store.trash(id)?;
        }
        self.forget(id);
        Ok(())
    }

    /// Re-create the retained title with the supplied text and a new identifier.
    /// The old note is forgotten only after creation succeeds.
    pub fn restore(&mut self, id: NoteId, body: &str) -> Result<NoteId> {
        let new_id = self.store.create_replacing(id, body)?;
        self.deleted_on_disk.remove(&id);
        self.deleted_on_disk.remove(&new_id);
        self.refresh(id);
        self.refresh(new_id);
        Ok(new_id)
    }

    /// Apply a filesystem event, logging failures and returning the resulting change.
    ///
    /// A caller holding unsaved edits must call `save` BEFORE applying a Changed
    /// event for that note, so the edits take the conflict path instead of being lost.
    pub fn apply_external(&mut self, event: StoreEvent) -> Change {
        match event {
            StoreEvent::Changed(path) => {
                let known = self.store.id_for_path(&path).is_some();
                match self.store.reload(&path) {
                    Ok(id) => {
                        self.deleted_on_disk.remove(&id);
                        self.refresh(id);
                        if known {
                            Change::Updated(id)
                        } else {
                            Change::Added(id)
                        }
                    }
                    Err(error) => {
                        log::warn!("Cannot reload note {}: {error}", path.display());
                        Change::None
                    }
                }
            }
            StoreEvent::Removed(path) => match self.store.id_for_path(&path) {
                Some(id) => {
                    self.deleted_on_disk.insert(id);
                    Change::Removed(id)
                }
                None => Change::None,
            },
            StoreEvent::Renamed { from, to } => {
                let Some(id) = self.store.id_for_path(&from) else {
                    return self.apply_external(StoreEvent::Changed(to));
                };
                self.store.set_path(id, to.clone());
                self.refresh(id);
                match self.store.reload(&to) {
                    Ok(_) => {
                        self.deleted_on_disk.remove(&id);
                        self.refresh(id);
                        Change::Renamed(id)
                    }
                    Err(error) => {
                        log::warn!("Cannot reload renamed note {}: {error}", to.display());
                        Change::None
                    }
                }
            }
        }
    }

    /// Drop a cached note without touching its file.
    pub fn forget(&mut self, id: NoteId) {
        let _ = self.store.forget(id);
        self.fields.remove(&id);
        self.deleted_on_disk.remove(&id);
        self.dirty = true;
    }

    pub fn get(&self, id: NoteId) -> Option<&Note> {
        self.store.get(id)
    }

    pub fn all(&self) -> Vec<&Note> {
        self.store.all()
    }

    /// Report whether autosave must stop because the note was removed externally.
    pub fn is_deleted_on_disk(&self, id: NoteId) -> bool {
        self.deleted_on_disk.contains(&id)
    }

    /// Last loaded or written hash, suitable for `FolderWatcher::expect_write`.
    pub fn fingerprint(&self, id: NoteId) -> Option<[u8; 32]> {
        self.store.fingerprint(id)
    }

    fn note(&self, id: NoteId) -> Result<&Note> {
        self.get(id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("Unknown note id {id}")).into()
        })
    }

    fn require_present(&self, id: NoteId) -> Result<()> {
        let note = self.note(id)?;
        if self.is_deleted_on_disk(id) {
            return Err(Error::ExternalChange(note.path.clone()));
        }
        Ok(())
    }

    fn refresh(&mut self, id: NoteId) {
        if let Some(note) = self.store.get(id) {
            self.fields.insert(id, note.search_fields());
        } else {
            self.fields.remove(&id);
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, FileTimes};
    use std::time::{Duration, SystemTime};

    use tempfile::{tempdir, TempDir};

    use super::*;
    use crate::storage::FakeTrasher;

    fn library(root: &TempDir, sort: SortOrder) -> Library {
        Library::new(
            NoteStore::open(
                root.path().join("notes"),
                "txt",
                Box::new(FakeTrasher {
                    dir: root.path().join("trash"),
                }),
            )
            .unwrap(),
            sort,
        )
    }

    #[test]
    fn sorts_by_title_or_modified_with_deterministic_ties() {
        let root = tempdir().unwrap();
        let dir = root.path().join("notes");
        fs::create_dir(&dir).unwrap();
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for (name, modified) in [
            ("zebra", earlier + Duration::from_secs(10)),
            ("beta", earlier),
            ("Alpha", earlier),
            ("alpha", earlier),
        ] {
            let path = dir.join(format!("{name}.txt"));
            fs::write(&path, "matching").unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(modified))
                .unwrap();
        }
        let mut lib = library(&root, SortOrder::Title);
        let mut alphas: Vec<_> = lib
            .all()
            .into_iter()
            .filter(|note| note.title.to_lowercase() == "alpha")
            .map(|note| note.id)
            .collect();
        alphas.sort();
        let beta = lib.search("beta")[0];
        let zebra = lib.search("zebra")[0];
        assert_eq!(lib.search(""), [alphas[0], alphas[1], beta, zebra]);
        assert_eq!(lib.search("matching"), [alphas[0], alphas[1], beta, zebra]);
        lib.sort = SortOrder::Modified;
        assert_eq!(lib.search("matching"), [zebra, alphas[0], alphas[1], beta]);
    }

    #[test]
    fn cached_search_refreshes_on_create_save_rename_and_forget() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        assert!(lib.search("needle").is_empty());
        let id = lib.create("MiXeD", "NEEDLE").unwrap();
        assert_eq!(lib.fields[&id].title, "mixed");
        assert_eq!(lib.fields[&id].body, "needle");
        assert_eq!(lib.search("needle"), [id]);
        assert!(!lib.dirty);
        assert!(lib.search("replacement").is_empty());
        let hash = lib.fingerprint(id);
        lib.save(id, "REPLACEMENT").unwrap();
        assert_ne!(lib.fingerprint(id), hash);
        assert_eq!(lib.search("replacement"), [id]);
        assert!(lib.search("needle").is_empty());
        assert!(lib.search("renamed").is_empty());
        lib.rename(id, "RENAMED").unwrap();
        assert_eq!(lib.search("renamed"), [id]);
        let path = lib.get(id).unwrap().path.clone();
        lib.forget(id);
        assert!(lib.search("renamed").is_empty());
        assert!(lib.fingerprint(id).is_none());
        assert!(!lib.fields.contains_key(&id));
        assert!(path.exists());
    }

    #[test]
    fn autocomplete_uses_supplied_order_and_cached_titles() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let dogs = lib.create("Dogs", "").unwrap();
        let dog = lib.create("Dog", "").unwrap();
        let dot = lib.create("Dot", "").unwrap();
        assert_eq!(lib.autocomplete("DO", &[dogs, dog, dot]), Some(dog));
        assert_eq!(lib.autocomplete("do", &[dot, dog]), Some(dot));
        assert_eq!(lib.autocomplete("", &[dog]), None);
        assert_eq!(lib.autocomplete("og", &[dog]), None);
        lib.forget(dog);
        assert_eq!(lib.autocomplete("do", &[dog, dogs]), Some(dogs));
    }

    #[test]
    fn unchanged_save_does_not_write_or_reload_external_text() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let id = lib.create("note", "original").unwrap();
        let path = lib.get(id).unwrap().path.clone();
        let modified = lib.get(id).unwrap().modified;
        let hash = lib.fingerprint(id);
        lib.search("");
        assert_eq!(lib.save(id, "original").unwrap(), SaveOutcome::Saved);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert!(!lib.dirty);
        fs::write(&path, "external").unwrap();
        assert_eq!(lib.save(id, "original").unwrap(), SaveOutcome::Saved);
        assert_eq!(fs::read_to_string(&path).unwrap(), "external");
        assert_eq!(lib.get(id).unwrap().body, "original");
        assert_eq!(lib.fingerprint(id), hash);
        assert_eq!(lib.all().len(), 1);
    }

    #[test]
    fn conflict_preserves_both_texts_and_refreshes_search() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let id = lib.create("note", "original").unwrap();
        let path = lib.get(id).unwrap().path.clone();
        assert!(lib.search("disk").is_empty());
        fs::write(&path, "disk text").unwrap();
        let SaveOutcome::Conflict { copy_id } = lib.save(id, "local text").unwrap() else {
            panic!("expected a conflict copy");
        };
        assert_ne!(copy_id, id);
        let copy = lib.get(copy_id).unwrap();
        assert!(copy.title.starts_with("note (conflict "));
        assert!(copy.title.ends_with(')'));
        assert!(chrono::NaiveDateTime::parse_from_str(
            copy.title
                .strip_prefix("note (conflict ")
                .unwrap()
                .strip_suffix(')')
                .unwrap(),
            "%Y-%m-%d %H%M"
        )
        .is_ok());
        assert_eq!(copy.path.extension().unwrap(), "txt");
        assert_eq!(fs::read_to_string(&copy.path).unwrap(), "local text");
        assert_eq!(lib.get(id).unwrap().body, "disk text");
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk text");
        assert_eq!(lib.search("disk"), [id]);
        assert_eq!(lib.search("local"), [copy_id]);
        assert_eq!(lib.save(id, "accepted").unwrap(), SaveOutcome::Saved);
    }

    #[test]
    fn external_delete_refuses_save_and_rename_until_restore() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let id = lib.create("lost", "retained").unwrap();
        let path = lib.get(id).unwrap().path.clone();
        fs::remove_file(&path).unwrap();
        assert!(matches!(lib.save(id, "unsaved"), Err(Error::ExternalChange(p)) if p == path));
        assert!(lib.is_deleted_on_disk(id));
        assert_eq!(lib.get(id).unwrap().body, "retained");
        assert_eq!(lib.search("retained"), [id]);
        assert!(!path.exists());
        assert!(matches!(
            lib.save(id, "retained"),
            Err(Error::ExternalChange(_))
        ));
        assert!(matches!(
            lib.rename(id, "new"),
            Err(Error::ExternalChange(_))
        ));
        assert_eq!(fs::read_dir(root.path().join("notes")).unwrap().count(), 0);
        let restored = lib.restore(id, "unsaved").unwrap();
        assert_ne!(restored, id);
        assert!(lib.get(id).is_none());
        assert!(!lib.is_deleted_on_disk(id));
        assert!(!lib.is_deleted_on_disk(restored));
        let note = lib.get(restored).unwrap();
        assert_eq!(note.title, "lost");
        assert_eq!(note.path, path);
        assert!(!lib.fields.contains_key(&id));
        assert_eq!(fs::read_to_string(&note.path).unwrap(), "unsaved");
        assert_eq!(lib.search("unsaved"), [restored]);
        lib.save(restored, "edited again").unwrap();
    }

    #[test]
    fn trash_deleted_note_only_forgets_and_normal_trash_preserves_text() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let id = lib.create("lost", "retained").unwrap();
        let path = lib.get(id).unwrap().path.clone();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Removed(path)),
            Change::Removed(id)
        );
        lib.trash(id).unwrap();
        assert!(lib.get(id).is_none());
        assert!(!lib.is_deleted_on_disk(id));
        assert!(!root.path().join("trash").exists());
        assert!(lib.search("").is_empty());
        let id = lib.create("normal", "keep in trash").unwrap();
        lib.trash(id).unwrap();
        assert!(lib.search("").is_empty());
        assert_eq!(
            fs::read_to_string(root.path().join("trash/normal.txt")).unwrap(),
            "keep in trash"
        );
    }

    #[test]
    fn external_events_reload_follow_renames_and_handle_unknown_paths() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let path = root.path().join("notes/incoming.txt");
        let missing = root.path().join("notes/missing.txt");
        assert_eq!(
            lib.apply_external(StoreEvent::Changed(missing.clone())),
            Change::None
        );
        assert_eq!(
            lib.apply_external(StoreEvent::Removed(missing.clone())),
            Change::None
        );
        fs::write(&path, "first").unwrap();
        let Change::Added(id) = lib.apply_external(StoreEvent::Changed(path.clone())) else {
            panic!("expected added note");
        };
        assert_eq!(lib.search("first"), [id]);
        assert!(lib.search("second").is_empty());
        fs::write(&path, "second").unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Changed(path.clone())),
            Change::Updated(id)
        );
        assert_eq!(lib.search("second"), [id]);
        fs::remove_file(&path).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Removed(path.clone())),
            Change::Removed(id)
        );
        assert!(lib.is_deleted_on_disk(id));
        assert_eq!(lib.get(id).unwrap().body, "second");
        fs::write(&path, "returned").unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Changed(path.clone())),
            Change::Updated(id)
        );
        assert!(!lib.is_deleted_on_disk(id));
        let to = path.with_file_name("renamed.md");
        assert!(lib.search("renamed").is_empty());
        fs::rename(&path, &to).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Renamed {
                from: path,
                to: to.clone(),
            }),
            Change::Renamed(id)
        );
        assert_eq!(lib.get(id).unwrap().path, to);
        assert_eq!(lib.search("renamed"), [id]);
        lib.save(id, "after rename").unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "after rename");
        let incoming = to.with_file_name("another.txt");
        fs::write(&incoming, "another").unwrap();
        let Change::Added(other) = lib.apply_external(StoreEvent::Renamed {
            from: missing.clone(),
            to: incoming,
        }) else {
            panic!("unknown rename source should add destination");
        };
        assert_ne!(other, id);
        assert_eq!(
            lib.apply_external(StoreEvent::Renamed {
                from: missing.clone(),
                to,
            }),
            Change::Updated(id)
        );
        assert_eq!(
            lib.apply_external(StoreEvent::Renamed {
                from: missing.clone(),
                to: missing,
            }),
            Change::None
        );
    }

    #[test]
    fn failed_rename_reload_keeps_new_path_and_refreshes_title_cache() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let id = lib.create("old", "retained").unwrap();
        let from = lib.get(id).unwrap().path.clone();
        let to = from.with_file_name("new.txt");
        fs::remove_file(&from).unwrap();
        assert!(lib.search("new").is_empty());
        assert_eq!(
            lib.apply_external(StoreEvent::Renamed {
                from,
                to: to.clone(),
            }),
            Change::None
        );
        assert_eq!(lib.get(id).unwrap().path, to);
        assert_eq!(lib.get(id).unwrap().body, "retained");
        assert_eq!(lib.search("new"), [id]);
    }

    #[test]
    fn lossy_save_is_refused_and_failed_restore_retains_old_note() {
        let root = tempdir().unwrap();
        let mut lib = library(&root, SortOrder::Title);
        let path = root.path().join("notes/lossy.txt");
        fs::write(&path, [0xff]).unwrap();
        let Change::Added(id) = lib.apply_external(StoreEvent::Changed(path.clone())) else {
            panic!("expected lossy note");
        };
        assert!(matches!(
            lib.save(id, "replacement"),
            Err(Error::ReadOnly(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), [0xff]);
        assert_eq!(lib.all().len(), 1);
        fs::remove_file(&path).unwrap();
        lib.apply_external(StoreEvent::Removed(path));
        fs::remove_dir(root.path().join("notes")).unwrap();
        assert!(lib.restore(id, "local").is_err());
        assert!(lib.get(id).is_some());
        assert!(lib.is_deleted_on_disk(id));
    }
}
