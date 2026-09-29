use std::io::{self, Write};
use std::process::{Command, ExitCode, Stdio};

use nv_core::config::Config;
use nv_sync::dropbox::oauth::{agent, authorize_url, exchange_code, Pkce};
use nv_sync::dropbox::{DropboxRemote, Endpoints};
use nv_sync::secret::Secret;
use nv_sync::state::state_dir_with;
use nv_sync::tokens::{default_store, StoredLogin, TokenStore};

fn connect() -> Result<(), Box<dyn std::error::Error>> {
    let app_key = Config::load()?.sync.dropbox_app_key;
    if app_key.trim().is_empty() {
        return Err("Set dropbox_app_key under [sync] in ~/.config/nv-linux/config.toml".into());
    }
    let state_dir = state_dir_with(&|key| std::env::var(key).ok())
        .ok_or("Cannot determine the nv-linux state directory")?;
    let pkce = Pkce::generate()?;
    let endpoints = Endpoints::dropbox();
    let url = authorize_url(&endpoints, &app_key, &pkce.challenge);
    println!("Open this link, allow access, then paste the code Dropbox shows you:");
    println!("{url}");
    io::stdout().flush()?;
    if !std::env::args_os().any(|arg| arg == "--no-open") {
        let _ = Command::new("xdg-open")
            .arg(&url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let code = Secret::new(line);
    let tokens = exchange_code(&agent(), &endpoints, &app_key, &code, &pkce.verifier)?;
    let refresh_token = tokens
        .refresh_token
        .ok_or("Dropbox did not return a refresh token")?;
    let account = DropboxRemote::new(app_key, refresh_token.clone()).current_account()?;
    let login = StoredLogin {
        refresh_token,
        account_id: account.account_id,
        email: account.email,
    };
    let store = default_store(&state_dir);
    store.save(&login)?;
    println!(
        "Signed in as {}",
        login.email.as_deref().unwrap_or(&login.account_id)
    );
    println!("Login stored in {}", store.describe());
    println!("Files live in Dropbox/Apps/<your app name>/");
    Ok(())
}

fn main() -> ExitCode {
    match connect() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
