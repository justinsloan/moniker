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
//! The sync key is held in `Zeroizing` buffers and never shown; every message
//! is plain text (profile names come from sync data and may contain markup
//! characters).

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk4 as gtk;
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

/// Everything a sync's exchange needs, completed on the worker from a
/// [`SyncRequest`] and the keyring. No `Debug`: it holds the sync passphrase.
pub(crate) struct SyncInputs {
    pub(crate) profile: Profile,
    pub(crate) has_password: bool,
    pub(crate) passphrase: Zeroizing<String>,
    pub(crate) askpass: PathBuf,
    pub(crate) store_path: PathBuf,
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
    pub(crate) askpass: PathBuf,
    pub(crate) store_path: PathBuf,
    pub(crate) stale: StaleSync,
}

/// On the main thread: the profile to sync with, and where the worker finds
/// the profile file and the askpass helper. Reads no secret.
///
/// Refused while sync is turned off, here and now, so that no worker starts and
/// the keyring is not read for a sync that cannot run (issue #10). This is the
/// snapshot's word, which can be a moment behind the file: the worker looks at
/// the file itself before it reads the keyring ([`check_switch_is_on`]), and the
/// core refuses again before anything is sent, which is what protects the data.
pub(crate) fn sync_request(
    store: &ProfileStore,
    askpass: &Path,
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
    Ok(SyncRequest {
        profile,
        askpass: askpass.to_path_buf(),
        store_path,
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
    // StoreError messages carry no file contents.
    let store = ProfileStore::open(store_path)
        .map_err(|err| SyncFailure::Sync(SyncError::Store(err.to_string())))?;
    if store.sync_switch().is_on() {
        Ok(())
    } else {
        Err(SyncFailure::Sync(SyncError::SyncOff))
    }
}

/// On the worker: complete `request` from `secrets`, in the CLI's order: the
/// sync key, then whether the profile has a stored password (and, if it has,
/// that the askpass helper exists). The `Err` text never holds the key.
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
    // terminal, which for a sync means the sync fails with ssh's message.
    let has_password = matches!(secrets.get(&password_key(&request.profile.id)), Ok(Some(_)));
    // A sync has no terminal to ask in: with a stored password but no helper to
    // answer ssh, say so now rather than fail with an authentication error.
    if has_password && !request.askpass.is_file() {
        return Err(SyncFailure::NotStarted(ASKPASS_MISSING_SYNC.to_owned()));
    }
    Ok(SyncInputs {
        profile: request.profile,
        has_password,
        passphrase,
        askpass: request.askpass,
        store_path: request.store_path,
        stale: request.stale,
    })
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

/// The worker's whole job: open this thread's own store and secret store, build
/// the ssh command, and run the exchange. `tweak` may change the command before
/// it runs (the real runner changes nothing; tests swap in a fake `ssh`).
pub(crate) fn run_with(
    inputs: SyncInputs,
    secrets: Box<dyn SecretStore>,
    tweak: impl FnOnce(&mut SshCommand),
) -> Result<SyncOutcome, SyncError> {
    // StoreError messages carry no file contents.
    let store =
        ProfileStore::open(&inputs.store_path).map_err(|e| SyncError::Store(e.to_string()))?;
    let mut local = LocalState::new(store, secrets);
    let mut cmd = sync_command(
        &inputs.profile,
        &inputs.askpass,
        inputs.has_password,
        SERVER_PROGRAM,
    );
    tweak(&mut cmd);
    // In a session of its own: a question ssh would ask on a terminal fails at
    // once instead of waiting in the terminal Moniker was started from.
    sync_over_ssh(
        &mut local,
        &cmd,
        &inputs.passphrase,
        SshTerminal::Detached,
        inputs.stale,
    )
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
/// or what is wrong with the profile (gone, or its file path not absolute).
/// Nothing here reads the keyring: the worker reads the sync key and the
/// stored-password flag ([`complete_inputs`]), after it has looked at the
/// profile file ([`check_switch_is_on`]), and what it finds missing (no key, no
/// askpass helper, an unreadable keyring, sync turned off meanwhile) comes back
/// as the sync's result, like any failure of the exchange.
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
        sync_request(&store, &ctx.askpass_path, partner_id)
    }?;
    request.stale = stale;
    let open_secrets = worker_secrets(ctx);
    ctx.sync_running.set(true);
    ctx.sync_event(&SyncEvent::Started);
    let ctx = Rc::clone(ctx);
    let work = move || {
        check_switch_is_on(&request.store_path)?;
        let secrets = open_secrets();
        let inputs = complete_inputs(request, secrets.as_ref())?;
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
        let request = sync_request(store, askpass, profile_id)?;
        complete_inputs(request, secrets).map_err(|failure| failure_text(&failure))
    }

    #[test]
    fn a_missing_key_is_its_own_failure_and_never_shows_a_key() {
        let w = world();
        let request = || sync_request(&w.store, &w.askpass, &w.profile.id).unwrap();
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
        let err = sync_request(&w.store, &w.askpass, &w.profile.id)
            .err()
            .expect("refused");
        assert_eq!(err, SYNC_IS_OFF);
        // Back on: both work again.
        w.store.set_sync(true).unwrap();
        assert_eq!(
            auto_sync_target(&partner, &w.store),
            Some(w.profile.id.clone())
        );
        assert!(sync_request(&w.store, &w.askpass, &w.profile.id).is_ok());
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
        let err = sync_request(&unreadable, &w.askpass, &w.profile.id);
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
        assert!(sync_request(&w.store, &w.askpass, &w.profile.id).is_ok());
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
            passphrase: Zeroizing::new(KEY.to_owned()),
            askpass: w.askpass.clone(),
            store_path: w.store.path().to_path_buf(),
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
}
