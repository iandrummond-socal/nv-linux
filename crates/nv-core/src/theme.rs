use std::fs;
use std::path::Path;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub background: String,
    pub foreground: String,
    pub lighter_background: String,
    pub accent: String,
    pub selection: String,
    pub bright_foreground: String,
    pub dark_foreground: String,
    pub muted: String,
    pub dark_background: String,
    pub red: String,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: "#1a1b26".into(),
            foreground: "#c0caf5".into(),
            lighter_background: "#24283b".into(),
            accent: "#7aa2f7".into(),
            selection: "#283457".into(),
            bright_foreground: "#ffffff".into(),
            dark_foreground: "#a9b1d6".into(),
            muted: "#414868".into(),
            dark_background: "#16161e".into(),
            red: "#f7768e".into(),
        }
    }
}

fn is_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

impl Palette {
    fn colors_mut(&mut self) -> [(&'static str, &mut String); 10] {
        [
            ("background", &mut self.background),
            ("foreground", &mut self.foreground),
            ("lighter_background", &mut self.lighter_background),
            ("accent", &mut self.accent),
            ("selection", &mut self.selection),
            ("bright_foreground", &mut self.bright_foreground),
            ("dark_foreground", &mut self.dark_foreground),
            ("muted", &mut self.muted),
            ("dark_background", &mut self.dark_background),
            ("red", &mut self.red),
        ]
    }

    /// Load theme/colors.toml relative to the supplied theme root.
    ///
    /// Missing keys retain their built-in defaults. Invalid values are logged
    /// and ignored individually; unreadable files and invalid TOML return errors.
    pub fn load(theme_dir: impl AsRef<Path>) -> Result<Self> {
        let path = theme_dir.as_ref().join("theme").join("colors.toml");
        let source = fs::read_to_string(&path).map_err(|error| {
            log::warn!("Cannot read theme {}: {error}", path.display());
            Error::Theme(format!("Cannot read {}: {error}", path.display()))
        })?;
        let table = source.parse::<toml::Table>().map_err(|error| {
            log::warn!("Cannot parse theme {}: {error}", path.display());
            Error::Theme(format!("Cannot parse {}: {error}", path.display()))
        })?;
        let mut palette = Self::default();
        for (key, color) in palette.colors_mut() {
            if let Some(value) = table.get(key) {
                match value.as_str() {
                    Some(value) if is_color(value) => *color = value.to_owned(),
                    _ => log::warn!(
                        "Invalid theme color {key} in {}; using built-in default",
                        path.display()
                    ),
                }
            }
        }
        Ok(palette)
    }

    /// Generate GTK 4 CSS, validating even colors set through the public fields.
    pub fn to_css(&self) -> String {
        let mut palette = self.clone();
        let mut defaults = Self::default();
        for ((key, color), (_, fallback)) in
            palette.colors_mut().into_iter().zip(defaults.colors_mut())
        {
            if !is_color(color) {
                log::warn!("Invalid theme color {key}; using built-in default");
                color.clone_from(fallback);
            }
        }
        let Self {
            background,
            foreground,
            lighter_background,
            accent,
            selection,
            bright_foreground,
            dark_foreground,
            muted,
            dark_background,
            red,
        } = palette;
        format!(
            r#"window.nv, window.nv * {{
    font-family: monospace;
}}
window.nv {{
    background-color: {background};
    color: {foreground};
}}
entry.nv-search {{
    background-color: {lighter_background};
    color: {foreground};
    caret-color: {accent};
    border: 1px solid {muted};
}}
entry.nv-search:focus-within {{
    border-color: {accent};
}}
listview.nv-list {{
    background-color: {dark_background};
    color: {foreground};
}}
listview.nv-list > row:selected {{
    background-color: {selection};
    color: {bright_foreground};
}}
.nv-preview, .nv-date {{
    color: {dark_foreground};
}}
textview.nv-editor, textview.nv-editor > text {{
    background-color: {background};
    color: {foreground};
    caret-color: {accent};
}}
entry.nv-search selection,
textview.nv-editor selection {{
    background-color: {selection};
    color: {bright_foreground};
}}
.nv-banner {{
    background-color: {red};
    color: {background};
}}
paned.nv > separator {{
    background-color: {muted};
}}
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_THEME: &str = r##"
mode = "dark"
accent = "#ff8c00"
selection = "#3d2a0c"
muted = "#6b4a14"
background = "#17100a"
dark_background = "#110b06"
darker_background = "#0b0704"
lighter_background = "#241a0f"
foreground = "#ffb000"
dark_foreground = "#8a5c1c"
light_foreground = "#d69418"
bright_foreground = "#ffd486"
red = "#ff4d1a"
yellow = "#ffb000"
orange = "#e08a1e"
green = "#b8791c"
cyan = "#a88a3c"
blue = "#d19a35"
magenta = "#c86a28"
brown = "#5c3a10"
bright_red = "#ff7a4d"
bright_yellow = "#ffd25e"
bright_green = "#d99a2e"
bright_cyan = "#cbab5c"
bright_blue = "#f0b855"
bright_magenta = "#e8894a"
"##;

    fn load_source(source: &str) -> Result<Palette> {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("theme")).unwrap();
        fs::write(dir.path().join("theme/colors.toml"), source).unwrap();
        Palette::load(dir.path())
    }

