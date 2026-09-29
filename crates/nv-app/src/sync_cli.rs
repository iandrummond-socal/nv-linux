//! `nv-linux sync connect | status | now | disconnect`.
//!
//! Parsing is a pure function, and every command runs against injected
//! dependencies ([`Ctx`]), so the whole flow is testable without the network,
//! the real keyring or the member's folders. [`run_from_env`] wires the real ones.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use nv_core::config::Config;
use nv_core::storage::{SystemTrasher, Trasher};
use nv_core::watcher::is_note_path;
use nv_sync::dropbox::oauth::{self, Endpoints, Pkce};
use nv_sync::dropbox::{Account, DropboxRemote};
use nv_sync::engine::{try_lock, Engine, EngineOptions, PassReport};
use nv_sync::plan::SkipReason;
use nv_sync::remote::{Remote, RemoteChange, RemoteError, RemoteResult};
use nv_sync::secret::Secret;
use nv_sync::state::{state_dir_with, SyncState, STATE_FILE};
use nv_sync::tokens::{default_store, StoredLogin, TokenStore};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ATTENTION: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

const CONNECTED_APPS: &str = "https://www.dropbox.com/account/connected_apps";

pub const USAGE: &str = "\
Usage: nv-linux sync <command>

Commands:
  connect [--no-open]     Sign in to Dropbox and run the first sync
  status                  Show the connection and last sync (exit 1 if it needs attention)
  now [--allow-deletes]   Run one sync pass now
  disconnect              Sign out of Dropbox; your notes stay where they are
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCommand {
    Connect { open_browser: bool },
    Status,
    Now { allow_deletes: bool },
    Disconnect,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

/// Parse the arguments after `sync`.
pub fn parse(args: &[String]) -> Result<SyncCommand, UsageError> {
    let Some((verb, rest)) = args.split_first() else {
        return Err(UsageError("missing command".into()));
    };
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(SyncCommand::Help);
    }
    let allowed: &[&str] = match verb.as_str() {
        "help" | "-h" | "--help" => return Ok(SyncCommand::Help),
        "connect" => &["--no-open"],
        "now" => &["--allow-deletes"],
        "status" | "disconnect" => &[],
        other => return Err(UsageError(format!("unknown command '{other}'"))),
    };
    if let Some(extra) = rest.iter().find(|arg| !allowed.contains(&arg.as_str())) {
        return Err(UsageError(format!(
            "unexpected argument '{extra}' for 'sync {verb}'"
        )));
    }
    let has = |flag: &str| rest.iter().any(|arg| arg == flag);
    Ok(match verb.as_str() {
        "connect" => SyncCommand::Connect {
            open_browser: !has("--no-open"),
        },
        "now" => SyncCommand::Now {
            allow_deletes: has("--allow-deletes"),
        },
        "status" => SyncCommand::Status,
        _ => SyncCommand::Disconnect,
    })
}

/// Everything that talks to Dropbox, so tests never touch the network.
pub trait DropboxApi {
    fn authorize_url(&self, app_key: &str, challenge: &str) -> String;
    /// Exchange a pasted code; returns the long-lived refresh token.
    fn exchange(&self, app_key: &str, code: &Secret, verifier: &Secret) -> RemoteResult<Secret>;
    fn account(&self, app_key: &str, refresh: &Secret) -> RemoteResult<Account>;
    fn remote(&self, app_key: &str, refresh: &Secret) -> Box<dyn Remote>;
    fn revoke(&self, app_key: &str, refresh: &Secret) -> RemoteResult<()>;
}

/// The real Dropbox HTTP API. Building one is offline; only its calls use the network.
pub struct RealDropbox {
    endpoints: Endpoints,
}

impl RealDropbox {
    pub fn new() -> Self {
        Self {
            endpoints: Endpoints::dropbox(),
        }
    }
}

impl Default for RealDropbox {
    fn default() -> Self {
        Self::new()
    }
}

impl DropboxApi for RealDropbox {
    fn authorize_url(&self, app_key: &str, challenge: &str) -> String {
        oauth::authorize_url(&self.endpoints, app_key, challenge)
    }

    fn exchange(&self, app_key: &str, code: &Secret, verifier: &Secret) -> RemoteResult<Secret> {
        let tokens =
            oauth::exchange_code(&oauth::agent(), &self.endpoints, app_key, code, verifier)?;
        tokens
            .refresh_token
            .ok_or_else(|| RemoteError::Other("Dropbox did not return a long-lived sign-in".into()))
    }

    fn account(&self, app_key: &str, refresh: &Secret) -> RemoteResult<Account> {
        DropboxRemote::with_endpoints(app_key, refresh.clone(), &self.endpoints).current_account()
    }

    fn remote(&self, app_key: &str, refresh: &Secret) -> Box<dyn Remote> {
        Box::new(DropboxRemote::with_endpoints(
            app_key,
            refresh.clone(),
            &self.endpoints,
        ))
    }

