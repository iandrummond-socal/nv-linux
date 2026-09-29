//! Authentication token storage with a private file fallback.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::secret::Secret;

const LOGIN_FILE: &str = "dropbox-login.json";
const UNREADABLE: &str = "stored Dropbox login is unreadable; run `nv-linux sync connect` again";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct StoredLogin {
    pub refresh_token: Secret,
    pub account_id: String,
    pub email: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RawLogin {
    refresh_token: String,
    account_id: String,
    email: Option<String>,
}

impl From<&StoredLogin> for RawLogin {
    fn from(login: &StoredLogin) -> Self {
        Self {
            refresh_token: login.refresh_token.expose().to_owned(),
            account_id: login.account_id.clone(),
            email: login.email.clone(),
        }
    }
}

impl From<RawLogin> for StoredLogin {
    fn from(login: RawLogin) -> Self {
        Self {
            refresh_token: Secret::new(login.refresh_token),
            account_id: login.account_id,
            email: login.email,
        }
    }
}

pub trait TokenStore: Send {
    fn load(&self) -> Result<Option<StoredLogin>>;
    fn save(&self, login: &StoredLogin) -> Result<()>;
    fn clear(&self) -> Result<()>;
    fn describe(&self) -> &'static str;
}

pub struct KeyringStore;

// Do not format backend errors: their payloads may contain credential values.
fn keyring_error(error: keyring::Error) -> Error {
    let reason = match error {
        keyring::Error::NoEntry => "credential not found",
        keyring::Error::NoStorageAccess(_) => "storage access denied",
        keyring::Error::PlatformFailure(_) => "platform failure",
        _ => "credential operation failed",
    };
    Error::Auth(format!("keyring unavailable: {reason}"))
}

impl KeyringStore {
    fn entry() -> keyring::Result<keyring::Entry> {
        keyring::Entry::new("nv-linux", "dropbox")
    }

    pub fn available() -> bool {
        if !matches!(
            keyring::default::default_credential_builder().persistence(),
            keyring::credential::CredentialPersistence::UntilDelete
        ) {
            return false;
        }
        matches!(
            Self::entry().and_then(|entry| entry.get_password()),
            Ok(_) | Err(keyring::Error::NoEntry)
        )
    }
}

impl TokenStore for KeyringStore {
    fn load(&self) -> Result<Option<StoredLogin>> {
        match Self::entry().and_then(|entry| entry.get_password()) {
            Ok(value) => serde_json::from_str::<RawLogin>(&value)
                .map(|login| Some(login.into()))
                .map_err(|_| Error::Auth("keyring unavailable: stored login is unreadable".into())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_error(error)),
        }
    }

    fn save(&self, login: &StoredLogin) -> Result<()> {
        let value = serde_json::to_string(&RawLogin::from(login))?;
        Self::entry()
            .and_then(|entry| entry.set_password(&value))
            .map_err(keyring_error)
    }

    fn clear(&self) -> Result<()> {
        match Self::entry().and_then(|entry| entry.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keyring_error(error)),
        }
    }

    fn describe(&self) -> &'static str {
        "keyring"
    }
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self {
            path: state_dir.as_ref().join(LOGIN_FILE),
        }
    }
}

