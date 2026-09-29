//! Black-box model tests for the sync engine, driven only through the public API.
//!
//! The engine's own unit tests pin single behaviours; this file checks the
//! properties that make a green suite trustworthy: random interleavings of
//! local and remote edits with injected faults converge without losing text,
//! cold starts never overwrite, the reconcile grid agrees with the engine, and
//! nothing secret or partial is ever left on disk or in the log.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

use nv_core::storage::{FakeTrasher, NoteStore};
use nv_sync::content_hash::content_hash;
use nv_sync::dropbox::{DropboxRemote, Endpoints};
use nv_sync::engine::{Engine, EngineOptions, PassReport};
use nv_sync::fake::{FakeRemote, SharedFake};
use nv_sync::plan::{reconcile, Action, LocalFile, SkipReason, MAX_SIZE};
use nv_sync::remote::{
    is_note_name, Cursor, Remote, RemoteChange, RemoteError, RemoteFile, RemoteResult,
};
use nv_sync::secret::Secret;
use nv_sync::state::STATE_FILE;
use nv_sync::tokens::{FileStore, StoredLogin, TokenStore};
use nv_sync::Error;
use proptest::prelude::*;

use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

/// Planted in every fixture's login file; must never appear anywhere else.
const REFRESH_SENTINEL: &str = "nvModelRefreshSentinel-7f3c9a";
/// Handed out by the loopback token endpoint; must never be persisted or logged.
const ACCESS_SENTINEL: &str = "nvModelAccessSentinel-91ad4e";
const LOGIN_FILE: &str = "dropbox-login.json";
const ACCOUNT: &str = "dbid:model-a";

// ---------------------------------------------------------------------------
// Log capture: one logger per test binary, records tagged by thread.

struct LogLine {
    thread: ThreadId,
    level: log::Level,
    target: String,
    message: String,
}

static LOGS: Mutex<Vec<LogLine>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let line = LogLine {
            thread: thread::current().id(),
            level: record.level(),
            target: record.target().to_owned(),
            message: record.args().to_string(),
        };
        lock(&LOGS).push(line);
    }

    fn flush(&self) {}
}

fn install_logger() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // `set_boxed_logger` needs log's `std` feature, which nv-sync does not
        // enable; a static logger is the same one-per-binary capture.
        static CAPTURE: Capture = Capture;
        log::set_logger(&CAPTURE).expect("one logger per test binary");
        log::set_max_level(log::LevelFilter::Trace);
    });
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Fixture.

fn fake() -> SharedFake {
    SharedFake::new(FakeRemote::with_hasher(content_hash))
}

struct Fx {
    notes: TempDir,
    state: TempDir,
    trash: TempDir,
    home: TempDir,
    /// The engine's Dropbox and "another device" share one fake.
    device: SharedFake,
}

impl Fx {
    fn new() -> Self {
        install_logger();
        let fx = Self {
            notes: tempdir().unwrap(),
            state: tempdir().unwrap(),
            trash: tempdir().unwrap(),
            home: tempdir().unwrap(),
            device: fake(),
        };
        // The real layout: the file-store login sits in the state dir beside
        // sync-state.json, so every sweep checks the token never leaks next door.
        FileStore::new(fx.state.path())
            .save(&StoredLogin {
                refresh_token: Secret::new(REFRESH_SENTINEL),
                account_id: ACCOUNT.into(),
                email: None,
            })
            .unwrap();
        fx
    }

    fn opts_for(&self, notes: &Path, account: &str) -> EngineOptions {
        EngineOptions {
            notes_dir: notes.to_owned(),
            state_dir: self.state.path().to_owned(),
            account_id: account.into(),
            config_loaded_ok: true,
            home_dir: Some(self.home.path().to_owned()),
            debounce: Duration::from_millis(50),
        }
    }

    fn engine(&self) -> Engine {
        self.engine_with(self.opts_for(self.notes.path(), ACCOUNT), &self.device)
    }

    fn engine_with(&self, opts: EngineOptions, remote: &SharedFake) -> Engine {
        Engine::new(
            opts,
            Box::new(remote.clone()),
            Box::new(FakeTrasher {
                dir: self.trash.path().to_owned(),
            }),
        )
        .unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.notes.path().join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        fs::write(self.path(name), bytes).unwrap();
    }

    fn remove(&self, name: &str) {
        fs::remove_file(self.path(name)).unwrap();
    }

    fn local(&self) -> BTreeMap<String, Vec<u8>> {
        files_in(self.notes.path())
    }

    fn trashed(&self) -> BTreeMap<String, Vec<u8>> {
        files_in(self.trash.path())
    }

    fn remote(&self) -> BTreeMap<String, Vec<u8>> {
        remote_notes(&self.device)
    }

    fn writes(&self) -> usize {
        self.device.with(|fake| fake.write_count())
    }

    fn edit_remotely(&self, name: &str, bytes: &[u8]) {
        self.device.with(|fake| {
            fake.edit_remotely(name, bytes);
        });
    }

    fn delete_remotely(&self, name: &str) {
        assert!(self.device.with(|fake| fake.delete_remotely(name)));
    }

    fn assert_converged(&self) {
        assert_eq!(self.local(), self.remote(), "local and remote diverge");
    }

    fn survives(&self, text: &[u8]) -> bool {
        let local = self.local();
        let remote = self.remote();
        let trash = self.trashed();
        local
            .values()
            .chain(remote.values())
            .chain(trash.values())
            .any(|bytes| bytes == text)
    }

    /// The invariant sweep run after every scenario.
    fn sweep(&self, known: Option<&BTreeSet<Vec<u8>>>) {
        sweep_notes(self.notes.path(), known);
        sweep_notes(self.trash.path(), known);
        sweep_state(self.state.path());
        sweep_logs();
    }
}

fn files_in(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .map(|name| {
            let bytes = fs::read(dir.join(&name)).unwrap();
            (name, bytes)
        })
        .collect()
}

/// Remote notes by display name (non-note entries excluded).
fn remote_notes(device: &SharedFake) -> BTreeMap<String, Vec<u8>> {
    device.with(|fake| {
        fake.snapshot()
            .into_values()
            .filter(|(name, _)| is_note_name(name))
            .collect()
    })
}

