use std::thread;
use std::time::{Duration, Instant};

use nv_sync::content_hash::content_hash;
use nv_sync::remote::{
    Cursor, Notifier, Remote, RemoteChange, RemoteError, RemoteFile, RemoteResult,
};

/// Track every successful write, including unexpected successes before an assertion.
struct Cleanup<'a> {
    remote: &'a mut dyn Remote,
    files: Vec<RemoteFile>,
}

impl<'a> Cleanup<'a> {
    fn new(remote: &'a mut dyn Remote) -> Self {
        Self {
            remote,
            files: Vec::new(),
        }
    }

    fn upload(&mut self, name: &str, bytes: &[u8], rev: Option<&str>) -> RemoteResult<RemoteFile> {
        let file = self.remote.upload(name, bytes, rev)?;
        self.files.retain(|old| old.id != file.id);
        self.files.push(file.clone());
        Ok(file)
    }

    fn delete(&mut self, file: &RemoteFile) {
        self.remote
            .delete(&file.id, &file.rev)
            .unwrap_or_else(|error| panic!("cleanup: {error}"));
        self.files.retain(|old| old.id != file.id);
    }

    fn finish(&mut self) {
        while let Some(file) = self.files.last().cloned() {
            self.delete(&file);
        }
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for file in &self.files {
            if let Err(error) = self.remote.delete(&file.id, &file.rev) {
                eprintln!("Best-effort cleanup of {} failed: {error}", file.name);
            }
        }
    }
}

#[allow(dead_code)]
pub fn upload_new_then_list_and_download(remote: &mut dyn Remote, prefix: &str) {
    let mut guard = Cleanup::new(remote);
    let name = format!("{prefix}a.txt");
    let bytes = b"new note\n";
    let file = guard.upload(&name, bytes, None).unwrap();
    assert_eq!(file.name, name);
    assert_eq!(file.key, format!("/{}", name.to_lowercase()));
    assert_eq!(file.size, bytes.len() as u64);
    assert!(!file.rev.is_empty());
    assert_eq!(file.content_hash, content_hash(bytes));
    assert!(guard.remote.list().unwrap().0.contains(&file));
    assert_eq!(guard.remote.download(&file.id).unwrap(), bytes);
    guard.finish();
}

#[allow(dead_code)]
pub fn stale_rev_conflicts(remote: &mut dyn Remote, prefix: &str) {
    let mut guard = Cleanup::new(remote);
    let name = format!("{prefix}stale.txt");
    let first = guard.upload(&name, b"first", None).unwrap();
    let second = guard.upload(&name, b"second", Some(&first.rev)).unwrap();
    assert_eq!(first.id, second.id);
    assert_ne!(first.rev, second.rev);
    assert_eq!(
        guard.upload(&name, b"stale", Some(&first.rev)),
        Err(RemoteError::Conflict)
    );
    assert_eq!(
        guard.upload(&name, b"add", None),
        Err(RemoteError::Conflict)
    );
    guard.finish();
}

#[allow(dead_code)]
pub fn case_insensitive_same_file(remote: &mut dyn Remote, prefix: &str) {
    let mut guard = Cleanup::new(remote);
    let first = guard
        .upload(&format!("{prefix}Todo.txt"), b"first", None)
        .unwrap();
    assert_eq!(
        guard.upload(&format!("{prefix}todo.txt"), b"add", None),
        Err(RemoteError::Conflict)
    );
    let second = guard
        .upload(&format!("{prefix}TODO.txt"), b"second", Some(&first.rev))
        .unwrap();
    assert_eq!(first.id, second.id);
    guard.finish();
}

#[allow(dead_code)]
pub fn non_ascii_names(remote: &mut dyn Remote, prefix: &str) {
    let mut guard = Cleanup::new(remote);
    let name = format!("{prefix}Café 🎉 ünïcödé.txt");
    let bytes = "Unicode content: café 🎉\n".as_bytes();
    let file = guard.upload(&name, bytes, None).unwrap();
    eprintln!("Unicode name {name:?}: path_lower = {:?}", file.key);
    assert!(guard.remote.list().unwrap().0.contains(&file));
    assert_eq!(guard.remote.download(&file.id).unwrap(), bytes);
    guard.finish();
}

#[allow(dead_code)]
pub fn cursor_deltas(remote: &mut dyn Remote, prefix: &str, _real: bool) {
    let mut guard = Cleanup::new(remote);
    let (_, cursor) = guard.remote.list().unwrap();
    let file = guard
        .upload(&format!("{prefix}delta.txt"), b"delta", None)
        .unwrap();
    let (changes, cursor) = guard.remote.changes(&cursor).unwrap();
    assert!(changes.contains(&RemoteChange::Upsert(file.clone())));
    guard.delete(&file);
    let (changes, _) = guard.remote.changes(&cursor).unwrap();
    assert!(changes.contains(&RemoteChange::Deleted { key: file.key }));
    match guard.remote.changes(&Cursor("garbage".into())) {
        Err(RemoteError::ResetCursor) => eprintln!("Invalid cursor: ResetCursor"),
        result => panic!("unexpected invalid-cursor result: {result:?}"),
    }
}

#[allow(dead_code)]
pub fn longpoll_sees_change(
    remote: &mut dyn Remote,
    notifier: &mut dyn Notifier,
    prefix: &str,
    factory: impl Fn() -> Box<dyn Remote + Send>,
) {
    let (_, cursor) = remote.list().unwrap();
    let mut second = factory();
    let name = format!("{prefix}poll.txt");
    // Dropping the sender also releases the worker during assertion unwinding.
    let (release, finished) = std::sync::mpsc::channel::<()>();
    thread::scope(|scope| {
        let worker = scope.spawn(move || {
            thread::sleep(Duration::from_secs(2));
            let mut guard = Cleanup::new(second.as_mut());
            let file = guard.upload(&name, b"poll", None).unwrap();
            let _ = finished.recv();
            guard.finish();
            file
        });
        let start = Instant::now();
        let result = notifier.wait(&cursor, Duration::from_secs(30));
        let elapsed = start.elapsed();
        eprintln!("Longpoll returned after {elapsed:?}");
        drop(release);
        let file = worker.join().expect("upload worker panicked");
        assert!(file.name.starts_with(prefix));
        assert_eq!(result, Ok(true));
        assert!(
            elapsed < Duration::from_secs(15),
            "longpoll took {elapsed:?}"
        );
    });
}

#[allow(dead_code)]
pub fn rejected_characters(remote: &mut dyn Remote, prefix: &str) -> Vec<(String, String)> {
    let mut names: Vec<_> = [
        ("lt", '<'),
        ("gt", '>'),
        ("colon", ':'),
        ("quote", '"'),
        ("pipe", '|'),
        ("question", '?'),
        ("star", '*'),
        ("backslash", '\\'),
    ]
    .into_iter()
    .map(|(desc, ch)| format!("{prefix}c-{desc}-a{ch}b.txt"))
    .collect();
    names.extend([
        format!("{prefix}sp-trailing .txt"),
        format!("{prefix}trailing-dot..txt"),
        format!("{prefix}nfd-e\u{0301}.txt"),
        format!("{prefix}nfc-\u{e9}.txt"),
    ]);
    let mut guard = Cleanup::new(remote);
    let mut table = Vec::new();
    for name in names {
        let result = match guard.upload(&name, b"name probe", None) {
            Ok(file) => {
                let result = format!("Ok({})", file.key);
                guard.delete(&file);
                result
            }
            Err(error) => format!("Err({error})"),
        };
        eprintln!("{name:?}\t{result}");
        table.push((name, result));
    }
    table
}
