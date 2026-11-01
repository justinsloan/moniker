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
//! | 1    | HELLO  | `[protocol version]` (1, or 2 while a profile sent has a jump host or in answer to a 2), from the client optionally followed by a flags byte |
//! | 2    | BUNDLE | a sealed bundle (see [`crate::crypto`])          |
//! | 3    | ERROR  | `[code u8]` then a UTF-8 message                 |
//!
//! Error codes: 1 wrong sync key, 2 version mismatch (a Moniker older than jump
//! hosts answers a HELLO of 2 this way, and the sender reports
//! [`SyncError::PeerTooOld`] for it), 3 no secret store,
//! 4 the server has been away ([`SyncError::Stale`] there,
//! [`SyncError::PeerStale`] here), 5 sync is turned off on the server
//! ([`SyncError::SyncOff`] there, [`SyncError::PeerSyncOff`] here), 255
//! anything else. Messages are fixed, non-secret text.
//!
//! The client's HELLO flags byte has one flag, bit 0: the person accepted
//! syncing with a machine that has been away ([`StaleSync::Allow`]). A client
//! that accepts nothing sends a one-byte HELLO, exactly what clients sent
//! before the flags existed, and servers that predate them read only the first
//! byte, so the flags need no new protocol version.
//!
//! # Exchange
//!
//! ```text
//! client                                  server
//!   export (once)
//!   HELLO [1 or 2]  ──────────────────▶   sync on? passphrase set? ready()?
//!                                         version 1 or 2? not away? export (once)
//!              ◀───────────────────────   HELLO [1 or 2]        (or ERROR, stop)
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
//! # Forgetting deletions (issue #8)
//!
//! A tombstone is kept for [`TOMBSTONE_KEEP_MS`] after the deletion it
//! records, and only dropped once a completed sync has carried it away from
//! this machine (see `plan_apply`). A machine that has not completed a sync
//! for longer than [`STALE_AFTER_MS`] may hold a profile whose deletion the
//! others have since forgotten, and would bring it back, so a sync with one
//! stops unless the person accepted that: a client checks itself before it
//! connects ([`SyncError::Stale`]), a server checks itself after the HELLO
//! and answers with error 4. When this machine last completed a sync is kept
//! by [`crate::sync_state`]; a client records it after applying, a server
//! after sending its reply.
//!
//! # Sync turned off (issue #10)
//!
//! The person can turn sync off on a machine (the switch lives in
//! `profiles.json`, see [`crate::store::SyncSwitch`]). Then that machine
//! takes part in no sync, in either role, whoever starts it: a client stops
//! before it connects and sends nothing ([`SyncError::SyncOff`]), a server
//! answers the HELLO with error 5 and goes no further, so the client reports
//! [`SyncError::PeerSyncOff`]. The switch is the first thing either side
//! looks at, before the sync key, the secret store or how long the machine
//! has been away, and it is read from the file each time
//! ([`SyncTarget::sync_on`]), so turning it off in the app stops a
//! `moniker-server sync-serve` that is only now answering. A switch that is
//! in the file but cannot be understood counts as off. Old servers know no
//! code 5 and show its text as it is; the HELLO is unchanged, so there is no
//! new protocol version. [`refuse`] lets `sync-serve` answer this way before
//! it asks the keyring for the sync key.
//!
//! # Profiles kept on this machine (issue #10)
//!
//! A profile can be kept on this machine only ([`Profile::local_only`]). Then
//! nothing about it leaves but its id and a stamp: both roles export it as
//! its tombstone, stamped with its own `updated_at`, and never read its
//! password for a sync. To the other machines that is an ordinary deletion,
//! which removes their copy and its password (older versions understand it
//! too). Nothing that arrives for the id changes the profile or its password
//! here, in either role; when a peer had edited its copy after the profile was
//! kept here, the local stamp is raised to that edit's, so that the next
//! sync's tombstone wins the tie (see `plan_apply`). Turning sync back on for
//! the profile is an ordinary edit, stamped above every tombstone it sent or
//! met, so the live profile and its password reach the others at the next
//! sync. Whether a profile is kept is this machine's own choice: the flag is
//! cleared on everything received. The HELLO and the bundle format are
//! unchanged.
//!
//! # Jump hosts (issue #9)
//!
//! A profile can go through another profile, its jump host
//! ([`Profile::jump_host`]). A Moniker older than that knows nothing of the
//! field, drops it when it writes the profile, and would connect the profile
//! straight to its host, so it must not take part in a sync that carries one.
//! The HELLO's version says so: a machine announces [`PROTOCOL_JUMP_HOSTS`]
//! (2) exactly when a live profile it sends has a jump host
//! ([`holds_jump_host`]), and [`PROTOCOL_VERSION`] (1) otherwise, so two
//! machines with no jump host say byte for byte what machines said before.
//! A profile kept on this machine goes as its tombstone, which has none, so it
//! does not count.
//!
//! * A client says 2 or 1 as above. A server accepts 1 or 2 and answers 2 if
//!   the client said it or if its own export needs it, else 1. A client that
//!   said 2 accepts only a 2 back; one that said 1 accepts either, since this
//!   build can read what a server with jump hosts sends it.
//! * An older server turns a 2 away with error 2 before reading any bundle, and
//!   the client reports [`SyncError::PeerTooOld`] (it sent nothing but the
//!   HELLO). An older client turns a server's 2 away the same way, with its
//!   "incompatible versions" text, and the server (which answered 2) reports
//!   [`SyncError::PeerTooOld`] and applies nothing. Either way both machines are
//!   left as they were, and syncing works again as soon as no live profile has
//!   a jump host, or the older machine is updated.
//! * The export a side's HELLO is decided from is the one it sends or merges:
//!   each side reads it once per sync ([`SyncTarget::export`]; the file is read
//!   afresh at each call, so a second read could differ). A client reads it
//!   after its own checks and before the HELLO; a server after the checks that
//!   come before the answer (sync on, key, secret store, version, away), which
//!   keep their order from issues #8 and #10.
//! * A profile that arrives with a jump host this machine does not have (it is
//!   kept on the other machine only, or was deleted) is accepted like any
//!   other: it waits, and the GUI says so. A jump host that is not a canonical
//!   id, or is the profile's own id, makes the entry invalid
//!   ([`Profile::validate`]) and rejects the whole sync.
//! * A sync with a partner that goes through a jump host goes through it too
//!   ([`sync_over_ssh`] with a [`JumpLeg`]): a master `ssh` signs in to the
//!   jump host first, the sync's own `ssh` reaches the partner through the
//!   master's shared connection, and the master is ended when the sync
//!   returns, however it returns. A jump host whose key is not known yet, or
//!   that refuses the login, stops the sync before the partner is tried.
//!
//! # Passwords: set and changed travel, removal does not
//!
//! A password reaches the other machine by being set or changed. Clearing one
//! (the editor's "Remove stored password") stays on the machine where it was
//! done, and a later sync from a machine that still has the password can bring
//! it back. This is an intentional limit; see `plan_apply`.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::bundle::{BundleEntry, merge};
use crate::crypto::{self, CryptoError};
use crate::jump::{CONTROL_LIMIT, JumpLeg, end_master, holds_jump_host, learn_master, run_control};
use crate::profile::{Profile, now_ms};
use crate::secrets::{SecretError, SecretStore, password_key};
use crate::ssh::SshCommand;
use crate::store::{ProfileStore, StoreError};
use crate::sync_state;

/// Largest frame accepted or sent, kind byte included (16 MiB).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// The protocol version carried in HELLO while no profile sent has a jump host
/// ([`PROTOCOL_JUMP_HOSTS`] otherwise, and in a server's answer to a 2).
pub const PROTOCOL_VERSION: u8 = 1;
/// The protocol version a machine announces while a live profile it sends has
/// a jump host (issue #9). An older Moniker knows nothing of jump hosts and
/// would connect such a profile straight to its host, so it refuses this
/// version (error 2) and the sync stops before any profile data is sent.
pub const PROTOCOL_JUMP_HOSTS: u8 = 2;
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
const CODE_STALE: u8 = 4;
const CODE_SYNC_OFF: u8 = 5;
const CODE_OTHER: u8 = 255;

/// The bit of the client's HELLO flags byte that says the person accepted
/// syncing with a machine that has been away ([`StaleSync::Allow`]).
const HELLO_ALLOW_STALE: u8 = 1;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How long a tombstone is kept after the deletion it records (issue #8): a
/// year, from its `updated_at`.
pub const TOMBSTONE_KEEP_MS: i64 = 365 * DAY_MS;

/// How long a machine may go without completing a sync before a sync with it
/// asks first ([`StaleSync`]): half of [`TOMBSTONE_KEEP_MS`].
pub const STALE_AFTER_MS: i64 = 180 * DAY_MS;

/// Whether a sync goes ahead when this machine or the other one has not
/// completed a sync for longer than [`STALE_AFTER_MS`] (issue #8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleSync {
    /// Stop with [`SyncError::Stale`] or [`SyncError::PeerStale`].
    Refuse,
    /// Go ahead: the person accepted that a deletion the others have forgotten
    /// may come back.
    Allow,
}

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
    /// This machine has not completed a sync for longer than
    /// [`STALE_AFTER_MS`]; nothing was sent.
    #[error(
        "this machine has not synced for {days} days, so a profile deleted on another machine more than a year ago could come back on every machine"
    )]
    Stale { days: i64 },
    /// The other machine has not completed a sync for longer than
    /// [`STALE_AFTER_MS`]; nothing was sent but the HELLO.
    #[error(
        "the other machine has not synced for more than 180 days, so a profile deleted on another machine more than a year ago could come back on every machine"
    )]
    PeerStale,
    /// The person turned sync off on this machine (issue #10); nothing was
    /// sent.
    #[error("sync is turned off on this machine")]
    SyncOff,
    /// Sync is turned off on the other machine; nothing was sent but the
    /// HELLO.
    #[error("sync is turned off on the other machine")]
    PeerSyncOff,
    /// The other machine runs a Moniker older than jump hosts (issue #9) while
    /// a profile has one; nothing was sent but the HELLO.
    #[error(
        "the other machine runs an older Moniker, which does not know jump hosts; update it to sync while a profile has a jump host"
    )]
    PeerTooOld,
    /// The sync goes through a jump host (issue #9) whose host key is not
    /// known yet: its connection, which has no terminal to ask in and checks
    /// host keys strictly, stopped there. Nothing was sent. `name` is the jump
    /// host's profile name.
    #[error("connect to {name} once in a tab first, so its host key is known")]
    JumpHostKeyUnknown { name: String },
    /// The connection to the jump host `name` failed (a refused login, an
    /// unreachable host, too slow) or could not start; the target was not
    /// tried and nothing was sent. `detail` says how ssh ended, with the last
    /// lines it wrote, sanitized.
    #[error("could not connect to the jump host {name}: {detail}")]
    JumpFailed { name: String, detail: String },
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
    /// `Ok` unless the person turned sync off on this side (issue #10), which
    /// is [`SyncError::SyncOff`]. Looked at before anything else, in either
    /// role.
    fn sync_on(&self) -> Result<(), SyncError> {
        Ok(())
    }
    /// `Ok` if received data can be stored (the secret store is usable).
    fn ready(&self) -> Result<(), SyncError>;
    /// Every profile, tombstones included, live ones with their passwords; a
    /// profile kept on this machine (issue #10) as its tombstone.
    fn export(&self) -> Result<Vec<BundleEntry>, SyncError>;
    /// Store the merged set. Entries are already validated by the protocol.
    fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError>;
    /// When this side last completed a sync, if that is known.
    fn last_sync(&self) -> Option<i64> {
        None
    }
    /// Record that a sync completed now. Best effort.
    fn record_sync(&mut self) {}
    /// The time now, in milliseconds since the epoch.
    fn now(&self) -> i64 {
        now_ms()
    }
}

/// The real sync target: the profile file plus the secret store.
pub struct LocalState {
    store: ProfileStore,
    secrets: Box<dyn SecretStore>,
    clock: Box<dyn Fn() -> i64 + Send>,
}

impl LocalState {
    pub fn new(store: ProfileStore, secrets: Box<dyn SecretStore>) -> Self {
        LocalState::with_clock(store, secrets, Box::new(now_ms))
    }

    /// A [`LocalState`] whose time is `clock` (milliseconds since the epoch)
    /// rather than the system clock: for tests that let a year go by.
    pub fn with_clock(
        store: ProfileStore,
        secrets: Box<dyn SecretStore>,
        clock: Box<dyn Fn() -> i64 + Send>,
    ) -> Self {
        LocalState {
            store,
            secrets,
            clock,
        }
    }

    /// The profile store, as of the last write by [`apply`](SyncTarget::apply)
    /// (or as it was opened).
    pub fn store(&self) -> &ProfileStore {
        &self.store
    }
}

impl SyncTarget for LocalState {
    /// Reads the profile file afresh (not the snapshot): the switch is turned
    /// off by the app or by `moniker-server` in another process, and a sync
    /// that runs on must see it at once. A switch the file holds but that
    /// cannot be understood counts as off ([`SyncSwitch`]); a file that cannot
    /// be read is the store's error, as for every other read of it.
    ///
    /// [`SyncSwitch`]: crate::store::SyncSwitch
    fn sync_on(&self) -> Result<(), SyncError> {
        let fresh = ProfileStore::open(self.store.path()).map_err(store_error)?;
        if fresh.sync_switch().is_on() {
            Ok(())
        } else {
            Err(SyncError::SyncOff)
        }
    }

    fn ready(&self) -> Result<(), SyncError> {
        self.secrets.probe().map_err(|_| SyncError::NoSecretStore)
    }