/// No dot-prefixed temp file left, no token, and (when known) no content that
/// is not one of the complete texts ever written: a partial write shows up here.
fn sweep_notes(dir: &Path, known: Option<&BTreeSet<Vec<u8>>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        assert!(
            !name.starts_with('.'),
            "temp file left behind: {}",
            entry.path().display()
        );
        if entry.metadata().unwrap().len() >= MAX_SIZE {
            continue;
        }
        let bytes = fs::read(entry.path()).unwrap();
        for token in [REFRESH_SENTINEL, ACCESS_SENTINEL] {
            assert!(
                !contains(&bytes, token),
                "token in {}",
                entry.path().display()
            );
        }
        if let Some(known) = known {
            assert!(
                known.contains(&bytes),
                "partial or unknown content in {name}: {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }
}

fn sweep_state(dir: &Path) {
    let mut planted = false;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        assert!(
            !name.starts_with('.'),
            "temp file left in state dir: {name}"
        );
        let bytes = fs::read(entry.path()).unwrap();
        assert!(
            !contains(&bytes, ACCESS_SENTINEL),
            "access token persisted in {name}"
        );
        if name == LOGIN_FILE {
            planted = contains(&bytes, REFRESH_SENTINEL);
        } else {
            assert!(
                !contains(&bytes, REFRESH_SENTINEL),
                "refresh token leaked into {name}"
            );
        }
    }
    assert!(planted, "the planted login must still be in place");
}

fn sweep_logs() {
    for line in lock(&LOGS).iter() {
        for token in [REFRESH_SENTINEL, ACCESS_SENTINEL] {
            assert!(
                !line.message.contains(token),
                "token logged by {}: {}",
                line.target,
                line.message
            );
        }
    }
}

/// Run passes until one does nothing; return the reports.
fn settle(engine: &mut Engine, allow_deletes: bool) -> Vec<PassReport> {
    let mut reports = Vec::new();
    for _ in 0..8 {
        let report = engine.run_pass(allow_deletes).unwrap();
        let quiet = report.actions() == 0;
        reports.push(report);
        if quiet {
            return reports;
        }
    }
    panic!("no quiescence: {reports:?}");
}

#[test]
fn same_size_save_during_upload_converges() {
    struct SaveDuringUpload {
        remote: SharedFake,
        path: PathBuf,
        saved: bool,
    }

    impl Remote for SaveDuringUpload {
        fn list(&mut self) -> RemoteResult<(Vec<RemoteFile>, Cursor)> {
            self.remote.list()
        }

        fn changes(&mut self, since: &Cursor) -> RemoteResult<(Vec<RemoteChange>, Cursor)> {
            self.remote.changes(since)
        }

        fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
            self.remote.download(id)
        }

        fn upload(
            &mut self,
            name: &str,
            bytes: &[u8],
            expect_rev: Option<&str>,
        ) -> RemoteResult<RemoteFile> {
            if !self.saved {
                assert_eq!(bytes, b"bbbb");
                let mtime = fs::metadata(&self.path).unwrap().modified().unwrap();
                fs::write(&self.path, b"cccc").unwrap();
                File::open(&self.path)
                    .unwrap()
                    .set_modified(mtime + Duration::from_secs(2))
                    .unwrap();
                self.saved = true;
            }
            self.remote.upload(name, bytes, expect_rev)
        }

        fn delete(&mut self, id: &str, parent_rev: &str) -> RemoteResult<()> {
            self.remote.delete(id, parent_rev)
        }
    }

    let fx = Fx::new();
    fx.write("note.txt", b"bbbb");
    let mut engine = Engine::new(
        fx.opts_for(fx.notes.path(), ACCOUNT),
        Box::new(SaveDuringUpload {
            remote: fx.device.clone(),
            path: fx.path("note.txt"),
            saved: false,
        }),
        Box::new(FakeTrasher {
            dir: fx.trash.path().to_owned(),
        }),
    )
    .unwrap();
    settle(&mut engine, false);
    assert_eq!(fx.local()["note.txt"], b"cccc");
    assert_eq!(fx.remote()["note.txt"], b"cccc");
    fx.assert_converged();
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

fn assert_idempotent(fx: &Fx, engine: &mut Engine) {
    let writes = fx.writes();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(
        (report.actions(), report.network_writes),
        (0, 0),
        "{report:?}"
    );
    assert_eq!(fx.writes(), writes);
    assert_eq!(report.attention, None);
}

fn conflict_copies(files: &BTreeMap<String, Vec<u8>>) -> Vec<(&String, &Vec<u8>)> {
    files
        .iter()
        .filter(|(name, _)| name.contains("(conflict "))
        .collect()
}

/// (uploaded, downloaded, conflicts, trashed, deleted_remote, adopted)
fn counts(report: &PassReport) -> (usize, usize, usize, usize, usize, usize) {
    (
        report.uploaded,
        report.downloaded,
        report.conflicts,
        report.trashed,
        report.deleted_remote,
        report.adopted,
    )
}

fn is_crash(error: &Error) -> bool {
    matches!(error, Error::Io(io) if io.to_string() == "injected crash")
}

// ---------------------------------------------------------------------------
// Model-based no-loss property.

const POOL: [&str; 5] = ["a.txt", "b.md", "c.txt", "d.text", "e.taskpaper"];

#[derive(Debug, Clone)]
enum Target {
    Pool(usize),
    Existing(usize),
}

/// A fault that fires just before the next action the engine applies.
#[derive(Debug, Clone)]
enum Mid {
    Crash,
    Conflict,
    Offline,
    RemoteEdit(usize),
    Touch(usize),
}

#[derive(Debug, Clone)]
enum Op {
    LocalWrite(Target),
    LocalDelete(usize),
    LocalRename(usize, usize),
    RemoteWrite(Target),
    RemoteDelete(usize),
    RemoteTouch(usize),
    /// Another device writes exactly the local bytes (exercises Adopt).
    RemoteCopyLocal(usize),
    Pass {
        allow_deletes: bool,
    },
    Offline,
    RateLimited,
    ExpireCursor,
    MidPass(Mid),
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        (0..POOL.len()).prop_map(Target::Pool),
        any::<usize>().prop_map(Target::Existing),
    ]
}

fn mid() -> impl Strategy<Value = Mid> {
    prop_oneof![
        Just(Mid::Crash),
        Just(Mid::Conflict),
        Just(Mid::Offline),
        any::<usize>().prop_map(Mid::RemoteEdit),
        any::<usize>().prop_map(Mid::Touch),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => target().prop_map(Op::LocalWrite),
        1 => any::<usize>().prop_map(Op::LocalDelete),
        1 => (any::<usize>(), 0..POOL.len()).prop_map(|(from, to)| Op::LocalRename(from, to)),
        3 => target().prop_map(Op::RemoteWrite),
        1 => any::<usize>().prop_map(Op::RemoteDelete),
        1 => any::<usize>().prop_map(Op::RemoteTouch),
        1 => any::<usize>().prop_map(Op::RemoteCopyLocal),
        4 => prop::bool::weighted(0.25).prop_map(|allow_deletes| Op::Pass { allow_deletes }),
        1 => Just(Op::Offline),
        1 => Just(Op::RateLimited),
        1 => Just(Op::ExpireCursor),
        2 => mid().prop_map(Op::MidPass),
    ]
}

/// Every text is unique. `must` holds the texts no user has superseded: a text
/// leaves it only when the side holding it edits or deletes that file itself.
#[derive(Default)]
struct Ledger {
    next: usize,
    ever: BTreeSet<Vec<u8>>,
    must: BTreeSet<Vec<u8>>,
}

impl Ledger {
    fn fresh(&mut self, side: &str) -> Vec<u8> {
        self.next += 1;
        let text = format!("{side}{} {}\n", self.next, "~".repeat(self.next % 4)).into_bytes();
        self.ever.insert(text.clone());
        self.must.insert(text.clone());
        text
    }

    fn supersede(&mut self, old: Option<&Vec<u8>>) {
        if let Some(old) = old {
            self.must.remove(old);
        }
    }
}

