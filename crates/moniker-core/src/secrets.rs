//! Secret storage: the [`SecretStore`] abstraction over the system keyring.
//!
//! Profile passwords (key [`password_key`]) and the sync passphrase (key
//! [`SYNC_PASSPHRASE_KEY`]) live here, never in the profile file.
//!
//! [`KeyringSecretStore`] talks to the freedesktop Secret Service over D-Bus
//! through the `keyring` crate's pure-Rust (zbus) backend, so there is no
//! libsecret or libdbus dependency. [`MemorySecretStore`] is the in-process
//! implementation for tests.
//!
//! Handling rules:
//!
//! * Values come back as [`Zeroizing<String>`] so they are wiped when dropped.
//! * [`SecretError`] never contains a secret value. Backend errors are turned
//!   into text by `describe`, which formats only the variants that cannot
//!   carry stored data: the `keyring` error type has variants that hold the raw
//!   secret bytes, so a keyring error is never printed with `{:?}` or `{}`.
//! * Stores that hold secrets do not derive `Debug`.
//! * `get` of a missing key is `Ok(None)`, `delete` of a missing key is `Ok(())`,
//!   and `set` overwrites.
//!
//! # No default keyring (issue #18)
//!
//! GNOME makes the Login keyring, the default one, with the password typed at
//! login, so a desktop that logs in automatically has none until someone makes
//! one. The `keyring` crate can read there but cannot store anything: it never
//! creates the default keyring, and reports it missing as "locked or access
//! denied". [`KeyringSecretStore::set`] then asks the Secret Service whether a
//! default keyring exists and, if it does not, does what its
//! [`MissingKeyring`] says: create one (GNOME asks the person to choose its
//! password) and store again, or fail with [`SecretError::NoDefaultKeyring`].

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use secret_service::EncryptionType;
use secret_service::blocking::SecretService;
use zeroize::Zeroizing;

/// Keyring key under which the sync passphrase is stored. Part of the stored
/// data's interface: changing it orphans passphrases already saved.
pub const SYNC_PASSPHRASE_KEY: &str = "sync-passphrase";

/// The fewest characters a sync key may have when it is set.
///
/// The client sends its sealed bundle to whatever endpoint the profile names
/// before that endpoint has shown it knows the key, so anyone who can
/// impersonate that endpoint (or watch the connection's far end) gets a blob to
/// guess the key against offline. Argon2id makes each guess slow; a long key
/// makes guessing hopeless. The limit applies when a key is *set*: a shorter key
/// that is already stored keeps working.
pub const MIN_SYNC_PASSPHRASE_CHARS: usize = 12;

/// Check a sync key that is about to be stored (by `moniker-server sync-key set` or the
/// GUI). The key is judged exactly as typed: no trimming, and length counts
/// characters, not bytes. `Err` is a fixed sentence that never contains the key;
/// an empty key and a too-short one get different sentences.
pub fn check_sync_passphrase(passphrase: &str) -> Result<(), &'static str> {
    if passphrase.is_empty() {
        Err("the sync key must not be empty")
    } else if passphrase.chars().count() < MIN_SYNC_PASSPHRASE_CHARS {
        // Keep the number in step with MIN_SYNC_PASSPHRASE_CHARS (a test checks).
        Err("the sync key must be at least 12 characters long")
    } else {
        Ok(())
    }
}

/// Service name used for keyring entries unless [`SERVICE_ENV`] overrides it.
pub const DEFAULT_SERVICE: &str = "moniker";

/// Environment variable that overrides the keyring service name (for tests and
/// for running a second, isolated set of entries). Empty counts as unset.
pub const SERVICE_ENV: &str = "MONIKER_KEYRING_SERVICE";

/// Keyring key read by [`SecretStore::probe`]. Never written.
const PROBE_KEY: &str = "moniker-probe";