    /// Reads the profile file afresh (not the snapshot), so edits made since
    /// the store was opened are included. A profile kept on this machine goes
    /// as its tombstone (`exported`), turned into one before any password is
    /// looked up, so its password is not even read.
    fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
        let fresh = ProfileStore::open(self.store.path()).map_err(store_error)?;
        let sent: Vec<Profile> = fresh.all().iter().map(exported).collect();
        with_passwords(&sent, &*self.secrets).map_err(secret_error)
    }

    /// Merges `merged` into the profile file as it is now, under the store
    /// lock, so an edit made by another process since [`export`] is kept when
    /// it is newer.
    ///
    /// Still under the lock, the secrets go first: the passwords of incoming
    /// winners are stored and the passwords of profiles this sync deletes are
    /// removed (never that of a profile kept on this machine, see
    /// `plan_apply`), each previous value recorded in an undo log. Only then is the
    /// profile file written. If a secret operation or the file write fails, the
    /// secrets are restored from the log (best effort) and the file is left as
    /// it was. Invariant: a profile version on disk implies its secret is
    /// already stored; a failure leaves the secrets at worst ahead of the file,
    /// never behind, and the next sync converges them.
    ///
    /// [`export`]: SyncTarget::export
    fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
        validate_entries(&merged)?;
        // Tombstones a year old that a completed sync has already carried
        // away are not kept ([`TOMBSTONE_KEEP_MS`]). A last sync recorded
        // ahead of the clock says nothing about that: nothing is forgotten
        // until a sync has recorded the clock's time.
        let now = self.now();
        let prune_before = self
            .last_sync()
            .filter(|last| last.saturating_sub(now) <= sync_state::AHEAD_MS)
            .map(|last| last.min(now.saturating_sub(TOMBSTONE_KEEP_MS)));
        let secrets: &dyn SecretStore = &*self.secrets;
        let mut undo = UndoLog::default();
        // `update` wants a StoreError from the closure; keep the real cause.
        let mut secret_failure = None;
        let result = self.store.update(|disk| {
            let staged = with_passwords(disk, secrets).and_then(|local| {
                let (profiles, (outcome, ops)) = plan_apply(local, merged, prune_before);
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

    fn last_sync(&self) -> Option<i64> {
        sync_state::last_sync(self.store.path())
    }

    /// Best effort: a sync that could not be recorded only makes pruning
    /// later, and the question for a machine that has been away earlier.
    fn record_sync(&mut self) {
        let _ = sync_state::record_sync(self.store.path(), (self.clock)());
    }

    fn now(&self) -> i64 {
        (self.clock)()
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
///
/// # Forgetting deletions (issue #8)
///
/// A tombstone whose `updated_at` is before `prune_before` is left out of the
/// profiles to write, after the merge has used it, unless it has just deleted
/// a live copy here (password included): that one is kept until the next sync,
/// so a deletion that arrives late is still handed on to the next machine
/// rather than stopping at the first one past the year. [`LocalState::apply`] sets
/// `prune_before` to [`TOMBSTONE_KEEP_MS`] ago, or to this machine's last
/// completed sync if that is earlier, so a deletion is forgotten a year after
/// it was made, and only once a sync has carried it away from this machine.
/// Every machine measures from the same deletion time, so one that still has
/// the tombstone and sends it back does not make the others keep it again.
/// `None`: nothing is forgotten.
///
/// # Profiles kept on this machine (issue #10)
///
/// A profile that is live and `local_only` in `local` is written back exactly
/// as it is, whatever the merge chose for its id: a newer edit from a peer, a
/// deletion, anything. No secret operation touches its password and nothing
/// is counted. The peers only ever get its tombstone, stamped with its own
/// `updated_at` (see `exported`), which deletes their copy because a
/// tombstone wins a tie and the change that kept the profile here was an edit
/// stamped above every version this machine had seen.
///
/// One thing changes: when the merge carries a newer version of the id
/// (a peer edited its copy after this machine kept the profile, before it
/// heard), the local `updated_at` is raised to that version's. The next
/// sync's tombstone, at that same stamp, then beats the edit, because a
/// tombstone wins a tie on `updated_at` (rule 2 of [`merge`]); so the
/// deletion wins on the sync after the one that saw the edit, without
/// trusting any clock. Nothing is added to the stamp, so nothing can
/// overflow: a peer that pinned the profile at `i64::MAX` gets a tombstone at
/// `i64::MAX`, which still wins. A newer tombstone (a peer deleted its copy,
/// or keeps it too) raises the stamp as well, so it is never below any
/// version this machine has heard of: turning sync back on, an ordinary edit
/// stamped one above it, then beats those deletions too, not only the
/// tombstones this machine sent, whatever the clocks say (except at
/// `i64::MAX`, where turning sync back on is refused). Two machines that both
/// keep the profile each keep their own copy and settle on the same stamp,
/// after which a sync between them writes nothing.
///
/// The kept profile is live, so the pruning above never touches it.
fn plan_apply(
    local: Vec<BundleEntry>,
    incoming: Vec<BundleEntry>,
    prune_before: Option<i64>,
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
        if let Some(kept) = prev.filter(|p| p.profile.local_only && !p.profile.deleted) {
            // Kept on this machine: left alone, password included, only
            // raised to the stamp of a newer version (see the docs above).
            let mut profile = kept.profile.clone();
            profile.updated_at = profile.updated_at.max(w.profile.updated_at);
            profiles.push(profile);
            continue;
        }
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
            if !was_live && prune_before.is_some_and(|cutoff| w.profile.updated_at < cutoff) {
                // Forgotten (see the docs above): not written.
                continue;
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

/// What this machine sends for `p`: the profile itself, or for one kept on
/// this machine (issue #10) its tombstone, stamped with the profile's own
/// `updated_at`, which carries nothing but the id. See `plan_apply` for why
/// that stamp makes the deletion win.
fn exported(p: &Profile) -> Profile {
    if p.local_only && !p.deleted {
        p.tombstone(p.updated_at)
    } else {
        p.clone()
    }
}

/// Entries for `profiles`: live ones with their stored password, tombstones
/// without, and profiles kept on this machine without too. A sync never needs
/// a kept profile's password (`plan_apply` neither sends nor changes it), so
/// it is not read: not taken out of the keyring into this process at all.
fn with_passwords(
    profiles: &[Profile],
    secrets: &dyn SecretStore,
) -> Result<Vec<BundleEntry>, SecretError> {
    profiles
        .iter()
        .map(|p| {
            let password = if p.deleted || p.local_only {
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
        CODE_STALE => SyncError::PeerStale,
        CODE_SYNC_OFF => SyncError::PeerSyncOff,
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
        SyncError::Stale { .. } | SyncError::PeerStale => CODE_STALE,
        SyncError::SyncOff | SyncError::PeerSyncOff => CODE_SYNC_OFF,
        _ => CODE_OTHER,
    };
    let text = match err {
        // Said from the peer's side, for a Moniker that knows no code 4 or 5
        // and shows the text as it is.
        SyncError::SyncOff => SyncError::PeerSyncOff.to_string(),
        SyncError::Stale { days } => format!(
            "the other machine has not synced for {days} days, so a profile deleted on another \
             machine more than a year ago could come back on every machine; a newer Moniker can \
             sync anyway"
        ),
        _ => err.to_string(),
    };
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

/// `Err(Stale)` if `local` has not completed a sync for longer than
/// [`STALE_AFTER_MS`] and the person did not accept that. A side with no
/// recorded sync is not asked: a new machine has nothing the others could
/// have forgotten deleting. (The exception, documented: a machine last synced
/// by Moniker 0.5.0 or older, which kept no record, and upgraded after more
/// than a year away.)
fn check_not_away(local: &dyn SyncTarget, stale: StaleSync) -> Result<(), SyncError> {
    let Some(last) = local.last_sync() else {
        return Ok(());
    };
    let away = local.now().saturating_sub(last);
    if stale == StaleSync::Refuse && away > STALE_AFTER_MS {
        return Err(SyncError::Stale {
            days: away / DAY_MS,
        });
    }
    Ok(())
}

/// What the client's HELLO says about a server that has been away: its
/// optional second byte. A one-byte HELLO (every client before issue #8)
/// accepts nothing.
fn stale_from_hello(hello: &[u8]) -> StaleSync {
    match hello.get(1) {
        Some(flags) if flags & HELLO_ALLOW_STALE != 0 => StaleSync::Allow,
        _ => StaleSync::Refuse,
    }
}

/// The client's HELLO: the protocol version `said`, then the flags byte only
/// when there is a flag to send, so a client that accepts nothing and has no
/// jump host sends exactly what clients before issues #8 and #9 sent.
fn client_hello(said: u8, stale: StaleSync) -> Vec<u8> {
    match stale {
        StaleSync::Refuse => vec![said],
        StaleSync::Allow => vec![said, HELLO_ALLOW_STALE],
    }
}

/// The version `entries` need (issue #9): [`PROTOCOL_JUMP_HOSTS`] while a live
/// profile among them has a jump host, else [`PROTOCOL_VERSION`]. Decided from
/// the very entries that are then sent, never from a second read.
fn version_for(entries: &[BundleEntry]) -> u8 {
    if holds_jump_host(entries.iter().map(|e| &e.profile)) {
        PROTOCOL_JUMP_HOSTS
    } else {
        PROTOCOL_VERSION
    }
}

fn check_passphrase(passphrase: &str) -> Result<(), SyncError> {
    if passphrase.is_empty() {
        Err(SyncError::EmptyPassphrase)
    } else {
        Ok(())
    }
}

/// The version in a peer's HELLO, if this build speaks it (1 or 2).
fn peer_version(hello: &[u8]) -> Result<u8, SyncError> {
    match hello.first() {
        Some(&version @ (PROTOCOL_VERSION | PROTOCOL_JUMP_HOSTS)) => Ok(version),
        Some(_) => Err(SyncError::VersionMismatch),
        None => Err(SyncError::Protocol("empty HELLO".to_string())),
    }
}

/// Whether the server's HELLO answers a client that said `said`: a client with
/// no jump host takes either version, and a client that said 2 takes only 2,
/// because a server that answers 1 to it has not understood it, and a profile
/// with a jump host must not be sent to one that does not know jump hosts.
fn check_reply(hello: &[u8], said: u8) -> Result<(), SyncError> {
    if peer_version(hello)? >= said {
        Ok(())
    } else {
        Err(SyncError::VersionMismatch)
    }
}

/// What an ERROR frame the peer sent in answer to our HELLO of version `said`
/// means. Error 2 in answer to version 2 is a Moniker that predates jump hosts
/// ([`SyncError::PeerTooOld`], which says what to do), not some other
/// incompatibility.
fn refused_version(err: SyncError, said: u8) -> SyncError {
    match err {
        SyncError::VersionMismatch if said == PROTOCOL_JUMP_HOSTS => SyncError::PeerTooOld,
        other => other,
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

/// Open the peer's bundle. Whether a profile is kept on this machine is this
/// machine's own choice, so `local_only` is cleared on every entry: a peer
/// (an older version, a bug, a hostile one) cannot make this machine stop
/// sending a profile and delete it from every other machine.
fn open_bundle(passphrase: &str, sealed: &[u8]) -> Result<Vec<BundleEntry>, SyncError> {
    let mut entries = crypto::open(passphrase, sealed).map_err(|e| match e {
        CryptoError::WrongKeyOrCorrupt => SyncError::WrongSyncKey,
        CryptoError::EmptyPassphrase => SyncError::EmptyPassphrase,
        CryptoError::BadFormat => {
            SyncError::Remote("the other machine sent data this version cannot read".to_string())
        }
        other => SyncError::Remote(other.to_string()),
    })?;
    for e in &mut entries {
        e.profile.local_only = false;
    }
    Ok(entries)
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
    let (said, entries) = tell(&mut w, accept_hello(&*local, passphrase, &hello))?;
    // Version 2 if the client said it or if what this server sends needs it
    // (issue #9). A client of an older Moniker turns 2 away, and nothing more
    // is sent or applied.
    let answer = said.max(version_for(&entries));
    write_frame(&mut w, KIND_HELLO, &[answer])?;

    let sealed = read_expected(&mut r, KIND_BUNDLE).map_err(|e| refused_version(e, answer))?;
    let (merged, reply) = tell(&mut w, server_merge(entries, passphrase, &sealed))?;
    let outcome = tell(&mut w, local.apply(merged))?;
    write_frame(&mut w, KIND_BUNDLE, &reply)?;
    local.record_sync();
    Ok(outcome)
}

/// What a server checks before it answers a HELLO, in this order (the first
/// failure is the answer): sync is on (issue #10), a sync key is set, the secret
/// store works, the version is one this build speaks, this machine has not been
/// away (issue #8). Only then is its export read, once: the HELLO's version and
/// the merge are both made from that one read (issue #9). Returns the client's
/// version and the export.
fn accept_hello(
    local: &dyn SyncTarget,
    passphrase: &str,
    hello: &[u8],
) -> Result<(u8, Vec<BundleEntry>), SyncError> {
    local.sync_on()?;
    check_passphrase(passphrase)?;
    local.ready()?;
    let said = peer_version(hello)?;
    check_not_away(local, stale_from_hello(hello))?;
    Ok((said, local.export()?))
}

/// Answer the client's HELLO with `err` and do nothing else, for a server that
/// knows it will not take part before it has what [`serve`] needs: `sync-serve`
/// with sync turned off must not even ask the keyring for the sync key (issue
/// #10). The HELLO is read first so that the ERROR is the answer to something
/// the client said, as in [`serve`]. Returns `Err(err)`, or the reason the
/// HELLO could not be read.
pub fn refuse(mut r: impl Read, mut w: impl Write, err: SyncError) -> Result<(), SyncError> {
    read_expected(&mut r, KIND_HELLO)?;
    tell(&mut w, Err(err))
}

/// Open and check the client's bundle, merge it with the local export
/// (`exported`, read once in [`accept_hello`]), and seal the result. Nothing is
/// changed yet.
fn server_merge(
    exported: Vec<BundleEntry>,
    passphrase: &str,
    sealed: &[u8],
) -> Result<(Vec<BundleEntry>, Vec<u8>), SyncError> {
    let incoming = open_bundle(passphrase, sealed)?;
    validate_entries(&incoming)?;
    let merged = merge(exported, incoming);
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
    stale: StaleSync,
) -> Result<SyncOutcome, SyncError> {
    local.sync_on()?;
    check_passphrase(passphrase)?;
    local.ready()?;
    check_not_away(&*local, stale)?;
    // Read once, before the HELLO: the version it says is decided from the
    // entries that are sealed below, whatever is edited meanwhile (issue #9).
    // Nothing of them is sent before the server has answered. A read that
    // fails is told to the server in place of the HELLO (every server reads an
    // ERROR there), so it reports why instead of a connection that closed.
    let entries = tell(&mut w, local.export())?;
    let said = version_for(&entries);
    write_frame(&mut w, KIND_HELLO, &client_hello(said, stale))?;
    let hello = read_expected(&mut r, KIND_HELLO).map_err(|e| refused_version(e, said))?;
    let sealed = tell(
        &mut w,
        check_reply(&hello, said).and_then(|()| seal_bundle(passphrase, &entries)),
    );
    // The export holds every password in plain text (zeroized when dropped):
    // gone now that it is sealed, not once the server has answered.
    drop(entries);
    write_frame(&mut w, KIND_BUNDLE, &sealed?)?;

    let reply = read_expected(&mut r, KIND_BUNDLE)?;
    let merged = open_bundle(passphrase, &reply)?;
    validate_entries(&merged)?;
    let outcome = local.apply(merged)?;
    local.record_sync();
    Ok(outcome)
}

// ----- ssh transport -----

/// Run `cmd` (normally [`crate::ssh::sync_command`]) and sync with the
/// `moniker-server sync-serve` at the other end of its stdin/stdout, through
/// `jump` when the partner goes through a jump host (issue #9).
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
///
/// With `jump`, whose [`hop`](JumpLeg::hop) `cmd` is to carry as its
/// `ProxyCommand` (`-o ProxyCommand=<hop>` among its options; a `cmd` that does
/// not is an [`io::ErrorKind::InvalidInput`] error, and nothing starts, so a
/// caller cannot sign in to the jump host and reach the partner directly), the
/// leg's master signs in to the jump host before `cmd` starts, within the same
/// limit, and is ended when this returns, whatever happens after it signed in,
/// a panic included (see `sign_in_to_jump_host`).
/// A master that fails stops the sync before `cmd` starts:
/// [`SyncError::JumpHostKeyUnknown`] when its stderr says "Host key
/// verification failed", else [`SyncError::JumpFailed`].
///
/// Nothing is started, the master included, when this machine does not take
/// part (sync turned off, no passphrase, no secret store, or away and not
/// accepted).
pub fn sync_over_ssh(
    local: &mut dyn SyncTarget,
    cmd: &SshCommand,
    jump: Option<&JumpLeg>,
    passphrase: &str,
    terminal: SshTerminal,
    stale: StaleSync,
) -> Result<SyncOutcome, SyncError> {
    sync_over_ssh_with_timeout(local, cmd, jump, passphrase, terminal, stale, SYNC_TIMEOUT)
}

/// Whether the `ssh` of a sync, and the master of its jump connection if it
/// has one, keep the controlling terminal of this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshTerminal {
    /// It keeps it, so `ssh` can ask there (a key passphrase, a ProxyJump
    /// hop's password, a password when none is stored): `moniker-server sync`,
    /// which a person runs in a terminal.
    Inherit,
    /// It runs in a session of its own with no controlling terminal, so such a
    /// question fails at once instead of appearing in whatever terminal this
    /// process was started from, where nobody is looking: the GUI's syncs, the
    /// one at launch included. The stored password still reaches `ssh` through
    /// the askpass helper, and `ssh` is still a child of this process (the
    /// helper checks that). With no password stored, `ssh` may still show the
    /// desktop's own graphical ssh-askpass if one is installed, as it always
    /// did for a Moniker started from the desktop.
    Detached,
}

fn sync_over_ssh_with_timeout(
    local: &mut dyn SyncTarget,
    cmd: &SshCommand,
    jump: Option<&JumpLeg>,
    passphrase: &str,
    terminal: SshTerminal,
    stale: StaleSync,
    timeout: Duration,
) -> Result<SyncOutcome, SyncError> {
    // No point connecting if sync is turned off, if received passwords could
    // not be stored, or if this machine has been away and the person has not
    // accepted that: checked before any ssh starts, the jump connection's too.
    local.sync_on()?;
    check_passphrase(passphrase)?;
    local.ready()?;
    check_not_away(&*local, stale)?;
    // Fail closed: a leg's master signs in to the jump host only for a command
    // that goes through it, or the sync would reach the partner directly.
    if jump.is_some_and(|leg| !goes_through(cmd, leg)) {
        return Err(SyncError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the sync's ssh does not go through the jump host",
        )));
    }
    let deadline = Instant::now() + timeout;
    // Declared before the child, so it is dropped after it: the master is
    // ended once the sync's own ssh is gone, on every path, panics included.
    let _master = jump
        .map(|leg| sign_in_to_jump_host(leg, terminal, deadline))
        .transpose()?;
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    set_terminal(&mut command, terminal);
    let child = command.spawn().map_err(|e| {
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
    let result = run_client(reader, writer, local, passphrase, stale);

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

/// Whether `cmd` takes `leg`'s hop as the `ProxyCommand` option of its `ssh`
/// (`-o ProxyCommand=<hop>` before the `--` that ends the options), as
/// `crate::ssh::sync_command` builds it for its `proxy`.
fn goes_through(cmd: &SshCommand, leg: &JumpLeg) -> bool {
    let option = format!("ProxyCommand={}", leg.hop);
    let end = cmd.args.iter().position(|arg| arg == "--");
    let options = &cmd.args[..end.unwrap_or(cmd.args.len())];
    options
        .windows(2)
        .any(|pair| pair[0] == "-o" && pair[1] == option)
}

/// Make `command` start its process where `terminal` says: in a session of its
/// own, with no controlling terminal, for [`SshTerminal::Detached`], else in
/// this process's. The one place that does so, for the sync's `ssh` and its
/// jump connection's master alike.
fn set_terminal(command: &mut Command, terminal: SshTerminal) {
    if terminal == SshTerminal::Detached {
        // SAFETY: the closure runs in the child between fork and exec, where
        // only async-signal-safe calls may be made. It makes one system call,
        // setsid (rustix calls it directly and allocates nothing), and turns a
        // failure's errno into an `io::Error`, which allocates nothing either.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
    }
}

/// Start `leg`'s master, the sync's connection to the partner's jump host, and
/// wait until it has signed in, exiting 0 with its background part serving the
/// shared connection, or failed, at the latest at `deadline`.
///
/// It is started from its argv like the sync's own `ssh`, in the place
/// `terminal` says ([`set_terminal`]), with `leg.master.env` added; its input
/// and output are null, and its stderr is drained, the end kept to say why it
/// failed. Only the part that signs in is waited for: the part that went to the
/// background lets go of the pipe, but a `ProxyCommand` that `~/.ssh/config`
/// gives the jump host keeps it open as long as the master runs.
///
/// Signed in, it is held by the [`EndsMaster`] returned, which ends it when
/// dropped, and its pid is asked for at once ([`learn_master`]): the caller is
/// to start the target's `ssh` right after, since a master nothing uses ends
/// by itself `crate::jump::PERSIST_SECS` after signing in. A master whose pid
/// cannot be learned is used all the same and ended with `ssh -O exit`.
///
/// Failed, nothing of it runs any more (one still signing in at `deadline` is
/// killed and reaped; that one and one ended by a signal are also asked to
/// end with `ssh -O exit`, as ssh's `-f` fork may have come just before), and
/// the error says why:
/// [`SyncError::JumpHostKeyUnknown`] when it exited saying "Host key
/// verification failed", else [`SyncError::JumpFailed`] with how it ended and
/// its last stderr lines, sanitized as for [`explain_exit`].
fn sign_in_to_jump_host(
    leg: &JumpLeg,
    terminal: SshTerminal,
    deadline: Instant,
) -> Result<EndsMaster<'_>, SyncError> {
    let failed = |detail: String| SyncError::JumpFailed {
        name: leg.name.clone(),
        detail,
    };
    let mut command = Command::new(&leg.master.program);
    command
        .args(&leg.master.args)
        .envs(leg.master.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    set_terminal(&mut command, terminal);
    let child = command
        .spawn()
        .map_err(|e| failed(format!("could not start {}: {e}", leg.master.program)))?;
    // Killed and reaped on every path from here, panics included.
    let mut child = Reaped(child);
    let stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| failed("its stderr could not be read".to_string()))?;
    let (tail, drain) =
        drain_stderr(stderr).map_err(|e| failed(format!("its stderr could not be read: {e}")))?;
    let ended = child.wait_until(deadline);
    if let Ok(Some(status)) = &ended
        && status.success()
    {
        // The drain ends by itself when the pipe closes.
        drop(drain);
        let mut master = EndsMaster {
            pidfd: None,
            exit: &leg.exit,
        };
        master.pidfd = learn_master(&leg.check);
        return Ok(master);
    }
    join_briefly(vec![drain]);
    let tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
    let stderr = String::from_utf8_lossy(&tail);
    Err(match ended {
        Ok(Some(_)) if stderr.contains(HOST_KEY_FAILED) => SyncError::JumpHostKeyUnknown {
            name: leg.name.clone(),
        },
        Ok(Some(status)) => {
            if status.code().is_none() {
                // Ended by a signal: it may have forked its background part
                // before, which nothing else would end.
                end_through_socket(&leg.exit);
            }
            failed(with_stderr(how_ssh_ended(status), &stderr))
        }
        Ok(None) => {
            // Killed at the deadline, which can land just after ssh's `-f`
            // fork: ended as a master would be.
            end_through_socket(&leg.exit);
            failed(with_stderr(
                "ssh did not sign in in time".to_string(),
                &stderr,
            ))
        }
        Err(e) => {
            // How it ended is not known, so it may have signed in: ended as a
            // master would be.
            end_through_socket(&leg.exit);
            failed(format!("its end could not be read: {e}"))
        }
    })
}

/// The master of a sync's jump connection, signed in: ended when this is
/// dropped, however the sync ends (success, an error, a timeout, a panic),
/// through its pidfd ([`end_master`]) when [`learn_master`] gave one, else, or
/// should the pidfd find it gone already, with `ssh -O exit` through its socket
/// ([`run_control`], within [`CONTROL_LIMIT`]). Either way it closes its
/// channels and removes its socket at once.
struct EndsMaster<'a> {
    pidfd: Option<OwnedFd>,
    /// `ssh -O exit` for it.
    exit: &'a SshCommand,
}

impl Drop for EndsMaster<'_> {
    fn drop(&mut self) {
        let ended = self
            .pidfd
            .as_ref()
            .is_some_and(|pidfd| end_master(pidfd).is_ok());
        if !ended {
            end_through_socket(self.exit);
        }
    }
}

/// Ask a master to end with `exit` (`ssh -O exit` through its socket), best
/// effort, within [`CONTROL_LIMIT`]. With no master listening it fails at once.
fn end_through_socket(exit: &SshCommand) {
    let _ = run_control(exit, CONTROL_LIMIT);
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
        Some(255) if stderr.contains(HOST_KEY_FAILED) => SyncError::HostKeyUnknown,
        _ => SyncError::Remote(with_stderr(how_ssh_ended(status), stderr)),
    }
}

/// What ssh writes on stderr when it cannot check a host's key: unknown (with
/// `StrictHostKeyChecking=yes` and nobody to ask) or changed.
const HOST_KEY_FAILED: &str = "Host key verification failed";

/// How ssh ended, in words: its exit status, or a signal.
fn how_ssh_ended(status: ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("ssh exited with status {c}"),
        None => "ssh was terminated by a signal".to_string(),
    }
}

/// `what` happened to ssh, followed by the last lines it wrote on stderr
/// ([`stderr_summary`]), if there are any.
fn with_stderr(what: String, stderr: &str) -> String {
    let text = stderr_summary(stderr);
    if text.is_empty() {
        what
    } else {
        format!("{what}: {text}")
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
    use crate::jump::{JumpLeg, JumpSocket};
    use crate::profile::Profile;
    use crate::secrets::{MemorySecretStore, SecretError, password_key};
    use crate::ssh::{SERVER_PROGRAM, sync_command};
    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;
    use tempfile::TempDir;

    const PASS: &str = "correct horse battery staple";

    // ----- fixtures -----

    /// A memory secret store that counts writes (sets and deletes), records
    /// the keys it was asked for, and can be told to fail one of the writes,
    /// shared with the test.
    #[derive(Default)]
    struct Counted {
        inner: MemorySecretStore,
        writes: AtomicUsize,
        deletes: AtomicUsize,
        /// Every key `get` was called with, in order.
        reads: Mutex<Vec<String>>,
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
        /// Whether the password of `id` was asked for (by the code under
        /// test: [`password`](Self::password) does not count).
        fn was_read(&self, id: &str) -> bool {
            let key = password_key(id);
            self.0.reads.lock().unwrap().contains(&key)
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
            self.0.reads.lock().unwrap().push(key.to_owned());
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

        let out = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap();
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

        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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

        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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

        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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
        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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
        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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

        let err = sync_over_ssh(
            &mut a.state,
            &sh("exit 127"),
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::ServerNotFound), "got {err:?}");

        let err = sync_over_ssh(
            &mut a.state,
            &sh(r#"echo "Host key verification failed." >&2; exit 255"#),
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::HostKeyUnknown), "got {err:?}");

        // Another failure: the last stderr lines, control characters removed.
        let err = sync_over_ssh(
            &mut a.state,
            &sh(r#"printf 'early noise\nssh: connect to host x: \033[31mrefused\a\n' >&2; exit 255"#),
            None,
            PASS, SshTerminal::Inherit, StaleSync::Refuse
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
            None,
            PASS, SshTerminal::Inherit, StaleSync::Refuse
        )
        .unwrap_err();
        let SyncError::Remote(msg) = &err else {
            panic!("expected Remote, got {err:?}");
        };
        assert!(msg.len() <= 400, "{} bytes", msg.len());

        // Exit 0 without speaking the protocol keeps the protocol error.
        let err = sync_over_ssh(
            &mut a.state,
            &sh("exit 0"),
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::Io(_)), "got {err:?}");

        // A program that does not exist.
        let missing = SshCommand {
            program: "/nonexistent/moniker-test-ssh".to_string(),
            args: vec![],
            env: vec![],
        };
        let err = sync_over_ssh(
            &mut a.state,
            &missing,
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::Io(_)), "got {err:?}");

        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    // ----- where ssh may ask -----

    /// `(pid, parent, session)` of the `ssh` a sync starts, as the child itself
    /// reads them from `/proc`, for a sync run with `terminal`. The child then
    /// exits 127, like a remote without moniker-server.
    fn ids_of_the_sync_ssh(terminal: SshTerminal) -> (String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("ids");
        let cmd = SshCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"out=$1; set -- $(cat /proc/$$/stat); printf '%s %s %s' "$1" "$4" "$6" > "$out"; exit 127"#
                    .to_string(),
                "sh".to_string(),
                out.to_str().unwrap().to_string(),
            ],
            env: vec![],
        };
        let mut a = side(&[]);
        let err =
            sync_over_ssh(&mut a.state, &cmd, None, PASS, terminal, StaleSync::Refuse).unwrap_err();
        assert!(matches!(err, SyncError::ServerNotFound), "got {err:?}");
        let ids = fs::read_to_string(&out).unwrap();
        let mut ids = ids.split(' ').map(str::to_owned);
        (
            ids.next().unwrap(),
            ids.next().unwrap(),
            ids.next().unwrap(),
        )
    }

    /// The session of this test process, from `/proc/self/stat`.
    fn own_session() -> String {
        let stat = fs::read_to_string("/proc/self/stat").unwrap();
        let after_name = stat.rsplit_once(')').unwrap().1;
        after_name.split_whitespace().nth(3).unwrap().to_owned()
    }

    #[test]
    fn a_detached_sync_runs_ssh_in_a_session_of_its_own() {
        let (pid, parent, session) = ids_of_the_sync_ssh(SshTerminal::Detached);
        // A session leader with no controlling terminal: a prompt ssh would
        // put on /dev/tty fails at once instead of showing up in the terminal
        // this process was started from.
        assert_eq!(session, pid, "the session leader");
        assert_ne!(session, own_session());
        // Still this process's own child, which the askpass helper checks.
        assert_eq!(parent, std::process::id().to_string());
    }

    #[test]
    fn an_inheriting_sync_keeps_ssh_in_this_session() {
        let (pid, parent, session) = ids_of_the_sync_ssh(SshTerminal::Inherit);
        assert_eq!(session, own_session());
        assert_ne!(session, pid);
        assert_eq!(parent, std::process::id().to_string());
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
        let err = sync_over_ssh(
            &mut a.state,
            &cmd,
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
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
        let err = sync_over_ssh(
            &mut stub,
            &cmd,
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
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
        let out = sync_over_ssh(
            &mut a.state,
            &cmd,
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap();
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
        let err = run_client(&cs, &cs, &mut stub, PASS, StaleSync::Refuse).unwrap_err();
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
        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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
        let c = run_client(&cs, &cs, client, PASS, StaleSync::Refuse);
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
        let err = run_client(&cs, &cs, &mut a.state, "", StaleSync::Refuse).unwrap_err();
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
        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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
        let err = sync_over_ssh(
            &mut a.state,
            &cmd,
            None,
            "",
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
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
        let err = sync_over_ssh_with_timeout(
            &mut a.state,
            &cmd,
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
            Duration::from_secs(1),
        )
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
        let err = run_client(&cs, &cs, &mut stub, PASS, StaleSync::Refuse).unwrap_err();
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
        assert!(matches!(error_from_payload(b"\x04x"), SyncError::PeerStale));
        assert!(matches!(
            error_from_payload(b"\x05x"),
            SyncError::PeerSyncOff
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
        let err = run_client(&cs, &cs, &mut a.state, PASS, StaleSync::Refuse).unwrap_err();
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

    // ----- forgetting deletions, and machines that have been away (issue #8) -----

    /// A clock the tests move by hand, shared by the machines of a scenario.
    #[derive(Clone)]
    struct Clock(Arc<AtomicI64>);

    impl Clock {
        fn starting_now() -> Clock {
            Clock(Arc::new(AtomicI64::new(now_ms())))
        }
        fn now(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
        fn set(&self, ms: i64) {
            self.0.store(ms, Ordering::SeqCst);
        }
    }

    /// A machine of these scenarios: its files, its keyring and its sync
    /// state, on a shared clock.
    struct Machine {
        _dir: TempDir,
        path: PathBuf,
        secrets: Shared,
        state: Option<LocalState>,
        clock: Clock,
    }

    impl Machine {
        fn new(clock: &Clock, profiles: &[(Profile, Option<&str>)]) -> Machine {
            let side = side(profiles);
            let store = ProfileStore::open(&side.path).unwrap();
            let theirs = clock.clone();
            let state = LocalState::with_clock(
                store,
                Box::new(side.secrets.clone()),
                Box::new(move || theirs.now()),
            );
            Machine {
                _dir: side._dir,
                path: side.path,
                secrets: side.secrets,
                state: Some(state),
                clock: clock.clone(),
            }
        }

        /// The ids in its profile file, each with whether it is a tombstone.
        fn on_disk(&self) -> Vec<(String, bool)> {
            ProfileStore::open(&self.path)
                .unwrap()
                .all()
                .into_iter()
                .map(|p| (p.id, p.deleted))
                .collect()
        }

        /// Delete the profile `id` as the app does: its password, then the
        /// profile (a tombstone).
        fn delete(&self, id: &str) {
            self.secrets.0.inner.delete(&password_key(id)).unwrap();
            ProfileStore::open(&self.path).unwrap().delete(id).unwrap();
        }

        fn last_sync(&self) -> Option<i64> {
            crate::sync_state::last_sync(&self.path)
        }

        /// Turn sync off as the app does, through a store of its own: the
        /// machine's `LocalState` was opened before and has to notice.
        fn sync_off(&self) {
            ProfileStore::open(&self.path)
                .unwrap()
                .set_sync(false)
                .unwrap();
        }

        /// The profile `id` in its profile file, tombstone or not.
        fn profile(&self, id: &str) -> Option<Profile> {
            ProfileStore::open(&self.path)
                .unwrap()
                .all()
                .into_iter()
                .find(|p| p.id == id)
        }

        /// Edit the profile `id` as the app does, through a store of its
        /// own, stamped with the scenario's clock but never below one past
        /// the stamp it replaces (as `store::next_stamp` does).
        fn edit(&self, id: &str, change: impl FnOnce(&mut Profile)) {
            let now = self.clock.now();
            ProfileStore::open(&self.path)
                .unwrap()
                .update(|all| {
                    let mut all = all.to_vec();
                    let p = all.iter_mut().find(|p| p.id == id).unwrap();
                    change(p);
                    p.updated_at = now.max(p.updated_at.saturating_add(1));
                    Ok((all, ()))
                })
                .unwrap();
        }

        /// Keep the profile `id` on this machine only (`on`), or sync it
        /// again: an edit like any other.
        fn keep_here(&self, id: &str, on: bool) {
            self.edit(id, |p| p.local_only = on);
        }

        /// What it would send in a sync now.
        fn export(&self) -> Vec<BundleEntry> {
            self.state.as_ref().unwrap().export().unwrap()
        }
    }

    /// Sync `client` with `server`: what each side's sync returned.
    fn sync_machines(
        client: &mut Machine,
        server: &mut Machine,
        stale: StaleSync,
    ) -> (
        Result<SyncOutcome, SyncError>,
        Result<SyncOutcome, SyncError>,
    ) {
        let (cs, ss) = pair();
        let served = spawn_server(ss, server.state.take().unwrap(), PASS);
        let out = run_client(&cs, &cs, client.state.as_mut().unwrap(), PASS, stale);
        drop(cs);
        let (served, back) = served.join().unwrap();
        server.state = Some(back);
        (out, served)
    }

    /// Sync and expect both sides to succeed.
    fn synced(client: &mut Machine, server: &mut Machine) {
        let (out, served) = sync_machines(client, server, StaleSync::Refuse);
        out.unwrap();
        served.unwrap();
    }

    #[test]
    fn a_tombstone_older_than_the_cutoff_is_not_written() {
        let x = prof_at("x", 10);
        let t = x.tombstone(100);
        let (profiles, (outcome, ops)) = plan_apply(vec![entry(&t, None)], vec![], Some(101));
        assert!(profiles.is_empty());
        assert_eq!(outcome, SyncOutcome::default());
        assert!(ops.is_empty());
        for cutoff in [Some(100), None] {
            let (profiles, _) = plan_apply(vec![entry(&t, None)], vec![], cutoff);
            assert_eq!(profiles, vec![t.clone()], "kept with the cutoff {cutoff:?}");
        }
    }

    #[test]
    fn live_profiles_are_never_pruned() {
        let x = prof_at("x", 10);
        let (profiles, _) = plan_apply(vec![entry(&x, Some("pw"))], vec![], Some(i64::MAX));
        assert_eq!(profiles, vec![x]);
    }

    #[test]
    fn an_old_tombstone_from_a_peer_deletes_the_live_copy_and_is_kept_one_sync_more() {
        let x = prof_at("x", 10);
        let t = x.tombstone(100);
        let (profiles, (outcome, ops)) = plan_apply(
            vec![entry(&x, Some("pw"))],
            vec![entry(&t, None)],
            Some(101),
        );
        assert_eq!(profiles, vec![t.clone()], "kept, to be handed on");
        assert_eq!(outcome.removed, 1);
        assert!(
            matches!(ops.as_slice(), [SecretOp::Delete { id }] if *id == x.id),
            "its password is removed"
        );
        // The next sync forgets it.
        let (profiles, _) = plan_apply(vec![entry(&t, None)], vec![], Some(101));
        assert!(profiles.is_empty());
        // For a profile this machine never had: nothing at all.
        let (profiles, (outcome, ops)) = plan_apply(vec![], vec![entry(&t, None)], Some(101));
        assert!(profiles.is_empty());
        assert_eq!(outcome, SyncOutcome::default());
        assert!(ops.is_empty());
    }

    #[test]
    fn a_deletion_is_forgotten_a_year_after_it_was_made_on_every_machine() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[]);
        synced(&mut a, &mut b);
        assert_eq!(a.last_sync(), Some(start), "recorded on the client");
        assert_eq!(b.last_sync(), Some(start), "recorded on the server");

        a.delete(&x.id);
        clock.set(start + DAY_MS);
        synced(&mut a, &mut b);
        let tombstone = vec![(x.id.clone(), true)];
        assert_eq!(b.on_disk(), tombstone);
        assert_eq!(b.secrets.password(&x.id), None);

        // Within the year it is kept, by both.
        for day in [150, 300] {
            clock.set(start + day * DAY_MS);
            synced(&mut a, &mut b);
            assert_eq!(a.on_disk(), tombstone, "day {day}");
            assert_eq!(b.on_disk(), tombstone, "day {day}");
        }
        // A year after the deletion, both forget it, and it does not come back.
        for day in [366, 367] {
            clock.set(start + day * DAY_MS);
            synced(&mut a, &mut b);
            assert!(a.on_disk().is_empty(), "day {day}");
            assert!(b.on_disk().is_empty(), "day {day}");
        }
    }

    #[test]
    fn a_deletion_is_kept_however_old_until_a_sync_has_carried_it_away() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[]);
        crate::sync_state::record_sync(&a.path, start - 10 * DAY_MS).unwrap();
        a.delete(&x.id);
        // A year and more without a sync: still there, and it reaches `b`.
        clock.set(start + 400 * DAY_MS);
        let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Allow);
        out.unwrap();
        served.unwrap();
        let tombstone = vec![(x.id.clone(), true)];
        assert_eq!(a.on_disk(), tombstone);
        assert_eq!(b.on_disk(), tombstone);
        // Once it has left, it is forgotten.
        clock.set(start + 401 * DAY_MS);
        synced(&mut a, &mut b);
        assert!(a.on_disk().is_empty());
        assert!(b.on_disk().is_empty());
    }

    #[test]
    fn along_a_chain_a_deletion_reaches_everyone_and_is_forgotten_without_bouncing() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[]);
        let mut c = Machine::new(&clock, &[]);
        synced(&mut a, &mut b);
        synced(&mut c, &mut b);
        assert_eq!(c.secrets.password(&x.id).as_deref(), Some("pw"));

        a.delete(&x.id);
        clock.set(start + DAY_MS);
        synced(&mut a, &mut b);
        clock.set(start + 2 * DAY_MS);
        synced(&mut c, &mut b);
        let tombstone = vec![(x.id.clone(), true)];
        for m in [&a, &b, &c] {
            assert_eq!(m.on_disk(), tombstone);
            assert_eq!(m.secrets.password(&x.id), None);
        }
        for day in [100, 200, 300] {
            clock.set(start + day * DAY_MS);
            synced(&mut a, &mut b);
            synced(&mut c, &mut b);
        }
        // `a` and `b` forget it first; `c` still has it and hands it to `b`,
        // which does not keep it; then `c` forgets it too.
        clock.set(start + 366 * DAY_MS);
        synced(&mut a, &mut b);
        assert!(a.on_disk().is_empty());
        assert!(b.on_disk().is_empty());
        assert_eq!(c.on_disk(), tombstone);
        clock.set(start + 367 * DAY_MS);
        synced(&mut c, &mut b);
        for m in [&a, &b, &c] {
            assert!(m.on_disk().is_empty());
        }
        clock.set(start + 368 * DAY_MS);
        synced(&mut a, &mut b);
        synced(&mut c, &mut b);
        for m in [&a, &b, &c] {
            assert!(m.on_disk().is_empty(), "nothing comes back");
        }
    }

    #[test]
    fn a_late_deletion_still_travels_on_from_a_machine_past_the_year() {
        // `a` deletes and then syncs only elsewhere for a year; `m` is a hub
        // that `c` syncs with often. When the deletion reaches `m` at last,
        // `m` hands it on to `c` instead of forgetting it at once.
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut m = Machine::new(&clock, &[]);
        let mut c = Machine::new(&clock, &[]);
        synced(&mut a, &mut m);
        synced(&mut c, &mut m);
        a.delete(&x.id);
        for day in [100, 200, 300] {
            clock.set(start + day * DAY_MS);
            synced(&mut c, &mut m);
            crate::sync_state::record_sync(&a.path, clock.now()).unwrap();
        }
        clock.set(start + 366 * DAY_MS);
        synced(&mut a, &mut m);
        assert_eq!(m.secrets.password(&x.id), None, "deleted on the hub");
        clock.set(start + 367 * DAY_MS);
        synced(&mut c, &mut m);
        assert_eq!(c.secrets.password(&x.id), None, "and on the far side");
        assert!(c.on_disk().iter().all(|(_, deleted)| *deleted));
        clock.set(start + 368 * DAY_MS);
        synced(&mut c, &mut m);
        synced(&mut a, &mut m);
        for machine in [&a, &m, &c] {
            assert!(machine.on_disk().is_empty(), "then forgotten everywhere");
        }
    }

    #[test]
    fn a_last_sync_recorded_ahead_of_the_clock_prunes_nothing() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), None)]);
        let mut b = Machine::new(&clock, &[]);
        a.delete(&x.id);
        clock.set(start + 400 * DAY_MS);
        // Recorded while the clock ran a year ahead.
        crate::sync_state::record_sync(&a.path, start + 800 * DAY_MS).unwrap();
        synced(&mut a, &mut b);
        assert_eq!(
            a.on_disk(),
            vec![(x.id.clone(), true)],
            "kept: not known to have left"
        );
        assert_eq!(
            a.last_sync(),
            Some(start + 400 * DAY_MS),
            "the clock's time replaces it"
        );
    }

    #[test]
    fn a_machine_that_has_been_away_asks_before_syncing() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut d = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[]);
        synced(&mut d, &mut b);
        b.delete(&x.id);
        clock.set(start + 200 * DAY_MS);
        // `b` has synced since, with another machine; `d` has not.
        crate::sync_state::record_sync(&b.path, start + 199 * DAY_MS).unwrap();
        let (d_file, b_file) = (file_of(&d.path), file_of(&b.path));

        // `d` as the client: it stops before saying anything. (The timeout
        // makes a client that waits for an answer fail instead of hanging.)
        let (cs, ss) = pair();
        cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let listener = thread::spawn(move || {
            let mut heard = Vec::new();
            (&ss).read_to_end(&mut heard).unwrap();
            heard
        });
        let err =
            run_client(&cs, &cs, d.state.as_mut().unwrap(), PASS, StaleSync::Refuse).unwrap_err();
        drop(cs);
        assert!(matches!(err, SyncError::Stale { days: 200 }), "got {err:?}");
        assert!(listener.join().unwrap().is_empty(), "nothing was sent");

        // `d` as the server: it answers the HELLO with error 4 and goes no
        // further.
        let (out, served) = sync_machines(&mut b, &mut d, StaleSync::Refuse);
        assert!(matches!(out, Err(SyncError::PeerStale)), "got {out:?}");
        assert!(
            matches!(served, Err(SyncError::Stale { days: 200 })),
            "got {served:?}"
        );
        assert_eq!(file_of(&d.path), d_file);
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(d.last_sync(), Some(start), "a refused sync is not a sync");

        // Allowed, it syncs (the deletion is recent, so it wins), and `d` is
        // no longer away.
        let (out, served) = sync_machines(&mut b, &mut d, StaleSync::Allow);
        out.unwrap();
        served.unwrap();
        assert_eq!(d.on_disk(), vec![(x.id.clone(), true)]);
        assert_eq!(d.last_sync(), Some(start + 200 * DAY_MS));
        synced(&mut d, &mut b);
    }

    #[test]
    fn a_machine_that_never_synced_is_not_away() {
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[(prof("one"), None)]);
        let mut b = Machine::new(&clock, &[]);
        clock.set(clock.now() + 1000 * DAY_MS);
        synced(&mut a, &mut b);
    }

    #[test]
    fn a_failed_sync_is_not_recorded() {
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[(prof("one"), None)]);
        let b = side(&[]);
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state, "another sync key entirely");
        let err = run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse);
        drop(cs);
        let _ = server.join().unwrap();
        assert!(matches!(err, Err(SyncError::WrongSyncKey)), "got {err:?}");
        assert_eq!(a.last_sync(), None);
        assert_eq!(crate::sync_state::last_sync(&b.path), None);
    }

    #[test]
    fn a_one_byte_hello_from_an_older_client_still_works() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let mut b = Machine::new(&clock, &[]);
        // Fresh: the HELLO is answered as before.
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state.take().unwrap(), PASS);
        write_frame(&mut &cs, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
        assert_eq!(
            read_frame(&mut &cs).unwrap(),
            (KIND_HELLO, vec![PROTOCOL_VERSION])
        );
        drop(cs);
        b.state = Some(server.join().unwrap().1);

        // Away: error 4, with a text an older client shows as it is.
        crate::sync_state::record_sync(&b.path, start - 200 * DAY_MS).unwrap();
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state.take().unwrap(), PASS);
        write_frame(&mut &cs, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
        let (kind, payload) = read_frame(&mut &cs).unwrap();
        drop(cs);
        let _ = server.join().unwrap();
        assert_eq!(kind, KIND_ERROR);
        assert_eq!(payload[0], CODE_STALE);
        let text = String::from_utf8_lossy(&payload[1..]).into_owned();
        assert!(
            text.starts_with("the other machine has not synced for 200 days"),
            "{text}"
        );
        assert!(matches!(error_from_payload(&payload), SyncError::PeerStale));
    }

    #[test]
    fn sync_over_ssh_asks_before_connecting() {
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
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[]);
        crate::sync_state::record_sync(&a.path, clock.now() - 181 * DAY_MS).unwrap();
        let err = sync_over_ssh(
            a.state.as_mut().unwrap(),
            &cmd,
            None,
            PASS,
            SshTerminal::Inherit,
            StaleSync::Refuse,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::Stale { days: 181 }), "{err:?}");
        assert!(!marker.exists(), "ssh must not be started");
    }

    // ----- sync turned off (issue #10) -----

    #[test]
    fn a_client_with_sync_off_sends_nothing() {
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[(prof("one"), Some("pw"))]);
        let mut b = Machine::new(&clock, &[(prof("two"), Some("pw"))]);
        a.sync_off();
        let (a_file, b_file) = (file_of(&a.path), file_of(&b.path));

        // The server's read times out instead of waiting for ever, were the
        // client to leave the connection open.
        let (cs, ss) = pair();
        ss.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let served = spawn_server(ss, b.state.take().unwrap(), PASS);
        let out = run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse);
        drop(cs);
        let (served, back) = served.join().unwrap();
        b.state = Some(back);

        assert!(matches!(out, Err(SyncError::SyncOff)), "got {out:?}");
        assert!(
            served.is_err(),
            "no HELLO came, so the server has no sync: {served:?}"
        );
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!((a.last_sync(), b.last_sync()), (None, None));
    }

    #[test]
    fn a_server_with_sync_off_answers_code_5() {
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[(prof("one"), Some("pw"))]);
        let mut b = Machine::new(&clock, &[(prof("two"), Some("pw"))]);
        b.sync_off();
        let (a_file, b_file) = (file_of(&a.path), file_of(&b.path));

        let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);

        assert!(matches!(out, Err(SyncError::PeerSyncOff)), "got {out:?}");
        assert!(matches!(served, Err(SyncError::SyncOff)), "got {served:?}");
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!((a.last_sync(), b.last_sync()), (None, None));
    }

    #[test]
    fn sync_off_is_said_before_anything_else_is_checked() {
        // With no sync key, and on a machine that has been away: the switch
        // is the first thing either role looks at.
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[]);
        crate::sync_state::record_sync(&a.path, clock.now() - 300 * DAY_MS).unwrap();
        a.sync_off();

        let (cs, ss) = pair();
        let err = run_client(&cs, &cs, a.state.as_mut().unwrap(), "", StaleSync::Refuse);
        assert!(matches!(err, Err(SyncError::SyncOff)), "got {err:?}");
        drop((cs, ss));

        let (cs, ss) = pair();
        write_frame(&mut &cs, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
        let err = serve(&ss, &ss, a.state.as_mut().unwrap(), "");
        assert!(matches!(err, Err(SyncError::SyncOff)), "got {err:?}");
        let (kind, payload) = read_frame(&mut &cs).unwrap();
        assert_eq!((kind, payload[0]), (KIND_ERROR, CODE_SYNC_OFF));
    }

    #[test]
    fn sync_off_is_read_from_the_file_when_asked_not_from_a_snapshot() {
        let clock = Clock::starting_now();
        let a = Machine::new(&clock, &[]);
        let state = a.state.as_ref().unwrap();
        assert!(state.sync_on().is_ok());
        // Another process (the app, `moniker-server`) turns it off, and on.
        a.sync_off();
        assert!(matches!(state.sync_on(), Err(SyncError::SyncOff)));
        ProfileStore::open(&a.path).unwrap().set_sync(true).unwrap();
        assert!(state.sync_on().is_ok());
    }

    #[test]
    fn a_switch_not_understood_or_a_damaged_file_stops_a_sync() {
        let clock = Clock::starting_now();
        let a = Machine::new(&clock, &[]);
        let state = a.state.as_ref().unwrap();
        // Not understood: off, as the store reads it.
        fs::write(&a.path, r#"{"version":2,"profiles":[]}"#).unwrap();
        assert!(matches!(state.sync_on(), Err(SyncError::SyncOff)));
        // Damaged: the store's error, as for every other read of the file.
        fs::write(&a.path, "{").unwrap();
        assert!(matches!(state.sync_on(), Err(SyncError::Store(_))));
    }

    #[test]
    fn refuse_answers_the_hello_with_code_5() {
        let (cs, ss) = pair();
        write_frame(&mut &cs, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
        let err = refuse(&ss, &ss, SyncError::SyncOff).unwrap_err();
        assert!(matches!(err, SyncError::SyncOff), "got {err:?}");
        drop(ss);

        let (kind, payload) = read_frame(&mut &cs).unwrap();
        assert_eq!(kind, KIND_ERROR);
        assert_eq!(payload[0], CODE_SYNC_OFF);
        let text = String::from_utf8_lossy(&payload[1..]).into_owned();
        assert!(text.contains("turned off on the other machine"), "{text}");
        assert!(matches!(
            error_from_payload(&payload),
            SyncError::PeerSyncOff
        ));
        // Nothing else was said.
        let mut rest = Vec::new();
        (&cs).read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn refuse_says_nothing_to_a_connection_that_did_not_speak_first() {
        let (cs, ss) = pair();
        drop(cs);
        let err = refuse(&ss, &ss, SyncError::SyncOff).unwrap_err();
        assert!(matches!(err, SyncError::Io(_)), "got {err:?}");

        let (cs, ss) = pair();
        write_frame(&mut &cs, KIND_BUNDLE, b"zzz").unwrap();
        let err = refuse(&ss, &ss, SyncError::SyncOff).unwrap_err();
        assert!(matches!(err, SyncError::Protocol(_)), "got {err:?}");
        drop(ss);
        let mut said = Vec::new();
        (&cs).read_to_end(&mut said).unwrap();
        assert!(said.is_empty());
    }

    #[test]
    fn an_old_one_byte_hello_is_refused_the_same() {
        let clock = Clock::starting_now();
        let mut b = Machine::new(&clock, &[]);
        b.sync_off();
        let b_file = file_of(&b.path);
        for hello in [
            vec![PROTOCOL_VERSION],
            client_hello(PROTOCOL_VERSION, StaleSync::Allow),
        ] {
            let (cs, ss) = pair();
            let served = spawn_server(ss, b.state.take().unwrap(), PASS);
            write_frame(&mut &cs, KIND_HELLO, &hello).unwrap();
            let (kind, payload) = read_frame(&mut &cs).unwrap();
            drop(cs);
            let (served, back) = served.join().unwrap();
            b.state = Some(back);

            assert_eq!(kind, KIND_ERROR);
            assert_eq!(payload[0], CODE_SYNC_OFF);
            assert!(matches!(served, Err(SyncError::SyncOff)), "got {served:?}");
            // Said from the peer's side: a Moniker that knows no code 5
            // shows the text as it is.
            let text = String::from_utf8_lossy(&payload[1..]).into_owned();
            assert_eq!(text, "sync is turned off on the other machine");
            let unknown_code = [&[9u8][..], &payload[1..]].concat();
            let SyncError::Remote(shown) = error_from_payload(&unknown_code) else {
                panic!("expected Remote");
            };
            assert_eq!(shown, text);
        }
        assert_eq!(file_of(&b.path), b_file);
    }

    #[test]
    fn sync_over_ssh_with_sync_off_starts_nothing() {
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
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[]);
        a.sync_off();
        for terminal in [SshTerminal::Inherit, SshTerminal::Detached] {
            let err = sync_over_ssh(
                a.state.as_mut().unwrap(),
                &cmd,
                None,
                PASS,
                terminal,
                StaleSync::Allow,
            )
            .unwrap_err();
            assert!(matches!(err, SyncError::SyncOff), "{err:?}");
        }
        assert!(!marker.exists(), "ssh must not be started");
    }

    // ----- profiles kept on this machine (issue #10) -----

    /// A profile with every field a bundle could carry filled in, with text
    /// found nowhere else in these scenarios, so a test can look for it.
    fn full(name: &str) -> Profile {
        let mut p = Profile::new(name, "kept.example.org", "carol");
        p.port = 2222;
        p.key_file = Some(PathBuf::from("/home/carol/.ssh/kept_key"));
        p.sequences = vec![crate::profile::StartupSequence {
            name: "kept update".to_string(),
            commands: vec!["kept-command --now".to_string()],
        }];
        p.startup_sequence = Some(0);
        p.transcript = true;
        p
    }

    /// Nothing of `p` but its id is in `sent`: not its name, host, user name,
    /// port, key file, sequences, default sequence or `password`, nor the flag
    /// that keeps it.
    fn assert_nothing_of(sent: &[BundleEntry], p: &Profile, password: &str) {
        let text = serde_json::to_string(sent).unwrap();
        let port = format!("\"port\":{}", p.port);
        let mut private = vec![
            p.name.as_str(),
            p.host.as_str(),
            p.username.as_str(),
            port.as_str(),
            password,
            "local_only",
            "startup_sequence",
            "\"transcript\":true",
        ];
        let key_file = p.key_file.as_ref().map(|k| k.to_str().unwrap().to_owned());
        private.extend(key_file.as_deref());
        for seq in &p.sequences {
            private.push(&seq.name);
            private.extend(seq.commands.iter().map(String::as_str));
        }
        for s in private {
            assert!(!text.contains(s), "{s:?} would leave the machine");
        }
    }

    /// `kept` (as it was kept) is still in `m`'s profile file and keyring
    /// exactly as it was, password included, and its password was never read.
    fn assert_kept(m: &Machine, kept: &Profile, password: &str) {
        assert_eq!(m.profile(&kept.id).as_ref(), Some(kept));
        assert!(kept.local_only && !kept.deleted);
        assert_eq!(m.secrets.password(&kept.id).as_deref(), Some(password));
        assert!(!m.secrets.was_read(&kept.id), "its password was not read");
    }

    #[test]
    fn a_profile_kept_here_leaves_as_a_bare_tombstone() {
        let clock = Clock::starting_now();
        let x = full("kept name");
        let y = prof("synced");
        let a = Machine::new(
            &clock,
            &[
                (x.clone(), Some("kept-password")),
                (y.clone(), Some("pw-y")),
            ],
        );
        a.keep_here(&x.id, true);
        let kept = a.profile(&x.id).unwrap();

        let sent = a.export();
        let gone = sent.iter().find(|e| e.profile.id == x.id).unwrap();
        assert_eq!(gone.profile, x.tombstone(kept.updated_at));
        assert!(gone.profile.deleted);
        assert!(gone.profile.name.is_empty() && gone.profile.host.is_empty());
        assert!(gone.profile.username.is_empty() && gone.profile.sequences.is_empty());
        assert_eq!(gone.password, None);
        assert_nothing_of(&sent, &x, "kept-password");
        assert!(!a.secrets.was_read(&x.id), "its password is not even read");
        // A profile that syncs leaves as before.
        let other = sent.iter().find(|e| e.profile.id == y.id).unwrap();
        assert_eq!(other.profile, y);
        assert_eq!(other.password.as_deref(), Some("pw-y"));
    }

    #[test]
    fn an_edit_of_a_kept_profile_still_leaves_bare() {
        let clock = Clock::starting_now();
        let x = full("kept name");
        let a = Machine::new(&clock, &[(x.clone(), Some("kept-password"))]);
        a.keep_here(&x.id, true);
        let before = a.profile(&x.id).unwrap().updated_at;
        a.edit(&x.id, |p| {
            p.name = "renamed while kept".to_string();
            p.port = 2200;
        });
        let edited = a.profile(&x.id).unwrap();
        assert!(edited.updated_at > before && edited.local_only);

        let sent = a.export();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].profile,
            x.tombstone(edited.updated_at),
            "at the new stamp"
        );
        assert_eq!(sent[0].password, None);
        assert_nothing_of(&sent, &edited, "kept-password");
        assert_nothing_of(&sent, &x, "kept-password");
    }

    #[test]
    fn nothing_incoming_changes_a_kept_profile() {
        let kept = Profile {
            local_only: true,
            ..prof_at("mine", 10)
        };
        let other = Profile {
            name: "other".to_string(),
            local_only: false,
            updated_at: 20,
            ..kept.clone()
        };
        let at = |updated_at| Profile {
            updated_at,
            ..kept.clone()
        };
        let (profiles, (outcome, ops)) = plan_apply(
            vec![entry(&kept, Some("a"))],
            vec![entry(&other, Some("b"))],
            None,
        );
        assert_eq!(
            profiles,
            vec![at(20)],
            "only raised to the newer edit's stamp, where its tombstone wins"
        );
        assert!(ops.is_empty());
        assert_eq!(outcome, SyncOutcome::default());

        // Nor a deletion, newer or as old; nor an older edit. A peer that
        // pinned its stamp at `i64::MAX` gets a tombstone there, which still
        // wins the tie.
        let older = Profile {
            updated_at: 5,
            ..other.clone()
        };
        let pinned = Profile {
            updated_at: i64::MAX,
            ..other.clone()
        };
        let cases = [
            (kept.tombstone(20), 20),
            (kept.tombstone(10), 10),
            (older, 10),
            (pinned, i64::MAX),
            (kept.tombstone(i64::MAX), i64::MAX),
        ];
        for (incoming, stamp) in cases {
            let what = (incoming.deleted, incoming.updated_at);
            let password = (!incoming.deleted).then_some("b");
            let (profiles, (outcome, ops)) = plan_apply(
                vec![entry(&kept, Some("a"))],
                vec![entry(&incoming, password)],
                Some(i64::MAX),
            );
            assert_eq!(profiles, vec![at(stamp)], "against {what:?}");
            assert!(ops.is_empty(), "against {what:?}");
            assert_eq!(outcome, SyncOutcome::default(), "against {what:?}");
        }
    }

    #[test]
    fn the_other_machine_deletes_its_copy_and_password() {
        for kept_by_the_client in [true, false] {
            let role = if kept_by_the_client {
                "client"
            } else {
                "server"
            };
            let clock = Clock::starting_now();
            let x = full("kept name");
            let y = prof("synced");
            let both = [
                (x.clone(), Some("kept-password")),
                (y.clone(), Some("pw-y")),
            ];
            let mut a = Machine::new(&clock, &both);
            let mut b = Machine::new(&clock, &both);
            a.keep_here(&x.id, true);
            let kept = a.profile(&x.id).unwrap();
            let a_file = file_of(&a.path);

            for round in 1..=2 {
                let (kept_out, other_out) = if kept_by_the_client {
                    let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);
                    (out.unwrap(), served.unwrap())
                } else {
                    let (out, served) = sync_machines(&mut b, &mut a, StaleSync::Refuse);
                    (served.unwrap(), out.unwrap())
                };
                let removed = usize::from(round == 1);
                assert_eq!(kept_out, SyncOutcome::default(), "{role}, round {round}");
                assert_eq!(other_out.removed, removed, "{role}, round {round}");
                assert_eq!(
                    b.profile(&x.id),
                    Some(x.tombstone(kept.updated_at)),
                    "{role}, round {round}"
                );
                assert_eq!(b.secrets.password(&x.id), None, "{role}, round {round}");
                assert_eq!(b.profile(&y.id), Some(y.clone()));
                assert_eq!(b.secrets.password(&y.id).as_deref(), Some("pw-y"));
                assert_kept(&a, &kept, "kept-password");
                assert_eq!(file_of(&a.path), a_file, "{role}, round {round}");
                assert_eq!(a.secrets.writes(), 0, "{role}, round {round}");
            }
        }
    }

    #[test]
    fn the_partner_profile_itself_can_be_kept_here() {
        // `a` syncs with `b` through its profile for `b`, which `b` has a
        // copy of too, and keeps that profile to itself.
        let clock = Clock::starting_now();
        let partner = Profile::new("Machine B", "machine-b.example.org", "dana");
        let mut a = Machine::new(&clock, &[(partner.clone(), Some("pw-b"))]);
        let mut b = Machine::new(&clock, &[(partner.clone(), Some("pw-b"))]);
        a.keep_here(&partner.id, true);
        let kept = a.profile(&partner.id).unwrap();
        for _ in 0..2 {
            synced(&mut a, &mut b);
            assert_eq!(
                b.profile(&partner.id),
                Some(partner.tombstone(kept.updated_at))
            );
            assert_eq!(b.secrets.password(&partner.id), None);
            assert_kept(&a, &kept, "pw-b");
        }
        // The next sync still connects with it.
        let store = ProfileStore::open(&a.path).unwrap();
        let cmd = crate::ssh::sync_command(
            store.get(&partner.id).unwrap(),
            Path::new("/usr/bin/moniker-askpass"),
            true,
            "moniker-server",
            None,
        );
        assert!(cmd.args.iter().any(|a| a == "dana@machine-b.example.org"));
    }

    #[test]
    fn a_newer_edit_elsewhere_loses_on_the_second_sync() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        a.keep_here(&x.id, true);
        let kept = a.profile(&x.id).unwrap();
        // `b` has not heard yet and edits its copy, password included.
        clock.set(start + 60_000);
        b.edit(&x.id, |p| p.name = "edited on b".to_string());
        b.secrets.0.inner.set(&password_key(&x.id), "pw-b").unwrap();
        let edit = b.profile(&x.id).unwrap();
        assert!(edit.updated_at > kept.updated_at);

        // The first sync sees the newer edit: `b` keeps it, and `a` only
        // raises its stamp to the edit's, counting nothing.
        let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);
        assert_eq!(out.unwrap(), SyncOutcome::default());
        served.unwrap();
        assert_eq!(b.profile(&x.id), Some(edit.clone()));
        assert_eq!(b.secrets.password(&x.id).as_deref(), Some("pw-b"));
        let raised = Profile {
            updated_at: edit.updated_at,
            ..kept.clone()
        };
        assert_kept(&a, &raised, "pw");
        // The second deletes it there: a tombstone wins the tie.
        let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);
        assert_eq!(out.unwrap(), SyncOutcome::default());
        assert_eq!(served.unwrap().removed, 1);
        assert_eq!(b.profile(&x.id), Some(x.tombstone(raised.updated_at)));
        assert_eq!(b.secrets.password(&x.id), None);
        assert_kept(&a, &raised, "pw");
    }

    #[test]
    fn a_chain_carries_the_deletion() {
        let clock = Clock::starting_now();
        let x = prof("x");
        let all = [(x.clone(), Some("pw"))];
        let mut a = Machine::new(&clock, &all);
        let mut b = Machine::new(&clock, &all);
        let mut c = Machine::new(&clock, &all);
        a.keep_here(&x.id, true);
        let kept = a.profile(&x.id).unwrap();
        synced(&mut a, &mut b);
        synced(&mut b, &mut c);
        for m in [&b, &c] {
            assert_eq!(m.profile(&x.id), Some(x.tombstone(kept.updated_at)));
            assert_eq!(m.secrets.password(&x.id), None);
        }
        assert_kept(&a, &kept, "pw");
    }

    #[test]
    fn syncing_it_again_brings_it_back_everywhere() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let all = [(x.clone(), Some("pw"))];
        let mut a = Machine::new(&clock, &all);
        let mut b = Machine::new(&clock, &all);
        let mut c = Machine::new(&clock, &all);
        a.keep_here(&x.id, true);
        // A password changed while it is kept stays here too.
        a.secrets.0.inner.set(&password_key(&x.id), "pw-a").unwrap();
        synced(&mut a, &mut b);
        synced(&mut b, &mut c);
        for m in [&b, &c] {
            assert_eq!(m.profile(&x.id).map(|p| p.deleted), Some(true));
            assert_eq!(m.secrets.password(&x.id), None);
        }

        clock.set(start + DAY_MS);
        a.keep_here(&x.id, false);
        let back = a.profile(&x.id).unwrap();
        assert!(!back.local_only);
        for day in 1..=3 {
            clock.set(start + day * DAY_MS);
            synced(&mut a, &mut b);
            synced(&mut b, &mut c);
            for m in [&a, &b, &c] {
                assert_eq!(m.profile(&x.id), Some(back.clone()), "day {day}");
                assert_eq!(m.secrets.password(&x.id).as_deref(), Some("pw-a"));
            }
        }
    }

    #[test]
    fn syncing_it_again_wins_with_the_clock_set_back() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        a.keep_here(&x.id, true);
        synced(&mut a, &mut b);
        let deletion = b.profile(&x.id).unwrap();
        assert!(deletion.deleted);

        clock.set(start - DAY_MS);
        a.keep_here(&x.id, false);
        let back = a.profile(&x.id).unwrap();
        assert!(
            back.updated_at > deletion.updated_at,
            "above every tombstone sent"
        );
        let rounds = [
            (true, start - DAY_MS),
            (false, start),
            (true, start + DAY_MS),
        ];
        for (a_is_the_client, at) in rounds {
            clock.set(at);
            if a_is_the_client {
                synced(&mut a, &mut b);
            } else {
                synced(&mut b, &mut a);
            }
            for m in [&a, &b] {
                assert_eq!(m.profile(&x.id), Some(back.clone()));
                assert_eq!(m.secrets.password(&x.id).as_deref(), Some("pw"));
            }
        }
    }

    #[test]
    fn syncing_it_again_beats_a_deletion_made_elsewhere_too() {
        // `b` deletes its copy after `a` kept the profile, with a clock that
        // runs a day ahead. `a` has heard of that deletion, so syncing the
        // profile again from `a` wins over it, with `a`'s clock behind.
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        a.keep_here(&x.id, true);
        clock.set(start + DAY_MS);
        b.secrets.0.inner.delete(&password_key(&x.id)).unwrap();
        b.edit(&x.id, |p| p.deleted = true);
        let deletion = b.profile(&x.id).unwrap();
        assert!(deletion.deleted);
        synced(&mut a, &mut b);

        clock.set(start);
        a.keep_here(&x.id, false);
        let back = a.profile(&x.id).unwrap();
        assert!(back.updated_at > deletion.updated_at);
        for _ in 0..2 {
            synced(&mut a, &mut b);
            for m in [&a, &b] {
                assert_eq!(m.profile(&x.id), Some(back.clone()));
                assert_eq!(m.secrets.password(&x.id).as_deref(), Some("pw"));
            }
        }
    }

    #[test]
    fn two_keepers_keep_their_own() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw-a"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw-b"))]);
        a.keep_here(&x.id, true);
        clock.set(start + 60_000);
        b.edit(&x.id, |p| {
            p.local_only = true;
            p.name = "b's own".to_string();
        });
        for round in 1..=2 {
            let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);
            assert_eq!(out.unwrap(), SyncOutcome::default(), "round {round}");
            assert_eq!(served.unwrap(), SyncOutcome::default(), "round {round}");
            for (m, name, password) in [(&a, "x", "pw-a"), (&b, "b's own", "pw-b")] {
                let p = m.profile(&x.id).unwrap();
                assert!(p.local_only && !p.deleted, "round {round}");
                assert_eq!(p.name, name);
                assert_eq!(m.secrets.password(&x.id).as_deref(), Some(password));
                assert!(!m.secrets.was_read(&x.id));
                assert_eq!(m.secrets.writes(), 0);
            }
        }
    }

    #[test]
    fn two_keepers_settle_on_one_stamp_and_then_write_nothing() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw-a"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw-b"))]);
        a.keep_here(&x.id, true);
        clock.set(start + 60_000);
        b.keep_here(&x.id, true);
        let newest = b.profile(&x.id).unwrap().updated_at;

        // The first sync brings `a`'s stamp up to `b`'s, and no further.
        synced(&mut a, &mut b);
        for m in [&a, &b] {
            assert_eq!(m.profile(&x.id).unwrap().updated_at, newest);
        }
        // From then on, in either role, neither profile file is written.
        let files = (file_of(&a.path), file_of(&b.path));
        for round in 1..=4 {
            if round % 2 == 0 {
                synced(&mut a, &mut b);
            } else {
                synced(&mut b, &mut a);
            }
            let now = (file_of(&a.path), file_of(&b.path));
            assert!(now == files, "a profile file was written in round {round}");
        }
        for (m, password) in [(&a, "pw-a"), (&b, "pw-b")] {
            let p = m.profile(&x.id).unwrap();
            assert!(p.local_only && !p.deleted);
            assert_eq!(m.secrets.password(&x.id).as_deref(), Some(password));
        }
    }

    #[test]
    fn when_one_keeper_syncs_again_the_other_keeps_it_alone() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw-a"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw-b"))]);
        a.keep_here(&x.id, true);
        b.keep_here(&x.id, true);
        clock.set(start + 60_000);
        a.keep_here(&x.id, false);

        // `a`'s copy reaches `b`, which keeps its own and raises its stamp;
        // then `b`'s deletion reaches `a`.
        let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);
        assert_eq!(out.unwrap(), SyncOutcome::default());
        assert_eq!(served.unwrap(), SyncOutcome::default());
        assert!(!a.profile(&x.id).unwrap().deleted, "not yet");
        let (out, served) = sync_machines(&mut a, &mut b, StaleSync::Refuse);
        assert_eq!(out.unwrap().removed, 1, "counted as any deletion");
        assert_eq!(served.unwrap(), SyncOutcome::default());
        for _ in 0..2 {
            assert_eq!(a.profile(&x.id).map(|p| p.deleted), Some(true));
            assert_eq!(a.secrets.password(&x.id), None);
            let kept = b.profile(&x.id).unwrap();
            assert!(kept.local_only && !kept.deleted);
            assert_eq!(b.secrets.password(&x.id).as_deref(), Some("pw-b"));
            assert_eq!(b.secrets.writes(), 0, "nothing of `a`'s was stored");
            synced(&mut b, &mut a);
        }
    }

    #[test]
    fn a_peer_cannot_mark_a_profile_kept_here() {
        let marked = Profile {
            local_only: true,
            ..prof("marked by the peer")
        };
        let sealed = seal_bundle(PASS, &[entry(&marked, Some("pw"))]).unwrap();
        let opened = open_bundle(PASS, &sealed).unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(
            opened[0].profile,
            Profile {
                local_only: false,
                ..marked.clone()
            }
        );
        assert_eq!(opened[0].password.as_deref(), Some("pw"));

        // As a client: from a server that sends one.
        let clock = Clock::starting_now();
        let mut a = Machine::new(&clock, &[]);
        let peer = Stub {
            entries: vec![entry(&marked, Some("pw"))],
            ..Stub::default()
        };
        let (cs, ss) = pair();
        let served = spawn_server(ss, peer, PASS);
        run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse).unwrap();
        drop(cs);
        served.join().unwrap().0.unwrap();
        assert!(!a.profile(&marked.id).unwrap().local_only, "as a client");

        // As a server: from a client that sends one.
        let mut b = Machine::new(&clock, &[]);
        let (cs, ss) = pair();
        cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let served = spawn_server(ss, b.state.take().unwrap(), PASS);
        write_frame(&mut &cs, KIND_HELLO, &[PROTOCOL_VERSION]).unwrap();
        assert_eq!(read_frame(&mut &cs).unwrap().0, KIND_HELLO);
        write_frame(&mut &cs, KIND_BUNDLE, &sealed).unwrap();
        assert_eq!(read_frame(&mut &cs).unwrap().0, KIND_BUNDLE);
        drop(cs);
        let (served, back) = served.join().unwrap();
        served.unwrap();
        b.state = Some(back);
        assert!(!b.profile(&marked.id).unwrap().local_only, "as a server");
    }

    #[test]
    fn forgetting_on_the_peer_does_not_bounce() {
        // `b` forgets the deletion a year after it was made (issue #8); `a`
        // sends it again at every sync, and `b` does not take it back.
        let clock = Clock::starting_now();
        let start = clock.now();
        let x = prof("x");
        let mut a = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        let mut b = Machine::new(&clock, &[(x.clone(), Some("pw"))]);
        a.keep_here(&x.id, true);
        let kept = a.profile(&x.id).unwrap();
        let a_file = file_of(&a.path);
        let deletion = Some(x.tombstone(kept.updated_at));
        for day in [0, 100, 200, 300, 365, 366, 367, 500, 600, 730] {
            clock.set(start + day * DAY_MS);
            if day % 2 == 0 {
                synced(&mut a, &mut b);
            } else {
                synced(&mut b, &mut a);
            }
            let held = if day <= 365 { deletion.clone() } else { None };
            assert_eq!(b.profile(&x.id), held, "day {day}");
            assert_eq!(b.secrets.password(&x.id), None, "day {day}");
            assert_kept(&a, &kept, "pw");
            assert_eq!(file_of(&a.path), a_file, "day {day}");
        }
    }

    // ----- jump hosts (issue #9) -----

    /// `p`, going through `jump`.
    fn via(mut p: Profile, jump: &Profile) -> Profile {
        p.jump_host = Some(jump.id.clone());
        p
    }

    /// The frames of one sync in order, each with whether the client sent it.
    type Seen = Vec<(bool, u8, Vec<u8>)>;

    /// Sync `client` with `server` through a relay that copies every frame
    /// across and keeps a copy: the two results, and what was said.
    fn sync_seen(
        client: &mut Machine,
        server: &mut Machine,
        stale: StaleSync,
    ) -> (
        Result<SyncOutcome, SyncError>,
        Result<SyncOutcome, SyncError>,
        Seen,
    ) {
        let (cs, c_side) = pair();
        let (s_side, ss) = pair();
        for side in [&c_side, &s_side] {
            side.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
        }
        let served = spawn_server(ss, server.state.take().unwrap(), PASS);
        let relay = thread::spawn(move || {
            let mut seen = Seen::new();
            // The exchange alternates: client, server, client, server.
            for from_client in [true, false, true, false] {
                let (from, to) = if from_client {
                    (&c_side, &s_side)
                } else {
                    (&s_side, &c_side)
                };
                let Ok((kind, payload)) = read_frame(&mut &*from) else {
                    break;
                };
                seen.push((from_client, kind, payload.clone()));
                if write_frame(&mut &*to, kind, &payload).is_err() || kind == KIND_ERROR {
                    break;
                }
            }
            seen
        });
        let out = run_client(&cs, &cs, client.state.as_mut().unwrap(), PASS, stale);
        drop(cs);
        let (served, back) = served.join().unwrap();
        server.state = Some(back);
        (out, served, relay.join().unwrap())
    }

    /// The payload of the HELLO the client (`true`) or the server (`false`)
    /// sent.
    fn hello_from(seen: &Seen, client: bool) -> Vec<u8> {
        seen.iter()
            .find(|(from_client, kind, _)| *from_client == client && *kind == KIND_HELLO)
            .map(|(_, _, payload)| payload.clone())
            .expect("a HELLO was sent")
    }

    /// What a Moniker that predates jump hosts answers to a HELLO of version 2
    /// (or sends back when it is answered with one): error 2, with its text.
    const OLD_VERSION_ERROR: &[u8] =
        b"\x02the two machines run incompatible Moniker sync versions; update both";

    /// A bastion, a profile `db` that goes through it, and both with a
    /// password.
    fn bastion_and_db() -> (Profile, Profile, Vec<(Profile, Option<&'static str>)>) {
        let bastion = prof("bastion");
        let db = via(prof("db"), &bastion);
        let both = vec![
            (bastion.clone(), Some("pw-bastion")),
            (db.clone(), Some("pw-db")),
        ];
        (bastion, db, both)
    }

    #[test]
    fn two_new_machines_without_jump_hosts_say_version_1() {
        // Byte for byte what machines said before jump hosts, the stale flag
        // included: the literals are the point.
        for (stale, client_hello) in [
            (StaleSync::Refuse, vec![1u8]),
            (StaleSync::Allow, vec![1u8, 1u8]),
        ] {
            let clock = Clock::starting_now();
            let mut a = Machine::new(&clock, &[(prof("one"), Some("pw-one"))]);
            let mut b = Machine::new(&clock, &[(prof("two"), Some("pw-two"))]);
            let (out, served, seen) = sync_seen(&mut a, &mut b, stale);
            out.unwrap();
            served.unwrap();
            assert_eq!(hello_from(&seen, true), client_hello);
            assert_eq!(hello_from(&seen, false), vec![1u8]);
        }
    }

    #[test]
    fn a_jump_host_on_the_client_says_version_2() {
        for (stale, client_hello) in [
            (StaleSync::Refuse, vec![2u8]),
            (StaleSync::Allow, vec![2u8, 1u8]),
        ] {
            let clock = Clock::starting_now();
            let (bastion, db, both) = bastion_and_db();
            let mut a = Machine::new(&clock, &both);
            let mut b = Machine::new(&clock, &[(prof("other"), Some("pw-other"))]);

            let (out, served, seen) = sync_seen(&mut a, &mut b, stale);

            assert_eq!(out.unwrap().added, 1);
            assert_eq!(served.unwrap().added, 2);
            assert_eq!(hello_from(&seen, true), client_hello);
            assert_eq!(hello_from(&seen, false), vec![2u8]);
            let arrived = b.profile(&db.id).unwrap();
            assert_eq!(arrived.jump_host.as_deref(), Some(bastion.id.as_str()));
            assert_eq!(b.secrets.password(&db.id).as_deref(), Some("pw-db"));
            assert!(b.profile(&bastion.id).is_some());
        }
    }

    #[test]
    fn a_jump_host_on_the_server_answers_version_2() {
        let clock = Clock::starting_now();
        let (bastion, db, both) = bastion_and_db();
        let mut a = Machine::new(&clock, &[(prof("one"), Some("pw-one"))]);
        let mut b = Machine::new(&clock, &both);

        let (out, served, seen) = sync_seen(&mut a, &mut b, StaleSync::Refuse);

        // This client has none, so it says 1, and accepts the 2 it is answered.
        assert_eq!(out.unwrap().added, 2);
        assert_eq!(served.unwrap().added, 1);
        assert_eq!(hello_from(&seen, true), vec![1u8]);
        assert_eq!(hello_from(&seen, false), vec![2u8]);
        let arrived = a.profile(&db.id).unwrap();
        assert_eq!(arrived.jump_host.as_deref(), Some(bastion.id.as_str()));
        assert_eq!(a.secrets.password(&db.id).as_deref(), Some("pw-db"));
    }

    #[test]
    fn an_old_server_keeps_a_jump_host_out() {
        let clock = Clock::starting_now();
        let (_, _, both) = bastion_and_db();
        let mut a = Machine::new(&clock, &both);
        let a_file = file_of(&a.path);

        let (cs, ss) = pair();
        let old_server = thread::spawn(move || {
            let hello = read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_ERROR, OLD_VERSION_ERROR).unwrap();
            let mut rest = Vec::new();
            (&ss).read_to_end(&mut rest).unwrap();
            (hello, rest)
        });
        let out = run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse);
        drop(cs);
        let (hello, rest) = old_server.join().unwrap();

        let err = out.unwrap_err();
        assert!(matches!(err, SyncError::PeerTooOld), "got {err:?}");
        assert_eq!(
            err.to_string(),
            "the other machine runs an older Moniker, which does not know jump hosts; \
             update it to sync while a profile has a jump host"
        );
        assert_eq!(hello, (KIND_HELLO, vec![2u8]));
        assert!(rest.is_empty(), "nothing at all after the HELLO: {rest:?}");
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
        assert_eq!(a.last_sync(), None);
    }

    #[test]
    fn an_old_client_is_answered_version_2_and_sends_nothing() {
        let clock = Clock::starting_now();
        let (_, _, both) = bastion_and_db();
        let mut b = Machine::new(&clock, &both);
        let b_file = file_of(&b.path);

        let (cs, ss) = pair();
        cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let server = spawn_server(ss, b.state.take().unwrap(), PASS);
        // What a client that predates jump hosts does: a HELLO of 1, and on a
        // HELLO of 2 an error 2 (its check_version) and nothing else.
        write_frame(&mut &cs, KIND_HELLO, &[1]).unwrap();
        assert_eq!(read_frame(&mut &cs).unwrap(), (KIND_HELLO, vec![2u8]));
        write_frame(&mut &cs, KIND_ERROR, OLD_VERSION_ERROR).unwrap();
        let mut rest = Vec::new();
        (&cs).read_to_end(&mut rest).unwrap();
        let (served, back) = server.join().unwrap();
        b.state = Some(back);

        // The server knows why the other side turned away.
        assert!(
            matches!(served, Err(SyncError::PeerTooOld)),
            "got {served:?}"
        );
        assert!(rest.is_empty(), "nothing after the HELLO: {rest:?}");
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(b.secrets.writes(), 0);
        assert_eq!(b.last_sync(), None);
    }

    #[test]
    fn an_old_server_still_syncs_without_jump_hosts() {
        let clock = Clock::starting_now();
        let two = prof("two");
        let mut a = Machine::new(&clock, &[(prof("one"), Some("pw-one"))]);

        let (cs, ss) = pair();
        let reply = crate::crypto::seal(PASS, &[entry(&two, Some("pw-two"))]).unwrap();
        let old_server = thread::spawn(move || {
            let hello = read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_HELLO, &[1]).unwrap();
            let (kind, _) = read_frame(&mut &ss).unwrap();
            assert_eq!(kind, KIND_BUNDLE);
            write_frame(&mut &ss, KIND_BUNDLE, &reply).unwrap();
            hello
        });
        let out = run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse);
        drop(cs);

        assert_eq!(out.unwrap().added, 1);
        assert_eq!(old_server.join().unwrap(), (KIND_HELLO, vec![1u8]));
        assert!(a.profile(&two.id).is_some());
        assert_eq!(a.secrets.password(&two.id).as_deref(), Some("pw-two"));
    }

    #[test]
    fn a_client_that_said_2_accepts_only_a_reply_of_2() {
        let clock = Clock::starting_now();
        let (_, _, both) = bastion_and_db();
        let mut a = Machine::new(&clock, &both);
        let a_file = file_of(&a.path);

        // A server that answers a HELLO of 2 with 1 has not understood it;
        // no profile may go to it.
        let (cs, ss) = pair();
        cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let server = thread::spawn(move || {
            read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_HELLO, &[1]).unwrap();
            let mut rest = Vec::new();
            (&ss).read_to_end(&mut rest).unwrap();
            rest
        });
        let out = run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse);
        drop(cs);

        let err = out.unwrap_err();
        assert!(matches!(err, SyncError::VersionMismatch), "got {err:?}");
        let rest = frames(&server.join().unwrap());
        assert_eq!(rest.len(), 1, "only the error: {rest:?}");
        assert_eq!((rest[0].0, rest[0].1[0]), (KIND_ERROR, 2));
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn a_kept_profile_s_jump_host_does_not_count() {
        let clock = Clock::starting_now();
        let (_, db, both) = bastion_and_db();
        let mut a = Machine::new(&clock, &both);
        // Control: as it is, the jump host is sent.
        assert!(holds_jump_host(a.export().iter().map(|e| &e.profile)));

        // Kept on this machine, `db` leaves as its bare tombstone, and with it
        // goes the only jump host.
        a.keep_here(&db.id, true);
        let sent = a.export();
        assert!(sent.iter().all(|e| e.profile.jump_host.is_none()));
        assert!(!holds_jump_host(sent.iter().map(|e| &e.profile)));
        let mut b = Machine::new(&clock, &[(prof("two"), Some("pw-two"))]);

        let (out, served, seen) = sync_seen(&mut a, &mut b, StaleSync::Refuse);

        out.unwrap();
        served.unwrap();
        assert_eq!(hello_from(&seen, true), vec![1u8]);
        assert_eq!(hello_from(&seen, false), vec![1u8]);
        assert!(a.profile(&db.id).unwrap().local_only);
        assert_eq!(b.profile(&db.id).and_then(|p| p.jump_host), None);
    }

    #[test]
    fn a_jump_host_to_a_missing_profile_is_accepted() {
        let clock = Clock::starting_now();

        // The jump host is kept on the sending machine only, so the other
        // never gets it: db arrives and waits, and nothing is refused.
        let (bastion, db, both) = bastion_and_db();
        let mut a = Machine::new(&clock, &both);
        a.keep_here(&bastion.id, true);
        let mut b = Machine::new(&clock, &[]);
        let (out, served, seen) = sync_seen(&mut a, &mut b, StaleSync::Refuse);
        out.unwrap();
        served.unwrap();
        assert_eq!(hello_from(&seen, true), vec![2u8]);
        let arrived = b.profile(&db.id).unwrap();
        assert!(!arrived.deleted);
        assert_eq!(arrived.jump_host.as_deref(), Some(bastion.id.as_str()));
        assert!(b.profile(&bastion.id).is_none_or(|p| p.deleted));
        assert_eq!(b.secrets.password(&db.id).as_deref(), Some("pw-db"));

        // And one whose jump host nobody ever had.
        let ghost = prof("ghost");
        let lonely = via(prof("lonely"), &ghost);
        let mut a = Machine::new(&clock, &[(lonely.clone(), Some("pw-lonely"))]);
        let mut b = Machine::new(&clock, &[]);
        let (out, served, _) = sync_seen(&mut a, &mut b, StaleSync::Refuse);
        out.unwrap();
        served.unwrap();
        let arrived = b.profile(&lonely.id).unwrap();
        assert_eq!(arrived.jump_host.as_deref(), Some(ghost.id.as_str()));
        assert!(b.profile(&ghost.id).is_none());

        // The same, arriving at a client from a server.
        let mut a = Machine::new(&clock, &[]);
        let mut b = Machine::new(&clock, &[(lonely.clone(), Some("pw-lonely"))]);
        let (out, served, _) = sync_seen(&mut a, &mut b, StaleSync::Refuse);
        out.unwrap();
        served.unwrap();
        assert_eq!(
            a.profile(&lonely.id).unwrap().jump_host.as_deref(),
            Some(ghost.id.as_str())
        );
    }

    #[test]
    fn a_self_reference_rejects_the_sync() {
        let mut looped = prof("looped");
        looped.jump_host = Some(looped.id.clone());
        let good = prof("good");
        let clock = Clock::starting_now();

        // A hostile client: the server rejects everything it sent.
        let mut b = Machine::new(&clock, &[(prof("two"), Some("pw-two"))]);
        let b_file = file_of(&b.path);
        let (cs, ss) = pair();
        cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let server = spawn_server(ss, b.state.take().unwrap(), PASS);
        write_frame(&mut &cs, KIND_HELLO, &[2]).unwrap();
        assert_eq!(read_frame(&mut &cs).unwrap(), (KIND_HELLO, vec![2u8]));
        let sealed =
            crate::crypto::seal(PASS, &[entry(&good, Some("g")), entry(&looped, Some("l"))])
                .unwrap();
        write_frame(&mut &cs, KIND_BUNDLE, &sealed).unwrap();
        let (kind, _) = read_frame(&mut &cs).unwrap();
        drop(cs);
        let (served, back) = server.join().unwrap();
        b.state = Some(back);
        assert_eq!(kind, KIND_ERROR);
        assert!(matches!(served, Err(SyncError::Store(_))), "{served:?}");
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(b.secrets.writes(), 0);

        // A hostile server: the client rejects its reply.
        let mut a = Machine::new(&clock, &[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let reply = crate::crypto::seal(PASS, &[entry(&looped, Some("l"))]).unwrap();
        let (cs, ss) = pair();
        let server = thread::spawn(move || {
            read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_HELLO, &[2]).unwrap();
            read_frame(&mut &ss).unwrap();
            write_frame(&mut &ss, KIND_BUNDLE, &reply).unwrap();
        });
        let out = run_client(&cs, &cs, a.state.as_mut().unwrap(), PASS, StaleSync::Refuse);
        drop(cs);
        server.join().unwrap();
        let err = out.unwrap_err();
        assert!(matches!(err, SyncError::Store(_)), "client got {err:?}");
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);

        // And apply itself refuses it.
        let err = a
            .state
            .as_mut()
            .unwrap()
            .apply(vec![entry(&looped, None)])
            .unwrap_err();
        assert!(matches!(err, SyncError::Store(_)), "{err:?}");
        assert_eq!(file_of(&a.path), a_file);
    }

    /// A target that counts how often it is asked to export.
    struct CountedExports {
        inner: LocalState,
        exports: Cell<usize>,
    }

    impl CountedExports {
        fn new(inner: LocalState) -> Self {
            CountedExports {
                inner,
                exports: Cell::new(0),
            }
        }
    }

    impl SyncTarget for CountedExports {
        fn sync_on(&self) -> Result<(), SyncError> {
            self.inner.sync_on()
        }
        fn ready(&self) -> Result<(), SyncError> {
            self.inner.ready()
        }
        fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
            self.exports.set(self.exports.get() + 1);
            self.inner.export()
        }
        fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
            self.inner.apply(merged)
        }
        fn last_sync(&self) -> Option<i64> {
            self.inner.last_sync()
        }
        fn record_sync(&mut self) {
            self.inner.record_sync();
        }
        fn now(&self) -> i64 {
            self.inner.now()
        }
    }

    #[test]
    fn the_export_is_read_once_per_sync() {
        for with_jump_host in [false, true] {
            let clock = Clock::starting_now();
            let (_, _, both) = bastion_and_db();
            let kept = if with_jump_host { 2 } else { 1 };
            let mut a = Machine::new(&clock, &both[..kept]);
            let mut b = Machine::new(&clock, &[(prof("two"), Some("pw-two"))]);
            let mut client = CountedExports::new(a.state.take().unwrap());
            let (cs, ss) = pair();
            let server = spawn_server(ss, CountedExports::new(b.state.take().unwrap()), PASS);

            run_client(&cs, &cs, &mut client, PASS, StaleSync::Refuse).unwrap();
            drop(cs);
            let (served, server) = server.join().unwrap();
            served.unwrap();

            assert_eq!(
                client.exports.get(),
                1,
                "client, jump host: {with_jump_host}"
            );
            assert_eq!(
                server.exports.get(),
                1,
                "server, jump host: {with_jump_host}"
            );
        }
    }

    #[test]
    fn nothing_is_exported_before_the_checks_pass() {
        let clock = Clock::starting_now();
        let start = clock.now();
        let one = || (prof("one"), Some("pw-one"));

        // A client with sync off: no export, and nothing sent.
        let a = Machine::new(&clock, &[one()]);
        a.sync_off();
        let mut client = CountedExports::new(a.state.unwrap());
        let (cs, _ss) = pair();
        let out = run_client(&cs, &cs, &mut client, PASS, StaleSync::Refuse);
        assert!(matches!(out, Err(SyncError::SyncOff)), "{out:?}");
        assert_eq!(client.exports.get(), 0);

        // A server with sync off, a version it does not know, or away: the
        // HELLO is refused before it reads anything.
        for case in ["sync off", "version 9", "away"] {
            let b = Machine::new(&clock, &[one()]);
            match case {
                "sync off" => b.sync_off(),
                "away" => crate::sync_state::record_sync(&b.path, start - 200 * DAY_MS).unwrap(),
                _ => {}
            }
            let (cs, ss) = pair();
            cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let server = spawn_server(ss, CountedExports::new(b.state.unwrap()), PASS);
            let hello: &[u8] = if case == "version 9" { &[9] } else { &[1] };
            write_frame(&mut &cs, KIND_HELLO, hello).unwrap();
            assert_eq!(read_frame(&mut &cs).unwrap().0, KIND_ERROR, "{case}");
            drop(cs);
            let (served, server) = server.join().unwrap();
            assert!(served.is_err(), "{case}");
            assert_eq!(server.exports.get(), 0, "{case}");
        }
    }

    /// A target whose profiles change between the first time it is asked and
    /// every time after: an edit made while a sync runs.
    struct Changing {
        reads: Cell<usize>,
        first: Vec<BundleEntry>,
        later: Vec<BundleEntry>,
        applied: Option<Vec<BundleEntry>>,
    }

    impl SyncTarget for Changing {
        fn ready(&self) -> Result<(), SyncError> {
            Ok(())
        }
        fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
            let n = self.reads.get();
            self.reads.set(n + 1);
            Ok(if n == 0 {
                self.first.clone()
            } else {
                self.later.clone()
            })
        }
        fn apply(&mut self, merged: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
            self.applied = Some(merged);
            Ok(SyncOutcome::default())
        }
    }

    /// `entries` in the order of their ids (`merge` sorts that way).
    fn by_id(mut entries: Vec<BundleEntry>) -> Vec<BundleEntry> {
        entries.sort_by(|a, b| a.profile.id.cmp(&b.profile.id));
        entries
    }

    #[test]
    fn the_hello_and_the_bundle_come_from_the_same_read() {
        let (_, _, both) = bastion_and_db();
        let plain = vec![entry(&prof("plain"), Some("pw"))];
        let jumped: Vec<BundleEntry> = both.iter().map(|(p, pw)| entry(p, *pw)).collect();
        for (first, later) in [(&plain, &jumped), (&jumped, &plain)] {
            let said = if holds_jump_host(first.iter().map(|e| &e.profile)) {
                2u8
            } else {
                1u8
            };
            let target = || Changing {
                reads: Cell::new(0),
                first: first.clone(),
                later: later.clone(),
                applied: None,
            };
            let nothing = crate::crypto::seal(PASS, &[]).unwrap();

            // As a client: the bundle is what the HELLO's version was decided
            // from, and the target was read once.
            let mut client = target();
            let (cs, ss) = pair();
            let reply = nothing.clone();
            let raw_server = thread::spawn(move || {
                let hello = read_frame(&mut &ss).unwrap();
                write_frame(&mut &ss, KIND_HELLO, &[2]).unwrap();
                let (kind, sealed) = read_frame(&mut &ss).unwrap();
                assert_eq!(kind, KIND_BUNDLE);
                write_frame(&mut &ss, KIND_BUNDLE, &reply).unwrap();
                (hello, open_bundle(PASS, &sealed).unwrap())
            });
            run_client(&cs, &cs, &mut client, PASS, StaleSync::Refuse).unwrap();
            drop(cs);
            let (hello, sent) = raw_server.join().unwrap();
            assert_eq!(hello, (KIND_HELLO, vec![said]));
            assert!(
                by_id(sent) == by_id(first.clone()),
                "client: not what was read"
            );
            assert_eq!(client.reads.get(), 1);

            // As a server: the same.
            let (cs, ss) = pair();
            cs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let server = spawn_server(ss, target(), PASS);
            write_frame(&mut &cs, KIND_HELLO, &[1]).unwrap();
            let answer = read_frame(&mut &cs).unwrap();
            write_frame(&mut &cs, KIND_BUNDLE, &nothing).unwrap();
            let (kind, sealed) = read_frame(&mut &cs).unwrap();
            drop(cs);
            let (served, server) = server.join().unwrap();
            served.unwrap();
            assert_eq!(answer, (KIND_HELLO, vec![said]));
            assert_eq!(kind, KIND_BUNDLE);
            let replied = by_id(open_bundle(PASS, &sealed).unwrap());
            assert!(replied == by_id(first.clone()), "server: not what was read");
            assert!(
                by_id(server.applied.clone().unwrap()) == replied,
                "server: applied what it replied"
            );
            assert_eq!(server.reads.get(), 1);
        }
    }

    #[test]
    fn a_client_whose_export_fails_tells_the_server_why() {
        /// A machine whose profiles cannot be read for the sync.
        struct Unreadable;

        impl SyncTarget for Unreadable {
            fn ready(&self) -> Result<(), SyncError> {
                Ok(())
            }
            fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
                Err(SyncError::Store(
                    "the profile file could not be read".to_string(),
                ))
            }
            fn apply(&mut self, _: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
                unreachable!("nothing arrives for a client that sent nothing")
            }
        }
        const SAID: &str = "profile data error: the profile file could not be read";

        // On the wire: an ERROR in place of the HELLO, and nothing else.
        let (cs, ss) = pair();
        let err = run_client(&cs, &cs, &mut Unreadable, PASS, StaleSync::Refuse).unwrap_err();
        drop(cs);
        assert!(matches!(err, SyncError::Store(_)), "{err:?}");
        let mut sent = Vec::new();
        (&ss).read_to_end(&mut sent).unwrap();
        let mut said = vec![CODE_OTHER];
        said.extend_from_slice(SAID.as_bytes());
        assert_eq!(frames(&sent), vec![(KIND_ERROR, said)]);

        // A server reads it where the HELLO would be, says why, and changes
        // nothing.
        let clock = Clock::starting_now();
        let mut b = Machine::new(&clock, &[(prof("two"), Some("pw-two"))]);
        let b_file = file_of(&b.path);
        let (cs, ss) = pair();
        let server = spawn_server(ss, b.state.take().unwrap(), PASS);
        run_client(&cs, &cs, &mut Unreadable, PASS, StaleSync::Refuse).unwrap_err();
        drop(cs);
        let (served, back) = server.join().unwrap();
        b.state = Some(back);
        match served {
            Err(SyncError::Remote(text)) => assert_eq!(text, SAID),
            other => panic!("the server got {other:?}"),
        }
        assert_eq!(file_of(&b.path), b_file);
        assert_eq!(b.secrets.writes(), 0);
        assert_eq!(b.last_sync(), None);
    }

    // ----- a sync through the partner's jump host (issue #9) -----

    /// A sync's connection through a jump host, played by scripts in place of
    /// `ssh` (no network), in a private folder that is also the runtime folder
    /// of the socket. The commands are the real ones (`JumpLeg::for_sync`, and
    /// `sync_command` through its hop) with `sh` reading a script instead of
    /// running `ssh`: `master`, `check`, `exit` and `target`, each of which
    /// writes its name to `log` when it runs. (`sh` reads the scripts, which are
    /// never executed themselves, so one just written can never be busy.)
    ///
    /// The master that signs in leaves a process standing for ssh's master gone
    /// to the background, a `sleep` with its own input and output (so the
    /// master's stderr ends when the master exits), and writes its pid to
    /// `pid`. `check` answers with that pid as ssh does, and `exit` ends it. If
    /// it still runs when the test is over, it is killed.
    struct FakeJump {
        dir: TempDir,
        leg: JumpLeg,
        target: SshCommand,
    }

    /// A master that signs in.
    const SIGNS_IN: &str = r#"echo master >> "$D/log"