fn same_key(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn pick(files: &BTreeMap<String, Vec<u8>>, index: &usize) -> Option<(String, Vec<u8>)> {
    if files.is_empty() {
        return None;
    }
    files
        .iter()
        .nth(*index % files.len())
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
}

fn resolve(files: &BTreeMap<String, Vec<u8>>, target: &Target) -> Option<String> {
    match target {
        Target::Pool(i) => Some(
            files
                .keys()
                .find(|name| same_key(name, POOL[*i]))
                .cloned()
                .unwrap_or_else(|| POOL[*i].to_owned()),
        ),
        Target::Existing(index) => pick(files, index).map(|(name, _)| name),
    }
}

fn remote_write(device: &SharedFake, ledger: &Mutex<Ledger>, target: &Target) {
    let files = remote_notes(device);
    let Some(name) = resolve(&files, target) else {
        return;
    };
    let mut ledger = lock(ledger);
    ledger.supersede(files.get(&name));
    let text = ledger.fresh("R");
    device.with(|fake| {
        fake.edit_remotely(&name, &text);
    });
}

fn remote_touch(device: &SharedFake, index: &usize) {
    if let Some((name, _)) = pick(&remote_notes(device), index) {
        device.with(|fake| fake.touch_remotely(&name));
    }
}

struct Sim {
    fx: Fx,
    engine: Option<Engine>,
    ledger: Arc<Mutex<Ledger>>,
    armed: Arc<Mutex<Option<Mid>>>,
    /// Upper bound on queued remote faults still unconsumed.
    faults: usize,
}

impl Sim {
    fn new() -> Self {
        let mut sim = Self {
            fx: Fx::new(),
            engine: None,
            ledger: Arc::default(),
            armed: Arc::default(),
            faults: 0,
        };
        sim.restart();
        sim
    }

    /// Drop the engine (releasing its lock) and start a fresh one on the same state.
    fn restart(&mut self) {
        self.engine = None;
        let mut engine = self.fx.engine();
        let armed = Arc::clone(&self.armed);
        let device = self.fx.device.clone();
        let ledger = Arc::clone(&self.ledger);
        engine.set_fault_hook(move |_| {
            let Some(mid) = lock(&armed).take() else {
                return false;
            };
            match mid {
                Mid::Crash => return true,
                Mid::Conflict => device.with(|fake| fake.fail_next(RemoteError::Conflict)),
                Mid::Offline => device.with(|fake| fake.fail_next(RemoteError::Offline)),
                Mid::RemoteEdit(index) => {
                    remote_write(&device, &ledger, &Target::Existing(index));
                }
                Mid::Touch(index) => remote_touch(&device, &index),
            }
            false
        });
        self.engine = Some(engine);
    }

    fn pass(&mut self, allow_deletes: bool) -> Option<PassReport> {
        let result = self.engine.as_mut().unwrap().run_pass(allow_deletes);
        match result {
            Ok(report) => Some(report),
            Err(error) if is_crash(&error) => {
                self.restart();
                None
            }
            Err(error) => {
                assert!(
                    matches!(
                        error,
                        Error::Remote(
                            RemoteError::Offline
                                | RemoteError::RateLimited(_)
                                | RemoteError::Conflict
                        )
                    ),
                    "unexpected pass error: {error}"
                );
                None
            }
        }
    }

    fn step(&mut self, op: &Op) {
        let fx = &self.fx;
        match op {
            Op::LocalWrite(target) => {
                let files = fx.local();
                let Some(name) = resolve(&files, target) else {
                    return;
                };
                let text = {
                    let mut ledger = lock(&self.ledger);
                    ledger.supersede(files.get(&name));
                    ledger.fresh("L")
                };
                fx.write(&name, &text);
            }
            Op::LocalDelete(index) => {
                if let Some((name, bytes)) = pick(&fx.local(), index) {
                    lock(&self.ledger).supersede(Some(&bytes));
                    fx.remove(&name);
                }
            }
            Op::LocalRename(index, to) => {
                let files = fx.local();
                let to = POOL[*to];
                if files.keys().any(|name| same_key(name, to)) {
                    return;
                }
                if let Some((from, _)) = pick(&files, index) {
                    fs::rename(fx.path(&from), fx.path(to)).unwrap();
                }
            }
            Op::RemoteWrite(target) => remote_write(&fx.device, &self.ledger, target),
            Op::RemoteDelete(index) => {
                if let Some((name, bytes)) = pick(&fx.remote(), index) {
                    lock(&self.ledger).supersede(Some(&bytes));
                    fx.delete_remotely(&name);
                }
            }
            Op::RemoteTouch(index) => remote_touch(&fx.device, index),
            Op::RemoteCopyLocal(index) => {
                if let Some((name, bytes)) = pick(&fx.local(), index) {
                    let remote = fx.remote();
                    let old = remote
                        .iter()
                        .find(|(other, _)| same_key(other, &name))
                        .map(|(_, old)| old);
                    if old != Some(&bytes) {
                        lock(&self.ledger).supersede(old);
                    }
                    fx.edit_remotely(&name, &bytes);
                }
            }
            Op::Pass { allow_deletes } => {
                self.pass(*allow_deletes);
            }
            Op::Offline => {
                fx.device.with(|fake| fake.fail_next(RemoteError::Offline));
                self.faults += 1;
            }
            Op::RateLimited => {
                fx.device.with(|fake| {
                    fake.fail_next(RemoteError::RateLimited(Duration::from_secs(1)));
                });
                self.faults += 1;
            }
            Op::ExpireCursor => fx.device.with(FakeRemote::expire_cursors),
            Op::MidPass(mid) => {
                if matches!(mid, Mid::Conflict | Mid::Offline) {
                    self.faults += 1;
                }
                *lock(&self.armed) = Some(mid.clone());
            }
        }
    }

    /// A final fault-free run to quiescence, then every property.
    fn finish(mut self) {
        lock(&self.armed).take();
        let mut quiet = None;
        for _ in 0..self.faults + 10 {
            if let Some(report) = self.pass(true) {
                if report.actions() == 0 {
                    quiet = Some(report);
                    break;
                }
            }
        }
        let quiet = quiet.expect("no quiescence after the final fault-free passes");
        assert_eq!(quiet.attention, None);
        assert!(quiet.skipped.is_empty(), "{:?}", quiet.skipped);

        let fx = &self.fx;
        // Idempotence: a second pass right after convergence does nothing.
        assert_idempotent(fx, self.engine.as_mut().unwrap());
        // Convergence: identical name -> content on both sides.
        fx.assert_converged();
        // No loss: every unsuperseded text survives somewhere.
        let ledger = lock(&self.ledger);
        for text in &ledger.must {
            assert!(
                fx.survives(text),
                "lost {:?}",
                String::from_utf8_lossy(text)
            );
        }
        let remote = fx.remote();
        for (name, bytes) in &remote {
            assert!(
                ledger.ever.contains(bytes),
                "unknown remote content in {name}"
            );
        }
        fx.sweep(Some(&ledger.ever));
    }
}

fn run_model(ops: &[Op]) {
    let mut sim = Sim::new();
    for op in ops {
        sim.step(op);
    }
    sim.finish();
}

fn model_config() -> ProptestConfig {
    // Default honours PROPTEST_CASES; never fewer than 256 cases. Failures are
    // not persisted to a file: a failing case is committed as a named test.
    let base = ProptestConfig::default();
    ProptestConfig {
        cases: base.cases.max(256),
        failure_persistence: None,
        ..base
    }
}

proptest! {
    #![proptest_config(model_config())]

    #[test]
    fn model_random_interleavings_converge_without_loss(
        ops in prop::collection::vec(op(), 1..28)
    ) {
        run_model(&ops);
    }
}

#[test]
fn model_fixed_sequence_with_every_fault() {
    use Op::*;
    let first = 0;
    run_model(&[
        LocalWrite(Target::Pool(0)),
        RemoteWrite(Target::Pool(1)),
        Pass {
            allow_deletes: false,
        },
        LocalWrite(Target::Pool(0)),
        RemoteWrite(Target::Pool(0)),
        MidPass(Mid::Crash),
        Pass {
            allow_deletes: false,
        },
        RemoteWrite(Target::Pool(1)),
        LocalWrite(Target::Pool(1)),
        MidPass(Mid::RemoteEdit(first)),
        Pass {
            allow_deletes: false,
        },
        Offline,
        Pass {
            allow_deletes: false,
        },
        LocalRename(first, 2),
        RateLimited,
        Pass {
            allow_deletes: false,
        },
        ExpireCursor,
        RemoteDelete(first),
        LocalWrite(Target::Pool(3)),
        MidPass(Mid::Conflict),
        Pass {
            allow_deletes: false,
        },
        RemoteCopyLocal(first),
        MidPass(Mid::Touch(first)),
        Pass {
            allow_deletes: true,
        },
        MidPass(Mid::Offline),
        LocalDelete(first),
    ]);
}

// ---------------------------------------------------------------------------
// Cold-start twins.

#[derive(Clone, Copy, Debug)]
enum Twin {
    LocalOnly,
    RemoteOnly,
    BothEqual,
    BothDifferent,
}

const TWINS: [Twin; 4] = [
    Twin::LocalOnly,
    Twin::RemoteOnly,
    Twin::BothEqual,
    Twin::BothDifferent,
];

#[test]
fn cold_start_twins_first_run_never_overwrites() {
    const NAME: &str = "twin.txt";
    for twin in TWINS {
        // Cold: empty state, the shape exists before the first sync ever.
        let fx = Fx::new();
        match twin {
            Twin::LocalOnly => fx.write(NAME, b"local text"),
            Twin::RemoteOnly => fx.edit_remotely(NAME, b"remote text"),
            Twin::BothEqual => {
                fx.write(NAME, b"same text");
                fx.edit_remotely(NAME, b"same text");
            }
            Twin::BothDifferent => {
                fx.write(NAME, b"local text");
                fx.edit_remotely(NAME, b"remote text");
            }
        }
        let before: Vec<Vec<u8>> = fx
            .local()
            .into_values()
            .chain(fx.remote().into_values())
            .collect();
        let mut engine = fx.engine();
        assert!(engine.state().is_cold());
        let report = engine.run_pass(false).unwrap();
        let expected = match twin {
            Twin::LocalOnly => (1, 0, 0, 0, 0, 0),
            Twin::RemoteOnly => (0, 1, 0, 0, 0, 0),
            Twin::BothEqual => (0, 0, 0, 0, 0, 1),
            // The conflict copy is uploaded in the same pass.
            Twin::BothDifferent => (1, 0, 1, 0, 0, 0),
        };
        assert_eq!(counts(&report), expected, "cold {twin:?}");
        let local = fx.local();
        for text in &before {
            assert!(
                local.values().any(|bytes| bytes == text),
                "cold {twin:?} overwrote {:?}",
                String::from_utf8_lossy(text)
            );
        }
        assert!(fx.trashed().is_empty(), "cold {twin:?} trashed something");
        if let Twin::BothDifferent = twin {
            assert_eq!(local[NAME], b"local text");
            let copies = conflict_copies(&local);
            assert_eq!(copies.len(), 1);
            assert_eq!(copies[0].1, b"remote text");
        }
        if let Twin::BothEqual = twin {
            assert_eq!(fx.writes(), 0);
        }
        settle(&mut engine, false);
        fx.assert_converged();
        assert_idempotent(&fx, &mut engine);
        fx.sweep(None);
        drop(engine);

        // Steady twin: the same on-disk shape, reached from a synced base.
        let fx = Fx::new();
        fx.write(NAME, b"base text");
        let mut engine = fx.engine();
        settle(&mut engine, false);
        let expected = match twin {
            Twin::LocalOnly => {
                fx.delete_remotely(NAME);
                (0, 0, 0, 1, 0, 0)
            }
            Twin::RemoteOnly => {
                fx.remove(NAME);
                (0, 0, 0, 0, 1, 0)
            }
            Twin::BothEqual => {
                fx.write(NAME, b"same new text");
                fx.edit_remotely(NAME, b"same new text");
                (0, 0, 0, 0, 0, 1)
            }
            Twin::BothDifferent => {
                fx.write(NAME, b"local new text");
                fx.edit_remotely(NAME, b"remote new text");
                (1, 0, 1, 0, 0, 0)
            }
        };
        let writes = fx.writes();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(counts(&report), expected, "steady {twin:?}");
        match twin {
            Twin::LocalOnly => assert_eq!(fx.trashed()[NAME], b"base text"),
            Twin::BothEqual => assert_eq!(fx.writes(), writes),
            _ => {}
        }
        settle(&mut engine, false);
        fx.assert_converged();
        assert_idempotent(&fx, &mut engine);
        fx.sweep(None);
    }
}

fn mixed_cold_folder(fx: &Fx) -> Vec<&'static [u8]> {
    fx.write("local only.txt", b"local only");
    fx.edit_remotely("remote only.md", b"remote only");
    fx.write("equal.txt", b"equal both");
    fx.edit_remotely("equal.txt", b"equal both");
    fx.write("differ.txt", b"differ local");
    fx.edit_remotely("differ.txt", b"differ remote");
    vec![
        b"local only",
        b"remote only",
        b"equal both",
        b"differ local",
        b"differ remote",
    ]
}

