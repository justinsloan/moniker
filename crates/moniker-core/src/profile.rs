//! A saved SSH connection profile and its validation.
//!
//! A [`Profile`] holds everything about a connection except secrets: the
//! password lives in the system keyring, keyed by the profile id.
//!
//! Profiles also arrive from other machines (sync bundles), so validation is
//! strict about anything that ends up on an `ssh` command line. The host and
//! username are restricted to a small charset and may not start with `-`, which
//! stops them being parsed by `ssh` as options (for example
//! `-oProxyCommand=...`).

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default SSH port.
const DEFAULT_PORT: u16 = 22;

/// A named list of commands run after login.
///
/// Commands may be blank (later code skips them) but may not span lines.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct StartupSequence {
    pub name: String,
    pub commands: Vec<String>,
}

/// A saved SSH connection. Holds no password.
///
/// `Debug` prints every field, startup commands included, and those can hold
/// secrets: do not log a profile.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Profile {
    /// Lowercase hyphenated UUID string (v4 when created here). Stable for the life of
    /// the profile, including its tombstone. Enforced by `validate`.
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub key_file: Option<PathBuf>,
    pub sequences: Vec<StartupSequence>,
    /// Whether sessions are recorded to a transcript by default.
    pub transcript: bool,
    /// Kept on this machine only: sync never sends it, password included. Left
    /// out of the JSON when false, so a bundle or a version-1 profile file stays
    /// byte for byte what it was; a profile file that holds one with it set is
    /// written as format version 2 (see `store`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_only: bool,
    /// Last edit time, Unix milliseconds UTC. Newest edit wins when merging.
    pub updated_at: i64,
    /// Tombstone marker so deletions propagate through sync.
    pub deleted: bool,
}

/// Why a [`Profile`] failed [`Profile::validate`].
///
/// The variants carry no data: the offending value may be a startup command,
/// which can contain secrets, so it is never echoed into an error message.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    #[error("profile id must be a lowercase hyphenated UUID")]
    Id,
    #[error("profile name must not be empty or contain control characters")]
    Name,
    #[error(
        "host must be non-empty, use only letters, digits and . _ : % -, and not start with '-'"
    )]
    Host,
    #[error(
        "username must be non-empty, use only letters, digits and . _ @ -, and not start with '-'"
    )]
    Username,
    #[error("port must be between 1 and 65535")]
    Port,
    #[error("startup sequence name must not be empty or contain control characters")]
    SequenceName,
    #[error("startup commands must not contain a newline or NUL character")]
    Command,
    #[error("key file path must not contain a NUL character")]
    KeyFile,
}

/// Current time as Unix milliseconds UTC.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        // Clock set before 1970: treat as the epoch rather than failing.
        Err(_) => 0,
    }
}

impl Profile {
    /// A new live profile with a fresh id, port 22, no key file, no sequences,
    /// transcripts off, synced with peers, and `updated_at` set to now.
    pub fn new(name: &str, host: &str, username: &str) -> Profile {
        Profile {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            host: host.to_string(),
            port: DEFAULT_PORT,
            username: username.to_string(),
            key_file: None,
            sequences: Vec::new(),
            transcript: false,
            local_only: false,
            updated_at: now_ms(),
            deleted: false,
        }
    }

    /// Check that a profile is safe to store and to turn into an `ssh` invocation.
    ///
    /// The id is checked for every profile, tombstones included: it reaches the
    /// `MONIKER_PROFILE_ID` environment variable and keyring keys, and it can
    /// arrive from another machine. Every other field is skipped for tombstones
    /// (`deleted == true`), which have no host or username by design.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if !is_canonical_uuid(&self.id) {
            return Err(ProfileError::Id);
        }
        if self.deleted {
            return Ok(());
        }
        if self.name.trim().is_empty() || has_control_char(&self.name) {
            return Err(ProfileError::Name);
        }
        if !valid_token(&self.host, is_host_char) {
            return Err(ProfileError::Host);
        }
        if !valid_token(&self.username, is_username_char) {
            return Err(ProfileError::Username);
        }
        if self.port == 0 {
            return Err(ProfileError::Port);
        }
        if self.key_file.as_ref().is_some_and(|k| has_nul(k)) {
            return Err(ProfileError::KeyFile);
        }
        for seq in &self.sequences {
            if seq.name.trim().is_empty() || has_control_char(&seq.name) {
                return Err(ProfileError::SequenceName);
            }
            if seq
                .commands
                .iter()
                .any(|c| c.contains('\n') || c.contains('\0'))
            {
                return Err(ProfileError::Command);
            }
        }
        Ok(())
    }

    /// A deletion marker for this profile: same id, `deleted = true`, every
    /// other field cleared (so no host, username or commands linger), stamped
    /// with `now_ms`.
    pub fn tombstone(&self, now_ms: i64) -> Profile {
        Profile {
            id: self.id.clone(),
            name: String::new(),
            host: String::new(),
            port: 0,
            username: String::new(),
            key_file: None,
            sequences: Vec::new(),
            transcript: false,
            local_only: false,
            updated_at: now_ms,
            deleted: true,
        }
    }
}

