//! Dropbox sync inside the app: the bottom-right status indicator and the
//! in-app connect flow (Ctrl+Shift+D).
//!
//! Everything that decides *what* to show or *what* to do is pure and tested
//! here ([`status_text`], [`status_tone`], [`ConnectFlow`]); the GTK parts
//! ([`SyncIndicator`], [`StatusHub`]) are thin and live on the GTK main thread.
//!
//! Threading: the engine reports on a `std::sync::mpsc` channel from its worker
//! thread. [`status_channel`] bridges that onto an `async_channel`, and
//! [`StatusHub::run`] drains it with `glib::spawn_future_local`, so labels are
//! only ever touched on the GTK main context — never from the worker.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gtk::prelude::*;
use gtk::{gio, glib};
use nv_core::config::Config;
use nv_core::storage::{SystemTrasher, Trasher};
use nv_sync::dropbox::oauth::Pkce;
use nv_sync::dropbox::DropboxNotifier;
use nv_sync::engine::{Engine, EngineHandle, SyncStatus};
use nv_sync::remote::{Notifier, RemoteError};
use nv_sync::secret::Secret;
use nv_sync::state::state_dir_with;
use nv_sync::tokens::{default_store, StoredLogin, TokenStore};

use crate::sync_cli::{engine_options, error_message, DropboxApi, RealDropbox};

/// Longest attention message shown in the indicator before it is cut with `…`.
const ATTENTION_CHARS: usize = 40;
/// How often "synced Nm ago" is re-rendered.
const REFRESH_SECS: u32 = 30;

/// The indicator text, or `None` when it must be hidden entirely
/// (sync not connected / turned off).
pub fn status_text(status: &SyncStatus, conflicts: usize, now: DateTime<Utc>) -> Option<String> {
    let base = match status {
        SyncStatus::Off => return None,
        SyncStatus::Starting | SyncStatus::Syncing => "↻ syncing".to_owned(),
        SyncStatus::Idle { last_ok } => format!("✓ synced {}", ago(*last_ok, now)),
        SyncStatus::Background => "syncing in background".to_owned(),
        SyncStatus::Offline => "offline".to_owned(),
        SyncStatus::AuthExpired => "reconnect Dropbox · Ctrl+Shift+D".to_owned(),
        SyncStatus::Attention(message) => format!("⚠ {}", shorten(message)),
    };
    Some(match (conflicts, status) {
        (0, _) => base,
        (n, SyncStatus::Attention(_)) => format!("{base} · {}", conflict_count(n)),
        (n, _) => format!("{base} · ⚠ {}", conflict_count(n)),
    })
}

/// How the indicator is coloured; each maps to a CSS class in `theming.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Synced or offline: quiet, muted text.
    Quiet,
    /// A pass is running.
    Busy,
    /// Needs the member: reconnect, attention, conflicts.
    Warn,
}

impl Tone {
    pub fn css_class(self) -> &'static str {
        match self {
            Tone::Quiet => "nv-sync-quiet",
            Tone::Busy => "nv-sync-busy",
            Tone::Warn => "nv-sync-warn",
        }
    }
}

pub fn status_tone(status: &SyncStatus, conflicts: usize) -> Tone {
    match status {
        SyncStatus::AuthExpired | SyncStatus::Attention(_) => Tone::Warn,
        _ if conflicts > 0 => Tone::Warn,
        SyncStatus::Starting | SyncStatus::Syncing => Tone::Busy,
        _ => Tone::Quiet,
    }
}

fn conflict_count(n: usize) -> String {
    if n == 1 {
        "1 conflict".to_owned()
    } else {
        format!("{n} conflicts")
    }
}

