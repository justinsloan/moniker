//! Pure logic behind the Connection Profiles window (per-field saving, the password, the
//! New Profile dialog, the startup-sequence sub-page, search): no GTK types, so
//! it is unit-tested without a display.
//!
//! Error strings returned here are shown to the user, so they never include a
//! startup command or a password (commands can hold secrets).

use std::cell::RefCell;
use std::path::PathBuf;

use moniker_core::profile::{Profile, ProfileError, StartupSequence, now_ms};
use moniker_core::secrets::{SecretStore, password_key};
use moniker_core::store::ProfileStore;

/// The editor's fields as plain values.
#[derive(Clone, Debug, PartialEq)]
pub struct FormValues {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Empty means no key file.
    pub key_file: String,
    pub transcript: bool,
}

/// Label of the editor's check box that removes the stored password. It says
/// "this machine only" because removal is not synced; see
/// [`REMOVE_PASSWORD_NOTE`].
pub const REMOVE_PASSWORD_LABEL: &str = "Remove stored password (this machine only)";

/// The line under [`REMOVE_PASSWORD_LABEL`]: what the removal does not do.
pub const REMOVE_PASSWORD_NOTE: &str = "Other machines keep their copy; a later sync from a machine that still has it can bring it back.";

/// What saving the editor does to the stored password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordAction {
    /// Nothing typed and "remove" not checked: leave the stored password alone.
    Keep,
    /// A new password was typed: store it.
    Set,
    /// "Remove stored password (this machine only)" was checked: delete it from
    /// this machine's keyring. The removal is not synced: other machines keep
    /// their copy and a later sync can bring the password back.
    Remove,
}

/// Turn the editor's values into a [`Profile`], or a message for the user.
///
/// An edit keeps the `id`, the startup sequences (edited on their own page)
/// and anything else the form does not show of `existing`; otherwise the
/// profile is new, with a fresh id and no sequences. Name, host, username and
/// key file are trimmed. The result has passed [`Profile::validate`].
pub fn build_profile(form: &FormValues, existing: Option<&Profile>) -> Result<Profile, String> {
    let mut profile = match existing {
        Some(p) => p.clone(),
        None => Profile::new("", "", ""),
    };
    profile.name = form.name.trim().to_owned();
    profile.host = form.host.trim().to_owned();
    profile.port = form.port;
    profile.username = form.username.trim().to_owned();
    profile.key_file = key_file_from(form.key_file.trim());
    profile.transcript = form.transcript;
    profile.validate().map_err(describe)?;
    Ok(profile)
}

/// A trimmed key-file path as stored: empty means none.
fn key_file_from(trimmed: &str) -> Option<PathBuf> {
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// A message for the user for each way validation can fail.
fn describe(err: ProfileError) -> String {
    match err {
        ProfileError::Id => "Internal error: the profile id is not valid.",
        ProfileError::Name => "Name must not be empty or contain control characters.",
        ProfileError::Host => {
            "Host must not be empty, may use only letters, digits and . _ : % -, \
             and must not start with '-'."
        }
        ProfileError::Username => {
            "Username must not be empty, may use only letters, digits and . _ @ -, \
             and must not start with '-'."
        }
        ProfileError::Port => "Port must be between 1 and 65535.",
        ProfileError::SequenceName => {
            "Every startup sequence needs a name, without control characters."
        }
        ProfileError::Command => "Startup commands must not contain a NUL character.",
        ProfileError::KeyFile => "The key file path must not contain a NUL character.",
    }
    .to_owned()
}

/// Decide what to do with the stored password from what the editor shows.
///
/// Any typed text counts (a password may be all spaces). Typing a password
/// and checking "remove" is contradictory, so it is an error rather than a guess.
pub fn password_action(typed: &str, remove_checked: bool) -> Result<PasswordAction, String> {
    match (typed.is_empty(), remove_checked) {
        (true, false) => Ok(PasswordAction::Keep),
        (false, false) => Ok(PasswordAction::Set),
        (true, true) => Ok(PasswordAction::Remove),
        (false, true) => Err(format!(
            "Either type a new password or check \u{201c}{REMOVE_PASSWORD_LABEL}\u{201d}, not both."
        )),
    }
}

/// `user@host:port` for the profile list, with an IPv6 host in brackets.
pub fn subtitle(profile: &Profile) -> String {
    let host = if profile.host.contains(':') {
        format!("[{}]", profile.host)
    } else {
        profile.host.clone()
    };
    format!("{}@{}:{}", profile.username, host, profile.port)
}

/// Save a whole profile at once (the New Profile dialog), with no GTK in it.
/// `Ok` is the profile as stored; `Err` is a message for the user.
///
/// Order matters, so that a profile version on disk always implies its secret
/// is already stored:
///
/// 1. build and validate the profile and decide what to do with the password;
///    if either fails nothing has been touched;
/// 2. a newly typed password goes to the keyring (a failure here means the
///    profile is not saved), or "remove" deletes the stored one;
/// 3. the profile is written. If that fails for a *new* profile, the password
///    just stored is removed again: a retry builds a new profile id, so it
///    would otherwise be orphaned.
///
/// Messages never contain the password or a startup command.
pub fn save_profile(
    store: &RefCell<ProfileStore>,
    secrets: &dyn SecretStore,
    form: &FormValues,
    typed_password: &str,
    remove_password: bool,
    existing: Option<&Profile>,
) -> Result<Profile, String> {
    let profile = build_profile(form, existing)?;
    let action = password_action(typed_password, remove_password)?;

    let id = profile.id.clone();
    let key = password_key(&id);
    match action {
        PasswordAction::Set => secrets
            .set(&key, typed_password)
            .map_err(|err| format!("Could not store the password in the keyring: {err}"))?,
        PasswordAction::Remove => secrets
            .delete(&key)
            .map_err(|err| format!("Could not remove the stored password: {err}"))?,
        PasswordAction::Keep => {}
    }

    // Bind first so the store borrow ends here.
    let saved = store.borrow_mut().upsert(profile);
    if let Err(err) = saved {
        let mut message = format!("Could not save the profile: {err}");
        if action == PasswordAction::Set && existing.is_none() && secrets.delete(&key).is_err() {
            message
                .push_str(" The password just stored in the keyring could not be removed again.");
        }
        return Err(message);
    }
    // The store stamped `updated_at`; hand back what it holds.
    let stored = store.borrow().get(&id).cloned();
    stored.ok_or_else(|| GONE.to_owned())
}

/// What the person sees when a field is applied to a profile that is gone.
pub const GONE: &str =
    "This profile no longer exists. It may have been deleted on another machine and synced here.";

/// What the person sees when a profile cannot be synced again (issue #10):
/// another machine pinned its version at the largest possible value, so the
/// live profile would only tie with the deletions sent for it while it was
/// kept here, and a tie goes to the deletion.
pub const CANNOT_SYNC_AGAIN: &str = "This profile cannot sync again: another machine gave it the largest possible version. Delete it and create it again to sync it.";

/// One field of a profile, as the Connection Profiles window saves it: each row of the
/// profile page applies its own field on its own (see [`apply_field`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Name(String),
    Host(String),
    Port(u16),
    Username(String),
    /// `None`, or a path that is empty once trimmed, means no key file.
    KeyFile(Option<PathBuf>),
    Transcript(bool),
    /// Keep the profile on this machine only (`true`), or sync it again
    /// (`false`). Syncing again can be refused: see [`CANNOT_SYNC_AGAIN`].
    LocalOnly(bool),
}

