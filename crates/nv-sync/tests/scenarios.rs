mod common;

use std::thread;
use std::time::{Duration, Instant};

use nv_sync::fake::{FakeRemote, SharedFake};
use nv_sync::remote::{Cursor, Notifier, RemoteResult};

fn fake() -> SharedFake {
    SharedFake::new(FakeRemote::with_hasher(nv_sync::content_hash::content_hash))
}

// SharedFake reports the current state immediately; adapt it to the blocking
// notifier contract without holding its mutex while waiting for the writer.
struct WaitingFake(SharedFake);

impl Notifier for WaitingFake {
    fn wait(&mut self, since: &Cursor, timeout: Duration) -> RemoteResult<bool> {
        let start = Instant::now();
        loop {
            if self.0.wait(since, Duration::ZERO)? {
                return Ok(true);
            }
            if start.elapsed() >= timeout {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn upload_new_then_list_and_download() {
    common::upload_new_then_list_and_download(&mut fake(), "nvfake-upload-");
}

#[test]
fn stale_rev_conflicts() {
    common::stale_rev_conflicts(&mut fake(), "nvfake-stale-");
}

#[test]
fn case_insensitive_same_file() {
    common::case_insensitive_same_file(&mut fake(), "nvfake-case-");
}

#[test]
fn non_ascii_names() {
    common::non_ascii_names(&mut fake(), "nvfake-unicode-");
}

#[test]
fn cursor_deltas() {
    common::cursor_deltas(&mut fake(), "nvfake-cursor-", false);
}

#[test]
fn longpoll_sees_change() {
    let shared = fake();
    common::longpoll_sees_change(
        &mut shared.clone(),
        &mut WaitingFake(shared.clone()),
        "nvfake-poll-",
        || Box::new(shared.clone()),
    );
}

#[test]
fn rejected_characters() {
    common::rejected_characters(&mut fake(), "nvfake-names-");
}