/// `just now` · `2m ago` · `3h ago` · `4d ago` (floored; a clock in the future reads `just now`).
fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    match secs {
        s if s < 60 => "just now".to_owned(),
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// First clause of an attention message, capped for a small label.
fn shorten(message: &str) -> String {
    let first = message
        .lines()
        .next()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('.');
    let first = if first.is_empty() {
        "sync needs attention"
    } else {
        first
    };
    if first.chars().count() <= ATTENTION_CHARS {
        first.to_owned()
    } else {
        let cut: String = first.chars().take(ATTENTION_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

/// True for a note title the library made as a conflict copy (`name (conflict …)`).
/// The window reports these as they appear to [`StatusHub::note_conflict_copy`].
pub fn is_conflict_copy(title: &str) -> bool {
    title.contains(" (conflict ")
}

// ---------------------------------------------------------------------------
// Threading bridge
// ---------------------------------------------------------------------------

/// A sender to hand to `nv_sync::engine::spawn`, and the receiver to give
/// [`StatusHub::run`]. A small bridge thread forwards between them and ends
/// when the engine drops its sender.
pub fn status_channel() -> (
    mpsc::Sender<SyncStatus>,
    async_channel::Receiver<SyncStatus>,
) {
    let (tx, rx) = mpsc::channel::<SyncStatus>();
    let (async_tx, async_rx) = async_channel::unbounded();
    let spawned = std::thread::Builder::new()
        .name("nv-sync-status".into())
        .spawn(move || {
            while let Ok(status) = rx.recv() {
                if async_tx.send_blocking(status).is_err() {
                    break;
                }
            }
        });
    if let Err(error) = spawned {
        glib::g_warning!("nv-linux", "Sync status bridge didn't start: {error}");
    }
    (tx, async_rx)
}

// ---------------------------------------------------------------------------
// Widget
// ---------------------------------------------------------------------------

/// A small label pinned bottom-right of the window's `gtk::Overlay`.
/// Hidden until a status arrives that [`status_text`] shows.
#[derive(Clone)]
pub struct SyncIndicator {
    label: gtk::Label,
}

impl Default for SyncIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncIndicator {
    pub fn new() -> Self {
        let label = gtk::Label::new(None);
        label.set_css_classes(&["nv-sync", Tone::Quiet.css_class()]);
        label.set_halign(gtk::Align::End);
        label.set_valign(gtk::Align::End);
        label.set_margin_end(10);
        label.set_margin_bottom(8);
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(48);
        // Mouseless and never in the way: no focus, no pointer target.
        label.set_can_focus(false);
        label.set_can_target(false);
        label.set_visible(false);
        Self { label }
    }

    /// Add to the window's overlay (next to the toast revealer).
    pub fn attach(&self, overlay: &gtk::Overlay) {
        overlay.add_overlay(&self.label);
    }

    /// Render one status. Call on the GTK main thread only.
    pub fn set_status(&self, status: &SyncStatus, conflicts: usize) {
        match status_text(status, conflicts, Utc::now()) {
            None => self.label.set_visible(false),
            Some(text) => {
                self.label.set_text(&text);
                let tone = status_tone(status, conflicts);
                self.label.set_css_classes(&["nv-sync", tone.css_class()]);
                self.label.set_visible(true);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fan-out to every window
// ---------------------------------------------------------------------------

/// One per app: remembers the last status and the session's conflict copies,
/// pushes them to every attached window's indicator, and re-renders every
/// 30 s so "synced Nm ago" stays true. Main thread only (`Rc`).
pub struct StatusHub {
    status: RefCell<SyncStatus>,
    /// Titles, not a counter: every window's watcher sees the same new copy.
    conflict_copies: RefCell<HashSet<String>>,
    indicators: RefCell<Vec<Weak<SyncIndicator>>>,
}

impl StatusHub {
    pub fn new() -> Rc<Self> {
        let hub = Rc::new(Self {
            status: RefCell::new(SyncStatus::Off),
            conflict_copies: RefCell::new(HashSet::new()),
            indicators: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&hub);
        glib::timeout_add_seconds_local(REFRESH_SECS, move || match weak.upgrade() {
            Some(hub) => {
                if matches!(*hub.status.borrow(), SyncStatus::Idle { .. }) {
                    hub.render();
                }
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        hub
    }

    /// Register a window's indicator; it shows the current status at once.
    /// The hub holds it weakly, so a closed window simply drops out.
    pub fn attach(&self, indicator: &Rc<SyncIndicator>) {
        indicator.set_status(&self.status(), self.conflicts());
        self.indicators.borrow_mut().push(Rc::downgrade(indicator));
    }

    /// Drain engine statuses on the GTK main context.
    pub fn run(self: &Rc<Self>, receiver: async_channel::Receiver<SyncStatus>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(status) = receiver.recv().await {
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                hub.set_status(status);
            }
        });
    }

    pub fn set_status(&self, status: SyncStatus) {
        self.status.replace(status);
        self.render();
    }

    /// A conflict copy appeared in the notes folder. Counted once per title,
    /// and only while sync is on (a member without sync never sees a count).
    pub fn note_conflict_copy(&self, title: &str) {
        if self.status() == SyncStatus::Off {
            return;
        }
        let added = self.conflict_copies.borrow_mut().insert(title.to_owned());
        if added {
            self.render();
        }
    }

    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }

    fn conflicts(&self) -> usize {
        self.conflict_copies.borrow().len()
    }

    fn render(&self) {
        // Clone out first: widget calls must not run under our own borrows.
        let status = self.status();
        let conflicts = self.conflicts();
        let live: Vec<Rc<SyncIndicator>> = {
            let mut indicators = self.indicators.borrow_mut();
            indicators.retain(|weak| weak.strong_count() > 0);
            indicators.iter().filter_map(Weak::upgrade).collect()
        };
        for indicator in live {
            indicator.set_status(&status, conflicts);
        }
    }
}

// ---------------------------------------------------------------------------
// Starting the engine (once per app)
// ---------------------------------------------------------------------------

/// How long quitting waits for a last pass before it lets go.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Why sync stays off. None of these shows an indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffReason {
    InvalidConfig,
    Disabled,
    NoAppKey,
    NotConnected,
}

/// Cheap and offline, and it never touches the keyring: may this app sync at
/// all? A member who never set Dropbox up stops here.
pub fn sync_gate(config: &Config) -> Result<(), OffReason> {
    if !config.loaded_ok {
        Err(OffReason::InvalidConfig)
    } else if !config.sync.enabled {
        Err(OffReason::Disabled)
    } else if config.sync.dropbox_app_key.trim().is_empty() {
        Err(OffReason::NoAppKey)
    } else {
        Ok(())
    }
}

/// What one start attempt came to.
pub enum StartOutcome {
    Started(EngineHandle),
    Off(OffReason),
    /// Another syncer (the background service or another app instance) holds the lock: the indicator says so and the app retries.
    Locked,
    /// Connected, but the engine refused to start: shown in the indicator.
    Failed(String),
}

/// Everything one start needs: the real things in production
/// ([`StartDeps::production`]), fakes in tests.
pub struct StartDeps {
    pub config: Config,
    pub state_dir: PathBuf,
    pub store: Box<dyn TokenStore>,
    pub dropbox: Box<dyn DropboxApi + Send>,
    pub notifier: Box<dyn Notifier>,
    pub trasher: Box<dyn Trasher + Send>,
    /// Home folder for the engine's Dropbox-client guard; `None` = the real one.
    pub home_dir: Option<PathBuf>,
}

impl StartDeps {
    /// The real Dropbox, sign-in store and Trash. Building the store probes
    /// the keyring, which can block: call it off the GTK main thread.
    pub fn production(config: Config, state_dir: PathBuf) -> Self {
        let store = default_store(&state_dir);
        Self {
            config,
            state_dir,
            store: Box::new(store),
            dropbox: Box::new(RealDropbox::new()),
            notifier: Box::new(DropboxNotifier::new()),
            trasher: Box::new(SystemTrasher),
            home_dir: None,
        }
    }
}

/// Read the sign-in and start the engine thread (its first pass runs at once).
/// Blocking (keyring, state file): run it off the GTK main thread.
pub fn start_engine(deps: StartDeps, status: mpsc::Sender<SyncStatus>) -> StartOutcome {
    if let Err(reason) = sync_gate(&deps.config) {
        return StartOutcome::Off(reason);
    }
    let login = match deps.store.load() {
        Ok(Some(login)) => login,
        Ok(None) => return StartOutcome::Off(OffReason::NotConnected),
        Err(error) => {
            return StartOutcome::Failed(format!("couldn't read the Dropbox sign-in: {error}"))
        }
    };
    let options = engine_options(&deps.config, &deps.state_dir, &login, deps.home_dir);
    let remote = deps.dropbox.remote(
        deps.config.sync.dropbox_app_key.trim(),
        &login.refresh_token,
    );
    match Engine::new(options, remote, deps.trasher) {
        Ok(engine) => {
            let debounce = engine.debounce();
            StartOutcome::Started(nv_sync::engine::spawn(
                engine,
                deps.notifier,
                status,
                debounce,
            ))
        }
        Err(nv_sync::Error::Locked) => StartOutcome::Locked,
        Err(error) => StartOutcome::Failed(error_message(&error)),
    }
}

/// Where the app's engine stands, for the Ctrl+Shift+Y answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    NotStarted,
    Starting,
    Running,
    Off(OffReason),
    Locked,
    Failed(String),
}

/// What Ctrl+Shift+Y says when there is no engine to ask.
pub fn sync_now_refusal(state: &EngineState) -> String {
    match state {
        EngineState::NotStarted
        | EngineState::Off(OffReason::NotConnected | OffReason::NoAppKey) => {
            "Not connected to Dropbox — Ctrl+Shift+D connects".into()
        }
        EngineState::Off(OffReason::Disabled) => {
            "Sync is off in config.toml ([sync] enabled = false)".into()
        }
        EngineState::Off(OffReason::InvalidConfig) => {
            "config.toml has a mistake, so sync is paused".into()
        }
        EngineState::Starting | EngineState::Running => "Sync is starting".into(),
        EngineState::Locked => "The background service is syncing these notes".into(),
        EngineState::Failed(message) => format!("Sync couldn't start: {message}"),
    }
}

/// One per app, main thread only: owns the engine handle and the status hub.
/// A member who can't sync gets no thread, no indicator and no network.
pub struct SyncService {
    hub: Rc<StatusHub>,
    config: RefCell<Config>,
    state_dir: Option<PathBuf>,
    handle: RefCell<Option<EngineHandle>>,
    state: RefCell<EngineState>,
    restart_pending: Cell<bool>,
    retry_scheduled: Cell<bool>,
    shutting_down: Cell<bool>,
}

impl SyncService {
    pub fn new(config: &Config) -> Rc<Self> {
        Rc::new(Self {
            hub: StatusHub::new(),
            config: RefCell::new(config.clone()),
            state_dir: state_dir_with(&|key| std::env::var(key).ok()),
            handle: RefCell::new(None),
            state: RefCell::new(EngineState::NotStarted),
            restart_pending: Cell::new(false),
            retry_scheduled: Cell::new(false),
            shutting_down: Cell::new(false),
        })
    }

    pub fn hub(&self) -> &Rc<StatusHub> {
        &self.hub
    }

    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    /// Take a freshly loaded config's `[sync]` section (e.g. an app key added
    /// while the app runs). The notes folder stays the one the windows show.
    pub fn refresh_sync_config(&self, fresh: &Config) {
        if fresh.loaded_ok {
            self.config.borrow_mut().sync = fresh.sync.clone();
        }
    }

    pub fn state_dir(&self) -> Option<&Path> {
        self.state_dir.as_deref()
    }

    /// Start the engine, or restart it (a reconnect): the old engine is
    /// stopped before the new one takes the lock. The keyring read and the
    /// engine start run on a worker thread; the result lands on the main context.
    pub fn start(self: &Rc<Self>) {
        if self.shutting_down.get() {
            return;
        }
        if *self.state.borrow() == EngineState::Starting {
            self.restart_pending.set(true);
            return;
        }
        let previous = self.handle.borrow_mut().take();
        let config = self.config();
        if let Err(reason) = sync_gate(&config) {
            drop(previous);
            self.finish_start(StartOutcome::Off(reason));
            return;
        }
        let Some(state_dir) = self.state_dir.clone() else {
            drop(previous);
            self.finish_start(StartOutcome::Failed(
                "can't find a state folder (set NV_STATE_DIR)".into(),
            ));
            return;
        };
        self.state.replace(EngineState::Starting);
        let (status_tx, status_rx) = status_channel();
        self.hub.run(status_rx);
        let (result_tx, result_rx) = async_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("nv-sync-start".into())
            .spawn(move || {
                if let Some(previous) = previous {
                    previous.stop();
                }
                let outcome = start_engine(StartDeps::production(config, state_dir), status_tx);
                let _ = result_tx.send_blocking(outcome);
            });
        if let Err(error) = spawned {
            self.finish_start(StartOutcome::Failed(format!("sync didn't start: {error}")));
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = result_rx
                .recv()
                .await
                .unwrap_or_else(|_| StartOutcome::Failed("sync didn't start".into()));
            if let Some(this) = weak.upgrade() {
                this.finish_start(outcome);
            }
        });
    }

    fn finish_start(self: &Rc<Self>, outcome: StartOutcome) {
        if self.shutting_down.get() {
            return;
        }
        let (state, status) = match outcome {
            StartOutcome::Started(handle) => {
                self.handle.replace(Some(handle));
                // The engine reports its own statuses from here on.
                (EngineState::Running, None)
            }
            StartOutcome::Off(reason) => (EngineState::Off(reason), Some(SyncStatus::Off)),
            StartOutcome::Locked => (EngineState::Locked, Some(SyncStatus::Background)),
            StartOutcome::Failed(message) => (
                EngineState::Failed(message.clone()),
                Some(SyncStatus::Attention(message)),
            ),
        };
        self.state.replace(state);
        if let Some(status) = status {
            self.hub.set_status(status);
        }
        if *self.state.borrow() == EngineState::Locked && !self.retry_scheduled.replace(true) {
            let weak = Rc::downgrade(self);
            glib::timeout_add_seconds_local_once(60, move || {
                if let Some(this) = weak.upgrade() {
                    this.retry_scheduled.set(false);
                    if !this.shutting_down.get() && *this.state.borrow() == EngineState::Locked {
                        this.start();
                    }
                }
            });
        }
        if self.restart_pending.replace(false) {
            self.start();
        }
    }

    /// Ask for a pass now; `Err` is the toast when there's no engine.
    pub fn sync_now(&self) -> Result<(), String> {
        match self.handle.borrow().as_ref() {
            Some(handle) => {
                handle.sync_now(false);
                Ok(())
            }
            None => Err(sync_now_refusal(&self.state.borrow())),
        }
    }

    /// On quit: one last pass (≤ [`FLUSH_TIMEOUT`]), then stop. A pass still
    /// running past the timeout is told to stop and not waited on.
    pub fn shutdown(&self) {
        self.shutting_down.set(true);
        self.restart_pending.set(false);
        let handle = self.handle.borrow_mut().take();
        if let Some(handle) = handle {
            if handle.flush(FLUSH_TIMEOUT) {
                handle.stop();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// In-app connect
// ---------------------------------------------------------------------------

/// Why connecting stopped; `message()` is the one-line toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    InvalidConfig,
    NoAppKey,
    Prepare(String),
    NoCode,
    CodeRejected,
    Offline,
    Dropbox(String),
    Store(String),
}

impl ConnectError {
    pub fn message(&self) -> String {
        match self {
            ConnectError::InvalidConfig => {
                "config.toml has a mistake, so sync is paused; fix it, then try again".into()
            }
            ConnectError::NoAppKey => {
                "Add your Dropbox app key to config.toml first (see \"Dropbox sync\" in the README)"
                    .into()
            }
            ConnectError::Prepare(why) => format!("Couldn't prepare the sign-in: {why}"),
            ConnectError::NoCode => "No code entered; nothing changed".into(),
            ConnectError::CodeRejected => {
                "Dropbox didn't accept that code (codes work once); press Ctrl+Shift+D for a fresh one"
                    .into()
            }
            ConnectError::Offline => "Can't reach Dropbox; check your connection".into(),
            ConnectError::Dropbox(why) => format!("Dropbox error: {why}"),
            ConnectError::Store(why) => format!("Couldn't store the sign-in: {why}"),
        }
    }
}

fn remote_error(error: RemoteError) -> ConnectError {
    match error {
        RemoteError::Unauthorized => ConnectError::CodeRejected,
        RemoteError::Offline => ConnectError::Offline,
        other => ConnectError::Dropbox(other.to_string()),
    }
}

/// A successful connect.
#[derive(Debug)]
pub struct Connected {
    /// The account email (or id) for the "Connected as …" toast.
    pub who: String,
}

/// One sign-in attempt: [`ConnectFlow::begin`] makes the authorize URL and
/// keeps the PKCE verifier; [`ConnectFlow::finish`] exchanges the pasted
/// code and stores the login. `Send`, so `finish` can run off the main thread.
pub struct ConnectFlow {
    app_key: String,
    verifier: Secret,
    url: String,
}

impl std::fmt::Debug for ConnectFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectFlow")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl ConnectFlow {
    /// Cheap and offline (no network): safe on the main thread.
    pub fn begin(config: &Config, api: &dyn DropboxApi) -> Result<Self, ConnectError> {
        if !config.loaded_ok {
            return Err(ConnectError::InvalidConfig);
        }
        let app_key = config.sync.dropbox_app_key.trim().to_owned();
        if app_key.is_empty() {
            return Err(ConnectError::NoAppKey);
        }
        let pkce = Pkce::generate().map_err(|error| ConnectError::Prepare(error.to_string()))?;
        let url = api.authorize_url(&app_key, &pkce.challenge);
        Ok(Self {
            app_key,
            verifier: pkce.verifier,
            url,
        })
    }

    /// Open this in the browser (see [`open_in_browser`]).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Blocking network calls: run it off the GTK main thread
    /// (see [`finish_in_background`]). The code is never logged or returned.
    pub fn finish(
        self,
        code: &Secret,
        api: &dyn DropboxApi,
        store: &dyn TokenStore,
    ) -> Result<Connected, ConnectError> {
        let code = Secret::new(code.expose().trim());
        if code.expose().is_empty() {
            return Err(ConnectError::NoCode);
        }
        let refresh = api
            .exchange(&self.app_key, &code, &self.verifier)
            .map_err(remote_error)?;
        let account =
            api.account(&self.app_key, &refresh)
                .map_err(|error| match remote_error(error) {
                    // A fresh token being refused is Dropbox's problem, not the code's.
                    ConnectError::CodeRejected => {
                        ConnectError::Dropbox("the new sign-in was refused".into())
                    }
                    other => other,
                })?;
        let login = StoredLogin {
            refresh_token: refresh,
            account_id: account.account_id,
            email: account.email,
        };
        store
            .save(&login)
            .map_err(|error| ConnectError::Store(error.to_string()))?;
        let who = login
            .email
            .clone()
            .unwrap_or_else(|| login.account_id.clone());
        Ok(Connected { who })
    }
}

/// Run [`ConnectFlow::finish`] on a worker thread; await the single result on
/// the main context with `glib::spawn_future_local(async move { rx.recv().await })`.
/// `store` is built on that thread too: opening the keyring can block.
pub fn finish_in_background(
    flow: ConnectFlow,
    code: Secret,
    api: Box<dyn DropboxApi + Send>,
    store: impl FnOnce() -> Box<dyn TokenStore> + Send + 'static,
) -> async_channel::Receiver<Result<Connected, ConnectError>> {
    let (tx, rx) = async_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("nv-sync-connect".into())
        .spawn({
            let tx = tx.clone();
            move || {
                let store = store();
                let result = flow.finish(&code, api.as_ref(), store.as_ref());
                let _ = tx.send_blocking(result);
            }
        });
    if let Err(error) = spawned {
        let _ = tx.try_send(Err(ConnectError::Prepare(error.to_string())));
    }
    rx
}

/// Open the authorize URL with the desktop's default browser (non-blocking).
pub fn open_in_browser(url: &str) -> bool {
    match gio::AppInfo::launch_default_for_uri(url, gio::AppLaunchContext::NONE) {
        Ok(()) => true,
        Err(error) => {
            glib::g_warning!("nv-linux", "Couldn't open the browser: {error}");
            false
        }
    }
}

/// Placeholder the search field shows while it waits for the pasted code.
pub const CODE_PROMPT: &str = "Paste the Dropbox code, then Enter (Esc cancels)";

/// Toast when no browser opened: the window copies the link to the clipboard.
pub const BROWSER_FALLBACK: &str =
    "Couldn't open a browser; the sign-in link is copied, paste it into one";

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone;
    use nv_sync::dropbox::Account;
    use nv_sync::remote::{Remote, RemoteResult};

    use super::*;

    const CODE: &str = "SENTINEL-CODE-5d1e";
    const REFRESH: &str = "SENTINEL-REFRESH-0c7b";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap()
    }

    fn idle(secs_ago: i64) -> SyncStatus {
        SyncStatus::Idle {
            last_ok: now() - chrono::Duration::seconds(secs_ago),
        }
    }

    fn text(status: &SyncStatus, conflicts: usize) -> Option<String> {
        status_text(status, conflicts, now())
    }

    #[test]
    fn sync_ui_off_hides_the_label_even_with_conflicts() {
        assert_eq!(text(&SyncStatus::Off, 0), None);
        assert_eq!(text(&SyncStatus::Off, 3), None);
    }

    #[test]
    fn sync_ui_status_grid() {
        let long = "this would remove 12 notes; check the notes folder and Dropbox, then run: nv-linux sync now --allow-deletes";
        let cases: Vec<(SyncStatus, usize, &str)> = vec![
            (SyncStatus::Starting, 0, "↻ syncing"),
            (SyncStatus::Syncing, 0, "↻ syncing"),
            (SyncStatus::Background, 0, "syncing in background"),
            (
                SyncStatus::Background,
                2,
                "syncing in background · ⚠ 2 conflicts",
            ),
            (idle(0), 0, "✓ synced just now"),
            (idle(59), 0, "✓ synced just now"),
            (idle(60), 0, "✓ synced 1m ago"),
            (idle(125), 0, "✓ synced 2m ago"),
            (idle(3_599), 0, "✓ synced 59m ago"),
            (idle(3_600), 0, "✓ synced 1h ago"),
            (idle(86_399), 0, "✓ synced 23h ago"),
            (idle(86_400), 0, "✓ synced 1d ago"),
            (idle(10 * 86_400), 0, "✓ synced 10d ago"),
            (idle(-300), 0, "✓ synced just now"),
            (SyncStatus::Offline, 0, "offline"),
            (
                SyncStatus::AuthExpired,
                0,
                "reconnect Dropbox · Ctrl+Shift+D",
            ),
            (
                SyncStatus::Attention(long.into()),
                0,
                "⚠ this would remove 12 notes",
            ),
            (
                SyncStatus::Attention("The notes folder is missing.".into()),
                0,
                "⚠ The notes folder is missing",
            ),
            (
                SyncStatus::Attention("".into()),
                0,
                "⚠ sync needs attention",
            ),
            (SyncStatus::Syncing, 1, "↻ syncing · ⚠ 1 conflict"),
            (idle(125), 2, "✓ synced 2m ago · ⚠ 2 conflicts"),
            (SyncStatus::Offline, 3, "offline · ⚠ 3 conflicts"),
            (
                SyncStatus::AuthExpired,
                1,
                "reconnect Dropbox · Ctrl+Shift+D · ⚠ 1 conflict",
            ),
            (
                SyncStatus::Attention("stuck".into()),
                2,
                "⚠ stuck · 2 conflicts",
            ),
        ];
        for (status, conflicts, want) in cases {
            assert_eq!(
                text(&status, conflicts).as_deref(),
                Some(want),
                "{status:?} with {conflicts} conflicts"
            );
        }
    }

    #[test]
    fn sync_ui_attention_is_capped_short() {
        let message = "x".repeat(200);
        let shown = text(&SyncStatus::Attention(message), 0).unwrap();
        assert!(shown.ends_with('…'));
        // "⚠ " + at most ATTENTION_CHARS.
        assert!(shown.chars().count() <= ATTENTION_CHARS + 2, "{shown}");
        let multi = text(&SyncStatus::Attention("first line\nsecond".into()), 0).unwrap();
        assert_eq!(multi, "⚠ first line");
        // Non-ASCII is cut on a char boundary, never mid-byte.
        let accents = "é".repeat(100);
        let shown = text(&SyncStatus::Attention(accents), 0).unwrap();
        assert_eq!(shown.chars().count(), ATTENTION_CHARS + 2);
    }

    #[test]
    fn sync_ui_tones() {
        assert_eq!(status_tone(&SyncStatus::Syncing, 0), Tone::Busy);
        assert_eq!(status_tone(&SyncStatus::Starting, 0), Tone::Busy);
        assert_eq!(status_tone(&idle(10), 0), Tone::Quiet);
        assert_eq!(status_tone(&SyncStatus::Background, 0), Tone::Quiet);
        assert_eq!(status_tone(&SyncStatus::Background, 1), Tone::Warn);
        assert_eq!(status_tone(&SyncStatus::Offline, 0), Tone::Quiet);
        assert_eq!(status_tone(&SyncStatus::AuthExpired, 0), Tone::Warn);
        assert_eq!(
            status_tone(&SyncStatus::Attention("x".into()), 0),
            Tone::Warn
        );
        assert_eq!(status_tone(&idle(10), 1), Tone::Warn);
        assert_eq!(status_tone(&SyncStatus::Syncing, 1), Tone::Warn);
        for tone in [Tone::Quiet, Tone::Busy, Tone::Warn] {
            assert!(tone.css_class().starts_with("nv-sync-"));
        }
    }

    #[test]
    fn sync_ui_conflict_copy_names() {
        assert!(is_conflict_copy("todo (conflict 2026-09-28 1204)"));
        assert!(!is_conflict_copy("conflict resolution notes"));
        assert!(!is_conflict_copy("todo"));
    }

    #[test]
    fn sync_ui_status_channel_bridges_threads() {
        let (tx, rx) = status_channel();
        std::thread::spawn(move || {
            tx.send(SyncStatus::Starting).unwrap();
            tx.send(SyncStatus::Offline).unwrap();
        });
        assert_eq!(rx.recv_blocking(), Ok(SyncStatus::Starting));
        assert_eq!(rx.recv_blocking(), Ok(SyncStatus::Offline));
        // The engine dropping its sender closes the receiver.
        assert!(rx.recv_blocking().is_err());
    }

    // --- connect flow, with fakes ----------------------------------------

    #[derive(Default)]
    struct Calls {
        challenges: Vec<String>,
        codes: Vec<String>,
        verifiers: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct FakeDropbox {
        calls: Arc<Mutex<Calls>>,
        exchange_error: Option<RemoteError>,
        account_error: Option<RemoteError>,
    }

    impl DropboxApi for FakeDropbox {
        fn authorize_url(&self, app_key: &str, challenge: &str) -> String {
            self.calls
                .lock()
                .unwrap()
                .challenges
                .push(challenge.to_owned());
            format!(
                "https://example.invalid/authorize?client_id={app_key}&code_challenge={challenge}"
            )
        }
        fn exchange(
            &self,
            app_key: &str,
            code: &Secret,
            verifier: &Secret,
        ) -> RemoteResult<Secret> {
            assert_eq!(app_key, "test-app-key");
            let mut calls = self.calls.lock().unwrap();
            calls.codes.push(code.expose().to_owned());
            calls.verifiers.push(verifier.expose().to_owned());
            match &self.exchange_error {
                Some(error) => Err(error.clone()),
                None => Ok(Secret::new(REFRESH)),
            }
        }
        fn account(&self, _: &str, refresh: &Secret) -> RemoteResult<Account> {
            assert_eq!(refresh.expose(), REFRESH);
            match &self.account_error {
                Some(error) => Err(error.clone()),
                None => Ok(Account {
                    account_id: "dbid:test".into(),
                    email: Some("member@example.com".into()),
                }),
            }
        }
        fn remote(&self, _: &str, _: &Secret) -> Box<dyn Remote> {
            panic!("connect must not build a remote")
        }
        fn revoke(&self, _: &str, _: &Secret) -> RemoteResult<()> {
            panic!("connect must not revoke")
        }
    }

    /// (refresh token, account id, email) — `StoredLogin` isn't `Clone`.
    type Saved = (String, String, Option<String>);

    #[derive(Clone, Default)]
    struct MemoryStore {
        saved: Arc<Mutex<Option<Saved>>>,
        fail: bool,
    }

    impl TokenStore for MemoryStore {
        fn load(&self) -> nv_sync::Result<Option<StoredLogin>> {
            Ok(self
                .saved
                .lock()
                .unwrap()
                .clone()
                .map(|(token, id, email)| StoredLogin {
                    refresh_token: Secret::new(token),
                    account_id: id,
                    email,
                }))
        }
        fn save(&self, login: &StoredLogin) -> nv_sync::Result<()> {
            if self.fail {
                return Err(nv_sync::Error::Auth("keyring unavailable: locked".into()));
            }
            *self.saved.lock().unwrap() = Some((
                login.refresh_token.expose().to_owned(),
                login.account_id.clone(),
                login.email.clone(),
            ));
            Ok(())
        }
        fn clear(&self) -> nv_sync::Result<()> {
            *self.saved.lock().unwrap() = None;
            Ok(())
        }
        fn describe(&self) -> &'static str {
            "memory"
        }
    }

    fn config() -> Config {
        let mut config = Config::default();
        config.sync.dropbox_app_key = " test-app-key ".into();
        config
    }

    #[test]
    fn sync_ui_connect_refuses_without_key_or_valid_config() {
        let api = FakeDropbox::default();
        let mut no_key = config();
        no_key.sync.dropbox_app_key = "   ".into();
        assert_eq!(
            ConnectFlow::begin(&no_key, &api).unwrap_err(),
            ConnectError::NoAppKey
        );
        let mut broken = config();
        broken.loaded_ok = false;
        assert_eq!(
            ConnectFlow::begin(&broken, &api).unwrap_err(),
            ConnectError::InvalidConfig
        );
        assert!(api.calls.lock().unwrap().challenges.is_empty());
        assert!(ConnectError::NoAppKey.message().contains("README"));
    }

    #[test]
    fn sync_ui_connect_happy_path_stores_login() {
        let api = FakeDropbox::default();
        let store = MemoryStore::default();
        let flow = ConnectFlow::begin(&config(), &api).unwrap();
        assert!(flow.url().starts_with("https://example.invalid/authorize"));
        assert!(flow.url().contains("client_id=test-app-key&"));
        // The verifier never shows up in the URL or the Debug output.
        let verifier = flow.verifier.expose().to_owned();
        assert_eq!(verifier.len(), 64);
        assert!(!flow.url().contains(&verifier));
        assert!(!format!("{flow:?}").contains(&verifier));

        let connected = flow
            .finish(&Secret::new(format!("  {CODE}\n")), &api, &store)
            .unwrap();
        assert_eq!(connected.who, "member@example.com");
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls.codes, vec![CODE.to_owned()]);
        assert_eq!(calls.verifiers, vec![verifier]);
        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.refresh_token.expose(), REFRESH);
        assert_eq!(saved.email.as_deref(), Some("member@example.com"));
        assert_eq!(saved.account_id, "dbid:test");
        assert!(!format!("{connected:?}").contains(REFRESH));
    }

    #[test]
    fn sync_ui_connect_falls_back_to_account_id() {
        struct NoEmail(FakeDropbox);
        impl DropboxApi for NoEmail {
            fn authorize_url(&self, a: &str, c: &str) -> String {
                self.0.authorize_url(a, c)
            }
            fn exchange(&self, a: &str, c: &Secret, v: &Secret) -> RemoteResult<Secret> {
                self.0.exchange(a, c, v)
            }
            fn account(&self, _: &str, _: &Secret) -> RemoteResult<Account> {
                Ok(Account {
                    account_id: "dbid:noemail".into(),
                    email: None,
                })
            }
            fn remote(&self, a: &str, r: &Secret) -> Box<dyn Remote> {
                self.0.remote(a, r)
            }
            fn revoke(&self, a: &str, r: &Secret) -> RemoteResult<()> {
                self.0.revoke(a, r)
            }
        }
        let api = NoEmail(FakeDropbox::default());
        let flow = ConnectFlow::begin(&config(), &api).unwrap();
        let connected = flow
            .finish(&Secret::new(CODE), &api, &MemoryStore::default())
            .unwrap();
        assert_eq!(connected.who, "dbid:noemail");
    }

    #[test]
    fn sync_ui_connect_failures_store_nothing() {
        let store = MemoryStore::default();
        let run = |api: &FakeDropbox, code: &str, store: &MemoryStore| {
            let flow = ConnectFlow::begin(&config(), api).unwrap();
            flow.finish(&Secret::new(code), api, store).unwrap_err()
        };

        let api = FakeDropbox::default();
        assert_eq!(run(&api, " \n", &store), ConnectError::NoCode);
        assert!(api.calls.lock().unwrap().codes.is_empty(), "no network");

        let api = FakeDropbox {
            exchange_error: Some(RemoteError::Unauthorized),
            ..FakeDropbox::default()
        };
        let error = run(&api, CODE, &store);
        assert_eq!(error, ConnectError::CodeRejected);
        assert!(error.message().contains("Ctrl+Shift+D"));

        let api = FakeDropbox {
            exchange_error: Some(RemoteError::Offline),
            ..FakeDropbox::default()
        };
        assert_eq!(run(&api, CODE, &store), ConnectError::Offline);

        let api = FakeDropbox {
            account_error: Some(RemoteError::Unauthorized),
            ..FakeDropbox::default()
        };
        assert!(matches!(run(&api, CODE, &store), ConnectError::Dropbox(_)));

        assert!(store.load().unwrap().is_none());

        let failing = MemoryStore {
            fail: true,
            ..MemoryStore::default()
        };
        let error = run(&FakeDropbox::default(), CODE, &failing);
        assert!(matches!(error, ConnectError::Store(_)), "{error:?}");
        for error in [
            error,
            ConnectError::CodeRejected,
            ConnectError::Offline,
            ConnectError::NoCode,
        ] {
            let message = error.message();
            assert!(!message.contains(CODE) && !message.contains(REFRESH));
        }
    }

    #[test]
    fn sync_ui_connect_finishes_on_a_worker_thread() {
        let api = FakeDropbox::default();
        let store = MemoryStore::default();
        let flow = ConnectFlow::begin(&config(), &api).unwrap();
        let rx = finish_in_background(flow, Secret::new(CODE), Box::new(api.clone()), {
            let store = store.clone();
            move || Box::new(store) as Box<dyn TokenStore>
        });
        let connected = rx.recv_blocking().unwrap().unwrap();
        assert_eq!(connected.who, "member@example.com");
        assert!(store.load().unwrap().is_some());
        // Exactly one result, then the channel closes.
        assert!(rx.recv_blocking().is_err());
    }

    // --- starting the engine, with fakes ---------------------------------

    /// Proves the keyring was never read.
    struct UntouchedStore;

    impl TokenStore for UntouchedStore {
        fn load(&self) -> nv_sync::Result<Option<StoredLogin>> {
            panic!("the sign-in store must not be read")
        }
        fn save(&self, _: &StoredLogin) -> nv_sync::Result<()> {
            panic!("save called")
        }
        fn clear(&self) -> nv_sync::Result<()> {
            panic!("clear called")
        }
        fn describe(&self) -> &'static str {
            "untouched"
        }
    }

    /// Dropbox stood in by `FakeRemote`; panics if asked for more than a remote.
    #[derive(Clone)]
    struct EngineDropbox(Option<nv_sync::fake::SharedFake>);

    impl DropboxApi for EngineDropbox {
        fn authorize_url(&self, _: &str, _: &str) -> String {
            panic!("authorize_url called")
        }
        fn exchange(&self, _: &str, _: &Secret, _: &Secret) -> RemoteResult<Secret> {
            panic!("exchange called")
        }
        fn account(&self, _: &str, _: &Secret) -> RemoteResult<Account> {
            panic!("account called")
        }
        fn remote(&self, app_key: &str, refresh: &Secret) -> Box<dyn Remote> {
            assert_eq!(app_key, "test-app-key");
            assert_eq!(refresh.expose(), REFRESH);
            match &self.0 {
                Some(fake) => Box::new(fake.clone()),
                None => panic!("no engine was expected: remote built"),
            }
        }
        fn revoke(&self, _: &str, _: &Secret) -> RemoteResult<()> {
            panic!("revoke called")
        }
    }

    struct StartEnv {
        _tmp: tempfile::TempDir,
        notes: std::path::PathBuf,
        state: std::path::PathBuf,
        home: std::path::PathBuf,
        trash: std::path::PathBuf,
    }

    impl StartEnv {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let env = Self {
                notes: root.join("notes"),
                state: root.join("state"),
                home: root.join("home"),
                trash: root.join("trash"),
                _tmp: tmp,
            };
            for dir in [&env.notes, &env.state, &env.home, &env.trash] {
                std::fs::create_dir_all(dir).unwrap();
            }
            env
        }

        fn config(&self) -> Config {
            let mut config = config();
            config.notes_dir = self.notes.clone();
            config
        }

        fn deps(
            &self,
            config: Config,
            store: Box<dyn TokenStore>,
            dropbox: EngineDropbox,
        ) -> StartDeps {
            let notifier = nv_sync::fake::SharedFake::new(nv_sync::fake::FakeRemote::new());
            StartDeps {
                config,
                state_dir: self.state.clone(),
                store,
                dropbox: Box::new(dropbox),
                notifier: Box::new(notifier),
                trasher: Box::new(nv_core::storage::FakeTrasher {
                    dir: self.trash.clone(),
                }),
                home_dir: Some(self.home.clone()),
            }
        }

        fn connected_store() -> Box<dyn TokenStore> {
            let store = MemoryStore::default();
            *store.saved.lock().unwrap() = Some((
                REFRESH.to_owned(),
                "dbid:test".to_owned(),
                Some("member@example.com".to_owned()),
            ));
            Box::new(store)
        }
    }

    fn outcome_name(outcome: &StartOutcome) -> String {
        match outcome {
            StartOutcome::Started(_) => "Started".into(),
            StartOutcome::Off(reason) => format!("Off({reason:?})"),
            StartOutcome::Locked => "Locked".into(),
            StartOutcome::Failed(message) => format!("Failed({message})"),
        }
    }

    #[test]
    fn sync_ui_gate_needs_valid_config_enabled_and_app_key() {
        assert_eq!(sync_gate(&config()), Ok(()));
        let mut broken = config();
        broken.loaded_ok = false;
        assert_eq!(sync_gate(&broken), Err(OffReason::InvalidConfig));
        let mut off = config();
        off.sync.enabled = false;
        assert_eq!(sync_gate(&off), Err(OffReason::Disabled));
        // A member who never set Dropbox up: the default config.
        assert_eq!(sync_gate(&Config::default()), Err(OffReason::NoAppKey));
    }

    #[test]
    fn sync_ui_start_stays_off_without_touching_the_keyring() {
        let env = StartEnv::new();
        let mut broken = env.config();
        broken.loaded_ok = false;
        let mut disabled = env.config();
        disabled.sync.enabled = false;
        let mut never_set_up = env.config();
        never_set_up.sync.dropbox_app_key = String::new();
        for (config, want) in [
            (broken, OffReason::InvalidConfig),
            (disabled, OffReason::Disabled),
            (never_set_up, OffReason::NoAppKey),
        ] {
            let (tx, rx) = mpsc::channel();
            let outcome = start_engine(
                env.deps(config, Box::new(UntouchedStore), EngineDropbox(None)),
                tx,
            );
            assert_eq!(outcome_name(&outcome), format!("Off({want:?})"));
            // No engine: the status sender is gone and nothing was reported.
            assert!(rx.recv().is_err());
        }
    }

    #[test]
    fn sync_ui_start_without_a_login_starts_nothing() {
        let env = StartEnv::new();
        let (tx, rx) = mpsc::channel();
        let outcome = start_engine(
            env.deps(
                env.config(),
                Box::new(MemoryStore::default()),
                EngineDropbox(None),
            ),
            tx,
        );
        assert_eq!(outcome_name(&outcome), "Off(NotConnected)");
        assert!(rx.recv().is_err());
    }

    #[test]
    fn sync_ui_start_is_silent_when_another_syncer_holds_the_lock() {
        let env = StartEnv::new();
        let _held = nv_sync::engine::try_lock(&env.state).unwrap().unwrap();
        let fake = nv_sync::fake::SharedFake::new(nv_sync::fake::FakeRemote::new());
        let (tx, rx) = mpsc::channel();
        let outcome = start_engine(
            env.deps(
                env.config(),
                StartEnv::connected_store(),
                EngineDropbox(Some(fake)),
            ),
            tx,
        );
        assert_eq!(outcome_name(&outcome), "Locked");
        assert!(rx.recv().is_err());
        assert_eq!(
            sync_now_refusal(&EngineState::Locked),
            "The background service is syncing these notes"
        );
    }

    #[test]
    fn sync_ui_start_reports_why_it_could_not_start() {
        let env = StartEnv::new();
        let mut config = env.config();
        config.notes_dir = env.notes.join("missing");
        let fake = nv_sync::fake::SharedFake::new(nv_sync::fake::FakeRemote::new());
        let (tx, _rx) = mpsc::channel();
        let outcome = start_engine(
            env.deps(
                config,
                StartEnv::connected_store(),
                EngineDropbox(Some(fake)),
            ),
            tx,
        );
        let name = outcome_name(&outcome);
        assert!(
            name.starts_with("Failed(") && name.contains("notes folder"),
            "{name}"
        );
        assert!(!name.contains(REFRESH));
    }

    #[test]
    fn sync_ui_start_runs_the_first_pass_and_flushes() {
        let env = StartEnv::new();
        std::fs::write(env.notes.join("hello.txt"), "hi").unwrap();
        let fake = nv_sync::fake::SharedFake::new(nv_sync::fake::FakeRemote::with_hasher(
            nv_sync::content_hash::content_hash,
        ));
        let (tx, rx) = mpsc::channel();
        let outcome = start_engine(
            env.deps(
                env.config(),
                StartEnv::connected_store(),
                EngineDropbox(Some(fake.clone())),
            ),
            tx,
        );
        let StartOutcome::Started(handle) = outcome else {
            panic!("expected Started, got {}", outcome_name(&outcome));
        };
        assert!(handle.flush(Duration::from_secs(10)), "flush timed out");
        assert!(fake.with(|remote| remote.file("hello.txt").is_some()));
        handle.stop();
        let statuses: Vec<SyncStatus> = rx.try_iter().collect();
        assert_eq!(statuses.first(), Some(&SyncStatus::Starting));
        assert!(
            statuses
                .iter()
                .any(|status| matches!(status, SyncStatus::Idle { .. })),
            "{statuses:?}"
        );
        // The lock is free again once the engine stopped.
        assert!(nv_sync::engine::try_lock(&env.state).unwrap().is_some());
    }

    #[test]
    fn sync_ui_sync_now_refusals_point_somewhere() {
        for state in [
            EngineState::NotStarted,
            EngineState::Off(OffReason::NotConnected),
            EngineState::Off(OffReason::NoAppKey),
        ] {
            assert_eq!(
                sync_now_refusal(&state),
                "Not connected to Dropbox — Ctrl+Shift+D connects"
            );
        }
        assert!(sync_now_refusal(&EngineState::Off(OffReason::Disabled)).contains("enabled"));
        assert!(
            sync_now_refusal(&EngineState::Off(OffReason::InvalidConfig)).contains("config.toml")
        );
        assert_eq!(
            sync_now_refusal(&EngineState::Failed("notes folder is missing".into())),
            "Sync couldn't start: notes folder is missing"
        );
    }
}