/// Why a secret operation failed.
///
/// No variant carries a secret value; see the module docs.
#[derive(thiserror::Error, Debug)]
pub enum SecretError {
    /// No usable Secret Service: no session bus, nothing owns
    /// `org.freedesktop.secrets`, or the connection could not be set up.
    #[error("no usable Secret Service (system keyring): {0}")]
    Unavailable(String),
    /// The Secret Service was reached but the operation failed (keyring locked
    /// or access denied, an ambiguous entry, an unreadable value, ...).
    #[error("secret store error: {0}")]
    Backend(String),
    /// There is no default keyring to store into, and this store may not
    /// create one ([`MissingKeyring::Refuse`]). Nothing was stored.
    #[error("this computer has no default keyring")]
    NoDefaultKeyring,
    /// There is no default keyring, and creating one was canceled: the person
    /// dismissed the prompt, or no prompt could be shown. Nothing was stored.
    #[error("this computer has no default keyring, and creating one was canceled")]
    KeyringNotCreated,
}

/// What [`KeyringSecretStore::set`] does when the Secret Service has no
/// default keyring (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingKeyring {
    /// Create the default keyring and store again: for writes asked for at
    /// this machine (its windows, `moniker-server sync-key set` and `sync`,
    /// the syncs they start). GNOME asks the person to choose its password.
    Create,
    /// Store nothing and fail with [`SecretError::NoDefaultKeyring`]: for
    /// `moniker-server sync-serve`, which another machine starts, so that no
    /// prompt appears on this machine's screen unasked and the other machine's
    /// sync does not wait for one. Also for what only reads or tests.
    Refuse,
}

/// The label of a default keyring Moniker creates: GNOME's own name for it.
const DEFAULT_KEYRING_LABEL: &str = "Login";

/// A place to keep secrets by key. Implementations are shared across threads
/// (the GUI runs sync on a worker thread).
pub trait SecretStore: Send + Sync {
    /// The value stored under `key`, or `None` if there is none.
    fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError>;
    /// Store `value` under `key`, replacing any existing value.
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    /// Remove `key`. Removing a key that does not exist is not an error.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
    /// `Ok(())` if the store is usable, [`SecretError::Unavailable`] if not.
    /// Creates nothing.
    fn probe(&self) -> Result<(), SecretError>;
}

/// Keyring key for a profile's password: `profile-password:<id>`.
pub fn password_key(profile_id: &str) -> String {
    format!("profile-password:{profile_id}")
}

/// In-process store for tests. Interior mutability, no persistence.
#[derive(Default)]
pub struct MemorySecretStore {
    entries: Mutex<HashMap<String, Zeroizing<String>>>,
}

impl MemorySecretStore {
    fn entries(&self) -> MutexGuard<'_, HashMap<String, Zeroizing<String>>> {
        // A panic while holding the lock cannot leave the map half-updated
        // (every critical section is a single map call), so recover from poison.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Prints only how many entries there are, never keys or values.
impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecretStore")
            .field("entries", &self.entries().len())
            .finish()
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
        Ok(self
            .entries()
            .get(key)
            .map(|v| Zeroizing::new(v.as_str().to_owned())))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        // The replaced value, if any, is zeroized when it drops.
        self.entries()
            .insert(key.to_owned(), Zeroizing::new(value.to_owned()));
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.entries().remove(key);
        Ok(())
    }

    fn probe(&self) -> Result<(), SecretError> {
        Ok(())
    }
}

/// The service name to use given the value of [`SERVICE_ENV`] (if any).
fn service_name_from(env_value: Option<&str>) -> String {
    match env_value {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => DEFAULT_SERVICE.to_owned(),
    }
}

/// The system keyring (freedesktop Secret Service) via the `keyring` crate.
///
/// Each secret is one keyring entry: the service attribute is this store's
/// service name and the user/account attribute is the key string. It holds no
/// secrets itself, so a derived `Debug` is fine.
///
/// The `keyring` crate sets up its Secret Service connection once per process
/// and remembers the outcome: if the session had no Secret Service the first
/// time it was used, every later call in that process reports it unavailable.
#[derive(Debug)]
pub struct KeyringSecretStore {
    service: String,
    missing: MissingKeyring,
}

