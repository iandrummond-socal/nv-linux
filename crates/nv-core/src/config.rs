#![allow(dead_code, unused_variables, clippy::new_without_default)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortOrder {
    Modified,
    Title,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub notes_dir: PathBuf,
    pub extension: String,
    pub sort: SortOrder,
    pub autosave_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        todo!()
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        todo!()
    }

    /// Load configuration from the supplied configuration directory.
    pub fn load_from(config_dir: impl AsRef<Path>) -> Result<Self> {
        todo!()
    }

    /// Resolve the theme root using NV_THEME_DIR or the Omarchy state directory.
    pub fn theme_dir(&self) -> PathBuf {
        todo!()
    }
}
