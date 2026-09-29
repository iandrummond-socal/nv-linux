mod common;

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use nv_core::config::Config;
use nv_sync::dropbox::{DropboxNotifier, DropboxRemote};
use nv_sync::remote::Remote;
use nv_sync::secret::Secret;
use nv_sync::state::state_dir_with;
use nv_sync::tokens::{default_store, TokenStore};

static LIVE_LOCK: Mutex<()> = Mutex::new(());
static PREFIX: OnceLock<String> = OnceLock::new();

struct Live {
    _lock: MutexGuard<'static, ()>,
    app_key: String,
    refresh: Secret,
    remote: DropboxRemote,
    prefix: &'static str,
}

impl Live {
    fn load() -> Option<Self> {
        if std::env::var("NV_DROPBOX_LIVE").as_deref() != Ok("1") {
            eprintln!("Skipping: set NV_DROPBOX_LIVE=1 to use a connected Dropbox account");
            return None;
        }
        let lock = LIVE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state_dir = state_dir_with(&|key| std::env::var(key).ok())
            .expect("Cannot determine the nv-linux state directory");
        let login = default_store(&state_dir)
            .load()
            .unwrap_or_else(|error| panic!("{error}"))
            .expect("Run `cargo run -p nv-sync --example connect` first to connect an account");
        let app_key = Config::load()
            .unwrap_or_else(|error| panic!("{error}"))
            .sync
            .dropbox_app_key;
        assert!(
            !app_key.trim().is_empty(),
            "Set [sync] dropbox_app_key in ~/.config/nv-linux/config.toml"
        );
        let prefix = PREFIX.get_or_init(|| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            // Keep the requested seconds prefix, with extra entropy for runs
            // started within the same second (including separate processes).
            format!(
                "nvlive-{}-{}-{}-",
                now.as_secs(),
                now.subsec_nanos(),
                std::process::id()
            )
        });
        Some(Self {
            _lock: lock,
            remote: DropboxRemote::new(app_key.clone(), login.refresh_token.clone()),
            app_key,
            refresh: login.refresh_token,
            prefix,
        })
    }
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn connect_round_trip() {
    let Some(mut live) = Live::load() else {
        return;
    };
    let account = live
        .remote
        .current_account()
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(!account.account_id.is_empty());
    eprintln!(
        "Connected as {}",
        account.email.as_deref().unwrap_or(&account.account_id)
    );
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn upload_new_then_list_and_download() {
    let Some(mut live) = Live::load() else {
        return;
    };
    common::upload_new_then_list_and_download(&mut live.remote, live.prefix);
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn stale_rev_conflicts() {
    let Some(mut live) = Live::load() else {
        return;
    };
    common::stale_rev_conflicts(&mut live.remote, live.prefix);
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn case_insensitive_same_file() {
    let Some(mut live) = Live::load() else {
        return;
    };
    common::case_insensitive_same_file(&mut live.remote, live.prefix);
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn non_ascii_names() {
    let Some(mut live) = Live::load() else {
        return;
    };
    common::non_ascii_names(&mut live.remote, live.prefix);
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn cursor_deltas() {
    let Some(mut live) = Live::load() else {
        return;
    };
    common::cursor_deltas(&mut live.remote, live.prefix, true);
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn longpoll_sees_change() {
    let Some(mut live) = Live::load() else {
        return;
    };
    let app_key = live.app_key.clone();
    let refresh = live.refresh.clone();
    common::longpoll_sees_change(
        &mut live.remote,
        &mut DropboxNotifier::new(),
        live.prefix,
        || Box::new(DropboxRemote::new(app_key.clone(), refresh.clone())),
    );
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn rejected_characters() {
    let Some(mut live) = Live::load() else {
        return;
    };
    common::rejected_characters(&mut live.remote, live.prefix);
}

#[test]
#[ignore = "needs NV_DROPBOX_LIVE=1 and a connected account"]
fn leaves_no_litter() {
    let Some(mut live) = Live::load() else {
        return;
    };
    // Test order is unspecified. Exercise cleanup here as well as sweeping this
    // run's prefix; never delete another concurrent run's nvlive files.
    common::upload_new_then_list_and_download(&mut live.remote, live.prefix);
    let (files, _) = live.remote.list().unwrap();
    let litter: Vec<_> = files
        .into_iter()
        .filter(|file| file.name.starts_with(live.prefix))
        .collect();
    let count = litter.len();
    for file in litter {
        live.remote
            .delete(&file.id, &file.rev)
            .unwrap_or_else(|error| panic!("cleanup: {error}"));
    }
    eprintln!("Cleanup sweep removed {count} files for {}", live.prefix);
    assert!(live
        .remote
        .list()
        .unwrap()
        .0
        .iter()
        .all(|file| !file.name.starts_with(live.prefix)));
}