impl TokenStore for FileStore {
    fn load(&self) -> Result<Option<StoredLogin>> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<RawLogin>(&bytes)
                .map(|login| Some(login.into()))
                .map_err(|_| Error::Auth(UNREADABLE.into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, login: &StoredLogin) -> Result<()> {
        let state_dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(state_dir)?;
        let (temp_path, mut temp) = loop {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = state_dir.join(format!(
                "{LOGIN_FILE}.tmp-{}-{sequence}",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let result = (|| -> Result<()> {
            // Restore owner read/write if a restrictive umask removed either bit.
            temp.set_permissions(fs::Permissions::from_mode(0o600))?;
            temp.write_all(&serde_json::to_vec(&RawLogin::from(login))?)?;
            temp.sync_all()?;
            fs::rename(&temp_path, &self.path)?;
            File::open(state_dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn describe(&self) -> &'static str {
        "file"
    }
}

pub struct DefaultStore {
    keyring: Option<Box<dyn TokenStore>>,
    file: FileStore,
}

impl TokenStore for DefaultStore {
    fn load(&self) -> Result<Option<StoredLogin>> {
        let primary = self.keyring.as_ref().map_or(Ok(None), |store| store.load());
        let fallback = self.file.load();
        match (primary, fallback) {
            (Ok(Some(login)), _) | (_, Ok(Some(login))) => Ok(Some(login)),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(None), Ok(None)) => Ok(None),
        }
    }

    fn save(&self, login: &StoredLogin) -> Result<()> {
        if let Some(primary) = &self.keyring {
            if primary.save(login).is_ok() {
                return self.file.clear();
            }
            log::warn!("Dropbox keyring save failed; storing login in file");
            self.file.save(login)?;
            if primary.clear().is_err() {
                log::warn!("Dropbox keyring cleanup failed; login stored in file");
            }
            return Ok(());
        }
        self.file.save(login)
    }

    fn clear(&self) -> Result<()> {
        let primary = self.keyring.as_ref().map_or(Ok(()), |store| store.clear());
        let fallback = self.file.clear();
        primary.and(fallback)
    }

    fn describe(&self) -> &'static str {
        if self.keyring.is_some() {
            "keyring"
        } else {
            "file"
        }
    }
}

pub fn default_store(state_dir: &Path) -> DefaultStore {
    let store = DefaultStore {
        keyring: KeyringStore::available().then(|| Box::new(KeyringStore) as Box<dyn TokenStore>),
        file: FileStore::new(state_dir),
    };
    log::info!("Dropbox login stored in {}", store.describe());
    store
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn login(token: &str) -> StoredLogin {
        StoredLogin {
            refresh_token: Secret::new(token),
            account_id: "account".into(),
            email: Some("user@example.com".into()),
        }
    }

    #[test]
    fn file_round_trip_is_private_and_leaves_no_temp() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join("nested/state");
        let store = FileStore::new(&state_dir);
        assert!(store.load().unwrap().is_none());
        store.save(&login("first")).unwrap();
        store.save(&login("refresh-secret")).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.refresh_token.expose(), "refresh-secret");
        assert_eq!(loaded.account_id, "account");
        assert_eq!(loaded.email.as_deref(), Some("user@example.com"));
        assert_eq!(
            fs::metadata(&store.path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        let entries: Vec<_> = fs::read_dir(state_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(LOGIN_FILE)]);
    }

    #[test]
    fn corrupt_file_is_preserved_without_exposing_contents() {
        let dir = tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let bytes = br#"{"refresh_token":"refresh-secret","account_id":false}"#;
        fs::write(&store.path, bytes).unwrap();
        match store.load().unwrap_err() {
            Error::Auth(message) => assert_eq!(message, UNREADABLE),
            error => panic!("unexpected error: {error}"),
        }
        assert_eq!(fs::read(&store.path).unwrap(), bytes);
    }

    #[test]
    fn file_clear_removes_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = FileStore::new(dir.path());
        store.save(&login("secret")).unwrap();
        store.clear().unwrap();
        store.clear().unwrap();
        assert!(!store.path.exists());
    }

    struct FakePrimary {
        value: Mutex<Option<RawLogin>>,
        fail: bool,
        fail_load_and_clear: bool,
    }

    impl TokenStore for FakePrimary {
        fn load(&self) -> Result<Option<StoredLogin>> {
            if self.fail_load_and_clear {
                return Err(Error::Auth("unavailable".into()));
            }
            Ok(self.value.lock().unwrap().as_ref().map(|raw| StoredLogin {
                refresh_token: Secret::new(raw.refresh_token.clone()),
                account_id: raw.account_id.clone(),
                email: raw.email.clone(),
            }))
        }

        fn save(&self, login: &StoredLogin) -> Result<()> {
            if self.fail {
                return Err(Error::Auth("unavailable".into()));
            }
            *self.value.lock().unwrap() = Some(RawLogin::from(login));
            Ok(())
        }

        fn clear(&self) -> Result<()> {
            if self.fail_load_and_clear {
                return Err(Error::Auth("unavailable".into()));
            }
            *self.value.lock().unwrap() = None;
            Ok(())
        }

        fn describe(&self) -> &'static str {
            "keyring"
        }
    }

    fn injected(dir: &Path, fail: bool) -> DefaultStore {
        DefaultStore {
            keyring: Some(Box::new(FakePrimary {
                value: Mutex::new(None),
                fail,
                fail_load_and_clear: false,
            })),
            file: FileStore::new(dir),
        }
    }

    #[test]
    fn failed_primary_save_falls_back_and_clears_stale_primary() {
        let dir = tempdir().unwrap();
        let store = DefaultStore {
            keyring: Some(Box::new(FakePrimary {
                value: Mutex::new(Some(RawLogin::from(&login("old")))),
                fail: true,
                fail_load_and_clear: false,
            })),
            file: FileStore::new(dir.path()),
        };
        store.save(&login("new")).unwrap();
        assert!(store.keyring.as_ref().unwrap().load().unwrap().is_none());
        assert_eq!(
            store.file.load().unwrap().unwrap().refresh_token.expose(),
            "new"
        );
        assert_eq!(store.load().unwrap().unwrap().refresh_token.expose(), "new");
    }

    #[test]
    fn unavailable_primary_save_and_clear_preserve_file_fallback() {
        let dir = tempdir().unwrap();
        let store = DefaultStore {
            keyring: Some(Box::new(FakePrimary {
                value: Mutex::new(Some(RawLogin::from(&login("old")))),
                fail: true,
                fail_load_and_clear: true,
            })),
            file: FileStore::new(dir.path()),
        };
        store.file.save(&login("old-file")).unwrap();
        store.save(&login("new")).unwrap();
        assert!(store.keyring.as_ref().unwrap().load().is_err());
        for loaded in [store.file.load(), store.load()] {
            let loaded = loaded.unwrap().unwrap();
            assert_eq!(loaded.refresh_token.expose(), "new");
            assert_eq!(loaded.account_id, "account");
            assert_eq!(loaded.email.as_deref(), Some("user@example.com"));
        }
    }

    #[test]
    fn working_primary_save_clears_file() {
        let dir = tempdir().unwrap();
        let store = injected(dir.path(), false);
        store.file.save(&login("old")).unwrap();
        store.save(&login("new")).unwrap();
        assert!(!store.file.path.exists());
        assert_eq!(
            store
                .keyring
                .as_ref()
                .unwrap()
                .load()
                .unwrap()
                .unwrap()
                .refresh_token
                .expose(),
            "new"
        );
    }

    #[test]
    fn load_finds_file_when_primary_empty_and_prefers_primary_when_populated() {
        let dir = tempdir().unwrap();
        let store = injected(dir.path(), false);
        store.file.save(&login("fallback")).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().refresh_token.expose(),
            "fallback"
        );
        store
            .keyring
            .as_ref()
            .unwrap()
            .save(&login("primary"))
            .unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().refresh_token.expose(),
            "primary"
        );
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn debug_redacts_refresh_token() {
        let login = login("refresh-secret");
        assert!(!format!("{login:?}").contains("refresh-secret"));
        assert!(!format!("{login:#?}").contains("refresh-secret"));
    }

    #[test]
    fn default_keyring_builder_is_persistent() {
        assert!(matches!(
            keyring::default::default_credential_builder().persistence(),
            keyring::credential::CredentialPersistence::UntilDelete
        ));
    }
}
