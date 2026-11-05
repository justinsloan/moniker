//! The `moniker-server` command line: list profiles, set the sync key, turn sync
//! on or off, and sync.
//!
//! `main` parses the arguments and calls one small function per subcommand.
//! Each returns `Err(message)` on failure; `main` prints the message to stderr
//! (with control characters made harmless) and exits with status 1.
//!
//! `sync-serve` is the other half of `sync`: `sync` runs `moniker-server sync-serve` on
//! the remote machine over ssh and speaks the sync protocol with it over the
//! ssh channel. Its stdout carries protocol frames and nothing else, so no
//! function on that path may print to stdout.

use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use moniker_cli::{sanitize_for_terminal, store_path};
use moniker_core::askpass::{DevTty, TtyPrompter};
use moniker_core::jump::{self, JumpLeg, JumpSocket, Route};
use moniker_core::profile::Profile;
use moniker_core::secrets::{
    KeyringSecretStore, MissingKeyring, SYNC_PASSPHRASE_KEY, SecretStore, check_sync_passphrase,
    password_key,
};
use moniker_core::ssh::{SERVER_PROGRAM, sync_command};
use moniker_core::store::{ProfileStore, SyncSwitch};
use moniker_core::sync::{
    LocalState, SshTerminal, StaleSync, SyncError, refuse, serve, sync_over_ssh,
};
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

/// What `moniker-server sync-switch on` tells before it asks: the app's "Turn
/// Sync On?" warning, worded for the command line, which cannot keep a profile
/// on this machine itself (that setting is on the profile's page in Moniker).
const TURN_ON_WARNING: &str = "At the next sync, every profile on this machine that syncs, \
    including any added while sync was off, will be copied to your other machines with its stored \
    password. To keep a profile on this machine only, turn off \"Sync to other machines\" on its \
    page in Moniker first.\n";

/// The question `sync-switch on` asks.
const TURN_ON_QUESTION: &str = "Turn sync on? [y/N] ";

