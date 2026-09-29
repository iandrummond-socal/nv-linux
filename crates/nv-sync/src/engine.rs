//! Coordinate note synchronization: one pass plans every note with
//! [`crate::plan::reconcile`] and applies it with [`crate::apply::apply`];
//! [`spawn`] runs passes on a worker thread driven by local and remote events.
//!
//! Crash safety: the remote view is always "base projection overlaid with
//! `state.pending`". A fetched delta (or a full listing) is persisted together
//! with its new cursor before anything is applied, and each key's pending
//! entries are dropped only once that key's action has landed. A restart
//! therefore resumes the same view instead of losing or replaying changes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::apply::{self, read_stable, Ctx, Item, Outcome};
use crate::content_hash::content_hash;
use crate::keys::key_for_name;
use crate::plan::{reconcile, Action, LocalFile, SkipReason, MAX_SIZE};
use crate::remote::{Cursor, Notifier, Remote, RemoteChange, RemoteError, RemoteFile};
use crate::state::SyncState;
use crate::{Error, Result};
use nv_core::storage::Trasher;
use nv_core::watcher::{is_note_path, FolderWatcher};

/// Rounds per pass: a replanned or retried key gets at most this many tries.
const MAX_ROUNDS: usize = 3;
/// The engine's lock file inside the state folder; see [`try_lock`].
const LOCK_FILE: &str = "sync.lock";
const LONGPOLL: Duration = Duration::from_secs(90);
const OFFLINE_MIN: Duration = Duration::from_secs(5);
const OFFLINE_MAX: Duration = Duration::from_secs(300);
const ERROR_RETRY: Duration = Duration::from_secs(60);
/// A notifier answering "no changes" faster than this is paced to it.
const NOTIFY_MIN_WAIT: Duration = Duration::from_secs(1);
/// After signalling a change, how long the notifier waits for a new cursor.
const NOTIFY_SETTLE: Duration = Duration::from_secs(30);
const POLL_STEP: Duration = Duration::from_millis(100);

type FaultHook = Box<dyn FnMut(usize) -> bool + Send>;

pub struct EngineOptions {
    pub notes_dir: PathBuf,
    pub state_dir: PathBuf,
    pub account_id: String,
    pub config_loaded_ok: bool,
    /// Home folder for the Dropbox-client guard; `None` uses the real one.
    pub home_dir: Option<PathBuf>,
    pub debounce: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Off,
    Starting,
    Syncing,
    Idle {
        last_ok: chrono::DateTime<chrono::Utc>,
    },
    Offline,
    AuthExpired,
    Attention(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub conflicts: usize,
    pub trashed: usize,
    pub deleted_remote: usize,
    pub adopted: usize,
    pub skipped: Vec<(String, SkipReason)>,
    pub attention: Option<String>,
    pub network_writes: usize,
}

impl PassReport {
    pub fn actions(&self) -> usize {
        self.uploaded
            + self.downloaded
            + self.conflicts
            + self.trashed
            + self.deleted_remote
            + self.adopted
    }

    fn skip(&mut self, name: String, reason: SkipReason) {
        let entry = (name, reason);
        if !self.skipped.contains(&entry) {
            self.skipped.push(entry);
        }
    }

    /// The mass-change message wins over any other attention.
    fn attend(&mut self, message: String) {
        if self.attention.is_none() {
            self.attention = Some(message);
        }
    }
}

pub enum Command {
    LocalChanged,
    RemoteChanged,
    SyncNow { allow_deletes: bool },
    Flush(Sender<()>),
    Stop,
}

/// Take the sync lock for `state_dir` without waiting: `Ok(None)` when
/// another engine (or anything else holding it) has it. The lock lasts as
/// long as the returned file. Every writer of the state folder takes this.
pub fn try_lock(state_dir: &Path) -> io::Result<Option<File>> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

pub struct Engine {
    dir: PathBuf,
    state_dir: PathBuf,
    remote: Box<dyn Remote>,
    trasher: Box<dyn Trasher + Send>,
    state: SyncState,
    _lock: File,
    conflicts_this_session: usize,
    debounce: Duration,
    fault: Option<FaultHook>,
}

/// The local folder as one pass sees it.
#[derive(Default)]
struct LocalScan {
    files: BTreeMap<String, LocalFile>,
    /// Keys that must not be planned at all (never treated as deleted).
    excluded: BTreeSet<String>,
    skipped: Vec<(String, SkipReason)>,
}

fn change_key(change: &RemoteChange) -> &str {
    match change {
        RemoteChange::Upsert(file) => &file.key,
        RemoteChange::Deleted { key } => key,
    }
}

fn is_fatal(error: &Error) -> bool {
    matches!(
        error,
        Error::Remote(
            RemoteError::Offline | RemoteError::RateLimited(_) | RemoteError::Unauthorized
        )
    )
}

/// Every Dropbox-client folder named by `<home>/.dropbox/info.json`, else `<home>/Dropbox`.
fn dropbox_client_folders(home: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Ok(bytes) = fs::read(home.join(".dropbox/info.json")) {
        if let Ok(serde_json::Value::Object(accounts)) = serde_json::from_slice(&bytes) {
            for account in accounts.values() {
                if let Some(path) = account.get("path").and_then(|path| path.as_str()) {
                    folders.push(PathBuf::from(path));
                }
            }
        }
    }
    if folders.is_empty() {
        folders.push(home.join("Dropbox"));
    }
    folders
        .into_iter()
        .filter_map(|folder| fs::canonicalize(folder).ok())
        .collect()
}

impl Engine {
    pub fn new(
        opts: EngineOptions,
        remote: Box<dyn Remote>,
        trasher: Box<dyn Trasher + Send>,
    ) -> Result<Engine> {
        if !opts.config_loaded_ok {
            return Err(Error::Config(
                "config.toml is invalid — sync is paused".into(),
            ));
        }
        if !opts.notes_dir.is_dir() {
            return Err(Error::Config(
                "notes folder is missing — sync is paused".into(),
            ));
        }
        let notes = fs::canonicalize(&opts.notes_dir)?;
        if let Some(home) = opts.home_dir.clone().or_else(dirs::home_dir) {
            if dropbox_client_folders(&home)
                .iter()
                .any(|folder| notes.starts_with(folder))
            {
                return Err(Error::Config(
                    "notes folder is inside your Dropbox folder — the Dropbox client and \
                     nv-linux sync would fight; use one"
                        .into(),
                ));
            }
        }
        fs::create_dir_all(&opts.state_dir)?;
        let lock = try_lock(&opts.state_dir)?.ok_or(Error::Locked)?;
        let state = SyncState::load(&opts.state_dir, &opts.notes_dir, Some(&opts.account_id))?;
        Ok(Engine {
            dir: opts.notes_dir,
            state_dir: opts.state_dir,
            remote,
            trasher,
            state,
            _lock: lock,
            conflicts_this_session: 0,
            debounce: opts.debounce,
            fault: None,
        })
    }

