//! Syncing with another machine: everything but the widgets.
//!
//! The exchange itself is [`moniker_core::sync::sync_over_ssh`], the same call
//! `moniker-server sync` makes, so the CLI and the GUI behave alike. This file
//! adds the texts the GUI shows, the GUI's sync settings logic (which partner
//! automatic sync uses), and, above all, the thread boundary:
//!
//! * What needs the GUI's shared state ([`AppContext`], its `Rc`s, the profile
//!   store) is read on the main thread FIRST, into a plain [`SyncRequest`]
//!   ([`sync_request`]). That is never the keyring: a locked keyring's unlock
//!   prompt would block the main loop, and with it every terminal.
//! * [`start_sync`] moves only that value to a worker thread. The worker first
//!   looks at the profile file itself and stops if sync has been turned off since
//!   the request was made ([`check_switch_is_on`], issue #10), then opens its own
//!   keyring handle and reads the sync key and the stored-password flag
//!   ([`complete_inputs`]), then opens its own profile store ([`run_with`]);
//!   it shares nothing with the main thread.
//! * The result comes back through a one-slot [`async_channel`] that a future on
//!   the GLib main context awaits, so the main loop never blocks and the
//!   callback runs on the main thread.
//!
//! Every sync of the app goes through [`begin_app_sync`]: the sync page's Sync
//! button (`profiles::sync_page`), Sync Now and the sync on launch (`main.rs`).
//! It lets one run at a time and tells whoever listens
//! ([`AppContext::on_sync_event`]) when it starts and ends.
//!
//! A partner that goes through a jump host (issue #9) is synced through it, as
//! `moniker-server sync` does it: the sync signs in to the jump host with an
//! `ssh` of its own, then reaches the partner through that shared connection
//! (`moniker_core::jump::JumpLeg`), and ends it when the sync is over. Where the
//! partner goes is decided from the window's profiles when the sync is asked for
//! ([`sync_request`]) and looked at again on the worker, before the keyring is
//! read and again after ([`check_route_unchanged`]): a partner whose jump host is
//! no longer saved, or goes through one of its own, waits; one whose address was
//! edited since is not signed in to at the old one; and a sync never goes
//! straight to a partner the person routed through a jump host ([`run_with`]
//! refuses that too, as the core sees only argv).
//!
//! The sync key is held in `Zeroizing` buffers and never shown; every message
//! is plain text (profile names come from sync data and may contain markup
//! characters).

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk4 as gtk;
use moniker_core::jump::{self, JumpLeg};
use moniker_core::profile::Profile;
#[cfg(not(test))]
use moniker_core::secrets::KeyringSecretStore;
use moniker_core::secrets::{
    SYNC_PASSPHRASE_KEY, SecretStore, check_sync_passphrase, password_key,
};
use moniker_core::ssh::{SERVER_PROGRAM, SshCommand, sync_command};
use moniker_core::store::{ProfileStore, SyncSwitch};
use moniker_core::sync::{
    LocalState, SshTerminal, StaleSync, SyncError, SyncOutcome, sync_over_ssh,
};
use zeroize::Zeroizing;

use crate::context::AppContext;
use crate::settings::{self, GuiSettings};
use crate::terminal_tab::{jump_socket, no_runtime_folder_note};

/// Everything a sync's exchange needs, completed on the worker from a
/// [`SyncRequest`] and the keyring. No `Debug`: it holds the sync passphrase.
pub(crate) struct SyncInputs {
    pub(crate) profile: Profile,
    pub(crate) has_password: bool,
    /// The jump host the partner goes through, if it has one (issue #9): the
    /// sync signs in to it first, with its own stored password if it has one.
    pub(crate) jump: Option<Profile>,
    /// Whether a password is stored for `jump`.
    pub(crate) jump_has_password: bool,
    pub(crate) passphrase: Zeroizing<String>,
    pub(crate) askpass: PathBuf,
    pub(crate) store_path: PathBuf,
    /// The per-user runtime folder, where the jump connection's socket is made
    /// ([`run_with`]).
    pub(crate) runtime_dir: Option<PathBuf>,
    /// Whether to go ahead when a machine has been away (issue #8).
    pub(crate) stale: StaleSync,
}

/// Whether a sync key is stored.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyState {
    Set,
    NotSet,
    /// The keyring could not be read; the text is the (secret-free) reason.
    Unreadable(String),
}

/// Shown instead of starting a sync when a password is stored for the profile
/// but the `moniker-askpass` helper is not an existing file.
pub(crate) const ASKPASS_MISSING_SYNC: &str = "moniker-askpass not found next to moniker, \
so the stored password cannot be used for the sync (build with `cargo build --workspace`).";

/// Plain, actionable text for a failed sync.
pub(crate) fn explain(err: &SyncError) -> String {
    match err {
        SyncError::WrongSyncKey => "The other machine has a different sync key.".to_owned(),
        SyncError::NoSecretStore => {
            "The other machine has no Secret Service (keyring) available.".to_owned()
        }
        SyncError::ServerNotFound => "moniker-server isn't on the other machine's PATH.".to_owned(),
        SyncError::HostKeyUnknown => {
            "Connect to this host once from a tab to trust its host key, then sync again."
                .to_owned()
        }
        SyncError::VersionMismatch => {
            "The other machine runs an incompatible version of Moniker.".to_owned()
        }
        SyncError::Stale { days } => {
            format!("This machine has not synced for {days} days. {FORGETS}")
        }
        SyncError::PeerStale => {
            format!("The other machine has not synced for more than 180 days. {FORGETS}")
        }
        SyncError::SyncOff => SYNC_IS_OFF.to_owned(),
        SyncError::PeerSyncOff => "Sync is turned off on the other machine.".to_owned(),
        SyncError::PeerTooOld => "The other machine runs an older Moniker, which does not know \
            jump hosts. Update it to sync while a profile has a jump host."
            .to_owned(),
        SyncError::JumpHostKeyUnknown { name } => {
            format!("Connect to {name} once in a tab first, so its host key is known.")
        }
        // The same words as a pane's, for the same reason.
        SyncError::JumpFailed { name, detail } if detail == NO_RUNTIME_FOLDER => {
            no_runtime_folder_note(name)
        }
        // Here `name` is the partner's: the jump host is what is missing.
        SyncError::JumpFailed { name, detail } if detail == NO_JUMP_HOST_GIVEN => {
            format!(
                "{name} goes through a jump host, and this sync was started without it. \
                 Sync again."
            )
        }
        // The jump host's login failed or it did not answer. The detail is
        // ssh's own words (sanitized in core), which can carry the jump host's
        // address and user name: only the name is said, and for a refused login
        // what a sync cannot do.
        SyncError::JumpFailed { name, detail } => {
            let text = format!("Could not connect to the jump host {name}.");
            if detail.contains("Permission denied") {
                format!("{text} {CANNOT_ASK}")
            } else {
                text
            }
        }
        // ssh's login failed. It cannot ask anyone (it has no terminal), so a
        // password that is not stored, or a key that needs its passphrase,
        // ends here too.
        SyncError::Remote(text) if text.contains("Permission denied") => {
            format!("{err} {CANNOT_ASK}")
        }
        // Timeout and everything else: core's own text, which carries no
        // passphrase, password or profile data (text from the other side has
        // been sanitized there).
        other => other.to_string(),
    }
}

/// Why a sync with a machine that has been away asks first (issue #8).
const FORGETS: &str = "Moniker forgets a deletion a year after it is made, so a profile deleted \
    on another machine more than a year ago could come back on every machine.";

/// Whether `failure` is a sync that stopped because this machine or the other
/// one has been away, which the person may choose to sync anyway (issue #8).
pub(crate) fn is_away(failure: &SyncFailure) -> bool {
    matches!(
        failure,
        SyncFailure::Sync(SyncError::Stale { .. } | SyncError::PeerStale)
    )
}

/// What a successful sync changed, in one line.
pub(crate) fn outcome_text(outcome: &SyncOutcome) -> String {
    if *outcome == SyncOutcome::default() {
        "Already up to date.".to_owned()
    } else {
        format!(
            "Synced: {} added, {} updated, {} removed.",
            outcome.added, outcome.updated, outcome.removed
        )
    }
}

/// Why Sync is unavailable right now, or `None` if it is available. The
/// switch comes first: it is the one reason that stops a sync even when
/// everything else is set up (issue #10).
pub(crate) fn sync_hint(sync_on: bool, key_set: bool, profiles: usize) -> Option<&'static str> {
    if !sync_on {
        Some(SYNC_IS_OFF)
    } else if !key_set {
        Some(NO_SYNC_KEY)
    } else if profiles == 0 {
        Some("Add a profile for the other machine first (+ above the list of profiles).")
    } else {
        None
    }
}

/// What the sync page's switch row says under its title, for what the profile
/// file says (issue #10). Both unavailable states read as off: a switch nobody
/// could read stops every sync, as the core treats it, and turning sync on is
/// what replaces it.
pub(crate) fn switch_subtitle(switch: SyncSwitch) -> &'static str {
    match switch {
        SyncSwitch::On => "Send and accept profiles from your other machines",
        SyncSwitch::Off => "Off: nothing is sent to or accepted from other machines",
        SyncSwitch::Unreadable => "Off: the setting could not be read. Turn sync on to replace it.",
    }
}

/// Whether a sync key is stored. An empty stored value counts as none, as in
/// the CLI.
pub(crate) fn key_state(secrets: &dyn SecretStore) -> KeyState {
    match secrets.get(SYNC_PASSPHRASE_KEY) {
        Ok(Some(key)) if !key.is_empty() => KeyState::Set,
        Ok(_) => KeyState::NotSet,
        Err(err) => KeyState::Unreadable(err.to_string()),
    }
}

/// The status line under "Sync key". Never contains the key.
pub(crate) fn key_status_text(state: &KeyState) -> String {
    match state {
        KeyState::Set => "Sync key: set".to_owned(),
        KeyState::NotSet => "Sync key: not set".to_owned(),
        KeyState::Unreadable(why) => {
            format!("Sync key: not set (could not read the keyring: {why})")
        }
    }
}

/// The status line under "Sync key" while the keyring is being read.
pub(crate) const KEY_CHECKING: &str = "Sync key: checking\u{2026}";

/// Whether `typed` may be stored as the sync key: it must have at least
/// [`moniker_core::secrets::MIN_SYNC_PASSPHRASE_CHARS`] characters (a key that is
/// already stored with fewer keeps working). `Err` is a message for the sync
/// page's inline line and never contains the key.
pub(crate) fn check_sync_key(typed: &str) -> Result<(), String> {
    check_sync_passphrase(typed).map_err(sentence)
}

/// Store `typed` as the sync key, exactly as typed (no trimming), once
/// [`check_sync_key`] accepts it. `Err` is a message for the sync page's inline
/// line and never contains the key.
pub(crate) fn save_sync_key(secrets: &dyn SecretStore, typed: &str) -> Result<(), String> {
    check_sync_key(typed)?;
    secrets
        .set(SYNC_PASSPHRASE_KEY, typed)
        .map_err(|err| format!("Could not save the sync key: {err}"))
}

/// Remove the sync key from the keyring (the Sync page's Forget, issue #11),
/// leaving every other entry, the profiles' passwords included. A key that is
/// not there is not an error. `Err` is a message for a notification and never
/// contains the key.
pub(crate) fn remove_sync_key(secrets: &dyn SecretStore) -> Result<(), String> {
    secrets
        .delete(SYNC_PASSPHRASE_KEY)
        .map_err(|err| format!("Could not remove the sync key: {err}"))
}

