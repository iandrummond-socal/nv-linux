use std::fs;
use std::path::{Path, PathBuf};

use nv_core::config::SortOrder;
use nv_core::library::{Change, Library, SaveOutcome};
use nv_core::model::NoteId;
use nv_core::storage::{FakeTrasher, NoteStore};
use nv_core::watcher::StoreEvent;
use nv_core::Error;
use tempfile::{tempdir, TempDir};

fn library(root: &TempDir) -> Library {
    Library::new(
        NoteStore::open(
            root.path().join("notes"),
            "txt",
            Box::new(FakeTrasher {
                dir: root.path().join("trash"),
            }),
        )
        .unwrap(),
        SortOrder::Title,
    )
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("cannot read {}: {error}", dir.display()),
    };
    let mut paths: Vec<_> = entries.map(|entry| entry.unwrap().path()).collect();
    paths.sort();
    paths.into_iter().filter(|path| path.is_file()).collect()
}

fn all_text(notes_dir: &Path, trash_dir: &Path) -> Vec<String> {
    files(notes_dir)
        .into_iter()
        .chain(files(trash_dir))
        .map(|path| String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned())
        .collect()
}

fn assert_on_disk(root: &TempDir, expected: &[String]) {
    let texts = all_text(&root.path().join("notes"), &root.path().join("trash"));
    for text in expected {
        assert!(
            texts.iter().any(|body| body.contains(text)),
            "lost text: {text:?}"
        );
    }
}

fn assert_retained(root: &TempDir, lib: &Library, id: NoteId, pending: &str, expected: &[String]) {
    let mut texts = all_text(&root.path().join("notes"), &root.path().join("trash"));
    if let Some(note) = lib.get(id) {
        assert!(lib.is_deleted_on_disk(id));
        texts.push(note.body.clone());
    }
    texts.push(pending.to_owned());
    for text in expected {
        assert!(
            texts.iter().any(|body| body.contains(text)),
            "lost retained text: {text:?}"
        );
    }
}

#[test]
fn cold_start_twins() {
    for state in ["missing", "empty", "populated"] {
        let root = tempdir().unwrap();
        let dir = root.path().join("notes");
        if state != "missing" {
            fs::create_dir(&dir).unwrap();
        }
        if state == "populated" {
            fs::write(dir.join("existing.md"), "preexisting body").unwrap();
        }
        let mut lib = library(&root);
        assert!(dir.is_dir());
        assert_eq!(lib.all().len(), usize::from(state == "populated"));
        if state == "populated" {
            let matches = lib.search("preexisting body");
            assert_eq!(matches.len(), 1);
            assert_eq!(lib.get(matches[0]).unwrap().body, "preexisting body");
        }
        let id = lib.create("created", "new body").unwrap();
        assert_eq!(lib.search("new body"), [id]);
        drop(lib);
        let mut reopened = library(&root);
        assert_eq!(reopened.all().len(), 1 + usize::from(state == "populated"));
        assert_eq!(reopened.search("new body").len(), 1);
    }
}

