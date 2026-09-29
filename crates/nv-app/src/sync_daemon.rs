//! Headless sync with fresh dependencies on every start attempt.

use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use nv_sync::engine::SyncStatus;

use crate::sync_ui::{start_engine, OffReason, StartDeps, StartOutcome};

/// Sync until the process is stopped, logging to the command's output.
pub fn run(ctx: &mut crate::sync_cli::Ctx) -> i32 {
    run_with_output(ctx.out)
}

/// Start without preloading CLI dependencies, so startup failures are retried.
pub(crate) fn run_with_output(out: &mut dyn Write) -> i32 {
    let mut make_deps = || {
        let config = nv_core::config::Config::load()
            .map_err(|error| format!("config.toml has a mistake: {error}"))?;
        let state_dir = nv_sync::state::state_dir_with(&|key| std::env::var(key).ok())
            .ok_or_else(|| "can't find a state folder (set NV_STATE_DIR)".to_owned())?;
        Ok(StartDeps::production(config, state_dir))
    };
    // systemd's SIGTERM ends the process; atomic writes and persisted pending
    // operations keep the engine crash-safe at every step.
    run_loop(&mut make_deps, &mut std::thread::sleep, out, None)
}

/// Retry starts and report status changes with injected dependencies and sleep.
pub(crate) fn run_loop(
    make_deps: &mut dyn FnMut() -> Result<StartDeps, String>,
    sleep: &mut dyn FnMut(Duration),
    out: &mut dyn Write,
    max_attempts: Option<usize>,
) -> i32 {
    let mut previous = None;
    let mut attempts = 0;
    while max_attempts.is_none_or(|limit| attempts < limit) {
        attempts += 1;
        let deps = match make_deps() {
            Ok(deps) => deps,
            Err(message) => {
                log_line(
                    out,
                    &mut previous,
                    format!("{message} (retrying every minute)"),
                );
                sleep(Duration::from_secs(60));
                continue;
            }
        };
        let notes_dir = deps.config.notes_dir.clone();
        let (tx, rx) = mpsc::channel::<SyncStatus>();
        match start_engine(deps, tx) {
            StartOutcome::Started(handle) => {
                // Every new engine gets a start line, even if its folder is unchanged.
                previous = None;
                log_line(
                    out,
                    &mut previous,
                    format!("syncing {}", notes_dir.display()),
                );
                loop {
                    match rx.recv_timeout(Duration::from_secs(60)) {
                        Ok(status) => {
                            if let Some(line) = status_line(&status) {
                                log_line(out, &mut previous, line);
                            }
                            if status == SyncStatus::AuthExpired {
                                handle.stop();
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            log_line(out, &mut previous, "sync stopped — restarting".into());
                            handle.stop();
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
                sleep(Duration::from_secs(30));
            }
            StartOutcome::Locked => {
                log_line(
                    out,
                    &mut previous,
                    "the app is syncing these notes — waiting to take over when it closes".into(),
                );
                sleep(Duration::from_secs(30));
            }
            StartOutcome::Off(reason) => {
                let line = match reason {
                    OffReason::NotConnected => "not connected to Dropbox — run `nv-linux sync connect` (checking again every minute)",
                    OffReason::NoAppKey => "no Dropbox app key in config.toml — see \"Dropbox sync\" in the README (checking again every minute)",
                    OffReason::Disabled => "sync is off in config.toml ([sync] enabled = false) (checking again every minute)",
                    OffReason::InvalidConfig => "config.toml has a mistake, so sync is paused (checking again every minute)",
                };
                log_line(out, &mut previous, line.into());
                sleep(Duration::from_secs(60));
            }
            StartOutcome::Failed(message) => {
                log_line(
                    out,
                    &mut previous,
                    format!("{message} (retrying every minute)"),
                );
                sleep(Duration::from_secs(60));
            }
        }
    }
    0
}

fn log_line(out: &mut dyn Write, previous: &mut Option<String>, line: String) {
    if previous.as_ref() != Some(&line) {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
        *previous = Some(line);
    }
}

fn status_line(status: &SyncStatus) -> Option<String> {
    match status {
        SyncStatus::Off | SyncStatus::Starting | SyncStatus::Syncing | SyncStatus::Background => None,
        SyncStatus::Idle { .. } => Some("synced".into()),
        SyncStatus::Offline => Some("offline — will retry".into()),
        SyncStatus::AuthExpired => Some("the Dropbox sign-in has expired — reconnect with Ctrl+Shift+D in the app or `nv-linux sync connect`; the service picks the new sign-in up by itself".into()),
        SyncStatus::Attention(message) => Some(format!("needs attention: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nv_core::config::Config;
    use nv_sync::dropbox::Account;
    use nv_sync::fake::{FakeRemote, SharedFake};
    use nv_sync::remote::{Remote, RemoteResult};
    use nv_sync::secret::Secret;
    use nv_sync::tokens::{StoredLogin, TokenStore};

    use super::*;
    use crate::sync_cli::DropboxApi;

    struct MemoryStore;

    impl TokenStore for MemoryStore {
        fn load(&self) -> nv_sync::Result<Option<StoredLogin>> {
            Ok(Some(StoredLogin {
                refresh_token: Secret::new("test-refresh"),
                account_id: "dbid:test".into(),
                email: None,
            }))
        }

        fn save(&self, _: &StoredLogin) -> nv_sync::Result<()> {
            panic!("save called")
        }

        fn clear(&self) -> nv_sync::Result<()> {
            panic!("clear called")
        }

        fn describe(&self) -> &'static str {
            "memory"
        }
    }

    struct FakeDropbox;

    impl DropboxApi for FakeDropbox {
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
            Box::new(SharedFake::new(FakeRemote::new()))
        }

        fn revoke(&self, _: &str, _: &Secret) -> RemoteResult<()> {
            panic!("revoke called")
        }
    }

    fn deps(root: &Path, connected: bool) -> StartDeps {
        let mut config = Config {
            notes_dir: root.to_path_buf(),
            ..Config::default()
        };
        if connected {
            config.sync.dropbox_app_key = "test-app-key".into();
        }
        StartDeps {
            config,
            state_dir: root.to_path_buf(),
            store: Box::new(MemoryStore),
            dropbox: Box::new(FakeDropbox),
            notifier: Box::new(SharedFake::new(FakeRemote::new())),
            trasher: Box::new(nv_core::storage::FakeTrasher {
                dir: root.join("trash"),
            }),
            home_dir: Some(root.to_path_buf()),
        }
    }

    fn retry_log(mut make_deps: impl FnMut() -> Result<StartDeps, String>, seconds: u64) -> String {
        let mut sleeps = Vec::new();
        let mut out = Vec::new();
        assert_eq!(
            run_loop(
                &mut make_deps,
                &mut |duration| sleeps.push(duration),
                &mut out,
                Some(3),
            ),
            0
        );
        assert_eq!(sleeps, vec![Duration::from_secs(seconds); 3]);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn off_retries_and_deduplicates() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            retry_log(|| Ok(deps(root.path(), false)), 60),
            "no Dropbox app key in config.toml — see \"Dropbox sync\" in the README (checking again every minute)\n"
        );
    }

    #[test]
    fn dependency_error_retries_and_deduplicates() {
        assert_eq!(
            retry_log(|| Err("config.toml has a mistake: test".into()), 60),
            "config.toml has a mistake: test (retrying every minute)\n"
        );
    }

    #[test]
    fn locked_waits_to_take_over() {
        let root = tempfile::tempdir().unwrap();
        let _held = nv_sync::engine::try_lock(root.path()).unwrap().unwrap();
        assert_eq!(
            retry_log(|| Ok(deps(root.path(), true)), 30),
            "the app is syncing these notes — waiting to take over when it closes\n"
        );
    }

    #[test]
    fn every_status_has_a_log_mapping() {
        for status in [
            SyncStatus::Off,
            SyncStatus::Starting,
            SyncStatus::Syncing,
            SyncStatus::Background,
        ] {
            assert_eq!(status_line(&status), None);
        }
        for (status, expected) in [
            (
                SyncStatus::Idle {
                    last_ok: chrono::Utc::now(),
                },
                "synced",
            ),
            (SyncStatus::Offline, "offline — will retry"),
            (
                SyncStatus::AuthExpired,
                "the Dropbox sign-in has expired — reconnect with Ctrl+Shift+D in the app or `nv-linux sync connect`; the service picks the new sign-in up by itself",
            ),
            (SyncStatus::Attention("test".into()), "needs attention: test"),
        ] {
            assert_eq!(status_line(&status).as_deref(), Some(expected));
        }
    }
}
