//! Test-only helper for `scripts/e2e-sync.sh`. Not part of the shipped product.
//!
//! It seeds and inspects one Moniker instance's profiles and secrets so the
//! script can set up and check a sync without going through the GUI. The
//! instance is chosen by the environment, exactly as for the real CLI:
//! `XDG_CONFIG_HOME` picks the profile file and `MONIKER_KEYRING_SERVICE` the
//! keyring namespace.
//!
//! Secrets only ever travel on stdin (`set-secret`) and stdout (`get-secret`,
//! which the script captures in a variable and compares without echoing).
//! Nothing here puts a secret in an argument, an environment variable, a log
//! line or an error message.
//!
//! Profiles are named on the command line for convenience; `get-secret` also
//! takes a profile id, which is the only way to look at a deleted profile.
//! `store-path` and `profile-state` let the script look at the instance
//! through the same code the product uses instead of assuming a file layout.
//! `connection-argv` prints the commands of a connection through a jump host
//! as the product builds them, so the script runs exactly those with real
//! OpenSSH.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use moniker_cli::store_path;
use moniker_core::askpass::AskpassToken;
use moniker_core::jump;
use moniker_core::profile::Profile;
use moniker_core::secrets::{KeyringSecretStore, SecretStore, password_key};
use moniker_core::ssh::SshCommand;
use moniker_core::store::ProfileStore;
use zeroize::Zeroizing;

/// Exit status of `get-secret` when there is no secret (other failures use 2).
const EXIT_ABSENT: u8 = 1;
const EXIT_ERROR: u8 = 2;
/// The longest secret `set-secret` accepts on stdin.
const MAX_SECRET: usize = 64 * 1024;

#[derive(Parser)]
#[command(name = "e2e_helper", about = "Test helper for scripts/e2e-sync.sh")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or replace a profile; prints its id
    SeedProfile {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        user: String,
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Use this id (default: the id of the live profile of that name, else a new one)
        #[arg(long)]
        id: Option<String>,
        /// Go through the live profile of this name
        #[arg(long)]
        jump_host: Option<String>,
    },
    /// Print the commands of a connection through the profile's jump host, one
    /// item per line: `socket <path>` (a new one in $XDG_RUNTIME_DIR), then
    /// `master`, `target`, `check` and `exit`, each followed by `arg <argument>`
    /// lines (the program first) and `env <NAME>=<value>` lines
    ConnectionArgv {
        #[arg(long)]
        profile: String,
        /// The askpass helper the commands point ssh at
        #[arg(long)]
        askpass: PathBuf,
        /// The target's one-time token (none: no stored password)
        #[arg(long)]
        token: Option<String>,
        /// The jump host's one-time token (none: no stored password)
        #[arg(long)]
        jump_token: Option<String>,
    },
    /// Store the password read from stdin for a live profile
    SetSecret { profile: String },
    /// Print the password of a profile (name or id); exit 1 if there is none
    GetSecret { profile: String },
    /// Tombstone a live profile and delete its password
    DeleteProfile { name: String },
    /// Print the names of the live profiles, one per line
    List,
    /// Print the profile file this instance uses (as the product locates it)
    StorePath,
    /// Print `live`, `tombstone` or `absent` for the profile with this id
    ProfileState { id: String },
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("e2e_helper: {message}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(command: Command) -> Result<ExitCode, String> {
    match command {
        Command::SeedProfile {
            name,
            host,
            port,
            user,
            key_file,
            id,
            jump_host,
        } => seed_profile(&name, &host, port, &user, key_file, id, jump_host)
            .map(|()| ExitCode::SUCCESS),
        Command::ConnectionArgv {
            profile,
            askpass,
            token,
            jump_token,
        } => connection_argv(&profile, &askpass, token, jump_token).map(|()| ExitCode::SUCCESS),
        Command::SetSecret { profile } => set_secret(&profile).map(|()| ExitCode::SUCCESS),
        Command::GetSecret { profile } => get_secret(&profile),
        Command::DeleteProfile { name } => delete_profile(&name).map(|()| ExitCode::SUCCESS),
        Command::List => list().map(|()| ExitCode::SUCCESS),
        Command::StorePath => store_path_cmd().map(|()| ExitCode::SUCCESS),
        Command::ProfileState { id } => profile_state(&id).map(|()| ExitCode::SUCCESS),
    }
}