#[test]
fn never_lose_text_through_mutations() {
    let root = tempdir().unwrap();
    let mut lib = library(&root);
    let mut expected = vec!["original body".to_owned()];
    let id = lib.create("first", &expected[0]).unwrap();
    assert_on_disk(&root, &expected);
    let other = lib.create("occupied", "other note body").unwrap();
    expected.push(lib.get(other).unwrap().body.clone());
    assert_on_disk(&root, &expected);

    // Append edits so intentional user replacements do not erase history in
    // this invariant fixture. The engine is not a version-history service.
    let pending = "original body\nfirst local edit".to_owned();
    expected.push(pending.clone());
    assert_eq!(lib.save(id, &pending).unwrap(), SaveOutcome::Saved);
    assert_on_disk(&root, &expected);
    let old = lib.get(id).unwrap().path.clone();
    lib.rename(id, "renamed").unwrap();
    assert!(!old.exists());
    assert_on_disk(&root, &expected);
    lib.rename(id, "occupied").unwrap();
    assert_eq!(lib.get(id).unwrap().title, "occupied 2");
    assert_ne!(lib.get(id).unwrap().path, lib.get(other).unwrap().path);
    assert_on_disk(&root, &expected);

    let path = lib.get(id).unwrap().path.clone();
    let external = format!("{pending}\nexternal without pending edit");
    fs::write(&path, &external).unwrap();
    expected.push(external.clone());
    assert_eq!(
        lib.apply_external(StoreEvent::Changed(path.clone())),
        Change::Updated(id)
    );
    assert_on_disk(&root, &expected);
    let local = format!("{external}\nunsaved local branch");
    let foreign = format!("{external}\nexternal competing branch");
    expected.extend([local.clone(), foreign.clone()]);
    fs::write(&path, &foreign).unwrap();
    // Save BEFORE applying the event: the checked write must keep both branches.
    let SaveOutcome::Conflict { copy_id } = lib.save(id, &local).unwrap() else {
        panic!("expected conflict");
    };
    assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
    assert_eq!(lib.get(copy_id).unwrap().body, local);
    assert_on_disk(&root, &expected);
    assert_eq!(
        lib.apply_external(StoreEvent::Changed(path.clone())),
        Change::Updated(id)
    );
    assert_on_disk(&root, &expected);
    lib.trash(copy_id).unwrap();
    assert_on_disk(&root, &expected);

    let pending = format!("{foreign}\npending after deletion");
    expected.push(pending.clone());
    fs::remove_file(&path).unwrap();
    assert_eq!(
        lib.apply_external(StoreEvent::Removed(path)),
        Change::Removed(id)
    );
    assert!(matches!(
        lib.save(id, &pending),
        Err(Error::ExternalChange(_))
    ));
    assert_eq!(lib.get(id).unwrap().body, foreign);
    assert_retained(&root, &lib, id, &pending, &expected);
    let restored = lib.restore(id, &pending).unwrap();
    assert_eq!(
        fs::read_to_string(&lib.get(restored).unwrap().path).unwrap(),
        pending
    );
    assert_on_disk(&root, &expected);
    lib.trash(restored).unwrap();
    lib.trash(other).unwrap();
    assert!(lib.all().is_empty());
    assert_on_disk(&root, &expected);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Saved,
    Dirty,
    ReadOnly,
    Deleted,
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Save,
    Rename,
    Trash,
    Changed,
    Removed,
    Renamed,
}

#[test]
fn status_by_operation_grid() {
    for status in [
        Status::Saved,
        Status::Dirty,
        Status::ReadOnly,
        Status::Deleted,
    ] {
        for operation in [
            Operation::Save,
            Operation::Rename,
            Operation::Trash,
            Operation::Changed,
            Operation::Removed,
            Operation::Renamed,
        ] {
            exercise_cell(status, operation);
        }
    }
}

