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
// The window/app task wires these in; until then nothing calls them.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::mpsc;

use chrono::{DateTime, Utc};
use gtk::prelude::*;
use gtk::{gio, glib};
use nv_core::config::Config;
use nv_sync::dropbox::oauth::Pkce;
use nv_sync::engine::SyncStatus;
use nv_sync::remote::RemoteError;
use nv_sync::secret::Secret;
use nv_sync::tokens::{StoredLogin, TokenStore};

use crate::sync_cli::DropboxApi;

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
/// The window counts these as they appear to feed [`StatusHub::add_conflicts`].
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

    pub fn widget(&self) -> &gtk::Label {
        &self.label
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

/// One per app: remembers the last status and the session's conflict count,
/// pushes them to every attached window's indicator, and re-renders every
/// 30 s so "synced Nm ago" stays true. Main thread only (`Rc`).
pub struct StatusHub {
    status: RefCell<SyncStatus>,
    conflicts: Cell<usize>,
    indicators: RefCell<Vec<Weak<SyncIndicator>>>,
}

impl StatusHub {
    pub fn new() -> Rc<Self> {
        let hub = Rc::new(Self {
            status: RefCell::new(SyncStatus::Off),
            conflicts: Cell::new(0),
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
        indicator.set_status(&self.status.borrow(), self.conflicts.get());
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

    pub fn add_conflicts(&self, n: usize) {
        self.conflicts.set(self.conflicts.get() + n);
        self.render();
    }

    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }

    fn render(&self) {
        // Clone out first: widget calls must not run under our own borrows.
        let status = self.status.borrow().clone();
        let conflicts = self.conflicts.get();
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
    pub login: StoredLogin,
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
        Ok(Connected { who, login })
    }
}

/// Run [`ConnectFlow::finish`] on a worker thread; await the single result on
/// the main context with `glib::spawn_future_local(async move { rx.recv().await })`.
pub fn finish_in_background(
    flow: ConnectFlow,
    code: Secret,
    api: Box<dyn DropboxApi + Send>,
    store: Box<dyn TokenStore>,
) -> async_channel::Receiver<Result<Connected, ConnectError>> {
    let (tx, rx) = async_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("nv-sync-connect".into())
        .spawn({
            let tx = tx.clone();
            move || {
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
        assert_eq!(connected.login.account_id, "dbid:test");
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls.codes, vec![CODE.to_owned()]);
        assert_eq!(calls.verifiers, vec![verifier]);
        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.refresh_token.expose(), REFRESH);
        assert_eq!(saved.email.as_deref(), Some("member@example.com"));
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
        let rx = finish_in_background(
            flow,
            Secret::new(CODE),
            Box::new(api.clone()),
            Box::new(store.clone()),
        );
        let connected = rx.recv_blocking().unwrap().unwrap();
        assert_eq!(connected.who, "member@example.com");
        assert!(store.load().unwrap().is_some());
        // Exactly one result, then the channel closes.
        assert!(rx.recv_blocking().is_err());
    }
}
