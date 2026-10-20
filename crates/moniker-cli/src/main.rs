//! The `moniker-server` command line: list profiles, set the sync key, and sync.
//!
//! `main` parses the arguments and calls one small function per subcommand.
//! Each returns `Err(message)` on failure; `main` prints the message to stderr
//! (with control characters made harmless) and exits with status 1.
//!
//! `sync-serve` is the other half of `sync`: `sync` runs `moniker-server sync-serve` on
//! the remote machine over ssh and speaks the sync protocol with it over the
//! ssh channel. Its stdout carries protocol frames and nothing else, so no
//! function on that path may print to stdout.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use moniker_cli::{sanitize_for_terminal, store_path};
use moniker_core::askpass::{DevTty, TtyPrompter};
use moniker_core::profile::Profile;
use moniker_core::secrets::{
    KeyringSecretStore, SYNC_PASSPHRASE_KEY, SecretStore, check_sync_passphrase, password_key,
};
use moniker_core::ssh::{SERVER_PROGRAM, sync_command};
use moniker_core::store::ProfileStore;
use moniker_core::sync::{LocalState, serve, sync_over_ssh};
use zeroize::Zeroizing;

/// What `moniker-server sync` says when this machine has no sync key.
const NO_SYNC_KEY_LOCAL: &str = "no sync key is set; run `moniker-server sync-key set`";

/// What `moniker-server sync-serve` says when its machine has no sync key. It runs on
/// the other machine and the client relays this text, so it must not say "this
/// machine". `scripts/e2e-sync.sh` checks the same text.
const NO_SYNC_KEY_REMOTE: &str = "no sync key is set on the remote machine \
    (where `moniker-server sync-serve` runs); run `moniker-server sync-key set` there";

/// The longest sync key `sync-key set --stdin` accepts, in bytes.
const MAX_STDIN_KEY: usize = 64 * 1024;

/// Moniker: SSH connection profiles and credential sync between your machines.
#[derive(Parser)]
#[command(name = "moniker-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with saved connection profiles
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Manage the sync key
    SyncKey {
        #[command(subcommand)]
        command: SyncKeyCommand,
    },
    /// Serve one sync on stdin/stdout (run on the remote machine by `moniker-server sync`)
    SyncServe,
    /// Sync with the machine a profile connects to
    Sync {
        /// Name of the profile (must be unique)
        profile: String,
    },
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// List saved profiles
    List,
}

#[derive(Subcommand)]
enum SyncKeyCommand {
    /// Store the sync key in the system keyring (the same key on every machine; at least 12 characters)
    Set {
        /// Read the key from standard input instead of prompting (one trailing newline is removed)
        #[arg(long)]
        stdin: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Profile {
            command: ProfileCommand::List,
        } => profile_list(),
        Command::SyncKey {
            command: SyncKeyCommand::Set { stdin },
        } => sync_key_set(stdin),
        Command::SyncServe => sync_serve(),
        Command::Sync { profile } => sync(&profile),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("moniker-server: {}", sanitize_for_terminal(&message));
            ExitCode::FAILURE
        }
    }
}

// ----- subcommands -----

/// `moniker-server profile list`. Never touches the keyring.
fn profile_list() -> Result<(), String> {
    let store = open_store()?;
    print_stdout(&profile_list_text(&store.live()))
}

/// `moniker-server sync-key set [--stdin]`.
fn sync_key_set(from_stdin: bool) -> Result<(), String> {
    let key = if from_stdin {
        read_passphrase_stdin(io::stdin().lock())?
    } else {
        prompt_passphrase(&mut DevTty)?
    };
    KeyringSecretStore::new()
        .set(SYNC_PASSPHRASE_KEY, &key)
        .map_err(|e| format!("cannot save the sync key: {e}"))?;
    print_stdout("Sync key saved.\n")
}