fn exercise_cell(status: Status, operation: Operation) {
    let root = tempdir().unwrap();
    let mut lib = library(&root);
    let id = lib.create("open", "saved body").unwrap();
    let path = lib.get(id).unwrap().path.clone();
    if status == Status::ReadOnly {
        fs::write(&path, [b'a', 0xff, b'b']).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Changed(path.clone())),
            Change::Updated(id)
        );
        assert!(lib.get(id).unwrap().read_only);
    }
    let original = lib.get(id).unwrap().body.clone();
    let pending = format!("{original}\nlocal edit");
    let mut expected = vec![original.clone()];
    if matches!(status, Status::Dirty | Status::Deleted) {
        expected.push(pending.clone());
    }
    if status == Status::Deleted {
        fs::remove_file(&path).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Removed(path.clone())),
            Change::Removed(id)
        );
    }
    let mut retained_only = false;
    match operation {
        Operation::Save => match status {
            Status::ReadOnly => {
                assert!(matches!(lib.save(id, &pending), Err(Error::ReadOnly(_))));
                assert_eq!(fs::read(&path).unwrap(), [b'a', 0xff, b'b']);
            }
            Status::Deleted => {
                assert!(matches!(
                    lib.save(id, &pending),
                    Err(Error::ExternalChange(_))
                ));
                assert!(!path.exists());
                assert_eq!(lib.get(id).unwrap().body, original);
                assert_retained(&root, &lib, id, &pending, &expected);
                let restored = lib.restore(id, &pending).unwrap();
                assert_eq!(
                    fs::read_to_string(&lib.get(restored).unwrap().path).unwrap(),
                    pending
                );
            }
            _ => {
                assert_eq!(lib.save(id, &pending).unwrap(), SaveOutcome::Saved);
                expected.push(pending.clone());
                assert_eq!(fs::read_to_string(&path).unwrap(), pending);
            }
        },
        Operation::Rename => {
            if status == Status::Deleted {
                assert!(matches!(
                    lib.rename(id, "new"),
                    Err(Error::ExternalChange(_))
                ));
                assert_eq!(lib.get(id).unwrap().path, path);
                retained_only = true;
            } else {
                if status == Status::Dirty {
                    assert_eq!(lib.save(id, &pending).unwrap(), SaveOutcome::Saved);
                }
                lib.rename(id, "new").unwrap();
                let new = &lib.get(id).unwrap().path;
                assert_eq!(new.file_name().unwrap(), "new.txt");
                assert!(!path.exists());
                if status == Status::Dirty {
                    assert_eq!(fs::read_to_string(new).unwrap(), pending);
                }
                if status == Status::ReadOnly {
                    assert_eq!(fs::read(new).unwrap(), [b'a', 0xff, b'b']);
                }
            }
        }
        Operation::Trash => {
            if status == Status::Dirty {
                lib.save(id, &pending).unwrap();
            }
            // Unrelated files must survive even the already-deleted case.
            let sentinel = lib.create("sentinel", "untouched sibling").unwrap();
            expected.push("untouched sibling".to_owned());
            let before = all_text(&root.path().join("notes"), &root.path().join("trash"));
            lib.trash(id).unwrap();
            assert!(lib.get(id).is_none());
            assert!(!path.exists());
            assert!(lib.get(sentinel).unwrap().path.exists());
            if status == Status::Deleted {
                assert!(files(&root.path().join("trash")).is_empty());
                assert_eq!(
                    all_text(&root.path().join("notes"), &root.path().join("trash")),
                    before
                );
                retained_only = true; // The caller still owns pending text.
            } else {
                let trashed = root.path().join("trash/open.txt");
                assert!(trashed.exists());
                if status == Status::ReadOnly {
                    assert_eq!(fs::read(trashed).unwrap(), [b'a', 0xff, b'b']);
                }
            }
        }
        Operation::Changed => {
            if status == Status::ReadOnly {
                assert_eq!(
                    lib.apply_external(StoreEvent::Changed(path.clone())),
                    Change::Updated(id)
                );
                assert_eq!(fs::read(&path).unwrap(), [b'a', 0xff, b'b']);
            } else {
                let foreign = format!("{original}\nforeign edit");
                fs::write(&path, &foreign).unwrap();
                expected.push(foreign.clone());
                if status == Status::Dirty {
                    let SaveOutcome::Conflict { copy_id } = lib.save(id, &pending).unwrap() else {
                        panic!("{status:?} x {operation:?}: expected conflict");
                    };
                    assert_eq!(lib.get(copy_id).unwrap().body, pending);
                    assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
                }
                assert_eq!(
                    lib.apply_external(StoreEvent::Changed(path.clone())),
                    Change::Updated(id)
                );
                assert_eq!(lib.get(id).unwrap().body, foreign);
                assert!(!lib.is_deleted_on_disk(id));
                if status == Status::Deleted {
                    // The returning file clears deletion; flush the retained edit
                    // together with the newly accepted external text.
                    lib.save(id, &format!("{foreign}\n{pending}")).unwrap();
                }
            }
        }
        Operation::Removed => {
            if status == Status::ReadOnly {
                let trash = root.path().join("trash");
                fs::create_dir(&trash).unwrap();
                let removed = trash.join("original lossy bytes.txt");
                fs::rename(&path, &removed).unwrap();
                assert_eq!(fs::read(removed).unwrap(), [b'a', 0xff, b'b']);
            } else if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            assert_eq!(
                lib.apply_external(StoreEvent::Removed(path.clone())),
                Change::Removed(id)
            );
            assert_eq!(lib.get(id).unwrap().body, original);
            assert!(lib.is_deleted_on_disk(id));
            assert!(matches!(
                lib.save(id, &pending),
                Err(Error::ExternalChange(_))
            ));
            assert_retained(&root, &lib, id, &pending, &expected);
            let restored = lib.restore(id, &pending).unwrap();
            assert_eq!(
                fs::read_to_string(&lib.get(restored).unwrap().path).unwrap(),
                pending
            );
        }
        Operation::Renamed => {
            if status == Status::Dirty {
                lib.save(id, &pending).unwrap();
            }
            if status == Status::Deleted {
                // An external editor returns the retained note under a new name.
                fs::write(&path, &pending).unwrap();
            }
            let new = path.with_file_name("external.md");
            fs::rename(&path, &new).unwrap();
            assert_eq!(
                lib.apply_external(StoreEvent::Renamed {
                    from: path.clone(),
                    to: new.clone()
                }),
                Change::Renamed(id)
            );
            assert_eq!(lib.get(id).unwrap().path, new);
            assert!(!lib.is_deleted_on_disk(id));
            if status == Status::ReadOnly {
                assert!(matches!(lib.save(id, &pending), Err(Error::ReadOnly(_))));
                assert_eq!(fs::read(&new).unwrap(), [b'a', 0xff, b'b']);
            } else {
                let edited = format!("{}\nedit after rename", lib.get(id).unwrap().body);
                lib.save(id, &edited).unwrap();
                expected.push(edited.clone());
                assert_eq!(fs::read_to_string(new).unwrap(), edited);
            }
            assert!(!path.exists());
        }
    }
    if retained_only {
        assert_retained(&root, &lib, id, &pending, &expected);
        if lib.get(id).is_some() {
            lib.restore(id, &pending).unwrap();
        } else {
            // Trashing an already missing note forgets its ID, so the caller
            // can explicitly recreate the pending buffer as a new note.
            lib.create("recovered pending edit", &pending).unwrap();
        }
    }
    assert_on_disk(&root, &expected);
}