    fn revoke(&self, app_key: &str, refresh: &Secret) -> RemoteResult<()> {
        DropboxRemote::with_endpoints(app_key, refresh.clone(), &self.endpoints).revoke()
    }
}

pub type TrasherFactory<'a> = &'a dyn Fn() -> Box<dyn Trasher + Send>;

/// Injected dependencies for one command.
pub struct Ctx<'a> {
    pub config: &'a Config,
    pub state_dir: &'a Path,
    pub store: &'a dyn TokenStore,
    pub dropbox: &'a dyn DropboxApi,
    pub trasher: TrasherFactory<'a>,
    /// Home folder for the engine's Dropbox-client guard; `None` = the real one.
    pub home_dir: Option<PathBuf>,
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Opens a URL in the browser; false when that failed.
    pub open_url: &'a dyn Fn(&str) -> bool,
}

impl Ctx<'_> {
    fn say(&mut self, line: impl AsRef<str>) {
        let _ = writeln!(self.out, "{}", line.as_ref());
    }

    fn fail(&mut self, line: impl AsRef<str>) -> i32 {
        let _ = writeln!(self.err, "nv-linux sync: {}", line.as_ref());
        EXIT_ATTENTION
    }

    fn app_key(&self) -> &str {
        self.config.sync.dropbox_app_key.trim()
    }
}

/// Parse `args` (after `sync`), wire the real dependencies and run.
pub fn run_from_env(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(UsageError(message)) => {
            eprintln!("nv-linux sync: {message}\n\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    if command == SyncCommand::Help {
        print!("{USAGE}");
        return EXIT_OK;
    }
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("nv-linux sync: couldn't load config.toml: {error}");
            return EXIT_ATTENTION;
        }
    };
    let Some(state_dir) = state_dir_with(&|key| std::env::var(key).ok()) else {
        eprintln!("nv-linux sync: can't find a state folder (set NV_STATE_DIR)");
        return EXIT_ATTENTION;
    };
    let store = default_store(&state_dir);
    let dropbox = RealDropbox::new();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = io::stdout();
    let mut err = io::stderr();
    let mut ctx = Ctx {
        config: &config,
        state_dir: &state_dir,
        store: &store,
        dropbox: &dropbox,
        trasher: &|| Box::new(SystemTrasher),
        home_dir: None,
        input: &mut input,
        out: &mut out,
        err: &mut err,
        open_url: &xdg_open,
    };
    execute(command, &mut ctx)
}

fn xdg_open(url: &str) -> bool {
    // Not waited on: a terminal browser must never take over this prompt.
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

pub fn execute(command: SyncCommand, ctx: &mut Ctx) -> i32 {
    let code = match command {
        SyncCommand::Connect { open_browser } => connect(ctx, open_browser),
        SyncCommand::Status => status(ctx),
        SyncCommand::Now { allow_deletes } => now(ctx, allow_deletes),
        SyncCommand::Disconnect => disconnect(ctx),
        SyncCommand::Help => {
            let _ = write!(ctx.out, "{USAGE}");
            EXIT_OK
        }
    };
    let _ = ctx.out.flush();
    let _ = ctx.err.flush();
    code
}

fn not_configured_help() -> &'static str {
    "Dropbox sync needs your own Dropbox app key first. \
     See \"Dropbox sync\" in the nv-linux README: create the app, then add\n\n  \
     [sync]\n  dropbox_app_key = \"<your app key>\"\n\n\
     to ~/.config/nv-linux/config.toml and run: nv-linux sync connect"
}

const INVALID_CONFIG: &str = "config.toml has a mistake, so nv-linux fell back to its defaults \
     and sync is paused. Fix ~/.config/nv-linux/config.toml, then try again.";

fn connect(ctx: &mut Ctx, open_browser: bool) -> i32 {
    if !ctx.config.loaded_ok {
        return ctx.fail(INVALID_CONFIG);
    }
    if ctx.app_key().is_empty() {
        return ctx.fail(not_configured_help());
    }
    let app_key = ctx.app_key().to_owned();
    let pkce = match Pkce::generate() {
        Ok(pkce) => pkce,
        Err(error) => return ctx.fail(format!("couldn't prepare the sign-in: {error}")),
    };
    let url = ctx.dropbox.authorize_url(&app_key, &pkce.challenge);
    ctx.say("Open this link, allow access, then paste the code Dropbox shows you:");
    ctx.say(&url);
    if open_browser && !(ctx.open_url)(&url) {
        ctx.say("(Couldn't open a browser; open the link above yourself.)");
    }
    let _ = write!(ctx.out, "Code: ");
    let _ = ctx.out.flush();
    let mut line = String::new();
    if let Err(error) = ctx.input.read_line(&mut line) {
        return ctx.fail(format!("couldn't read the code: {error}"));
    }
    let code = Secret::new(line.trim());
    line.clear();
    if code.expose().is_empty() {
        return ctx.fail("no code entered; nothing changed.");
    }
    let refresh = match ctx.dropbox.exchange(&app_key, &code, &pkce.verifier) {
        Ok(refresh) => refresh,
        Err(RemoteError::Unauthorized) => {
            return ctx.fail(
                "Dropbox didn't accept that code (codes work once and expire quickly). \
                 Run connect again for a fresh one.",
            )
        }
        Err(error) => return ctx.fail(remote_message(&error)),
    };
    let account = match ctx.dropbox.account(&app_key, &refresh) {
        Ok(account) => account,
        Err(error) => return ctx.fail(remote_message(&error)),
    };
    let login = StoredLogin {
        refresh_token: refresh,
        account_id: account.account_id,
        email: account.email,
    };
    if let Err(error) = ctx.store.save(&login) {
        return ctx.fail(format!("couldn't store the sign-in: {error}"));
    }
    ctx.say(format!("Connected as {}.", who(&login)));
    ctx.say("Your notes sync with your Dropbox app folder: Apps/<your Dropbox app's name>/");
    let stored = describe_store(ctx.store, ctx.state_dir);
    ctx.say(format!("Sign-in stored in {stored}."));
    if !ctx.config.sync.enabled {
        ctx.say("Sync is turned off in config.toml ([sync] enabled = false), so no sync ran.");
        return EXIT_OK;
    }
    ctx.say("Running the first sync...");
    finish_pass(ctx, &login, false)
}