/// `moniker-server sync-serve`: the remote half of a sync. Stdout is for protocol
/// frames only; everything else goes to stderr.
fn sync_serve() -> Result<(), String> {
    let secrets = KeyringSecretStore::new();
    let passphrase = sync_passphrase(&secrets, NO_SYNC_KEY_REMOTE)?;
    let mut local = LocalState::new(open_store()?, Box::new(secrets));
    // `serve` has already sent an ERROR frame for anything the peer should hear
    // about; the message here is for whoever reads this machine's stderr.
    serve(
        io::stdin().lock(),
        io::stdout().lock(),
        &mut local,
        &passphrase,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// `moniker-server sync <profile-name>`: the local half of a sync, over ssh.
fn sync(name: &str) -> Result<(), String> {
    // Look the profile up before anything touches the keyring.
    let store = open_store()?;
    let profile = find_profile(&store, name)?.clone();

    let secrets = KeyringSecretStore::new();
    let passphrase = sync_passphrase(&secrets, NO_SYNC_KEY_LOCAL)?;
    let has_password = secrets
        .get(&password_key(&profile.id))
        .map_err(|e| format!("cannot look up the stored password: {e}"))?
        .is_some();

    let current = std::env::current_exe()
        .map_err(|e| format!("cannot locate the moniker-server executable: {e}"))?;
    let askpass = current.with_file_name("moniker-askpass");
    check_askpass(has_password, &askpass)?;
    let cmd = sync_command(&profile, &askpass, has_password, SERVER_PROGRAM);

    let mut local = LocalState::new(store, Box::new(secrets));
    let outcome = sync_over_ssh(&mut local, &cmd, &passphrase).map_err(|e| e.to_string())?;
    print_stdout(&format!(
        "added {}, updated {}, removed {}\n",
        outcome.added, outcome.updated, outcome.removed
    ))
}

// ----- helpers -----

/// A sync runs with no terminal, so a stored password reaches ssh only through
/// the `moniker-askpass` helper. If one is stored and the helper is not an
/// existing file, say so now, before connecting, instead of failing later with
/// an authentication error.
fn check_askpass(has_password: bool, askpass: &Path) -> Result<(), String> {
    if has_password && !askpass.is_file() {
        Err(format!(
            "moniker-askpass not found next to moniker-server (looked for {}), so the stored \
             password cannot be used for the sync (build with `cargo build --workspace`)",
            askpass.display()
        ))
    } else {
        Ok(())
    }
}

/// Write `text` to stdout. A closed stdout is an error, not a panic.
fn print_stdout(text: &str) -> Result<(), String> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| format!("cannot write to standard output: {e}"))
}

fn open_store() -> Result<ProfileStore, String> {
    let path = store_path()?;
    ProfileStore::open(&path).map_err(|e| format!("{e} ({})", path.display()))
}

/// The sync passphrase from the keyring. `if_missing` is the message for
/// "none stored" (an empty stored value counts as none).
fn sync_passphrase(
    secrets: &dyn SecretStore,
    if_missing: &str,
) -> Result<Zeroizing<String>, String> {
    match secrets.get(SYNC_PASSPHRASE_KEY) {
        Ok(Some(key)) if !key.is_empty() => Ok(key),
        Ok(_) => Err(if_missing.to_owned()),
        Err(e) => Err(format!("cannot read the sync key: {e}")),
    }
}

/// The one live profile named exactly `name`.
fn find_profile<'a>(store: &'a ProfileStore, name: &str) -> Result<&'a Profile, String> {
    let mut matches = store.live().into_iter().filter(|p| p.name == name);
    match (matches.next(), matches.next()) {
        (Some(profile), None) => Ok(profile),
        (None, _) => Err(format!(
            "no profile named \"{}\"",
            sanitize_for_terminal(name)
        )),
        (Some(_), Some(_)) => Err(format!(
            "more than one profile is named \"{}\"; profile names must be unique to sync by name",
            sanitize_for_terminal(name)
        )),
    }
}

/// The output of `profile list`: `<name>  <user>@<host>:<port>` per profile, or
/// `No profiles.`. Every field is made safe to print.
fn profile_list_text(profiles: &[&Profile]) -> String {
    if profiles.is_empty() {
        return "No profiles.\n".to_owned();
    }
    profiles
        .iter()
        .map(|p| {
            format!(
                "{}  {}@{}:{}\n",
                sanitize_for_terminal(&p.name),
                sanitize_for_terminal(&p.username),
                sanitize_for_terminal(&p.host),
                p.port
            )
        })
        .collect()
}