impl KeyringSecretStore {
    /// A store using the service name from `$MONIKER_KEYRING_SERVICE`, read
    /// once here (empty or not valid Unicode counts as unset), else `"moniker"`.
    /// `missing` says what a write does with no default keyring.
    pub fn new(missing: MissingKeyring) -> Self {
        let env_value = std::env::var(SERVICE_ENV).ok();
        Self::with_service(service_name_from(env_value.as_deref()), missing)
    }

    /// A store using an explicit service name (tests use a throwaway one).
    pub fn with_service(service: impl Into<String>, missing: MissingKeyring) -> Self {
        Self {
            service: service.into(),
            missing,
        }
    }

    /// What a write does with no default keyring.
    pub fn missing_keyring(&self) -> MissingKeyring {
        self.missing
    }

    /// The service name of this store's entries.
    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, SecretError> {
        // `Entry::new` only says "no default store" when the one-time backend
        // setup failed; ask for the setup result itself to report the cause.
        if let Err(err) = keyring::Entry::store_status() {
            return Err(SecretError::Unavailable(describe(err)));
        }
        keyring::Entry::new(&self.service, key).map_err(|err| backend_error(&err))
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
        match self.entry(key)?.get_secret() {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(value) => Ok(Some(Zeroizing::new(value))),
                Err(err) => {
                    // Wipe the rejected bytes; do not put them in the error.
                    drop(Zeroizing::new(err.into_bytes()));
                    Err(SecretError::Backend(
                        "stored secret is not valid UTF-8".to_owned(),
                    ))
                }
            },
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(backend_error(&err)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let entry = self.entry(key)?;
        set_creating_keyring(self.missing, &SessionKeyring, || {
            entry.set_secret(value.as_bytes())
        })
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(backend_error(&err)),
        }
    }

    /// Looks up a fixed dummy key; "no such entry" is a success. Writes nothing.
    ///
    /// This proves the Secret Service is reachable, not that the keyring is
    /// unlocked: a lookup that matches nothing never asks to unlock it.
    fn probe(&self) -> Result<(), SecretError> {
        let entry = self.entry(PROBE_KEY).map_err(|err| match err {
            SecretError::Backend(msg) => SecretError::Unavailable(msg),
            other => other,
        })?;
        match entry.get_secret() {
            Ok(bytes) => {
                drop(Zeroizing::new(bytes));
                Ok(())
            }
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(SecretError::Unavailable(describe(&err))),
        }
    }
}

/// The Secret Service's default keyring, as [`set_creating_keyring`] sees it.
trait DefaultKeyring {
    /// `Some(true)` if there is a default keyring, `Some(false)` if there is
    /// none, `None` if the Secret Service could not tell.
    fn exists(&self) -> Option<bool>;
    /// Ask the Secret Service for one. [`SecretError::KeyringNotCreated`] if
    /// the person canceled it.
    fn create(&self) -> Result<(), SecretError>;
}

/// Store with `store`; with no default keyring, create one and store once
/// more, or refuse, as `missing` says (issue #18).
fn set_creating_keyring(
    missing: MissingKeyring,
    keyring: &dyn DefaultKeyring,
    mut store: impl FnMut() -> Result<(), keyring::Error>,
) -> Result<(), SecretError> {
    let err = match store() {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };
    // Asked again because the Secret Service's "no result" is not only for an
    // alias that names nothing. Anything it cannot confirm, or any other
    // failure (a locked keyring whose unlock was canceled, say), is reported
    // as it is.
    let no_keyring = names_no_keyring(&err) && keyring.exists() == Some(false);
    if !no_keyring {
        return Err(backend_error(&err));
    }
    match missing {
        MissingKeyring::Refuse => Err(SecretError::NoDefaultKeyring),
        MissingKeyring::Create => {
            keyring.create()?;
            store().map_err(|err| backend_error(&err))
        }
    }
}