#[test]
fn cold_start_mixed_folder_one_pass() {
    let fx = Fx::new();
    let texts = mixed_cold_folder(&fx);
    let mut engine = fx.engine();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(counts(&report), (2, 1, 1, 0, 0, 1), "{report:?}");
    assert_eq!(fx.local()["differ.txt"], b"differ local");
    for text in texts {
        assert!(fx.local().values().any(|bytes| bytes == text));
    }
    fx.assert_converged();
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

#[test]
fn cold_start_crash_at_every_action_still_never_overwrites() {
    let mut crashed = 0;
    for k in 0..8 {
        let fx = Fx::new();
        let texts = mixed_cold_folder(&fx);
        let mut engine = fx.engine();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        engine.set_fault_hook(move |index| {
            let hit = index == k;
            flag.fetch_or(hit, Ordering::SeqCst);
            hit
        });
        let result = engine.run_pass(false);
        let fired = fired.load(Ordering::SeqCst);
        assert_eq!(result.is_err(), fired, "k={k}");
        crashed += usize::from(fired);
        drop(engine);
        let mut engine = fx.engine();
        settle(&mut engine, false);
        fx.assert_converged();
        let local = fx.local();
        for text in &texts {
            assert!(
                local.values().any(|bytes| bytes == text),
                "k={k}: lost {:?}",
                String::from_utf8_lossy(text)
            );
        }
        assert_eq!(local["differ.txt"], b"differ local", "k={k}");
        assert_eq!(conflict_copies(&local).len(), 1, "k={k}");
        assert!(fx.trashed().is_empty(), "k={k}");
        assert_idempotent(&fx, &mut engine);
        fx.sweep(None);
    }
    // Upload, download, adopt, conflict, and the conflict copy's upload.
    assert_eq!(crashed, 5);
}

// ---------------------------------------------------------------------------
// Idempotence.

#[test]
fn idempotent_after_convergence_across_restart_and_cursor_reset() {
    let fx = Fx::new();
    for i in 0..6 {
        fx.write(&format!("local {i}.txt"), format!("local {i}").as_bytes());
        fx.edit_remotely(&format!("remote {i}.md"), format!("remote {i}").as_bytes());
    }
    let mut engine = fx.engine();
    settle(&mut engine, false);
    fx.assert_converged();
    assert_idempotent(&fx, &mut engine);
    // Across a restart: the persisted state alone must be enough.
    drop(engine);
    let mut engine = fx.engine();
    assert_idempotent(&fx, &mut engine);
    // A full re-list after an expired cursor must still find nothing to do.
    fx.device.with(FakeRemote::expire_cursors);
    assert_idempotent(&fx, &mut engine);
    // Touched-only remote revisions are adopted without any network write.
    fx.device.with(|fake| fake.touch_remotely("local 0.txt"));
    let writes = fx.writes();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(counts(&report), (0, 0, 0, 0, 0, 1));
    assert_eq!(fx.writes(), writes);
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

// ---------------------------------------------------------------------------
// Contract drift: the reconcile grid through plan::reconcile and the engine.

const T0: &[u8] = b"base text\n";
const T1: &[u8] = b"local change, longer\n";
const T2: &[u8] = b"remote change, different\n";
const GRID_NAME: &str = "note.txt";
const GRID_KEY: &str = "/note.txt";

#[derive(Clone, Copy, Debug)]
enum LocalCell {
    Absent,
    Base,
    Changed,
    Huge,
}

#[derive(Clone, Copy, Debug)]
enum RemoteCell {
    Absent,
    Base,
    Touched,
    SameAsLocalChange,
    Different,
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Upload { .. } => "upload",
        Action::Download { .. } => "download",
        Action::DeleteRemote { .. } => "delete-remote",
        Action::TrashLocal { .. } => "trash-local",
        Action::Adopt { .. } => "adopt",
        Action::Conflict { .. } => "conflict",
        Action::ForgetBase { .. } => "forget-base",
        Action::Skip { .. } => "skip",
        Action::Nothing { .. } => "nothing",
    }
}

/// Build one grid cell, compute reconcile's verdict from the same inputs the
/// engine sees, run one engine pass and check the engine did exactly that.
fn grid_cell(with_base: bool, local: LocalCell, remote: RemoteCell) -> &'static str {
    let cell = format!("base={with_base} local={local:?} remote={remote:?}");
    let fx = Fx::new();
    let mut engine = fx.engine();
    if with_base {
        fx.write(GRID_NAME, T0);
        settle(&mut engine, false);
    }
    match (with_base, remote) {
        (true, RemoteCell::Absent) => fx.delete_remotely(GRID_NAME),
        (true, RemoteCell::Base) | (false, RemoteCell::Absent) => {}
        (true, RemoteCell::Touched) => fx.device.with(|fake| fake.touch_remotely(GRID_NAME)),
        (false, RemoteCell::Base) => fx.edit_remotely(GRID_NAME, T0),
        (false, RemoteCell::Touched) => {
            fx.edit_remotely(GRID_NAME, T0);
            fx.device.with(|fake| fake.touch_remotely(GRID_NAME));
        }
        (_, RemoteCell::SameAsLocalChange) => fx.edit_remotely(GRID_NAME, T1),
        (_, RemoteCell::Different) => fx.edit_remotely(GRID_NAME, T2),
    }
    match (with_base, local) {
        (true, LocalCell::Absent) => fx.remove(GRID_NAME),
        (true, LocalCell::Base) | (false, LocalCell::Absent) => {}
        (false, LocalCell::Base) => fx.write(GRID_NAME, T0),
        (_, LocalCell::Changed) => fx.write(GRID_NAME, T1),
        (_, LocalCell::Huge) => {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(fx.path(GRID_NAME))
                .unwrap();
            file.set_len(MAX_SIZE).unwrap();
        }
    }

    let base = engine.state().files.get(GRID_KEY).cloned();
    let local_bytes = fs::read(fx.path(GRID_NAME)).ok();
    let local_file = local_bytes.as_ref().map(|bytes| LocalFile {
        name: GRID_NAME.into(),
        hash: content_hash(bytes),
        size: bytes.len() as u64,
    });
    let remote_file = fx.device.with(|fake| fake.file(GRID_NAME));
    let remote_bytes = fx.remote().get(GRID_NAME).cloned();
    let expected = reconcile(
        GRID_KEY,
        base.as_ref(),
        local_file.as_ref(),
        remote_file.as_ref(),
    );
    let writes = fx.writes();
    let report = engine.run_pass(false).unwrap();
    let kind = action_kind(&expected);
    let huge = matches!(local, LocalCell::Huge);
    let local_after = || fs::read(fx.path(GRID_NAME)).ok();
    match &expected {
        Action::Upload { .. } => {
            assert_eq!(counts(&report), (1, 0, 0, 0, 0, 0), "{cell}");
            assert_eq!(report.network_writes, 1, "{cell}");
            assert_eq!(fx.remote().get(GRID_NAME), local_bytes.as_ref(), "{cell}");
        }
        Action::Download { .. } => {
            assert_eq!(counts(&report), (0, 1, 0, 0, 0, 0), "{cell}");
            assert_eq!(report.network_writes, 0, "{cell}");
            assert_eq!(local_after(), remote_bytes, "{cell}");
        }
        Action::DeleteRemote { .. } => {
            assert_eq!(counts(&report), (0, 0, 0, 0, 1, 0), "{cell}");
            assert!(!fx.remote().contains_key(GRID_NAME), "{cell}");
        }
        Action::TrashLocal { .. } => {
            assert_eq!(counts(&report), (0, 0, 0, 1, 0, 0), "{cell}");
            assert!(local_after().is_none(), "{cell}");
            assert_eq!(fx.trashed().get(GRID_NAME), local_bytes.as_ref(), "{cell}");
        }
        Action::Adopt { .. } => {
            assert_eq!(counts(&report), (0, 0, 0, 0, 0, 1), "{cell}");
            assert_eq!(fx.writes(), writes, "{cell}");
            let base = &engine.state().files[GRID_KEY];
            assert_eq!(
                Some(&base.rev),
                remote_file.as_ref().map(|f| &f.rev),
                "{cell}"
            );
        }
        Action::Conflict { .. } => {
            // The copy of the remote text is uploaded in the pass's next round.
            assert_eq!(counts(&report), (1, 0, 1, 0, 0, 0), "{cell}");
            let local = fx.local();
            assert_eq!(local.get(GRID_NAME), local_bytes.as_ref(), "{cell}");
            let copies = conflict_copies(&local);
            assert_eq!(copies.len(), 1, "{cell}");
            assert_eq!(Some(copies[0].1), remote_bytes.as_ref(), "{cell}");
            assert_eq!(fx.remote().get(GRID_NAME), local_bytes.as_ref(), "{cell}");
        }
        Action::ForgetBase { .. } => {
            assert_eq!(report.actions(), 0, "{cell}");
            assert!(!engine.state().files.contains_key(GRID_KEY), "{cell}");
        }
        Action::Nothing { .. } => {
            assert_eq!((report.actions(), report.network_writes), (0, 0), "{cell}");
        }
        Action::Skip { reason, .. } => {
            assert_eq!(report.actions(), 0, "{cell}");
            assert_eq!(fx.writes(), writes, "{cell}");
            assert!(
                report
                    .skipped
                    .contains(&(GRID_NAME.to_owned(), reason.clone())),
                "{cell}: {:?}",
                report.skipped
            );
            assert_eq!(fx.remote().get(GRID_NAME), remote_bytes.as_ref(), "{cell}");
            assert_eq!(
                fs::metadata(fx.path(GRID_NAME)).unwrap().len(),
                MAX_SIZE,
                "{cell}"
            );
        }
    }
    if !huge {
        settle(&mut engine, false);
        fx.assert_converged();
        assert_idempotent(&fx, &mut engine);
    }
    fx.sweep(None);
    kind
}