sleep 600 </dev/null >/dev/null 2>&1 &
echo $! > "$D/pid""#;

    /// `ssh -O check` answering for the master.
    const ANSWERS: &str = r#"echo check >> "$D/log"
printf 'Master running (pid=%s)\r\n' "$(cat "$D/pid")" >&2"#;

    /// `ssh -O exit` ending the master.
    const ENDS: &str = r#"echo exit >> "$D/log"
kill "$(cat "$D/pid")""#;

    /// The target's `ssh`, finding no moniker-server on the other machine.
    const NO_SERVER: &str = r#"echo target >> "$D/log"
exit 127"#;

    impl FakeJump {
        fn new() -> FakeJump {
            // Short and plain enough for a socket's path, and private.
            let dir = tempfile::Builder::new()
                .prefix("rt")
                .tempdir_in("/tmp")
                .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            }
            let socket = JumpSocket::new(dir.path()).unwrap();
            let bastion = Profile::new("bastion", "bastion.example", "alice");
            let mut db = prof("db");
            db.jump_host = Some(bastion.id.clone());
            let askpass = Path::new("/nonexistent/moniker-askpass");
            let mut leg = JumpLeg::for_sync(&bastion, &socket, askpass, false).unwrap();
            let script = |name: &str| dir.path().join(name);
            leg.master = played_by(leg.master, &script("master"));
            leg.check = played_by(leg.check, &script("check"));
            leg.exit = played_by(leg.exit, &script("exit"));
            let target = played_by(
                sync_command(&db, askpass, false, SERVER_PROGRAM, Some(&leg.hop)),
                &script("target"),
            );
            let fake = FakeJump { dir, leg, target };
            fake.script("master", SIGNS_IN);
            fake.script("check", ANSWERS);
            fake.script("exit", ENDS);
            fake.script("target", NO_SERVER);
            fake
        }

        /// Make `body` the script `name`, with `$D` its folder.
        fn script(&self, name: &str, body: &str) {
            let text = format!("D='{}'\n{body}\n", self.dir.path().display());
            fs::write(self.path(name), text).unwrap();
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// What ran, in order.
        fn log(&self) -> Vec<String> {
            fs::read_to_string(self.path("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// The pid of the process standing for the master.
        fn stand_in(&self) -> u32 {
            fs::read_to_string(self.path("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }

        /// Sync `local` through this jump host within `timeout`.
        fn sync(
            &self,
            local: &mut dyn SyncTarget,
            timeout: Duration,
        ) -> Result<SyncOutcome, SyncError> {
            sync_over_ssh_with_timeout(
                local,
                &self.target,
                Some(&self.leg),
                PASS,
                SshTerminal::Inherit,
                StaleSync::Refuse,
                timeout,
            )
        }
    }

    impl Drop for FakeJump {
        fn drop(&mut self) {
            let Some(pid) = fs::read_to_string(self.path("pid"))
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok())
            else {
                return;
            };
            // Only the stand-in this test started, if it still runs.
            let ours =
                fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.starts_with(b"sleep\x00"));
            if ours && !has_ended(pid) {
                let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }

    /// `cmd` with `sh` reading `script` in place of running `ssh`, given the
    /// same arguments and environment.
    fn played_by(mut cmd: SshCommand, script: &Path) -> SshCommand {
        cmd.args.insert(0, script.to_str().unwrap().to_owned());
        cmd.program = "sh".to_owned();
        cmd
    }

    /// Whether the process `pid` has ended: it is gone, or a zombie. (`kill -0`
    /// still finds a zombie, and the zombie of a process that lost its parent
    /// waits for whatever adopted it; a container's first process may never
    /// reap it.)
    fn has_ended(pid: u32) -> bool {
        let state = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let (_, rest) = stat.rsplit_once(')')?;
                rest.trim_start().chars().next()
            });
        matches!(state, None | Some('Z' | 'X'))
    }

    /// Whether the process `pid` ends within a few seconds: ending it is a
    /// signal, which takes effect a moment later.
    fn ends_soon(pid: u32) -> bool {
        let start = Instant::now();
        while !has_ended(pid) {
            if start.elapsed() > Duration::from_secs(5) {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn a_sync_goes_through_its_jump_host() {
        let fake = FakeJump::new();
        // The target relays the protocol to a real server, as in
        // `sync_over_ssh_full_exchange_with_a_child_process`.
        let (to_server, to_client) = (fake.path("to_server"), fake.path("to_client"));
        for f in [&to_server, &to_client] {
            let made = std::process::Command::new("mkfifo").arg(f).status();
            assert!(made.unwrap().success());
        }
        // It also says whether the master still ran when it started and when
        // it was done.
        fake.script(
            "target",
            r#"up() { s=$(cut -d' ' -f3 "/proc/$(cat "$D/pid")/stat"); [ "${s:-Z}" != Z ]; }
up && echo target >> "$D/log" || echo 'target, no master' >> "$D/log"
printf '%s\n' "$@" > "$D/target-args"
exec 3<&0; cat <&3 > "$D/to_server" & cat < "$D/to_client"; wait
up && echo 'target, done' >> "$D/log" || echo 'target, done, no master' >> "$D/log""#,
        );
        let (p1, p2) = (prof("one"), prof("two"));
        let mut a = side(&[(p1.clone(), Some("pw-one"))]);
        let b = side(&[(p2.clone(), Some("pw-two"))]);
        let server = thread::spawn(move || {
            let mut b_state = b.state;
            let r = fs::File::open(&to_server).unwrap();
            let w = fs::OpenOptions::new().write(true).open(&to_client).unwrap();
            let res = serve(r, w, &mut b_state, PASS);
            (res, b.secrets, b._dir)
        });

        let out = fake.sync(&mut a.state, SYNC_TIMEOUT).unwrap();

        assert_eq!(out.added, 1);
        let (served, b_secrets, _b_dir) = server.join().unwrap();
        assert_eq!(served.unwrap().added, 1);
        assert_eq!(a.secrets.password(&p2.id).as_deref(), Some("pw-two"));
        assert_eq!(b_secrets.password(&p1.id).as_deref(), Some("pw-one"));
        // The master signed in first and was asked for its pid twice, then
        // the target went through it (its ProxyCommand is the hop) while it
        // ran.
        assert_eq!(
            fake.log(),
            ["master", "check", "check", "target", "target, done"]
        );
        let args = fs::read_to_string(fake.path("target-args")).unwrap();
        let proxy = format!("ProxyCommand={}", fake.leg.hop);
        assert!(args.lines().any(|arg| arg == proxy), "{args}");
        // Ended when the sync returned, through its pidfd: no `-O exit`.
        assert!(ends_soon(fake.stand_in()), "the master was left running");
        assert!(!fake.log().contains(&"exit".to_string()));
    }

    #[test]
    fn a_failed_sync_still_ends_the_master() {
        // The other machine refuses the login.
        let fake = FakeJump::new();
        fake.script(
            "target",
            r#"echo target >> "$D/log"
echo 'alice@db: Permission denied (publickey,password).' >&2
exit 255"#,
        );
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let err = fake.sync(&mut a.state, SYNC_TIMEOUT).unwrap_err();
        assert!(
            matches!(&err, SyncError::Remote(text) if text.contains("Permission denied")),
            "{err:?}"
        );
        assert_eq!(fake.log(), ["master", "check", "check", "target"]);
        assert!(ends_soon(fake.stand_in()), "the master was left running");

        // The other machine never answers.
        let fake = FakeJump::new();
        fake.script("target", r#"echo target >> "$D/log"; exec sleep 30"#);
        let err = fake.sync(&mut a.state, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, SyncError::Timeout), "{err:?}");
        assert_eq!(fake.log(), ["master", "check", "check", "target"]);
        assert!(ends_soon(fake.stand_in()), "the master was left running");

        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn a_panic_still_ends_the_master() {
        /// A machine whose code panics while the sync runs.
        struct Panics;

        impl SyncTarget for Panics {
            fn ready(&self) -> Result<(), SyncError> {
                Ok(())
            }
            fn export(&self) -> Result<Vec<BundleEntry>, SyncError> {
                panic!("a bug in the middle of a sync")
            }
            fn apply(&mut self, _: Vec<BundleEntry>) -> Result<SyncOutcome, SyncError> {
                unreachable!("the sync panicked before")
            }
        }

        let fake = FakeJump::new();
        fake.script("target", r#"echo target >> "$D/log"; exec sleep 30"#);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fake.sync(&mut Panics, SYNC_TIMEOUT)
        }));
        assert!(panicked.is_err());
        // The target may have been killed by then before it said anything.
        assert_eq!(fake.log()[..3], ["master", "check", "check"]);
        assert!(ends_soon(fake.stand_in()), "the master was left running");
    }

    #[test]
    fn a_master_whose_stderr_stays_open_does_not_hold_the_sync() {
        // A ProxyCommand that ~/.ssh/config gives the jump host keeps the
        // master's stderr open as long as the master runs: here the stand-in
        // does. (Should the sync wait for that stderr to end, this takes the
        // stand-in's 20 s.)
        let fake = FakeJump::new();
        fake.script(
            "master",
            r#"echo master >> "$D/log"
sleep 20 </dev/null >/dev/null &
echo $! > "$D/pid""#,
        );
        let mut a = side(&[]);
        let start = Instant::now();
        let err = fake.sync(&mut a.state, SYNC_TIMEOUT).unwrap_err();
        assert!(matches!(err, SyncError::ServerNotFound), "{err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(fake.log(), ["master", "check", "check", "target"]);
        assert!(ends_soon(fake.stand_in()), "the master was left running");
    }

    #[test]
    fn a_master_with_no_pid_is_ended_with_exit() {
        let fake = FakeJump::new();
        fake.script(
            "check",
            r#"echo check >> "$D/log"
echo 'Control socket connect(/run/x): No such file or directory' >&2
exit 255"#,
        );
        let mut a = side(&[]);
        let err = fake.sync(&mut a.state, SYNC_TIMEOUT).unwrap_err();
        assert!(matches!(err, SyncError::ServerNotFound), "{err:?}");
        // No pid: the target still goes through it, and the master is then
        // ended through its socket.
        assert_eq!(fake.log(), ["master", "check", "target", "exit"]);
        assert!(ends_soon(fake.stand_in()), "the master was left running");
    }

    #[test]
    fn an_unknown_jump_host_key_says_to_connect_in_a_tab() {
        let fake = FakeJump::new();
        fake.script(
            "master",
            r#"echo master >> "$D/log"
echo 'Host key verification failed.' >&2
exit 255"#,
        );
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        let err = fake.sync(&mut a.state, SYNC_TIMEOUT).unwrap_err();
        let SyncError::JumpHostKeyUnknown { name } = &err else {
            panic!("expected JumpHostKeyUnknown, got {err:?}");
        };
        assert_eq!(name, "bastion");
        assert_eq!(
            err.to_string(),
            "connect to bastion once in a tab first, so its host key is known"
        );
        assert_eq!(fake.log(), ["master"], "nothing more was started");
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn a_failed_jump_connection_names_the_jump_host_and_never_starts_the_target() {
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        // `ran` is what ran: the master and, should it have been left behind,
        // the `-O exit` that ends it. Never the target.
        let failed = |fake: &FakeJump, timeout: Duration, a: &mut Side, ran: &[&str]| {
            let err = fake.sync(&mut a.state, timeout).unwrap_err();
            let SyncError::JumpFailed { name, detail } = &err else {
                panic!("expected JumpFailed, got {err:?}");
            };
            assert_eq!(name, "bastion");
            assert_eq!(
                err.to_string(),
                format!("could not connect to the jump host bastion: {detail}")
            );
            assert!(!detail.chars().any(char::is_control), "{detail:?}");
            assert_eq!(fake.log(), ran);
            detail.clone()
        };

        // The jump host refuses the login: the last lines ssh wrote, cleaned.
        // ssh exited with a status before it went to the background, so there
        // is no master to end.
        let fake = FakeJump::new();
        fake.script(
            "master",
            r#"echo master >> "$D/log"
printf 'early noise\nalice@bastion.example: Permission denied \033[1m(publickey,password).\r\n' >&2
exit 255"#,
        );
        assert_eq!(
            failed(&fake, SYNC_TIMEOUT, &mut a, &["master"]),
            "ssh exited with status 255: early noise | \
             alice@bastion.example: Permission denied [1m(publickey,password)."
        );

        // Killed by a signal, saying nothing. It may have forked its
        // background part first, so the master is ended through its socket.
        let fake = FakeJump::new();
        fake.script("master", r#"echo master >> "$D/log"; kill -KILL $$"#);
        assert_eq!(
            failed(&fake, SYNC_TIMEOUT, &mut a, &["master", "exit"]),
            "ssh was terminated by a signal"
        );

        // Still signing in when the sync's time is up: killed and reaped, and
        // asked to end too, because the time can be up just after ssh's `-f`
        // fork.
        let fake = FakeJump::new();
        fake.script(
            "master",
            r#"echo master >> "$D/log"
echo $$ > "$D/master"
echo 'Connecting to bastion.example' >&2
exec sleep 30"#,
        );
        let start = Instant::now();
        assert_eq!(
            failed(&fake, Duration::from_secs(1), &mut a, &["master", "exit"]),
            "ssh did not sign in in time: Connecting to bastion.example"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let master: u32 = fs::read_to_string(fake.path("master"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(has_ended(master), "the master still signs in");

        // No ssh to start.
        let mut fake = FakeJump::new();
        fake.leg.master.program = "/nonexistent/moniker-test-ssh".to_string();
        let detail = failed(&fake, SYNC_TIMEOUT, &mut a, &[]);
        assert!(
            detail.starts_with("could not start /nonexistent/moniker-test-ssh: "),
            "{detail}"
        );

        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn a_command_that_does_not_go_through_the_hop_is_refused_before_the_master() {
        // A caller that signed in to the jump host and then reached the
        // partner directly would defeat the person's choice of route: fail
        // closed, before anything starts.
        let fake = FakeJump::new();
        let db = prof("db");
        let askpass = Path::new("/nonexistent/moniker-askpass");
        let hop = fake.leg.hop.clone();
        let wrong = [
            // No proxy at all.
            sync_command(&db, askpass, false, SERVER_PROGRAM, None),
            // Another jump host's.
            sync_command(
                &db,
                askpass,
                false,
                SERVER_PROGRAM,
                Some("ssh -W '[%h]:%p' elsewhere"),
            ),
            // The hop's text where it is no option: as the remote command.
            sync_command(&db, askpass, false, &format!("ProxyCommand={hop}"), None),
        ];
        let mut a = side(&[(prof("one"), Some("pw-one"))]);
        let a_file = file_of(&a.path);
        for cmd in wrong {
            let cmd = played_by(cmd, &fake.path("target"));
            let err = sync_over_ssh_with_timeout(
                &mut a.state,
                &cmd,
                Some(&fake.leg),
                PASS,
                SshTerminal::Inherit,
                StaleSync::Refuse,
                SYNC_TIMEOUT,
            )
            .unwrap_err();
            let SyncError::Io(io_err) = &err else {
                panic!("expected an I/O error, got {err:?}");
            };
            assert_eq!(io_err.kind(), io::ErrorKind::InvalidInput, "{err:?}");
            assert_eq!(
                io_err.to_string(),
                "the sync's ssh does not go through the jump host"
            );
        }
        // Neither the master nor the target ever ran.
        assert_eq!(fake.log(), Vec::<String>::new());
        assert!(!fake.path("pid").exists());
        assert_eq!(file_of(&a.path), a_file);
        assert_eq!(a.secrets.writes(), 0);
    }

    #[test]
    fn sync_off_refuses_before_the_master_starts() {
        let fake = FakeJump::new();
        let refused = |local: &mut dyn SyncTarget, passphrase: &str| {
            sync_over_ssh(
                local,
                &fake.target,
                Some(&fake.leg),
                passphrase,
                SshTerminal::Detached,
                StaleSync::Refuse,
            )
            .unwrap_err()
        };
        let clock = Clock::starting_now();

        let mut off = Machine::new(&clock, &[(prof("one"), Some("pw-one"))]);
        off.sync_off();
        let err = refused(off.state.as_mut().unwrap(), PASS);
        assert!(matches!(err, SyncError::SyncOff), "{err:?}");

        // So do the sync's other refusals: no sync key, no secret store, a
        // machine that has been away.
        let mut a = Machine::new(&clock, &[(prof("one"), Some("pw-one"))]);
        let err = refused(a.state.as_mut().unwrap(), "");
        assert!(matches!(err, SyncError::EmptyPassphrase), "{err:?}");
        let mut no_keyring = Stub {
            no_secret_store: true,
            ..Stub::default()
        };
        let err = refused(&mut no_keyring, PASS);
        assert!(matches!(err, SyncError::NoSecretStore), "{err:?}");
        crate::sync_state::record_sync(&a.path, clock.now() - 181 * DAY_MS).unwrap();
        let err = refused(a.state.as_mut().unwrap(), PASS);
        assert!(matches!(err, SyncError::Stale { days: 181 }), "{err:?}");

        assert_eq!(fake.log(), Vec::<String>::new(), "no ssh was started");
    }

    /// `[pid, parent, session, stdin, stdout]` of a jump connection's master,
    /// as it reads them itself, in a sync run with `terminal`. It then fails,
    /// as a jump host that refuses the login.
    fn ids_of_the_master(terminal: SshTerminal) -> [String; 5] {
        let fake = FakeJump::new();
        fake.script(
            "master",
            r#"fd=/proc/$$/fd
set -- $(cat /proc/$$/stat)
printf '%s %s %s %s %s' "$1" "$4" "$6" "$(readlink $fd/0)" "$(readlink $fd/1)" > "$D/ids"
exit 255"#,
        );
        let mut a = side(&[]);
        let err = sync_over_ssh(
            &mut a.state,
            &fake.target,
            Some(&fake.leg),
            PASS,
            terminal,
            StaleSync::Refuse,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::JumpFailed { .. }), "{err:?}");
        let ids = fs::read_to_string(fake.path("ids")).unwrap();
        let ids: Vec<String> = ids.split(' ').map(str::to_owned).collect();
        ids.try_into().unwrap()
    }

    #[test]
    fn a_jump_connection_may_ask_only_where_the_sync_s_ssh_may() {
        // Detached, as the GUI's syncs: a session of its own, so a question
        // fails at once instead of waiting in the terminal Moniker came from.
        let [pid, parent, session, stdin, stdout] = ids_of_the_master(SshTerminal::Detached);
        assert_eq!(session, pid, "the session leader");
        assert_ne!(session, own_session());
        // Still this process's own child, which the askpass helper checks.
        assert_eq!(parent, std::process::id().to_string());
        assert_eq!([stdin, stdout], ["/dev/null", "/dev/null"]);

        // Inherit, as `moniker-server sync`: in this session, so it can ask.
        let [pid, parent, session, stdin, stdout] = ids_of_the_master(SshTerminal::Inherit);
        assert_eq!(session, own_session());
        assert_ne!(session, pid);
        assert_eq!(parent, std::process::id().to_string());
        assert_eq!([stdin, stdout], ["/dev/null", "/dev/null"]);
    }
}
