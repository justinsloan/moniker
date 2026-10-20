//! Helpers shared by the `moniker-server` and `moniker-askpass` binaries.
//!
//! This library target exists only so the two binaries do not duplicate code.
//! It is not a public API. Like the binaries, it must never depend on GTK or VTE.

use std::path::PathBuf;

use moniker_core::store::ProfileStore;

/// Where the profile file lives (`ProfileStore::default_path`), or a clear
/// error if the config directory cannot be determined.
pub fn store_path() -> Result<PathBuf, String> {
    checked_store_path(ProfileStore::default_path())
}

/// `default_path` falls back to a relative `.config/...` when neither
/// `XDG_CONFIG_HOME` nor `HOME` is usable. Using that would read or write
/// whatever directory the process happens to be in, so refuse it.
fn checked_store_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("cannot determine the config directory (set HOME or XDG_CONFIG_HOME)".to_owned())
    }
}

/// `text` with every control character replaced by `?`, so that it can be
/// printed without acting on the terminal.
///
/// Profile names, hosts and the like come from the profile file, which another
/// machine can write through sync, and must not be able to send escape
/// sequences to the user's terminal. This covers the C0 controls (including
/// ESC, newline and tab), DEL and the C1 controls (U+0080..=U+009F, whose
/// U+009B is a one-character CSI).
pub fn sanitize_for_terminal(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(sanitize_for_terminal("web1 prod-db_2"), "web1 prod-db_2");
        assert_eq!(sanitize_for_terminal(""), "");
    }

    #[test]
    fn non_ascii_text_is_unchanged() {
        assert_eq!(sanitize_for_terminal("café 日本語 🚀"), "café 日本語 🚀");
    }

    #[test]
    fn escape_sequences_are_defanged() {
        assert_eq!(
            sanitize_for_terminal("a\x1b[31mred\x1b[0m"),
            "a?[31mred?[0m"
        );
    }

    #[test]
    fn line_and_tab_controls_are_replaced() {
        assert_eq!(sanitize_for_terminal("a\nb\rc\td\0e"), "a?b?c?d?e");
    }

    #[test]
    fn del_and_c1_controls_are_replaced() {
        // U+009B is the one-character form of CSI: many terminals act on it.
        assert_eq!(sanitize_for_terminal("a\x7fb\u{9b}c\u{85}d"), "a?b?c?d");
    }

    #[test]
    fn store_path_accepts_an_absolute_path() {
        let path = PathBuf::from("/home/me/.config/moniker/profiles.json");
        assert_eq!(checked_store_path(path.clone()), Ok(path));
    }

    #[test]
    fn store_path_rejects_a_relative_path() {
        // What `ProfileStore::default_path` returns when neither
        // XDG_CONFIG_HOME nor HOME is usable.
        let err = checked_store_path(PathBuf::from(".config/moniker/profiles.json")).unwrap_err();
        assert!(err.contains("config directory"), "{err}");
        assert!(err.contains("HOME"), "{err}");
        assert!(err.contains("XDG_CONFIG_HOME"), "{err}");
    }

    #[test]
    fn store_path_from_the_environment_is_absolute_or_an_error() {
        // Reads the real environment but changes nothing.
        if let Ok(path) = store_path() {
            assert!(path.is_absolute());
        }
    }
}
