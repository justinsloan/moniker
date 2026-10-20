//! Sync between two Moniker instances over a byte stream (in practice the
//! stdin/stdout of `ssh <host> moniker-server sync-serve`).
//!
//! # Wire format
//!
//! Every message is a frame: a `u32` big-endian length, then that many bytes,
//! of which the first is the frame kind and the rest the payload. The length
//! therefore counts the kind byte, is at least 1, and is at most [`MAX_FRAME`];
//! [`read_frame`] rejects a larger one before allocating anything.
//!
//! | kind | name   | payload                                          |
//! |------|--------|--------------------------------------------------|
//! | 1    | HELLO  | `[protocol version]` (currently 1)               |
//! | 2    | BUNDLE | a sealed bundle (see [`crate::crypto`])          |
//! | 3    | ERROR  | `[code u8]` then a UTF-8 message                 |
//!
//! Error codes: 1 wrong sync key, 2 version mismatch, 3 no secret store,
//! 255 anything else. Messages are fixed, non-secret text.
//!
//! # Exchange
//!
//! ```text
//! client                                  server
//!   HELLO [1]  ───────────────────────▶   ready()? passphrase set? version 1?
//!              ◀───────────────────────   HELLO [1]            (or ERROR, stop)
//!   BUNDLE(seal(export))  ────────────▶   open, validate, merge with export,
//!                                         seal the merged set, apply
//!              ◀───────────────────────   BUNDLE(seal(merged)) (or ERROR, stop)
//!   open, validate, apply
//! ```
//!
//! The client sends its bundle only after the server's HELLO, so a server that
//! cannot take part (no Secret Service, wrong version) is found out before any
//! profile data leaves the client. The sync passphrase never goes on the wire;
//! only sealed bundles do. A side that fails before its peer is done sends an
//! ERROR frame (best effort) so the peer reports the real reason.
//!
//! # Nothing partial
//!
//! Each side changes its own state only after it has read the peer's whole
//! bundle, opened it with the passphrase, and validated every entry in it. Any
//! failure before that (a dropped connection, a wrong key, an invalid profile,
//! a timeout) leaves the profile file and the secret store exactly as they
//! were.
//!
//! [`LocalState::apply`] stores the secrets first and publishes the profile
//! file last, keeping an undo log of every secret it touches. If a secret
//! operation or the file write fails, the secrets are rolled back (best
//! effort) and the file is not changed. The invariant is: **a profile version
//! on disk implies its secret is already stored.** Any failure leaves the
//! secrets at worst *ahead* of the profile file, never behind, so the next
//! sync converges: a secret that is ahead is overwritten with the same value
//! when its profile version arrives, and a secret deleted before a failed
//! write is deleted again because the tombstone still wins the merge.
//!
//! One window remains that a two-message protocol cannot close: the server
//! applies before its reply is delivered, so a connection lost at that moment
//! leaves only the server updated. Both sides are then internally consistent,
//! and the next sync converges them.
//!
//! Entries from the peer are untrusted: every one is checked with
//! [`Profile::validate`] and any invalid entry rejects the whole sync.
//!
//! # Passwords: set and changed travel, removal does not
//!
//! A password reaches the other machine by being set or changed. Clearing one
//! (the editor's "Remove stored password") stays on the machine where it was
//! done, and a later sync from a machine that still has the password can bring
//! it back. This is an intentional limit; see `plan_apply`.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::bundle::{BundleEntry, merge};
use crate::crypto::{self, CryptoError};
use crate::profile::Profile;
use crate::secrets::{SecretError, SecretStore, password_key};
use crate::ssh::SshCommand;
use crate::store::{ProfileStore, StoreError};

/// Largest frame accepted or sent, kind byte included (16 MiB).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// The protocol version carried in HELLO.
pub const PROTOCOL_VERSION: u8 = 1;
/// Frame kind: HELLO `[version]`.
pub const KIND_HELLO: u8 = 1;
/// Frame kind: BUNDLE (sealed bytes).
pub const KIND_BUNDLE: u8 = 2;
/// Frame kind: ERROR `[code][utf8 message]`.
pub const KIND_ERROR: u8 = 3;
/// Overall limit on one [`sync_over_ssh`] run, connection setup included.
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

const CODE_WRONG_SYNC_KEY: u8 = 1;
const CODE_VERSION_MISMATCH: u8 = 2;
const CODE_NO_SECRET_STORE: u8 = 3;
const CODE_OTHER: u8 = 255;

/// Longest message put in an outgoing ERROR frame, in bytes.
const MAX_SENT_MESSAGE: usize = 1024;
/// Longest text taken from the peer (ERROR message, ssh stderr), in bytes.
const MAX_SHOWN_MESSAGE: usize = 300;
/// How much of the child's stderr is kept (the end of it).
const STDERR_TAIL: usize = 4096;
/// How many of the last stderr lines are shown.
const STDERR_LINES: usize = 3;
/// How long to wait for the child to exit once the exchange is over.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for the pipe threads once the child is reaped.
const THREAD_GRACE: Duration = Duration::from_secs(1);
/// Read size and queue depth for the child's stdout.
const PIPE_CHUNK: usize = 64 * 1024;
const PIPE_QUEUE: usize = 8;

/// Why a sync failed.
///
/// Messages never contain a passphrase, a password or profile data.
#[derive(thiserror::Error, Debug)]
pub enum SyncError {
    #[error("the sync passphrases differ (or the data was corrupted on the way)")]
    WrongSyncKey,
    #[error("the two machines run incompatible Moniker sync versions; update both")]
    VersionMismatch,
    #[error("no usable Secret Service (system keyring) to store passwords in")]
    NoSecretStore,
    #[error("sync message exceeds the 16 MiB limit")]
    FrameTooLarge,
    #[error("moniker-server is not installed on the remote machine (or not on its PATH)")]
    ServerNotFound,
    #[error(
        "the remote host key is not known or has changed; connect once in a terminal to check and accept it"
    )]
    HostKeyUnknown,
    /// Raised before this machine applied anything.
    #[error(
        "the other machine did not finish in time; it may or may not have applied the changes (this machine was not changed)"
    )]
    Timeout,
    #[error("the sync passphrase is not set")]
    EmptyPassphrase,
    /// The peer broke the frame protocol (bad length, unexpected frame).
    #[error("sync protocol error: {0}")]
    Protocol(String),
    /// The other side or the ssh transport failed; the text comes from there
    /// and has been sanitized.
    #[error("remote error: {0}")]
    Remote(String),
    /// The local profile store or secret store failed, or data was invalid.
    #[error("profile data error: {0}")]
    Store(String),
    #[error("sync connection error: {0}")]
    Io(#[from] io::Error),
}

/// What a sync changed on one side, relative to its profile file just before
/// the change was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncOutcome {
    /// Profiles that were absent or deleted and are now live.
    pub added: usize,
    /// Live profiles whose fields or password changed.
    pub updated: usize,
    /// Live profiles that are now deleted.
    pub removed: usize,
}

/// One side's data, as the protocol sees it.
pub trait SyncTarget {
    /// `Ok` if received data can be stored (the secret store is usable).
    fn ready(&self) -> Result<(), SyncError>;
    /// Every profile, tombstones included, live ones with their passwords.
    fn export(&self) -> Result<Vec<BundleEntry>, SyncError>;
    /// Store the merged set. Entries are already validated by the protocol.
    fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError>;
}

/// The real sync target: the profile file plus the secret store.
pub struct LocalState {
    store: ProfileStore,
    secrets: Box<dyn SecretStore>,
}

impl LocalState {
    pub fn new(store: ProfileStore, secrets: Box<dyn SecretStore>) -> Self {
        LocalState { store, secrets }
    }

    /// The profile store, as of the last write by [`apply`](SyncTarget::apply)
    /// (or as it was opened).
    pub fn store(&self) -> &ProfileStore {
        &self.store
    }
}

impl SyncTarget for LocalState {
    fn ready(&self) -> Result<(), SyncError> {
        self.secrets.probe().map_err(|_| SyncError::NoSecretStore)
    }

    /// Reads the profile file afresh (not the snapshot), so edits made since
    /// the store was opened are included.
    fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
        let fresh = ProfileStore::open(self.store.path()).map_err(store_error)?;
        with_passwords(&fresh.all(), &*self.secrets).map_err(secret_error)
    }

    /// Merges `merged` into the profile file as it is now, under the store
    /// lock, so an edit made by another process since [`export`] is kept when
    /// it is newer.
    ///
    /// Still under the lock, the secrets go first: the passwords of incoming
    /// winners are stored and the passwords of profiles this sync deletes are
    /// removed, each previous value recorded in an undo log. Only then is the
    /// profile file written. If a secret operation or the file write fails, the
    /// secrets are restored from the log (best effort) and the file is left as
    /// it was. Invariant: a profile version on disk implies its secret is
    /// already stored; a failure leaves the secrets at worst ahead of the file,
    /// never behind, and the next sync converges them.
    ///
    /// [`export`]: SyncTarget::export
    fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
        validate_entries(&merged)?;
        let secrets: &dyn SecretStore = &*self.secrets;
        let mut undo = UndoLog::default();
        // `update` wants a StoreError from the closure; keep the real cause.
        let mut secret_failure = None;
        let result = self.store.update(|disk| {
            let staged = with_passwords(disk, secrets).and_then(|local| {
                let (profiles, (outcome, ops)) = plan_apply(local, merged);
                undo.run(secrets, &ops)?;
                Ok((profiles, outcome))
            });
            staged.map_err(|e| {
                // Roll back while the lock is still held; `update` then
                // writes nothing.
                undo.roll_back(secrets);
                secret_failure = Some(e);
                StoreError::Io(io::Error::other("a secret store operation failed"))
            })
        });
        match result {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                // If the file write failed, the secrets were stored: undo them.
                // (After a secret failure the log is already empty.)
                undo.roll_back(secrets);
                Err(match secret_failure {
                    Some(se) => secret_error(se),
                    None => store_error(e),
                })
            }
        }
    }
}