/// True only for the lowercase hyphenated spelling (`8-4-4-4-12` hex digits).
/// `Uuid::parse_str` also accepts braces, `urn:uuid:`, uppercase and simple
/// (unhyphenated) forms; comparing against the canonical rendering rejects them.
pub(crate) fn is_canonical_uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

/// True if `s` has a control character (C0, DEL or C1; NUL and ESC included).
///
/// Names are shown in GTK labels and printed to terminals. A NUL panics glib
/// when it builds a C string (in debug builds), and an escape sequence acts on
/// a terminal, so neither may get into the store, whichever machine it came from.
fn has_control_char(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// True if the path's bytes contain a NUL, which no C string (and so no `ssh`
/// argument) can hold.
fn has_nul(path: &Path) -> bool {
    path.as_os_str().as_bytes().contains(&0)
}

/// Non-empty, every char allowed, and not starting with `-`.
fn valid_token(s: &str, allowed: fn(char) -> bool) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.chars().all(allowed)
}

/// Host charset `[A-Za-z0-9._:%-]`: hostnames, IPv4, IPv6 (with `%zone`).
fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '%' | '-')
}

/// Username charset `[A-Za-z0-9._@-]`.
fn is_username_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Profile {
        Profile::new("Prod web", "example.com", "alice")
    }

    #[test]
    fn new_profile_has_defaults_and_is_valid() {
        let before = now_ms();
        let p = valid();
        let after = now_ms();
        assert_eq!(p.name, "Prod web");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.username, "alice");
        assert_eq!(p.port, 22);
        assert_eq!(p.key_file, None);
        assert!(p.sequences.is_empty());
        assert!(!p.transcript);
        assert!(!p.deleted);
        assert!(p.updated_at >= before && p.updated_at <= after);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn new_profiles_get_distinct_uuid_ids() {
        let a = valid();
        let b = valid();
        assert_ne!(a.id, b.id);
        assert!(uuid::Uuid::parse_str(&a.id).is_ok());
    }

    #[test]
    fn accepts_ordinary_hosts() {
        for host in ["example.com", "10.0.0.5", "fe80::1", "host-1.local"] {
            let mut p = valid();
            p.host = host.to_string();
            assert_eq!(p.validate(), Ok(()), "host {host:?} should be accepted");
        }
    }

    #[test]
    fn rejects_option_like_host() {
        let mut p = valid();
        p.host = "-oProxyCommand=x".to_string();
        assert_eq!(p.validate(), Err(ProfileError::Host));
    }

    #[test]
    fn rejects_bad_host_and_user() {
        for host in ["a b", "", "a\nb", "-x", "a;b", "a/b", "a\0b", "h\u{f6}st"] {
            let mut p = valid();
            p.host = host.to_string();
            assert_eq!(
                p.validate(),
                Err(ProfileError::Host),
                "host {host:?} should be rejected"
            );
        }
        for user in ["", "-o", "a b", "a\nb", "a;b", "a:b", "\u{fc}ser"] {
            let mut p = valid();
            p.username = user.to_string();
            assert_eq!(
                p.validate(),
                Err(ProfileError::Username),
                "username {user:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_usernames() {
        for user in ["root", "alice", "a.b_c-d", "alice@corp.example", "svc01"] {
            let mut p = valid();
            p.username = user.to_string();
            assert_eq!(p.validate(), Ok(()), "username {user:?} should be accepted");
        }
    }

    #[test]
    fn rejects_port_zero() {
        let mut p = valid();
        p.port = 0;
        assert_eq!(p.validate(), Err(ProfileError::Port));
        p.port = 1;
        assert_eq!(p.validate(), Ok(()));
        p.port = 65535;
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_or_blank_name() {
        for name in ["", "   ", "\t"] {
            let mut p = valid();
            p.name = name.to_string();
            assert_eq!(
                p.validate(),
                Err(ProfileError::Name),
                "name {name:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_multiline_command() {
        for bad in ["echo a\necho b", "echo a\0b", "\n", "\0"] {
            let mut p = valid();
            p.sequences.push(StartupSequence {
                name: "update".to_string(),
                commands: vec!["ls".to_string(), bad.to_string()],
            });
            assert_eq!(
                p.validate(),
                Err(ProfileError::Command),
                "command {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_and_blank_commands() {
        let mut p = valid();
        p.sequences.push(StartupSequence {
            name: "update".to_string(),
            commands: vec![
                "sudo apt update && sudo apt -y upgrade".to_string(),
                String::new(),
                "   ".to_string(),
            ],
        });
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_sequence_name() {
        let mut p = valid();
        p.sequences.push(StartupSequence {
            name: String::new(),
            commands: vec!["ls".to_string()],
        });
        assert_eq!(p.validate(), Err(ProfileError::SequenceName));
    }

    #[test]
    fn tombstone_keeps_id_and_clears_fields() {
        let mut p = valid();
        p.key_file = Some(PathBuf::from("/home/alice/.ssh/id_ed25519"));
        p.transcript = true;
        p.sequences.push(StartupSequence {
            name: "update".to_string(),
            commands: vec!["apt update".to_string()],
        });
        let t = p.tombstone(123_456);
        assert!(t.deleted);
        assert_eq!(t.id, p.id);
        assert_eq!(t.host, "");
        assert_eq!(t.name, "");
        assert_eq!(t.username, "");
        assert_eq!(t.key_file, None);
        assert!(t.sequences.is_empty());
        assert!(!t.transcript);
        assert_eq!(t.updated_at, 123_456);
    }

    #[test]
    fn local_only_is_left_out_of_json_when_false() {
        let p = valid();
        assert!(!serde_json::to_string(&p).unwrap().contains("local_only"));
        let kept = Profile {
            local_only: true,
            ..valid()
        };
        assert!(
            serde_json::to_string(&kept)
                .unwrap()
                .contains("\"local_only\":true")
        );
        assert!(!kept.tombstone(5).local_only);
    }

    #[test]
    fn tombstone_skips_validation() {
        let t = valid().tombstone(1);
        // A tombstone has an empty host, name and username and a zero port.
        assert_eq!(t.validate(), Ok(()));
    }

    #[test]
    fn live_profile_with_deleted_false_is_still_validated() {
        let mut t = valid().tombstone(1);
        t.deleted = false;
        assert!(t.validate().is_err());
    }

    #[test]
    fn new_profile_has_valid_id() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn rejects_non_uuid_id() {
        for id in [
            "",
            "abc",
            "../x",
            "550e8400-e29b-41d4-a716-446655440000\n",
            " 550e8400-e29b-41d4-a716-446655440000",
            "550E8400-E29B-41D4-A716-446655440000",
            "{550e8400-e29b-41d4-a716-446655440000}",
            "urn:uuid:550e8400-e29b-41d4-a716-446655440000",
            "550e8400e29b41d4a716446655440000",
            "550e8400-e29b-41d4-a716-4466554400000",
        ] {
            let mut p = valid();
            p.id = id.to_string();
            assert_eq!(
                p.validate(),
                Err(ProfileError::Id),
                "id {id:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_canonical_lowercase_uuid_id() {
        let mut p = valid();
        p.id = "550e8400-e29b-41d4-a716-446655440000".to_string();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn tombstone_with_bad_id_is_rejected() {
        let mut t = valid().tombstone(1);
        t.id = "../x".to_string();
        assert_eq!(t.validate(), Err(ProfileError::Id));
    }

    // ----- control characters -----
    //
    // A NUL in a name makes glib panic (debug builds) when GTK is handed the
    // string, and an escape sequence in one is printed to terminals. Profiles
    // arrive from other machines, so validation keeps them out of the store.

    #[test]
    fn rejects_control_characters_in_a_name() {
        for name in [
            "a\0b", "a\x1bb", "a\nb", "a\tb", "a\rb", "a\x7fb", "a\u{85}b", "a\u{9b}b", "\0",
        ] {
            let mut p = valid();
            p.name = name.to_string();
            assert_eq!(
                p.validate(),
                Err(ProfileError::Name),
                "name {name:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_unicode_names() {
        for name in [
            "Prod web",
            "web-1 (eu) / backup",
            "caf\u{e9} \u{2615}",
            "\u{65e5}\u{672c}\u{8a9e} \u{30b5}\u{30fc}\u{30d0}\u{30fc}",
            "\u{1f680} launch",
            "<b>x</b> & _y_",
            "  padded  ",
        ] {
            let mut p = valid();
            p.name = name.to_string();
            assert_eq!(p.validate(), Ok(()), "name {name:?} should be accepted");
        }
    }

    #[test]
    fn rejects_control_characters_in_a_sequence_name() {
        for name in ["a\0b", "a\x1bb", "a\nb", "a\tb", "\x7f", "a\u{9b}b"] {
            let mut p = valid();
            p.sequences.push(StartupSequence {
                name: name.to_string(),
                commands: vec!["ls".to_string()],
            });
            assert_eq!(
                p.validate(),
                Err(ProfileError::SequenceName),
                "sequence name {name:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_unicode_sequence_names() {
        for name in [
            "update",
            "apt update & upgrade",
            "caf\u{e9}",
            "\u{65e5}\u{672c}",
        ] {
            let mut p = valid();
            p.sequences.push(StartupSequence {
                name: name.to_string(),
                commands: vec!["ls".to_string()],
            });
            assert_eq!(
                p.validate(),
                Ok(()),
                "sequence name {name:?} should be accepted"
            );
        }
    }

    #[test]
    fn rejects_a_key_file_path_with_a_nul() {
        use std::ffi::OsStr;
        for bytes in [&b"/home/a/.ssh/id\0_rsa"[..], b"\0", b"/k\0"] {
            let mut p = valid();
            p.key_file = Some(PathBuf::from(OsStr::from_bytes(bytes)));
            assert_eq!(
                p.validate(),
                Err(ProfileError::KeyFile),
                "key file {bytes:?} should be rejected"
            );
        }
    }

    #[test]
    fn key_file_rules_for_none_and_ordinary_paths_are_unchanged() {
        let mut p = valid();
        p.key_file = None;
        assert_eq!(p.validate(), Ok(()));
        // Present but empty was accepted before and still is: the editor never
        // produces it (blank means none), and ssh is handed the path as given.
        p.key_file = Some(PathBuf::new());
        assert_eq!(p.validate(), Ok(()));
        for path in [
            "/home/alice/.ssh/id_ed25519",
            "~/.ssh/key",
            "rel/k with space",
        ] {
            p.key_file = Some(PathBuf::from(path));
            assert_eq!(p.validate(), Ok(()), "key file {path:?} should be accepted");
        }
        // A non-UTF-8 path is not this check's business (the store reports it
        // when it cannot serialize it).
        use std::ffi::OsStr;
        p.key_file = Some(PathBuf::from(OsStr::from_bytes(b"/k/\xff")));
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn tombstones_are_not_checked_for_control_characters() {
        // A tombstone's text fields are ignored (and wiped when stored).
        let mut t = valid().tombstone(1);
        t.name = "a\0b".to_string();
        assert_eq!(t.validate(), Ok(()));
    }

    #[test]
    fn new_errors_have_readable_messages() {
        for err in [
            ProfileError::Name,
            ProfileError::SequenceName,
            ProfileError::KeyFile,
        ] {
            let text = err.to_string();
            assert!(text.chars().all(|c| !c.is_control()), "{text:?}");
            assert!(text.len() > 20, "{text:?}");
        }
        assert!(ProfileError::KeyFile.to_string().contains("NUL"));
        assert!(ProfileError::Name.to_string().contains("control"));
        assert!(ProfileError::SequenceName.to_string().contains("control"));
    }
}