/// `message` as a sentence for the sync page: first letter capitalized, period at the end.
fn sentence(message: &str) -> String {
    let mut chars = message.chars();
    let first: String = chars
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    format!("{first}{}.", chars.as_str())
}

/// The partner automatic sync should sync with on launch: the recorded
/// partner, if sync is turned on, automatic sync is on and that profile still
/// exists (a sync may have deleted it). `None`: no automatic sync. With sync
/// turned off (or a switch that cannot be read) there is none, and the launch
/// says nothing: the person turned it off on purpose (issue #10).
pub(crate) fn auto_sync_target(settings: &GuiSettings, store: &ProfileStore) -> Option<String> {
    if !settings.auto_sync || !store.sync_switch().is_on() {
        return None;
    }
    live_partner(settings, store)
}

/// The recorded partner, if that profile still exists. Sync Now is shown
/// only while there is one (grayed out while sync is turned off).
pub(crate) fn live_partner(settings: &GuiSettings, store: &ProfileStore) -> Option<String> {
    let partner = settings.sync_partner.as_deref()?;
    store.get(partner).map(|p| p.id.clone())
}

/// The settings after a sync started by hand with `partner_id` succeeded: that
/// profile is the partner from now on. The switch stays as it was: a sync by
/// hand never turns automatic sync back on after the user turned it off.
pub(crate) fn after_manual_success(settings: &GuiSettings, partner_id: &str) -> GuiSettings {
    GuiSettings {
        sync_partner: Some(partner_id.to_owned()),
        ..settings.clone()
    }
}

/// What the main thread hands a sync's worker: what needs the profile store
/// or the GUI, and nothing from the keyring. The worker completes it into
/// [`SyncInputs`] ([`complete_inputs`]), reading the keyring there, so a
/// locked keyring (whose unlock prompt blocks the caller) never blocks the
/// GUI.
pub(crate) struct SyncRequest {
    pub(crate) profile: Profile,
    /// The jump host the partner goes through, as the window's profiles have it
    /// (issue #9); `None` when the partner has none.
    pub(crate) jump: Option<Profile>,
    pub(crate) askpass: PathBuf,
    pub(crate) store_path: PathBuf,
    /// The per-user runtime folder for the jump connection's socket.
    pub(crate) runtime_dir: Option<PathBuf>,
    pub(crate) stale: StaleSync,
}

/// On the main thread: the profile to sync with, the jump host it goes through
/// if it has one, and where the worker finds the profile file, the askpass
/// helper and (for a jump host's socket) the runtime folder `runtime_dir`.
/// Reads no secret.
///
/// Refused while sync is turned off, here and now, so that no worker starts and
/// the keyring is not read for a sync that cannot run (issue #10). This is the
/// snapshot's word, which can be a moment behind the file: the worker looks at
/// the file itself before it reads the keyring ([`check_switch_is_on`]), and the
/// core refuses again before anything is sent, which is what protects the data.
///
/// Refused too, with the route's own text, when the partner's jump host is no
/// longer saved or goes through a jump host of its own (issue #9): such a partner
/// waits, and is never synced straight in its place. Nothing starts and nothing
/// is read from the keyring, as for a sync that is turned off.
pub(crate) fn sync_request(
    store: &ProfileStore,
    askpass: &Path,
    runtime_dir: Option<&Path>,
    profile_id: &str,
) -> Result<SyncRequest, String> {
    if !store.sync_switch().is_on() {
        return Err(SYNC_IS_OFF.to_owned());
    }
    // The worker opens the file again by this path.
    let store_path = store.path().to_path_buf();
    if !store_path.is_absolute() {
        return Err(format!(
            "The profile file path is not absolute ({}), so Moniker will not sync.",
            store_path.display()
        ));
    }
    let profile = store
        .get(profile_id)
        .cloned()
        .ok_or_else(|| "That profile no longer exists.".to_owned())?;
    let route = jump::route(&profile, |id| store.get(id));
    if let Some(refusal) = route.refusal() {
        return Err(refusal);
    }
    let jump = route.jump().cloned();
    Ok(SyncRequest {
        profile,
        jump,
        askpass: askpass.to_path_buf(),
        store_path,
        runtime_dir: runtime_dir.map(Path::to_path_buf),
        stale: StaleSync::Refuse,
    })
}

/// On the worker, before anything is read from the keyring: whether the profile
/// file, as it is now, still has sync turned on (issue #10). The request was made
/// on the main thread from its snapshot of the file, which another process (the
/// command line) can have outdated: the file watch reloads the snapshot only
/// after its delay, and later when a reload fails. A sync that started in that
/// while stops here, so the sync key and the partner's password stay in the
/// keyring, and the person is told as for any refusal. Fails closed like the
/// core: a switch that cannot be read counts as off, and a file that cannot be
/// opened is a store error, as [`run_with`] would say.
pub(crate) fn check_switch_is_on(store_path: &Path) -> Result<(), SyncFailure> {
    if open_store(store_path)?.sync_switch().is_on() {
        Ok(())
    } else {
        Err(SyncFailure::Sync(SyncError::SyncOff))
    }
}

/// What a sync says when its partner was given another jump host, or had its
/// jump host taken off or put on, since the sync was asked for.
pub(crate) const JUMP_CHANGED: &str = "This profile's jump host changed just now. Sync again.";

/// On the worker: whether the partner still goes where the request said (issue
/// #9), by the profile file as it is now. The request was made from the main
/// thread's snapshot, which another process or a sync can have outdated, and
/// the keyring read that follows can wait at an unlock prompt for as long as
/// the person takes. Nothing in the core protects against a partner synced
/// the wrong way, so this is asked before the keyring is read and again after
/// it, just before the exchange. `partner` is the profile the request was made
/// for, `asked` the jump host it carried (and the stored password looked up is
/// for).
///
/// The route is worked out from the partner as the file has it (as the request
/// had it if the file no longer has it). It stops the sync, with the route's own
/// text, when the jump host is no longer saved or goes through one of its own;
/// and with [`JUMP_CHANGED`] when it is a different jump host from `asked`, or
/// the same one with another host, port, user name or key file, or there was
/// none or is none now: never straight to the partner, never to an address the
/// person did not choose, and never with the password of another jump host. A
/// file that cannot be opened is a store error, as for [`check_switch_is_on`].
pub(crate) fn check_route_unchanged(
    store_path: &Path,
    partner: &Profile,
    asked: Option<&Profile>,
) -> Result<(), SyncFailure> {
    let store = open_store(store_path)?;
    let now = store.get(&partner.id).unwrap_or(partner);
    let route = jump::route(now, |id| store.get(id));
    if let Some(refusal) = route.refusal() {
        return Err(SyncFailure::NotStarted(refusal));
    }
    let same = match (route.jump(), asked) {
        (None, None) => true,
        (Some(now), Some(asked)) => same_connection(now, asked),
        _ => false,
    };
    if same {
        Ok(())
    } else {
        Err(SyncFailure::NotStarted(JUMP_CHANGED.to_owned()))
    }
}

/// Whether `now` and `asked` are one jump host that connects the same way: the
/// leg of a sync is built from `asked`, and the stored password looked up is
/// for its id, so a host, port, user name or key file edited since (a sync that
/// landed while this one waited at the keyring, a snapshot not yet reloaded)
/// would send the master, and the password, where the person did not choose.
/// The name is not part of the connection.
fn same_connection(now: &Profile, asked: &Profile) -> bool {
    now.id == asked.id
        && now.host == asked.host
        && now.port == asked.port
        && now.username == asked.username
        && now.key_file == asked.key_file
}

/// The profile file, for a check on the worker. The error text carries no file
/// contents (`StoreError`).
fn open_store(store_path: &Path) -> Result<ProfileStore, SyncFailure> {
    ProfileStore::open(store_path)
        .map_err(|err| SyncFailure::Sync(SyncError::Store(err.to_string())))
}

/// On the worker: complete `request` from `secrets`, in the CLI's order: the
/// sync key, then whether the partner has a stored password and, with a jump
/// host, whether it has (a keyring that cannot say counts as none), and then,
/// if either has, that the askpass helper exists. The `Err` text never holds
/// the key.
pub(crate) fn complete_inputs(
    request: SyncRequest,
    secrets: &dyn SecretStore,
) -> Result<SyncInputs, SyncFailure> {
    let passphrase = match secrets.get(SYNC_PASSPHRASE_KEY) {
        Ok(Some(key)) if !key.is_empty() => key,
        Ok(_) => return Err(SyncFailure::NoSyncKey),
        Err(err) => {
            return Err(SyncFailure::NotStarted(format!(
                "Could not read the sync key: {err}"
            )));
        }
    };
    // A keyring error here means "no stored password": ssh then asks in the
    // terminal, which for a sync means the sync fails with ssh's message. The
    // same for the jump host's password, whose ssh is asked about it first.
    let stored = |profile: &Profile| matches!(secrets.get(&password_key(&profile.id)), Ok(Some(_)));
    let has_password = stored(&request.profile);
    let jump_has_password = request.jump.as_ref().is_some_and(stored);
    // A sync has no terminal to ask in: with a stored password but no helper to
    // answer ssh, say so now rather than fail with an authentication error. The
    // partner's is looked at first, as the command line does.
    if !request.askpass.is_file() {
        if has_password {
            return Err(SyncFailure::NotStarted(ASKPASS_MISSING_SYNC.to_owned()));
        }
        if let (true, Some(jump)) = (jump_has_password, &request.jump) {
            let why = askpass_missing_for_jump(&jump.name);
            return Err(SyncFailure::NotStarted(why));
        }
    }
    Ok(SyncInputs {
        profile: request.profile,
        has_password,
        jump: request.jump,
        jump_has_password,
        passphrase,
        askpass: request.askpass,
        store_path: request.store_path,
        runtime_dir: request.runtime_dir,
        stale: request.stale,
    })
}

/// [`ASKPASS_MISSING_SYNC`] for the stored password of the jump host `name`.
fn askpass_missing_for_jump(name: &str) -> String {
    format!(
        "moniker-askpass not found next to moniker, so the stored password of the jump host \
         {name} cannot be used for the sync (build with `cargo build --workspace`)."
    )
}

/// Why a sync that had started (its worker ran) did not succeed.
#[derive(Debug)]
pub(crate) enum SyncFailure {
    /// No sync key is stored (or an empty one).
    NoSyncKey,
    /// The worker could not get as far as connecting; the text says why.
    NotStarted(String),
    /// The exchange failed.
    Sync(SyncError),
}

/// What a sync of the app ends with.
pub(crate) type SyncResult = Result<SyncOutcome, SyncFailure>;

/// Plain, actionable text for a failed sync. Never holds the key.
pub(crate) fn failure_text(failure: &SyncFailure) -> String {
    match failure {
        SyncFailure::NoSyncKey => NO_SYNC_KEY.to_owned(),
        SyncFailure::NotStarted(why) => why.clone(),
        SyncFailure::Sync(err) => explain(err),
    }
}

/// What to do when ssh's login failed during a sync, which has no terminal to
/// ask on.
const CANNOT_ASK: &str = "A sync cannot ask for a password or a key passphrase: store the \
profile's password, or add the key to ssh-agent.";

