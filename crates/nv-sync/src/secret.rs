//! Secret storage.

use std::fmt;

/// A sensitive value that must never be logged or serialized.
///
/// Use `expose()` only at the point of use, such as when constructing an
/// Authorization header or writing to a keyring. Formatting always redacts
/// the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn secret_debug_and_display_are_redacted() {
        let secret = Secret::new("abc123");

        for formatted in [format!("{secret:?}"), format!("{secret}")] {
            assert_eq!(formatted, "Secret(***)");
            assert!(!formatted.contains("abc123"));
        }
    }

    #[test]
    fn secret_is_redacted_in_pretty_debug() {
        #[derive(Debug)]
        struct Credentials {
            token: Secret,
        }

        let credentials = Credentials {
            token: Secret::new("abc123"),
        };
        let formatted = format!("{credentials:#?}");

        assert!(formatted.contains("Secret(***)"));
        assert!(!formatted.contains("abc123"));
        assert_eq!(credentials.token.expose(), "abc123");
    }

    #[test]
    fn secret_exposes_original_value() {
        assert_eq!(Secret::new("abc123").expose(), "abc123");
        assert_eq!(Secret::from(String::from("abc123")).expose(), "abc123");
        assert_eq!(Secret::from("abc123").expose(), "abc123");
    }
}