fn open_store() -> Result<ProfileStore, String> {
    let path = store_path()?;
    ProfileStore::open(&path).map_err(|e| format!("{e} ({})", path.display()))
}

/// The id of the one live profile named `name`.
fn live_id_by_name(store: &ProfileStore, name: &str) -> Result<Option<String>, String> {
    let mut ids = store
        .live()
        .into_iter()
        .filter(|p| p.name == name)
        .map(|p| p.id.clone());
    match (ids.next(), ids.next()) {
        (_, Some(_)) => Err(format!("more than one live profile is named {name:?}")),
        (id, None) => Ok(id),
    }
}

fn seed_profile(
    name: &str,
    host: &str,
    port: u16,
    user: &str,
    key_file: Option<PathBuf>,
    id: Option<String>,
    jump_host: Option<String>,
) -> Result<(), String> {
    let mut store = open_store()?;
    let mut profile = Profile::new(name, host, user);
    profile.port = port;
    profile.key_file = key_file;
    if let Some(jump) = jump_host {
        let id = live_id_by_name(&store, &jump)?
            .ok_or_else(|| format!("no live profile {jump:?} to go through"))?;
        profile.jump_host = Some(id);
    }
    // Only look the name up when no id was given (a duplicate name is then
    // an error that does not matter).
    let id = match id {
        Some(id) => Some(id),
        None => live_id_by_name(&store, name)?,
    };
    if let Some(id) = id {
        profile.id = id;
    }
    let id = profile.id.clone();
    store.upsert(profile).map_err(|e| e.to_string())?;
    println!("{id}");
    Ok(())
}

/// Print the commands of a connection to the live profile `name` through its
/// jump host, as the GUI builds them: a new socket in `$XDG_RUNTIME_DIR`
/// (`JumpSocket::new`), then exactly what `jump::pane_commands` gives for it
/// (no startup sequence). A token is given only when a password is stored, as
/// in the GUI; `MONIKER_PID` is this helper's id, and the script puts the id of
/// the process that starts `ssh` in its place.
fn connection_argv(
    name: &str,
    askpass: &Path,
    token: Option<String>,
    jump_token: Option<String>,
) -> Result<(), String> {
    let store = open_store()?;
    let id = live_id_by_name(&store, name)?.ok_or_else(|| format!("no live profile {name:?}"))?;
    let profile = store
        .get(&id)
        .ok_or_else(|| format!("no live profile {name:?}"))?;
    let route = jump::route(profile, |id| store.get(id));
    if let Some(refusal) = route.refusal() {
        return Err(refusal);
    }
    let jump = route
        .jump()
        .ok_or_else(|| format!("{name:?} has no jump host"))?;
    let token = parse_token(token)?;
    let jump_token = parse_token(jump_token)?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let socket = jump::JumpSocket::new(Path::new(&runtime))
        .map_err(|e| format!("cannot make the socket: {e}"))?;
    let cmds = jump::pane_commands(
        profile,
        None,
        jump,
        &socket,
        askpass,
        token.as_ref(),
        jump_token.as_ref(),
    )
    .ok_or("the hop refuses the jump host's user name or host")?;
    let jump::PaneCommands {
        master,
        target,
        check,
        exit,
    } = &cmds;
    let commands = [
        ("master", master),
        ("target", target),
        ("check", check),
        ("exit", exit),
    ];
    let mut text = format!("socket {}\n", socket.path().display());
    for (stage, cmd) in &commands {
        text.push_str(&format!("{stage}\n"));
        push_items(&mut text, cmd)?;
    }
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| format!("cannot write to standard output: {e}"))
}

/// The token `text` names, if any; an error if it is not a canonical UUID.
fn parse_token(text: Option<String>) -> Result<Option<AskpassToken>, String> {
    text.map(|text| {
        AskpassToken::parse(&text).ok_or_else(|| format!("not a canonical UUID: {text:?}"))
    })
    .transpose()
}

