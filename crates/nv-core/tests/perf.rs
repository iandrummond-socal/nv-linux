use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use nv_core::config::SortOrder;
use nv_core::library::Library;
use nv_core::storage::{FakeTrasher, NoteStore};
use tempfile::tempdir;

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn generate_notes(dir: &Path) {
    fs::create_dir(dir).unwrap();
    let mut state = 0x1234_5678_9abc_def0;
    for index in 0..10_000 {
        let mut body = String::with_capacity(2064);
        while body.len() < 2048 {
            let length = 3 + (next(&mut state) % 8) as usize;
            for _ in 0..length {
                body.push(char::from(b'a' + (next(&mut state) % 26) as u8));
            }
            body.push(' ');
        }
        // Keep a real match throughout all twelve keystrokes.
        if index % 100 == 0 {
            body.push_str("abcdefghijkl");
        }
        fs::write(dir.join(format!("note {index:05}.txt")), body).unwrap();
    }
}

#[test]
#[ignore = "release-only timing budget; generates 10,000 notes"]
fn load_and_search_ten_thousand_notes() {
    let root = tempdir().unwrap();
    let dir = root.path().join("notes");
    generate_notes(&dir);
    let started = Instant::now();
    let store = NoteStore::open(
        &dir,
        "txt",
        Box::new(FakeTrasher {
            dir: root.path().join("trash"),
        }),
    )
    .unwrap();
    let mut lib = Library::new(store, SortOrder::Title);
    let load = started.elapsed();
    println!("10,000 notes load: {load:?}");
    assert_eq!(lib.all().len(), 10_000);
    assert!(load < Duration::from_secs(1), "load took {load:?}");

    let mut query = String::new();
    let mut max = Duration::ZERO;
    let mut timings = Vec::new();
    for ch in "abcdefghijkl".chars() {
        query.push(ch);
        let started = Instant::now();
        let matches = lib.search(&query);
        let completion = lib.autocomplete(&query, &matches);
        let elapsed = started.elapsed();
        max = max.max(elapsed);
        timings.push((query.clone(), elapsed));
        assert!(matches.len() >= 100);
        assert_eq!(completion, None);
    }
    println!("maximum search + autocomplete keystroke: {max:?}");
    for (query, elapsed) in timings {
        assert!(
            elapsed < Duration::from_millis(16),
            "query {query:?} took {elapsed:?}"
        );
    }
}