#[test]
fn contract_drift_reconcile_grid_agrees_with_engine() {
    let mut seen = BTreeSet::new();
    for with_base in [false, true] {
        for local in [LocalCell::Absent, LocalCell::Base, LocalCell::Changed] {
            for remote in [
                RemoteCell::Absent,
                RemoteCell::Base,
                RemoteCell::Touched,
                RemoteCell::SameAsLocalChange,
                RemoteCell::Different,
            ] {
                seen.insert(grid_cell(with_base, local, remote));
            }
        }
    }
    let every = [
        "upload",
        "download",
        "delete-remote",
        "trash-local",
        "adopt",
        "conflict",
        "forget-base",
        "nothing",
    ];
    for kind in every {
        assert!(seen.contains(kind), "grid never produced {kind}: {seen:?}");
    }
}

#[test]
fn contract_drift_too_large_and_bad_name_cells() {
    assert_eq!(
        grid_cell(false, LocalCell::Huge, RemoteCell::Absent),
        "skip"
    );
    assert_eq!(grid_cell(true, LocalCell::Huge, RemoteCell::Base), "skip");
    assert_eq!(
        grid_cell(true, LocalCell::Huge, RemoteCell::Different),
        "skip"
    );

    // A local name Dropbox rejects: reconcile and the engine both skip it.
    let fx = Fx::new();
    let name = "bad\\name.txt";
    fx.write(name, b"bad name");
    let local = LocalFile {
        name: name.into(),
        hash: content_hash(b"bad name"),
        size: 8,
    };
    let key = "/bad\\name.txt";
    assert_eq!(
        reconcile(key, None, Some(&local), None),
        Action::Skip {
            key: key.into(),
            reason: SkipReason::BadName
        }
    );
    let mut engine = fx.engine();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.actions(), 0);
    assert_eq!(report.skipped, vec![(name.to_owned(), SkipReason::BadName)]);
    assert_eq!(fx.writes(), 0);
    fx.sweep(None);
}

// ---------------------------------------------------------------------------
// Failure modes the single-threaded property cannot reach.