/// A change to the secret store decided while the profile file was locked.
enum SecretOp {
    Set {
        id: String,
        password: Zeroizing<String>,
    },
    Delete {
        id: String,
    },
}

impl SecretOp {
    fn id(&self) -> &str {
        match self {
            SecretOp::Set { id, .. } | SecretOp::Delete { id } => id,
        }
    }
}

/// The previous value of every secret an apply touched, in order, so the
/// apply can be undone. Values are zeroized when dropped.
#[derive(Default)]
struct UndoLog(Vec<(String, Option<Zeroizing<String>>)>);

impl UndoLog {
    /// Carry out `ops`, recording each secret's previous value (or absence)
    /// before touching it. Stops at the first failure.
    fn run(&mut self, secrets: &dyn SecretStore, ops: &[SecretOp]) -> Result<(), SecretError> {
        for op in ops {
            let key = password_key(op.id());
            let previous = secrets.get(&key)?;
            self.0.push((key.clone(), previous));
            match op {
                SecretOp::Set { password, .. } => secrets.set(&key, password)?,
                SecretOp::Delete { .. } => secrets.delete(&key)?,
            }
        }
        Ok(())
    }

    /// Restore the recorded values, newest first, and empty the log. Best
    /// effort: a secret that cannot be restored stays as the sync left it,
    /// ahead of the profile file, which the next sync brings level.
    fn roll_back(&mut self, secrets: &dyn SecretStore) {
        while let Some((key, previous)) = self.0.pop() {
            let _ = match &previous {
                Some(value) => secrets.set(&key, value),
                None => secrets.delete(&key),
            };
        }
    }
}

/// Merge `incoming` into `local` (the file's current entries with their
/// passwords). Returns the profiles to write, the outcome, and the secret
/// store changes to make before the write.
///
/// # Removing a password does not propagate (intentional, documented limit)
///
/// An incoming winner that carries no password does **not** remove a password
/// already stored here: the stored one is left alone. A password therefore only
/// ever travels by being set or changed, never by being cleared. So when the
/// editor's "Remove stored password" is used on one machine, the removal is
/// that machine's alone: the other machines keep their copy, and a later sync
/// from a machine that still has the password can bring it back (its entry
/// wins the merge when it is the newer edit, or the tie-break picks it). Only
/// deleting the whole profile propagates, through its tombstone.
///
/// This is deliberate: if removal propagated, a machine with a reset or
/// freshly created keyring (every profile there has no password) could wipe
/// the passwords on all its peers with one sync. Whether removal should
/// propagate is an open design question for the product owner.
fn plan_apply(
    local: Vec<BundleEntry>,
    incoming: Vec<BundleEntry>,
) -> (Vec<Profile>, (SyncOutcome, Vec<SecretOp>)) {
    let before: BTreeMap<String, BundleEntry> = local
        .iter()
        .map(|e| (e.profile.id.clone(), e.clone()))
        .collect();
    let winners = merge(local, incoming);

    let mut outcome = SyncOutcome::default();
    let mut ops = Vec::new();
    let mut profiles = Vec::with_capacity(winners.len());
    for w in &winners {
        let prev = before.get(&w.profile.id);
        let was_live = prev.is_some_and(|p| !p.profile.deleted);
        if w.profile.deleted {
            // Only a profile this sync deletes has a secret to remove; older
            // tombstones were cleaned up when they were applied.
            if was_live {
                outcome.removed += 1;
                ops.push(SecretOp::Delete {
                    id: w.profile.id.clone(),
                });
            }
        } else {
            // A winner taken from the file has the file's password, so this
            // is only ever an incoming password. An incoming winner without a
            // password leaves the stored one alone (see the limit documented
            // on this function).
            let new_password = match (&w.password, prev) {
                (Some(pw), Some(p)) if p.password.as_ref() == Some(pw) => None,
                (pw, _) => pw.as_ref(),
            };
            match prev {
                Some(p) if was_live => {
                    if p.profile != w.profile || new_password.is_some() {
                        outcome.updated += 1;
                    }
                }
                _ => outcome.added += 1,
            }
            if let Some(pw) = new_password {
                ops.push(SecretOp::Set {
                    id: w.profile.id.clone(),
                    password: Zeroizing::new(pw.clone()),
                });
            }
        }
        profiles.push(w.profile.clone());
    }
    (profiles, (outcome, ops))
}

/// Entries for `profiles`: live ones with their stored password, tombstones
/// without.
fn with_passwords(
    profiles: &[Profile],
    secrets: &dyn SecretStore,
) -> Result<Vec<BundleEntry>, SecretError> {
    profiles
        .iter()
        .map(|p| {
            let password = if p.deleted {
                None
            } else {
                secrets
                    .get(&password_key(&p.id))?
                    .map(|pw| pw.as_str().to_owned())
            };
            Ok(BundleEntry {
                profile: p.clone(),
                password,
            })
        })
        .collect()
}

/// Every entry must be a valid profile (for a tombstone: a valid id).
fn validate_entries(entries: &[BundleEntry]) -> Result<(), SyncError> {
    for e in entries {
        e.profile.validate().map_err(|err| {
            SyncError::Store(format!(
                "received an invalid profile, the sync was rejected: {err}"
            ))
        })?;
    }
    Ok(())
}

fn store_error(e: StoreError) -> SyncError {
    // StoreError messages carry no file contents.
    SyncError::Store(e.to_string())
}

fn secret_error(e: SecretError) -> SyncError {
    match e {
        SecretError::Unavailable(_) => SyncError::NoSecretStore,
        // SecretError messages never carry a secret value.
        SecretError::Backend(_) => SyncError::Store(e.to_string()),
    }
}

// ----- framing -----

/// Write one frame: `u32` BE length (kind byte plus payload), kind, payload.
/// Fails without writing if the frame would exceed [`MAX_FRAME`].
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload
        .len()
        .checked_add(1)
        .filter(|&n| n <= MAX_FRAME)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync message exceeds the 16 MiB limit",
            )
        })?;
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&len.to_be_bytes());
    header[4] = kind;
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame, returning its kind and payload. A length of zero is a
/// protocol error; one above [`MAX_FRAME`] is [`SyncError::FrameTooLarge`],
/// decided from the header alone. End of input anywhere is an I/O error.
pub fn read_frame(r: &mut impl Read) -> Result<(u8, Vec<u8>), SyncError> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let len = usize::try_from(u32::from_be_bytes(header)).map_err(|_| SyncError::FrameTooLarge)?;
    if len == 0 {
        return Err(SyncError::Protocol("empty frame".to_string()));
    }
    if len > MAX_FRAME {
        return Err(SyncError::FrameTooLarge);
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let want = len - 1;
    // Grow with the data actually received rather than trusting the length.
    let mut payload = Vec::with_capacity(want.min(PIPE_CHUNK));
    r.by_ref().take(want as u64).read_to_end(&mut payload)?;
    if payload.len() != want {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sync message cut short").into());
    }
    Ok((kind[0], payload))
}

/// Read a frame that must be of kind `want`. An ERROR frame becomes the error
/// it describes.
fn read_expected(r: &mut impl Read, want: u8) -> Result<Vec<u8>, SyncError> {
    match read_frame(r)? {
        (kind, payload) if kind == want => Ok(payload),
        (KIND_ERROR, payload) => Err(error_from_payload(&payload)),
        (kind, _) => Err(SyncError::Protocol(format!(
            "expected message type {want}, got {kind}"
        ))),
    }
}

/// The error an ERROR frame's payload describes.
fn error_from_payload(payload: &[u8]) -> SyncError {
    let Some((&code, message)) = payload.split_first() else {
        return SyncError::Protocol("empty error message".to_string());
    };
    match code {
        CODE_WRONG_SYNC_KEY => SyncError::WrongSyncKey,
        CODE_VERSION_MISMATCH => SyncError::VersionMismatch,
        CODE_NO_SECRET_STORE => SyncError::NoSecretStore,
        _ => {
            let text = clean(&String::from_utf8_lossy(message));
            SyncError::Remote(first_bytes(&text, MAX_SHOWN_MESSAGE).to_string())
        }
    }
}

/// Tell the peer about `err` with an ERROR frame. Best effort: the peer may
/// already be gone, and the local error is what gets reported.
fn send_error(w: &mut impl Write, err: &SyncError) {
    let code = match err {
        SyncError::WrongSyncKey => CODE_WRONG_SYNC_KEY,
        SyncError::VersionMismatch => CODE_VERSION_MISMATCH,
        SyncError::NoSecretStore => CODE_NO_SECRET_STORE,
        _ => CODE_OTHER,
    };
    let text = err.to_string();
    let mut payload = vec![code];
    payload.extend_from_slice(first_bytes(&text, MAX_SENT_MESSAGE).as_bytes());
    let _ = write_frame(w, KIND_ERROR, &payload);
}

/// Pass `result` through, telling the peer if it is an error.
fn tell<T>(w: &mut impl Write, result: Result<T, SyncError>) -> Result<T, SyncError> {
    if let Err(e) = &result {
        send_error(w, e);
    }
    result
}