    #[test]
    fn real_theme_loads_every_color_into_css() {
        let mut palette = load_source(REAL_THEME).unwrap();
        let table = REAL_THEME.parse::<toml::Table>().unwrap();
        let css = palette.to_css();
        for (key, color) in palette.colors_mut() {
            assert_eq!(color.as_str(), table[key].as_str().unwrap());
            assert!(css.contains(color.as_str()), "Missing color for {key}");
        }
        assert!(css.contains("font-family: monospace;"));
        assert!(css.contains("entry.nv-search:focus-within"));
        assert!(css.contains("listview.nv-list > row:selected"));
        assert!(css.contains("textview.nv-editor > text"));
        assert!(css.contains("entry.nv-search selection"));
        assert!(css.contains("textview.nv-editor selection"));
    }

    #[test]
    fn missing_keys_keep_defaults() {
        let palette = load_source("accent = '#Ab12Cd'\n").unwrap();
        let expected = Palette {
            accent: "#Ab12Cd".into(),
            ..Palette::default()
        };
        assert_eq!(palette, expected);
        assert_eq!(load_source("").unwrap(), Palette::default());
    }

    #[test]
    fn invalid_values_fall_back_individually() {
        for invalid in [
            "red",
            "#12345",
            "#zzzzzz",
            "\"; } * {",
            "#12345678",
            " #123456",
        ] {
            let source = format!("background = '{invalid}'\naccent = '#123ABC'\n");
            let palette = load_source(&source).unwrap();
            assert_eq!(palette.background, Palette::default().background);
            assert_eq!(palette.accent, "#123ABC");
            assert!(!palette.to_css().contains(invalid));
        }
        for value in ["42", "true", "[]", "{}"] {
            let palette = load_source(&format!("foreground = {value}\n")).unwrap();
            assert_eq!(palette, Palette::default());
        }
    }

    #[test]
    fn every_public_field_is_validated_before_interpolation() {
        let mut palette = Palette::default();
        for (_, color) in palette.colors_mut() {
            *color = "\"; } * {".into();
        }
        assert_eq!(palette.to_css(), Palette::default().to_css());
    }

    #[test]
    fn missing_file_is_a_theme_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(Palette::load(dir.path()), Err(Error::Theme(_))));
    }

    #[test]
    fn malformed_toml_is_a_theme_error() {
        assert!(matches!(load_source("accent = ["), Err(Error::Theme(_))));
    }

    #[test]
    fn unreadable_theme_content_is_a_theme_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("theme")).unwrap();
        fs::write(dir.path().join("theme/colors.toml"), [0xff]).unwrap();
        assert!(matches!(Palette::load(dir.path()), Err(Error::Theme(_))));
    }

    #[test]
    fn color_validation_requires_exact_ascii_hex() {
        for valid in ["#000000", "#ffffff", "#ABCDEF", "#aB12cD"] {
            assert!(is_color(valid));
        }
        for invalid in ["", "1234567", "#12345", "#1234567", "#é1234", "#12345\n"] {
            assert!(!is_color(invalid));
        }
    }
}