#[test]
fn lossy_fixtures_remain_byte_identical() {
    for bytes in [
        &[b'a', 0xff, b'b'][..],
        &[0xf0, 0x28, 0x8c, 0x28],
        &[0xc3],
        &[0, 0xfe, b'z'],
    ] {
        let root = tempdir().unwrap();
        let mut lib = library(&root);
        let mut path = root.path().join("notes/lossy.txt");
        fs::write(&path, bytes).unwrap();
        let Change::Added(id) = lib.apply_external(StoreEvent::Changed(path.clone())) else {
            panic!("expected new lossy note");
        };
        let body = String::from_utf8_lossy(bytes).into_owned();
        assert_eq!(lib.get(id).unwrap().body, body);
        assert!(lib.get(id).unwrap().read_only);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(matches!(
            lib.save(id, "attempted edit"),
            Err(Error::ReadOnly(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(lib.save(id, &body).unwrap(), SaveOutcome::Saved);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        lib.rename(id, "renamed lossy").unwrap();
        assert!(!path.exists());
        path = lib.get(id).unwrap().path.clone();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(lib.search("\u{fffd}"), [id]);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            lib.apply_external(StoreEvent::Changed(path.clone())),
            Change::Updated(id)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let new = path.with_file_name("external lossy.md");
        fs::rename(&path, &new).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Renamed {
                from: path,
                to: new.clone()
            }),
            Change::Renamed(id)
        );
        assert_eq!(fs::read(&new).unwrap(), bytes);
        assert!(matches!(
            lib.save(id, "another edit"),
            Err(Error::ReadOnly(_))
        ));
        assert_eq!(fs::read(&new).unwrap(), bytes);

        // Move outside the notes folder to simulate removal without throwing
        // away the original bytes; all preservation destinations stay isolated.
        let trash = root.path().join("trash");
        fs::create_dir(&trash).unwrap();
        let removed = trash.join("externally removed.md");
        fs::rename(&new, &removed).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Removed(new.clone())),
            Change::Removed(id)
        );
        assert_eq!(lib.get(id).unwrap().body, body);
        assert_eq!(fs::read(&removed).unwrap(), bytes);
        assert!(matches!(
            lib.save(id, "deleted edit"),
            Err(Error::ExternalChange(_))
        ));
        assert_eq!(fs::read(&removed).unwrap(), bytes);
        fs::rename(&removed, &new).unwrap();
        assert_eq!(
            lib.apply_external(StoreEvent::Changed(new.clone())),
            Change::Updated(id)
        );
        assert_eq!(fs::read(&new).unwrap(), bytes);
        assert!(lib.get(id).unwrap().read_only);
        lib.trash(id).unwrap();
        assert!(lib.get(id).is_none());
        assert!(!new.exists());
        assert_eq!(
            fs::read(trash.join(new.file_name().unwrap())).unwrap(),
            bytes
        );
        assert_on_disk(&root, &[body]);
    }
}