// ----- protocol -----

fn check_passphrase(passphrase: &str) -> Result<(), SyncError> {
    if passphrase.is_empty() {
        Err(SyncError::EmptyPassphrase)
    } else {
        Ok(())
    }
}

fn check_version(hello: &[u8]) -> Result<(), SyncError> {
    match hello.first() {
        Some(&PROTOCOL_VERSION) => Ok(()),
        Some(_) => Err(SyncError::VersionMismatch),
        None => Err(SyncError::Protocol("empty HELLO".to_string())),
    }
}

fn seal_bundle(passphrase: &str, entries: &[BundleEntry]) -> Result<Vec<u8>, SyncError> {
    let sealed = crypto::seal(passphrase, entries).map_err(|e| match e {
        CryptoError::EmptyPassphrase => SyncError::EmptyPassphrase,
        // Serialize, TooLarge, Random: about local data; messages are fixed text.
        other => SyncError::Store(other.to_string()),
    })?;
    if sealed.len() >= MAX_FRAME {
        return Err(SyncError::FrameTooLarge);
    }
    Ok(sealed)
}

fn open_bundle(passphrase: &str, sealed: &[u8]) -> Result<Vec<BundleEntry>, SyncError> {
    crypto::open(passphrase, sealed).map_err(|e| match e {
        CryptoError::WrongKeyOrCorrupt => SyncError::WrongSyncKey,
        CryptoError::EmptyPassphrase => SyncError::EmptyPassphrase,
        CryptoError::BadFormat => {
            SyncError::Remote("the other machine sent data this version cannot read".to_string())
        }
        other => SyncError::Remote(other.to_string()),
    })
}

/// The server's side of one sync (what `moniker-server sync-serve` runs on its
/// stdin/stdout). See the module docs for the exchange.
///
/// The server applies the merged set before it sends it back, so its result
/// is `Ok` only if both the apply and the reply succeeded.
pub fn serve(
    mut r: impl Read,
    mut w: impl Write,
    local: &mut dyn SyncTarget,
    passphrase: &str,
) -> Result<SyncOutcome, SyncError> {
    let hello = read_expected(&mut r, KIND_HELLO)?;
    tell(
        &mut w,
        check_passphrase(passphrase)
            .and_then(|()| local.ready())
            .and_then(|()| check_version(&hello)),
    )?;
    write_frame(&mut w, KIND_HELLO, &[PROTOCOL_VERSION])?;

    let sealed = read_expected(&mut r, KIND_BUNDLE)?;
    let (merged, reply) = tell(&mut w, server_merge(&*local, passphrase, &sealed))?;
    let outcome = tell(&mut w, local.apply(merged))?;
    write_frame(&mut w, KIND_BUNDLE, &reply)?;
    Ok(outcome)
}

/// Open and check the client's bundle, merge it with the local export, and
/// seal the result. Nothing is changed yet.
fn server_merge(
    local: &dyn SyncTarget,
    passphrase: &str,
    sealed: &[u8],
) -> Result<(Vec<BundleEntry>, Vec<u8>), SyncError> {
    let incoming = open_bundle(passphrase, sealed)?;
    validate_entries(&incoming)?;
    let merged = merge(local.export()?, incoming);
    let reply = seal_bundle(passphrase, &merged)?;
    Ok((merged, reply))
}

/// The client's side of one sync. Applies the server's merged set only after
/// reading, opening and validating all of it.
pub fn run_client(
    mut r: impl Read,
    mut w: impl Write,
    local: &mut dyn SyncTarget,
    passphrase: &str,
) -> Result<SyncOutcome, SyncError> {
    check_passphrase(passphrase)?;
    local.ready()?;
    write_frame(&mut w, KIND_HELLO, &[PROTOCOL_VERSION])?;
    let hello = read_expected(&mut r, KIND_HELLO)?;
    let sealed = tell(
        &mut w,
        check_version(&hello)
            .and_then(|()| local.export())
            .and_then(|entries| seal_bundle(passphrase, &entries)),
    )?;
    write_frame(&mut w, KIND_BUNDLE, &sealed)?;

    let reply = read_expected(&mut r, KIND_BUNDLE)?;
    let merged = open_bundle(passphrase, &reply)?;
    validate_entries(&merged)?;
    local.apply(merged)
}

// ----- ssh transport -----

/// Run `cmd` (normally [`crate::ssh::sync_command`]) and sync with the
/// `moniker-server sync-serve` at the other end of its stdin/stdout.
///
/// The command is spawned directly from its argv, never through a shell, with
/// `cmd.env` added to the environment. Its stderr is drained (the last few KiB
/// kept) so it can never block. The whole run is limited to [`SYNC_TIMEOUT`];
/// on expiry the child is killed and reaped, and nothing has been changed.
///
/// When the exchange fails at the transport level, the exit status explains
/// why: 127 is [`SyncError::ServerNotFound`], 255 with "Host key verification
/// failed" is [`SyncError::HostKeyUnknown`], any other non-zero status is
/// [`SyncError::Remote`] with the last stderr lines (sanitized). An error the
/// server reported itself, or any error with exit status 0, is kept as it is.
/// A completed exchange is `Ok` whatever the exit status.
pub fn sync_over_ssh(
    local: &mut dyn SyncTarget,
    cmd: &SshCommand,
    passphrase: &str,
) -> Result<SyncOutcome, SyncError> {
    sync_over_ssh_with_timeout(local, cmd, passphrase, SYNC_TIMEOUT)
}

fn sync_over_ssh_with_timeout(
    local: &mut dyn SyncTarget,
    cmd: &SshCommand,
    passphrase: &str,
    timeout: Duration,
) -> Result<SyncOutcome, SyncError> {
    check_passphrase(passphrase)?;
    // No point connecting if received passwords could not be stored.
    local.ready()?;
    let deadline = Instant::now() + timeout;
    let child = Command::new(&cmd.program)
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            SyncError::Io(io::Error::new(
                e.kind(),
                format!("could not start {}: {e}", cmd.program),
            ))
        })?;
    // From here on the child is killed and reaped on every path, panics included.
    let mut child = Reaped(child);
    let (stdin, stdout, stderr) = match (
        child.0.stdin.take(),
        child.0.stdout.take(),
        child.0.stderr.take(),
    ) {
        (Some(i), Some(o), Some(e)) => (i, o, e),
        _ => return Err(io::Error::other("child pipes missing").into()),
    };

    let (reader, stdout_thread) = pump_stdout(stdout, deadline)?;
    let (writer, stdin_thread) = pump_stdin(stdin)?;
    let (tail, stderr_thread) = drain_stderr(stderr)?;

    // `reader` and `writer` are dropped when this returns, which closes the
    // child's stdin once everything queued has been written.
    let result = run_client(reader, writer, local, passphrase);

    let timed_out = matches!(&result, Err(SyncError::Io(e)) if e.kind() == io::ErrorKind::TimedOut);
    let wait_until = deadline.min(Instant::now() + EXIT_GRACE);
    // A failure to wait must not turn a completed (applied) sync into an
    // error; the status is then unknown, and `Reaped` still reaps on drop.
    let status = child.wait_until(wait_until).ok().flatten();
    join_briefly(vec![stdout_thread, stdin_thread, stderr_thread]);

    match result {
        Ok(outcome) => Ok(outcome),
        Err(_) if timed_out => Err(SyncError::Timeout),
        Err(e) if is_transport_failure(&e) => {
            let tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
            Err(explain_exit(e, status, &String::from_utf8_lossy(&tail)))
        }
        Err(e) => Err(e),
    }
}

/// Failures that say only that the stream broke, so the exit status and
/// stderr of the transport explain them better.
fn is_transport_failure(e: &SyncError) -> bool {
    matches!(
        e,
        SyncError::Io(_) | SyncError::Protocol(_) | SyncError::FrameTooLarge
    )
}

/// Map a transport failure to what the child's exit says about it. `status`
/// is `None` if the child had to be killed.
fn explain_exit(err: SyncError, status: Option<ExitStatus>, stderr: &str) -> SyncError {
    let Some(status) = status else {
        return err;
    };
    match status.code() {
        Some(0) => err,
        Some(127) => SyncError::ServerNotFound,
        Some(255) if stderr.contains("Host key verification failed") => SyncError::HostKeyUnknown,
        code => {
            let what = match code {
                Some(c) => format!("ssh exited with status {c}"),
                None => "ssh was terminated by a signal".to_string(),
            };
            let text = stderr_summary(stderr);
            if text.is_empty() {
                SyncError::Remote(what)
            } else {
                SyncError::Remote(format!("{what}: {text}"))
            }
        }
    }
}

/// The last few non-blank stderr lines joined with " | ", control characters
/// removed, cut to [`MAX_SHOWN_MESSAGE`] bytes from the end.
fn stderr_summary(stderr: &str) -> String {
    let lines: Vec<String> = stderr
        .lines()
        .map(|l| clean(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(STDERR_LINES);
    let joined = lines[start..].join(" | ");
    last_bytes(&joined, MAX_SHOWN_MESSAGE).to_string()
}

/// `s` without control characters, bidirectional overrides, zero-width
/// characters or line/paragraph separators (so text from elsewhere cannot
/// drive a terminal, reorder what is shown, hide text or break lines).
fn clean(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() && !is_invisible_or_separator(c))
        .collect()
}

/// Characters `char::is_control` misses that could still disguise text.
fn is_invisible_or_separator(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200D}' // zero-width space, non-joiner, joiner
            | '\u{2028}' | '\u{2029}' // line and paragraph separators
            | '\u{202A}'..='\u{202E}' // bidi embeddings and overrides
            | '\u{2060}' // word joiner
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}' // zero-width no-break space (BOM)
    )
}

