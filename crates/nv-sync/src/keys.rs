//! Canonical keys and syncable local note names.

use std::path::Path;

use unicode_normalization::UnicodeNormalization;

use crate::plan::SkipReason;

/// Compute Dropbox's `path_lower` for a filename at the app-folder root.
///
/// The live spike confirmed a leading `/`, lowercase names, and Unicode NFC
/// normalization: NFD `e` + U+0301 and NFC `é` share a key. Linux filenames may
/// use either normalization and any case.
pub fn key_for_name(name: &str) -> String {
    format!("/{}", name.nfc().collect::<String>().to_lowercase())
}

/// Check the note-path rules and Dropbox's observed filename restriction.
/// The live spike rejected only backslash as a filename character.
pub fn syncable_name(name: &str) -> Result<(), SkipReason> {
    if name.contains('\\') {
        Err(SkipReason::BadName)
    } else if !nv_core::watcher::is_note_path(Path::new(name)) {
        Err(SkipReason::NotANote)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeRemote;

    #[test]
    fn ascii_lowercase_key() {
        assert_eq!(key_for_name("Todo.txt"), "/todo.txt");
    }

    #[test]
    fn nfd_and_nfc_share_key() {
        assert_eq!(key_for_name("e\u{301}.txt"), key_for_name("é.txt"));
        assert_eq!(key_for_name("e\u{301}.txt"), "/é.txt");
    }

    #[test]
    fn emoji_and_umlauts_match_dropbox() {
        assert_eq!(key_for_name("Café 🎉 ünïcödé.txt"), "/café 🎉 ünïcödé.txt");
        assert_eq!(key_for_name("CAFÉ 🎉 ÜNÏCÖDÉ.TXT"), "/café 🎉 ünïcödé.txt");
    }

    #[test]
    fn fake_agrees_for_ascii_names() {
        for name in ["Todo.txt", "NOTES.MD", "a b.taskpaper", "Mixed_CASE.text"] {
            assert_eq!(key_for_name(name), FakeRemote::key_for(name));
        }
    }

    #[test]
    fn syncable_note_names() {
        for name in [
            "Todo.TXT",
            "é.md",
            "e\u{301}.txt",
            "🎉.utf8",
            "a:*?\"<>|.txt",
        ] {
            assert_eq!(syncable_name(name), Ok(()), "{name}");
        }
        for name in [".hidden.txt", "4913", "a.txt~", "a.jpg", "a", ""] {
            assert_eq!(syncable_name(name), Err(SkipReason::NotANote), "{name}");
        }
        for name in ["bad\\name.txt", "bad\\name.jpg"] {
            assert_eq!(syncable_name(name), Err(SkipReason::BadName), "{name}");
        }
    }
}