fn now(ctx: &mut Ctx, allow_deletes: bool) -> i32 {
    let login = match ctx.store.load() {
        Ok(Some(login)) => login,
        Ok(None) => {
            let _ = writeln!(ctx.err, "nv-linux sync: Dropbox isn't connected.");
            return ctx.fail(how_to_connect(ctx.config));
        }
        Err(error) => return ctx.fail(format!("couldn't read the stored sign-in: {error}")),
    };
    if !ctx.config.sync.enabled {
        return ctx.fail(
            "sync is turned off in config.toml ([sync] enabled = false); \
             set it to true to sync.",
        );
    }
    if ctx.app_key().is_empty() {
        return ctx.fail(not_configured_help());
    }
    if allow_deletes {
        ctx.say("Removals allowed for this one pass.");
    }
    finish_pass(ctx, &login, allow_deletes)
}

/// Run one pass, print its report, and turn it into an exit code.
fn finish_pass(ctx: &mut Ctx, login: &StoredLogin, allow_deletes: bool) -> i32 {
    match pass(ctx, login, allow_deletes) {
        Ok(report) => {
            for line in report_lines(&report) {
                ctx.say(line);
            }
            if report.attention.is_some() {
                EXIT_ATTENTION
            } else {
                EXIT_OK
            }
        }
        Err(message) => ctx.fail(message),
    }
}

/// The engine's options for this config and login; the CLI and the app share it.
pub fn engine_options(
    config: &Config,
    state_dir: &Path,
    login: &StoredLogin,
    home_dir: Option<PathBuf>,
) -> EngineOptions {
    EngineOptions {
        notes_dir: config.notes_dir.clone(),
        state_dir: state_dir.to_path_buf(),
        account_id: login.account_id.clone(),
        config_loaded_ok: config.loaded_ok,
        home_dir,
        debounce: Duration::from_millis(config.sync.debounce_ms),
    }
}

fn pass(ctx: &Ctx, login: &StoredLogin, allow_deletes: bool) -> Result<PassReport, String> {
    let options = engine_options(ctx.config, ctx.state_dir, login, ctx.home_dir.clone());
    let remote = ctx.dropbox.remote(ctx.app_key(), &login.refresh_token);
    let mut engine =
        Engine::new(options, remote, (ctx.trasher)()).map_err(|e| error_message(&e))?;
    engine
        .run_pass(allow_deletes)
        .map_err(|error| error_message(&error))
}

pub fn error_message(error: &nv_sync::Error) -> String {
    match error {
        nv_sync::Error::Locked => {
            "another nv-linux is already syncing; let it finish (or quit the app), then try again."
                .into()
        }
        nv_sync::Error::Config(message) | nv_sync::Error::Auth(message) => message.clone(),
        nv_sync::Error::Remote(error) => remote_message(error),
        other => format!("sync stopped: {other}"),
    }
}

fn remote_message(error: &RemoteError) -> String {
    match error {
        RemoteError::Offline => "can't reach Dropbox; check your connection and try again.".into(),
        RemoteError::Unauthorized => {
            "the Dropbox sign-in has expired or was revoked; run: nv-linux sync connect".into()
        }
        RemoteError::RateLimited(wait) => format!(
            "Dropbox asked nv-linux to slow down; try again in {} seconds.",
            wait.as_secs().max(1)
        ),
        other => format!("Dropbox error: {other}"),
    }
}

fn skip_reason(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::TooLarge => "150 MB or larger, too big to sync",
        SkipReason::BadName => "Dropbox can't store that name (it contains a backslash)",
        SkipReason::NotANote => "not a note",
        SkipReason::CaseCollision => "another note's name differs from it only by case",
        SkipReason::NonUtf8Name => "its name isn't valid UTF-8",
    }
}

