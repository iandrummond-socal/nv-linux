use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Modified,
    Title,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub notes_dir: PathBuf,
    pub extension: String,
    pub sort: SortOrder,
    pub autosave_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notes_dir: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Notes"),
            extension: "txt".to_owned(),
            sort: SortOrder::Modified,
            autosave_ms: 500,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_dir = match real_env("NV_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir()
                .ok_or_else(|| {
                    Error::Config("Cannot determine configuration directory".to_owned())
                })?
                .join("nv-linux"),
        };
        Self::load_from(config_dir)
    }

    /// Load configuration from the supplied configuration directory.
    pub fn load_from(config_dir: impl AsRef<Path>) -> Result<Self> {
        Self::load_from_env(config_dir, &real_env)
    }

    /// Load with an injected environment lookup, consulting only NV_NOTES_DIR.
    /// Environment overrides are applied after loading and are never persisted.
    pub fn load_from_env(
        config_dir: impl AsRef<Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let config_dir = config_dir.as_ref();
        let path = config_dir.join("config.toml");
        let mut config = match fs::read(&path) {
            Ok(bytes) => parse_config(&path, &bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let defaults = Self::default();
                let contents = toml::to_string_pretty(&defaults)
                    .map_err(|error| Error::Config(error.to_string()))?;
                fs::create_dir_all(config_dir)?;
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(mut file) => {
                        file.write_all(contents.as_bytes())?;
                        defaults
                    }
                    // Another loader may have created the file since our read.
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                        parse_config(&path, &fs::read(&path)?)
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(dir) = env("NV_NOTES_DIR") {
            config.notes_dir = PathBuf::from(dir);
        }
        Ok(config)
    }

    /// Resolve the theme root using NV_THEME_DIR or the Omarchy state directory.
    pub fn theme_dir(&self) -> PathBuf {
        Self::theme_dir_with(&real_env)
    }

    /// Resolve theme paths without changing the process environment.
    pub fn theme_dir_with(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
        if let Some(dir) = env("NV_THEME_DIR") {
            return PathBuf::from(dir);
        }
        let state_dir = env("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state")
        });
        state_dir.join("omarchy/current")
    }

    /// Whether the app should use its Devel application ID and NON_UNIQUE flag.
    pub fn is_dev() -> bool {
        Self::is_dev_with(&real_env)
    }

    pub fn is_dev_with(env: &dyn Fn(&str) -> Option<String>) -> bool {
        env("NV_DEV").as_deref() == Some("1")
    }
}

fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn parse_config(path: &Path, bytes: &[u8]) -> Config {
    let parsed = std::str::from_utf8(bytes)
        .map_err(|error| error.to_string())
        .and_then(|contents| toml::from_str(contents).map_err(|error| error.to_string()));
    match parsed {
        Ok(config) => config,
        Err(error) => {
            log::warn!(
                "Invalid configuration {}: {error}; using defaults without overwriting the file",
                path.display()
            );
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn assert_defaults(config: &Config) {
        assert_eq!(config.notes_dir, Config::default().notes_dir);
        assert_eq!(config.extension, "txt");
        assert_eq!(config.sort, SortOrder::Modified);
        assert_eq!(config.autosave_ms, 500);
    }

    #[test]
    fn missing_file_is_written_once_and_subsequent_load_reads_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("nested/config");
        assert_defaults(&Config::load_from_env(&dir, &no_env).unwrap());
        let path = dir.join("config.toml");
        let written = fs::read_to_string(&path).unwrap();
        assert_defaults(&toml::from_str::<Config>(&written).unwrap());
        assert!(written.contains("sort = \"modified\""));
        let edited = "# preserve this comment\nsort = 'title'\n";
        fs::write(&path, edited).unwrap();
        let loaded = Config::load_from_env(&dir, &no_env).unwrap();
        assert_eq!(loaded.sort, SortOrder::Title);
        assert_eq!(fs::read_to_string(path).unwrap(), edited);
    }

    #[test]
    fn empty_file_uses_defaults_without_rewriting() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "").unwrap();
        assert_defaults(&Config::load_from_env(temp.path(), &no_env).unwrap());
        assert!(fs::read(path).unwrap().is_empty());
    }

    #[test]
    fn valid_file_loads_all_fields() {
        let temp = tempfile::tempdir().unwrap();
        let expected = Config {
            notes_dir: temp.path().join("notes"),
            extension: "md".to_owned(),
            sort: SortOrder::Title,
            autosave_ms: 1250,
        };
        let contents = toml::to_string(&expected).unwrap();
        assert!(contents.contains("\"title\""));
        fs::write(temp.path().join("config.toml"), contents).unwrap();
        let loaded = Config::load_from_env(temp.path(), &no_env).unwrap();
        assert_eq!(loaded.notes_dir, expected.notes_dir);
        assert_eq!(loaded.extension, "md");
        assert_eq!(loaded.sort, SortOrder::Title);
        assert_eq!(loaded.autosave_ms, 1250);
    }

    #[test]
    fn partial_file_defaults_missing_fields() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("config.toml"), "autosave_ms = 800\n").unwrap();
        let loaded = Config::load_from_env(temp.path(), &no_env).unwrap();
        assert_eq!(loaded.notes_dir, Config::default().notes_dir);
        assert_eq!(loaded.extension, "txt");
        assert_eq!(loaded.sort, SortOrder::Modified);
        assert_eq!(loaded.autosave_ms, 800);
    }

    #[test]
    fn malformed_files_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        for contents in [
            &b"not valid = ["[..],
            &b"sort = 'unknown'"[..],
            &b"autosave_ms = 'slow'"[..],
            &b"\xff\xfe"[..],
        ] {
            fs::write(&path, contents).unwrap();
            assert_defaults(&Config::load_from_env(temp.path(), &no_env).unwrap());
            assert_eq!(fs::read(&path).unwrap(), contents);
        }
    }

    #[test]
    fn notes_override_is_not_persisted_and_wins_over_file() {
        let temp = tempfile::tempdir().unwrap();
        let notes = temp.path().join("override-notes");
        let env = |key: &str| {
            assert_eq!(key, "NV_NOTES_DIR");
            Some(notes.to_str().unwrap().to_owned())
        };
        for _ in 0..2 {
            let loaded = Config::load_from_env(temp.path(), &env).unwrap();
            assert_eq!(loaded.notes_dir, notes);
            assert_defaults(&Config::load_from_env(temp.path(), &no_env).unwrap());
        }
        assert!(!notes.exists());
    }

    #[test]
    fn theme_override_takes_precedence_over_state_home() {
        let temp = tempfile::tempdir().unwrap();
        let theme = temp.path().join("theme");
        let env = |key: &str| match key {
            "NV_THEME_DIR" => Some(theme.to_str().unwrap().to_owned()),
            "XDG_STATE_HOME" => panic!("theme override should take precedence"),
            _ => None,
        };
        assert_eq!(Config::theme_dir_with(&env), theme);
        assert!(!theme.exists());
    }

    #[test]
    fn theme_uses_xdg_state_home() {
        let temp = tempfile::tempdir().unwrap();
        let env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(temp.path().to_str().unwrap().to_owned()),
            _ => None,
        };
        assert_eq!(
            Config::theme_dir_with(&env),
            temp.path().join("omarchy/current")
        );
    }

    #[test]
    fn theme_falls_back_to_home_state_path() {
        let expected = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local/state/omarchy/current");
        assert_eq!(Config::theme_dir_with(&no_env), expected);
    }

    #[test]
    fn dev_mode_requires_exactly_one() {
        assert!(!Config::is_dev_with(&no_env));
        for value in ["", "0", "true", "01", "1"] {
            let env = |key: &str| {
                assert_eq!(key, "NV_DEV");
                Some(value.to_owned())
            };
            assert_eq!(Config::is_dev_with(&env), value == "1");
        }
    }
}