/// Read the sync key from `reader` to the end of input (`--stdin`).
fn read_passphrase_stdin(reader: impl Read) -> Result<Zeroizing<String>, String> {
    // Reserve the whole limit once: a buffer that grows would leave copies of
    // the key in freed memory. The buffer is wiped when it drops.
    let mut raw = Zeroizing::new(Vec::with_capacity(MAX_STDIN_KEY + 1));
    reader
        .take(MAX_STDIN_KEY as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot read the sync key from standard input: {e}"))?;
    if raw.len() > MAX_STDIN_KEY {
        return Err("the sync key on standard input is too long (limit 64 KiB)".to_owned());
    }
    let text = std::str::from_utf8(&raw)
        .map_err(|_| "the sync key on standard input is not valid UTF-8".to_owned())?;
    let key = passphrase_from_stdin_text(text)?;
    check_sync_passphrase(key).map_err(str::to_owned)?;
    Ok(Zeroizing::new(key.to_owned()))
}

/// The sync key in text read from standard input: exactly one trailing `\n` or
/// `\r\n` is removed and nothing else (spaces and further newlines can be part
/// of the key). An empty result is an error. (The length minimum is checked by
/// the caller, [`read_passphrase_stdin`].)
fn passphrase_from_stdin_text(raw: &str) -> Result<&str, String> {
    let key = match raw.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => raw,
    };
    if key.is_empty() {
        Err("the sync key must not be empty".to_owned())
    } else {
        Ok(key)
    }
}