/// Why a sync cannot run without a key.
pub(crate) const NO_SYNC_KEY: &str = "Set a sync key first.";

/// Why no sync runs while the switch is off (issue #10): the sync page's hint,
/// the refusal of a request, and the core's own error ([`explain`]).
pub(crate) const SYNC_IS_OFF: &str = "Sync is turned off on this machine.";

/// What a [`SyncError::JumpFailed`] says (as its detail) when no private runtime
/// folder holds the socket of the jump connection ([`jump_leg`]).
const NO_RUNTIME_FOLDER: &str = "there is no private runtime folder for its shared connection";

/// What a [`SyncError::JumpFailed`] says (as its detail, with the partner's name)
/// when the worker was given a partner that goes through a jump host and no jump
/// host to go through ([`run_with`]).
const NO_JUMP_HOST_GIVEN: &str = "the sync was started without the jump host";

/// The worker's whole job: open this thread's own store and secret store, build
/// the ssh command, and run the exchange. `tweak` may change the command before
/// it runs (the real runner changes nothing; tests swap in a fake `ssh`).
///
/// With a jump host (issue #9) the sync goes through a shared connection to it:
/// a new socket in the runtime folder, the commands that sign in to the jump host
/// and end the connection ([`jump_leg`]), and the partner's command taking the
/// hop through that socket as its `ProxyCommand`. `tweak` is applied to each of
/// them (the target's and the leg's three), and must leave their arguments
/// alone: the core refuses a sync whose command does not carry the leg's hop.
pub(crate) fn run_with(
    inputs: SyncInputs,
    secrets: Box<dyn SecretStore>,
    mut tweak: impl FnMut(&mut SshCommand),
) -> Result<SyncOutcome, SyncError> {
    // The core sees only argv: a partner the person routed through a jump host
    // and given no jump host here would be synced straight, a connection that
    // way was never chosen. `sync_request` and the checks on the worker make
    // this unreachable; it stays as the last line.
    if inputs.profile.jump_host.is_some() && inputs.jump.is_none() {
        return Err(SyncError::JumpFailed {
            name: inputs.profile.name.clone(),
            detail: NO_JUMP_HOST_GIVEN.to_owned(),
        });
    }
    // StoreError messages carry no file contents.
    let store =
        ProfileStore::open(&inputs.store_path).map_err(|e| SyncError::Store(e.to_string()))?;
    let mut local = LocalState::new(store, secrets);
    let mut leg = inputs
        .jump
        .as_ref()
        .map(|jump| jump_leg(&inputs, jump))
        .transpose()?;
    let mut cmd = sync_command(
        &inputs.profile,
        &inputs.askpass,
        inputs.has_password,
        SERVER_PROGRAM,
        leg.as_ref().map(|leg| leg.hop.as_str()),
    );
    tweak(&mut cmd);
    if let Some(leg) = &mut leg {
        for command in [&mut leg.master, &mut leg.check, &mut leg.exit] {
            tweak(command);
        }
    }
    // In a session of its own: a question ssh would ask on a terminal fails at
    // once instead of waiting in the terminal Moniker was started from. The
    // jump host's ssh too.
    sync_over_ssh(
        &mut local,
        &cmd,
        leg.as_ref(),
        &inputs.passphrase,
        SshTerminal::Detached,
        inputs.stale,
    )
}

/// The commands of the sync's connection through `jump`: a new socket in the
/// runtime folder and the leg made for it, its master answered by the jump
/// host's stored password when there is one. Refused, before anything is
/// started, when there is no private runtime folder (`jump_socket` says why on
/// standard error: a socket anywhere else could be reached by others) or when
/// the jump host's user name or host cannot go in the hop's command: the sync
/// never goes straight to the partner in its place.
fn jump_leg(inputs: &SyncInputs, jump: &Profile) -> Result<JumpLeg, SyncError> {
    let failed = |detail: &str| SyncError::JumpFailed {
        name: jump.name.clone(),
        detail: detail.to_owned(),
    };
    let socket = jump_socket(inputs.runtime_dir.as_deref(), &jump.name)
        .ok_or_else(|| failed(NO_RUNTIME_FOLDER))?;
    JumpLeg::for_sync(jump, &socket, &inputs.askpass, inputs.jump_has_password)
        .ok_or_else(|| failed("its user name or host cannot be used in the connection command"))
}

// ----- the worker thread -----

/// Run `work` on a worker thread and call `done` with its result on the main
/// thread, from the GLib main loop. Never blocks the caller.
///
/// Only `work` crosses to the worker (it is `Send`); `done` stays on the main
/// thread and may hold GTK objects. If the worker panics or cannot be started,
/// `done` gets an error instead. The thread is never canceled: if whatever
/// `done` belongs to is gone by then, `done` simply finds nothing to update.
///
/// Must be called from the thread that runs the main loop.
pub(crate) fn start_sync(
    work: impl FnOnce() -> SyncResult + Send + 'static,
    done: impl FnOnce(SyncResult) + 'static,
) {
    let (tx, rx) = async_channel::bounded::<SyncResult>(1);
    let spawned = std::thread::Builder::new()
        .name("moniker-sync".to_owned())
        .spawn(move || {
            // The key read there is dropped, and wiped, inside `work`. A
            // closed channel means the main loop is gone.
            let _ = tx.send_blocking(work());
        });
    if let Err(err) = spawned {
        done(Err(SyncFailure::Sync(SyncError::Io(err))));
        return;
    }
    // The handle is dropped: the task is detached and runs to completion.
    glib::MainContext::default().spawn_local(async move {
        let result = rx.recv().await.unwrap_or_else(|_| {
            // The sender went away without a result: the worker panicked.
            Err(SyncFailure::Sync(SyncError::Io(io::Error::other(
                "the sync stopped unexpectedly",
            ))))
        });
        done(result);
    });
}

/// The real runner: this thread's own profile store and keyring handle, the
/// system `ssh`, and `moniker-server` on the other machine's PATH.
#[cfg(not(test))]
fn run_sync(inputs: SyncInputs) -> Result<SyncOutcome, SyncError> {
    run_with(inputs, Box::new(KeyringSecretStore::new()), |_| {})
}

/// A sync's worker function, as [`runner`] gives it.
pub(crate) type Runner = Box<dyn FnOnce(SyncInputs) -> Result<SyncOutcome, SyncError> + Send>;

/// The worker function every sync of the app runs: the real one.
#[cfg(not(test))]
pub(crate) fn runner(_ctx: &AppContext) -> Runner {
    Box::new(run_sync)
}

/// The worker function every sync of the app runs. In tests it is whatever
/// the test put in [`AppContext::sync_runner`], and a test that forgot is
/// stopped here: no test ever runs the real `ssh` or touches the keyring.
#[cfg(test)]
pub(crate) fn runner(ctx: &AppContext) -> Runner {
    let make = ctx.sync_runner.borrow();
    let make = make
        .as_ref()
        .expect("a test that starts a sync must set ctx.sync_runner");
    make()
}

/// Opens, on the worker, the secret store a sync reads the sync key and the
/// stored-password flag from.
pub(crate) type OpenSecrets = Arc<dyn Fn() -> Box<dyn SecretStore> + Send + Sync>;

/// The secret store a sync's worker reads: its own handle on the system
/// keyring.
#[cfg(not(test))]
fn worker_secrets(_ctx: &AppContext) -> OpenSecrets {
    Arc::new(|| Box::new(KeyringSecretStore::new()))
}

/// The secret store a sync's worker reads. In tests it is whatever the test
/// put in [`AppContext::sync_secrets`] (an in-memory store); a test that
/// forgot is stopped here, so no test reads the real keyring.
#[cfg(test)]
fn worker_secrets(ctx: &AppContext) -> OpenSecrets {
    let open = ctx.sync_secrets.borrow();
    Arc::clone(
        open.as_ref()
            .expect("a test that starts a sync must set ctx.sync_secrets"),
    )
}

// ----- one sync at a time, for the whole app -----

/// Why a sync did not start: one is running already.
pub(crate) const ALREADY_RUNNING: &str = "A sync is already running.";

/// Shown while a sync runs.
pub(crate) const SYNCING: &str = "Syncing\u{2026} (can take up to two minutes)";

/// What [`AppContext::on_sync_event`] listeners hear.
pub(crate) enum SyncEvent<'a> {
    /// A sync has started; [`AppContext::sync_running`] is set.
    Started,
    /// The sync has ended with this result; the flag is clear again and, after
    /// a success, the profiles are already reloaded.
    Finished(&'a SyncResult),
}

/// Start a sync with the profile `partner_id`, run by `run` on a worker thread;
/// `stale` says whether it goes ahead when a machine has been away (issue #8).
///
/// `Err` (a plain sentence) if it does not start: [`ALREADY_RUNNING`] while
/// another sync of this app runs, [`SYNC_IS_OFF`] while sync is turned off or a
/// request to turn it off is still being saved ([`AppContext::sync_is_on`]),
/// or what is wrong with the profile (gone, its file path not absolute, or its
/// jump host not saved or itself going through one: the route's text, issue #9).
/// Nothing here reads the keyring: the worker reads the sync key and the
/// stored-password flags ([`complete_inputs`]), after it has looked at the
/// profile file ([`check_switch_is_on`], [`check_route_unchanged`]), and what it
/// finds missing (no key, no askpass helper, an unreadable keyring, sync turned
/// off or the route changed meanwhile) comes back as the sync's result, like any
/// failure of the exchange.
///
/// Once started, listeners hear [`SyncEvent::Started`]. When the worker is
/// done (on the main thread): the running flag is cleared, a success reloads
/// the profiles for everyone ([`AppContext::profiles_changed`]), `done` gets
/// the result, and then listeners hear [`SyncEvent::Finished`].
pub(crate) fn begin_app_sync<R>(
    ctx: &Rc<AppContext>,
    partner_id: &str,
    stale: StaleSync,
    run: R,
    done: impl FnOnce(&SyncResult) + 'static,
) -> Result<(), String>
where
    R: FnOnce(SyncInputs) -> Result<SyncOutcome, SyncError> + Send + 'static,
{
    if ctx.sync_running.get() {
        return Err(ALREADY_RUNNING.to_owned());
    }
    // A request to turn sync off counts before its save has landed in the file.
    if !ctx.sync_is_on() {
        return Err(SYNC_IS_OFF.to_owned());
    }
    // Bind first so the store borrow ends here.
    let mut request = {
        let store = ctx.store.borrow();
        sync_request(
            &store,
            &ctx.askpass_path,
            ctx.runtime_dir.as_deref(),
            partner_id,
        )
    }?;
    request.stale = stale;
    let open_secrets = worker_secrets(ctx);
    ctx.sync_running.set(true);
    ctx.sync_event(&SyncEvent::Started);
    let ctx = Rc::clone(ctx);
    let work = move || {
        check_switch_is_on(&request.store_path)?;
        check_route_unchanged(&request.store_path, &request.profile, request.jump.as_ref())?;
        let secrets = open_secrets();
        let inputs = complete_inputs(request, secrets.as_ref())?;
        // The keyring may have waited at an unlock prompt for as long as the
        // person took: the jump host can have been deleted or changed meanwhile.
        check_route_unchanged(&inputs.store_path, &inputs.profile, inputs.jump.as_ref())?;
        run(inputs).map_err(SyncFailure::Sync)
    };
    start_sync(work, move |result| {
        ctx.sync_running.set(false);
        // The profile file changed: everything that shows profiles reloads.
        if result.is_ok() {
            ctx.profiles_changed();
        }
        done(&result);
        ctx.sync_event(&SyncEvent::Finished(&result));
    });
    Ok(())
}