impl Field {
    /// Set this field on `profile`, trimming text the way [`build_profile`] does.
    fn set_on(self, profile: &mut Profile) {
        match self {
            Field::Name(name) => profile.name = name.trim().to_owned(),
            Field::Host(host) => profile.host = host.trim().to_owned(),
            Field::Port(port) => profile.port = port,
            Field::Username(user) => profile.username = user.trim().to_owned(),
            Field::KeyFile(path) => {
                profile.key_file = path.and_then(|path| match path.to_str() {
                    Some(text) => key_file_from(text.trim()),
                    // Not UTF-8: kept as it is; the store refuses to write it.
                    None => Some(path),
                });
            }
            Field::Transcript(on) => profile.transcript = on,
            Field::LocalOnly(kept) => profile.local_only = kept,
        }
    }
}

/// Save one field of the live profile `id` and return the profile as stored.
///
/// The change is made to the profile as it is in the file *now*, not to this
/// window's snapshot, so a sync that changed other fields meanwhile keeps its
/// changes. A profile that is missing or deleted in the file is [`GONE`]: an
/// apply never brings back a profile that a sync deleted. The whole profile is
/// validated with the field changed; on any error nothing is written.
///
/// Syncing a kept profile again is refused when its stored version is the
/// largest possible ([`CANNOT_SYNC_AGAIN`]): the store's stamp rule can only
/// saturate there, so the live profile would tie with the deletions the other
/// machines hold for it, a tie goes to the deletion, and the next sync would
/// delete the profile here. The check is made on the profile under the store's
/// lock, so a version that arrives while the save waits is the one judged.
pub fn apply_field(
    store: &RefCell<ProfileStore>,
    id: &str,
    field: Field,
) -> Result<Profile, String> {
    let saved = rewrite_live_if(store, id, |profile| {
        if field == Field::LocalOnly(false) && profile.updated_at == i64::MAX {
            return Err(CANNOT_SYNC_AGAIN.to_owned());
        }
        field.set_on(profile);
        Ok(true)
    })?;
    // `None` only for a change that says it changed nothing; this one never does.
    saved.ok_or_else(|| GONE.to_owned())
}

/// Store a newly typed password for the live profile `id`.
///
/// Empty `typed` means "keep the stored one": nothing is touched. Otherwise
/// the order is the one every save keeps, so that a profile version on disk
/// implies its secret is already stored: the password goes to the keyring
/// first, then the profile is written again unchanged, which moves its
/// `updated_at` so the next sync carries the new password to the other
/// machines. Messages never contain the password.
pub fn apply_password(
    store: &RefCell<ProfileStore>,
    secrets: &dyn SecretStore,
    id: &str,
    typed: &str,
) -> Result<(), String> {
    if typed.is_empty() {
        return Ok(());
    }
    // Bind first so the store borrow ends here.
    let reloaded = store.borrow_mut().reload();
    reloaded.map_err(|err| format!("Could not read the profile file: {err}"))?;
    if store.borrow().get(id).is_none() {
        return Err(GONE.to_owned());
    }

    let key = password_key(id);
    secrets
        .set(&key, typed)
        .map_err(|err| format!("Could not store the password in the keyring: {err}"))?;

    match rewrite_live(store, id, |_| {}) {
        Ok(_) => Ok(()),
        Err(mut message) if message == GONE => {
            // Deleted by a sync in the moment between the check and the
            // write: a password for a profile that is gone is not kept.
            if secrets.delete(&key).is_err() {
                message.push_str(
                    " The password just stored in the keyring could not be removed again.",
                );
            }
            Err(message)
        }
        Err(message) => Err(format!(
            "{message} The new password is stored in the keyring, but other machines \
             will not receive it until this profile is saved again."
        )),
    }
}

/// Remove the stored password of profile `id` from this machine's keyring.
/// The profile file is not touched, so the removal is not synced (see
/// [`REMOVE_PASSWORD_NOTE`]).
pub fn remove_password(secrets: &dyn SecretStore, id: &str) -> Result<(), String> {
    secrets
        .delete(&password_key(id))
        .map_err(|err| format!("Could not remove the stored password: {err}"))
}

/// What the sub-page says when the sequence it was opened on is not what the
/// file holds any more.
pub const SEQUENCE_CHANGED: &str = "This startup sequence was changed or removed elsewhere, \
     perhaps by a sync. Go back to the list to see what is saved now.";

/// What the sub-page says when it has no commands to save.
const NO_COMMANDS: &str = "A startup sequence needs at least one command.";

/// The commands typed in the sub-page's editor: one per line (a carriage
/// return ends a line too, so pasted CRLF text works), each trimmed, blank
/// lines dropped.
///
/// At least one must be left, and none may hold a NUL. The errors never quote
/// the text: commands can hold secrets.
pub fn parse_commands(text: &str) -> Result<Vec<String>, String> {
    if text.contains('\0') {
        return Err(describe(ProfileError::Command));
    }
    let commands: Vec<String> = text
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if commands.is_empty() {
        return Err(NO_COMMANDS.to_owned());
    }
    Ok(commands)
}

/// The editor text for `commands`: one per line ([`parse_commands`] reverses it).
pub fn format_commands(commands: &[String]) -> String {
    commands.join("\n")
}

/// Which way to move a startup sequence in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
}

/// `seqs` with the sequence at `index` swapped with its neighbor in direction
/// `dir`. Unchanged at either end, or for an `index` that is not in the list.
///
/// The order is the order of the connect choices.
pub fn moved(seqs: &[StartupSequence], index: usize, dir: Move) -> Vec<StartupSequence> {
    let mut list = seqs.to_vec();
    let other = match dir {
        Move::Up => index.checked_sub(1),
        Move::Down => index.checked_add(1),
    };
    if let Some(other) = other
        && index < list.len()
        && other < list.len()
    {
        list.swap(index, other);
    }
    list
}

/// `seqs` with `seq` appended (`index` is `None`) or put in place of the one at
/// `index`. An `index` past the end changes nothing.
pub fn with_sequence(
    seqs: &[StartupSequence],
    index: Option<usize>,
    seq: StartupSequence,
) -> Vec<StartupSequence> {
    let mut list = seqs.to_vec();
    match index {
        None => list.push(seq),
        Some(i) => {
            if let Some(slot) = list.get_mut(i) {
                *slot = seq;
            }
        }
    }
    list
}

/// `seqs` without the sequence at `index`. An `index` past the end changes
/// nothing.
pub fn without_sequence(seqs: &[StartupSequence], index: usize) -> Vec<StartupSequence> {
    let mut list = seqs.to_vec();
    if index < list.len() {
        list.remove(index);
    }
    list
}

