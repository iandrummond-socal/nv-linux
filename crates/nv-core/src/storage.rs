#![allow(dead_code, unused_variables, clippy::new_without_default)]

use std::path::{Path, PathBuf};

use crate::model::{Note, NoteId};
use crate::Result;

pub trait Trasher {
    fn trash(&self, path: &Path) -> Result<()>;
}

pub struct SystemTrasher;

impl Trasher for SystemTrasher {
    fn trash(&self, path: &Path) -> Result<()> {
        todo!()
    }
}

pub struct FakeTrasher {
    pub dir: PathBuf,
}

impl Trasher for FakeTrasher {
    fn trash(&self, path: &Path) -> Result<()> {
        todo!()
    }
}

pub struct NoteStore;

impl NoteStore {
    /// Open a directory with a preferred creation extension and an owned trash backend.
    pub fn open(dir: impl AsRef<Path>, ext: &str, trasher: Box<dyn Trasher>) -> Result<Self> {
        todo!()
    }

    /// Create a note and return its stable session identifier.
    pub fn create(&mut self, title: &str, body: &str) -> Result<NoteId> {
        todo!()
    }

    pub fn write(&mut self, id: NoteId, body: &str) -> Result<()> {
        todo!()
    }

    pub fn rename(&mut self, id: NoteId, new_title: &str) -> Result<()> {
        todo!()
    }

    pub fn trash(&mut self, id: NoteId) -> Result<()> {
        todo!()
    }

    /// Remove a cached note without touching the filesystem.
    pub fn forget(&mut self, id: NoteId) -> Option<Note> {
        todo!()
    }

    /// Load or refresh a path, preserving the identifier of an existing note.
    pub fn reload(&mut self, path: &Path) -> Result<NoteId> {
        todo!()
    }

    pub fn get(&self, id: NoteId) -> Option<&Note> {
        todo!()
    }

    /// Borrow all cached notes; callers choose their display order.
    pub fn all(&self) -> Vec<&Note> {
        todo!()
    }
}