/// Whether `err` is how the `keyring` crate fails a write with no default
/// keyring: no access, caused by the Secret Service's "no result" for the
/// alias `default` (a locked keyring is no access too, with another cause).
fn names_no_keyring(err: &keyring::Error) -> bool {
    match err {
        keyring::Error::NoStorageAccess(cause) => matches!(
            cause.downcast_ref::<secret_service::Error>(),
            Some(secret_service::Error::NoResult)
        ),
        _ => false,
    }
}

/// The default keyring of the Secret Service on the session bus, looked at
/// and created on a connection of its own (no secret travels on it).
struct SessionKeyring;

impl DefaultKeyring for SessionKeyring {
    fn exists(&self) -> Option<bool> {
        let service = SecretService::connect(EncryptionType::Plain).ok()?;
        match service.get_default_collection() {
            Ok(_) => Some(true),
            // The alias "default" names no keyring.
            Err(secret_service::Error::NoResult) => Some(false),
            Err(_) => None,
        }
    }

    /// Blocks until the person answers the prompt: callers already run the
    /// keyring off the GUI's main thread (issue #4).
    fn create(&self) -> Result<(), SecretError> {
        // The causes are the Secret Service's own; creating a keyring sends no
        // secret, so none can carry one.
        let failed = |err: secret_service::Error| {
            SecretError::Backend(format!(
                "this computer has no default keyring, and creating one failed: {err}"
            ))
        };
        let service = SecretService::connect(EncryptionType::Plain).map_err(failed)?;
        match service.create_collection(DEFAULT_KEYRING_LABEL, "default") {
            Ok(_) => Ok(()),
            Err(secret_service::Error::Prompt) => Err(SecretError::KeyringNotCreated),
            Err(err) => Err(failed(err)),
        }
    }
}

/// Map a backend error from an operation on one entry.
fn backend_error(err: &keyring::Error) -> SecretError {
    match err {
        keyring::Error::NoDefaultStore => SecretError::Unavailable(describe(err)),
        _ => SecretError::Backend(describe(err)),
    }
}