/// The printed summary of one pass.
pub fn report_lines(report: &PassReport) -> Vec<String> {
    let mut summary = format!(
        "{} uploaded, {} downloaded, {} conflicts",
        report.uploaded, report.downloaded, report.conflicts
    );
    if report.trashed > 0 {
        summary.push_str(&format!(", {} moved to Trash", report.trashed));
    }
    if report.deleted_remote > 0 {
        summary.push_str(&format!(
            ", {} removed from Dropbox (recoverable there)",
            report.deleted_remote
        ));
    }
    summary.push('.');
    let mut lines = vec![summary];
    if report.conflicts > 0 {
        lines.push("Both versions were kept: look for notes named \"... (conflict ...)\".".into());
    }
    for (name, reason) in &report.skipped {
        lines.push(format!("Skipped {name}: {}.", skip_reason(reason)));
    }
    if let Some(attention) = &report.attention {
        lines.push(format!("Needs attention: {attention}"));
    }
    lines
}

fn who(login: &StoredLogin) -> &str {
    login.email.as_deref().unwrap_or(&login.account_id)
}

fn describe_store(store: &dyn TokenStore, state_dir: &Path) -> String {
    match store.describe() {
        "keyring" => "your system keyring".into(),
        _ => format!(
            "a private file (readable only by you) in {}",
            state_dir.display()
        ),
    }
}

fn how_to_connect(config: &Config) -> &'static str {
    if config.sync.dropbox_app_key.trim().is_empty() {
        not_configured_help()
    } else {
        "To connect, run: nv-linux sync connect"
    }
}

fn status(ctx: &mut Ctx) -> i32 {
    let login = match ctx.store.load() {
        Ok(Some(login)) => login,
        Ok(None) => {
            ctx.say("Dropbox sync: not connected.");
            let help = how_to_connect(ctx.config);
            ctx.say(help);
            return EXIT_ATTENTION;
        }
        Err(error) => return ctx.fail(format!("couldn't read the stored sign-in: {error}")),
    };
    let mut attention: Vec<String> = Vec::new();
    ctx.say(format!("Dropbox sync: connected as {}", who(&login)));
    if !ctx.config.sync.enabled {
        ctx.say("Sync is turned off in config.toml ([sync] enabled = false).");
    }
    let notes_dir = ctx.config.notes_dir.clone();
    ctx.say(format!("Notes folder: {}", notes_dir.display()));
    if !ctx.config.loaded_ok {
        attention.push("config.toml is invalid; sync is paused until it's fixed.".into());
    }
    if !notes_dir.is_dir() {
        attention.push("the notes folder is missing; sync is paused.".into());
    }

    let state_path = ctx.state_dir.join(STATE_FILE);
    match fs::read(&state_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => ctx.say("Last sync: never"),
        Err(error) => ctx.say(format!(
            "Last sync: unknown (can't read the sync state: {error})"
        )),
        Ok(bytes) => {
            let last = fs::metadata(&state_path)
                .and_then(|meta| meta.modified())
                .map(|time| {
                    chrono::DateTime::<chrono::Local>::from(time)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|_| "unknown".into());
            ctx.say(format!("Last sync: {last}"));
            match serde_json::from_slice::<SyncState>(&bytes) {
                Ok(state) => describe_state(ctx, &state, &login, &notes_dir, &mut attention),
                Err(_) => {
                    ctx.say("The saved sync state is unreadable; the next sync starts fresh.");
                }
            }
        }
    }
    let conflicts = conflict_copies(&notes_dir);
    ctx.say(format!("Conflict copies in your notes: {conflicts}"));
    if attention.is_empty() {
        ctx.say("Status: OK");
        EXIT_OK
    } else {
        for line in attention {
            ctx.say(format!("Needs attention: {line}"));
        }
        EXIT_ATTENTION
    }
}

fn describe_state(
    ctx: &mut Ctx,
    state: &SyncState,
    login: &StoredLogin,
    notes_dir: &Path,
    attention: &mut Vec<String>,
) {
    ctx.say(format!("Notes synced: {}", state.files.len()));
    ctx.say(format!("Remote changes waiting: {}", state.pending.len()));
    let canonical = fs::canonicalize(notes_dir).unwrap_or_else(|_| notes_dir.to_path_buf());
    let other_folder = state
        .notes_dir
        .as_ref()
        .is_some_and(|dir| dir != &canonical);
    let other_account = state
        .account_id
        .as_deref()
        .is_some_and(|id| id != login.account_id);
    if other_folder || other_account {
        ctx.say("The next sync starts fresh: the saved sync state is for a different folder or account.");
        return;
    }
    // Remote deletions the mass-change guard is holding back stay pending.
    let held = state
        .pending
        .iter()
        .filter(|change| match change {
            RemoteChange::Deleted { key } => state.files.contains_key(key),
            RemoteChange::Upsert(_) => false,
        })
        .count();
    let known = state.files.len();
    if held > 10 || (held >= 3 && held * 2 > known) {
        attention.push(format!(
            "a sync is waiting for your OK to remove {held} notes; check the notes folder and \
             Dropbox, then run: nv-linux sync now --allow-deletes"
        ));
    }
}

fn conflict_copies(notes_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(notes_dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| is_note_path(&entry.path()))
        .filter(|entry| entry.file_name().to_string_lossy().contains(" (conflict "))
        .count()
}

fn disconnect(ctx: &mut Ctx) -> i32 {
    // Hold the engine's lock so a running app can't rewrite state we remove.
    let _lock = if ctx.state_dir.is_dir() {
        match try_lock(ctx.state_dir) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                return ctx.fail(
                    "nv-linux is syncing right now (the app or another sync command); \
                     quit it, then disconnect.",
                )
            }
            Err(error) => return ctx.fail(format!("couldn't check for a running sync: {error}")),
        }
    } else {
        None
    };
    let login = match ctx.store.load() {
        Ok(login) => login,
        Err(error) => {
            ctx.say(format!(
                "Couldn't read the stored sign-in ({error}); clearing it anyway."
            ));
            None
        }
    };
    if let Some(login) = &login {
        let app_key = ctx.app_key().to_owned();
        if app_key.is_empty() {
            ctx.say(format!(
                "Couldn't sign out at Dropbox (no app key in config.toml). \
                 To revoke access there too, visit {CONNECTED_APPS}"
            ));
        } else {
            match ctx.dropbox.revoke(&app_key, &login.refresh_token) {
                Ok(()) => ctx.say("Signed out at Dropbox."),
                Err(error) => ctx.say(format!(
                    "Couldn't sign out at Dropbox ({}) It's removed from this computer anyway; \
                     to revoke access there too, visit {CONNECTED_APPS}",
                    remote_message(&error)
                )),
            }
        }
    }
    if let Err(error) = ctx.store.clear() {
        return ctx.fail(format!("couldn't remove the stored sign-in: {error}"));
    }
    match fs::remove_file(ctx.state_dir.join(STATE_FILE)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return ctx.fail(format!("couldn't remove the sync state: {error}")),
    }
    ctx.say(if login.is_some() {
        "Disconnected."
    } else {
        "Dropbox wasn't connected."
    });
    let notes = ctx.config.notes_dir.display().to_string();
    ctx.say(format!(
        "Your notes were not touched: nothing was deleted from {notes} or from Dropbox."
    ));
    EXIT_OK
}