/// Change the startup sequences of the live profile `id` as the file holds
/// them now: `change` gets the list and returns the one to save.
///
/// The sub-page works on one sequence, opened at `target` (its index and what
/// it held then). A sync can change the list while the page is open, or while
/// this waits for the file's lock, and the index could then name a different
/// sequence: moving or deleting that one would be a silent loss, and saving a
/// list built from an older one would undo the sync. So the check and the
/// change are made on what the file holds under the store's lock
/// ([`rewrite_live_if`]): the sequence at that index must still be the one the
/// page was opened on, else [`SEQUENCE_CHANGED`] and nothing is written. A new
/// sequence (`None`) has no such expectation. `Ok(None)`: the list came out the
/// same, and nothing was written. A profile that is missing or deleted is
/// [`GONE`].
pub fn apply_sequences(
    store: &RefCell<ProfileStore>,
    id: &str,
    target: Option<(usize, &StartupSequence)>,
    change: impl FnOnce(&[StartupSequence]) -> Vec<StartupSequence>,
) -> Result<Option<Profile>, String> {
    rewrite_live_if(store, id, |profile| {
        if let Some((index, expected)) = target
            && profile.sequences.get(index) != Some(expected)
        {
            return Err(SEQUENCE_CHANGED.to_owned());
        }
        let next = change(&profile.sequences);
        if next == profile.sequences {
            return Ok(false);
        }
        profile.sequences = next;
        Ok(true)
    })
}

/// The startup sequences of the live profile `id` in `store` as it was last
/// read, to open the sub-page on. A profile that is missing or deleted is
/// [`GONE`].
pub fn sequences_of(store: &ProfileStore, id: &str) -> Result<Vec<StartupSequence>, String> {
    store
        .get(id)
        .map(|profile| profile.sequences.clone())
        .ok_or_else(|| GONE.to_owned())
}

/// What the New Profile dialog asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewProfile {
    pub name: String,
    pub host: String,
    pub username: String,
    pub port: u16,
}

/// Save a new profile (and its password, if one was typed: keyring first) and
/// return its id. Everything else is edited afterwards on the profile page.
pub fn create_profile(
    store: &RefCell<ProfileStore>,
    secrets: &dyn SecretStore,
    new: &NewProfile,
    typed_password: &str,
) -> Result<String, String> {
    let form = FormValues {
        name: new.name.clone(),
        host: new.host.clone(),
        port: new.port,
        username: new.username.clone(),
        key_file: String::new(),
        transcript: false,
    };
    save_profile(store, secrets, &form, typed_password, false, None).map(|p| p.id)
}

/// Whether `profile` belongs in a list filtered by `query`: the trimmed query
/// is part of its name, host or username, ignoring case. A blank query
/// matches every profile.
pub fn matches_query(profile: &Profile, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || [&profile.name, &profile.host, &profile.username]
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
}

/// Change the live profile `id` as it is in the file now, under the store's
/// lock, and write it back with a new `updated_at`.
///
/// One locked read-change-write (`ProfileStore::update`), not a reload
/// followed by an `upsert`: a sync that lands in between could otherwise have
/// its edits to other fields overwritten, or a profile it deleted brought
/// back. `update` keeps the `updated_at` it is given, so the stamp is made
/// here by the store's own rule for edits: now, but always past the stamp it
/// replaces, so this edit wins the next merge. Nothing is written when the
/// profile is gone or the changed profile does not validate.
fn rewrite_live(
    store: &RefCell<ProfileStore>,
    id: &str,
    change: impl FnOnce(&mut Profile),
) -> Result<Profile, String> {
    let saved = rewrite_live_if(store, id, |profile| {
        change(profile);
        Ok(true)
    })?;
    // `None` only for a change that says it changed nothing; this one never does.
    saved.ok_or_else(|| GONE.to_owned())
}