/// A sync's result in one plain sentence, and whether it is an error.
pub(crate) fn result_text(result: &SyncResult) -> (String, bool) {
    match result {
        Ok(outcome) => (outcome_text(outcome), false),
        Err(failure) => (failure_text(failure), true),
    }
}

/// A sync started by hand with `partner_id` succeeded: record that profile as
/// the partner in the settings file (the switch keeps its value).
pub(crate) fn record_partner(ctx: &AppContext, partner_id: &str) -> io::Result<()> {
    let settings = settings::load(&ctx.settings_path);
    settings::save(
        &ctx.settings_path,
        &after_manual_success(&settings, partner_id),
    )
}

/// Forget the sync partner (issue #11): clear `sync_partner` in the settings
/// file, so there is no Sync Now and no sync at launch until a sync from the
/// sync page succeeds again ([`record_partner`]). Every other setting stays as
/// the file has it, the automatic-sync switch included.
///
/// This goes through [`settings::update`], not [`settings::save`]: a file that
/// exists but cannot be read or parsed is left as it is (an error), where
/// `save` would replace it with the defaults, and a file with no partner is not
/// written at all (`Ok`).
pub(crate) fn forget_partner(path: &Path) -> io::Result<()> {
    settings::update(path, |settings| settings.sync_partner = None)
}

// ----- texts of the sync page -----

/// The title of the empty sync key field.
pub(crate) const KEY_PLACEHOLDER: &str = "New sync key (at least 12 characters)";

/// The explanation of the "Sync key" group.
pub(crate) const KEY_NOTE: &str = "Use the same sync key on every machine you sync, at least 12 characters \
    long (a few random words make a good one). Passwords are encrypted with it before \
    they leave this machine. It is kept in your system keyring and is never shown.";

#[cfg(test)]
mod tests {
    use super::*;
    use moniker_core::secrets::{MemorySecretStore, SecretError};
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;

    const KEY: &str = "  correct horse  battery staple  ";
    /// Stands in for any passphrase or password that must never be shown.
    const SENTINEL: &str = "SENTINEL-s3cret-passphrase";

    fn outcome(added: usize, updated: usize, removed: usize) -> SyncOutcome {
        SyncOutcome {
            added,
            updated,
            removed,
        }
    }

    // ----- explain -----

    #[test]
    fn wrong_sync_key_says_the_other_machine_differs() {
        assert_eq!(
            explain(&SyncError::WrongSyncKey),
            "The other machine has a different sync key."
        );
    }

    #[test]
    fn no_secret_store_says_no_keyring() {
        assert_eq!(
            explain(&SyncError::NoSecretStore),
            "The other machine has no Secret Service (keyring) available."
        );
    }

    #[test]
    fn server_not_found_says_path() {
        assert_eq!(
            explain(&SyncError::ServerNotFound),
            "moniker-server isn't on the other machine's PATH."
        );
    }

    #[test]
    fn unknown_host_key_says_connect_once() {
        assert_eq!(
            explain(&SyncError::HostKeyUnknown),
            "Connect to this host once from a tab to trust its host key, then sync again."
        );
    }

    #[test]
    fn version_mismatch_says_incompatible() {
        assert_eq!(
            explain(&SyncError::VersionMismatch),
            "The other machine runs an incompatible version of Moniker."
        );
    }

    #[test]
    fn timeout_keeps_the_core_message() {
        assert_eq!(explain(&SyncError::Timeout), SyncError::Timeout.to_string());
        assert!(explain(&SyncError::Timeout).contains("did not finish in time"));
    }

    #[test]
    fn other_errors_show_the_core_text() {
        let others = [
            SyncError::FrameTooLarge,
            SyncError::EmptyPassphrase,
            SyncError::Protocol("expected message type 2, got 7".to_owned()),
            SyncError::Remote("ssh exited with status 255: Connection refused".to_owned()),
            SyncError::Store("the profile file is corrupt".to_owned()),
            SyncError::Io(io::Error::other("could not start ssh: not found")),
        ];
        for err in &others {
            assert_eq!(explain(err), err.to_string());
        }
    }

    #[test]
    fn fixed_messages_never_contain_a_secret() {
        // These variants carry no text, so nothing from outside can reach the
        // message: it must be exactly the fixed sentence, never the sentinel.
        let fixed = [
            SyncError::WrongSyncKey,
            SyncError::NoSecretStore,
            SyncError::ServerNotFound,
            SyncError::HostKeyUnknown,
            SyncError::VersionMismatch,
            SyncError::Timeout,
            SyncError::FrameTooLarge,
            SyncError::EmptyPassphrase,
        ];
        for err in &fixed {
            assert!(!explain(err).contains(SENTINEL), "{err:?}");
            assert!(!explain(err).contains(KEY.trim()), "{err:?}");
        }
    }

    #[test]
    fn only_core_sanitized_text_is_passed_through() {
        // Core puts the text from the other side into Remote and Protocol (it
        // strips control characters first). That text, and only that text, is
        // shown; the mapped variants above ignore any payload by construction.
        for err in [
            SyncError::Remote(format!("ssh said {SENTINEL}")),
            SyncError::Protocol(format!("odd frame {SENTINEL}")),
        ] {
            assert!(explain(&err).contains(SENTINEL));
            assert_eq!(explain(&err), err.to_string());
        }
    }

    #[test]
    fn explain_jump_errors() {
        assert_eq!(
            explain(&SyncError::PeerTooOld),
            "The other machine runs an older Moniker, which does not know jump hosts. \
             Update it to sync while a profile has a jump host."
        );
        assert_eq!(
            explain(&SyncError::JumpHostKeyUnknown {
                name: "Bastion".to_owned()
            }),
            "Connect to Bastion once in a tab first, so its host key is known."
        );
        // A jump host that did not answer: its name, not ssh's words (which
        // can carry its address and user name).
        let failed = SyncError::JumpFailed {
            name: "Bastion".to_owned(),
            detail: format!("ssh exited with status 255: bob@bastion: Refused {SENTINEL}"),
        };
        assert_eq!(
            explain(&failed),
            "Could not connect to the jump host Bastion."
        );
        // A refused login: a sync cannot ask, so the hint follows.
        let refused = SyncError::JumpFailed {
            name: "Bastion".to_owned(),
            detail: "ssh exited with status 255: bob@bastion: Permission denied \
                     (publickey,password)."
                .to_owned(),
        };
        assert_eq!(
            explain(&refused),
            "Could not connect to the jump host Bastion. A sync cannot ask for a password or \
             a key passphrase: store the profile's password, or add the key to ssh-agent."
        );
        // The name is plain text, whatever characters it has.
        let markup = SyncError::JumpFailed {
            name: "<b>x</b> &amp;".to_owned(),
            detail: String::new(),
        };
        assert_eq!(
            explain(&markup),
            "Could not connect to the jump host <b>x</b> &amp;."
        );
        // No private folder for the socket: said as a pane says it.
        let no_folder = SyncError::JumpFailed {
            name: "Bastion".to_owned(),
            detail: NO_RUNTIME_FOLDER.to_owned(),
        };
        assert_eq!(
            explain(&no_folder),
            "Could not connect through the jump host Bastion: there is no private runtime \
             folder for its shared connection."
        );
        assert_eq!(
            failure_text(&SyncFailure::Sync(failed)),
            "Could not connect to the jump host Bastion."
        );
    }

    // ----- outcome_text -----

    #[test]
    fn nothing_changed_is_up_to_date() {
        assert_eq!(outcome_text(&outcome(0, 0, 0)), "Already up to date.");
    }

    #[test]
    fn counts_are_listed() {
        assert_eq!(
            outcome_text(&outcome(1, 2, 3)),
            "Synced: 1 added, 2 updated, 3 removed."
        );
        assert_eq!(
            outcome_text(&outcome(0, 0, 1)),
            "Synced: 0 added, 0 updated, 1 removed."
        );
        assert_eq!(
            outcome_text(&outcome(10, 0, 0)),
            "Synced: 10 added, 0 updated, 0 removed."
        );
    }

    // ----- sync_hint -----

    #[test]
    fn sync_is_available_with_a_key_and_a_profile() {
        assert_eq!(sync_hint(true, true, 1), None);
        assert_eq!(sync_hint(true, true, 5), None);
    }

    #[test]
    fn a_missing_key_comes_first() {
        assert_eq!(sync_hint(true, false, 3), Some("Set a sync key first."));
        assert_eq!(sync_hint(true, false, 0), Some("Set a sync key first."));
    }

    #[test]
    fn no_profiles_asks_for_one() {
        let hint = sync_hint(true, true, 0).expect("a hint");
        assert!(hint.contains("profile"), "{hint}");
    }

    #[test]
    fn sync_off_is_the_first_reason_sync_is_unavailable() {
        // Whatever else is missing, the switch is what the person can act on
        // first, and the one thing that stops a sync even when all is set up.
        assert_eq!(sync_hint(false, true, 3), Some(SYNC_IS_OFF));
        assert_eq!(sync_hint(false, false, 0), Some(SYNC_IS_OFF));
        assert_eq!(sync_hint(true, false, 3), Some(NO_SYNC_KEY));
        assert_eq!(SYNC_IS_OFF, "Sync is turned off on this machine.");
    }

    #[test]
    fn the_switch_row_says_what_each_state_means() {
        assert_eq!(
            switch_subtitle(SyncSwitch::On),
            "Send and accept profiles from your other machines"
        );
        assert_eq!(
            switch_subtitle(SyncSwitch::Off),
            "Off: nothing is sent to or accepted from other machines"
        );
        // A switch nobody could read counts as off, and says how to mend it.
        assert_eq!(
            switch_subtitle(SyncSwitch::Unreadable),
            "Off: the setting could not be read. Turn sync on to replace it."
        );
    }

    #[test]
    fn the_two_off_errors_are_explained() {
        assert_eq!(explain(&SyncError::SyncOff), SYNC_IS_OFF);
        assert_eq!(
            explain(&SyncError::PeerSyncOff),
            "Sync is turned off on the other machine."
        );
        assert_eq!(
            failure_text(&SyncFailure::Sync(SyncError::SyncOff)),
            SYNC_IS_OFF
        );
    }

    // ----- key status -----

    /// A store whose every operation fails with a message that has no secret.
    struct Broken;

    impl SecretStore for Broken {
        fn get(&self, _: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            Err(SecretError::Unavailable("no bus".to_owned()))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretError> {
            Err(SecretError::Backend("keyring is locked".to_owned()))
        }
        fn delete(&self, _: &str) -> Result<(), SecretError> {
            Err(SecretError::Backend("keyring is locked".to_owned()))
        }
        fn probe(&self) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no bus".to_owned()))
        }
    }

    #[test]
    fn key_state_follows_the_keyring() {
        let secrets = MemorySecretStore::default();
        assert_eq!(key_state(&secrets), KeyState::NotSet);
        secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        assert_eq!(key_state(&secrets), KeyState::Set);
        // An empty stored value counts as none, as in the CLI.
        secrets.set(SYNC_PASSPHRASE_KEY, "").unwrap();
        assert_eq!(key_state(&secrets), KeyState::NotSet);
    }