/// `Ok(None)` when another process holds the lock.
#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    use nv_core::storage::FakeTrasher;
    use nv_sync::content_hash::content_hash;
    use nv_sync::fake::{FakeRemote, SharedFake};
    use nv_sync::tokens::FileStore;
    use tempfile::TempDir;

    use super::*;

    const REFRESH: &str = "SENTINEL-REFRESH-7f3a9c";
    const CODE: &str = "SENTINEL-CODE-91b2";
    const VERIFIER_MARK: &str = "SENTINEL";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn sync_cli_parser_verbs_and_flags() {
        use SyncCommand::*;
        assert_eq!(
            parse(&args(&["connect"])),
            Ok(Connect { open_browser: true })
        );
        assert_eq!(
            parse(&args(&["connect", "--no-open"])),
            Ok(Connect {
                open_browser: false
            })
        );
        assert_eq!(parse(&args(&["status"])), Ok(Status));
        assert_eq!(
            parse(&args(&["now"])),
            Ok(Now {
                allow_deletes: false
            })
        );
        assert_eq!(
            parse(&args(&["now", "--allow-deletes"])),
            Ok(Now {
                allow_deletes: true
            })
        );
        assert_eq!(parse(&args(&["disconnect"])), Ok(Disconnect));
        for help in [&["help"][..], &["--help"], &["-h"], &["now", "--help"]] {
            assert_eq!(parse(&args(help)), Ok(Help));
        }
    }

    #[test]
    fn sync_cli_parser_rejects_bad_input() {
        assert!(parse(&[]).is_err());
        let error = parse(&args(&["bogus"])).unwrap_err();
        assert!(error.0.contains("bogus"));
        for bad in [
            &["status", "extra"][..],
            &["disconnect", "--allow-deletes"],
            &["now", "--no-open"],
            &["connect", "--allow-deletes"],
            &["connect", "some-code"],
            &["now", "--allow-deletes", "x"],
        ] {
            let error = parse(&args(bad)).unwrap_err();
            assert!(
                error.0.contains("unexpected argument"),
                "{bad:?}: {error:?}"
            );
        }
    }

    /// Never reached: proves a command stayed off the network.
    struct NoNetwork;

    impl DropboxApi for NoNetwork {
        fn authorize_url(&self, _: &str, _: &str) -> String {
            panic!("authorize_url called")
        }
        fn exchange(&self, _: &str, _: &Secret, _: &Secret) -> RemoteResult<Secret> {
            panic!("exchange called")
        }
        fn account(&self, _: &str, _: &Secret) -> RemoteResult<Account> {
            panic!("account called")
        }
        fn remote(&self, _: &str, _: &Secret) -> Box<dyn Remote> {
            panic!("remote factory called")
        }
        fn revoke(&self, _: &str, _: &Secret) -> RemoteResult<()> {
            panic!("revoke called")
        }
    }

    /// Dropbox stood in by `FakeRemote`, with the token exchange injected.
    struct FakeDropbox {
        fake: SharedFake,
        codes: RefCell<Vec<String>>,
        revokes: Cell<usize>,
        revoke_fails: bool,
    }

    impl FakeDropbox {
        fn new() -> Self {
            Self {
                fake: SharedFake::new(FakeRemote::with_hasher(content_hash)),
                codes: RefCell::new(Vec::new()),
                revokes: Cell::new(0),
                revoke_fails: false,
            }
        }
    }

    impl DropboxApi for FakeDropbox {
        fn authorize_url(&self, app_key: &str, challenge: &str) -> String {
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
            assert_eq!(verifier.expose().len(), 64);
            self.codes.borrow_mut().push(code.expose().to_owned());
            Ok(Secret::new(REFRESH))
        }
        fn account(&self, _: &str, refresh: &Secret) -> RemoteResult<Account> {
            assert_eq!(refresh.expose(), REFRESH);
            Ok(Account {
                account_id: "dbid:test".into(),
                email: Some("member@example.com".into()),
            })
        }
        fn remote(&self, _: &str, refresh: &Secret) -> Box<dyn Remote> {
            assert_eq!(refresh.expose(), REFRESH);
            Box::new(self.fake.clone())
        }
        fn revoke(&self, _: &str, refresh: &Secret) -> RemoteResult<()> {
            assert_eq!(refresh.expose(), REFRESH);
            self.revokes.set(self.revokes.get() + 1);
            if self.revoke_fails {
                Err(RemoteError::Offline)
            } else {
                Ok(())
            }
        }
    }

    struct Env {
        _tmp: TempDir,
        notes: PathBuf,
        state: PathBuf,
        home: PathBuf,
        trash: PathBuf,
        config: Config,
        store: FileStore,
    }

    struct Output {
        code: i32,
        out: String,
        err: String,
    }

    impl Output {
        fn all(&self) -> String {
            format!("{}{}", self.out, self.err)
        }
    }

    impl Env {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let notes = tmp.path().join("notes");
            let state = tmp.path().join("state");
            let home = tmp.path().join("home");
            let trash = tmp.path().join("trash");
            fs::create_dir_all(&notes).unwrap();
            fs::create_dir_all(&home).unwrap();
            let mut config = Config {
                notes_dir: notes.clone(),
                ..Config::default()
            };
            config.sync.dropbox_app_key = "test-app-key".into();
            Self {
                store: FileStore::new(&state),
                _tmp: tmp,
                notes,
                state,
                home,
                trash,
                config,
            }
        }

        fn run(&self, dropbox: &dyn DropboxApi, command: SyncCommand, stdin: &str) -> Output {
            let mut input = io::Cursor::new(stdin.as_bytes().to_vec());
            let mut out = Vec::new();
            let mut err = Vec::new();
            let trash = self.trash.clone();
            let trasher =
                move || Box::new(FakeTrasher { dir: trash.clone() }) as Box<dyn Trasher + Send>;
            let opened = RefCell::new(Vec::<String>::new());
            let open = |url: &str| {
                opened.borrow_mut().push(url.to_owned());
                false
            };
            let code = {
                let mut ctx = Ctx {
                    config: &self.config,
                    state_dir: &self.state,
                    store: &self.store,
                    dropbox,
                    trasher: &trasher,
                    home_dir: Some(self.home.clone()),
                    input: &mut input,
                    out: &mut out,
                    err: &mut err,
                    open_url: &open,
                };
                execute(command, &mut ctx)
            };
            let output = Output {
                code,
                out: String::from_utf8(out).unwrap(),
                err: String::from_utf8(err).unwrap(),
            };
            assert_no_secret(&output);
            output
        }

        fn login(&self) {
            self.store
                .save(&StoredLogin {
                    refresh_token: Secret::new(REFRESH),
                    account_id: "dbid:test".into(),
                    email: Some("member@example.com".into()),
                })
                .unwrap();
        }

        fn write_note(&self, name: &str, text: &str) {
            fs::write(self.notes.join(name), text).unwrap();
        }

        fn notes(&self) -> BTreeMap<String, Vec<u8>> {
            fs::read_dir(&self.notes)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        fs::read(entry.path()).unwrap(),
                    )
                })
                .collect()
        }
    }

    fn assert_no_secret(output: &Output) {
        for line in output.all().lines() {
            assert!(!line.contains(REFRESH), "token printed: {line}");
            assert!(!line.contains(CODE), "code printed: {line}");
            assert!(!line.contains(VERIFIER_MARK), "secret printed: {line}");
        }
    }

    #[test]
    fn sync_cli_status_unconnected_is_offline_and_exits_1() {
        let env = Env::new();
        let output = env.run(&NoNetwork, SyncCommand::Status, "");
        assert_eq!(output.code, 1);
        assert!(output.out.contains("not connected"), "{}", output.out);
        assert!(
            output.out.contains("nv-linux sync connect"),
            "{}",
            output.out
        );

        // Without an app key, it points at the README setup instead.
        let mut env = Env::new();
        env.config.sync.dropbox_app_key.clear();
        let output = env.run(&NoNetwork, SyncCommand::Status, "");
        assert_eq!(output.code, 1);
        assert!(output.out.contains("README"), "{}", output.out);
    }

    #[test]
    fn sync_cli_connect_without_app_key_refuses_offline() {
        let mut env = Env::new();
        env.config.sync.dropbox_app_key = "  ".into();
        let output = env.run(
            &NoNetwork,
            SyncCommand::Connect { open_browser: true },
            &format!("{CODE}\n"),
        );
        assert_eq!(output.code, 1);
        assert!(output.err.contains("Dropbox sync"), "{}", output.err);
        assert!(output.err.contains("dropbox_app_key"), "{}", output.err);
        assert!(env.store.load().unwrap().is_none());
        assert!(!env.state.exists());
    }

    #[test]
    fn sync_cli_connect_with_invalid_config_refuses_offline() {
        let mut env = Env::new();
        env.config.loaded_ok = false;
        let output = env.run(
            &NoNetwork,
            SyncCommand::Connect {
                open_browser: false,
            },
            "",
        );
        assert_eq!(output.code, 1);
        assert!(output.err.contains("config.toml"), "{}", output.err);
    }

    #[test]
    fn sync_cli_connect_happy_path_stores_login_and_first_syncs() {
        let env = Env::new();
        let dropbox = FakeDropbox::new();
        env.write_note("Local one.txt", "local one");
        env.write_note("Local two.txt", "local two");
        env.write_note("Both.txt", "local side");
        dropbox.fake.with(|fake| {
            fake.edit_remotely("Remote.txt", b"remote");
            fake.edit_remotely("Both.txt", b"remote side");
        });

        let output = env.run(
            &dropbox,
            SyncCommand::Connect { open_browser: true },
            &format!("  {CODE}  \n"),
        );
        assert_eq!(output.code, 0, "{}", output.all());
        assert_eq!(*dropbox.codes.borrow(), vec![CODE.to_owned()]);
        assert!(output.out.contains("https://example.invalid/authorize"));
        assert!(output.out.contains("Couldn't open a browser"));
        assert!(output.out.contains("Connected as member@example.com."));
        assert!(output.out.contains("Apps/"));
        assert!(
            output
                .out
                .contains("3 uploaded, 1 downloaded, 1 conflicts."),
            "{}",
            output.out
        );
        let login = env.store.load().unwrap().unwrap();
        assert_eq!(login.refresh_token.expose(), REFRESH);
        assert_eq!(login.account_id, "dbid:test");
        assert_eq!(env.notes()["Remote.txt"], b"remote");
        assert_eq!(env.notes()["Both.txt"], b"local side");
        assert!(env.notes().keys().any(|name| name.contains(" (conflict ")));

        // Status after a sync: healthy, and nothing reaches the network.
        let output = env.run(&NoNetwork, SyncCommand::Status, "");
        assert_eq!(output.code, 0, "{}", output.all());
        assert!(output.out.contains("connected as member@example.com"));
        assert!(!output.out.contains("Last sync: never"));
        assert!(output.out.contains("Conflict copies in your notes: 1"));
        assert!(output.out.contains("Status: OK"));
    }

    #[test]
    fn sync_cli_connect_without_code_changes_nothing() {
        let env = Env::new();
        let dropbox = FakeDropbox::new();
        let output = env.run(
            &dropbox,
            SyncCommand::Connect {
                open_browser: false,
            },
            "\n",
        );
        assert_eq!(output.code, 1);
        assert!(output.err.contains("no code entered"));
        assert!(dropbox.codes.borrow().is_empty());
        assert!(env.store.load().unwrap().is_none());
    }

    #[test]
    fn sync_cli_now_prints_counts() {
        let env = Env::new();
        env.login();
        let dropbox = FakeDropbox::new();
        env.write_note("a.txt", "a");
        dropbox.fake.with(|fake| fake.edit_remotely("b.txt", b"b"));
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 0, "{}", output.all());
        assert!(output
            .out
            .contains("1 uploaded, 1 downloaded, 0 conflicts."));
        assert!(dropbox.fake.with(|fake| fake.file("a.txt").is_some()));

        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 0);
        assert!(output
            .out
            .contains("0 uploaded, 0 downloaded, 0 conflicts."));
    }

    #[test]
    fn sync_cli_now_mass_delete_is_guarded_until_allowed() {
        let env = Env::new();
        env.login();
        let dropbox = FakeDropbox::new();
        for index in 0..12 {
            env.write_note(&format!("note {index}.txt"), &format!("text {index}"));
        }
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 0, "{}", output.all());
        assert!(output.out.contains("12 uploaded"));

        dropbox.fake.with(|fake| {
            for index in 0..12 {
                assert!(fake.delete_remotely(&format!("note {index}.txt")));
            }
        });
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 1, "{}", output.all());
        assert!(output
            .out
            .contains("Needs attention: this would remove 12 notes"));
        assert!(output.out.contains("nv-linux sync now --allow-deletes"));
        assert_eq!(env.notes().len(), 12);

        // Status sees the held-back removal too.
        let output = env.run(&NoNetwork, SyncCommand::Status, "");
        assert_eq!(output.code, 1, "{}", output.all());
        assert!(output.out.contains("--allow-deletes"));

        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: true,
            },
            "",
        );
        assert_eq!(output.code, 0, "{}", output.all());
        assert!(output.out.contains("12 moved to Trash"), "{}", output.out);
        assert!(env.notes().is_empty());
        assert_eq!(fs::read_dir(&env.trash).unwrap().count(), 12);
    }

    #[test]
    fn sync_cli_now_reports_errors_in_one_line() {
        let env = Env::new();
        // Not connected: how-to, no network.
        let output = env.run(
            &NoNetwork,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 1);
        assert!(output.err.contains("isn't connected"));

        env.login();
        let dropbox = FakeDropbox::new();
        dropbox
            .fake
            .with(|fake| fake.fail_next(RemoteError::Offline));
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 1);
        assert!(output.err.contains("can't reach Dropbox"), "{}", output.err);

        dropbox
            .fake
            .with(|fake| fake.fail_next(RemoteError::Unauthorized));
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 1);
        assert!(
            output.err.contains("nv-linux sync connect"),
            "{}",
            output.err
        );

        fs::create_dir_all(&env.state).unwrap();
        let held = try_lock(&env.state).unwrap().unwrap();
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 1);
        assert!(output.err.contains("already syncing"), "{}", output.err);
        drop(held);

        let mut env = env;
        env.config.loaded_ok = false;
        let output = env.run(
            &dropbox,
            SyncCommand::Now {
                allow_deletes: false,
            },
            "",
        );
        assert_eq!(output.code, 1);
        assert!(
            output.err.contains("config.toml is invalid"),
            "{}",
            output.err
        );
    }

    #[test]
    fn sync_cli_disconnect_clears_login_and_state_only() {
        let env = Env::new();
        let dropbox = FakeDropbox::new();
        env.write_note("keep.txt", "keep me");
        dropbox
            .fake
            .with(|fake| fake.edit_remotely("remote.txt", b"remote"));
        let output = env.run(
            &dropbox,
            SyncCommand::Connect {
                open_browser: false,
            },
            &format!("{CODE}\n"),
        );
        assert_eq!(output.code, 0, "{}", output.all());
        assert!(env.state.join(STATE_FILE).exists());
        let notes_before = env.notes();
        let remote_before = dropbox.fake.with(|fake| fake.snapshot());

        let output = env.run(&dropbox, SyncCommand::Disconnect, "");
        assert_eq!(output.code, 0, "{}", output.all());
        assert_eq!(dropbox.revokes.get(), 1);
        assert!(output.out.contains("Disconnected."));
        assert!(output.out.contains("not touched"));
        assert!(env.store.load().unwrap().is_none());
        assert!(!env.state.join(STATE_FILE).exists());
        assert_eq!(env.notes(), notes_before);
        assert_eq!(dropbox.fake.with(|fake| fake.snapshot()), remote_before);

        let output = env.run(&NoNetwork, SyncCommand::Status, "");
        assert_eq!(output.code, 1);
        assert!(output.out.contains("not connected"));
    }

    #[test]
    fn sync_cli_disconnect_survives_a_failed_revoke() {
        let env = Env::new();
        env.login();
        let mut dropbox = FakeDropbox::new();
        dropbox.revoke_fails = true;
        let output = env.run(&dropbox, SyncCommand::Disconnect, "");
        assert_eq!(output.code, 0, "{}", output.all());
        assert!(output.out.contains("Couldn't sign out at Dropbox"));
        assert!(output.out.contains(CONNECTED_APPS));
        assert!(env.store.load().unwrap().is_none());
    }

    #[test]
    fn sync_cli_disconnect_waits_for_a_running_sync() {
        let env = Env::new();
        env.login();
        fs::create_dir_all(&env.state).unwrap();
        let held = try_lock(&env.state).unwrap().unwrap();
        let output = env.run(&NoNetwork, SyncCommand::Disconnect, "");
        assert_eq!(output.code, 1);
        assert!(output.err.contains("syncing right now"));
        assert!(env.store.load().unwrap().is_some());
        drop(held);
    }

    #[test]
    fn sync_cli_log_is_capped_at_debug_in_the_app() {
        // nv-sync caps `log` at debug at compile time so ureq-proto's TRACE
        // dumps (which include the bearer token) can't exist; feature
        // unification must carry that cap into this binary.
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Debug);
    }
}