/// `arg` lines for the program and each argument of `cmd`, then `env` lines.
/// An item with a newline could not be told from the next one: an error.
fn push_items(text: &mut String, cmd: &SshCommand) -> Result<(), String> {
    let args = std::iter::once(&cmd.program).chain(&cmd.args);
    let env = cmd
        .env
        .iter()
        .map(|(name, value)| format!("{name}={value}"));
    let items = args
        .map(|arg| format!("arg {arg}"))
        .chain(env.map(|var| format!("env {var}")));
    for item in items {
        if item.contains('\n') {
            return Err(format!("an item has a newline: {item:?}"));
        }
        text.push_str(&item);
        text.push('\n');
    }
    Ok(())
}

fn set_secret(name: &str) -> Result<(), String> {
    let store = open_store()?;
    let id = live_id_by_name(&store, name)?.ok_or_else(|| format!("no live profile {name:?}"))?;
    let secret = read_stdin_secret()?;
    KeyringSecretStore::new()
        .set(&password_key(&id), &secret)
        .map_err(|e| format!("cannot store the secret: {e}"))
}

/// The secret on stdin without one trailing newline. Empty is an error.
fn read_stdin_secret() -> Result<Zeroizing<String>, String> {
    // Reserve the whole limit up front so the buffer never reallocates and
    // leaves copies behind; it is wiped when dropped.
    let mut raw = Zeroizing::new(Vec::with_capacity(MAX_SECRET + 1));
    io::stdin()
        .lock()
        .take(MAX_SECRET as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot read standard input: {e}"))?;
    if raw.len() > MAX_SECRET {
        return Err("the secret on standard input is too long".to_owned());
    }
    let text = std::str::from_utf8(&raw).map_err(|_| "the secret is not UTF-8".to_owned())?;
    let secret = text.strip_suffix('\n').unwrap_or(text);
    if secret.is_empty() {
        return Err("the secret must not be empty".to_owned());
    }
    Ok(Zeroizing::new(secret.to_owned()))
}

/// Prints the secret, or exits 1 when there is none. `profile` is a live
/// profile's name or any profile's id (tombstones included).
fn get_secret(profile: &str) -> Result<ExitCode, String> {
    let store = open_store()?;
    let id = match live_id_by_name(&store, profile)? {
        Some(id) => id,
        None if store.all().iter().any(|p| p.id == profile) => profile.to_owned(),
        None => return Err(format!("no profile {profile:?}")),
    };
    let secret = KeyringSecretStore::new()
        .get(&password_key(&id))
        .map_err(|e| format!("cannot read the secret: {e}"))?;
    let Some(secret) = secret else {
        return Ok(ExitCode::from(EXIT_ABSENT));
    };
    let mut out = io::stdout().lock();
    writeln!(out, "{}", secret.as_str())
        .and_then(|()| out.flush())
        .map_err(|e| format!("cannot write to standard output: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

fn delete_profile(name: &str) -> Result<(), String> {
    let mut store = open_store()?;
    let id = live_id_by_name(&store, name)?.ok_or_else(|| format!("no live profile {name:?}"))?;
    store.delete(&id).map_err(|e| e.to_string())?;
    KeyringSecretStore::new()
        .delete(&password_key(&id))
        .map_err(|e| format!("cannot delete the secret: {e}"))
}

fn list() -> Result<(), String> {
    let store = open_store()?;
    let names: String = store
        .live()
        .iter()
        .map(|p| format!("{}\n", p.name))
        .collect();
    print!("{names}");
    io::stdout()
        .flush()
        .map_err(|e| format!("cannot write to standard output: {e}"))
}

fn store_path_cmd() -> Result<(), String> {
    println!("{}", store_path()?.display());
    Ok(())
}

/// `live`, `tombstone` (deleted, record kept) or `absent` (no record at all).
fn profile_state(id: &str) -> Result<(), String> {
    let store = open_store()?;
    let state = match store.all().iter().find(|p| p.id == id) {
        Some(p) if p.deleted => "tombstone",
        Some(_) => "live",
        None => "absent",
    };
    println!("{state}");
    Ok(())
}
