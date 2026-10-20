//! `moniker-askpass`, the helper OpenSSH runs through `SSH_ASKPASS` to get a
//! password, key passphrase or host-key answer.
//!
//! `ssh` runs it once per prompt with the prompt text as its only argument and
//! reads the answer from its stdout. The decisions (answer a login password
//! from the keyring, ask the user anything else) are in
//! `moniker_core::askpass`; this file reads the environment, finds the profile's
//! identity and prints the result.
//!
//! The environment carries only non-secret values: `SSH_ASKPASS_PROMPT` (set by
//! `ssh`), `MONIKER_PROFILE_ID` and, for a terminal tab, `MONIKER_ASKPASS_TOKEN`
//! (set by Moniker), and `XDG_RUNTIME_DIR` (where the token's one-time marker
//! goes). With a token the stored password is sent at most once per connection;
//! if no marker can be made the user is asked. The helper logs nothing: no
//! prompt, no answer, no profile data. On failure stdout stays empty and stderr
//! gets one generic line, and the exit status is 1.

use std::io::{self, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use moniker_cli::store_path;
use moniker_core::askpass::{DevTty, OneTime, TOKEN_ENV, TtyPrompter, answer, claim_marker};
use moniker_core::secrets::{KeyringSecretStore, SecretStore};
use moniker_core::store::ProfileStore;

/// Why no answer was given. Deliberately generic: the prompt can name a key
/// file or host, and the reason must never echo it.
const NO_ANSWER: &str = "could not get an answer for ssh (is there a terminal to ask on?)";

fn main() -> ExitCode {
    // A panic message would print the panic's payload and location. Nothing here
    // puts a secret in a panic message, but the helper handles secrets, so it
    // prints a fixed line instead of trusting every dependency.
    panic::set_hook(Box::new(|_| {}));
    match panic::catch_unwind(run_from_environment) {
        Ok(code) => code,
        Err(_) => {
            eprintln!("moniker-askpass: internal error");
            ExitCode::FAILURE
        }
    }
}

/// Read the prompt and the environment, answer, and report the exit status.
fn run_from_environment() -> ExitCode {
    // No prompt is a misuse: say nothing, answer nothing.
    let Some(prompt) = std::env::args_os().nth(1) else {
        return ExitCode::FAILURE;
    };
    let prompt = prompt.to_string_lossy();
    let env_prompt = std::env::var("SSH_ASKPASS_PROMPT").ok();
    let profile_id = std::env::var("MONIKER_PROFILE_ID").ok();
    // A token that is set but not UTF-8 still counts as a token (an empty one,
    // which names no marker): it must not pass for "no token", which would
    // lift the one-time limit.
    let token = std::env::var_os(TOKEN_ENV).map(|t| t.into_string().unwrap_or_default());
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let identity = profile_id.as_deref().and_then(identity_from_store);
    let expected = identity
        .as_ref()
        .map(|(user, host)| (user.as_str(), host.as_str()));

    let env = Env {
        prompt_kind: env_prompt.as_deref(),
        profile_id: profile_id.as_deref(),
        token: token.as_deref(),
        runtime_dir: runtime_dir.as_deref(),
    };
    let result = respond(
        &prompt,
        &env,
        expected,
        &KeyringSecretStore::new(),
        &mut DevTty,
        &mut io::stdout().lock(),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("moniker-askpass: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// What `ssh` and Moniker put in the helper's environment. None of it is
/// secret.
#[derive(Default)]
struct Env<'a> {
    /// `SSH_ASKPASS_PROMPT`, set by `ssh`: `confirm`, `none` or unset.
    prompt_kind: Option<&'a str>,
    /// `MONIKER_PROFILE_ID`: whose password may answer.
    profile_id: Option<&'a str>,
    /// `MONIKER_ASKPASS_TOKEN`: the connection's one-time token. Set for a
    /// terminal tab, not for a sync.
    token: Option<&'a str>,
    /// `XDG_RUNTIME_DIR`, where the token's marker goes.
    runtime_dir: Option<&'a Path>,
}

/// Answer one prompt and print the answer and a newline to `out`.
///
/// With a token the stored password may be used once per connection: only the
/// helper that creates the token's marker sends it. A runtime directory that is
/// unset or not absolute, or a marker that cannot be made, means no marker, and
/// the user is asked (`moniker_core::askpass`). Without a token the marker is
/// not used.
///
/// On `Err` nothing has been written to `out`, except if writing the answer
/// itself failed halfway.
fn respond(
    prompt: &str,
    env: &Env,
    expected: Option<(&str, &str)>,
    secrets: &dyn SecretStore,
    tty: &mut dyn TtyPrompter,
    out: &mut dyn Write,
) -> Result<(), &'static str> {
    let usable_dir = env.runtime_dir.filter(|dir| dir.is_absolute());
    let claim = || match (env.token, usable_dir) {
        (Some(token), Some(dir)) => claim_marker(dir, token).unwrap_or(false),
        _ => false,
    };
    let once = match env.token {
        Some(_) => OneTime::Claim(&claim),
        None => OneTime::NotUsed,
    };
    let reply = answer(
        prompt,
        env.prompt_kind,
        env.profile_id,
        expected,
        once,
        secrets,
        tty,
    )
    .map_err(|_| NO_ANSWER)?;
    out.write_all(reply.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .map_err(|_| "could not write the answer to ssh")
}

/// The `(username, host)` of the profile `profile_id` names, read from the
/// profile file. Any failure (no config directory, unreadable store, no such
/// profile) is `None`, which makes the helper ask the user instead.
fn identity_from_store(profile_id: &str) -> Option<(String, String)> {
    let store = ProfileStore::open(&store_path().ok()?).ok()?;
    identity_of(&store, profile_id)
}

/// The identity `ssh` prints in a password prompt for this profile. `ssh`
/// prints the host name lowercased, so the host is lowercased here.
fn identity_of(store: &ProfileStore, profile_id: &str) -> Option<(String, String)> {
    let profile = store.get(profile_id)?;
    Some((profile.username.clone(), profile.host.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use std::io;

    use moniker_core::askpass::marker_dir;
    use moniker_core::profile::Profile;
    use moniker_core::secrets::{MemorySecretStore, password_key};

    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    /// A terminal with one prepared reply that records what it was asked.
    struct FakeTty {
        reply: io::Result<String>,
        asked: Vec<String>,
        notified: Vec<String>,
    }

    impl FakeTty {
        fn replying(reply: &str) -> Self {
            Self::with(Ok(reply.to_owned()))
        }

        fn broken() -> Self {
            Self::with(Err(io::Error::from(io::ErrorKind::NotFound)))
        }

        fn with(reply: io::Result<String>) -> Self {
            FakeTty {
                reply,
                asked: Vec::new(),
                notified: Vec::new(),
            }
        }
    }

    impl TtyPrompter for FakeTty {
        fn ask(&mut self, prompt: &str, _echo: bool) -> io::Result<String> {
            self.asked.push(prompt.to_owned());
            match &self.reply {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(io::Error::from(e.kind())),
            }
        }

        fn notify(&mut self, msg: &str) -> io::Result<()> {
            self.notified.push(msg.to_owned());
            match &self.reply {
                Ok(_) => Ok(()),
                Err(e) => Err(io::Error::from(e.kind())),
            }
        }
    }

    fn secrets_with_password(pw: &str) -> MemorySecretStore {
        let s = MemorySecretStore::default();
        s.set(&password_key(ID), pw).unwrap();
        s
    }

    #[test]
    fn a_matching_password_prompt_is_answered_from_the_keyring() {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::broken();
        let mut out = Vec::new();
        respond(
            "admin@example.com's password: ",
            &Env {
                prompt_kind: None,
                profile_id: Some(ID),
                ..Env::default()
            },
            Some(("admin", "example.com")),
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"hunter2\n");
        assert!(tty.asked.is_empty(), "the terminal must not be asked");
    }

    #[test]
    fn without_an_identity_the_user_is_asked_and_the_reply_printed() {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::replying("typed by the user");
        let mut out = Vec::new();
        respond(
            "admin@example.com's password: ",
            &Env {
                prompt_kind: None,
                profile_id: Some(ID),
                ..Env::default()
            },
            None,
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"typed by the user\n");
        assert_eq!(tty.asked, ["admin@example.com's password: "]);
    }

    #[test]
    fn a_host_key_question_goes_to_the_user() {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::replying("no");
        let mut out = Vec::new();
        respond(
            "Are you sure you want to continue connecting (yes/no)? ",
            &Env {
                prompt_kind: Some("confirm"),
                profile_id: Some(ID),
                ..Env::default()
            },
            Some(("admin", "example.com")),
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"no\n");
    }

    #[test]
    fn a_message_is_shown_and_answered_with_an_empty_line() {
        let mut tty = FakeTty::replying("");
        let mut out = Vec::new();
        respond(
            "Authenticated to example.com",
            &Env {
                prompt_kind: Some("none"),
                profile_id: None,
                ..Env::default()
            },
            None,
            &MemorySecretStore::default(),
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"\n");
        assert_eq!(tty.notified, ["Authenticated to example.com"]);
    }

    #[test]
    fn a_failure_prints_nothing_and_does_not_repeat_the_prompt() {
        let secrets = MemorySecretStore::default();
        let mut tty = FakeTty::broken();
        let mut out = Vec::new();
        let prompt = "Enter passphrase for key '/home/me/.ssh/id_secret_name': ";
        let reason = respond(
            prompt,
            &Env {
                prompt_kind: None,
                profile_id: None,
                ..Env::default()
            },
            None,
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap_err();
        assert!(out.is_empty(), "stdout must stay empty on failure");
        assert!(!reason.contains("id_secret_name"), "{reason}");
        assert!(!reason.contains("passphrase"), "{reason}");
        assert!(!reason.contains('\n'), "one line only: {reason}");
        assert!(!reason.is_empty());
    }

    #[test]
    fn a_stdout_that_cannot_be_written_is_a_failure() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::broken();
        let reason = respond(
            "admin@example.com's password: ",
            &Env {
                prompt_kind: None,
                profile_id: Some(ID),
                ..Env::default()
            },
            Some(("admin", "example.com")),
            &secrets,
            &mut tty,
            &mut Closed,
        )
        .unwrap_err();
        assert!(!reason.contains("hunter2"), "{reason}");
    }

    // The one-time marker of a terminal tab.

    const TOKEN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";
    const PROMPT: &str = "admin@example.com's password: ";
    const IDENTITY: Option<(&str, &str)> = Some(("admin", "example.com"));

    /// What the helper prints for `PROMPT` with `env`, the keyring holding
    /// "hunter2" and the user typing "typed", and whether the user was asked.
    fn printed(env: &Env) -> (String, bool) {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::replying("typed");
        let mut out = Vec::new();
        respond(PROMPT, env, IDENTITY, &secrets, &mut tty, &mut out).unwrap();
        let asked = !tty.asked.is_empty();
        if asked {
            assert_eq!(tty.asked, [PROMPT]);
        }
        (String::from_utf8(out).unwrap(), asked)
    }

    #[test]
    fn with_a_token_the_stored_password_is_sent_once() {
        let rt = tempfile::tempdir().unwrap();
        let env = Env {
            profile_id: Some(ID),
            token: Some(TOKEN),
            runtime_dir: Some(rt.path()),
            ..Env::default()
        };
        assert_eq!(printed(&env), ("hunter2\n".to_owned(), false));
        // The server asks again: the user types it.
        assert_eq!(printed(&env), ("typed\n".to_owned(), true));
        assert_eq!(printed(&env), ("typed\n".to_owned(), true));
        assert!(marker_dir(rt.path()).join(TOKEN).exists());
    }

    #[test]
    fn without_a_token_the_marker_is_not_used() {
        // A sync: ssh allows one prompt itself.
        let rt = tempfile::tempdir().unwrap();
        let env = Env {
            profile_id: Some(ID),
            runtime_dir: Some(rt.path()),
            ..Env::default()
        };
        assert_eq!(printed(&env), ("hunter2\n".to_owned(), false));
        assert_eq!(printed(&env), ("hunter2\n".to_owned(), false));
        assert!(!rt.path().join("moniker").exists(), "no marker made");
    }

    #[test]
    fn a_token_without_a_usable_runtime_dir_asks_the_user() {
        for runtime_dir in [None, Some(Path::new("")), Some(Path::new("run/user/1000"))] {
            let env = Env {
                profile_id: Some(ID),
                token: Some(TOKEN),
                runtime_dir,
                ..Env::default()
            };
            assert_eq!(
                printed(&env),
                ("typed\n".to_owned(), true),
                "{runtime_dir:?}"
            );
        }
        assert!(
            !Path::new("run").exists(),
            "nothing made in the working directory"
        );
    }

    #[test]
    fn a_token_that_is_not_a_uuid_asks_the_user() {
        let rt = tempfile::tempdir().unwrap();
        for token in ["", "../../x", "x", &TOKEN.to_uppercase()] {
            let env = Env {
                profile_id: Some(ID),
                token: Some(token),
                runtime_dir: Some(rt.path()),
                ..Env::default()
            };
            assert_eq!(printed(&env), ("typed\n".to_owned(), true), "{token:?}");
        }
        assert!(!rt.path().join("moniker").exists());
    }

    fn store_with(p: Profile) -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProfileStore::open(&dir.path().join("profiles.json")).unwrap();
        store.upsert(p).unwrap();
        (dir, store)
    }

    #[test]
    fn identity_is_the_username_and_the_lowercased_host() {
        // ssh prints the host name lowercased in its password prompt.
        let mut p = Profile::new("web1", "Example.COM", "Admin");
        p.id = ID.to_owned();
        let (_dir, store) = store_with(p);
        assert_eq!(
            identity_of(&store, ID),
            Some(("Admin".to_owned(), "example.com".to_owned()))
        );
    }

    #[test]
    fn identity_of_an_unknown_profile_is_none() {
        let (_dir, store) = store_with(Profile::new("web1", "example.com", "admin"));
        assert_eq!(identity_of(&store, ID), None);
        assert_eq!(identity_of(&store, ""), None);
    }

    #[test]
    fn identity_of_a_deleted_profile_is_none() {
        let mut p = Profile::new("web1", "example.com", "admin");
        p.id = ID.to_owned();
        let (_dir, mut store) = store_with(p);
        store.delete(ID).unwrap();
        assert_eq!(identity_of(&store, ID), None);
    }
}