/// What `sync-switch on` says when it has nobody to ask and was not told `--yes`.
const NO_TERMINAL_TO_ASK: &str =
    "sync was not turned on: no terminal to ask in; run again with --yes to turn it on";

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
    /// Turn sync on or off on this machine
    SyncSwitch {
        #[command(subcommand)]
        command: SyncSwitchCommand,
    },
    /// Serve one sync on stdin/stdout (run on the remote machine by `moniker-server sync`)
    SyncServe,
    /// Sync with the machine a profile connects to
    Sync {
        /// Sync even if this machine or the other one has not synced for more than 180 days
        /// (a profile deleted elsewhere more than a year ago could then come back)
        #[arg(long)]
        allow_stale: bool,
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

#[derive(Subcommand)]
enum SyncSwitchCommand {
    /// Turn sync on (warns, and asks first when run in a terminal)
    On {
        /// Do not ask: turn sync on and print the warning
        #[arg(long)]
        yes: bool,
    },
    /// Turn sync off: nothing is sent to or accepted from other machines
    Off,
    /// Say whether sync is on
    Status,
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
        Command::SyncSwitch { command } => sync_switch(command),
        Command::SyncServe => sync_serve(),
        Command::Sync {
            profile,
            allow_stale,
        } => sync(&profile, allow_stale),
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

/// The keyring of a command typed at this machine (`sync-key set`, `sync`): a
/// missing default keyring is created, and GNOME asks for its password
/// (issue #18).
fn this_machines_keyring() -> KeyringSecretStore {
    KeyringSecretStore::new(MissingKeyring::Create)
}

/// The keyring `sync-serve` keeps received passwords in. Another machine
/// started it, so a missing default keyring is never created here: no prompt
/// appears on this machine's screen unasked, and the other machine's sync does
/// not wait for one (issue #18).
fn served_keyring() -> KeyringSecretStore {
    KeyringSecretStore::new(MissingKeyring::Refuse)
}

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
    this_machines_keyring()
        .set(SYNC_PASSPHRASE_KEY, &key)
        .map_err(|e| format!("cannot save the sync key: {e}"))?;
    print_stdout("Sync key saved.\n")
}

/// `moniker-server sync-switch on|off|status`. Never touches the keyring.
fn sync_switch(command: SyncSwitchCommand) -> Result<(), String> {
    let path = store_path()?;
    let text = match command {
        SyncSwitchCommand::On { yes } => {
            // Ask only when standard input is a terminal (`--yes` for anything
            // else): a script that pipes into this command has not seen the
            // warning, and /dev/tty may exist behind it all the same.
            let mut dev_tty = DevTty;
            let tty: Option<&mut dyn TtyPrompter> = if !yes && io::stdin().is_terminal() {
                Some(&mut dev_tty)
            } else {
                None
            };
            turn_sync_on(&path, yes, tty)?
        }
        SyncSwitchCommand::Off => turn_sync_off(&path)?,
        SyncSwitchCommand::Status => sync_status(&path)?,
    };
    print_stdout(&text)
}

/// `moniker-server sync-serve`: the remote half of a sync. Stdout is for protocol
/// frames only; everything else goes to stderr.
fn sync_serve() -> Result<(), String> {
    serve_with(
        open_store()?,
        Box::new(served_keyring()),
        io::stdin().lock(),
        io::stdout().lock(),
    )
}

/// One `sync-serve` on `r` and `w`. With sync turned off the HELLO is answered
/// with the refusal (error code 5) before the sync key is looked up: the
/// keyring may be locked, and a machine that takes no part in a sync has no
/// business asking it for anything.
fn serve_with(
    store: ProfileStore,
    secrets: Box<dyn SecretStore>,
    r: impl Read,
    w: impl Write,
) -> Result<(), String> {
    if !store.sync_switch().is_on() {
        let err = refuse(r, w, SyncError::SyncOff)
            .err()
            .unwrap_or(SyncError::SyncOff);
        return Err(err.to_string());
    }
    let passphrase = sync_passphrase(&*secrets, NO_SYNC_KEY_REMOTE)?;
    let mut local = LocalState::new(store, secrets);
    // `serve` has already sent an ERROR frame for anything the peer should hear
    // about; the message here is for whoever reads this machine's stderr.
    serve(r, w, &mut local, &passphrase)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `moniker-server sync [--allow-stale] <profile-name>`: the local half of a
/// sync, over ssh. A partner that goes through a jump host is reached through a
/// shared connection to it, which this sync opens and ends (issue #9).
fn sync(name: &str, allow_stale: bool) -> Result<(), String> {
    // The switch, the profile and its route are looked at before anything
    // touches the keyring.
    let store = open_store()?;
    if !store.sync_switch().is_on() {
        return Err(sync_error_text(&SyncError::SyncOff));
    }
    let profile = find_profile(&store, name)?.clone();
    let jump = partner_jump(&store, &profile)?;

    let secrets = this_machines_keyring();
    let passphrase = sync_passphrase(&secrets, NO_SYNC_KEY_LOCAL)?;
    let has_password = stored_password(&secrets, &profile, "the stored password")?;
    let jump_has_password = match &jump {
        Some(jump) => stored_password(&secrets, jump, "the jump host's stored password")?,
        None => false,
    };

    let current = std::env::current_exe()
        .map_err(|e| format!("cannot locate the moniker-server executable: {e}"))?;
    let askpass = current.with_file_name("moniker-askpass");
    check_sync_askpass(
        &askpass,
        &profile,
        has_password,
        jump.as_ref().map(|jump| (jump, jump_has_password)),
    )?;
    let leg = jump
        .as_ref()
        .map(|jump| jump_leg(jump, runtime_dir().as_deref(), &askpass, jump_has_password))
        .transpose()?;
    let cmd = sync_command(
        &profile,
        &askpass,
        has_password,
        SERVER_PROGRAM,
        leg.as_ref().map(|leg| leg.hop.as_str()),
    );

    let mut local = LocalState::new(store, Box::new(secrets));
    let stale = if allow_stale {
        StaleSync::Allow
    } else {
        StaleSync::Refuse
    };
    let outcome = sync_over_ssh(
        &mut local,
        &cmd,
        leg.as_ref(),
        &passphrase,
        SshTerminal::Inherit,
        stale,
    )
    .map_err(|e| sync_error_text(&e))?;
    print_stdout(&format!(
        "added {}, updated {}, removed {}\n",
        outcome.added, outcome.updated, outcome.removed
    ))
}

// ----- the sync switch -----

/// `sync-switch on`: the text to print. Does nothing, and says so, when sync is
/// already on (the file is not even created). Otherwise the warning is shown
/// first: on `tty` before the question, so that it is read before it is
/// answered, or, with `yes` or no terminal to show it on, at the head of the
/// returned text. Without `yes` and without an answer from a terminal the
/// switch stays as it was.
fn turn_sync_on(
    path: &Path,
    yes: bool,
    tty: Option<&mut dyn TtyPrompter>,
) -> Result<String, String> {
    let mut store = open_store_at(path)?;
    if store.sync_switch() == SyncSwitch::On {
        return Ok("Sync is already on.\n".to_owned());
    }
    let mut text = String::new();
    match (yes, tty) {
        (false, Some(tty)) => {
            if !confirmed(tty)? {
                return Ok("Sync stays off.\n".to_owned());
            }
        }
        (false, None) => return Err(NO_TERMINAL_TO_ASK.to_owned()),
        (true, _) => text.push_str(TURN_ON_WARNING),
    }
    store
        .set_sync(true)
        .map_err(|e| format!("cannot turn sync on: {e} ({})", path.display()))?;
    text.push_str("Sync is on.\n");
    Ok(text)
}

/// Show the warning on the terminal, then ask. Only `y` or `yes`, in any case,
/// is a yes. The end of the input (Ctrl-D) is a no, like Enter: the person is
/// there and did not say yes. A terminal that cannot be reached is no terminal.
fn confirmed(tty: &mut dyn TtyPrompter) -> Result<bool, String> {
    tty.notify(TURN_ON_WARNING)
        .map_err(|_| NO_TERMINAL_TO_ASK.to_owned())?;
    match tty.ask(TURN_ON_QUESTION, true) {
        Ok(answer) => Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            // Ctrl-D leaves the cursor on the prompt's line: end it.
            let _ = tty.notify("");
            Ok(false)
        }
        Err(_) => Err(NO_TERMINAL_TO_ASK.to_owned()),
    }
}

/// `sync-switch off`: the text to print. Off needs no confirmation: it sends
/// nothing.
fn turn_sync_off(path: &Path) -> Result<String, String> {
    open_store_at(path)?
        .set_sync(false)
        .map_err(|e| format!("cannot turn sync off: {e} ({})", path.display()))?;
    Ok("Sync is off. Nothing is sent to or accepted from other machines.\n".to_owned())
}

/// `sync-switch status`: the text to print. A setting that is in the file but
/// cannot be understood counts as off, as it does everywhere else, and the
/// text says why.
fn sync_status(path: &Path) -> Result<String, String> {
    Ok(match open_store_at(path)?.sync_switch() {
        SyncSwitch::On => "Sync is on.\n".to_owned(),
        SyncSwitch::Off => "Sync is off.\n".to_owned(),
        SyncSwitch::Unreadable => format!(
            "Sync is off: the setting in {} could not be read; \
             `moniker-server sync-switch on` turns it on.\n",
            sanitize_for_terminal(&path.display().to_string())
        ),
    })
}

// ----- helpers -----

/// What `moniker-server sync` says about a failed sync: the error, and for a
/// machine that has been away or has sync turned off, how to go ahead anyway;
/// for a jump host whose key is not known yet, what to do about it here.
fn sync_error_text(err: &SyncError) -> String {
    match err {
        SyncError::Stale { .. } | SyncError::PeerStale => {
            format!("{err}; run again with --allow-stale to sync anyway")
        }
        SyncError::SyncOff => {
            format!("{err}; turn it on with `moniker-server sync-switch on` or on the Sync page")
        }
        // The core's text sends the person to a tab of the app, as the GUI
        // words it; a command line has none.
        SyncError::JumpHostKeyUnknown { name } => {
            format!("connect to {name} once with ssh first, so its host key is known")
        }
        _ => err.to_string(),
    }
}

/// The jump host `profile` goes through, if it has one, to sync with it: the
/// route's own text as the error when the jump host is no longer saved or goes
/// through a jump host of its own (Moniker uses one jump host only, and never
/// connects straight to a partner the person routed through one). Decided from
/// the profile file alone, before the keyring is asked for anything.
fn partner_jump(store: &ProfileStore, profile: &Profile) -> Result<Option<Profile>, String> {
    let route = jump::route(profile, |id| store.get(id));
    match route.refusal() {
        Some(why) => Err(why),
        None => Ok(route.jump().cloned()),
    }
}

/// Whether a password is stored for `profile`; `what` names it in the error.
fn stored_password(
    secrets: &dyn SecretStore,
    profile: &Profile,
    what: &str,
) -> Result<bool, String> {
    secrets
        .get(&password_key(&profile.id))
        .map(|password| password.is_some())
        .map_err(|e| format!("cannot look up {what}: {e}"))
}

/// The per-user runtime folder (`XDG_RUNTIME_DIR`), where a jump connection's
/// socket goes.
fn runtime_dir() -> Option<PathBuf> {
    runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// The folder `value` (`XDG_RUNTIME_DIR`) names, if it is an absolute path; the
/// askpass helper ignores anything else too.
fn runtime_dir_from(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|dir| dir.is_absolute())
}

/// The commands of a sync's connection through `jump`: a new socket in
/// `runtime_dir`, which must be a runtime folder private to this user, and the
/// leg made for it, its master answered by the jump host's stored password when
/// `has_password`. Refused, before anything is started, when there is no such
/// folder (`JumpSocket::new` says no, and a socket anywhere else could be
/// reached by others) or when the jump host's user name or host cannot go in
/// the hop's shell string: failing closed, the sync never goes straight to the
/// partner instead.
fn jump_leg(
    jump: &Profile,
    runtime_dir: Option<&Path>,
    askpass: &Path,
    has_password: bool,
) -> Result<JumpLeg, String> {
    let socket = runtime_dir
        .and_then(|dir| JumpSocket::new(dir).ok())
        .ok_or_else(|| {
            format!(
                "cannot connect through the jump host {}: \
                 no private runtime folder for its shared connection",
                jump.name
            )
        })?;
    JumpLeg::for_sync(jump, &socket, askpass, has_password)
        .ok_or_else(|| format!("cannot connect through the jump host {}", jump.name))
}

/// A sync runs with no terminal, so a stored password reaches ssh only through
/// the `moniker-askpass` helper. If one is stored and the helper is not an
/// existing file, say so now, before connecting, instead of failing later with
/// an authentication error. `whose` says which stored password it is.
fn check_askpass(has_password: bool, askpass: &Path, whose: &str) -> Result<(), String> {
    if has_password && !askpass.is_file() {
        Err(format!(
            "moniker-askpass not found next to moniker-server (looked for {}), so {whose} \
             cannot be used for the sync (build with `cargo build --workspace`)",
            askpass.display()
        ))
    } else {
        Ok(())
    }
}

/// [`check_askpass`] for a sync's two logins: the partner's, then the jump
/// host's, each as `(profile, whether a password is stored)`. The error names
/// the profile whose stored password cannot be used.
fn check_sync_askpass(
    askpass: &Path,
    partner: &Profile,
    partner_has_password: bool,
    jump: Option<(&Profile, bool)>,
) -> Result<(), String> {
    let whose = format!("the stored password of {}", partner.name);
    check_askpass(partner_has_password, askpass, &whose)?;
    if let Some((jump, has_password)) = jump {
        let whose = format!("the stored password of the jump host {}", jump.name);
        check_askpass(has_password, askpass, &whose)?;
    }
    Ok(())
}

/// Write `text` to stdout. A closed stdout is an error, not a panic.
fn print_stdout(text: &str) -> Result<(), String> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| format!("cannot write to standard output: {e}"))
}