/// A concurrent writer calling NoteStore::write while passes run and another
/// device keeps editing the same note. Each write appends a unique line, so a
/// line that is on no side afterwards is text sync destroyed.
#[test]
fn concurrent_note_store_writer_loses_no_line() {
    let fx = Fx::new();
    fx.write("shared.txt", b"seed\n");
    let mut engine = fx.engine();
    settle(&mut engine, false);
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let dir = fx.notes.path().to_owned();
        let trash = fx.trash.path().to_owned();
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut store =
                NoteStore::open(&dir, "txt", Box::new(FakeTrasher { dir: trash })).unwrap();
            let (id, path) = store
                .all()
                .iter()
                .find(|note| note.title == "shared")
                .map(|note| (note.id, note.path.clone()))
                .expect("shared note loaded");
            let mut written = Vec::new();
            let mut i = 0;
            while !stop.load(Ordering::Acquire) && i < 3000 {
                let line = format!("W{i}:writer");
                let body = format!("{}{line}\n", store.get(id).unwrap().body);
                match store.write(id, &body) {
                    Ok(()) => {
                        written.push(line);
                        i += 1;
                    }
                    Err(nv_core::Error::ExternalChange(_)) => {
                        // Sync changed the file: pick up what is on disk now.
                        while store.reload(&path).is_err() {
                            thread::yield_now();
                        }
                    }
                    Err(error) => panic!("writer failed: {error}"),
                }
                thread::sleep(Duration::from_micros(500));
            }
            written
        })
    };
    for k in 0..40 {
        // The other device edits the synced note it holds: it appends, so it
        // never supersedes writer lines it has already received.
        let mut text = fx.remote()["shared.txt"].clone();
        text.extend_from_slice(format!("R{k}:remote\n").as_bytes());
        fx.edit_remotely("shared.txt", &text);
        engine.run_pass(false).unwrap();
    }
    stop.store(true, Ordering::Release);
    let written = writer.join().unwrap();
    assert!(written.len() > 10, "writer barely ran: {}", written.len());
    settle(&mut engine, false);
    fx.assert_converged();

    let files: Vec<Vec<u8>> = fx
        .local()
        .into_values()
        .chain(fx.remote().into_values())
        .chain(fx.trashed().into_values())
        .collect();
    let lines: HashSet<&str> = files
        .iter()
        .flat_map(|bytes| std::str::from_utf8(bytes).unwrap().lines())
        .collect();
    let lost: Vec<&String> = written
        .iter()
        .filter(|line| !lines.contains(line.as_str()))
        .collect();
    assert!(
        lost.is_empty(),
        "{} of {} writer lines lost: {lost:?}",
        lost.len(),
        written.len()
    );
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

/// An external editor writing in place (truncate, then chunks) while passes
/// run: the remote may briefly hold a consistent prefix, never a mix of two
/// versions, and the final state is the complete last version on both sides.
#[test]
fn torn_in_place_write_never_syncs_garbage() {
    let fx = Fx::new();
    let versions: Vec<Vec<u8>> = (0..=25)
        .map(|i| format!("version {i:02}\n{}", "abcdefghij\n".repeat(1500 + i)).into_bytes())
        .collect();
    fx.write("doc.txt", &versions[0]);
    let mut engine = fx.engine();
    settle(&mut engine, false);
    let done = Arc::new(AtomicBool::new(false));
    let editor = {
        let path = fx.path("doc.txt");
        let versions = versions.clone();
        let done = Arc::clone(&done);
        thread::spawn(move || {
            for version in &versions[1..] {
                let mut file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .unwrap();
                for chunk in version.chunks(1024) {
                    file.write_all(chunk).unwrap();
                    thread::yield_now();
                }
                drop(file);
                thread::sleep(Duration::from_millis(2));
            }
            done.store(true, Ordering::Release);
        })
    };
    let is_prefix = |bytes: &[u8]| versions.iter().any(|version| version.starts_with(bytes));
    let mut passes = 0;
    while !done.load(Ordering::Acquire) {
        engine.run_pass(false).unwrap();
        passes += 1;
        if let Some(remote) = fx.remote().get("doc.txt") {
            assert!(is_prefix(remote), "remote holds a mix of two versions");
        }
    }
    editor.join().unwrap();
    assert!(passes > 0);
    settle(&mut engine, false);
    let last = versions.last().unwrap();
    assert_eq!(&fx.local()["doc.txt"], last);
    fx.assert_converged();
    assert!(conflict_copies(&fx.local()).is_empty());
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

#[test]
fn remote_non_note_and_folder_entries_are_ignored() {
    let fx = Fx::new();
    fx.device.with(|fake| {
        fake.add_non_note("folder.txt");
        fake.edit_remotely("photo.jpg", b"jpeg bytes");
        fake.edit_remotely("real.txt", b"real note");
    });
    fx.write("mine.md", b"my note");
    let mut engine = fx.engine();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(counts(&report), (1, 1, 0, 0, 0, 0));
    let local = fx.local();
    assert_eq!(
        local.keys().collect::<Vec<_>>(),
        vec!["mine.md", "real.txt"]
    );

    // Churn on the ignored entries produces nothing, by delta or by re-list.
    fx.device.with(|fake| {
        fake.touch_remotely("folder.txt");
        fake.edit_remotely("photo.jpg", b"new jpeg");
    });
    assert_idempotent(&fx, &mut engine);
    fx.device.with(FakeRemote::expire_cursors);
    assert_idempotent(&fx, &mut engine);
    fx.device.with(|fake| {
        fake.delete_remotely("photo.jpg");
    });
    assert_idempotent(&fx, &mut engine);
    assert!(fx.device.with(|fake| fake.file("folder.txt")).is_some());

    // A local note whose name is taken by a remote folder: bounded retries,
    // attention, the local text untouched and the folder not replaced.
    fx.write("folder.txt", b"note named like the folder");
    let writes = fx.writes();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.uploaded, 0);
    assert!(report.attention.is_some(), "{report:?}");
    assert_eq!(fx.writes(), writes);
    assert_eq!(fx.local()["folder.txt"], b"note named like the folder");
    assert!(fx.trashed().is_empty());
    fx.sweep(None);
}

fn synced(names: &[String]) -> (Fx, Engine) {
    let fx = Fx::new();
    for name in names {
        fx.write(name, format!("text of {name}").as_bytes());
    }
    let mut engine = fx.engine();
    settle(&mut engine, false);
    fx.assert_converged();
    (fx, engine)
}

fn names(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("note {i:02}.txt")).collect()
}

#[test]
fn mass_guard_half_arm_blocks_local_deletes_but_not_new_work() {
    let names = names(4);
    let (fx, mut engine) = synced(&names);
    for name in &names[..3] {
        fx.remove(name);
    }
    fx.write("fresh.txt", b"new work still syncs");
    let report = engine.run_pass(false).unwrap();
    assert_eq!(counts(&report), (1, 0, 0, 0, 0, 0));
    assert!(report
        .attention
        .as_deref()
        .is_some_and(|message| message.contains("this would remove 3 notes")));
    assert_eq!(fx.remote().len(), 5);
    // Guarded work is not forgotten across a restart, and stays guarded.
    drop(engine);
    let mut engine = fx.engine();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.deleted_remote, 0);
    assert!(report.attention.is_some());
    let report = engine.run_pass(true).unwrap();
    assert_eq!(report.deleted_remote, 3);
    fx.assert_converged();
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

#[test]
fn mass_guard_thresholds_both_directions() {
    let names = names(30);
    let (fx, mut engine) = synced(&names);
    // Ten remote deletions of thirty: under both arms, applied.
    for name in &names[..10] {
        fx.delete_remotely(name);
    }
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.trashed, 10);
    assert_eq!(report.attention, None);
    assert_eq!(fx.trashed().len(), 10);
    // Eleven local deletions of the remaining twenty: the ">10" arm.
    for name in &names[10..21] {
        fx.remove(name);
    }
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.deleted_remote, 0);
    assert!(report.attention.is_some());
    assert_eq!(fx.remote().len(), 20);
    // One remote deletion would pass alone; together with the eleven pending
    // local ones the pass stays guarded, and the flag releases both directions.
    fx.delete_remotely(&names[21]);
    let report = engine.run_pass(false).unwrap();
    assert_eq!((report.deleted_remote, report.trashed), (0, 0));
    let report = engine.run_pass(true).unwrap();
    assert_eq!((report.deleted_remote, report.trashed), (11, 1));
    fx.assert_converged();
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