/// Ask for the sync key twice on the terminal, without echo.
fn prompt_passphrase(tty: &mut dyn TtyPrompter) -> Result<Zeroizing<String>, String> {
    let mut ask = |prompt: &str| {
        tty.ask(prompt, false).map(Zeroizing::new).map_err(|e| {
            format!(
                "cannot read from the terminal ({e}); to pipe the key in, use `moniker-server sync-key set --stdin`"
            )
        })
    };
    let first = ask("Sync key: ")?;
    // Refuse a key that is empty or too short before asking for it again.
    check_sync_passphrase(&first).map_err(str::to_owned)?;
    let second = ask("Repeat sync key: ")?;
    if *first != *second {
        Err("the two entries differ; the sync key was not changed".to_owned())
    } else {
        Ok(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moniker_core::secrets::MemorySecretStore;
    use std::io;

    #[test]
    fn one_trailing_newline_is_stripped() {
        assert_eq!(passphrase_from_stdin_text("secret\n"), Ok("secret"));
    }

    #[test]
    fn crlf_is_stripped() {
        assert_eq!(passphrase_from_stdin_text("secret\r\n"), Ok("secret"));
    }

    #[test]
    fn only_one_newline_is_stripped() {
        assert_eq!(passphrase_from_stdin_text("secret\n\n"), Ok("secret\n"));
        assert_eq!(
            passphrase_from_stdin_text("secret\r\n\r\n"),
            Ok("secret\r\n")
        );
        assert_eq!(passphrase_from_stdin_text("secret\r\r\n"), Ok("secret\r"));
        assert_eq!(passphrase_from_stdin_text("secret\n\r\n"), Ok("secret\n"));
    }

    #[test]
    fn no_newline_unchanged() {
        assert_eq!(passphrase_from_stdin_text("secret"), Ok("secret"));
        assert_eq!(
            passphrase_from_stdin_text("  sp aced \t"),
            Ok("  sp aced \t")
        );
        // A lone carriage return is not a line ending.
        assert_eq!(passphrase_from_stdin_text("secret\r"), Ok("secret\r"));
    }

    #[test]
    fn empty_is_rejected() {
        assert!(passphrase_from_stdin_text("").is_err());
        assert!(passphrase_from_stdin_text("\n").is_err());
        assert!(passphrase_from_stdin_text("\r\n").is_err());
    }

    #[test]
    fn a_line_break_alone_after_stripping_is_a_valid_key() {
        // Exactly one newline is removed; what is left counts.
        assert_eq!(passphrase_from_stdin_text("\n\n"), Ok("\n"));
    }

    #[test]
    fn stdin_passphrase_is_read_to_the_end() {
        let key = read_passphrase_stdin(io::Cursor::new(b"correct horse\nbattery\n".to_vec()));
        assert_eq!(key.unwrap().as_str(), "correct horse\nbattery");
    }

    #[test]
    fn stdin_passphrase_that_is_not_utf8_is_rejected() {
        let err = read_passphrase_stdin(io::Cursor::new(vec![0xff, 0xfe, b'\n'])).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn stdin_passphrase_that_is_too_long_is_rejected() {
        let huge = vec![b'a'; 1024 * 1024];
        let err = read_passphrase_stdin(io::Cursor::new(huge)).unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    #[test]
    fn empty_stdin_is_rejected() {
        assert!(read_passphrase_stdin(io::Cursor::new(Vec::new())).is_err());
        assert!(read_passphrase_stdin(io::Cursor::new(b"\n".to_vec())).is_err());
    }

    /// A terminal that replays prepared answers and records the prompts.
    struct ScriptedTty {
        answers: Vec<io::Result<String>>,
        asked: Vec<(String, bool)>,
    }

    impl ScriptedTty {
        fn new(mut answers: Vec<io::Result<String>>) -> Self {
            answers.reverse();
            ScriptedTty {
                answers,
                asked: Vec::new(),
            }
        }
    }

    impl TtyPrompter for ScriptedTty {
        fn ask(&mut self, prompt: &str, echo: bool) -> io::Result<String> {
            self.asked.push((prompt.to_owned(), echo));
            self.answers.pop().expect("asked more than expected")
        }

        fn notify(&mut self, _msg: &str) -> io::Result<()> {
            panic!("notify is not used for the sync key");
        }
    }

    fn typed(a: &str, b: &str) -> ScriptedTty {
        ScriptedTty::new(vec![Ok(a.to_owned()), Ok(b.to_owned())])
    }

    #[test]
    fn prompting_asks_twice_without_echo() {
        let mut tty = typed("hunter2 hunter2", "hunter2 hunter2");
        let key = prompt_passphrase(&mut tty).unwrap();
        assert_eq!(key.as_str(), "hunter2 hunter2");
        assert_eq!(
            tty.asked,
            vec![
                ("Sync key: ".to_owned(), false),
                ("Repeat sync key: ".to_owned(), false)
            ]
        );
    }

    #[test]
    fn prompting_rejects_entries_that_differ() {
        let err = prompt_passphrase(&mut typed("alpha-secret", "beta-secret")).unwrap_err();
        assert!(err.contains("differ"), "{err}");
        assert!(!err.contains("alpha") && !err.contains("beta"), "{err}");
    }

    #[test]
    fn prompting_rejects_an_empty_key() {
        let err = prompt_passphrase(&mut typed("", "")).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn prompting_reports_a_missing_terminal() {
        let mut tty = ScriptedTty::new(vec![Err(io::Error::from(io::ErrorKind::NotFound))]);
        let err = prompt_passphrase(&mut tty).unwrap_err();
        assert!(err.contains("--stdin"), "{err}");
    }

    // ----- the "no sync key" messages -----

    #[test]
    fn the_server_side_message_names_the_remote_machine() {
        // `sync-serve` runs on the other machine and this text is relayed
        // verbatim to the client, so it must not say "this machine".
        assert_eq!(
            NO_SYNC_KEY_REMOTE,
            "no sync key is set on the remote machine (where `moniker-server sync-serve` runs); \
             run `moniker-server sync-key set` there"
        );
        assert!(!NO_SYNC_KEY_REMOTE.contains("this machine"));
    }

    #[test]
    fn the_local_message_is_unchanged() {
        assert_eq!(
            NO_SYNC_KEY_LOCAL,
            "no sync key is set; run `moniker-server sync-key set`"
        );
    }

    #[test]
    fn the_two_messages_are_given_for_a_missing_or_empty_key() {
        let secrets = MemorySecretStore::default();
        assert_eq!(
            sync_passphrase(&secrets, NO_SYNC_KEY_REMOTE).unwrap_err(),
            NO_SYNC_KEY_REMOTE
        );
        secrets.set(SYNC_PASSPHRASE_KEY, "").unwrap();
        assert_eq!(
            sync_passphrase(&secrets, NO_SYNC_KEY_LOCAL).unwrap_err(),
            NO_SYNC_KEY_LOCAL
        );
    }

    // ----- the askpass helper -----

    #[test]
    fn a_stored_password_without_the_helper_is_an_error_before_connecting() {
        let missing = Path::new("/nonexistent/moniker/moniker-askpass");
        let err = check_askpass(true, missing).unwrap_err();
        assert!(
            err.contains("moniker-askpass not found next to moniker-server"),
            "{err}"
        );
        assert!(
            err.contains("/nonexistent/moniker/moniker-askpass"),
            "{err}"
        );
        assert!(err.contains("cargo build --workspace"), "{err}");
        // A directory is not a helper either.
        let dir = tempfile::tempdir().unwrap();
        assert!(check_askpass(true, dir.path()).is_err());
    }

    #[test]
    fn the_helper_is_needed_only_with_a_stored_password_and_only_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("moniker-askpass");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        assert_eq!(check_askpass(true, &helper), Ok(()));
        assert_eq!(check_askpass(false, &helper), Ok(()));
        assert_eq!(check_askpass(false, Path::new("/nonexistent/x")), Ok(()));
    }

    // ----- the sync key minimum (set time only) -----

    #[test]
    fn a_short_stdin_key_is_rejected_without_echoing_it() {
        let err = read_passphrase_stdin(io::Cursor::new(b"shortie\n".to_vec())).unwrap_err();
        assert!(err.contains("at least 12 characters"), "{err}");
        assert!(!err.contains("shortie"), "{err}");
        // The line ending is not part of the key: eleven characters and a CRLF.
        let err = read_passphrase_stdin(io::Cursor::new(b"abcdefghijk\r\n".to_vec())).unwrap_err();
        assert!(err.contains("at least 12 characters"), "{err}");
    }

    #[test]
    fn a_stdin_key_of_exactly_the_minimum_is_accepted() {
        let key = read_passphrase_stdin(io::Cursor::new(b"abcdefghijkl\n".to_vec())).unwrap();
        assert_eq!(key.as_str(), "abcdefghijkl");
        let key = read_passphrase_stdin(io::Cursor::new(b"abcdefghijkl".to_vec())).unwrap();
        assert_eq!(key.as_str(), "abcdefghijkl");
    }

    #[test]
    fn the_stdin_minimum_counts_characters_not_bytes() {
        let twelve = "\u{e9}".repeat(12);
        let key = read_passphrase_stdin(io::Cursor::new(format!("{twelve}\n").into_bytes()));
        assert_eq!(key.unwrap().as_str(), twelve);
        // Four three-byte characters are twelve bytes but four characters.
        let four = "\u{65e5}".repeat(4);
        assert!(read_passphrase_stdin(io::Cursor::new(four.into_bytes())).is_err());
    }

    #[test]
    fn a_short_typed_key_is_refused_before_asking_again() {
        let mut tty = typed("shortie", "shortie");
        let err = prompt_passphrase(&mut tty).unwrap_err();
        assert!(err.contains("at least 12 characters"), "{err}");
        assert!(!err.contains("shortie"), "{err}");
        assert_eq!(
            tty.asked.len(),
            1,
            "no point repeating a key that is refused"
        );
    }

    #[test]
    fn a_typed_key_of_exactly_the_minimum_is_accepted() {
        let key = prompt_passphrase(&mut typed("abcdefghijkl", "abcdefghijkl")).unwrap();
        assert_eq!(key.as_str(), "abcdefghijkl");
        let err = prompt_passphrase(&mut typed("abcdefghijkl", "abcdefghijkm")).unwrap_err();
        assert!(err.contains("differ"), "{err}");
    }

    #[test]
    fn an_already_stored_short_key_still_works_for_syncing() {
        // The minimum applies when a key is set, never when one is used.
        let secrets = MemorySecretStore::default();
        secrets.set(SYNC_PASSPHRASE_KEY, "old").unwrap();
        assert_eq!(
            sync_passphrase(&secrets, "missing").unwrap().as_str(),
            "old"
        );
    }

    fn store_with(profiles: Vec<Profile>) -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProfileStore::open(&dir.path().join("profiles.json")).unwrap();
        for p in profiles {
            store.upsert(p).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn find_profile_returns_the_exact_match() {
        let web1 = Profile::new("web1", "example.com", "admin");
        let id = web1.id.clone();
        let (_dir, store) = store_with(vec![web1, Profile::new("web2", "example.org", "root")]);
        assert_eq!(find_profile(&store, "web1").unwrap().id, id);
    }

    #[test]
    fn find_profile_reports_an_unknown_name() {
        let (_dir, store) = store_with(vec![Profile::new("Web1", "example.com", "admin")]);
        // Names match exactly, including case.
        let err = find_profile(&store, "web1").unwrap_err();
        assert_eq!(err, "no profile named \"web1\"");
    }

    #[test]
    fn find_profile_sanitizes_the_name_it_reports() {
        let (_dir, store) = store_with(Vec::new());
        let err = find_profile(&store, "a\x1b[2Jb").unwrap_err();
        assert_eq!(err, "no profile named \"a?[2Jb\"");
    }

    #[test]
    fn find_profile_ignores_deleted_profiles() {
        let gone = Profile::new("gone", "example.com", "admin");
        let id = gone.id.clone();
        let (_dir, mut store) = store_with(vec![gone]);
        store.delete(&id).unwrap();
        assert!(find_profile(&store, "gone").is_err());
    }

    #[test]
    fn find_profile_refuses_ambiguous_names() {
        let (_dir, store) = store_with(vec![
            Profile::new("web1", "a.example.com", "admin"),
            Profile::new("web1", "b.example.com", "admin"),
        ]);
        let err = find_profile(&store, "web1").unwrap_err();
        assert!(err.contains("more than one profile"), "{err}");
        assert!(err.contains("unique"), "{err}");
    }

    #[test]
    fn profile_list_text_for_no_profiles() {
        assert_eq!(profile_list_text(&[]), "No profiles.\n");
    }

    #[test]
    fn profile_list_text_has_one_line_per_profile() {
        let mut db = Profile::new("db", "db.example.com", "root");
        db.port = 2222;
        let web = Profile::new("web1", "example.com", "admin");
        assert_eq!(
            profile_list_text(&[&db, &web]),
            "db  root@db.example.com:2222\nweb1  admin@example.com:22\n"
        );
    }

    #[test]
    fn profile_list_text_sanitizes_every_field() {
        let mut p = Profile::new("a\x1bb\nc", "h\x1b.example.com", "u\x07ser");
        p.port = 22;
        let text = profile_list_text(&[&p]);
        assert_eq!(text, "a?b?c  u?ser@h?.example.com:22\n");
    }

    #[test]
    fn sync_passphrase_is_read_from_the_secret_store() {
        let secrets = MemorySecretStore::default();
        secrets.set(SYNC_PASSPHRASE_KEY, "correct horse").unwrap();
        assert_eq!(
            sync_passphrase(&secrets, "missing").unwrap().as_str(),
            "correct horse"
        );
    }

    #[test]
    fn a_missing_or_empty_sync_passphrase_gives_the_callers_message() {
        let secrets = MemorySecretStore::default();
        assert_eq!(sync_passphrase(&secrets, "missing").unwrap_err(), "missing");
        secrets.set(SYNC_PASSPHRASE_KEY, "").unwrap();
        assert_eq!(sync_passphrase(&secrets, "missing").unwrap_err(), "missing");
    }
}