fn open_store() -> Result<ProfileStore, String> {
    open_store_at(&store_path()?)
}

fn open_store_at(path: &Path) -> Result<ProfileStore, String> {
    ProfileStore::open(path).map_err(|e| format!("{e} ({})", path.display()))
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
/// `No profiles.`. A profile that goes through a jump host (found among
/// `profiles`, which are all the live ones) continues with `  via <its name>`,
/// or `  (jump host no longer saved)` when none is saved; a profile kept on this
/// machine (it never syncs) ends with `  (this machine only)`. Every field is
/// made safe to print.
fn profile_list_text(profiles: &[&Profile]) -> String {
    if profiles.is_empty() {
        return "No profiles.\n".to_owned();
    }
    profiles
        .iter()
        .map(|p| {
            let route = jump::route(p, |id| profiles.iter().copied().find(|q| q.id == id));
            format!(
                "{}  {}@{}:{}{}{}\n",
                sanitize_for_terminal(&p.name),
                sanitize_for_terminal(&p.username),
                sanitize_for_terminal(&p.host),
                p.port,
                match route {
                    Route::Direct => String::new(),
                    Route::Missing => "  (jump host no longer saved)".to_owned(),
                    Route::Through(jump) | Route::Chained(jump) => {
                        format!("  via {}", sanitize_for_terminal(&jump.name))
                    }
                },
                if p.local_only {
                    "  (this machine only)"
                } else {
                    ""
                }
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
    use moniker_core::sync::{KIND_ERROR, KIND_HELLO, read_frame, write_frame};
    use std::io;

    #[test]
    fn only_commands_typed_here_create_a_default_keyring() {
        // Issue #18: `sync-key set` and `sync` are asked for at this machine;
        // `sync-serve` is started by another one.
        assert_eq!(
            this_machines_keyring().missing_keyring(),
            MissingKeyring::Create
        );
        assert_eq!(served_keyring().missing_keyring(), MissingKeyring::Refuse);
    }

    #[test]
    fn sync_takes_allow_stale() {
        let cli = Cli::try_parse_from(["moniker-server", "sync", "--allow-stale", "Box"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Sync { allow_stale: true, ref profile } if profile == "Box"
        ));
        let cli = Cli::try_parse_from(["moniker-server", "sync", "Box"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Sync {
                allow_stale: false,
                ..
            }
        ));
    }

    #[test]
    fn a_machine_that_has_been_away_is_told_how_to_sync_anyway() {
        for err in [SyncError::Stale { days: 200 }, SyncError::PeerStale] {
            let text = sync_error_text(&err);
            assert!(text.starts_with(&err.to_string()), "{text}");
            assert!(
                text.ends_with("run again with --allow-stale to sync anyway"),
                "{text}"
            );
        }
        assert_eq!(
            sync_error_text(&SyncError::WrongSyncKey),
            SyncError::WrongSyncKey.to_string()
        );
    }

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
        /// What was shown without asking (the sync key prompts never do).
        notified: Vec<String>,
    }

    impl ScriptedTty {
        fn new(mut answers: Vec<io::Result<String>>) -> Self {
            answers.reverse();
            ScriptedTty {
                answers,
                asked: Vec::new(),
                notified: Vec::new(),
            }
        }
    }

    impl TtyPrompter for ScriptedTty {
        fn ask(&mut self, prompt: &str, echo: bool) -> io::Result<String> {
            self.asked.push((prompt.to_owned(), echo));
            self.answers.pop().expect("asked more than expected")
        }

        fn notify(&mut self, msg: &str) -> io::Result<()> {
            self.notified.push(msg.to_owned());
            Ok(())
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
        assert!(
            tty.notified.is_empty(),
            "the sync key prompts show nothing else"
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
        let err = check_askpass(true, missing, "the stored password of Db").unwrap_err();
        assert!(
            err.contains("moniker-askpass not found next to moniker-server"),
            "{err}"
        );
        assert!(
            err.contains("/nonexistent/moniker/moniker-askpass"),
            "{err}"
        );
        assert!(
            err.contains("so the stored password of Db cannot be used for the sync"),
            "{err}"
        );
        assert!(err.contains("cargo build --workspace"), "{err}");
        // A directory is not a helper either.
        let dir = tempfile::tempdir().unwrap();
        assert!(check_askpass(true, dir.path(), "x").is_err());
    }

    #[test]
    fn the_helper_is_needed_only_with_a_stored_password_and_only_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("moniker-askpass");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        assert_eq!(check_askpass(true, &helper, "x"), Ok(()));
        assert_eq!(check_askpass(false, &helper, "x"), Ok(()));
        assert_eq!(
            check_askpass(false, Path::new("/nonexistent/x"), "x"),
            Ok(())
        );
    }

    #[test]
    fn a_missing_helper_names_whose_stored_password_cannot_be_used() {
        let missing = Path::new("/nonexistent/moniker/moniker-askpass");
        let db = Profile::new("Db", "db", "alice");
        let bastion = Profile::new("Bastion", "bastion", "admin");

        // The partner's, when it has one (the jump host's is not reached).
        let err = check_sync_askpass(missing, &db, true, Some((&bastion, true))).unwrap_err();
        assert!(err.contains("so the stored password of Db cannot"), "{err}");
        assert!(!err.contains("Bastion"), "{err}");

        // The jump host's, when only it has one.
        let err = check_sync_askpass(missing, &db, false, Some((&bastion, true))).unwrap_err();
        assert!(
            err.contains("so the stored password of the jump host Bastion cannot"),
            "{err}"
        );
        assert!(!err.contains("Db"), "{err}");

        // No stored password, no helper needed.
        assert_eq!(
            check_sync_askpass(missing, &db, false, Some((&bastion, false))),
            Ok(())
        );
        assert_eq!(check_sync_askpass(missing, &db, false, None), Ok(()));
        // And with the helper there, any combination.
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("moniker-askpass");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        assert_eq!(
            check_sync_askpass(&helper, &db, true, Some((&bastion, true))),
            Ok(())
        );
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

    // ----- jump hosts (issue #9) -----

    const OTHER_ID: &str = "00000000-0000-4000-8000-000000000001";

    /// `p`, going through `jump`.
    fn via(p: Profile, jump: &Profile) -> Profile {
        Profile {
            jump_host: Some(jump.id.clone()),
            ..p
        }
    }

    #[test]
    fn profile_list_names_the_jump_host() {
        let bastion = Profile::new("Bastion", "bastion.example.com", "admin");
        let db = via(Profile::new("Db", "db", "alice"), &bastion);
        let lost = Profile {
            jump_host: Some(OTHER_ID.to_owned()),
            ..Profile::new("Lost", "lost", "alice")
        };
        let kept = Profile {
            local_only: true,
            ..via(Profile::new("Kept", "kept", "alice"), &bastion)
        };
        let kept_lost = Profile {
            local_only: true,
            ..Profile {
                jump_host: Some(OTHER_ID.to_owned()),
                ..Profile::new("KeptLost", "kept", "alice")
            }
        };
        let all = [&bastion, &db, &lost, &kept, &kept_lost];
        assert_eq!(
            profile_list_text(&all),
            "Bastion  admin@bastion.example.com:22\n\
             Db  alice@db:22  via Bastion\n\
             Lost  alice@lost:22  (jump host no longer saved)\n\
             Kept  alice@kept:22  via Bastion  (this machine only)\n\
             KeptLost  alice@kept:22  (jump host no longer saved)  (this machine only)\n"
        );
    }

    #[test]
    fn profile_list_makes_the_jump_host_s_name_safe_to_print() {
        let bastion = Profile::new("bas\x1b[2Jtion", "bastion", "admin");
        let db = via(Profile::new("db", "db", "alice"), &bastion);
        assert_eq!(
            profile_list_text(&[&bastion, &db]),
            "bas?[2Jtion  admin@bastion:22\ndb  alice@db:22  via bas?[2Jtion\n"
        );
    }

    #[test]
    fn a_partner_through_a_saved_jump_host_gives_it() {
        let bastion = Profile::new("Bastion", "bastion.example.com", "admin");
        let db = via(Profile::new("Db", "db", "alice"), &bastion);
        let (_dir, store) = store_with(vec![bastion.clone(), db.clone()]);
        assert_eq!(partner_jump(&store, &db).unwrap().unwrap().id, bastion.id);
        // No jump host: straight to the partner.
        assert_eq!(partner_jump(&store, &bastion).unwrap(), None);
    }

    #[test]
    fn a_waiting_or_chained_partner_is_refused_with_the_routes_text() {
        let outer = Profile::new("Outer", "outer", "admin");
        let mid = via(Profile::new("Mid", "mid", "admin"), &outer);
        let db = via(Profile::new("Db", "db", "alice"), &mid);
        let lost = Profile {
            jump_host: Some(OTHER_ID.to_owned()),
            ..Profile::new("Lost", "lost", "alice")
        };
        let (_dir, store) = store_with(vec![outer, mid, db.clone(), lost.clone()]);
        assert_eq!(
            partner_jump(&store, &db).unwrap_err(),
            "Mid goes through a jump host of its own. Moniker uses only one jump host."
        );
        assert_eq!(
            partner_jump(&store, &lost).unwrap_err(),
            "This profile's jump host is no longer saved."
        );
    }

    /// A private runtime folder, short enough for a socket's path.
    fn runtime_folder() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("rt")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn a_jump_connection_needs_a_private_runtime_folder() {
        let bastion = Profile::new("Bastion", "bastion.example.com", "admin");
        let askpass = Path::new("/nonexistent/moniker-askpass");
        let no_folder = "cannot connect through the jump host Bastion: \
                         no private runtime folder for its shared connection";
        assert_eq!(
            jump_leg(&bastion, None, askpass, false).unwrap_err(),
            no_folder
        );
        // Not an absolute path, or no folder at all.
        for bad in ["relative/run", "/nonexistent/moniker-run"] {
            assert_eq!(
                jump_leg(&bastion, Some(Path::new(bad)), askpass, false).unwrap_err(),
                no_folder,
                "{bad}"
            );
        }
        // One that others may write in.
        let open = runtime_folder();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        assert_eq!(
            jump_leg(&bastion, Some(open.path()), askpass, false).unwrap_err(),
            no_folder
        );
    }

    #[test]
    fn a_jump_connection_the_hop_refuses_is_not_made() {
        // A user name the hop's shell string cannot carry (the profile
        // validation keeps it out of a store; a leg must still refuse).
        let folder = runtime_folder();
        let bad = Profile::new("Bastion", "bastion.example.com", "ad min");
        let err = jump_leg(
            &bad,
            Some(folder.path()),
            Path::new("/nonexistent/moniker-askpass"),
            false,
        )
        .unwrap_err();
        assert_eq!(err, "cannot connect through the jump host Bastion");
    }

    #[test]
    fn a_jump_connection_is_made_for_the_jump_host_with_its_password() {
        let folder = runtime_folder();
        let bastion = Profile::new("Bastion", "bastion.example.com", "admin");
        let askpass = Path::new("/usr/lib/moniker/moniker-askpass");
        let leg = jump_leg(&bastion, Some(folder.path()), askpass, true).unwrap();
        assert_eq!(leg.name, "Bastion");
        // The master answers for the jump host's password, not the partner's.
        let env: Vec<_> = leg
            .master
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        assert!(
            env.contains(&("MONIKER_PROFILE_ID", &bastion.id)),
            "{env:?}"
        );
        // The socket is the one the hop and the commands share, in the
        // runtime folder.
        let socket = leg
            .master
            .args
            .iter()
            .find(|arg| arg.starts_with(folder.path().to_str().unwrap()))
            .unwrap()
            .clone();
        assert!(socket.contains("/moniker/jump/"), "{socket}");
        assert!(leg.hop.contains(&socket), "{}", leg.hop);
        // Without a stored password the master has none to ask the helper for.
        let leg = jump_leg(&bastion, Some(folder.path()), askpass, false).unwrap();
        assert!(leg.master.env.is_empty(), "{:?}", leg.master.env);
    }

    #[test]
    fn the_runtime_folder_is_the_one_the_environment_names() {
        assert_eq!(
            runtime_dir_from(Some("/run/user/1000".into())),
            Some(PathBuf::from("/run/user/1000"))
        );
        // The helper ignores anything else too.
        assert_eq!(runtime_dir_from(Some("run".into())), None);
        assert_eq!(runtime_dir_from(Some("".into())), None);
        assert_eq!(runtime_dir_from(None), None);
    }

    // ----- the sync switch (issue #10) -----

    /// A secret store that must never be reached.
    struct NoKeyring;

    type SecretResult<T> = Result<T, moniker_core::secrets::SecretError>;

    impl SecretStore for NoKeyring {
        fn get(&self, key: &str) -> SecretResult<Option<Zeroizing<String>>> {
            panic!("the keyring was read for {key}");
        }
        fn set(&self, key: &str, _value: &str) -> SecretResult<()> {
            panic!("the keyring was written for {key}");
        }
        fn delete(&self, key: &str) -> SecretResult<()> {
            panic!("the keyring was changed for {key}");
        }
        fn probe(&self) -> SecretResult<()> {
            panic!("the keyring was probed");
        }
    }

    /// A path in a fresh directory, with no file there yet.
    fn fresh_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        (dir, path)
    }

    /// The same, with sync turned off in the file.
    fn off_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = fresh_path();
        ProfileStore::open(&path).unwrap().set_sync(false).unwrap();
        (dir, path)
    }

    fn switch_at(path: &Path) -> SyncSwitch {
        ProfileStore::open(path).unwrap().sync_switch()
    }

    fn answers(text: &str) -> ScriptedTty {
        ScriptedTty::new(vec![Ok(text.to_owned())])
    }

    #[test]
    fn profile_list_marks_profiles_kept_here() {
        let kept = Profile {
            local_only: true,
            ..Profile::new("Box", "10.0.0.5", "root")
        };
        let shared = Profile::new("Web", "example.com", "admin");
        assert_eq!(
            profile_list_text(&[&kept, &shared]),
            "Box  root@10.0.0.5:22  (this machine only)\nWeb  admin@example.com:22\n"
        );
    }

    #[test]
    fn sync_switch_parses() {
        let parse = |args: &[&str]| {
            let mut all = vec!["moniker-server", "sync-switch"];
            all.extend_from_slice(args);
            Cli::try_parse_from(all)
        };
        assert!(matches!(
            parse(&["on", "--yes"]).unwrap().command,
            Command::SyncSwitch {
                command: SyncSwitchCommand::On { yes: true }
            }
        ));
        assert!(matches!(
            parse(&["on"]).unwrap().command,
            Command::SyncSwitch {
                command: SyncSwitchCommand::On { yes: false }
            }
        ));
        assert!(matches!(
            parse(&["off"]).unwrap().command,
            Command::SyncSwitch {
                command: SyncSwitchCommand::Off
            }
        ));
        assert!(matches!(
            parse(&["status"]).unwrap().command,
            Command::SyncSwitch {
                command: SyncSwitchCommand::Status
            }
        ));
        // `--yes` belongs to `on` alone, and a word is needed.
        assert!(parse(&["off", "--yes"]).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn turning_sync_on_asks_and_can_be_declined() {
        for no in ["n", "N", "no", "", "  ", "yep", "ye", "maybe"] {
            let (_dir, path) = off_path();
            let mut tty = answers(no);
            let text = turn_sync_on(&path, false, Some(&mut tty)).unwrap();
            assert_eq!(text, "Sync stays off.\n", "answer {no:?}");
            assert_eq!(switch_at(&path), SyncSwitch::Off, "answer {no:?}");
            // Declining is told the warning too: it is read before the question.
            assert_eq!(tty.notified, vec![TURN_ON_WARNING.to_owned()], "{no:?}");
        }

        for yes in ["y", "Y", "yes", "YES", "Yes", " y ", "y\r"] {
            let (_dir, path) = off_path();
            let mut tty = answers(yes);
            let text = turn_sync_on(&path, false, Some(&mut tty)).unwrap();
            assert_eq!(text, "Sync is on.\n", "answer {yes:?}");
            assert_eq!(switch_at(&path), SyncSwitch::On, "answer {yes:?}");
            // The warning is read on the terminal before the question, which
            // is asked with the answer shown as it is typed.
            assert_eq!(tty.notified, vec![TURN_ON_WARNING.to_owned()], "{yes:?}");
            assert_eq!(
                tty.asked,
                vec![("Turn sync on? [y/N] ".to_owned(), true)],
                "{yes:?}"
            );
        }
    }

    #[test]
    fn the_warning_says_what_will_happen() {
        assert_eq!(
            TURN_ON_WARNING,
            "At the next sync, every profile on this machine that syncs, including any added \
             while sync was off, will be copied to your other machines with its stored password. \
             To keep a profile on this machine only, turn off \"Sync to other machines\" on its \
             page in Moniker first.\n"
        );
    }

    #[test]
    fn turning_sync_on_without_a_terminal_needs_yes() {
        let (_dir, path) = off_path();
        let before = std::fs::read(&path).unwrap();
        let err = turn_sync_on(&path, false, None).unwrap_err();
        assert!(err.contains("--yes"), "{err}");
        assert_eq!(
            err,
            "sync was not turned on: no terminal to ask in; run again with --yes to turn it on"
        );
        assert_eq!(switch_at(&path), SyncSwitch::Off);
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

        // With --yes there is nobody to ask, and the warning is in the output.
        let text = turn_sync_on(&path, true, None).unwrap();
        assert_eq!(text, format!("{TURN_ON_WARNING}Sync is on.\n"));
        assert_eq!(switch_at(&path), SyncSwitch::On);
    }

    #[test]
    fn end_of_input_at_the_question_is_a_no() {
        // Ctrl-D at "Turn sync on? [y/N]": the person is there and did not say
        // yes. That is a decline, not a missing terminal.
        let (_dir, path) = off_path();
        let before = std::fs::read(&path).unwrap();
        let mut tty = ScriptedTty::new(vec![Err(io::Error::from(io::ErrorKind::UnexpectedEof))]);
        let text = turn_sync_on(&path, false, Some(&mut tty)).unwrap();
        assert_eq!(text, "Sync stays off.\n");
        assert_eq!(switch_at(&path), SyncSwitch::Off);
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
        assert_eq!(
            tty.asked,
            vec![("Turn sync on? [y/N] ".to_owned(), true)],
            "it did ask"
        );
        // Ctrl-D leaves the cursor on the prompt's line: the line is ended.
        assert_eq!(
            tty.notified,
            vec![TURN_ON_WARNING.to_owned(), String::new()]
        );
    }

    #[test]
    fn a_terminal_that_cannot_be_reached_is_no_terminal() {
        let (_dir, path) = off_path();
        let mut tty = ScriptedTty::new(vec![Err(io::Error::from(io::ErrorKind::NotFound))]);
        let err = turn_sync_on(&path, false, Some(&mut tty)).unwrap_err();
        assert!(err.contains("--yes"), "{err}");
        assert_eq!(switch_at(&path), SyncSwitch::Off);
    }

    #[test]
    fn yes_does_not_ask_even_with_a_terminal() {
        let (_dir, path) = off_path();
        let mut tty = ScriptedTty::new(Vec::new());
        let text = turn_sync_on(&path, true, Some(&mut tty)).unwrap();
        assert_eq!(text, format!("{TURN_ON_WARNING}Sync is on.\n"));
        assert!(tty.asked.is_empty() && tty.notified.is_empty());
        assert_eq!(switch_at(&path), SyncSwitch::On);
    }

    #[test]
    fn turning_sync_on_when_on_changes_nothing() {
        // No file at all reads as on: nothing is asked and nothing is created.
        let (_dir, path) = fresh_path();
        let mut tty = ScriptedTty::new(Vec::new());
        assert_eq!(
            turn_sync_on(&path, false, Some(&mut tty)).unwrap(),
            "Sync is already on.\n"
        );
        assert_eq!(
            turn_sync_on(&path, false, None).unwrap(),
            "Sync is already on.\n"
        );
        assert_eq!(
            turn_sync_on(&path, true, None).unwrap(),
            "Sync is already on.\n"
        );
        assert!(tty.asked.is_empty() && tty.notified.is_empty());
        assert!(!path.exists(), "no file was created");

        // A file that says on is left byte for byte as it is.
        let (_dir, path) = fresh_path();
        let mut store = ProfileStore::open(&path).unwrap();
        store
            .upsert(Profile::new("Box", "10.0.0.5", "root"))
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            turn_sync_on(&path, true, None).unwrap(),
            "Sync is already on.\n"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn turning_sync_on_replaces_a_setting_that_could_not_be_read() {
        let (_dir, path) = fresh_path();
        std::fs::write(&path, r#"{"version":2,"profiles":[]}"#).unwrap();
        assert_eq!(switch_at(&path), SyncSwitch::Unreadable);
        let text = turn_sync_on(&path, true, None).unwrap();
        assert!(text.ends_with("Sync is on.\n"), "{text}");
        assert_eq!(switch_at(&path), SyncSwitch::On);
    }

    #[test]
    fn turning_sync_off_and_asking_how_it_is() {
        let (_dir, path) = fresh_path();
        assert_eq!(sync_status(&path).unwrap(), "Sync is on.\n");
        assert!(!path.exists(), "status reads only");

        let off = "Sync is off. Nothing is sent to or accepted from other machines.\n";
        assert_eq!(turn_sync_off(&path).unwrap(), off);
        assert_eq!(switch_at(&path), SyncSwitch::Off);
        assert_eq!(sync_status(&path).unwrap(), "Sync is off.\n");

        // Again: still off, the same words, the file as it was.
        let before = std::fs::read(&path).unwrap();
        assert_eq!(turn_sync_off(&path).unwrap(), off);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn turning_sync_off_keeps_the_profiles() {
        let (_dir, path) = fresh_path();
        let mut store = ProfileStore::open(&path).unwrap();
        store
            .upsert(Profile::new("Box", "10.0.0.5", "root"))
            .unwrap();
        let all = store.all();
        turn_sync_off(&path).unwrap();
        assert_eq!(ProfileStore::open(&path).unwrap().all(), all);
    }

    #[test]
    fn status_says_when_the_setting_cannot_be_read() {
        let (_dir, path) = fresh_path();
        std::fs::write(&path, r#"{"version":2,"profiles":[]}"#).unwrap();
        let text = sync_status(&path).unwrap();
        assert!(text.starts_with("Sync is off: the setting in "), "{text}");
        assert!(text.contains("could not be read"), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(
            text.ends_with("`moniker-server sync-switch on` turns it on.\n"),
            "{text}"
        );
    }

    #[test]
    fn status_makes_the_path_safe_to_print() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("a\x1b[2Jb");
        std::fs::create_dir(&odd).unwrap();
        let path = odd.join("profiles.json");
        std::fs::write(&path, r#"{"version":2,"profiles":[]}"#).unwrap();
        let text = sync_status(&path).unwrap();
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.contains("a?[2Jb"), "{text:?}");
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_an_error_not_a_guess() {
        let (_dir, path) = fresh_path();
        std::fs::write(&path, "not json").unwrap();
        for err in [
            sync_status(&path).unwrap_err(),
            turn_sync_off(&path).unwrap_err(),
            turn_sync_on(&path, true, None).unwrap_err(),
        ] {
            assert!(err.contains(&path.display().to_string()), "{err}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    /// A HELLO frame as a client sends it.
    fn hello() -> Vec<u8> {
        let mut buf = Vec::new();
        write_frame(&mut buf, KIND_HELLO, &[1]).unwrap();
        buf
    }

    #[test]
    fn sync_serve_refuses_before_reading_the_key() {
        let (_dir, path) = off_path();
        let store = ProfileStore::open(&path).unwrap();
        let mut out = Vec::new();
        let err = serve_with(
            store,
            Box::new(NoKeyring),
            io::Cursor::new(hello()),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err, SyncError::SyncOff.to_string());

        let mut said = io::Cursor::new(&out);
        let (kind, payload) = read_frame(&mut said).unwrap();
        assert_eq!(kind, KIND_ERROR);
        assert_eq!(payload[0], 5, "code 5: sync is turned off");
        let text = String::from_utf8_lossy(&payload[1..]).into_owned();
        assert!(text.contains("turned off on the other machine"), "{text}");
        // That was all it said.
        assert_eq!(said.position() as usize, out.len());
    }

    #[test]
    fn sync_serve_refuses_a_setting_it_cannot_read_too() {
        let (_dir, path) = fresh_path();
        std::fs::write(&path, r#"{"version":2,"profiles":[]}"#).unwrap();
        let store = ProfileStore::open(&path).unwrap();
        let mut out = Vec::new();
        serve_with(
            store,
            Box::new(NoKeyring),
            io::Cursor::new(hello()),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(read_frame(&mut io::Cursor::new(&out)).unwrap().1[0], 5);
    }

    #[test]
    fn sync_serve_with_sync_on_goes_on_to_the_key() {
        // The switch comes first, then the key: with sync on and no key stored
        // the answer is the missing key, not a refusal, and nothing was sent.
        let (_dir, path) = fresh_path();
        let store = ProfileStore::open(&path).unwrap();
        let mut out = Vec::new();
        let err = serve_with(
            store,
            Box::new(MemorySecretStore::default()),
            io::Cursor::new(hello()),
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err, NO_SYNC_KEY_REMOTE);
        assert!(out.is_empty());
    }

    #[test]
    fn a_refusal_needs_a_hello_first() {
        let (_dir, path) = off_path();
        let store = ProfileStore::open(&path).unwrap();
        let mut out = Vec::new();
        let err = serve_with(store, Box::new(NoKeyring), io::empty(), &mut out).unwrap_err();
        assert!(!err.is_empty());
        assert!(
            out.is_empty(),
            "nothing is said to a connection that did not speak"
        );
    }

    #[test]
    fn sync_error_text_says_how_to_turn_sync_on() {
        assert_eq!(
            sync_error_text(&SyncError::SyncOff),
            "sync is turned off on this machine; turn it on with `moniker-server sync-switch on` \
             or on the Sync page"
        );
        // The other machine's refusal is its own business: as it is.
        assert_eq!(
            sync_error_text(&SyncError::PeerSyncOff),
            SyncError::PeerSyncOff.to_string()
        );
    }

    #[test]
    fn an_unknown_jump_host_key_says_to_connect_with_ssh() {
        // The core's text is the GUI's ("in a tab"); a command line has no tab.
        let err = SyncError::JumpHostKeyUnknown {
            name: "Bastion".to_owned(),
        };
        assert_eq!(
            sync_error_text(&err),
            "connect to Bastion once with ssh first, so its host key is known"
        );
        assert!(err.to_string().contains("in a tab"));
        // Another jump failure is the core's text as it is.
        let other = SyncError::JumpFailed {
            name: "Bastion".to_owned(),
            detail: "ssh exited with status 255".to_owned(),
        };
        assert_eq!(sync_error_text(&other), other.to_string());
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
