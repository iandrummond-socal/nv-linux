#![allow(dead_code, unused_variables, clippy::new_without_default)]

use std::path::Path;

use crate::Result;

#[derive(Debug, Clone)]
pub struct Palette {
    pub background: String,
    pub foreground: String,
    pub lighter_background: String,
    pub accent: String,
    pub selection: String,
    pub bright_foreground: String,
    pub dark_foreground: String,
    pub muted: String,
}

impl Default for Palette {
    fn default() -> Self {
        todo!()
    }
}

impl Palette {
    /// Load theme/colors.toml relative to the supplied theme root.
    pub fn load(theme_dir: impl AsRef<Path>) -> Result<Self> {
        todo!()
    }

    pub fn to_css(&self) -> String {
        todo!()
    }
}