    pub fn state(&self) -> &SyncState {
        &self.state
    }

    pub fn conflicts_this_session(&self) -> usize {
        self.conflicts_this_session
    }

    /// The local-change debounce this engine was configured with.
    pub fn debounce(&self) -> Duration {
        self.debounce
    }

    /// Called with the index of each action about to be applied in a pass;
    /// returning `true` aborts the pass without further state writes.
    #[doc(hidden)]
    pub fn set_fault_hook(&mut self, f: impl FnMut(usize) -> bool + Send + 'static) {
        self.fault = Some(Box::new(f));
    }

    fn save(&self) -> Result<()> {
        self.state.save(&self.state_dir)
    }

    fn scan_local(&self) -> Result<LocalScan> {
        let mut scan = LocalScan::default();
        let mut by_key: BTreeMap<String, Vec<(String, fs::Metadata)>> = BTreeMap::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let os_name = entry.file_name();
            if !is_note_path(Path::new(&os_name)) {
                continue;
            }
            let Some(name) = os_name.to_str() else {
                scan.skipped.push((
                    os_name.to_string_lossy().into_owned(),
                    SkipReason::NonUtf8Name,
                ));
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            by_key
                .entry(key_for_name(name))
                .or_default()
                .push((name.to_owned(), metadata));
        }
        for (key, mut files) in by_key {
            if files.len() > 1 {
                for (name, _) in files {
                    scan.skipped.push((name, SkipReason::CaseCollision));
                }
                scan.excluded.insert(key);
                continue;
            }
            let (name, metadata) = files.remove(0);
            let base = self.state.files.get(&key);
            let unchanged = base.is_some_and(|base| {
                base.size == metadata.len()
                    && metadata.modified().is_ok_and(|mtime| mtime == base.mtime)
            });
            let file = if metadata.len() >= MAX_SIZE {
                // Too large to sync: never read or hash it. `reconcile` checks the
                // size before anything else, so the empty hash is never compared
                // and the key is still present (never planned as deleted).
                LocalFile {
                    name,
                    hash: String::new(),
                    size: metadata.len(),
                }
            } else if let (true, Some(base)) = (unchanged, base) {
                LocalFile {
                    name,
                    hash: base.hash.clone(),
                    size: base.size,
                }
            } else {
                match read_stable(&self.dir.join(&name)) {
                    Ok(bytes) => LocalFile {
                        name,
                        hash: content_hash(&bytes),
                        size: bytes.len() as u64,
                    },
                    // Vanished since read_dir: genuinely absent.
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
                    // Unreadable or still being written: leave it alone this round,
                    // never plan it as deleted.
                    Err(error) => {
                        log::warn!("Cannot read {name} for sync: {error}");
                        scan.excluded.insert(key);
                        continue;
                    }
                }
            };
            scan.files.insert(key, file);
        }
        Ok(scan)
    }