/// [`rewrite_live`] for a change that can refuse (`Err`: nothing is written)
/// or find nothing to change (`Ok(false)`: nothing is written, and the result
/// is `Ok(None)`). It runs under the store's lock, on the profile as the file
/// holds it then.
fn rewrite_live_if(
    store: &RefCell<ProfileStore>,
    id: &str,
    change: impl FnOnce(&mut Profile) -> Result<bool, String>,
) -> Result<Option<Profile>, String> {
    let result = store.borrow_mut().update(|current| {
        let unchanged = current.to_vec();
        let Some(old) = current.iter().find(|p| p.id == id && !p.deleted) else {
            return Ok((unchanged, Err(GONE.to_owned())));
        };
        let mut changed = old.clone();
        match change(&mut changed) {
            Ok(true) => {}
            Ok(false) => return Ok((unchanged, Ok(None))),
            Err(message) => return Ok((unchanged, Err(message))),
        }
        if let Err(err) = changed.validate() {
            return Ok((unchanged, Err(describe(err))));
        }
        changed.updated_at = now_ms().max(old.updated_at.saturating_add(1));
        let next = current
            .iter()
            .map(|p| {
                if p.id == id {
                    changed.clone()
                } else {
                    p.clone()
                }
            })
            .collect();
        Ok((next, Ok(Some(changed))))
    });
    result.unwrap_or_else(|err| Err(format!("Could not save the profile: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(name: &str, commands: &[&str]) -> StartupSequence {
        StartupSequence {
            name: name.to_owned(),
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn values() -> FormValues {
        FormValues {
            name: "Prod web".to_owned(),
            host: "example.com".to_owned(),
            port: 22,
            username: "alice".to_owned(),
            key_file: String::new(),
            transcript: false,
        }
    }

    // ---- build_profile ----

    #[test]
    fn new_profile_gets_a_valid_uuid() {
        let p = build_profile(&values(), None).unwrap();
        assert_eq!(p.validate(), Ok(()));
        assert_eq!(p.id.len(), 36);
        assert_eq!(p.id.as_bytes()[14], b'4', "expected a version 4 UUID");
        assert!(!p.deleted);
        // Two new profiles never share an id.
        let q = build_profile(&values(), None).unwrap();
        assert_ne!(p.id, q.id);
    }

    #[test]
    fn editing_keeps_the_id() {
        let existing = Profile::new("Old", "old.example.com", "bob");
        let p = build_profile(&values(), Some(&existing)).unwrap();
        assert_eq!(p.id, existing.id);
        assert_eq!(p.name, "Prod web");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.username, "alice");
    }

    #[test]
    fn form_values_land_in_the_profile() {
        let mut v = values();
        v.port = 2222;
        v.key_file = "/home/alice/.ssh/id_ed25519".to_owned();
        v.transcript = true;
        let p = build_profile(&v, None).unwrap();
        assert_eq!(p.port, 2222);
        assert_eq!(
            p.key_file,
            Some(PathBuf::from("/home/alice/.ssh/id_ed25519"))
        );
        assert!(p.transcript);
        assert!(p.sequences.is_empty(), "a new profile has no sequences");
    }

    #[test]
    fn editing_keeps_the_sequences() {
        let mut existing = Profile::new("Old", "old.example.com", "bob");
        existing.sequences = vec![seq("update", &["sudo apt update"])];
        let p = build_profile(&values(), Some(&existing)).unwrap();
        assert_eq!(p.sequences, existing.sequences);
    }

    #[test]
    fn empty_key_file_means_none() {
        let mut existing = Profile::new("Old", "old.example.com", "bob");
        existing.key_file = Some(PathBuf::from("/k/id"));
        let mut v = values();
        v.key_file = "   ".to_owned();
        let p = build_profile(&v, Some(&existing)).unwrap();
        assert_eq!(p.key_file, None);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let mut v = values();
        v.name = "  Prod web ".to_owned();
        v.host = " example.com\t".to_owned();
        v.username = " alice ".to_owned();
        let p = build_profile(&v, None).unwrap();
        assert_eq!(p.name, "Prod web");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.username, "alice");
    }

    #[test]
    fn option_like_host_is_rejected() {
        let mut v = values();
        v.host = "-oProxyCommand=x".to_owned();
        let err = build_profile(&v, None).unwrap_err();
        assert!(err.to_lowercase().contains("host"), "got: {err}");
    }

    #[test]
    fn blank_name_is_rejected() {
        let mut v = values();
        v.name = "   ".to_owned();
        let err = build_profile(&v, None).unwrap_err();
        assert!(err.to_lowercase().contains("name"), "got: {err}");
    }

    #[test]
    fn bad_username_is_rejected() {
        let mut v = values();
        v.username = "-l root".to_owned();
        let err = build_profile(&v, None).unwrap_err();
        assert!(err.to_lowercase().contains("username"), "got: {err}");
    }

    #[test]
    fn port_zero_is_rejected() {
        let mut v = values();
        v.port = 0;
        let err = build_profile(&v, None).unwrap_err();
        assert!(err.to_lowercase().contains("port"), "got: {err}");
    }

    #[test]
    fn control_characters_in_the_name_are_rejected_with_a_readable_message() {
        for name in ["a\0b", "a\x1bb", "a\tb"] {
            let mut v = values();
            v.name = name.to_owned();
            let err = build_profile(&v, None).unwrap_err();
            assert_eq!(err, "Name must not be empty or contain control characters.");
        }
    }

    #[test]
    fn a_control_character_in_a_sequence_name_is_rejected() {
        let mut existing = Profile::new("Old", "old.example.com", "bob");
        existing.sequences = vec![seq("up\tdate", &["ls"])];
        let err = build_profile(&values(), Some(&existing)).unwrap_err();
        assert_eq!(
            err,
            "Every startup sequence needs a name, without control characters."
        );
    }

    #[test]
    fn a_nul_in_the_key_file_is_rejected_with_a_readable_message() {
        let mut v = values();
        v.key_file = "/k/id\0x".to_owned();
        let err = build_profile(&v, None).unwrap_err();
        assert_eq!(err, "The key file path must not contain a NUL character.");
    }

    #[test]
    fn every_validation_error_has_a_plain_sentence() {
        for err in [
            ProfileError::Id,
            ProfileError::Name,
            ProfileError::Host,
            ProfileError::Username,
            ProfileError::Port,
            ProfileError::SequenceName,
            ProfileError::Command,
            ProfileError::KeyFile,
        ] {
            let text = describe(err);
            assert!(text.ends_with('.'), "{text}");
            assert!(text.chars().all(|c| !c.is_control()), "{text:?}");
            assert!(!text.contains("::") && !text.contains('{'), "{text}");
        }
    }

    // ---- password_action ----

    #[test]
    fn password_action_cases() {
        assert_eq!(password_action("", false), Ok(PasswordAction::Keep));
        assert_eq!(password_action("hunter2", false), Ok(PasswordAction::Set));
        assert_eq!(password_action("", true), Ok(PasswordAction::Remove));
        let err = password_action("hunter2", true).unwrap_err();
        assert!(!err.contains("hunter2"), "got: {err}");
        assert!(err.contains(REMOVE_PASSWORD_LABEL), "got: {err}");
    }

    #[test]
    fn the_remove_password_text_says_it_is_local() {
        assert_eq!(
            REMOVE_PASSWORD_LABEL,
            "Remove stored password (this machine only)"
        );
        assert!(REMOVE_PASSWORD_NOTE.starts_with("Other machines keep their copy"));
        assert!(REMOVE_PASSWORD_NOTE.contains("a later sync"));
        assert!(REMOVE_PASSWORD_NOTE.contains("bring it back"));
        // Plain text: no markup characters, so it reads the same as a label.
        for text in [REMOVE_PASSWORD_LABEL, REMOVE_PASSWORD_NOTE] {
            assert!(!text.contains(['<', '>', '&']), "{text}");
        }
    }

    // ---- subtitle ----

    #[test]
    fn subtitle_is_user_at_host_colon_port() {
        let mut p = Profile::new("x", "example.com", "alice");
        p.port = 2222;
        assert_eq!(subtitle(&p), "alice@example.com:2222");
        p.host = "fe80::1%eth0".to_owned();
        assert_eq!(subtitle(&p), "alice@[fe80::1%eth0]:2222");
    }

    // ---- per-field saving (the Connection Profiles window) ----

    /// A store at `dir/profiles.json` holding one saved profile.
    fn store_with_profile() -> (tempfile::TempDir, RefCell<ProfileStore>, Profile) {
        let (dir, store) = temp_store();
        let p = Profile::new("Web", "web.example.com", "alice");
        store.borrow_mut().upsert(p.clone()).unwrap();
        let p = store.borrow().get(&p.id).unwrap().clone();
        (dir, store, p)
    }

    /// A second handle on the same file, as another process (a sync) has.
    fn other_handle(dir: &tempfile::TempDir) -> ProfileStore {
        ProfileStore::open(&dir.path().join("profiles.json")).unwrap()
    }

    fn file_bytes(dir: &tempfile::TempDir) -> Vec<u8> {
        std::fs::read(dir.path().join("profiles.json")).unwrap()
    }

    /// The profile `id` as it is in the file now.
    fn on_disk(dir: &tempfile::TempDir, id: &str) -> Option<Profile> {
        other_handle(dir).get(id).cloned()
    }

    #[test]
    fn apply_field_changes_one_field_and_bumps_updated_at() {
        let (dir, store, p) = store_with_profile();

        let saved = apply_field(&store, &p.id, Field::Host("  h2 ".to_owned())).unwrap();

        assert_eq!(saved.host, "h2");
        assert_eq!(saved.name, "Web");
        assert!(saved.updated_at > p.updated_at);
        assert_eq!(on_disk(&dir, &p.id), Some(saved));
    }

    #[test]
    fn apply_field_sets_each_kind_of_field() {
        let (dir, store, p) = store_with_profile();
        apply_field(&store, &p.id, Field::Name(" Web two ".to_owned())).unwrap();
        apply_field(&store, &p.id, Field::Username(" bob ".to_owned())).unwrap();
        apply_field(&store, &p.id, Field::Port(2222)).unwrap();
        apply_field(&store, &p.id, Field::Transcript(true)).unwrap();
        apply_sequences(&store, &p.id, None, |_| {
            vec![seq("update", &["sudo apt update"])]
        })
        .unwrap();
        apply_field(
            &store,
            &p.id,
            Field::KeyFile(Some(PathBuf::from(" /k/id_ed25519 "))),
        )
        .unwrap();

        let stored = on_disk(&dir, &p.id).unwrap();
        assert_eq!(stored.name, "Web two");
        assert_eq!(stored.username, "bob");
        assert_eq!(stored.port, 2222);
        assert!(stored.transcript);
        assert_eq!(stored.sequences, vec![seq("update", &["sudo apt update"])]);
        assert_eq!(stored.key_file, Some(PathBuf::from("/k/id_ed25519")));

        // An empty key file, or none, means no key file.
        apply_field(&store, &p.id, Field::KeyFile(Some(PathBuf::from("  ")))).unwrap();
        assert_eq!(on_disk(&dir, &p.id).unwrap().key_file, None);
        apply_field(&store, &p.id, Field::KeyFile(Some(PathBuf::from("/k/a")))).unwrap();
        apply_field(&store, &p.id, Field::KeyFile(None)).unwrap();
        assert_eq!(on_disk(&dir, &p.id).unwrap().key_file, None);
    }

    #[test]
    fn apply_field_rereads_the_file() {
        let (dir, store_a, p) = store_with_profile();
        let mut store_b = other_handle(&dir);
        let mut renamed = p.clone();
        renamed.name = "renamed".to_owned();
        store_b.upsert(renamed).unwrap();

        // A has not seen B's write.
        apply_field(&store_a, &p.id, Field::Host("h2".to_owned())).unwrap();

        let stored = on_disk(&dir, &p.id).unwrap();
        assert_eq!(stored.name, "renamed");
        assert_eq!(stored.host, "h2");
    }

    #[test]
    fn apply_to_a_deleted_profile_fails_and_writes_nothing() {
        let (dir, store_a, p) = store_with_profile();
        other_handle(&dir).delete(&p.id).unwrap();
        let before = file_bytes(&dir);

        let err = apply_field(&store_a, &p.id, Field::Name("x".to_owned())).unwrap_err();

        assert_eq!(err, GONE);
        assert_eq!(file_bytes(&dir), before);
        // The window's snapshot now knows it is gone too.
        assert!(store_a.borrow().get(&p.id).is_none());
    }

    #[test]
    fn apply_to_an_unknown_id_fails() {
        let (_dir, store, _p) = store_with_profile();
        let stranger = Profile::new("x", "x", "x");
        assert_eq!(
            apply_field(&store, &stranger.id, Field::Port(1)).unwrap_err(),
            GONE
        );
    }

    #[test]
    fn an_invalid_field_is_refused_without_writing() {
        let (dir, store, p) = store_with_profile();
        let before = file_bytes(&dir);

        let err = apply_field(&store, &p.id, Field::Host("bad host!".to_owned())).unwrap_err();
        assert_eq!(err, describe(ProfileError::Host));
        let err = apply_field(&store, &p.id, Field::Port(0)).unwrap_err();
        assert_eq!(err, describe(ProfileError::Port));
        let err = apply_field(&store, &p.id, Field::Name("   ".to_owned())).unwrap_err();
        assert_eq!(err, describe(ProfileError::Name));

        assert_eq!(file_bytes(&dir), before);
    }

    #[test]
    fn a_store_failure_is_a_message() {
        let (dir, store, p) = store_with_profile();
        // Something that is not a profile file now sits where it was.
        let path = dir.path().join("profiles.json");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        let err = apply_field(&store, &p.id, Field::Port(2)).unwrap_err();
        assert!(err.starts_with("Could not save the profile"), "got: {err}");
    }

    // ---- startup sequences (the sub-page) ----

    #[test]
    fn commands_are_one_per_line_and_blank_lines_drop() {
        assert_eq!(
            parse_commands(" sudo apt update \n\n  df -h\n").unwrap(),
            ["sudo apt update", "df -h"]
        );
    }

    #[test]
    fn a_carriage_return_ends_a_line_too() {
        // Pasted text may use CRLF (or a lone CR); a command never holds one.
        assert_eq!(parse_commands("a\r\nb\rc\n").unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn no_commands_is_an_error_that_echoes_nothing() {
        assert_eq!(
            parse_commands(" \n ").unwrap_err(),
            "A startup sequence needs at least one command."
        );
        assert_eq!(
            parse_commands("").unwrap_err(),
            "A startup sequence needs at least one command."
        );
        let err = parse_commands("echo s3cret\0").unwrap_err();
        assert_eq!(err, describe(ProfileError::Command));
        assert!(!err.contains("s3cret"));
        // A NUL on a line of its own is not a blank line.
        assert_eq!(
            parse_commands("\0").unwrap_err(),
            describe(ProfileError::Command)
        );
    }

    #[test]
    fn commands_round_trip_through_the_editor_text() {
        let commands = vec!["sudo apt update".to_owned(), "df -h".to_owned()];
        assert_eq!(format_commands(&commands), "sudo apt update\ndf -h");
        assert_eq!(
            parse_commands(&format_commands(&commands)).unwrap(),
            commands
        );
        assert_eq!(format_commands(&[]), "");
    }

    #[test]
    fn moving_swaps_neighbors_and_stops_at_the_ends() {
        let s = [seq("a", &["1"]), seq("b", &["2"]), seq("c", &["3"])];
        let names = |v: Vec<StartupSequence>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(moved(&s, 1, Move::Up)), ["b", "a", "c"]);
        assert_eq!(names(moved(&s, 0, Move::Up)), ["a", "b", "c"]);
        assert_eq!(names(moved(&s, 0, Move::Down)), ["b", "a", "c"]);
        assert_eq!(names(moved(&s, 2, Move::Down)), ["a", "b", "c"]);
        assert_eq!(names(moved(&s, 3, Move::Up)), ["a", "b", "c"]);
        assert_eq!(names(moved(&s, 9, Move::Down)), ["a", "b", "c"]);
        assert_eq!(names(moved(&[], 0, Move::Up)), Vec::<String>::new());
    }

    #[test]
    fn with_sequence_appends_or_replaces() {
        let s = [seq("a", &["1"]), seq("b", &["2"])];
        let names = |v: Vec<StartupSequence>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(with_sequence(&s, None, seq("c", &["3"]))),
            ["a", "b", "c"]
        );
        let replaced = with_sequence(&s, Some(0), seq("z", &["9"]));
        assert_eq!(replaced, [seq("z", &["9"]), seq("b", &["2"])]);
        // An index past the end changes nothing.
        assert_eq!(with_sequence(&s, Some(2), seq("c", &["3"])), s);
        assert_eq!(
            with_sequence(&[], None, seq("c", &["3"])),
            [seq("c", &["3"])]
        );
    }

    #[test]
    fn without_sequence_removes_one() {
        let s = [seq("a", &["1"]), seq("b", &["2"]), seq("c", &["3"])];
        assert_eq!(
            without_sequence(&s, 1),
            [seq("a", &["1"]), seq("c", &["3"])]
        );
        assert_eq!(without_sequence(&s, 3), s);
        assert_eq!(without_sequence(&[seq("a", &["1"])], 0), []);
    }

    /// The store with the profile's sequences set.
    fn store_with_sequences(
        sequences: &[StartupSequence],
    ) -> (tempfile::TempDir, RefCell<ProfileStore>, Profile) {
        let (dir, store, p) = store_with_profile();
        apply_sequences(&store, &p.id, None, |_| sequences.to_vec()).unwrap();
        let p = store.borrow().get(&p.id).unwrap().clone();
        (dir, store, p)
    }

    #[test]
    fn sequences_are_changed_as_the_file_holds_them() {
        let (dir, store, p) = store_with_sequences(&[seq("a", &["1"])]);
        let mut other = other_handle(&dir);
        let mut changed = p.clone();
        changed.sequences.push(seq("b", &["2"]));
        other.upsert(changed).unwrap();

        // The window's snapshot has not seen the second sequence; the change
        // gets the list the file holds.
        assert_eq!(store.borrow().get(&p.id).unwrap().sequences.len(), 1);
        let saved = apply_sequences(&store, &p.id, Some((0, &seq("a", &["1"]))), |now| {
            assert_eq!(now, [seq("a", &["1"]), seq("b", &["2"])]);
            with_sequence(now, Some(0), seq("a", &["1", "one"]))
        })
        .unwrap()
        .expect("written");
        assert_eq!(saved.sequences, [seq("a", &["1", "one"]), seq("b", &["2"])]);
        assert_eq!(
            other_handle(&dir).get(&p.id).unwrap().sequences,
            saved.sequences
        );
        // A new sequence expects nothing to be there.
        let saved = apply_sequences(&store, &p.id, None, |now| {
            with_sequence(now, None, seq("c", &["3"]))
        })
        .unwrap()
        .expect("written");
        assert_eq!(saved.sequences.len(), 3);
    }

    #[test]
    fn a_list_that_comes_out_the_same_is_not_written() {
        let (dir, store, p) = store_with_sequences(&[seq("a", &["1"])]);
        let before = file_bytes(&dir);
        // Moving the only sequence up changes nothing.
        let saved = apply_sequences(&store, &p.id, Some((0, &seq("a", &["1"]))), |now| {
            moved(now, 0, Move::Up)
        });
        assert_eq!(saved, Ok(None));
        assert_eq!(file_bytes(&dir), before);
    }

    #[test]
    fn a_sequence_that_changed_or_moved_is_not_written_over() {
        let (dir, store, p) = store_with_sequences(&[seq("a", &["1"]), seq("b", &["2"])]);
        let mut other = other_handle(&dir);
        let mut changed = p.clone();
        changed.sequences = vec![seq("b", &["2"]), seq("a", &["1"])];
        other.upsert(changed).unwrap();
        let before = file_bytes(&dir);

        // The page was opened on "a" at index 0; now index 0 holds "b".
        let err = apply_sequences(&store, &p.id, Some((0, &seq("a", &["1"]))), |now| {
            without_sequence(now, 0)
        })
        .unwrap_err();
        assert_eq!(err, SEQUENCE_CHANGED);
        // Past the end counts the same.
        let err = apply_sequences(&store, &p.id, Some((5, &seq("a", &["1"]))), |now| {
            now.to_vec()
        })
        .unwrap_err();
        assert_eq!(err, SEQUENCE_CHANGED);
        // Neither message holds a command, and nothing was written.
        assert!(!err.contains('1'));
        assert_eq!(file_bytes(&dir), before);
    }

    #[test]
    fn the_sequences_of_a_deleted_profile_are_gone() {
        let (dir, store, p) = store_with_sequences(&[seq("a", &["1"])]);
        other_handle(&dir).delete(&p.id).unwrap();
        let before = file_bytes(&dir);

        let change = |now: &[StartupSequence]| with_sequence(now, None, seq("b", &["2"]));
        assert_eq!(
            apply_sequences(&store, &p.id, Some((0, &seq("a", &["1"]))), change).unwrap_err(),
            GONE
        );
        assert_eq!(
            apply_sequences(&store, &p.id, None, change).unwrap_err(),
            GONE
        );
        assert_eq!(file_bytes(&dir), before);
        store.borrow_mut().reload().unwrap();
        assert_eq!(sequences_of(&store.borrow(), &p.id).unwrap_err(), GONE);
    }

    #[test]
    fn an_unreadable_file_is_reported_without_its_contents() {
        let (dir, store, p) = store_with_sequences(&[seq("a", &["echo s3cret"])]);
        std::fs::write(dir.path().join("profiles.json"), "{ echo s3cret").unwrap();

        let err = apply_sequences(&store, &p.id, None, |now| now.to_vec()).unwrap_err();
        assert!(err.starts_with("Could not save the profile"), "got: {err}");
        assert!(!err.contains("s3cret"));
    }

    /// A memory keyring that, on every `set`, notes the `updated_at` the
    /// profile has in the file at that moment, and can be told to fail.
    struct WatchingSecrets {
        inner: MemorySecretStore,
        file: PathBuf,
        id: String,
        stamps_at_set: std::sync::Mutex<Vec<i64>>,
        fail: bool,
    }

    impl SecretStore for WatchingSecrets {
        fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            if self.fail {
                return Err(SecretError::Backend("locked".to_owned()));
            }
            let stamp = ProfileStore::open(&self.file)
                .unwrap()
                .get(&self.id)
                .map_or(-1, |p| p.updated_at);
            self.stamps_at_set.lock().unwrap().push(stamp);
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<(), SecretError> {
            self.inner.delete(key)
        }
        fn probe(&self) -> Result<(), SecretError> {
            Ok(())
        }
    }

    fn watching(dir: &tempfile::TempDir, id: &str, fail: bool) -> WatchingSecrets {
        WatchingSecrets {
            inner: MemorySecretStore::default(),
            file: dir.path().join("profiles.json"),
            id: id.to_owned(),
            stamps_at_set: std::sync::Mutex::new(Vec::new()),
            fail,
        }
    }

    #[test]
    fn apply_password_stores_the_secret_before_the_profile() {
        let (dir, store, p) = store_with_profile();
        let secrets = watching(&dir, &p.id, false);

        apply_password(&store, &secrets, &p.id, PASSWORD).unwrap();

        let stored = on_disk(&dir, &p.id).unwrap();
        assert_eq!(
            *secrets.stamps_at_set.lock().unwrap(),
            [p.updated_at],
            "the keyring was written while the file still held the old version"
        );
        assert!(
            stored.updated_at > p.updated_at,
            "sync must carry the password"
        );
        assert_eq!(stored.name, p.name);
        assert_eq!(
            secrets
                .get(&password_key(&p.id))
                .unwrap()
                .map(|s| s.as_str().to_owned()),
            Some(PASSWORD.to_owned())
        );
    }

    #[test]
    fn a_keyring_failure_leaves_the_profile_alone() {
        let (dir, store, p) = store_with_profile();
        let secrets = watching(&dir, &p.id, true);
        let before = file_bytes(&dir);

        let err = apply_password(&store, &secrets, &p.id, PASSWORD).unwrap_err();

        assert!(err.contains("keyring"), "got: {err}");
        assert!(!err.contains(PASSWORD), "got: {err}");
        assert_eq!(file_bytes(&dir), before);
    }

    #[test]
    fn a_password_for_a_deleted_profile_is_not_stored() {
        let (dir, store, p) = store_with_profile();
        other_handle(&dir).delete(&p.id).unwrap();
        let before = file_bytes(&dir);
        let secrets = MemorySecretStore::default();

        let err = apply_password(&store, &secrets, &p.id, PASSWORD).unwrap_err();

        assert_eq!(err, GONE);
        assert!(!err.contains(PASSWORD));
        assert!(secret_count(&secrets).contains("entries: 0"));
        assert_eq!(file_bytes(&dir), before);
    }

    /// A memory keyring whose `set` lets a "sync" delete the profile from the
    /// file right after storing the secret, and whose `delete` can fail.
    struct DeletedDuringSet {
        inner: MemorySecretStore,
        file: PathBuf,
        id: String,
        fail_delete: bool,
    }

    impl SecretStore for DeletedDuringSet {
        fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            self.inner.set(key, value)?;
            ProfileStore::open(&self.file)
                .unwrap()
                .delete(&self.id)
                .unwrap();
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<(), SecretError> {
            if self.fail_delete {
                return Err(SecretError::Backend("locked".to_owned()));
            }
            self.inner.delete(key)
        }
        fn probe(&self) -> Result<(), SecretError> {
            Ok(())
        }
    }

    #[test]
    fn a_profile_deleted_while_the_password_is_stored_takes_the_password_back() {
        for fail_delete in [false, true] {
            let (dir, store, p) = store_with_profile();
            let secrets = DeletedDuringSet {
                inner: MemorySecretStore::default(),
                file: dir.path().join("profiles.json"),
                id: p.id.clone(),
                fail_delete,
            };

            let err = apply_password(&store, &secrets, &p.id, PASSWORD).unwrap_err();

            assert!(err.starts_with(GONE), "got: {err}");
            assert!(!err.contains(PASSWORD), "got: {err}");
            assert!(on_disk(&dir, &p.id).is_none(), "never brought back");
            if fail_delete {
                assert_eq!(
                    err,
                    format!(
                        "{GONE} The password just stored in the keyring could not be removed again."
                    )
                );
                assert!(stored_password(&secrets.inner, &p.id).is_some());
            } else {
                assert_eq!(err, GONE);
                assert_eq!(stored_password(&secrets.inner, &p.id), None);
            }
        }
    }

    /// Seed the file with `p` carrying exactly `stamp` (`upsert` would restamp it).
    fn seed_with_stamp(store: &RefCell<ProfileStore>, p: &Profile, stamp: i64) {
        let mut seeded = p.clone();
        seeded.updated_at = stamp;
        store.borrow_mut().replace_all(vec![seeded]).unwrap();
    }

    #[test]
    fn an_apply_beats_a_stamp_from_the_future() {
        let (_dir, store, p) = store_with_profile();
        // From a machine whose clock runs ten days ahead.
        let future = now_ms() + 10 * 24 * 3600 * 1000;
        seed_with_stamp(&store, &p, future);

        let saved = apply_field(&store, &p.id, Field::Port(2222)).unwrap();
        assert_eq!(saved.updated_at, future + 1);

        // The password path stamps the same way.
        seed_with_stamp(&store, &p, future);
        apply_password(&store, &MemorySecretStore::default(), &p.id, PASSWORD).unwrap();
        assert_eq!(store.borrow().get(&p.id).unwrap().updated_at, future + 1);
    }

    #[test]
    fn an_apply_on_a_pinned_stamp_saturates() {
        let (dir, store, p) = store_with_profile();
        seed_with_stamp(&store, &p, i64::MAX);

        let saved = apply_field(&store, &p.id, Field::Port(2222)).unwrap();

        assert_eq!(saved.updated_at, i64::MAX);
        assert_eq!(on_disk(&dir, &p.id).unwrap().port, 2222);
    }

    #[test]
    fn a_profile_can_be_kept_here_and_synced_again() {
        let (dir, store, p) = store_with_profile();
        assert!(!p.local_only);

        let kept = apply_field(&store, &p.id, Field::LocalOnly(true)).unwrap();

        assert!(kept.local_only);
        assert!(kept.updated_at > p.updated_at, "an edit like any other");
        assert_eq!(on_disk(&dir, &p.id), Some(kept.clone()));
        assert_eq!(kept.host, p.host, "nothing else changed");

        let synced = apply_field(&store, &p.id, Field::LocalOnly(false)).unwrap();

        assert!(!synced.local_only);
        assert!(synced.updated_at > kept.updated_at);
        assert_eq!(on_disk(&dir, &p.id), Some(synced));
    }

    #[test]
    fn syncing_again_is_refused_at_the_largest_version() {
        let (dir, store, p) = store_with_profile();
        let mut kept = p.clone();
        kept.local_only = true;
        seed_with_stamp(&store, &kept, i64::MAX);
        let before = file_bytes(&dir);

        let err = apply_field(&store, &p.id, Field::LocalOnly(false)).unwrap_err();

        assert_eq!(err, CANNOT_SYNC_AGAIN);
        assert_eq!(file_bytes(&dir), before, "nothing written");
        assert!(on_disk(&dir, &p.id).unwrap().local_only);

        // One below the largest is an ordinary edit: it can still go out.
        seed_with_stamp(&store, &kept, i64::MAX - 1);
        let synced = apply_field(&store, &p.id, Field::LocalOnly(false)).unwrap();
        assert!(!synced.local_only);
        assert_eq!(synced.updated_at, i64::MAX, "one above, saturating there");

        // Keeping it here at the largest version is no problem: the tombstone
        // it sends wins a tie.
        seed_with_stamp(&store, &p, i64::MAX);
        let kept_again = apply_field(&store, &p.id, Field::LocalOnly(true)).unwrap();
        assert!(kept_again.local_only);
    }

    #[test]
    fn an_empty_password_apply_touches_nothing() {
        let (dir, store, p) = store_with_profile();
        let secrets = watching(&dir, &p.id, false);
        let before = file_bytes(&dir);

        apply_password(&store, &secrets, &p.id, "").unwrap();

        assert!(secrets.stamps_at_set.lock().unwrap().is_empty());
        assert!(secret_count(&secrets.inner).contains("entries: 0"));
        assert_eq!(file_bytes(&dir), before);
    }

    #[test]
    fn remove_password_deletes_only_the_keyring_entry() {
        let (dir, store, p) = store_with_profile();
        let secrets = MemorySecretStore::default();
        secrets.set(&password_key(&p.id), PASSWORD).unwrap();
        let before = file_bytes(&dir);

        remove_password(&secrets, &p.id).unwrap();

        assert_eq!(stored_password(&secrets, &p.id), None);
        assert_eq!(file_bytes(&dir), before);
        drop(store);

        let err = remove_password(&BrokenSecrets, &p.id).unwrap_err();
        assert!(
            err.starts_with("Could not remove the stored password"),
            "got: {err}"
        );
    }

    fn new_values() -> NewProfile {
        NewProfile {
            name: " NAS ".to_owned(),
            host: "nas.lan".to_owned(),
            username: "admin".to_owned(),
            port: 2200,
        }
    }

    #[test]
    fn create_profile_returns_the_new_id() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();

        let id = create_profile(&store, &secrets, &new_values(), PASSWORD).unwrap();

        let store = store.borrow();
        let p = store.get(&id).expect("the new profile is live");
        assert_eq!(p.name, "NAS");
        assert_eq!(p.port, 2200);
        assert!(p.sequences.is_empty());
        assert!(!p.transcript);
        assert_eq!(stored_password(&secrets, &id), Some(PASSWORD.to_owned()));
    }

    #[test]
    fn create_profile_without_a_password_stores_none() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let id = create_profile(&store, &secrets, &new_values(), "").unwrap();
        assert!(store.borrow().get(&id).is_some());
        assert!(secret_count(&secrets).contains("entries: 0"));
    }

    #[test]
    fn create_profile_refuses_invalid_values_and_stores_nothing() {
        let (dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let mut bad = new_values();
        bad.username = "-l root".to_owned();

        let err = create_profile(&store, &secrets, &bad, PASSWORD).unwrap_err();

        assert_eq!(err, describe(ProfileError::Username));
        assert!(!err.contains(PASSWORD));
        assert!(secret_count(&secrets).contains("entries: 0"));
        assert!(!dir.path().join("profiles.json").exists());
    }

    #[test]
    fn query_matches_name_host_or_user_ignoring_case() {
        let p = Profile::new("Web One", "web-01.lan", "admin");
        assert!(matches_query(&p, "WEB"));
        assert!(matches_query(&p, "01.l"));
        assert!(matches_query(&p, "adm"));
        assert!(matches_query(&p, "  "));
        assert!(!matches_query(&p, "nas"));
    }

    #[test]
    fn query_is_trimmed() {
        let p = Profile::new("Web One", "web-01.lan", "admin");
        assert!(matches_query(&p, " one "));
    }

    // ---- save_profile ----

    use moniker_core::secrets::{MemorySecretStore, SecretError};
    use zeroize::Zeroizing;

    const PASSWORD: &str = "correct-horse-battery-staple";

    /// A store backed by a file in a temporary directory.
    fn temp_store() -> (tempfile::TempDir, RefCell<ProfileStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(&dir.path().join("profiles.json")).unwrap();
        (dir, RefCell::new(store))
    }

    /// A store that opened fine but can no longer be saved: a directory now
    /// sits where its file should be, so every write fails with an I/O error.
    fn unwritable_store() -> (tempfile::TempDir, RefCell<ProfileStore>) {
        let (dir, store) = temp_store();
        std::fs::create_dir(dir.path().join("profiles.json")).unwrap();
        (dir, store)
    }

    /// A secret store with nothing usable behind it.
    struct BrokenSecrets;

    impl SecretStore for BrokenSecrets {
        fn get(&self, _key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            Err(SecretError::Unavailable("no service".to_owned()))
        }
        fn set(&self, _key: &str, _value: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no service".to_owned()))
        }
        fn delete(&self, _key: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no service".to_owned()))
        }
        fn probe(&self) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no service".to_owned()))
        }
    }

    fn stored_password(secrets: &MemorySecretStore, id: &str) -> Option<String> {
        secrets
            .get(&password_key(id))
            .unwrap()
            .map(|p| p.as_str().to_owned())
    }

    /// How many secrets the memory store holds (its Debug output is just that count).
    fn secret_count(secrets: &MemorySecretStore) -> String {
        format!("{secrets:?}")
    }

    /// A saved profile with a stored password, for the editing tests.
    fn saved_with_password(store: &RefCell<ProfileStore>, secrets: &MemorySecretStore) -> Profile {
        let profile = Profile::new("Old", "old.example.com", "bob");
        store.borrow_mut().upsert(profile.clone()).unwrap();
        secrets
            .set(&password_key(&profile.id), "old-password")
            .unwrap();
        profile
    }

    #[test]
    fn saving_a_new_profile_stores_the_password_then_the_profile() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        save_profile(&store, &secrets, &values(), PASSWORD, false, None).unwrap();

        let store = store.borrow();
        let live = store.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].name, "Prod web");
        assert_eq!(
            stored_password(&secrets, &live[0].id),
            Some(PASSWORD.to_owned())
        );
    }

    #[test]
    fn saving_without_a_password_stores_none() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        save_profile(&store, &secrets, &values(), "", false, None).unwrap();
        assert_eq!(store.borrow().live().len(), 1);
        assert!(secret_count(&secrets).contains("entries: 0"));
    }

    #[test]
    fn editing_with_a_blank_password_keeps_the_stored_one() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let existing = saved_with_password(&store, &secrets);

        save_profile(&store, &secrets, &values(), "", false, Some(&existing)).unwrap();

        assert_eq!(
            stored_password(&secrets, &existing.id),
            Some("old-password".to_owned())
        );
        let store = store.borrow();
        assert_eq!(store.get(&existing.id).unwrap().name, "Prod web");
    }

    #[test]
    fn editing_with_a_new_password_replaces_the_stored_one() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let existing = saved_with_password(&store, &secrets);

        save_profile(
            &store,
            &secrets,
            &values(),
            PASSWORD,
            false,
            Some(&existing),
        )
        .unwrap();

        assert_eq!(
            stored_password(&secrets, &existing.id),
            Some(PASSWORD.to_owned())
        );
        assert_eq!(store.borrow().live().len(), 1);
    }

    #[test]
    fn remove_deletes_the_stored_password_and_saves() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let existing = saved_with_password(&store, &secrets);

        save_profile(&store, &secrets, &values(), "", true, Some(&existing)).unwrap();

        assert_eq!(stored_password(&secrets, &existing.id), None);
        assert_eq!(store.borrow().get(&existing.id).unwrap().name, "Prod web");
    }

    #[test]
    fn an_invalid_form_touches_nothing() {
        let (dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let mut v = values();
        v.host = "-oProxyCommand=x".to_owned();

        let err = save_profile(&store, &secrets, &v, PASSWORD, false, None).unwrap_err();

        assert!(err.to_lowercase().contains("host"), "got: {err}");
        assert!(!err.contains(PASSWORD), "got: {err}");
        assert!(store.borrow().live().is_empty());
        assert!(secret_count(&secrets).contains("entries: 0"));
        assert!(!dir.path().join("profiles.json").exists());
    }

    #[test]
    fn a_typed_password_together_with_remove_touches_nothing() {
        let (_dir, store) = temp_store();
        let secrets = MemorySecretStore::default();
        let existing = saved_with_password(&store, &secrets);

        let err =
            save_profile(&store, &secrets, &values(), PASSWORD, true, Some(&existing)).unwrap_err();

        assert!(!err.contains(PASSWORD), "got: {err}");
        assert_eq!(
            stored_password(&secrets, &existing.id),
            Some("old-password".to_owned())
        );
        assert_eq!(store.borrow().get(&existing.id).unwrap().name, "Old");
    }

    #[test]
    fn a_keyring_failure_means_the_profile_is_not_saved() {
        let (dir, store) = temp_store();

        let err =
            save_profile(&store, &BrokenSecrets, &values(), PASSWORD, false, None).unwrap_err();

        assert!(err.contains("keyring"), "got: {err}");
        assert!(!err.contains(PASSWORD), "got: {err}");
        assert!(store.borrow().live().is_empty());
        assert!(!dir.path().join("profiles.json").exists());
    }

    #[test]
    fn a_failed_remove_means_the_profile_is_not_saved() {
        let (_dir, store) = temp_store();
        let existing = Profile::new("Old", "old.example.com", "bob");
        store.borrow_mut().upsert(existing.clone()).unwrap();

        let err =
            save_profile(&store, &BrokenSecrets, &values(), "", true, Some(&existing)).unwrap_err();

        assert!(!err.is_empty());
        assert_eq!(store.borrow().get(&existing.id).unwrap().name, "Old");
    }

    #[test]
    fn a_store_failure_removes_the_password_stored_for_a_new_profile() {
        let (_dir, store) = unwritable_store();
        let secrets = MemorySecretStore::default();

        let err = save_profile(&store, &secrets, &values(), PASSWORD, false, None).unwrap_err();

        assert!(err.contains("Could not save the profile"), "got: {err}");
        assert!(!err.contains(PASSWORD), "got: {err}");
        assert!(store.borrow().live().is_empty());
        // No password left behind for a profile that does not exist.
        assert!(secret_count(&secrets).contains("entries: 0"));
    }

    #[test]
    fn a_store_failure_while_editing_does_not_delete_the_password() {
        let (_dir, store) = unwritable_store();
        let secrets = MemorySecretStore::default();
        // Editing: the profile's password was not created by this save, so a
        // failed save must not delete it.
        let existing = Profile::new("Old", "old.example.com", "bob");
        secrets
            .set(&password_key(&existing.id), "old-password")
            .unwrap();

        let err = save_profile(
            &store,
            &secrets,
            &values(),
            PASSWORD,
            false,
            Some(&existing),
        )
        .unwrap_err();

        assert!(err.contains("Could not save the profile"), "got: {err}");
        assert!(stored_password(&secrets, &existing.id).is_some());
    }
}