/// Text for a backend error that is safe to show and log.
///
/// Only variants whose payload is an error from the platform's own stack are
/// formatted. `BadEncoding` and `BadDataFormat` carry the raw secret bytes and
/// `Ambiguous` carries entries, so those get fixed text.
fn describe(err: &keyring::Error) -> String {
    use keyring::Error;
    match err {
        Error::NoDefaultStore => "no credential store could be set up".to_owned(),
        Error::PlatformFailure(cause) => format!("platform failure: {cause}"),
        Error::NoStorageAccess(cause) => {
            format!("keyring is locked or access was denied: {cause}")
        }
        Error::NoEntry => "no such entry".to_owned(),
        Error::BadEncoding(_) | Error::BadDataFormat(..) => {
            "stored secret could not be read".to_owned()
        }
        Error::Ambiguous(_) => "more than one keyring entry matches".to_owned(),
        Error::TooLong(..) | Error::Invalid(..) => "invalid keyring key or service name".to_owned(),
        _ => "unexpected keyring error".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_key_format() {
        assert_eq!(password_key("abc"), "profile-password:abc");
    }

    #[test]
    fn sync_passphrase_key_is_stable() {
        // Part of the stored-data interface: changing it orphans saved passphrases.
        assert_eq!(SYNC_PASSPHRASE_KEY, "sync-passphrase");
    }

    #[test]
    fn set_then_get_roundtrip() {
        let store = MemorySecretStore::default();
        store.set("k", "hunter2").unwrap();
        let got = store.get("k").unwrap().expect("value present");
        assert_eq!(got.as_str(), "hunter2");
    }

    #[test]
    fn get_missing_is_none() {
        let store = MemorySecretStore::default();
        assert!(store.get("nope").unwrap().is_none());
    }

    #[test]
    fn delete_missing_is_ok() {
        let store = MemorySecretStore::default();
        store.delete("nope").unwrap();
    }

    #[test]
    fn delete_removes_value() {
        let store = MemorySecretStore::default();
        store.set("k", "v").unwrap();
        store.delete("k").unwrap();
        assert!(store.get("k").unwrap().is_none());
        // Deleting again is still fine.
        store.delete("k").unwrap();
    }

    #[test]
    fn set_overwrites() {
        let store = MemorySecretStore::default();
        store.set("k", "first").unwrap();
        store.set("k", "second").unwrap();
        assert_eq!(store.get("k").unwrap().unwrap().as_str(), "second");
    }

    #[test]
    fn keys_are_independent() {
        let store = MemorySecretStore::default();
        store.set(&password_key("a"), "pa").unwrap();
        store.set(&password_key("b"), "pb").unwrap();
        store.set(SYNC_PASSPHRASE_KEY, "sync").unwrap();
        assert_eq!(
            store.get(&password_key("a")).unwrap().unwrap().as_str(),
            "pa"
        );
        assert_eq!(
            store.get(&password_key("b")).unwrap().unwrap().as_str(),
            "pb"
        );
        assert_eq!(
            store.get(SYNC_PASSPHRASE_KEY).unwrap().unwrap().as_str(),
            "sync"
        );
    }

    // ----- sync key strength -----

    #[test]
    fn an_empty_sync_key_is_rejected() {
        let msg = check_sync_passphrase("").unwrap_err();
        assert!(msg.contains("empty"), "{msg}");
    }

    #[test]
    fn a_short_sync_key_is_rejected_and_the_minimum_is_accepted() {
        assert_eq!(MIN_SYNC_PASSPHRASE_CHARS, 12);
        let eleven = "a".repeat(MIN_SYNC_PASSPHRASE_CHARS - 1);
        let twelve = "a".repeat(MIN_SYNC_PASSPHRASE_CHARS);
        let msg = check_sync_passphrase(&eleven).unwrap_err();
        assert!(msg.contains("at least 12"), "{msg}");
        assert_eq!(check_sync_passphrase(&twelve), Ok(()));
        assert_eq!(check_sync_passphrase(&format!("{twelve}!")), Ok(()));
        assert!(check_sync_passphrase("hunter2").is_err());
    }

    #[test]
    fn the_length_counts_characters_not_bytes() {
        // Twelve two-byte and twelve four-byte characters are long enough...
        assert_eq!(check_sync_passphrase(&"\u{e9}".repeat(12)), Ok(()));
        assert_eq!(check_sync_passphrase(&"\u{1f511}".repeat(12)), Ok(()));
        // ...and four three-byte characters (12 bytes) are not.
        assert!(check_sync_passphrase(&"\u{65e5}".repeat(4)).is_err());
        assert!(check_sync_passphrase(&"\u{1f511}".repeat(11)).is_err());
    }

    #[test]
    fn the_key_is_counted_as_typed() {
        // No trimming: spaces are characters, and so are the ends of the key.
        assert_eq!(check_sync_passphrase(&" ".repeat(12)), Ok(()));
        assert!(check_sync_passphrase(&format!("  {}  ", "a".repeat(7))).is_err());
        assert_eq!(
            check_sync_passphrase(&format!("  {}  ", "a".repeat(8))),
            Ok(())
        );
    }

    #[test]
    fn the_messages_are_distinct_fixed_text_and_never_show_the_value() {
        let empty = check_sync_passphrase("").unwrap_err();
        let short = check_sync_passphrase("SENTINEL-1").unwrap_err();
        assert_ne!(empty, short);
        assert!(!short.contains("SENTINEL"), "{short}");
        assert!(
            short.contains(&MIN_SYNC_PASSPHRASE_CHARS.to_string()),
            "{short}"
        );
        for msg in [empty, short] {
            assert!(
                msg.is_ascii() && !msg.chars().any(char::is_control),
                "{msg:?}"
            );
        }
        // Whatever is passed in, the text is the same one.
        assert_eq!(
            check_sync_passphrase("a").unwrap_err(),
            check_sync_passphrase("b").unwrap_err()
        );
    }

    #[test]
    fn memory_probe_is_ok() {
        MemorySecretStore::default().probe().unwrap();
    }

    #[test]
    fn memory_store_debug_does_not_leak_secrets() {
        let store = MemorySecretStore::default();
        store.set("k", "correct-horse-battery-staple").unwrap();
        store.set("k2", "another-secret").unwrap();
        let shown = format!("{store:?}");
        assert!(!shown.contains("correct-horse-battery-staple"), "{shown}");
        assert!(!shown.contains("another-secret"), "{shown}");
        assert!(shown.contains('2'), "expected the entry count in {shown}");
    }

    #[test]
    fn memory_store_is_usable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MemorySecretStore>();
        assert_send_sync::<KeyringSecretStore>();
        let _: &dyn SecretStore = &MemorySecretStore::default();
    }

    #[test]
    fn service_name_defaults_when_unset() {
        assert_eq!(service_name_from(None), "moniker");
    }

    #[test]
    fn service_name_defaults_when_empty() {
        assert_eq!(service_name_from(Some("")), "moniker");
    }

    #[test]
    fn service_name_uses_override() {
        assert_eq!(service_name_from(Some("moniker-test")), "moniker-test");
    }

    #[test]
    fn with_service_sets_service() {
        assert_eq!(
            KeyringSecretStore::with_service("x", MissingKeyring::Refuse).service,
            "x"
        );
    }

    #[test]
    fn a_store_says_what_it_does_without_a_default_keyring() {
        for missing in [MissingKeyring::Create, MissingKeyring::Refuse] {
            assert_eq!(KeyringSecretStore::new(missing).missing_keyring(), missing);
            assert_eq!(
                KeyringSecretStore::with_service("x", missing).missing_keyring(),
                missing
            );
        }
    }

    // ----- no default keyring (issue #18) -----

    use std::cell::Cell;

    /// A stand-in for the Secret Service's default keyring: whether there is
    /// one, and what asking for one gives. Counts both.
    struct FakeKeyring {
        exists: Option<bool>,
        create: fn() -> Result<(), SecretError>,
        asked: Cell<usize>,
        created: Cell<usize>,
    }

    impl FakeKeyring {
        fn new(exists: Option<bool>, create: fn() -> Result<(), SecretError>) -> Self {
            Self {
                exists,
                create,
                asked: Cell::new(0),
                created: Cell::new(0),
            }
        }
    }

    impl DefaultKeyring for FakeKeyring {
        fn exists(&self) -> Option<bool> {
            self.asked.set(self.asked.get() + 1);
            self.exists
        }

        fn create(&self) -> Result<(), SecretError> {
            self.created.set(self.created.get() + 1);
            (self.create)()
        }
    }

    fn created() -> Result<(), SecretError> {
        Ok(())
    }

    fn canceled() -> Result<(), SecretError> {
        Err(SecretError::KeyringNotCreated)
    }

    /// What the `keyring` crate gives for a write with no default keyring.
    fn no_default() -> keyring::Error {
        keyring::Error::NoStorageAccess(Box::new(secret_service::Error::NoResult))
    }

    /// What it gives for a keyring whose unlock was canceled, the default one
    /// or another that holds the entry.
    fn canceled_unlock() -> keyring::Error {
        keyring::Error::NoStorageAccess(Box::new(secret_service::Error::Prompt))
    }

    /// Runs `set_creating_keyring` with a store that gives `results` in turn,
    /// and returns its result and how many times it stored.
    fn run(
        missing: MissingKeyring,
        keyring: &FakeKeyring,
        results: Vec<Result<(), keyring::Error>>,
    ) -> (Result<(), SecretError>, usize) {
        let mut results = results.into_iter();
        let mut stores = 0;
        let out = set_creating_keyring(missing, keyring, || {
            stores += 1;
            results
                .next()
                .expect("stored more often than the test expects")
        });
        (out, stores)
    }

    #[test]
    fn the_two_texts_are_the_ones_decided() {
        assert_eq!(
            SecretError::KeyringNotCreated.to_string(),
            "this computer has no default keyring, and creating one was canceled"
        );
        assert_eq!(
            SecretError::NoDefaultKeyring.to_string(),
            "this computer has no default keyring"
        );
    }

    #[test]
    fn a_write_that_works_never_asks_about_the_keyring() {
        for missing in [MissingKeyring::Create, MissingKeyring::Refuse] {
            let keyring = FakeKeyring::new(Some(false), created);
            let (out, stores) = run(missing, &keyring, vec![Ok(())]);
            assert!(out.is_ok(), "{out:?}");
            assert_eq!(
                (stores, keyring.asked.get(), keyring.created.get()),
                (1, 0, 0)
            );
        }
    }

    #[test]
    fn with_no_default_keyring_one_is_created_and_the_secret_stored() {
        let keyring = FakeKeyring::new(Some(false), created);
        let (out, stores) = run(
            MissingKeyring::Create,
            &keyring,
            vec![Err(no_default()), Ok(())],
        );
        assert!(out.is_ok(), "{out:?}");
        assert_eq!((stores, keyring.created.get()), (2, 1));
    }

    #[test]
    fn a_canceled_keyring_stores_nothing_and_says_so() {
        let keyring = FakeKeyring::new(Some(false), canceled);
        let (out, stores) = run(MissingKeyring::Create, &keyring, vec![Err(no_default())]);
        assert!(
            matches!(out, Err(SecretError::KeyringNotCreated)),
            "{out:?}"
        );
        assert_eq!((stores, keyring.created.get()), (1, 1));
    }

    #[test]
    fn a_keyring_that_could_not_be_created_says_why() {
        fn failed() -> Result<(), SecretError> {
            Err(SecretError::Backend("the bus went away".to_owned()))
        }
        let keyring = FakeKeyring::new(Some(false), failed);
        let (out, stores) = run(MissingKeyring::Create, &keyring, vec![Err(no_default())]);
        assert!(
            matches!(&out, Err(SecretError::Backend(why)) if why == "the bus went away"),
            "{out:?}"
        );
        assert_eq!(stores, 1);
    }

    #[test]
    fn refuse_never_creates_a_keyring() {
        let keyring = FakeKeyring::new(Some(false), created);
        let (out, stores) = run(MissingKeyring::Refuse, &keyring, vec![Err(no_default())]);
        assert!(matches!(out, Err(SecretError::NoDefaultKeyring)), "{out:?}");
        assert_eq!(
            (stores, keyring.asked.get(), keyring.created.get()),
            (1, 1, 0)
        );
    }

    #[test]
    fn a_locked_keyring_that_exists_keeps_todays_error() {
        for missing in [MissingKeyring::Create, MissingKeyring::Refuse] {
            let keyring = FakeKeyring::new(Some(true), created);
            let (out, stores) = run(missing, &keyring, vec![Err(canceled_unlock())]);
            assert!(
                matches!(&out, Err(SecretError::Backend(why))
                    if why == "keyring is locked or access was denied: SS error: prompt dismissed"),
                "{out:?}"
            );
            assert_eq!((stores, keyring.created.get()), (1, 0));
        }
    }

    #[test]
    fn a_canceled_unlock_never_creates_a_keyring() {
        // The entry is in a keyring that is not the default and there is no
        // default: canceling its unlock is not a reason to make one.
        for missing in [MissingKeyring::Create, MissingKeyring::Refuse] {
            let keyring = FakeKeyring::new(Some(false), created);
            let (out, stores) = run(missing, &keyring, vec![Err(canceled_unlock())]);
            assert!(
                matches!(&out, Err(SecretError::Backend(why))
                    if why == "keyring is locked or access was denied: SS error: prompt dismissed"),
                "{out:?}"
            );
            assert_eq!((stores, keyring.created.get()), (1, 0));
        }
    }

    #[test]
    fn a_default_keyring_that_turns_out_to_exist_is_not_created() {
        // Made by someone else since the write failed, say.
        let keyring = FakeKeyring::new(Some(true), created);
        let (out, stores) = run(MissingKeyring::Create, &keyring, vec![Err(no_default())]);
        assert!(
            matches!(&out, Err(SecretError::Backend(why))
                if why.starts_with("keyring is locked or access was denied")),
            "{out:?}"
        );
        assert_eq!(
            (stores, keyring.asked.get(), keyring.created.get()),
            (1, 1, 0)
        );
    }

    #[test]
    fn a_keyring_that_cannot_be_looked_at_is_not_created() {
        let keyring = FakeKeyring::new(None, created);
        let (out, stores) = run(MissingKeyring::Create, &keyring, vec![Err(no_default())]);
        assert!(
            matches!(&out, Err(SecretError::Backend(why))
                if why.starts_with("keyring is locked or access was denied")),
            "{out:?}"
        );
        assert_eq!(
            (stores, keyring.asked.get(), keyring.created.get()),
            (1, 1, 0)
        );
    }

    #[test]
    fn other_failures_never_ask_about_the_keyring() {
        let keyring = FakeKeyring::new(Some(false), created);
        let failure = keyring::Error::PlatformFailure(Box::new(std::io::Error::other("boom")));
        let (out, stores) = run(MissingKeyring::Create, &keyring, vec![Err(failure)]);
        assert!(
            matches!(&out, Err(SecretError::Backend(why)) if why == "platform failure: boom"),
            "{out:?}"
        );
        assert_eq!(
            (stores, keyring.asked.get(), keyring.created.get()),
            (1, 0, 0)
        );
    }

    #[test]
    fn a_write_that_fails_again_after_creating_is_tried_only_twice() {
        let keyring = FakeKeyring::new(Some(false), created);
        let (out, stores) = run(
            MissingKeyring::Create,
            &keyring,
            vec![Err(no_default()), Err(no_default())],
        );
        assert!(
            matches!(&out, Err(SecretError::Backend(why))
                if why.starts_with("keyring is locked or access was denied")),
            "{out:?}"
        );
        assert_eq!((stores, keyring.created.get()), (2, 1));
    }

    /// Talks to the real Secret Service (needs an unlocked login keyring), so it
    /// is ignored by default. Run with:
    /// `cargo test -p moniker-core -- --ignored keyring_roundtrip`
    #[test]
    #[ignore = "needs a running, unlocked Secret Service"]
    fn keyring_roundtrip() {
        let store = KeyringSecretStore::with_service(
            service_name_from(Some("moniker-test")),
            MissingKeyring::Refuse,
        );
        let key = format!("moniker-test-{}", uuid::Uuid::new_v4());

        /// Deletes the entry however the test exits, including a failed assertion.
        struct Cleanup<'a>(&'a KeyringSecretStore, &'a str);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.delete(self.1);
            }
        }
        let _cleanup = Cleanup(&store, &key);

        store.probe().expect("secret service reachable");
        assert!(
            store.get(&key).unwrap().is_none(),
            "fresh key must be absent"
        );

        store.set(&key, "first-value").unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().as_str(), "first-value");

        store.set(&key, "second value with ünïcode").unwrap();
        assert_eq!(
            store.get(&key).unwrap().unwrap().as_str(),
            "second value with ünïcode"
        );

        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        store.delete(&key).unwrap(); // deleting a missing key is Ok
    }
}