    /// Fetch remote changes (persisting cursor and pending together) and
    /// return base projection overlaid with pending.
    fn refresh_remote(&mut self) -> Result<BTreeMap<String, RemoteFile>> {
        if let Some(cursor) = self.state.cursor.clone() {
            match self.remote.changes(&Cursor(cursor.clone())) {
                Ok((delta, next)) => {
                    if !delta.is_empty() || next.0 != cursor {
                        self.state.pending.extend(delta);
                        self.state.cursor = Some(next.0);
                        self.save()?;
                    }
                    return Ok(self.remote_view());
                }
                Err(RemoteError::ResetCursor) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let (files, cursor) = self.remote.list()?;
        let listed: BTreeSet<&str> = files.iter().map(|file| file.key.as_str()).collect();
        // Express the full listing as pending changes so it survives a crash
        // exactly as a delta does.
        let mut pending: Vec<RemoteChange> = self
            .state
            .files
            .keys()
            .filter(|key| !listed.contains(key.as_str()))
            .map(|key| RemoteChange::Deleted { key: key.clone() })
            .collect();
        pending.extend(files.into_iter().map(RemoteChange::Upsert));
        self.state.pending = pending;
        self.state.cursor = Some(cursor.0);
        self.save()?;
        Ok(self.remote_view())
    }

    fn remote_view(&self) -> BTreeMap<String, RemoteFile> {
        let mut view: BTreeMap<String, RemoteFile> = self
            .state
            .files
            .iter()
            .map(|(key, base)| {
                (
                    key.clone(),
                    RemoteFile {
                        id: base.id.clone(),
                        name: base.local_name.clone(),
                        key: key.clone(),
                        rev: base.rev.clone(),
                        content_hash: base.hash.clone(),
                        size: base.size,
                    },
                )
            })
            .collect();
        for change in &self.state.pending {
            match change {
                RemoteChange::Upsert(file) => {
                    view.insert(file.key.clone(), file.clone());
                }
                RemoteChange::Deleted { key } => {
                    view.remove(key);
                }
            }
        }
        view
    }

    /// Drop a key's pending entries; true when anything was removed.
    fn settle_pending(&mut self, key: &str) -> bool {
        let before = self.state.pending.len();
        self.state
            .pending
            .retain(|change| change_key(change) != key);
        self.state.pending.len() != before
    }

    pub fn run_pass(&mut self, allow_deletes: bool) -> Result<PassReport> {
        let mut report = PassReport::default();
        let mut guarded = false;
        let mut applied = 0_usize;
        for round in 1..=MAX_ROUNDS {
            let scan = self.scan_local()?;
            for (name, reason) in scan.skipped {
                report.skip(name, reason);
            }
            let remote = self.refresh_remote()?;
            let keys: BTreeSet<String> = scan
                .files
                .keys()
                .chain(remote.keys())
                .chain(self.state.files.keys())
                .map(String::as_str)
                .chain(self.state.pending.iter().map(change_key))
                .filter(|key| !scan.excluded.contains(*key))
                .map(str::to_owned)
                .collect();
            let plan: Vec<(Item, Action)> = keys
                .into_iter()
                .map(|key| {
                    let item = Item {
                        base: self.state.files.get(&key).cloned(),
                        local: scan.files.get(&key).cloned(),
                        remote: remote.get(&key).cloned(),
                        key,
                    };
                    let action = reconcile(
                        &item.key,
                        item.base.as_ref(),
                        item.local.as_ref(),
                        item.remote.as_ref(),
                    );
                    (item, action)
                })
                .collect();

            let destructive = plan
                .iter()
                .filter(|(_, action)| action.is_destructive())
                .count();
            let known = self.state.files.len();
            if !allow_deletes && (destructive > 10 || (destructive >= 3 && destructive * 2 > known))
            {
                guarded = true;
                report.attention = Some(format!(
                    "this would remove {destructive} notes — check the notes folder and \
                     Dropbox, then run: nv-linux sync now --allow-deletes"
                ));
            }

            let mut again: Vec<String> = Vec::new();
            let mut conflict_copy = false;
            let mut dirty = false;
            for (item, action) in plan {
                let name = display_name(&item);
                match &action {
                    Action::Nothing { .. } => {
                        dirty |= self.settle_pending(&item.key);
                        continue;
                    }
                    Action::ForgetBase { .. } if item.base.is_none() => {
                        dirty |= self.settle_pending(&item.key);
                        continue;
                    }
                    Action::Skip { reason, .. } => {
                        report.skip(name, reason.clone());
                        continue;
                    }
                    action if guarded && action.is_destructive() => continue,
                    _ => {}
                }
                if let Some(hook) = self.fault.as_mut() {
                    if hook(applied) {
                        return Err(io::Error::other("injected crash").into());
                    }
                }
                applied += 1;
                let result = apply::apply(
                    &action,
                    &item,
                    &mut Ctx {
                        dir: &self.dir,
                        remote: &mut *self.remote,
                        trasher: &*self.trasher,
                        state: &mut self.state,
                    },
                );
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(error) if is_fatal(&error) => return Err(error),
                    Err(error) => {
                        report.attend(format!("{name}: {error}"));
                        continue;
                    }
                };
                match outcome {
                    Outcome::Replan | Outcome::Retry => {
                        again.push(name);
                        continue;
                    }
                    Outcome::Skipped(reason) => {
                        report.skip(name, reason);
                        continue;
                    }
                    Outcome::Downloaded { .. } => report.downloaded += 1,
                    Outcome::ConflictCopy { .. } => {
                        report.conflicts += 1;
                        self.conflicts_this_session += 1;
                        conflict_copy = true;
                        if matches!(action, Action::Conflict { .. }) {
                            report.network_writes += 1;
                        }
                    }
                    Outcome::Done => match action {
                        Action::Upload { .. } => {
                            report.uploaded += 1;
                            report.network_writes += 1;
                        }
                        Action::DeleteRemote { .. } => {
                            report.deleted_remote += 1;
                            report.network_writes += 1;
                        }
                        Action::TrashLocal { .. } => report.trashed += 1,
                        Action::Adopt { .. } => report.adopted += 1,
                        Action::Download { .. } => report.downloaded += 1,
                        _ => {}
                    },
                }
                self.settle_pending(&item.key);
                self.save()?;
                dirty = false;
            }
            if dirty {
                self.save()?;
            }
            if again.is_empty() && !conflict_copy {
                break;
            }
            if round == MAX_ROUNDS {
                if let Some(name) = again.first() {
                    report.attend(format!("{name} keeps changing — will retry"));
                }
            }
        }
        Ok(report)
    }
}

fn display_name(item: &Item) -> String {
    item.local
        .as_ref()
        .map(|file| file.name.clone())
        .or_else(|| item.base.as_ref().map(|base| base.local_name.clone()))
        .or_else(|| item.remote.as_ref().map(|file| file.name.clone()))
        .unwrap_or_else(|| item.key.trim_start_matches('/').to_owned())
}

pub struct EngineHandle {
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }

    pub fn sync_now(&self, allow_deletes: bool) {
        self.send(Command::SyncNow { allow_deletes });
    }

    /// Run one pass now and wait for it; false on timeout or a stopped worker.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (reply, done) = mpsc::channel();
        if self.tx.send(Command::Flush(reply)).is_err() {
            return false;
        }
        done.recv_timeout(timeout).is_ok()
    }

    pub fn stop(mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.tx.send(Command::Stop);
        }
    }
}

/// Clears the shared liveness flag however the worker exits, so helper threads end.
struct Alive(Arc<AtomicBool>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Sleep up to `total`, returning early once the worker is gone.
fn pause(alive: &AtomicBool, total: Duration) {
    let until = Instant::now() + total;
    while alive.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= until {
            break;
        }
        thread::sleep(POLL_STEP.min(until - now));
    }
}

fn shared_cursor(shared: &Mutex<Option<Cursor>>) -> Option<Cursor> {
    shared.lock().ok().and_then(|cursor| cursor.clone())
}

pub fn spawn(
    engine: Engine,
    notifier: Box<dyn Notifier>,
    status: Sender<SyncStatus>,
    debounce: Duration,
) -> EngineHandle {
    let (tx, rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let cursor = Arc::new(Mutex::new(engine.state.cursor.clone().map(Cursor)));

    {
        let tx = tx.clone();
        let alive = Arc::clone(&alive);
        let cursor = Arc::clone(&cursor);
        thread::spawn(move || notifier_loop(notifier, &cursor, &tx, &alive));
    }
    {
        let tx = tx.clone();
        let alive = Arc::clone(&alive);
        let status = status.clone();
        let dir = engine.dir.clone();
        thread::spawn(move || forward_local(&dir, &tx, &status, &alive));
    }
    let thread = thread::spawn(move || {
        let alive = Alive(alive);
        worker(engine, &rx, &status, debounce, &cursor);
        drop(alive);
    });
    EngineHandle {
        tx,
        thread: Some(thread),
    }
}

fn notifier_loop(
    mut notifier: Box<dyn Notifier>,
    shared: &Mutex<Option<Cursor>>,
    tx: &Sender<Command>,
    alive: &AtomicBool,
) {
    while alive.load(Ordering::Acquire) {
        let Some(current) = shared_cursor(shared) else {
            pause(alive, NOTIFY_MIN_WAIT);
            continue;
        };
        let started = Instant::now();
        match notifier.wait(&current, LONGPOLL) {
            Ok(true) | Err(RemoteError::ResetCursor) => {
                if tx.send(Command::RemoteChanged).is_err() {
                    return;
                }
                // Give the worker a chance to take the change before asking again.
                let until = Instant::now() + NOTIFY_SETTLE;
                while alive.load(Ordering::Acquire)
                    && Instant::now() < until
                    && shared_cursor(shared).as_ref() == Some(&current)
                {
                    thread::sleep(POLL_STEP);
                }
            }
            Ok(false) => {
                let elapsed = started.elapsed();
                if elapsed < NOTIFY_MIN_WAIT {
                    pause(alive, NOTIFY_MIN_WAIT - elapsed);
                }
            }
            Err(RemoteError::Offline) => pause(alive, Duration::from_secs(10)),
            Err(RemoteError::RateLimited(delay)) => pause(alive, delay),
            Err(_) => pause(alive, Duration::from_secs(30)),
        }
    }
}

fn forward_local(
    dir: &Path,
    tx: &Sender<Command>,
    status: &Sender<SyncStatus>,
    alive: &AtomicBool,
) {
    let (events_tx, events) = mpsc::channel();
    let _watcher = match FolderWatcher::start(dir, events_tx) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = status.send(SyncStatus::Attention(format!(
                "cannot watch the notes folder ({error}) — local edits sync on the next pass"
            )));
            return;
        }
    };
    while alive.load(Ordering::Acquire) {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(_) => {
                if tx.send(Command::LocalChanged).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn worker(
    mut engine: Engine,
    rx: &Receiver<Command>,
    status: &Sender<SyncStatus>,
    debounce: Duration,
    shared: &Mutex<Option<Cursor>>,
) {
    let _ = status.send(SyncStatus::Starting);
    let mut local_due: Option<Instant> = None;
    // Set while backing off after an error; also schedules the first pass.
    let mut retry_due: Option<Instant> = Some(Instant::now());
    let mut backoff = OFFLINE_MIN;
    let mut auth_expired = false;
    loop {
        let due = [local_due, retry_due].into_iter().flatten().min();
        let command = match due {
            Some(due) => {
                let now = Instant::now();
                if due <= now {
                    None
                } else {
                    match rx.recv_timeout(due - now) {
                        Ok(command) => Some(command),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            None => match rx.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
        };
        let mut allow_deletes = false;
        let mut reply = None;
        match command {
            None => {}
            Some(Command::Stop) => return,
            Some(Command::LocalChanged) => {
                // During a backoff the scheduled retry covers local edits too.
                if !auth_expired && retry_due.is_none() {
                    local_due = Some(Instant::now() + debounce);
                }
                continue;
            }
            Some(Command::RemoteChanged) => {
                if auth_expired || retry_due.is_some() {
                    continue;
                }
            }
            Some(Command::SyncNow {
                allow_deletes: allow,
            }) => {
                allow_deletes = allow;
                auth_expired = false;
            }
            Some(Command::Flush(done)) => {
                if auth_expired {
                    let _ = done.send(());
                    continue;
                }
                reply = Some(done);
            }
        }
        local_due = None;
        retry_due = None;
        let _ = status.send(SyncStatus::Syncing);
        let result = engine.run_pass(allow_deletes);
        if let Ok(mut cursor) = shared.lock() {
            *cursor = engine.state.cursor.clone().map(Cursor);
        }
        match result {
            Ok(report) => {
                backoff = OFFLINE_MIN;
                let _ = status.send(match report.attention {
                    Some(message) => SyncStatus::Attention(message),
                    None => SyncStatus::Idle {
                        last_ok: chrono::Utc::now(),
                    },
                });
            }
            Err(Error::Remote(RemoteError::Offline)) => {
                let _ = status.send(SyncStatus::Offline);
                retry_due = Some(Instant::now() + backoff);
                backoff = (backoff * 2).min(OFFLINE_MAX);
            }
            Err(Error::Remote(RemoteError::RateLimited(delay))) => {
                let _ = status.send(SyncStatus::Attention(format!(
                    "Dropbox asked to slow down — retrying in {}s",
                    delay.as_secs().max(1)
                )));
                retry_due = Some(Instant::now() + delay);
            }
            Err(Error::Remote(RemoteError::Unauthorized)) => {
                if !auth_expired {
                    let _ = status.send(SyncStatus::AuthExpired);
                }
                auth_expired = true;
            }
            Err(error) => {
                let _ = status.send(SyncStatus::Attention(error.to_string()));
                retry_due = Some(Instant::now() + ERROR_RETRY);
            }
        }
        if let Some(done) = reply {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeRemote, SharedFake};
    use nv_core::storage::FakeTrasher;
    use tempfile::{tempdir, TempDir};

    struct Fx {
        notes: TempDir,
        state: TempDir,
        trash: TempDir,
        home: TempDir,
        /// The engine's view of Dropbox and "another device" share one fake.
        device: SharedFake,
    }

    impl Fx {
        fn new() -> Self {
            Self {
                notes: tempdir().unwrap(),
                state: tempdir().unwrap(),
                trash: tempdir().unwrap(),
                home: tempdir().unwrap(),
                device: SharedFake::new(FakeRemote::with_hasher(content_hash)),
            }
        }

        fn opts(&self) -> EngineOptions {
            EngineOptions {
                notes_dir: self.notes.path().to_owned(),
                state_dir: self.state.path().to_owned(),
                account_id: "account".into(),
                config_loaded_ok: true,
                home_dir: Some(self.home.path().to_owned()),
                debounce: Duration::from_millis(50),
            }
        }

        fn try_engine(&self, opts: EngineOptions) -> Result<Engine> {
            Engine::new(
                opts,
                Box::new(self.device.clone()),
                Box::new(FakeTrasher {
                    dir: self.trash.path().to_owned(),
                }),
            )
        }

        fn engine(&self) -> Engine {
            self.try_engine(self.opts()).unwrap()
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.notes.path().join(name), bytes).unwrap();
        }

        fn remove(&self, name: &str) {
            fs::remove_file(self.notes.path().join(name)).unwrap();
        }

        fn local(&self) -> BTreeMap<String, Vec<u8>> {
            dir_contents(self.notes.path())
        }

        fn trashed(&self) -> BTreeMap<String, Vec<u8>> {
            dir_contents(self.trash.path())
        }

        fn remote(&self) -> BTreeMap<String, Vec<u8>> {
            self.device.with(|fake| {
                fake.snapshot()
                    .into_values()
                    .collect::<BTreeMap<String, Vec<u8>>>()
            })
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
            assert_eq!(self.local(), self.remote());
        }
    }

    fn dir_contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
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

    /// Run passes until one does nothing; return the reports.
    fn settle(engine: &mut Engine) -> Vec<PassReport> {
        let mut reports = Vec::new();
        for _ in 0..6 {
            let report = engine.run_pass(false).unwrap();
            let quiet = report.actions() == 0;
            reports.push(report);
            if quiet {
                return reports;
            }
        }
        panic!("no quiescence: {reports:?}");
    }

    fn conflict_copies(files: &BTreeMap<String, Vec<u8>>) -> Vec<(&String, &Vec<u8>)> {
        files
            .iter()
            .filter(|(name, _)| name.contains("(conflict "))
            .collect()
    }

    // ---- guards ----

    #[test]
    fn engine_guard_invalid_config() {
        let f = Fx::new();
        let mut opts = f.opts();
        opts.config_loaded_ok = false;
        assert!(matches!(f.try_engine(opts), Err(Error::Config(m)) if m.contains("config.toml")));
    }

    #[test]
    fn engine_guard_missing_notes_dir() {
        let f = Fx::new();
        let mut opts = f.opts();
        opts.notes_dir = f.notes.path().join("missing");
        assert!(matches!(f.try_engine(opts), Err(Error::Config(m)) if m.contains("missing")));
        let mut opts = f.opts();
        opts.notes_dir = f.notes.path().join("file.txt");
        fs::write(&opts.notes_dir, b"x").unwrap();
        assert!(matches!(f.try_engine(opts), Err(Error::Config(_))));
    }

    #[test]
    fn engine_guard_inside_default_dropbox_folder() {
        let f = Fx::new();
        let notes = f.home.path().join("Dropbox/notes");
        fs::create_dir_all(&notes).unwrap();
        let mut opts = f.opts();
        opts.notes_dir = notes;
        assert!(
            matches!(f.try_engine(opts), Err(Error::Config(m)) if m.contains("inside your Dropbox"))
        );
    }

    #[test]
    fn engine_guard_inside_dropbox_info_json_folder() {
        let f = Fx::new();
        let client = tempdir().unwrap();
        let notes = client.path().join("Work/notes");
        fs::create_dir_all(&notes).unwrap();
        fs::create_dir_all(f.home.path().join(".dropbox")).unwrap();
        let info = serde_json::json!({
            "business": { "path": client.path().join("Work"), "host": 1 },
        });
        fs::write(
            f.home.path().join(".dropbox/info.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        let mut opts = f.opts();
        opts.notes_dir = notes;
        assert!(
            matches!(f.try_engine(opts), Err(Error::Config(m)) if m.contains("inside your Dropbox"))
        );
        // A sibling of the client folder is fine.
        let mut opts = f.opts();
        opts.notes_dir = client.path().to_owned();
        assert!(f.try_engine(opts).is_ok());
    }

    #[test]
    fn engine_guard_lock_is_exclusive_until_dropped() {
        let f = Fx::new();
        let first = f.engine();
        assert!(matches!(f.try_engine(f.opts()), Err(Error::Locked)));
        drop(first);
        assert!(f.try_engine(f.opts()).is_ok());
    }

    // ---- cold start twins ----

    #[test]
    fn engine_cold_local_only_uploads() {
        let f = Fx::new();
        f.write("Local.txt", b"local text");
        let mut engine = f.engine();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.uploaded, 1);
        assert_eq!(report.network_writes, 1);
        f.assert_converged();
        let again = engine.run_pass(false).unwrap();
        assert_eq!((again.actions(), again.network_writes), (0, 0));
    }

    #[test]
    fn engine_cold_remote_only_downloads() {
        let f = Fx::new();
        f.edit_remotely("Remote.txt", b"remote text");
        let mut engine = f.engine();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.downloaded, 1);
        assert_eq!(report.network_writes, 0);
        assert_eq!(f.local()["Remote.txt"], b"remote text");
        f.assert_converged();
        assert_eq!(engine.run_pass(false).unwrap().actions(), 0);
    }

    #[test]
    fn engine_cold_both_equal_adopts_without_writes() {
        let f = Fx::new();
        f.write("Same.txt", b"same");
        f.edit_remotely("Same.txt", b"same");
        let mut engine = f.engine();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.adopted, 1);
        assert_eq!(report.network_writes, 0);
        assert_eq!(f.writes(), 0);
        let again = engine.run_pass(false).unwrap();
        assert_eq!((again.actions(), again.network_writes), (0, 0));
        assert_eq!(f.writes(), 0);
    }

    #[test]
    fn engine_cold_both_different_conflict_copy_both_sides_in_one_pass() {
        let f = Fx::new();
        f.write("Note.txt", b"local text");
        f.edit_remotely("Note.txt", b"remote text");
        let mut engine = f.engine();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.conflicts, 1);
        assert_eq!(engine.conflicts_this_session(), 1);
        let local = f.local();
        assert_eq!(local["Note.txt"], b"local text");
        let copies = conflict_copies(&local);
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].1, b"remote text");
        f.assert_converged();
        let writes = f.writes();
        let again = engine.run_pass(false).unwrap();
        assert_eq!((again.actions(), again.network_writes), (0, 0));
        assert_eq!(f.writes(), writes);
    }

    // ---- idempotence and steady state ----

    fn synced(names: &[&str]) -> (Fx, Engine) {
        let f = Fx::new();
        for name in names {
            f.write(name, format!("text of {name}").as_bytes());
        }
        let mut engine = f.engine();
        settle(&mut engine);
        f.assert_converged();
        (f, engine)
    }

    #[test]
    fn engine_idempotent_after_converged_pass() {
        let (f, mut engine) = synced(&["a.txt", "b.md", "c.txt"]);
        f.edit_remotely("d.txt", b"from elsewhere");
        engine.run_pass(false).unwrap();
        let writes = f.writes();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.actions(), 0);
        assert_eq!(report.network_writes, 0);
        assert_eq!(f.writes(), writes);
    }

    #[test]
    fn engine_steady_remote_edit_downloads() {
        let (f, mut engine) = synced(&["a.txt"]);
        f.edit_remotely("a.txt", b"edited on another device");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.downloaded, 1);
        assert_eq!(f.local()["a.txt"], b"edited on another device");
        f.assert_converged();
    }

    #[test]
    fn engine_steady_local_edit_uploads() {
        let (f, mut engine) = synced(&["a.txt"]);
        f.write("a.txt", b"edited here, longer");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.uploaded, 1);
        assert_eq!(report.conflicts, 0);
        f.assert_converged();
    }

    #[test]
    fn engine_steady_remote_delete_trashes_locally() {
        let (f, mut engine) = synced(&["a.txt", "b.txt"]);
        f.delete_remotely("a.txt");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.trashed, 1);
        assert_eq!(f.trashed()["a.txt"], b"text of a.txt");
        f.assert_converged();
    }

    #[test]
    fn engine_steady_local_delete_removes_remote() {
        let (f, mut engine) = synced(&["a.txt", "b.txt"]);
        f.remove("a.txt");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.deleted_remote, 1);
        assert!(!f.remote().contains_key("a.txt"));
        f.assert_converged();
    }

    #[test]
    fn engine_own_delete_echo_is_settled() {
        let (f, mut engine) = synced(&["a.txt", "b.txt"]);
        f.remove("a.txt");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.deleted_remote, 1);

        let report = engine.run_pass(false).unwrap();
        assert!(engine.state().pending.is_empty());
        assert_eq!(report.uploaded, 0);
        assert_eq!(report.downloaded, 0);
        assert_eq!(report.deleted_remote, 0);
        assert_eq!(report.trashed, 0);
        assert_eq!(report.actions(), 0);
        assert_eq!(report.network_writes, 0);
        assert!(report.attention.is_none());
        f.assert_converged();
    }

    #[test]
    fn engine_steady_touched_remote_adopts() {
        let (f, mut engine) = synced(&["a.txt"]);
        f.device.with(|fake| fake.touch_remotely("a.txt"));
        let writes = f.writes();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.adopted, 1);
        assert_eq!((report.uploaded, report.conflicts), (0, 0));
        assert_eq!(f.writes(), writes);
        assert_eq!(engine.run_pass(false).unwrap().actions(), 0);
    }

    // ---- mass-change guard ----

    fn twelve() -> Vec<String> {
        (0..12).map(|i| format!("note {i}.txt")).collect()
    }

    #[test]
    fn engine_mass_guard_local_wipe() {
        let names = twelve();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (f, mut engine) = synced(&refs);
        for name in &names {
            f.remove(name);
        }
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.deleted_remote, 0);
        assert!(report.attention.unwrap().contains("--allow-deletes"));
        assert_eq!(f.remote().len(), 12);
        let report = engine.run_pass(true).unwrap();
        assert_eq!(report.deleted_remote, 12);
        assert!(f.remote().is_empty());
    }

    #[test]
    fn engine_mass_guard_remote_wipe() {
        let names = twelve();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (f, mut engine) = synced(&refs);
        for name in &names {
            f.delete_remotely(name);
        }
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.trashed, 0);
        assert!(report.attention.unwrap().contains("--allow-deletes"));
        assert_eq!(f.local().len(), 12);
        assert!(f.trashed().is_empty());
        // The guarded deletes stay pending: still guarded, and the flag applies them.
        assert_eq!(engine.run_pass(false).unwrap().trashed, 0);
        assert_eq!(engine.run_pass(true).unwrap().trashed, 12);
        assert!(f.local().is_empty());
        assert_eq!(f.trashed().len(), 12);
    }

    #[test]
    fn engine_mass_guard_small_delete_applies() {
        let (f, mut engine) = synced(&["a.txt", "b.txt"]);
        f.remove("a.txt");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.deleted_remote, 1);
        assert_eq!(report.attention, None);
        f.assert_converged();
    }

    // ---- crash safety ----

    #[test]
    fn engine_crash_at_every_action_converges_without_loss() {
        let mut crashed = 0;
        for k in 0..9 {
            let (f, mut engine) = synced(&["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
            f.edit_remotely("a.txt", b"a remote edit");
            f.write("a.txt", b"a local edit, longer");
            f.edit_remotely("b.txt", b"b remote edit");
            f.delete_remotely("c.txt");
            f.write("d.txt", b"d local edit, longer");
            f.remove("e.txt");
            f.edit_remotely("f.txt", b"f new remote");
            f.edit_remotely("g.txt", b"g new remote");
            let fired = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&fired);
            engine.set_fault_hook(move |index| {
                let hit = index == k;
                flag.fetch_or(hit, Ordering::SeqCst);
                hit
            });
            let result = engine.run_pass(false);
            assert_eq!(result.is_err(), fired.load(Ordering::SeqCst), "k={k}");
            if fired.load(Ordering::SeqCst) {
                crashed += 1;
            }
            drop(engine);
            let mut engine = f.engine();
            settle(&mut engine);
            f.assert_converged();
            let local = f.local();
            let trashed = f.trashed();
            let remote = f.remote();
            for text in [
                &b"a remote edit"[..],
                b"a local edit, longer",
                b"b remote edit",
                b"text of c.txt",
                b"d local edit, longer",
                b"f new remote",
                b"g new remote",
            ] {
                assert!(
                    local
                        .values()
                        .chain(remote.values())
                        .chain(trashed.values())
                        .any(|b| b == text),
                    "k={k}: lost {}",
                    String::from_utf8_lossy(text)
                );
            }
            assert!(!local.contains_key("e.txt"), "k={k}");
            assert!(!local.contains_key("c.txt"), "k={k}");
            assert_eq!(conflict_copies(&local).len(), 1, "k={k}");
        }
        // Seven first-round actions plus the conflict copy's upload.
        assert_eq!(crashed, 8);
    }

    #[test]
    fn engine_pending_delta_survives_restart() {
        let (f, mut engine) = synced(&["a.txt"]);
        f.edit_remotely("a.txt", b"remote edit");
        engine.set_fault_hook(|_| true);
        assert!(engine.run_pass(false).is_err());
        assert!(!engine.state().pending.is_empty());
        drop(engine);
        // The cursor has already moved past the edit; only pending remembers it.
        let mut engine = f.engine();
        assert_eq!(engine.run_pass(false).unwrap().downloaded, 1);
        assert!(engine.state().pending.is_empty());
        f.assert_converged();
    }

    // ---- cursor, collisions ----

    #[test]
    fn engine_expired_cursor_falls_back_to_list() {
        let (f, mut engine) = synced(&["a.txt", "b.txt"]);
        f.device.with(|fake| fake.expire_cursors());
        f.edit_remotely("a.txt", b"edited while expired");
        f.edit_remotely("new.txt", b"new while expired");
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.downloaded, 2);
        f.assert_converged();
        assert_eq!(engine.run_pass(false).unwrap().actions(), 0);
    }

    #[test]
    fn engine_case_collision_is_skipped_not_deleted() {
        let (f, mut engine) = synced(&["todo.txt"]);
        f.write("Todo.txt", b"a second todo, different case");
        let before = f.remote();
        let writes = f.writes();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.actions(), 0);
        let mut skipped = report.skipped.clone();
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            skipped,
            vec![
                ("Todo.txt".into(), SkipReason::CaseCollision),
                ("todo.txt".into(), SkipReason::CaseCollision),
            ]
        );
        assert_eq!(f.remote(), before);
        assert_eq!(f.writes(), writes);
        assert_eq!(f.local().len(), 2);
        assert!(f.trashed().is_empty());

        // Cold: neither is uploaded.
        let g = Fx::new();
        g.write("Todo.txt", b"one");
        g.write("todo.txt", b"two");
        let report = g.engine().run_pass(false).unwrap();
        assert_eq!(report.skipped.len(), 2);
        assert!(g.remote().is_empty());
        assert_eq!(g.writes(), 0);
    }

    /// An oversized local file is skipped from its metadata alone: made
    /// unreadable, it is still reported as too large (a read would exclude it
    /// silently instead), its base is untouched and it is never deleted.
    #[cfg(unix)]
    #[test]
    fn engine_oversized_local_file_is_skipped_without_reading() {
        use std::os::unix::fs::PermissionsExt;

        let make_huge_unreadable = |path: &Path| {
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .unwrap()
                .set_len(MAX_SIZE)
                .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        };

        // Steady: a synced note grows past the limit.
        let (f, mut engine) = synced(&["big.txt"]);
        let key = key_for_name("big.txt");
        let base = engine.state().files.get(&key).cloned().unwrap();
        let path = f.notes.path().join("big.txt");
        make_huge_unreadable(&path);
        if fs::File::open(&path).is_ok() {
            // Running as root: permissions cannot prove the file is unread.
            return;
        }
        let remote = f.remote();
        let writes = f.writes();
        for _ in 0..2 {
            let report = engine.run_pass(false).unwrap();
            assert_eq!((report.actions(), report.network_writes), (0, 0));
            assert_eq!(report.attention, None);
            assert_eq!(
                report.skipped,
                vec![("big.txt".to_owned(), SkipReason::TooLarge)]
            );
        }
        assert_eq!(engine.state().files.get(&key), Some(&base));
        assert_eq!(f.remote(), remote);
        assert_eq!(f.writes(), writes);
        assert!(f.trashed().is_empty());

        // Cold: never uploaded, never recorded.
        let g = Fx::new();
        make_huge_unreadable(&g.notes.path().join("huge.md"));
        let mut engine = g.engine();
        let report = engine.run_pass(false).unwrap();
        assert_eq!(report.actions(), 0);
        assert_eq!(
            report.skipped,
            vec![("huge.md".to_owned(), SkipReason::TooLarge)]
        );
        assert!(engine.state().files.is_empty());
        assert!(g.remote().is_empty());
        assert_eq!(g.writes(), 0);
    }

    // ---- spawn ----

    /// Receive statuses until `done` holds after an `Idle`, or panic after a generous timeout.
    fn wait_for(
        statuses: &Receiver<SyncStatus>,
        seen: &mut Vec<SyncStatus>,
        mut done: impl FnMut() -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let status = statuses
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("timed out; statuses so far: {seen:?}"));
            let idle = matches!(status, SyncStatus::Idle { .. });
            seen.push(status);
            if idle && done() {
                return;
            }
        }
    }

    #[test]
    fn engine_spawn_syncs_remote_and_local_changes() {
        let f = Fx::new();
        f.write("a.txt", b"initial");
        let engine = f.engine();
        let (status_tx, statuses) = mpsc::channel();
        let handle = spawn(
            engine,
            Box::new(f.device.clone()),
            status_tx,
            Duration::from_millis(50),
        );
        let mut seen = Vec::new();
        wait_for(&statuses, &mut seen, || f.remote().contains_key("a.txt"));

        f.edit_remotely("a.txt", b"edited remotely");
        handle.send(Command::RemoteChanged);
        wait_for(&statuses, &mut seen, || {
            f.local().get("a.txt").map(Vec::as_slice) == Some(&b"edited remotely"[..])
        });

        // The engine's own folder watcher drives an upload.
        f.write("b.txt", b"written locally");
        wait_for(&statuses, &mut seen, || {
            f.remote().get("b.txt").map(Vec::as_slice) == Some(&b"written locally"[..])
        });

        assert!(handle.flush(Duration::from_secs(20)));
        handle.stop();
        f.assert_converged();
        assert_eq!(seen.first(), Some(&SyncStatus::Starting));
        assert!(seen.contains(&SyncStatus::Syncing));
        assert!(seen.iter().any(|s| matches!(s, SyncStatus::Idle { .. })));
    }

    #[test]
    fn engine_spawn_auth_expired_reported_once() {
        let f = Fx::new();
        let engine = f.engine();
        f.device
            .with(|fake| fake.fail_next(RemoteError::Unauthorized));
        let (status_tx, statuses) = mpsc::channel();
        let handle = spawn(
            engine,
            Box::new(f.device.clone()),
            status_tx,
            Duration::from_millis(50),
        );
        // A flush while expired returns without a pass.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while !seen.contains(&SyncStatus::AuthExpired) {
            let left = deadline.saturating_duration_since(Instant::now());
            seen.push(statuses.recv_timeout(left).expect("no AuthExpired"));
        }
        handle.send(Command::RemoteChanged);
        assert!(handle.flush(Duration::from_secs(20)));
        // SyncNow resumes.
        handle.sync_now(false);
        wait_for(&statuses, &mut seen, || true);
        handle.stop();
        let expired = seen
            .iter()
            .filter(|s| **s == SyncStatus::AuthExpired)
            .count();
        assert_eq!(expired, 1, "{seen:?}");
    }
}