    #[test]
    fn an_unreadable_keyring_is_reported_not_hidden() {
        match key_state(&Broken) {
            KeyState::Unreadable(why) => assert!(why.contains("no bus"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn status_text_never_shows_the_value() {
        assert_eq!(key_status_text(&KeyState::Set), "Sync key: set");
        assert_eq!(key_status_text(&KeyState::NotSet), "Sync key: not set");
        let text = key_status_text(&KeyState::Unreadable("no bus".to_owned()));
        assert!(text.starts_with("Sync key: not set"), "{text}");
        assert!(text.contains("no bus"), "{text}");
    }

    // ----- save_sync_key -----

    #[test]
    fn saving_stores_the_key_exactly_as_typed() {
        let secrets = MemorySecretStore::default();
        save_sync_key(&secrets, KEY).unwrap();
        let stored = secrets.get(SYNC_PASSPHRASE_KEY).unwrap().unwrap();
        assert_eq!(stored.as_str(), KEY, "no trimming");
    }

    #[test]
    fn an_empty_key_is_rejected_and_stores_nothing() {
        let secrets = MemorySecretStore::default();
        secrets.set(SYNC_PASSPHRASE_KEY, "old key").unwrap();
        let err = save_sync_key(&secrets, "").unwrap_err();
        assert_eq!(err, "The sync key must not be empty.");
        let stored = secrets.get(SYNC_PASSPHRASE_KEY).unwrap().unwrap();
        assert_eq!(stored.as_str(), "old key", "the old key is untouched");
    }

    #[test]
    fn a_key_of_only_spaces_is_a_key() {
        // Twelve of them: as typed, spaces count like any other character.
        let secrets = MemorySecretStore::default();
        save_sync_key(&secrets, &" ".repeat(12)).unwrap();
        assert_eq!(key_state(&secrets), KeyState::Set);
    }

    #[test]
    fn a_short_key_is_rejected_and_stores_nothing() {
        let secrets = MemorySecretStore::default();
        secrets.set(SYNC_PASSPHRASE_KEY, "old key").unwrap();
        let err = save_sync_key(&secrets, "tooshort").unwrap_err();
        assert_eq!(err, "The sync key must be at least 12 characters long.");
        assert!(!err.contains("tooshort"), "{err}");
        let stored = secrets.get(SYNC_PASSPHRASE_KEY).unwrap().unwrap();
        assert_eq!(stored.as_str(), "old key", "the old key is untouched");
        // Nothing is stored either when there was no key before.
        let fresh = MemorySecretStore::default();
        assert!(save_sync_key(&fresh, "eleven char").is_err());
        assert_eq!(key_state(&fresh), KeyState::NotSet);
    }

    #[test]
    fn a_key_of_exactly_the_minimum_is_accepted() {
        let secrets = MemorySecretStore::default();
        save_sync_key(&secrets, "twelve chars").unwrap();
        let stored = secrets.get(SYNC_PASSPHRASE_KEY).unwrap().unwrap();
        assert_eq!(stored.as_str(), "twelve chars");
    }

    #[test]
    fn the_minimum_counts_characters_not_bytes() {
        let secrets = MemorySecretStore::default();
        // Twelve accented letters are 24 bytes and 12 characters.
        save_sync_key(&secrets, &"\u{e9}".repeat(12)).unwrap();
        // Four kanji are 12 bytes but 4 characters.
        let err = save_sync_key(&secrets, &"\u{65e5}".repeat(4)).unwrap_err();
        assert!(err.contains("at least 12"), "{err}");
    }

    #[test]
    fn the_key_field_and_note_mention_the_minimum() {
        assert!(KEY_PLACEHOLDER.contains("12"), "{KEY_PLACEHOLDER}");
        assert!(KEY_NOTE.contains("at least 12 characters"), "{KEY_NOTE}");
    }

    #[test]
    fn a_keyring_failure_is_reported_without_the_key() {
        let err = save_sync_key(&Broken, SENTINEL).unwrap_err();
        assert!(err.starts_with("Could not save the sync key"), "{err}");
        assert!(err.contains("keyring is locked"), "{err}");
        assert!(!err.contains(SENTINEL), "{err}");
    }

    // ----- remove_sync_key -----

    #[test]
    fn removing_the_key_leaves_the_passwords() {
        let secrets = MemorySecretStore::default();
        secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        secrets.set(&password_key("id-a"), SENTINEL).unwrap();
        remove_sync_key(&secrets).unwrap();
        assert_eq!(key_state(&secrets), KeyState::NotSet);
        let password = secrets.get(&password_key("id-a")).unwrap().unwrap();
        assert_eq!(password.as_str(), SENTINEL, "a profile's password stays");
    }

    #[test]
    fn removing_a_key_that_is_not_there_is_not_an_error() {
        let secrets = MemorySecretStore::default();
        remove_sync_key(&secrets).unwrap();
        assert_eq!(key_state(&secrets), KeyState::NotSet);
    }

    #[test]
    fn a_keyring_that_cannot_remove_the_key_says_so() {
        let err = remove_sync_key(&Broken).unwrap_err();
        assert!(err.starts_with("Could not remove the sync key: "), "{err}");
        assert!(err.contains("keyring is locked"), "{err}");
    }

    // ----- gather_inputs -----

    struct World {
        _dir: tempfile::TempDir,
        store: ProfileStore,
        secrets: MemorySecretStore,
        profile: Profile,
        /// An existing file standing in for the `moniker-askpass` helper.
        askpass: PathBuf,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProfileStore::open(&dir.path().join("profiles.json")).unwrap();
        let profile = Profile::new("Prod <web>", "app.example.test", "alice");
        store.upsert(profile.clone()).unwrap();
        let profile = store.get(&profile.id).unwrap().clone();
        let askpass = dir.path().join("moniker-askpass");
        fs::write(&askpass, "#!/bin/sh\n").unwrap();
        World {
            _dir: dir,
            store,
            secrets: MemorySecretStore::default(),
            profile,
            askpass,
        }
    }

    /// Both halves, as a sync runs them: the request on the main thread,
    /// then the keyring part on the worker.
    fn gather_inputs(
        store: &ProfileStore,
        secrets: &dyn SecretStore,
        askpass: &Path,
        profile_id: &str,
    ) -> Result<SyncInputs, String> {
        let request = sync_request(store, askpass, None, profile_id)?;
        complete_inputs(request, secrets).map_err(|failure| failure_text(&failure))
    }

    #[test]
    fn a_missing_key_is_its_own_failure_and_never_shows_a_key() {
        let w = world();
        let request = || sync_request(&w.store, &w.askpass, None, &w.profile.id).unwrap();
        assert!(matches!(
            complete_inputs(request(), &w.secrets),
            Err(SyncFailure::NoSyncKey)
        ));
        assert_eq!(
            failure_text(&SyncFailure::NoSyncKey),
            "Set a sync key first."
        );
        // An unreadable keyring: the reason, never a key.
        match complete_inputs(request(), &Broken) {
            Err(failure @ SyncFailure::NotStarted(_)) => {
                let text = failure_text(&failure);
                assert!(text.starts_with("Could not read the sync key"), "{text}");
            }
            other => panic!("{:?}", other.err()),
        }
        // The exchange's own failures read as before.
        assert_eq!(
            failure_text(&SyncFailure::Sync(SyncError::WrongSyncKey)),
            explain(&SyncError::WrongSyncKey)
        );
    }

    fn gather(w: &World, id: &str) -> Result<SyncInputs, String> {
        gather_inputs(&w.store, &w.secrets, &w.askpass, id)
    }

    #[test]
    fn inputs_carry_everything_the_worker_needs() {
        let w = world();
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        w.secrets
            .set(&password_key(&w.profile.id), "hunter2")
            .unwrap();
        let Ok(inputs) = gather(&w, &w.profile.id) else {
            panic!("gathering failed");
        };
        assert_eq!(inputs.profile, w.profile);
        assert!(inputs.has_password);
        assert_eq!(inputs.passphrase.as_str(), KEY, "exactly as stored");
        assert_eq!(inputs.askpass, w.askpass);
        assert_eq!(inputs.store_path, w.store.path());
        assert!(inputs.store_path.is_absolute());
    }

    #[test]
    fn a_profile_without_a_stored_password_says_so() {
        let w = world();
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        let Ok(inputs) = gather(&w, &w.profile.id) else {
            panic!("gathering failed");
        };
        assert!(!inputs.has_password);
    }

    #[test]
    fn a_stored_password_without_the_helper_stops_before_connecting() {
        let w = world();
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        w.secrets
            .set(&password_key(&w.profile.id), "hunter2")
            .unwrap();
        let missing = Path::new("/nonexistent/moniker/moniker-askpass");
        let err = gather_inputs(&w.store, &w.secrets, missing, &w.profile.id)
            .err()
            .expect("an error");
        assert_eq!(err, ASKPASS_MISSING_SYNC);
        assert!(
            err.contains("moniker-askpass not found next to moniker"),
            "{err}"
        );
        assert!(err.contains("cargo build --workspace"), "{err}");
        assert!(
            !err.contains("hunter2") && !err.contains(KEY.trim()),
            "{err}"
        );

        // A directory is not a helper.
        let dir = w.store.path().parent().unwrap();
        assert_eq!(
            gather_inputs(&w.store, &w.secrets, dir, &w.profile.id)
                .err()
                .as_deref(),
            Some(ASKPASS_MISSING_SYNC)
        );
    }

    #[test]
    fn without_a_stored_password_the_helper_is_not_needed() {
        let w = world();
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        let missing = Path::new("/nonexistent/moniker/moniker-askpass");
        let Ok(inputs) = gather_inputs(&w.store, &w.secrets, missing, &w.profile.id) else {
            panic!("gathering failed");
        };
        assert!(!inputs.has_password);
    }

    #[test]
    fn a_missing_key_asks_for_one() {
        let w = world();
        assert_eq!(
            gather(&w, &w.profile.id).err().as_deref(),
            Some("Set a sync key first.")
        );
        // An empty stored key counts as none.
        w.secrets.set(SYNC_PASSPHRASE_KEY, "").unwrap();
        assert_eq!(
            gather(&w, &w.profile.id).err().as_deref(),
            Some("Set a sync key first.")
        );
    }

    #[test]
    fn an_unreadable_keyring_stops_before_the_sync() {
        let w = world();
        let err = gather_inputs(&w.store, &Broken, Path::new("/a"), &w.profile.id)
            .err()
            .expect("an error");
        assert!(err.starts_with("Could not read the sync key"), "{err}");
    }

    #[test]
    fn an_unknown_or_deleted_profile_is_refused() {
        let mut w = world();
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        assert_eq!(
            gather(&w, "00000000-0000-4000-8000-000000000000")
                .err()
                .as_deref(),
            Some("That profile no longer exists.")
        );
        w.store.delete(&w.profile.id).unwrap();
        assert_eq!(
            gather(&w, &w.profile.id).err().as_deref(),
            Some("That profile no longer exists.")
        );
    }

    #[test]
    fn a_relative_store_path_is_refused() {
        let w = world();
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        // A missing file is an empty store and nothing is written.
        let relative = ProfileStore::open(Path::new("no/such/dir/profiles.json")).unwrap();
        let err = gather_inputs(&relative, &w.secrets, Path::new("/a"), &w.profile.id)
            .err()
            .expect("an error");
        assert!(err.contains("absolute"), "{err}");
    }

    // ----- automatic sync: the target, and recording the partner -----

    fn settings(partner: Option<&str>, auto_sync: bool) -> GuiSettings {
        GuiSettings {
            sync_partner: partner.map(str::to_owned),
            auto_sync,
            ..GuiSettings::default()
        }
    }

    #[test]
    fn no_partner_no_auto_sync() {
        let w = world();
        assert_eq!(auto_sync_target(&settings(None, true), &w.store), None);
        assert_eq!(auto_sync_target(&GuiSettings::default(), &w.store), None);
    }

    #[test]
    fn auto_sync_off_no_target() {
        let w = world();
        assert_eq!(
            auto_sync_target(&settings(Some(&w.profile.id), false), &w.store),
            None
        );
    }

    #[test]
    fn a_deleted_partner_is_no_target() {
        let mut w = world();
        let partner = settings(Some(&w.profile.id), true);
        w.store.delete(&w.profile.id).unwrap();
        assert_eq!(auto_sync_target(&partner, &w.store), None, "tombstoned");
        // Never known here at all (the file came from elsewhere, or is stale).
        let unknown = settings(Some("00000000-0000-4000-8000-000000000000"), true);
        assert_eq!(auto_sync_target(&unknown, &w.store), None);
        assert_eq!(auto_sync_target(&settings(Some(""), true), &w.store), None);
    }

    #[test]
    fn a_live_partner_is_the_target() {
        let w = world();
        assert_eq!(
            auto_sync_target(&settings(Some(&w.profile.id), true), &w.store),
            Some(w.profile.id.clone())
        );
    }

    #[test]
    fn an_off_switch_has_no_automatic_sync_target() {
        let mut w = world();
        let partner = settings(Some(&w.profile.id), true);
        w.store.set_sync(false).unwrap();
        assert_eq!(auto_sync_target(&partner, &w.store), None, "off");
        // The partner is still there: Sync Now is shown, grayed out.
        assert_eq!(live_partner(&partner, &w.store), Some(w.profile.id.clone()));
        // No request, so no worker and no keyring read.
        let err = sync_request(&w.store, &w.askpass, None, &w.profile.id)
            .err()
            .expect("refused");
        assert_eq!(err, SYNC_IS_OFF);
        // Back on: both work again.
        w.store.set_sync(true).unwrap();
        assert_eq!(
            auto_sync_target(&partner, &w.store),
            Some(w.profile.id.clone())
        );
        assert!(sync_request(&w.store, &w.askpass, None, &w.profile.id).is_ok());
    }

    #[test]
    fn a_switch_nobody_can_read_counts_as_off() {
        // Fail closed, as the core does: a version-2 file whose switch is
        // missing or not a boolean.
        let w = world();
        let partner = settings(Some(&w.profile.id), true);
        fs::write(w.store.path(), r#"{"version":2,"profiles":[]}"#).unwrap();
        let unreadable = ProfileStore::open(w.store.path()).unwrap();
        assert_eq!(auto_sync_target(&partner, &unreadable), None);
        let err = sync_request(&unreadable, &w.askpass, None, &w.profile.id);
        assert_eq!(err.err().as_deref(), Some(SYNC_IS_OFF));
    }

    #[test]
    fn the_worker_looks_at_the_file_not_at_the_main_threads_snapshot() {
        let mut w = world();
        // On, as the file says.
        assert!(check_switch_is_on(w.store.path()).is_ok());
        // Turned off through another handle: `w.store` is a stale snapshot, as
        // the main thread's is until the file watch reloads it.
        ProfileStore::open(w.store.path())
            .unwrap()
            .set_sync(false)
            .unwrap();
        assert!(w.store.sync_switch().is_on(), "stale");
        assert!(sync_request(&w.store, &w.askpass, None, &w.profile.id).is_ok());
        match check_switch_is_on(w.store.path()) {
            Err(SyncFailure::Sync(SyncError::SyncOff)) => {}
            other => panic!("{:?}", other.err()),
        }
        // Back on, and a switch nobody can read counts as off.
        w.store.set_sync(true).unwrap();
        assert!(check_switch_is_on(w.store.path()).is_ok());
        fs::write(w.store.path(), r#"{"version":2,"profiles":[]}"#).unwrap();
        assert!(matches!(
            check_switch_is_on(w.store.path()),
            Err(SyncFailure::Sync(SyncError::SyncOff))
        ));
    }

    #[test]
    fn a_profile_file_that_cannot_be_opened_stops_the_worker_without_quoting_it() {
        let w = world();
        fs::write(w.store.path(), "{ not json SENTINEL").unwrap();
        match check_switch_is_on(w.store.path()) {
            Err(SyncFailure::Sync(SyncError::Store(message))) => {
                assert!(!message.contains("SENTINEL"), "{message}");
            }
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn manual_success_records_the_partner_and_keeps_the_switch() {
        let off = after_manual_success(&settings(None, false), "id-a");
        assert_eq!(
            off,
            settings(Some("id-a"), false),
            "a switch turned off stays off"
        );
        let on = after_manual_success(&GuiSettings::default(), "id-a");
        assert_eq!(on, settings(Some("id-a"), true));
        // A sync by hand with another machine makes that one the partner.
        assert_eq!(
            after_manual_success(&on, "id-b"),
            settings(Some("id-b"), true)
        );
    }

    // ----- forgetting the partner -----

    #[test]
    fn forget_partner_clears_only_the_partner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(settings::FILE_NAME);
        // A file a hand edit has put other settings in.
        let before = GuiSettings {
            sync_partner: Some("id-a".to_owned()),
            auto_sync: false,
            font_scale: 1.5,
            show_sessions: false,
            scrollback_lines: 20_000,
            font: Some("Monospace 11".to_owned()),
            ping_hosts: false,
            main_window: Some(settings::WindowSize {
                width: 1000,
                height: 600,
                maximized: true,
            }),
            ..GuiSettings::default()
        };
        settings::save(&path, &before).unwrap();

        forget_partner(&path).unwrap();

        assert_eq!(
            settings::load(&path),
            GuiSettings {
                sync_partner: None,
                ..before
            }
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn forget_partner_leaves_a_damaged_file_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(settings::FILE_NAME);
        for bytes in [&b"{ not json"[..], b"[1, 2]", b""] {
            fs::write(&path, bytes).unwrap();
            assert!(forget_partner(&path).is_err(), "{bytes:?}");
            assert_eq!(fs::read(&path).unwrap(), bytes, "left as it was");
        }
    }

    #[test]
    fn forget_partner_without_a_partner_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(settings::FILE_NAME);
        forget_partner(&path).unwrap();
        assert!(!path.exists(), "no file, and none made");

        let text = br#"{"auto_sync": false}"#;
        fs::write(&path, text).unwrap();
        forget_partner(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), text, "unchanged");
    }

    // ----- run_with: the real worker flow, with a fake ssh -----

    /// A fake `ssh`: records its arguments, the askpass variables and whether
    /// it leads a session of its own (`SESSION-LEADER`) to `record`, then exits
    /// 127 like a shell that cannot find `moniker-server`.
    fn fake_ssh(dir: &Path) -> PathBuf {
        let path = dir.join("fake-ssh");
        let record = dir.join("record");
        let script = format!(
            "#!/bin/sh\n\
             {{\n\
             for a in \"$@\"; do printf 'ARG:%s\\n' \"$a\"; done\n\
             printf 'SSH_ASKPASS=%s\\n' \"${{SSH_ASKPASS-unset}}\"\n\
             printf 'MONIKER_PROFILE_ID=%s\\n' \"${{MONIKER_PROFILE_ID-unset}}\"\n\
             set -- $(cat /proc/$$/stat)\n\
             [ \"$1\" = \"$6\" ] && echo SESSION-LEADER\n\
             }} > '{}'\n\
             exit 127\n",
            record.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn inputs_for(w: &World, has_password: bool) -> SyncInputs {
        SyncInputs {
            profile: w.profile.clone(),
            has_password,
            jump: None,
            jump_has_password: false,
            passphrase: Zeroizing::new(KEY.to_owned()),
            askpass: w.askpass.clone(),
            store_path: w.store.path().to_path_buf(),
            runtime_dir: None,
            stale: StaleSync::Refuse,
        }
    }

    #[test]
    fn the_worker_builds_the_sync_command_from_the_inputs() {
        let w = world();
        let fake = fake_ssh(w.store.path().parent().unwrap());
        let result = run_with(
            inputs_for(&w, true),
            Box::new(MemorySecretStore::default()),
            |cmd| cmd.program = fake.to_str().unwrap().to_owned(),
        );
        // Exit status 127 from the transport means moniker-server was not found.
        assert!(
            matches!(result, Err(SyncError::ServerNotFound)),
            "got {result:?}"
        );
        let record = fs::read_to_string(w.store.path().parent().unwrap().join("record")).unwrap();
        let lines: Vec<&str> = record.lines().collect();
        assert!(lines.contains(&"ARG:alice@app.example.test"), "{lines:?}");
        assert!(lines.contains(&"ARG:sync-serve"), "{lines:?}");
        assert!(lines.contains(&"ARG:moniker-server"), "{lines:?}");
        assert!(
            lines.contains(&format!("SSH_ASKPASS={}", w.askpass.display()).as_str()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!("MONIKER_PROFILE_ID={}", w.profile.id).as_str()),
            "{lines:?}"
        );
        assert!(
            !record.contains("correct horse"),
            "the key is never in argv or env"
        );
        // No controlling terminal: ssh cannot ask in the terminal Moniker was
        // started from.
        assert!(lines.contains(&"SESSION-LEADER"), "{lines:?}");
        // No jump host, no hop: straight to the partner.
        assert!(!record.contains("ProxyCommand"), "{record}");
    }

    #[test]
    fn a_refused_login_says_a_sync_cannot_ask() {
        let refused = SyncError::Remote(
            "ssh exited with status 255: alice@app.example.test: Permission denied \
             (publickey,password)."
                .to_owned(),
        );
        let text = explain(&refused);
        assert!(text.starts_with(&refused.to_string()), "{text}");
        assert!(
            text.ends_with(
                "A sync cannot ask for a password or a key passphrase: store the profile's \
                 password, or add the key to ssh-agent."
            ),
            "{text}"
        );
        // Other remote failures are not given the hint.
        let other = SyncError::Remote("ssh exited with status 255: Connection refused".to_owned());
        assert_eq!(explain(&other), other.to_string());
    }

    #[test]
    fn without_a_stored_password_askpass_is_not_offered() {
        let w = world();
        let fake = fake_ssh(w.store.path().parent().unwrap());
        let _ = run_with(
            inputs_for(&w, false),
            Box::new(MemorySecretStore::default()),
            |cmd| cmd.program = fake.to_str().unwrap().to_owned(),
        );
        let record = fs::read_to_string(w.store.path().parent().unwrap().join("record")).unwrap();
        assert!(record.contains("SSH_ASKPASS=unset"), "{record}");
    }

    #[test]
    fn an_unreadable_profile_file_is_a_store_error_before_anything_runs() {
        let w = world();
        fs::write(w.store.path(), "{ not json").unwrap();
        let result = run_with(
            inputs_for(&w, false),
            Box::new(MemorySecretStore::default()),
            |_| panic!("ssh must not be started"),
        );
        match result {
            Err(SyncError::Store(message)) => assert!(!message.contains("not json"), "{message}"),
            other => panic!("got {other:?}"),
        }
    }

    // ----- a sync through the partner's jump host (issue #9) -----

    /// The text of a partner whose jump host is not saved here.
    const MISSING: &str = "This profile's jump host is no longer saved.";

    /// Save a jump host "Bastion" and make the world's profile go through it.
    /// `world.profile` is the partner as saved now.
    fn add_jump_host(w: &mut World) -> Profile {
        let bastion = Profile::new("Bastion", "bastion.example.test", "bob");
        w.store.upsert(bastion.clone()).unwrap();
        let bastion = w.store.get(&bastion.id).unwrap().clone();
        go_through(w, Some(&bastion.id));
        bastion
    }

    /// Make the world's profile go through the jump host `id` (or none).
    fn go_through(w: &mut World, id: Option<&str>) {
        let mut partner = w.profile.clone();
        partner.jump_host = id.map(str::to_owned);
        w.store.upsert(partner).unwrap();
        w.profile = w.store.get(&w.profile.id).unwrap().clone();
    }

    /// A keyring that has the sync key and fails on every other read.
    struct OnlyTheKey;

    impl SecretStore for OnlyTheKey {
        fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            if key == SYNC_PASSPHRASE_KEY {
                Ok(Some(Zeroizing::new(KEY.to_owned())))
            } else {
                Err(SecretError::Unavailable("no bus".to_owned()))
            }
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretError> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<(), SecretError> {
            Ok(())
        }
        fn probe(&self) -> Result<(), SecretError> {
            Ok(())
        }
    }

    #[test]
    fn a_partner_through_a_saved_jump_host_carries_it() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        let request = sync_request(&w.store, &w.askpass, None, &w.profile.id).unwrap();
        assert_eq!(request.jump.as_ref(), Some(&bastion));
        assert_eq!(request.profile, w.profile);
        // No jump host set: none carried.
        go_through(&mut w, None);
        let request = sync_request(&w.store, &w.askpass, None, &w.profile.id).unwrap();
        assert_eq!(request.jump, None);
        // The runtime folder travels with the request.
        let folder = Path::new("/run/user/1000");
        let request = sync_request(&w.store, &w.askpass, Some(folder), &w.profile.id).unwrap();
        assert_eq!(request.runtime_dir.as_deref(), Some(folder));
    }

    #[test]
    fn a_waiting_partner_is_not_synced() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        // Its jump host was deleted (a sync did it, say): it waits.
        w.store.delete(&bastion.id).unwrap();
        let err = sync_request(&w.store, &w.askpass, None, &w.profile.id)
            .err()
            .expect("refused");
        assert_eq!(err, MISSING);
        // One whose jump host this machine never had waits the same way.
        go_through(&mut w, Some("00000000-0000-4000-8000-000000000009"));
        assert_eq!(
            sync_request(&w.store, &w.askpass, None, &w.profile.id)
                .err()
                .as_deref(),
            Some(MISSING)
        );
        // Refused from the profile file alone: a keyring that fails on every
        // read is never asked, or the key's error would come first.
        assert_eq!(
            gather_inputs(&w.store, &Broken, &w.askpass, &w.profile.id)
                .err()
                .as_deref(),
            Some(MISSING)
        );
        // Never straight to the partner instead.
        assert!(sync_request(&w.store, &w.askpass, None, &w.profile.id).is_err());
    }

    #[test]
    fn a_chained_partner_is_not_synced() {
        let mut w = world();
        let far = Profile::new("Far", "far.example.test", "carol");
        w.store.upsert(far.clone()).unwrap();
        let mid = Profile {
            jump_host: Some(far.id.clone()),
            ..Profile::new("Mid", "mid.example.test", "bob")
        };
        w.store.upsert(mid.clone()).unwrap();
        go_through(&mut w, Some(&mid.id));
        let err = sync_request(&w.store, &w.askpass, None, &w.profile.id)
            .err()
            .expect("refused");
        assert_eq!(
            err,
            "Mid goes through a jump host of its own. Moniker uses only one jump host."
        );
        assert_eq!(
            gather_inputs(&w.store, &Broken, &w.askpass, &w.profile.id)
                .err()
                .as_deref(),
            Some(err.as_str()),
            "before the keyring"
        );
    }

    #[test]
    fn the_jump_host_s_password_is_looked_up() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        let inputs = |w: &World| {
            let Ok(inputs) = gather(w, &w.profile.id) else {
                panic!("gathering failed");
            };
            inputs
        };

        // Nothing stored for either.
        let got = inputs(&w);
        assert_eq!(got.jump.as_ref(), Some(&bastion));
        assert!(!got.has_password && !got.jump_has_password);

        // Each flag follows its own profile's password.
        w.secrets
            .set(&password_key(&bastion.id), "jump-pw")
            .unwrap();
        let got = inputs(&w);
        assert!(!got.has_password, "the partner has none");
        assert!(got.jump_has_password);
        w.secrets.delete(&password_key(&bastion.id)).unwrap();
        w.secrets
            .set(&password_key(&w.profile.id), "partner-pw")
            .unwrap();
        let got = inputs(&w);
        assert!(got.has_password);
        assert!(
            !got.jump_has_password,
            "the partner's is not the jump host's"
        );
        w.secrets
            .set(&password_key(&bastion.id), "jump-pw")
            .unwrap();
        let got = inputs(&w);
        assert!(got.has_password && got.jump_has_password);

        // A keyring that fails to read a password counts as none, for both:
        // the sync then fails with ssh's own message.
        let request = sync_request(&w.store, &w.askpass, None, &w.profile.id).unwrap();
        let Ok(got) = complete_inputs(request, &OnlyTheKey) else {
            panic!("a failed password read stopped the sync");
        };
        assert!(!got.has_password && !got.jump_has_password);

        // Without a jump host there is nothing to look up, even if a password
        // is stored under that profile's id.
        go_through(&mut w, None);
        let got = inputs(&w);
        assert_eq!(got.jump, None);
        assert!(!got.jump_has_password);
    }

    #[test]
    fn a_stored_jump_password_without_the_helper_stops_before_connecting() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        w.secrets.set(SYNC_PASSPHRASE_KEY, KEY).unwrap();
        w.secrets
            .set(&password_key(&bastion.id), "jump-pw")
            .unwrap();
        let missing = Path::new("/nonexistent/moniker/moniker-askpass");
        let err = gather_inputs(&w.store, &w.secrets, missing, &w.profile.id)
            .err()
            .expect("an error");
        assert_eq!(
            err,
            "moniker-askpass not found next to moniker, so the stored password of the jump \
             host Bastion cannot be used for the sync (build with `cargo build --workspace`)."
        );
        assert!(!err.contains("jump-pw"), "{err}");
        // With the helper there, and with only the partner's password stored,
        // it is the partner's that is named.
        assert!(gather(&w, &w.profile.id).is_ok());
        w.secrets.delete(&password_key(&bastion.id)).unwrap();
        w.secrets
            .set(&password_key(&w.profile.id), "partner-pw")
            .unwrap();
        assert_eq!(
            gather_inputs(&w.store, &w.secrets, missing, &w.profile.id)
                .err()
                .as_deref(),
            Some(ASKPASS_MISSING_SYNC)
        );
        // Both stored: the partner's is the first to be missed.
        w.secrets
            .set(&password_key(&bastion.id), "jump-pw")
            .unwrap();
        assert_eq!(
            gather_inputs(&w.store, &w.secrets, missing, &w.profile.id)
                .err()
                .as_deref(),
            Some(ASKPASS_MISSING_SYNC)
        );
        // Neither stored: no helper needed.
        w.secrets.delete(&password_key(&bastion.id)).unwrap();
        w.secrets.delete(&password_key(&w.profile.id)).unwrap();
        assert!(gather_inputs(&w.store, &w.secrets, missing, &w.profile.id).is_ok());
    }

    /// A fake `ssh` for a sync through a jump host. For each run it appends to
    /// `jump-record` the role it played (`master` when it is given `-M`,
    /// `check` or `exit` when it is given `-O`, else `target`), its arguments
    /// and the askpass variables it was given. The master "goes to the
    /// background" (exits 0), a check finds no master (exits 1), and the
    /// target finds no moniker-server (exits 127).
    fn fake_jump_ssh(dir: &Path) -> PathBuf {
        let path = dir.join("fake-jump-ssh");
        let record = dir.join("jump-record");
        let script = format!(
            "#!/bin/sh\n\
             role=target; prev=\n\
             for a in \"$@\"; do\n\
             [ \"$a\" = -M ] && role=master\n\
             [ \"$prev\" = -O ] && role=$a\n\
             prev=$a\n\
             done\n\
             {{\n\
             echo \"RUN:$role\"\n\
             for a in \"$@\"; do printf 'ARG:%s\\n' \"$a\"; done\n\
             printf 'SSH_ASKPASS=%s\\n' \"${{SSH_ASKPASS-unset}}\"\n\
             printf 'MONIKER_PROFILE_ID=%s\\n' \"${{MONIKER_PROFILE_ID-unset}}\"\n\
             }} >> '{}'\n\
             case $role in target) exit 127;; check) exit 1;; *) exit 0;; esac\n",
            record.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// What [`fake_jump_ssh`] recorded: each run's role, then its lines.
    fn jump_runs(dir: &Path) -> Vec<(String, Vec<String>)> {
        let mut runs: Vec<(String, Vec<String>)> = Vec::new();
        let record = fs::read_to_string(dir.join("jump-record")).unwrap_or_default();
        for line in record.lines() {
            match line.strip_prefix("RUN:") {
                Some(role) => runs.push((role.to_owned(), Vec::new())),
                None => runs
                    .last_mut()
                    .expect("a line before any run")
                    .1
                    .push(line.to_owned()),
            }
        }
        runs
    }

    /// A private folder with a short path, standing for the runtime folder.
    fn runtime_folder() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("rt")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    /// Inputs for a sync with the world's partner through `bastion`, the socket
    /// made in `runtime`, and a password stored for the jump host only.
    fn jump_inputs(w: &World, bastion: &Profile, runtime: &Path) -> SyncInputs {
        SyncInputs {
            jump: Some(bastion.clone()),
            jump_has_password: true,
            runtime_dir: Some(runtime.to_path_buf()),
            ..inputs_for(w, false)
        }
    }

    #[test]
    fn the_worker_builds_the_jump_leg() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        let dir = w.store.path().parent().unwrap().to_path_buf();
        let fake = fake_jump_ssh(&dir);
        let runtime = runtime_folder();
        let mut swapped = 0;
        let result = run_with(
            jump_inputs(&w, &bastion, runtime.path()),
            Box::new(MemorySecretStore::default()),
            |cmd| {
                // Only the program: the arguments are the real ones, and the
                // core checks that the sync's goes through the hop.
                cmd.program = fake.to_str().unwrap().to_owned();
                swapped += 1;
            },
        );
        // Exit status 127 from the target means moniker-server was not found.
        assert!(
            matches!(result, Err(SyncError::ServerNotFound)),
            "got {result:?}"
        );
        assert_eq!(swapped, 4, "the target, the master, the check and the exit");

        let runs = jump_runs(&dir);
        let roles: Vec<&str> = runs.iter().map(|(role, _)| role.as_str()).collect();
        // Signed in to the jump host first, its pid asked for, then the
        // partner through it; the master is ended last, by name, since its
        // pid was not learned.
        assert_eq!(roles, ["master", "check", "target", "exit"]);
        let line = |role: usize, prefix: &str| -> String {
            runs[role]
                .1
                .iter()
                .find_map(|l| l.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("no {prefix} for {role}: {:?}", runs[role].1))
                .to_owned()
        };
        let args = |role: usize| -> Vec<String> {
            runs[role]
                .1
                .iter()
                .filter_map(|l| l.strip_prefix("ARG:").map(str::to_owned))
                .collect()
        };

        // The master signs in to the jump host, answered by the jump host's
        // stored password; the sync's own ssh has none stored for the partner,
        // so it is offered no helper.
        let master = args(0);
        assert_eq!(
            master.last().map(String::as_str),
            Some("bob@bastion.example.test")
        );
        assert!(master.contains(&"-M".to_owned()), "{master:?}");
        assert_eq!(line(0, "MONIKER_PROFILE_ID="), bastion.id);
        assert_eq!(line(0, "SSH_ASKPASS="), w.askpass.display().to_string());
        assert_eq!(line(2, "MONIKER_PROFILE_ID="), "unset");
        assert_eq!(line(2, "SSH_ASKPASS="), "unset");

        // One socket in the runtime folder, named by all four.
        let socket = master
            .iter()
            .skip_while(|a| *a != "-S")
            .nth(1)
            .expect("the master's -S")
            .clone();
        let jump_folder = runtime.path().join("moniker").join("jump");
        assert!(
            Path::new(&socket).starts_with(&jump_folder),
            "{socket} is not in {}",
            jump_folder.display()
        );
        assert!(args(1).contains(&socket), "{:?}", args(1));
        assert!(args(3).contains(&socket), "{:?}", args(3));
        assert!(args(1).contains(&"check".to_owned()));
        assert!(args(3).contains(&"exit".to_owned()));

        // The target's ssh takes the hop as its ProxyCommand, through that
        // socket and to the target.
        let target = args(2);
        let hop = target
            .iter()
            .find_map(|a| a.strip_prefix("ProxyCommand="))
            .expect("a ProxyCommand");
        assert!(hop.starts_with(&format!("ssh -S {socket} ")), "{hop}");
        assert!(hop.ends_with("-- bob@bastion.example.test"), "{hop}");
        assert_eq!(target.last().map(String::as_str), Some("sync-serve"));
        assert!(
            target.contains(&"alice@app.example.test".to_owned()),
            "{target:?}"
        );

        // Nothing secret is in argv.
        let record = fs::read_to_string(dir.join("jump-record")).unwrap();
        assert!(!record.contains("jump-pw") && !record.contains("correct horse"));

        // The other way round: the partner's stored password is for the
        // partner's ssh, and the jump host's has none to be answered with.
        fs::remove_file(dir.join("jump-record")).unwrap();
        let inputs = SyncInputs {
            has_password: true,
            jump_has_password: false,
            ..jump_inputs(&w, &bastion, runtime.path())
        };
        let result = run_with(inputs, Box::new(MemorySecretStore::default()), |cmd| {
            cmd.program = fake.to_str().unwrap().to_owned();
        });
        assert!(
            matches!(result, Err(SyncError::ServerNotFound)),
            "{result:?}"
        );
        let runs = jump_runs(&dir);
        let env_of = |role: usize, prefix: &str| -> String {
            runs[role]
                .1
                .iter()
                .find_map(|l| l.strip_prefix(prefix))
                .unwrap()
                .to_owned()
        };
        assert_eq!(env_of(0, "SSH_ASKPASS="), "unset", "the master");
        assert_eq!(env_of(0, "MONIKER_PROFILE_ID="), "unset");
        assert_eq!(env_of(2, "MONIKER_PROFILE_ID="), w.profile.id, "the target");
        assert_eq!(env_of(2, "SSH_ASKPASS="), w.askpass.display().to_string());
    }

    #[test]
    fn the_worker_does_not_run_a_jump_connection_it_cannot_make() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        let runtime = runtime_folder();
        let no_ssh = |_: &mut SshCommand| panic!("ssh must not be started");
        let named = |err: Result<SyncOutcome, SyncError>| match err {
            Err(SyncError::JumpFailed { name, detail }) => {
                assert_eq!(name, "Bastion");
                detail
            }
            other => panic!("got {other:?}"),
        };

        // No runtime folder at all.
        let mut inputs = jump_inputs(&w, &bastion, runtime.path());
        inputs.runtime_dir = None;
        let detail = named(run_with(
            inputs,
            Box::new(MemorySecretStore::default()),
            no_ssh,
        ));
        assert_eq!(detail, NO_RUNTIME_FOLDER);

        // One others may write in: a socket there could be reached by them.
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let detail = named(run_with(
            jump_inputs(&w, &bastion, runtime.path()),
            Box::new(MemorySecretStore::default()),
            no_ssh,
        ));
        assert_eq!(detail, NO_RUNTIME_FOLDER);
        assert!(
            !runtime.path().join("moniker").exists(),
            "nothing was made in it"
        );

        // A relative one.
        let mut inputs = jump_inputs(&w, &bastion, runtime.path());
        inputs.runtime_dir = Some(PathBuf::from("run/user/1000"));
        let detail = named(run_with(
            inputs,
            Box::new(MemorySecretStore::default()),
            no_ssh,
        ));
        assert_eq!(detail, NO_RUNTIME_FOLDER);
    }

    #[test]
    fn the_worker_does_not_sync_straight_to_a_partner_routed_through_a_jump_host() {
        let mut w = world();
        let _bastion = add_jump_host(&mut w);
        let no_ssh = |_: &mut SshCommand| panic!("ssh must not be started");
        // The inputs name no jump host, though the partner has one: the core
        // sees only argv and would sync straight to it.
        let err = run_with(
            inputs_for(&w, false),
            Box::new(MemorySecretStore::default()),
            no_ssh,
        )
        .unwrap_err();
        match &err {
            SyncError::JumpFailed { name, detail } => {
                assert_eq!(name, &w.profile.name);
                assert_eq!(detail, NO_JUMP_HOST_GIVEN);
            }
            other => panic!("got {other:?}"),
        }
        // Said in the partner's name, as plain text, with no markup parsed.
        assert_eq!(
            explain(&err),
            "Prod <web> goes through a jump host, and this sync was started without it. \
             Sync again."
        );

        // Without a jump host on the partner, none is needed.
        go_through(&mut w, None);
        let result = run_with(
            inputs_for(&w, false),
            Box::new(MemorySecretStore::default()),
            |cmd| cmd.program = "/nonexistent/ssh".to_owned(),
        );
        assert!(
            !matches!(result, Err(SyncError::JumpFailed { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_jump_host_changed_since_the_request_stops_the_worker() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        let asked = Some(&bastion);
        let check = |w: &World, asked: Option<&Profile>| {
            check_route_unchanged(w.store.path(), &w.profile, asked)
                .err()
                .map(|failure| failure_text(&failure))
        };
        // The same as asked.
        assert_eq!(check(&w, asked), None);

        // Edits made through another handle on the file, as another process
        // (or a sync) would make them. `w.store` is then a stale snapshot, as
        // the main thread's is until the file watch reloads it.
        let other = || ProfileStore::open(w.store.path()).unwrap();

        // The jump host gains a jump host of its own: one hop only.
        let far = Profile::new("Far", "far.example.test", "carol");
        other().upsert(far.clone()).unwrap();
        other()
            .upsert(Profile {
                jump_host: Some(far.id.clone()),
                ..bastion.clone()
            })
            .unwrap();
        assert_eq!(
            check(&w, asked).as_deref(),
            Some("Bastion goes through a jump host of its own. Moniker uses only one jump host.")
        );

        // Deleted: the partner waits, and is never synced direct.
        other().delete(&bastion.id).unwrap();
        assert_eq!(check(&w, asked).as_deref(), Some(MISSING));
    }

    #[test]
    fn a_jump_host_edited_since_the_request_stops_the_worker() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        let other = || ProfileStore::open(w.store.path()).unwrap();
        let check = |asked: &Profile| {
            check_route_unchanged(w.store.path(), &w.profile, Some(asked))
                .err()
                .map(|failure| failure_text(&failure))
        };
        let changed = Some(JUMP_CHANGED.to_owned());
        assert_eq!(check(&bastion), None);

        // The same jump host id, but the master would sign in to the old
        // address (or as the old user, or with the old key) while the stored
        // password is looked up for the id: every part of where it connects
        // stops the sync, whichever way the file was edited.
        let edits = [
            (
                "host",
                Profile {
                    host: "elsewhere.example.test".to_owned(),
                    ..bastion.clone()
                },
            ),
            (
                "port",
                Profile {
                    port: 2222,
                    ..bastion.clone()
                },
            ),
            (
                "user name",
                Profile {
                    username: "mallory".to_owned(),
                    ..bastion.clone()
                },
            ),
            (
                "key file",
                Profile {
                    key_file: Some(PathBuf::from("/tmp/other-key")),
                    ..bastion.clone()
                },
            ),
        ];
        for (what, edited) in edits {
            other().upsert(edited).unwrap();
            assert_eq!(check(&bastion), changed, "the jump host's {what}");
            // Put back: the same as asked again.
            other().upsert(bastion.clone()).unwrap();
            assert_eq!(check(&bastion), None, "{what} put back");
        }

        // A rename changes nothing of where the connection goes.
        other()
            .upsert(Profile {
                name: "Renamed".to_owned(),
                ..bastion.clone()
            })
            .unwrap();
        assert_eq!(check(&bastion), None);
    }

    #[test]
    fn another_route_than_the_one_asked_for_stops_the_worker() {
        let mut w = world();
        let bastion = add_jump_host(&mut w);
        let other_host = Profile::new("Gate", "gate.example.test", "dave");
        w.store.upsert(other_host.clone()).unwrap();
        let other = || ProfileStore::open(w.store.path()).unwrap();
        let check = |w: &World, asked: Option<&Profile>| {
            check_route_unchanged(w.store.path(), &w.profile, asked)
                .err()
                .map(|failure| failure_text(&failure))
        };
        let changed = Some(JUMP_CHANGED.to_owned());

        // The partner was given another saved jump host: the password that
        // was looked up is the old one's.
        let rerouted = Profile {
            jump_host: Some(other_host.id.clone()),
            ..w.profile.clone()
        };
        other().upsert(rerouted).unwrap();
        assert_eq!(check(&w, Some(&bastion)), changed);

        // The jump host was taken off: not a direct sync that nobody asked for.
        let direct = Profile {
            jump_host: None,
            ..w.profile.clone()
        };
        other().upsert(direct).unwrap();
        assert_eq!(check(&w, Some(&bastion)), changed);

        // And one was put on a request made without.
        assert_eq!(check(&w, None), None);
        let through = Profile {
            jump_host: Some(bastion.id.clone()),
            ..w.profile.clone()
        };
        other().upsert(through).unwrap();
        assert_eq!(check(&w, None), changed);

        // A file that cannot be opened is a store error that quotes nothing.
        fs::write(w.store.path(), "{ not json SENTINEL").unwrap();
        match check_route_unchanged(w.store.path(), &w.profile, None) {
            Err(SyncFailure::Sync(SyncError::Store(message))) => {
                assert!(!message.contains("SENTINEL"), "{message}");
            }
            other => panic!("{:?}", other.err()),
        }
    }
}