/// The first `max` bytes of `s` or less, on a character boundary.
fn first_bytes(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The last `max` bytes of `s` or less, on a character boundary.
fn last_bytes(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// A child that is killed (if still running) and reaped when dropped.
struct Reaped(Child);

impl Reaped {
    /// Wait for the child to exit until `until`, then kill it. Reaps it either
    /// way. `None` means it had to be killed.
    fn wait_until(&mut self, until: Instant) -> io::Result<Option<ExitStatus>> {
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= until {
                // Killing an already reaped child is a no-op in std, so this
                // can never signal a recycled pid.
                let _ = self.0.kill();
                self.0.wait()?;
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reads the child's stdout on a thread so that reading can respect the
/// deadline. The queue is bounded, so a peer that floods stdout cannot make
/// it grow without limit.
struct DeadlineReader {
    rx: Receiver<io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    pos: usize,
    eof: bool,
    deadline: Instant,
}

impl Read for DeadlineReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.chunk.len() {
            if self.eof {
                return Ok(0);
            }
            let left = self.deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Ok(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "sync timed out"));
                }
                Err(RecvTimeoutError::Disconnected) => self.eof = true,
            }
        }
        let n = out.len().min(self.chunk.len() - self.pos);
        out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn pump_stdout(
    mut stdout: ChildStdout,
    deadline: Instant,
) -> io::Result<(DeadlineReader, JoinHandle<()>)> {
    let (tx, rx): (SyncSender<io::Result<Vec<u8>>>, _) = mpsc::sync_channel(PIPE_QUEUE);
    let handle = thread::Builder::new()
        .name("sync-stdout".to_string())
        .spawn(move || {
            loop {
                let mut buf = vec![0u8; PIPE_CHUNK];
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        // The reader is gone: nobody wants the rest.
                        if tx.send(Ok(buf)).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        })?;
    let reader = DeadlineReader {
        rx,
        chunk: Vec::new(),
        pos: 0,
        eof: false,
        deadline,
    };
    Ok((reader, handle))
}

/// Queues writes for a thread that feeds the child's stdin, so a child that
/// stops reading cannot block the exchange past its deadline. Only our own
/// frames go through it, so the queue is bounded by what we send.
struct QueuedWriter(Sender<Vec<u8>>);

impl Write for QueuedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ssh stopped reading"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn pump_stdin(mut stdin: ChildStdin) -> io::Result<(QueuedWriter, JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let handle = thread::Builder::new()
        .name("sync-stdin".to_string())
        .spawn(move || {
            for chunk in rx {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
            // Dropping stdin here closes the child's input.
        })?;
    Ok((QueuedWriter(tx), handle))
}

type Tail = Arc<Mutex<Vec<u8>>>;

/// Read the child's stderr to the end, keeping the last [`STDERR_TAIL`] bytes.
fn drain_stderr(mut stderr: ChildStderr) -> io::Result<(Tail, JoinHandle<()>)> {
    let tail: Tail = Arc::default();
    let keep = Arc::clone(&tail);
    let handle = thread::Builder::new()
        .name("sync-stderr".to_string())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut t = keep.lock().unwrap_or_else(PoisonError::into_inner);
                        t.extend_from_slice(&buf[..n]);
                        let excess = t.len().saturating_sub(STDERR_TAIL);
                        t.drain(..excess);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })?;
    Ok((tail, handle))
}

/// Join the pipe threads, waiting at most [`THREAD_GRACE`]. Once the child is
/// reaped they finish at once; one that does not is blocked on a pipe that a
/// process the child left behind still holds open, and it ends when that
/// process closes it. It is not waited for.
fn join_briefly(handles: Vec<JoinHandle<()>>) {
    let until = Instant::now() + THREAD_GRACE;
    let mut pending = handles;
    loop {
        let (done, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|h| h.is_finished());
        for h in done {
            let _ = h.join();
        }
        pending = rest;
        if pending.is_empty() || Instant::now() >= until {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use crate::secrets::{MemorySecretStore, SecretError, password_key};
    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;
    use tempfile::TempDir;

    const PASS: &str = "correct horse battery staple";

    // ----- fixtures -----

    /// A memory secret store that counts writes (sets and deletes) and can be
    /// told to fail one of them, shared with the test.
    #[derive(Default)]
    struct Counted {
        inner: MemorySecretStore,
        writes: AtomicUsize,
        deletes: AtomicUsize,
        /// The write number (1-based, counting all writes so far) that fails;
        /// 0 means none. Only that one write fails.
        fail_at: AtomicUsize,
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Counted>);

    impl Shared {
        fn password(&self, id: &str) -> Option<String> {
            self.0
                .inner
                .get(&password_key(id))
                .unwrap()
                .map(|v| v.as_str().to_owned())
        }
        fn writes(&self) -> usize {
            self.0.writes.load(Ordering::SeqCst)
        }
        fn deletes(&self) -> usize {
            self.0.deletes.load(Ordering::SeqCst)
        }
        /// Make the `n`-th write from now fail (once).
        fn fail_nth_write(&self, n: usize) {
            self.0.fail_at.store(self.writes() + n, Ordering::SeqCst);
        }
        /// Count a write; `Err` if it is the one chosen to fail.
        fn write(&self) -> Result<(), SecretError> {
            let n = self.0.writes.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.0.fail_at.load(Ordering::SeqCst) {
                Err(SecretError::Backend("injected failure".to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl SecretStore for Shared {
        fn get(&self, key: &str) -> Result<Option<zeroize::Zeroizing<String>>, SecretError> {
            self.0.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            self.write()?;
            self.0.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<(), SecretError> {
            self.0.deletes.fetch_add(1, Ordering::SeqCst);
            self.write()?;
            self.0.inner.delete(key)
        }
        fn probe(&self) -> Result<(), SecretError> {
            Ok(())
        }
    }

    /// One machine: a tempdir profile store plus a counted secret store.
    struct Side {
        _dir: TempDir,
        path: PathBuf,
        secrets: Shared,
        state: LocalState,
    }

    /// The profile file's bytes (empty if there is none).
    fn file_of(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap_or_default()
    }

    /// A side holding `profiles` (with `updated_at` kept) and their passwords.
    /// The write counter starts at zero.
    fn side(profiles: &[(Profile, Option<&str>)]) -> Side {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let mut store = ProfileStore::open(&path).unwrap();
        store
            .replace_all(profiles.iter().map(|(p, _)| p.clone()).collect())
            .unwrap();
        let secrets = Shared::default();
        for (p, pw) in profiles {
            if let Some(pw) = pw {
                secrets.0.inner.set(&password_key(&p.id), pw).unwrap();
            }
        }
        let state = LocalState::new(store, Box::new(secrets.clone()));
        Side {
            _dir: dir,
            path,
            secrets,
            state,
        }
    }

    fn prof(name: &str) -> Profile {
        Profile::new(name, "example.com", "alice")
    }

    fn prof_at(name: &str, updated_at: i64) -> Profile {
        let mut p = prof(name);
        p.updated_at = updated_at;
        p
    }

    fn entry(p: &Profile, pw: Option<&str>) -> BundleEntry {
        BundleEntry {
            profile: p.clone(),
            password: pw.map(str::to_owned),
        }
    }

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    type Served<T> = thread::JoinHandle<(Result<SyncOutcome, SyncError>, T)>;

    /// Run `serve` for `target` on `stream` in a thread; hands the target back.
    fn spawn_server<T: SyncTarget + Send + 'static>(
        stream: UnixStream,
        mut target: T,
        passphrase: &'static str,
    ) -> Served<T> {
        thread::spawn(move || {
            let r = serve(&stream, &stream, &mut target, passphrase);
            (r, target)
        })
    }

    /// A scripted `SyncTarget`.
    #[derive(Default)]
    struct Stub {
        no_secret_store: bool,
        entries: Vec<BundleEntry>,
        exports: Cell<usize>,
        applied: Option<Vec<BundleEntry>>,
    }

    impl SyncTarget for Stub {
        fn ready(&self) -> Result<(), SyncError> {
            if self.no_secret_store {
                Err(SyncError::NoSecretStore)
            } else {
                Ok(())
            }
        }
        fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
            self.exports.set(self.exports.get() + 1);
            Ok(self.entries.clone())
        }
        fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
            self.applied = Some(merged);
            Ok(SyncOutcome::default())
        }
    }

    /// Every frame in `bytes` (which must be whole frames).
    fn frames(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut cur = Cursor::new(bytes);
        let mut out = Vec::new();
        while (cur.position() as usize) < bytes.len() {
            out.push(read_frame(&mut cur).unwrap());
        }
        out
    }

    fn assert_no_secret_in(err: &SyncError, secrets: &[&str]) {
        let shown = format!("{err} {err:?}");
        for s in secrets {
            assert!(!shown.contains(s), "{shown:?} leaks {s:?}");
        }
    }

    fn sh(script: &str) -> SshCommand {
        SshCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: vec![],
        }
    }

    // ----- the brief's named tests -----

    #[test]
    fn two_instances_converge() {
        let p1 = prof("one");
        let p2 = prof("two");
        let mut a = side(&[(p1.clone(), Some("pw-one"))]);
        let b = side(&[(p2.clone(), Some("pw-two"))]);
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, PASS);

        let out = run_client(&cs, &cs, &mut a.state, PASS).unwrap();
        drop(cs);
        let (served, b_state) = server.join().unwrap();
        let served = served.unwrap();

        assert_eq!(
            out,
            SyncOutcome {
                added: 1,
                updated: 0,
                removed: 0
            }
        );
        assert_eq!(served.added, 1);

        let mut want = vec![p1.clone(), p2.clone()];
        want.sort_by(|x, y| x.id.cmp(&y.id));
        for (path, secrets, snapshot) in [
            (&a.path, &a.secrets, a.state.store().all()),
            (&b.path, &b.secrets, b_state.store().all()),
        ] {
            assert_eq!(ProfileStore::open(path).unwrap().all(), want);
            assert_eq!(snapshot, want, "in-memory snapshot is refreshed");
            assert_eq!(secrets.password(&p1.id).as_deref(), Some("pw-one"));
            assert_eq!(secrets.password(&p2.id).as_deref(), Some("pw-two"));
        }
    }

    #[test]
    fn wrong_passphrase_changes_nothing() {
        let p1 = prof("one");
        let p2 = prof("two");
        let mut a = side(&[(p1.clone(), Some("pw-one"))]);
        let b = side(&[(p2.clone(), Some("pw-two"))]);
        let (a_file, b_file) = (file_of(&a.path), file_of(&b.path));
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, "the server's passphrase");

        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        drop(cs);
        let (served, _b_state) = server.join().unwrap();
        let served = served.unwrap_err();

        assert!(matches!(err, SyncError::WrongSyncKey), "client got {err:?}");
        assert!(
            matches!(served, SyncError::WrongSyncKey),
            "server got {served:?}"
        );
        for e in [&err, &served] {
            assert_no_secret_in(e, &[PASS, "the server's passphrase", "pw-one", "pw-two"]);
        }
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(a.secrets.writes(), 0);
        assert_eq!(b.secrets.writes(), 0);
        assert_eq!(a.secrets.password(&p2.id), None);
        assert_eq!(b.secrets.password(&p1.id), None);
        assert_eq!(a.state.store().all(), vec![p1]);
    }

    #[test]
    fn server_without_secret_store() {
        let p1 = prof("one");
        let mut a = side(&[(p1.clone(), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let (cs, ss) = pair();
        let stub = Stub {
            no_secret_store: true,
            ..Stub::default()
        };
        let server = thread::spawn(move || {
            let mut stub = stub;
            let r = serve(&ss, &ss, &mut stub, PASS);
            // Whatever the client sent after its HELLO, up to its close.
            let mut rest = Vec::new();
            (&ss).read_to_end(&mut rest).unwrap();
            (r, stub, rest)
        });

        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        drop(cs);
        let (served, stub, rest) = server.join().unwrap();

        assert!(
            matches!(err, SyncError::NoSecretStore),
            "client got {err:?}"
        );
        assert!(matches!(served, Err(SyncError::NoSecretStore)));
        assert!(
            frames(&rest).iter().all(|(kind, _)| *kind != KIND_BUNDLE),
            "the client must not send its bundle"
        );
        assert!(stub.applied.is_none());
        assert_eq!(stub.exports.get(), 0);
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn dropped_connection_before_reply() {
        let p1 = prof("one");
        let mut a = side(&[(p1.clone(), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let (cs, ss) = pair();
        let server = thread::spawn(move || {
            let (kind, _) = read_frame(&mut &ss).unwrap();
            assert_eq!(kind, KIND_HELLO);
            write_frame(&mut &ss, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
            let (kind, _) = read_frame(&mut &ss).unwrap();
            assert_eq!(kind, KIND_BUNDLE);
            // Hang up without replying.
        });

        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        server.join().unwrap();
        assert!(matches!(err, SyncError::Io(_)), "client got {err:?}");
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
        assert_eq!(a.state.store().all(), vec![p1]);
    }

    /// Yields its bytes, then fails the test if read any further.
    struct ThenPanic(Cursor<Vec<u8>>);

    impl Read for ThenPanic {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.read(buf)?;
            assert!(n > 0 || buf.is_empty(), "read past the frame header");
            Ok(n)
        }
    }

    #[test]
    fn oversize_frame_rejected_without_allocating() {
        let mut r = ThenPanic(Cursor::new(vec![0xFF, 0xFF, 0xFF, 0xFF]));
        assert!(matches!(read_frame(&mut r), Err(SyncError::FrameTooLarge)));

        let just_over = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        let mut r = ThenPanic(Cursor::new(just_over.to_vec()));
        assert!(matches!(read_frame(&mut r), Err(SyncError::FrameTooLarge)));

        // Exactly MAX_FRAME (kind byte included) is allowed.
        let mut bytes = u32::try_from(MAX_FRAME).unwrap().to_be_bytes().to_vec();
        bytes.push(KIND_BUNDLE);
        bytes.resize(4 + MAX_FRAME, 0xAB);
        let (kind, payload) = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(kind, KIND_BUNDLE);
        assert_eq!(payload.len(), MAX_FRAME - 1);

        // And write_frame refuses to produce an oversize frame.
        let mut out = Vec::new();
        assert!(write_frame(&mut out, KIND_BUNDLE, &vec![0; MAX_FRAME]).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn version_mismatch() {
        // A client speaking version 9 to a real server.
        let b = side(&[(prof("two"), Some("pw-two"))]);
        let b_file = file_of(&b.path);
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, PASS);
        write_frame(&mut &cs, KIND_HELLO, &[9]).unwrap();
        let (kind, payload) = read_frame(&mut &cs).unwrap();
        drop(cs);
        let (served, _) = server.join().unwrap();
        assert!(matches!(served, Err(SyncError::VersionMismatch)));
        assert_eq!(kind, KIND_ERROR);
        assert_eq!(payload[0], 2);
        assert!(matches!(
            error_from_payload(&payload),
            SyncError::VersionMismatch
        ));
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(b.secrets.writes(), 0);

        // A real client facing a server that rejects its version.
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let (cs, ss) = pair();
        let server = thread::spawn(move || {
            read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_ERROR, b"\x02unsupported").unwrap();
            let mut rest = Vec::new();
            (&ss).read_to_end(&mut rest).unwrap();
            rest
        });
        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        drop(cs);
        assert!(matches!(err, SyncError::VersionMismatch), "got {err:?}");
        assert!(server.join().unwrap().is_empty(), "nothing after HELLO");

        // A real client facing a server that answers with version 9: the
        // client refuses and tells the server why.
        let (cs, ss) = pair();
        let server = thread::spawn(move || {
            read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_HELLO, &[9]).unwrap();
            let mut rest = Vec::new();
            (&ss).read_to_end(&mut rest).unwrap();
            rest
        });
        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        drop(cs);
        assert!(matches!(err, SyncError::VersionMismatch), "got {err:?}");
        let rest = frames(&server.join().unwrap());
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].0, KIND_ERROR);
        assert_eq!(rest[0].1[0], 2);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn exit_status_mapping() {
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);

        let err = sync_over_ssh(&mut a.state, &sh("exit 127"), PASS).unwrap_err();
        assert!(matches!(err, SyncError::ServerNotFound), "got {err:?}");

        let err = sync_over_ssh(
            &mut a.state,
            &sh(r#"echo "Host key verification failed." >&2; exit 255"#),
            PASS,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::HostKeyUnknown), "got {err:?}");

        // Another failure: the last stderr lines, control characters removed.
        let err = sync_over_ssh(
            &mut a.state,
            &sh(r#"printf 'early noise\nssh: connect to host x: \033[31mrefused\a\n' >&2; exit 255"#),
            PASS,
        )
        .unwrap_err();
        let SyncError::Remote(msg) = &err else {
            panic!("expected Remote, got {err:?}");
        };
        assert!(msg.contains("connect to host x"), "{msg:?}");
        assert!(msg.contains("refused"), "{msg:?}");
        assert!(!msg.chars().any(char::is_control), "{msg:?}");

        // Long stderr is cut down.
        let err = sync_over_ssh(
            &mut a.state,
            &sh("i=0; while [ $i -lt 400 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxx' >&2; i=$((i+1)); done; exit 3"),
            PASS,
        )
        .unwrap_err();
        let SyncError::Remote(msg) = &err else {
            panic!("expected Remote, got {err:?}");
        };
        assert!(msg.len() <= 400, "{} bytes", msg.len());

        // Exit 0 without speaking the protocol keeps the protocol error.
        let err = sync_over_ssh(&mut a.state, &sh("exit 0"), PASS).unwrap_err();
        assert!(matches!(err, SyncError::Io(_)), "got {err:?}");

        // A program that does not exist.
        let missing = SshCommand {
            program: "/nonexistent/moniker-test-ssh".to_string(),
            args: vec![],
            env: vec![],
        };
        let err = sync_over_ssh(&mut a.state, &missing, PASS).unwrap_err();
        assert!(matches!(err, SyncError::Io(_)), "got {err:?}");

        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    // ----- rulings -----

    #[test]
    fn sync_over_ssh_runs_argv_and_env_without_a_shell() {
        // The child gets exactly our argv (a single argument with spaces and
        // quotes stays one argument) and the extra environment.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let cmd = SshCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"printf '%s|%s|%s' "$#" "$1" "$MONIKER_TEST_VAR" > "$2"; exit 127"#.to_string(),
                "sh".to_string(),
                "a 'b' \"c\"; $(x)".to_string(),
                out.to_str().unwrap().to_string(),
            ],
            env: vec![("MONIKER_TEST_VAR".to_string(), "v1".to_string())],
        };
        let mut a = side(&[]);
        let err = sync_over_ssh(&mut a.state, &cmd, PASS).unwrap_err();
        assert!(matches!(err, SyncError::ServerNotFound), "got {err:?}");
        assert_eq!(fs::read_to_string(&out).unwrap(), "2|a 'b' \"c\"; $(x)|v1");
    }

    #[test]
    fn sync_over_ssh_checks_ready_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let cmd = SshCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"touch "$1""#.to_string(),
                "sh".to_string(),
                marker.to_str().unwrap().to_string(),
            ],
            env: vec![],
        };
        let mut stub = Stub {
            no_secret_store: true,
            ..Stub::default()
        };
        let err = sync_over_ssh(&mut stub, &cmd, PASS).unwrap_err();
        assert!(matches!(err, SyncError::NoSecretStore), "{err:?}");
        assert!(!marker.exists(), "ssh must not be started");
        assert!(stub.applied.is_none());
    }

    #[test]
    fn sync_over_ssh_full_exchange_with_a_child_process() {
        // The child relays the protocol to a real server over a socket pair:
        // `cat` bridges stdin/stdout through two fifos served by a thread.
        let dir = tempfile::tempdir().unwrap();
        let to_server = dir.path().join("to_server");
        let to_client = dir.path().join("to_client");
        for f in [&to_server, &to_client] {
            let ok = std::process::Command::new("mkfifo")
                .arg(f)
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        let p1 = prof("one");
        let p2 = prof("two");
        let mut a = side(&[(p1.clone(), Some("pw-one"))]);
        let b = side(&[(p2.clone(), Some("pw-two"))]);
        let (ts, tc) = (to_server.clone(), to_client.clone());
        let server = thread::spawn(move || {
            let mut b_state = b.state;
            let r = fs::File::open(&ts).unwrap();
            let w = fs::OpenOptions::new().write(true).open(&tc).unwrap();
            let res = serve(r, w, &mut b_state, PASS);
            (res, b.secrets, b._dir)
        });
        let cmd = SshCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                // An async list's stdin is /dev/null unless redirected.
                r#"exec 3<&0; cat <&3 > "$1" & cat < "$2"; wait"#.to_string(),
                "sh".to_string(),
                to_server.to_str().unwrap().to_string(),
                to_client.to_str().unwrap().to_string(),
            ],
            env: vec![],
        };
        let out = sync_over_ssh(&mut a.state, &cmd, PASS).unwrap();
        assert_eq!(out.added, 1);
        let (served, b_secrets, _b_dir) = server.join().unwrap();
        assert_eq!(served.unwrap().added, 1);
        assert_eq!(a.secrets.password(&p2.id).as_deref(), Some("pw-two"));
        assert_eq!(b_secrets.password(&p1.id).as_deref(), Some("pw-one"));
    }

    #[test]
    fn invalid_peer_profile_rejects_whole_sync() {
        let mut bad = prof("bad");
        bad.host = "-oProxyCommand=x".to_string();
        let good = prof("good");

        // A hostile client: the server rejects everything it sent.
        let b = side(&[(prof("two"), Some("pw-two"))]);
        let b_file = file_of(&b.path);
        let stub = Stub {
            entries: vec![entry(&good, Some("g")), entry(&bad, Some("b"))],
            ..Stub::default()
        };
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, PASS);
        let mut stub = stub;
        let err = run_client(&cs, &cs, &mut stub, PASS).unwrap_err();
        drop(cs);
        let (served, _) = server.join().unwrap();
        assert!(matches!(served, Err(SyncError::Store(_))), "{served:?}");
        assert!(matches!(err, SyncError::Remote(_)), "client got {err:?}");
        assert!(stub.applied.is_none());
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(b.secrets.writes(), 0);

        // A hostile server: the client rejects its reply.
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let (good2, bad2) = (good.clone(), bad.clone());
        let (cs, ss) = pair();
        let server = thread::spawn(move || {
            read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
            read_frame(&mut &ss).unwrap();
            let reply =
                crate::crypto::seal(PASS, &[entry(&good2, Some("g")), entry(&bad2, Some("b"))])
                    .unwrap();
            write_frame(&mut &ss, KIND_BUNDLE, &reply).unwrap();
        });
        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        server.join().unwrap();
        assert!(matches!(err, SyncError::Store(_)), "client got {err:?}");
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);

        // apply itself refuses, even when the bad entry would lose the merge
        // or is a tombstone with a bad id.
        let mut older_bad = bad.clone();
        older_bad.id = good.id.clone();
        older_bad.updated_at = good.updated_at - 1;
        let mut bad_tomb = prof("x").tombstone(5);
        bad_tomb.id = "../x".to_string();
        for incoming in [
            vec![entry(&good, None), entry(&older_bad, None)],
            vec![entry(&good, None), entry(&bad_tomb, None)],
        ] {
            let err = a.state.apply(incoming).unwrap_err();
            assert!(matches!(err, SyncError::Store(_)), "{err:?}");
            assert_eq!(file_of(&a.path), a_file);
            assert_eq!(a.secrets.writes(), 0);
        }
    }

    #[test]
    fn a_peer_profile_with_control_characters_rejects_the_whole_sync() {
        // A NUL in a name aborts debug builds of the GUI at startup; an escape
        // sequence acts on terminals. Both are refused like any invalid profile.
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let good = prof("good");

        let mut nul_name = prof("x");
        nul_name.name = "a\0b".to_string();
        let mut esc_name = prof("x");
        esc_name.name = "a\x1b[2Jb".to_string();
        let mut seq_name = prof("x");
        seq_name.sequences.push(crate::profile::StartupSequence {
            name: "up\0date".to_string(),
            commands: vec!["ls".to_string()],
        });
        for bad in [nul_name, esc_name, seq_name] {
            let err = a
                .state
                .apply(vec![entry(&good, Some("g")), entry(&bad, Some("b"))])
                .unwrap_err();
            let SyncError::Store(message) = &err else {
                panic!("expected a store error, got {err:?}");
            };
            assert!(message.contains("control characters"), "{message}");
            assert!(
                !message.contains('\0') && !message.contains('\x1b'),
                "{message:?}"
            );
            assert_eq!(file_of(&a.path), a_file, "nothing applied");
            assert_eq!(a.secrets.writes(), 0);
        }
    }

    #[test]
    fn apply_does_not_overwrite_concurrent_edit() {
        let p1 = prof_at("original", 1_000);
        let mut a = side(&[(p1.clone(), Some("pw-original"))]);
        let exported = a.state.export().unwrap();
        assert_eq!(exported.len(), 1);

        // Meanwhile the GUI (another handle on the same file) edits p1.
        let mut gui = ProfileStore::open(&a.path).unwrap();
        let mut edited = p1.clone();
        edited.name = "edited in the GUI".to_string();
        gui.upsert(edited).unwrap();

        // The peer's version is newer than what was exported, older than the edit.
        let mut incoming = p1.clone();
        incoming.name = "from the peer".to_string();
        incoming.updated_at = 2_000;
        let p2 = prof("new from peer");
        let out = a
            .state
            .apply(vec![
                entry(&incoming, Some("pw-peer")),
                entry(&p2, Some("pw-p2")),
            ])
            .unwrap();

        let disk = ProfileStore::open(&a.path).unwrap();
        assert_eq!(disk.get(&p1.id).unwrap().name, "edited in the GUI");
        assert_eq!(disk.get(&p2.id).unwrap().name, "new from peer");
        assert_eq!(
            a.secrets.password(&p1.id).as_deref(),
            Some("pw-original"),
            "the losing incoming password is not stored"
        );
        assert_eq!(a.secrets.password(&p2.id).as_deref(), Some("pw-p2"));
        assert_eq!(
            out,
            SyncOutcome {
                added: 1,
                updated: 0,
                removed: 0
            }
        );
    }

    #[test]
    fn tombstone_removes_secret() {
        let p1 = prof_at("one", 1_000);
        let p2 = prof_at("two", 1_000);
        let mut a = side(&[(p1.clone(), Some("pw-one")), (p2.clone(), Some("pw-two"))]);
        let out = a
            .state
            .apply(vec![entry(&p1.tombstone(2_000), None)])
            .unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                added: 0,
                updated: 0,
                removed: 1
            }
        );
        assert_eq!(a.secrets.password(&p1.id), None);
        assert_eq!(a.secrets.password(&p2.id).as_deref(), Some("pw-two"));
        let disk = ProfileStore::open(&a.path).unwrap();
        assert!(disk.get(&p1.id).is_none());
        assert!(disk.all().iter().any(|p| p.id == p1.id && p.deleted));

        // An older tombstone does not delete a newer live profile.
        let out = a
            .state
            .apply(vec![entry(&p2.tombstone(500), None)])
            .unwrap();
        assert_eq!(out, SyncOutcome::default());
        assert_eq!(a.secrets.password(&p2.id).as_deref(), Some("pw-two"));
    }

    #[test]
    fn passwords_arrive_in_secret_store() {
        let mut a = side(&[]);
        let p1 = prof_at("one", 1_000);
        let p2 = prof_at("two", 1_000);
        let out = a
            .state
            .apply(vec![entry(&p1, Some("s3cret")), entry(&p2, None)])
            .unwrap();
        assert_eq!(out.added, 2);
        assert_eq!(a.secrets.password(&p1.id).as_deref(), Some("s3cret"));
        assert_eq!(a.secrets.password(&p2.id), None);

        // A newer version with a new password replaces it.
        let mut p1b = p1.clone();
        p1b.updated_at = 2_000;
        let out = a.state.apply(vec![entry(&p1b, Some("rotated"))]).unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                added: 0,
                updated: 1,
                removed: 0
            }
        );
        assert_eq!(a.secrets.password(&p1.id).as_deref(), Some("rotated"));

        // Same profile version, only the password differs: the merge
        // tie-break (byte-wise JSON) picks `rotated2` over `rotated` ('2' >
        // '"'), and a changed password alone counts as an update.
        let out = a.state.apply(vec![entry(&p1b, Some("rotated2"))]).unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                added: 0,
                updated: 1,
                removed: 0
            }
        );
        let now = a.secrets.password(&p1.id).unwrap();
        assert_eq!(now, "rotated2");

        // A newer version without a password leaves the stored one alone.
        let mut p1c = p1.clone();
        p1c.updated_at = 3_000;
        p1c.name = "renamed".to_string();
        let out = a.state.apply(vec![entry(&p1c, None)]).unwrap();
        assert_eq!(out.updated, 1);
        assert_eq!(a.secrets.password(&p1.id).unwrap(), now);

        // Applying the same set again changes nothing.
        let writes = a.secrets.writes();
        let file = file_of(&a.path);
        let out = a.state.apply(vec![entry(&p1c, None)]).unwrap();
        assert_eq!(out, SyncOutcome::default());
        assert_eq!(a.secrets.writes(), writes);
        assert_eq!(file_of(&a.path), file);
    }

    // ----- R19: secrets first, profile file last, rollback on failure -----

    const ID_A: &str = "00000000-0000-4000-8000-00000000000a";
    const ID_B: &str = "00000000-0000-4000-8000-00000000000b";
    const ID_C: &str = "00000000-0000-4000-8000-00000000000c";

    /// A profile with a fixed id (ids order the secret operations).
    fn fixed(name: &str, id: &str, updated_at: i64) -> Profile {
        let mut p = prof_at(name, updated_at);
        p.id = id.to_string();
        p
    }

    /// One sync between two states over a socket pair: (client, server
    /// results, server state handed back).
    fn sync_once(
        client: &mut LocalState,
        server: LocalState,
    ) -> (
        Result<SyncOutcome, SyncError>,
        Result<SyncOutcome, SyncError>,
        LocalState,
    ) {
        let (cs, ss) = pair();
        let handle = spawn_server(ss, server, PASS);
        let c = run_client(&cs, &cs, client, PASS);
        drop(cs);
        let (s, state) = handle.join().unwrap();
        (c, s, state)
    }

    struct RollbackCase {
        what: &'static str,
        incoming: Vec<BundleEntry>,
        /// After a successful apply: (id, live?, password).
        after: Vec<(&'static str, bool, Option<&'static str>)>,
    }

    /// Starting state for every case: A (pw-a-old) and C (pw-c), live at 1000.
    /// Each case has exactly two secret operations, so failing the second
    /// write fails the second operation after the first one succeeded.
    fn rollback_start() -> Side {
        side(&[
            (fixed("a", ID_A, 1_000), Some("pw-a-old")),
            (fixed("c", ID_C, 1_000), Some("pw-c")),
        ])
    }

    fn rollback_cases() -> Vec<RollbackCase> {
        let a_new = fixed("a rotated", ID_A, 2_000);
        let b = fixed("b", ID_B, 2_000);
        let tomb_a = fixed("a", ID_A, 0).tombstone(2_000);
        let tomb_c = fixed("c", ID_C, 0).tombstone(2_000);
        vec![
            RollbackCase {
                what: "set, then a failing set",
                incoming: vec![entry(&a_new, Some("pw-a-new")), entry(&b, Some("pw-b"))],
                after: vec![
                    (ID_A, true, Some("pw-a-new")),
                    (ID_B, true, Some("pw-b")),
                    (ID_C, true, Some("pw-c")),
                ],
            },
            RollbackCase {
                what: "set, then a failing delete",
                incoming: vec![entry(&a_new, Some("pw-a-new")), entry(&tomb_c, None)],
                after: vec![(ID_A, true, Some("pw-a-new")), (ID_C, false, None)],
            },
            RollbackCase {
                what: "delete, then a failing set",
                incoming: vec![entry(&tomb_a, None), entry(&b, Some("pw-b"))],
                after: vec![
                    (ID_A, false, None),
                    (ID_B, true, Some("pw-b")),
                    (ID_C, true, Some("pw-c")),
                ],
            },
        ]
    }

    fn assert_rolled_back(s: &Side, file: &[u8], what: &str) {
        assert_eq!(file_of(&s.path), file, "{what}: profile file untouched");
        assert_eq!(
            s.state.store().all(),
            ProfileStore::open(&s.path).unwrap().all(),
            "{what}: snapshot matches disk"
        );
        assert_eq!(
            s.secrets.password(ID_A).as_deref(),
            Some("pw-a-old"),
            "{what}"
        );
        assert_eq!(s.secrets.password(ID_B), None, "{what}");
        assert_eq!(s.secrets.password(ID_C).as_deref(), Some("pw-c"), "{what}");
    }

    #[test]
    fn failed_secret_set_leaves_profile_file_untouched_and_rolls_back() {
        for case in rollback_cases() {
            let mut s = rollback_start();
            let file = file_of(&s.path);
            s.secrets.fail_nth_write(2);
            let err = s.state.apply(case.incoming).unwrap_err();
            assert!(matches!(err, SyncError::Store(_)), "{}: {err:?}", case.what);
            assert_rolled_back(&s, &file, case.what);
        }
    }

    #[test]
    fn apply_recovers_after_transient_secret_failure() {
        for case in rollback_cases() {
            let mut s = rollback_start();
            s.secrets.fail_nth_write(2);
            s.state.apply(case.incoming.clone()).unwrap_err();
            // The failure was transient: the same apply now goes through.
            s.state.apply(case.incoming).unwrap();
            let disk = ProfileStore::open(&s.path).unwrap();
            for (id, live, pw) in &case.after {
                assert_eq!(disk.get(id).is_some(), *live, "{}: {id} live", case.what);
                assert_eq!(
                    s.secrets.password(id).as_deref(),
                    *pw,
                    "{}: {id}",
                    case.what
                );
            }
            if case.after.iter().any(|(id, ..)| *id == ID_A) && disk.get(ID_A).is_some() {
                assert_eq!(disk.get(ID_A).unwrap().updated_at, 2_000, "{}", case.what);
            }
        }
    }

    #[test]
    fn file_write_failure_rolls_back_secrets() {
        let mut s = rollback_start();
        let file = file_of(&s.path);
        // A directory where the temp file goes: the atomic write fails.
        let tmp = s.path.with_file_name("profiles.json.tmp");
        fs::create_dir(&tmp).unwrap();
        let incoming = rollback_cases().remove(0).incoming;
        let err = s.state.apply(incoming.clone()).unwrap_err();
        assert!(matches!(err, SyncError::Store(_)), "{err:?}");
        assert_rolled_back(&s, &file, "file write failed");

        fs::remove_dir(&tmp).unwrap();
        s.state.apply(incoming).unwrap();
        assert_eq!(s.secrets.password(ID_A).as_deref(), Some("pw-a-new"));
        assert_eq!(s.secrets.password(ID_B).as_deref(), Some("pw-b"));
    }

    #[test]
    fn rotated_password_is_not_rolled_back_after_failure() {
        let old = fixed("p", ID_A, 1_000);
        let rotated = fixed("p", ID_A, 2_000);
        for fail_on_server in [false, true] {
            let mut stale = side(&[(old.clone(), Some("pw-old"))]);
            let mut rot = side(&[(rotated.clone(), Some("pw-new"))]);
            let stale_file = file_of(&stale.path);
            stale.secrets.fail_nth_write(1);

            let (c, s) = if fail_on_server {
                let (c, s, state) = sync_once(&mut rot.state, stale.state);
                stale.state = state;
                (c, s)
            } else {
                let (c, s, state) = sync_once(&mut stale.state, rot.state);
                rot.state = state;
                (c, s)
            };
            assert!(c.is_err() || s.is_err(), "fail_on_server={fail_on_server}");
            assert_eq!(file_of(&stale.path), stale_file, "stale side untouched");
            assert_eq!(stale.secrets.password(ID_A).as_deref(), Some("pw-old"));

            // Retry with the failure gone: the rotation wins on both sides.
            let (c, s) = if fail_on_server {
                let (c, s, state) = sync_once(&mut rot.state, stale.state);
                stale.state = state;
                (c, s)
            } else {
                let (c, s, state) = sync_once(&mut stale.state, rot.state);
                rot.state = state;
                (c, s)
            };
            c.unwrap();
            s.unwrap();
            for one in [&stale, &rot] {
                assert_eq!(one.secrets.password(ID_A).as_deref(), Some("pw-new"));
                let disk = ProfileStore::open(&one.path).unwrap();
                assert_eq!(disk.get(ID_A).unwrap().updated_at, 2_000);
            }
        }
    }

    #[test]
    fn tombstone_delete_only_for_previously_live_ids() {
        let live = fixed("live", ID_A, 1_000);
        let gone = fixed("gone", ID_C, 0).tombstone(1_500);
        let never = fixed("never", ID_B, 0).tombstone(1_500);
        let mut a = side(&[(live.clone(), Some("pw")), (gone.clone(), None)]);
        let b = side(&[(gone.clone(), None)]);

        // An old tombstone and one for an id this machine never had.
        a.state
            .apply(vec![
                entry(&gone, None),
                entry(&never, None),
                entry(&live, Some("pw")),
            ])
            .unwrap();
        assert_eq!(a.secrets.deletes(), 0);

        // A full sync between two machines that share the tombstone.
        let (c, s, _b_state) = sync_once(&mut a.state, b.state);
        c.unwrap();
        s.unwrap();
        assert_eq!(a.secrets.deletes(), 0);
        assert_eq!(b.secrets.deletes(), 0);
        assert_eq!(a.secrets.password(ID_A).as_deref(), Some("pw"));
    }

    #[test]
    fn clean_strips_invisible_and_separator_characters() {
        let dirty = "a\u{2028}b\u{2029}c\u{200B}d\u{200C}e\u{200D}f\u{2060}g\u{FEFF}h\u{202E}i\u{2066}j\u{1b}k";
        assert_eq!(clean(dirty), "abcdefghijk");
        assert_eq!(clean("plain text: ok"), "plain text: ok");
    }

    #[test]
    fn error_messages_make_no_false_claims() {
        // These can be raised after the other machine has applied the sync.
        for e in [
            SyncError::Timeout,
            SyncError::WrongSyncKey,
            SyncError::NoSecretStore,
        ] {
            let msg = e.to_string();
            assert!(!msg.contains("nothing was changed"), "{msg}");
        }
        assert!(SyncError::Timeout.to_string().contains("may or may not"));
    }

    #[test]
    fn empty_passphrase_rejected() {
        // Client: nothing goes on the wire.
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let (cs, ss) = pair();
        let err = run_client(&cs, &cs, &mut a.state, "").unwrap_err();
        drop(cs);
        assert!(matches!(err, SyncError::EmptyPassphrase), "{err:?}");
        let mut sent = Vec::new();
        (&ss).read_to_end(&mut sent).unwrap();
        assert!(sent.is_empty());

        // Server: refuses, tells the client, applies nothing.
        let b = side(&[(prof("two"), Some("pw-two"))]);
        let b_file = file_of(&b.path);
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, "");
        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        drop(cs);
        let (served, _) = server.join().unwrap();
        assert!(matches!(served, Err(SyncError::EmptyPassphrase)));
        assert!(matches!(err, SyncError::Remote(_)), "client got {err:?}");
        assert_eq!(file_of(&b.path), b_file);

        // Over ssh: the command is never started.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let cmd = SshCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"touch "$1""#.to_string(),
                "sh".to_string(),
                marker.to_str().unwrap().to_string(),
            ],
            env: vec![],
        };
        let err = sync_over_ssh(&mut a.state, &cmd, "").unwrap_err();
        assert!(matches!(err, SyncError::EmptyPassphrase), "{err:?}");
        assert!(!marker.exists());
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn hung_child_is_killed_after_timeout() {
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let cmd = SshCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"echo $$ > "$1"; exec sleep 30"#.to_string(),
                "sh".to_string(),
                pidfile.to_str().unwrap().to_string(),
            ],
            env: vec![],
        };
        let start = Instant::now();
        let err = sync_over_ssh_with_timeout(&mut a.state, &cmd, PASS, Duration::from_secs(1))
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(err, SyncError::Timeout), "got {err:?}");
        assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");

        // The child is gone: not running, not a zombie of ours.
        let pid: u32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            // Only possible if the pid was reused: then it is not our child.
            let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
            let ppid: u32 = after_comm.split(' ').nth(1).unwrap().parse().unwrap();
            assert_ne!(ppid, std::process::id(), "child still ours: {stat}");
        }
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn serialization_failure_is_reported() {
        let mut odd = prof("odd key path");
        odd.key_file = Some(PathBuf::from(OsStr::from_bytes(b"/home/a/\xff\xfe")));
        let mut stub = Stub {
            entries: vec![entry(&odd, Some("pw"))],
            ..Stub::default()
        };
        let b = side(&[(prof("two"), Some("pw-two"))]);
        let b_file = file_of(&b.path);
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, PASS);
        let err = run_client(&cs, &cs, &mut stub, PASS).unwrap_err();
        drop(cs);
        let (served, _) = server.join().unwrap();
        let SyncError::Store(msg) = &err else {
            panic!("expected Store, got {err:?}");
        };
        assert!(msg.contains("key file"), "{msg:?}");
        assert!(served.is_err());
        assert!(stub.applied.is_none());
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(b.secrets.writes(), 0);
    }

    #[test]
    fn frames_roundtrip() {
        let mut out = Vec::new();
        write_frame(&mut out, KIND_BUNDLE, b"abc").unwrap();
        assert_eq!(out, [0, 0, 0, 4, KIND_BUNDLE, b'a', b'b', b'c']);
        write_frame(&mut out, KIND_HELLO, &[]).unwrap();
        write_frame(&mut out, KIND_ERROR, b"\x01msg").unwrap();
        assert_eq!(
            frames(&out),
            vec![
                (KIND_BUNDLE, b"abc".to_vec()),
                (KIND_HELLO, vec![]),
                (KIND_ERROR, b"\x01msg".to_vec()),
            ]
        );

        // A truncated frame is an I/O error.
        for cut in [0, 2, 4, 6] {
            let mut r = Cursor::new(out[..cut].to_vec());
            assert!(
                matches!(read_frame(&mut r), Err(SyncError::Io(_))),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn zero_length_frame_rejected() {
        let mut r = ThenPanic(Cursor::new(vec![0, 0, 0, 0]));
        assert!(matches!(read_frame(&mut r), Err(SyncError::Protocol(_))));
    }

    #[test]
    fn error_frames_map_to_errors() {
        assert!(matches!(
            error_from_payload(b"\x01x"),
            SyncError::WrongSyncKey
        ));
        assert!(matches!(
            error_from_payload(b"\x02"),
            SyncError::VersionMismatch
        ));
        assert!(matches!(
            error_from_payload(b"\x03x"),
            SyncError::NoSecretStore
        ));
        assert!(matches!(error_from_payload(b""), SyncError::Protocol(_)));
        let SyncError::Remote(msg) = error_from_payload(b"\xffdisk \x1b[2Jfull\n\xff") else {
            panic!("expected Remote");
        };
        assert!(msg.contains("disk") && msg.contains("full"), "{msg:?}");
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
        let long = [&[255u8][..], &[b'y'; 5000][..]].concat();
        let SyncError::Remote(msg) = error_from_payload(&long) else {
            panic!("expected Remote");
        };
        assert!(msg.len() <= 400);
        // Unknown codes are generic remote errors.
        assert!(matches!(error_from_payload(b"\x09?"), SyncError::Remote(_)));
    }

    #[test]
    fn unexpected_frame_kind_is_a_protocol_error() {
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let (cs, ss) = pair();
        let server = thread::spawn(move || {
            read_frame(&mut &ss).unwrap();
            // A BUNDLE where a HELLO belongs.
            write_frame(&mut &ss, KIND_BUNDLE, b"zzz").unwrap();
            let mut rest = Vec::new();
            (&ss).read_to_end(&mut rest).unwrap();
        });
        let err = run_client(&cs, &cs, &mut a.state, PASS).unwrap_err();
        drop(cs);
        server.join().unwrap();
        assert!(matches!(err, SyncError::Protocol(_)), "{err:?}");
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn export_reads_fresh_state_and_passwords() {
        let p1 = prof("one");
        let a = side(&[(p1.clone(), Some("pw-one"))]);
        let mut other = ProfileStore::open(&a.path).unwrap();
        let p2 = prof("two");
        other.upsert(p2.clone()).unwrap();
        other.delete(&p1.id).unwrap();
        a.secrets
            .0
            .inner
            .set(&password_key(&p2.id), "pw-two")
            .unwrap();

        let mut got = a.state.export().unwrap();
        got.sort_by(|x, y| x.profile.id.cmp(&y.profile.id));
        let want_ids = {
            let mut v = vec![p1.id.clone(), p2.id.clone()];
            v.sort();
            v
        };
        assert_eq!(
            got.iter().map(|e| e.profile.id.clone()).collect::<Vec<_>>(),
            want_ids
        );
        for e in &got {
            if e.profile.id == p1.id {
                assert!(e.profile.deleted);
                assert_eq!(e.password, None, "tombstones carry no password");
            } else {
                assert_eq!(e.password.as_deref(), Some("pw-two"));
            }
        }
    }

    #[test]
    fn local_state_is_send_and_ready_uses_probe() {
        fn assert_send<T: Send>() {}
        assert_send::<LocalState>();

        struct NoService;
        impl SecretStore for NoService {
            fn get(&self, _: &str) -> Result<Option<zeroize::Zeroizing<String>>, SecretError> {
                Err(SecretError::Unavailable("none".into()))
            }
            fn set(&self, _: &str, _: &str) -> Result<(), SecretError> {
                Err(SecretError::Unavailable("none".into()))
            }
            fn delete(&self, _: &str) -> Result<(), SecretError> {
                Err(SecretError::Unavailable("none".into()))
            }
            fn probe(&self) -> Result<(), SecretError> {
                Err(SecretError::Unavailable("none".into()))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(&dir.path().join("p.json")).unwrap();
        let state = LocalState::new(store, Box::new(NoService));
        assert!(matches!(state.ready(), Err(SyncError::NoSecretStore)));
    }
}
