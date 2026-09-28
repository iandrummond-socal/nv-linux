use std::fs;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nv_core::config::SortOrder;
use nv_core::library::{Change, Library, SaveOutcome};
use nv_core::storage::{FakeTrasher, NoteStore};
use nv_core::watcher::{FolderWatcher, StoreEvent};
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

#[test]
fn library_end_to_end() {
    let root = tempdir().unwrap();
    let mut lib = library(&root);
    let dogs = lib.create("dogs", "beta then alpha").unwrap();
    let food = lib.create("dog food", "alpha then beta").unwrap();
    let dog = lib.create("dog", "alpha alone").unwrap();
    assert_eq!(lib.search(""), [dog, food, dogs]);
    assert_eq!(lib.search("alpha"), [dog, food, dogs]);
    assert_eq!(lib.search("alpha beta"), [food]);
    assert!(lib.search("dog alpha").is_empty());
    let matches = lib.search("do");
    assert_eq!(matches, [dog, food, dogs]);
    assert_eq!(lib.autocomplete("do", &matches), Some(dog));
    // Prefix parents win even when a longer child comes first.
    assert_eq!(lib.autocomplete("do", &[dogs, food, dog]), Some(dog));

    let old_path = lib.get(dog).unwrap().path.clone();
    lib.rename(dog, "hound").unwrap();
    let path = lib.get(dog).unwrap().path.clone();
    assert!(!old_path.exists());
    assert_eq!(fs::read_to_string(&path).unwrap(), "alpha alone");
    assert_eq!(lib.search("hound"), [dog]);

    fs::write(&path, "outside revision").unwrap();
    assert_eq!(
        lib.apply_external(StoreEvent::Changed(path.clone())),
        Change::Updated(dog)
    );
    assert_eq!(lib.search("outside revision"), [dog]);
    let pending = "local unsaved revision";
    fs::write(&path, "another external revision").unwrap();
    let SaveOutcome::Conflict { copy_id } = lib.save(dog, pending).unwrap() else {
        panic!("external edit must produce a conflict copy");
    };
    assert_ne!(copy_id, dog);
    assert_eq!(lib.get(copy_id).unwrap().body, pending);
    let copy_path = lib.get(copy_id).unwrap().path.clone();
    assert_eq!(fs::read_to_string(&copy_path).unwrap(), pending);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "another external revision"
    );
    assert_eq!(lib.search("local unsaved"), [copy_id]);
    lib.trash(copy_id).unwrap();
    assert!(!copy_path.exists());
    assert!(lib.get(copy_id).is_none());
    assert_eq!(
        fs::read_to_string(
            root.path()
                .join("trash")
                .join(copy_path.file_name().unwrap())
        )
        .unwrap(),
        pending
    );

    let retained = lib.get(dog).unwrap().body.clone();
    fs::remove_file(&path).unwrap();
    assert_eq!(
        lib.apply_external(StoreEvent::Removed(path.clone())),
        Change::Removed(dog)
    );
    assert!(lib.is_deleted_on_disk(dog));
    let pending = format!("{retained}\nrestored local edit");
    assert!(matches!(lib.save(dog, &pending), Err(Error::ExternalChange(p)) if p == path));
    assert_eq!(lib.get(dog).unwrap().body, retained);
    assert!(!path.exists());
    let restored = lib.restore(dog, &pending).unwrap();
    assert_ne!(restored, dog);
    assert!(lib.get(dog).is_none());
    assert_eq!(lib.get(restored).unwrap().path, path);
    assert_eq!(fs::read_to_string(&path).unwrap(), pending);
    assert_eq!(lib.search("restored local"), [restored]);
}

#[test]
fn real_folder_watcher_imports_an_external_write() {
    let root = tempdir().unwrap();
    let mut lib = library(&root);
    let dir = fs::canonicalize(root.path().join("notes")).unwrap();
    let (sender, receiver) = mpsc::channel();
    let _watcher = FolderWatcher::start(&dir, sender).unwrap();
    let path = dir.join("incoming.txt");
    let deadline = Instant::now() + Duration::from_secs(3);
    fs::write(&path, "visible foreign body").unwrap();
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("expected Changed within three seconds");
        if event == StoreEvent::Changed(path.clone()) {
            let Change::Added(id) = lib.apply_external(event) else {
                panic!("watcher event should import the note");
            };
            assert_eq!(lib.search("foreign body"), [id]);
            assert_eq!(lib.get(id).unwrap().body, "visible foreign body");
            break;
        }
        assert!(Instant::now() < deadline, "no matching Changed event");
    }
}