#[test]
fn mass_guard_unmounted_folder_and_emptied_dropbox() {
    // An empty notes folder (unmounted drive) must not empty Dropbox.
    let names = names(5);
    let (fx, mut engine) = synced(&names);
    let hidden = tempdir().unwrap();
    for name in &names {
        fs::rename(fx.path(name), hidden.path().join(name)).unwrap();
    }
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.deleted_remote, 0);
    assert!(report.attention.is_some());
    assert_eq!(fx.remote().len(), 5);
    // Remount: everything is back to in-sync, no action at all.
    for name in &names {
        fs::rename(hidden.path().join(name), fx.path(name)).unwrap();
    }
    assert_idempotent(&fx, &mut engine);

    // A Dropbox folder emptied remotely must not trash local notes, even
    // after a restart resumes the pending deletions.
    for name in &names {
        fx.delete_remotely(name);
    }
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.trashed, 0);
    drop(engine);
    let mut engine = fx.engine();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(report.trashed, 0);
    assert!(report.attention.is_some());
    assert_eq!(fx.local().len(), 5);
    assert!(fx.trashed().is_empty());
    fx.sweep(None);
}

#[test]
fn account_switch_resets_to_cold_start() {
    let names = names(3);
    let (fx, engine) = synced(&names);
    let old_writes = fx.writes();
    drop(engine);
    // Account B's Dropbox holds a different note 00 and a note of its own.
    let other = fake();
    other.with(|fake| {
        fake.edit_remotely(&names[0], b"account B's version");
        fake.edit_remotely("only in b.txt", b"from account B");
    });
    let mut engine = fx.engine_with(fx.opts_for(fx.notes.path(), "dbid:model-b"), &other);
    assert!(engine.state().is_cold());
    let report = engine.run_pass(false).unwrap();
    // Without the reset, notes 01 and 02 (known, absent in B) would be trashed.
    assert_eq!(
        (report.trashed, report.deleted_remote),
        (0, 0),
        "{report:?}"
    );
    assert_eq!(report.conflicts, 1);
    assert_eq!(report.downloaded, 1);
    let local = fx.local();
    assert_eq!(local[&names[0]], format!("text of {}", names[0]).as_bytes());
    assert_eq!(conflict_copies(&local)[0].1, b"account B's version");
    assert_eq!(local, remote_notes(&other));
    assert_eq!(engine.state().account_id.as_deref(), Some("dbid:model-b"));
    // Account A's Dropbox was never touched.
    assert_eq!(fx.writes(), old_writes);
    assert_eq!(fx.remote().len(), 3);
    fx.sweep(None);
}

#[test]
fn folder_switch_resets_to_cold_start() {
    let names = names(3);
    let (fx, engine) = synced(&names);
    drop(engine);
    let second = tempdir().unwrap();
    fs::write(second.path().join("new here.txt"), b"second folder note").unwrap();
    let mut engine = fx.engine_with(fx.opts_for(second.path(), ACCOUNT), &fx.device);
    assert!(engine.state().is_cold());
    let report = engine.run_pass(false).unwrap();
    // Without the reset, all three known notes (absent here) would be deleted remotely.
    assert_eq!(
        (report.trashed, report.deleted_remote),
        (0, 0),
        "{report:?}"
    );
    assert_eq!((report.downloaded, report.uploaded), (3, 1));
    assert_eq!(files_in(second.path()), fx.remote());
    drop(engine);
    // And back to the first folder: cold again, everything equal or new.
    let mut engine = fx.engine();
    assert!(engine.state().is_cold());
    let writes = fx.writes();
    let report = engine.run_pass(false).unwrap();
    assert_eq!(counts(&report), (0, 1, 0, 0, 0, 3), "{report:?}");
    assert_eq!(fx.writes(), writes);
    fx.assert_converged();
    fx.sweep(None);
    sweep_notes(second.path(), None);
}

#[test]
fn retry_is_bounded_per_pass_for_upload_and_delete() {
    let names = vec![
        "up.txt".to_owned(),
        "del.txt".to_owned(),
        "keep.txt".to_owned(),
    ];
    let (fx, mut engine) = synced(&names);
    fx.write("up.txt", b"local edit, longer than before");
    fx.remove("del.txt");
    // Another device touches both notes right before every action, so each
    // conditional write meets a newer revision.
    let attempts = Arc::new(AtomicUsize::new(0));
    {
        let attempts = Arc::clone(&attempts);
        let device = fx.device.clone();
        engine.set_fault_hook(move |_| {
            attempts.fetch_add(1, Ordering::SeqCst);
            device.with(|fake| {
                fake.touch_remotely("up.txt");
                fake.touch_remotely("del.txt");
            });
            false
        });
    }
    let report = engine.run_pass(false).unwrap();
    assert_eq!((report.uploaded, report.deleted_remote), (0, 0));
    assert!(report
        .attention
        .as_deref()
        .is_some_and(|message| message.contains("keeps changing")));
    // Two actions in each of the three rounds, then the pass gives up.
    assert_eq!(attempts.load(Ordering::SeqCst), 6);
    assert_eq!(fx.remote()["up.txt"], b"text of up.txt");
    assert!(fx.remote().contains_key("del.txt"));
    assert_eq!(fx.local()["up.txt"], b"local edit, longer than before");
    // Once the other device stops, the next pass completes both.
    engine.set_fault_hook(|_| false);
    let report = engine.run_pass(false).unwrap();
    assert_eq!((report.uploaded, report.deleted_remote), (1, 1));
    fx.assert_converged();
    assert_idempotent(&fx, &mut engine);
    fx.sweep(None);
}

/// A Conflict whose local file changes between the scan and the apply
/// writes its conflict copy, then replans; the next round writes a second
/// copy of the very same remote text.
#[test]
fn conflict_replan_does_not_duplicate_conflict_copies() {
    let fx = Fx::new();
    fx.write("note.txt", b"base");
    let mut engine = fx.engine();
    settle(&mut engine, false);
    fx.write("note.txt", b"local edit one");
    fx.edit_remotely("note.txt", b"remote edit");
    // The user saves again right before the first action is applied.
    let saved = Arc::new(AtomicBool::new(false));
    {
        let saved = Arc::clone(&saved);
        let path = fx.path("note.txt");
        engine.set_fault_hook(move |_| {
            if !saved.swap(true, Ordering::SeqCst) {
                fs::write(&path, b"local edit two, typed during the pass").unwrap();
            }
            false
        });
    }
    let report = engine.run_pass(false).unwrap();
    assert!(saved.load(Ordering::SeqCst));
    assert_eq!(report.conflicts, 1, "{report:?}");
    let local = fx.local();
    assert_eq!(local["note.txt"], b"local edit two, typed during the pass");
    let copies = conflict_copies(&local);
    assert_eq!(
        copies.len(),
        1,
        "one remote text, several copies: {:?}",
        copies.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    fx.assert_converged();
}

#[test]
fn local_file_at_size_limit_is_skipped_with_reason() {
    let fx = Fx::new();
    fx.write("grow.txt", b"small for now");
    fx.write("small.txt", b"small");
    let mut engine = fx.engine();
    settle(&mut engine, false);
    // Sparse, so creating it is instant.
    File::create(fx.path("huge.txt"))
        .unwrap()
        .set_len(MAX_SIZE)
        .unwrap();
    OpenOptions::new()
        .write(true)
        .open(fx.path("grow.txt"))
        .unwrap()
        .set_len(MAX_SIZE + 1)
        .unwrap();
    let writes = fx.writes();
    for _ in 0..2 {
        let report = engine.run_pass(false).unwrap();
        assert_eq!((report.actions(), report.network_writes), (0, 0));
        let mut skipped = report.skipped.clone();
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            skipped,
            vec![
                ("grow.txt".to_owned(), SkipReason::TooLarge),
                ("huge.txt".to_owned(), SkipReason::TooLarge),
            ]
        );
    }
    assert_eq!(fx.writes(), writes);
    let remote = fx.remote();
    assert!(!remote.contains_key("huge.txt"));
    // The grown note is neither deleted nor overwritten on either side.
    assert_eq!(remote["grow.txt"], b"small for now");
    assert_eq!(
        fs::metadata(fx.path("grow.txt")).unwrap().len(),
        MAX_SIZE + 1
    );
    assert!(fx.trashed().is_empty());
    fx.sweep(None);
}

// ---------------------------------------------------------------------------
// Tokens and logs: the real Dropbox client against a loopback mock server.

