#![allow(dead_code, unused_variables, clippy::new_without_default)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreEvent {
    Changed(PathBuf),
    Removed(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

pub struct FolderWatcher;

impl FolderWatcher {
    /// Start watching, forwarding events through a standard-library channel.
    pub fn start(dir: impl AsRef<Path>, sender: Sender<StoreEvent>) -> Result<Self> {
        todo!()
    }

    /// Register the SHA-256 digest of an expected write for event suppression.
    pub fn expect_write(&mut self, path: &Path, hash: [u8; 32]) {
        todo!()
    }
}