const REMOTE_BODY: &[u8] = b"text from the mock Dropbox\n";

struct Mock {
    ep: Endpoints,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Vec<Seen>>>,
}

#[derive(Debug)]
struct Seen {
    path: String,
    authorization: Option<String>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let ep = Endpoints {
            authorize: format!("{root}/authorize"),
            api: root.clone(),
            content: root.clone(),
            notify: root,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut seen = Vec::new();
            while !flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => seen.push(serve_one(stream)),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
            seen
        });
        Self {
            ep,
            stop,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> Vec<Seen> {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap()
    }
}

fn serve_one(mut stream: TcpStream) -> Seen {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let mut lines = head.split("\r\n");
    let path = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .map_or(0, |value| value.parse::<usize>().unwrap());
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    let (status, reply) = route(&path, &headers, &body);
    write!(
        stream,
        "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.len()
    )
    .unwrap();
    stream.write_all(&reply).unwrap();
    Seen {
        path,
        authorization: headers.get("authorization").cloned(),
    }
}

fn route(path: &str, headers: &BTreeMap<String, String>, body: &[u8]) -> (u16, Vec<u8>) {
    let json = |value: Value| serde_json::to_vec(&value).unwrap();
    match path {
        "/oauth2/token" => (
            200,
            json(json!({"access_token": ACCESS_SENTINEL, "expires_in": 14400})),
        ),
        "/2/files/list_folder" => (
            200,
            json(json!({
                "entries": [
                    {".tag": "file", "id": "id:remote", "name": "From Dropbox.txt",
                     "path_lower": "/from dropbox.txt", "rev": "0000000001",
                     "content_hash": content_hash(REMOTE_BODY), "size": REMOTE_BODY.len()},
                    {".tag": "folder", "id": "id:folder", "name": "folder.txt",
                     "path_lower": "/folder.txt"},
                    {".tag": "file", "id": "id:photo", "name": "photo.jpg",
                     "path_lower": "/photo.jpg", "rev": "0000000002",
                     "content_hash": content_hash(b"jpeg"), "size": 4},
                ],
                "cursor": "mock-cursor-1",
                "has_more": false,
            })),
        ),
        "/2/files/list_folder/continue" => (
            200,
            json(json!({"entries": [], "cursor": "mock-cursor-1", "has_more": false})),
        ),
        "/2/files/download" => (200, REMOTE_BODY.to_vec()),
        "/2/files/upload" => {
            let arg: Value = serde_json::from_str(&headers["dropbox-api-arg"]).unwrap();
            let path = arg["path"].as_str().unwrap();
            (
                200,
                json(json!({
                    ".tag": "file", "id": "id:uploaded",
                    "name": path.trim_start_matches('/'), "path_lower": path.to_lowercase(),
                    "rev": "0000000003", "content_hash": content_hash(body), "size": body.len(),
                })),
            )
        }
        _ => (404, b"{}".to_vec()),
    }
}

/// Run the real Dropbox client (login from the file store) through the engine
/// on this thread; return the report of the first pass and the mock's log.
fn loopback_pass(fx: &Fx) -> (PassReport, Vec<Seen>) {
    let mock = Mock::start();
    let login = FileStore::new(fx.state.path()).load().unwrap().unwrap();
    assert_eq!(login.refresh_token.expose(), REFRESH_SENTINEL);
    let remote = DropboxRemote::with_endpoints("model-app-key", login.refresh_token, &mock.ep);
    let mut engine = Engine::new(
        fx.opts_for(fx.notes.path(), ACCOUNT),
        Box::new(remote),
        Box::new(FakeTrasher {
            dir: fx.trash.path().to_owned(),
        }),
    )
    .unwrap();
    let report = engine.run_pass(false).unwrap();
    let again = engine.run_pass(false).unwrap();
    assert_eq!(again.actions(), 0, "{again:?}");
    drop(engine);
    (report, mock.finish())
}

fn this_thread_logs() -> Vec<(log::Level, String, String)> {
    let me = thread::current().id();
    lock(&LOGS)
        .iter()
        .filter(|line| line.thread == me)
        .map(|line| (line.level, line.target.clone(), line.message.clone()))
        .collect()
}

#[test]
fn tokens_never_reach_disk_or_ureq_request_logs() {
    let fx = Fx::new();
    fx.write("mine.txt", b"uploaded through the real client");
    let (report, seen) = loopback_pass(&fx);
    assert_eq!(counts(&report), (1, 1, 0, 0, 0, 0), "{report:?}");
    assert_eq!(fx.local()["From Dropbox.txt"], REMOTE_BODY);
    assert!(!fx.local().contains_key("folder.txt"));
    assert!(!fx.local().contains_key("photo.jpg"));
    // The wire really carried the bearer token, so its absence below means something.
    let bearer = format!("Bearer {ACCESS_SENTINEL}");
    assert!(seen
        .iter()
        .any(|request| request.authorization.as_deref() == Some(bearer.as_str())));
    assert!(seen.iter().any(|request| request.path == "/oauth2/token"));

    let logs = this_thread_logs();
    let requests: Vec<&(log::Level, String, String)> = logs
        .iter()
        .filter(|(level, target, _)| target.starts_with("ureq") && *level <= log::Level::Debug)
        .collect();
    assert!(
        !requests.is_empty(),
        "ureq logged no request at debug level"
    );
    for (_, target, message) in &requests {
        for secret in [REFRESH_SENTINEL, ACCESS_SENTINEL, bearer.as_str()] {
            assert!(
                !message.contains(secret),
                "{target} logged a token: {message}"
            );
        }
        assert!(
            !message.to_ascii_lowercase().contains("authorization"),
            "{target} logged the Authorization header: {message}"
        );
    }
    fx.sweep(None);
}

/// ureq-proto hex-dumps every raw request (and response body) at TRACE
/// level, which would carry the bearer token and the /oauth2/token reply.
/// nv-sync caps the `log` crate at Debug at compile time, so even with the
/// capture logger at max level no TRACE record can exist from any target.
#[test]
fn tokens_never_reach_ureq_trace_hexdump() {
    assert!(
        log::STATIC_MAX_LEVEL <= log::LevelFilter::Debug,
        "the compile-time log cap is not in force: {:?}",
        log::STATIC_MAX_LEVEL
    );
    let fx = Fx::new();
    fx.write("mine.txt", b"uploaded through the real client");
    let (_, seen) = loopback_pass(&fx);
    // The wire really carried the bearer token, so its absence below means something.
    let bearer = format!("Bearer {ACCESS_SENTINEL}");
    assert!(seen
        .iter()
        .any(|request| request.authorization.as_deref() == Some(bearer.as_str())));
    assert!(seen.iter().any(|request| request.path == "/oauth2/token"));

    let logs = this_thread_logs();
    // Non-vacuous: ureq did log this pass, at Debug (where headers are redacted).
    assert!(
        logs.iter()
            .any(|(level, target, _)| *level == log::Level::Debug && target.starts_with("ureq")),
        "ureq logged nothing at debug level"
    );
    let traces: Vec<&(log::Level, String, String)> = logs
        .iter()
        .filter(|(level, _, _)| *level == log::Level::Trace)
        .collect();
    assert!(
        traces.is_empty(),
        "TRACE records escaped the cap: {traces:?}"
    );
    for (_, target, message) in &logs {
        let lower = message.to_ascii_lowercase();
        for secret in [REFRESH_SENTINEL, ACCESS_SENTINEL, "authorization"] {
            assert!(
                !lower.contains(&secret.to_ascii_lowercase()),
                "{target} logged {secret}: {message}"
            );
        }
    }
    fx.sweep(None);
}

#[test]
fn state_file_never_holds_login_material() {
    // A direct check on the persisted state format beside the planted login.
    let fx = Fx::new();
    fx.write("a.txt", b"a");
    let mut engine = fx.engine();
    settle(&mut engine, false);
    let state = fs::read(fx.state.path().join(STATE_FILE)).unwrap();
    assert!(!contains(&state, REFRESH_SENTINEL));
    assert!(!contains(&state, "refresh_token"));
    fx.sweep(None);
}
