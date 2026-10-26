//! On-disk store of connection profiles (a JSON file, no secrets).
//!
//! The file is `{"version":1,"profiles":[...]}` with the profiles sorted by
//! `id` and pretty-printed. It lives at `$XDG_CONFIG_HOME/moniker/profiles.json`
//! (see [`ProfileStore::default_path`]). A shell script may write the same
//! format, so it is part of the interface: change it only with a version bump.
//!
//! Version 2, `{"version":2,"sync_off":<bool>,"profiles":[...]}`, is written
//! exactly when the sync switch is not on (see [`SyncSwitch`]) or a profile is
//! kept on this machine (`local_only`); otherwise the file is version 1, as it
//! always was. Older versions refuse a file of another version and leave it
//! alone, so none of them can serve or start a sync from a file that says sync
//! is off or holds a profile that must not leave. Version 1 has no switch and
//! reads as on. The switch lives in this file so that one atomic write under
//! the store's lock changes it together with the profiles.
//!
//! Passwords are not stored here; they live in the system keyring.
//!
//! The file can change under an open store: `moniker-server sync-serve` run over ssh
//! rewrites it while the GUI is running. So a store is a snapshot, and a
//! mutation ([`upsert`](ProfileStore::upsert), [`delete`](ProfileStore::delete),
//! [`replace_all`](ProfileStore::replace_all), [`update`](ProfileStore::update))
//! is a locked read-modify-write:
//!
//! 1. take an exclusive advisory lock on `<file>.lock` (blocking);
//! 2. re-read and re-validate the file as it is now (missing means empty;
//!    corrupt or invalid content fails the mutation and leaves the file alone);
//! 3. apply the change to that fresh state;
//! 4. write it to a temp file in the same directory (mode 0600 from the moment
//!    it is created), fsync, and rename it over the target;
//! 5. replace the in-memory snapshot with the result and drop the lock.
//!
//! A failure at any step leaves the file and the in-memory snapshot as they
//! were. Reads ([`live`](ProfileStore::live), [`get`](ProfileStore::get),
//! [`all`](ProfileStore::all)) serve the snapshot without locking;
//! [`reload`](ProfileStore::reload) refreshes it. Writers only ever rename a
//! complete file into place, so a reader sees the old file or the new one.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::profile::{Profile, ProfileError, now_ms};

/// The format of a file that holds neither the sync switch off nor a profile
/// kept on this machine: what every version before the switch wrote.
const FORMAT_V1: u64 = 1;
/// The format that carries `sync_off`. Written only when it matters (module docs).
const FORMAT_V2: u64 = 2;

/// Whether this machine syncs, as `profiles.json` records it.
///
/// A switch that is recorded but cannot be understood is `Unreadable`, not a
/// guess: the callers treat it as off (the conservative reading for a setting
/// whose purpose is security) and turning sync on or off rewrites it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSwitch {
    /// Sync works. Also what a version-1 file or no file at all means.
    On,
    /// The person turned sync off.
    Off,
    /// A version-2 file whose `sync_off` is missing or not a boolean.
    Unreadable,
}

impl SyncSwitch {
    /// Only `On` lets a sync run.
    pub fn is_on(self) -> bool {
        self == SyncSwitch::On
    }
}

/// Everything the profile file holds.
#[derive(Debug)]
struct Contents {
    /// Keyed by profile id.
    profiles: BTreeMap<String, Profile>,
    sync: SyncSwitch,
}

/// Why a store operation failed.
///
/// No variant carries file contents: profile fields (startup commands in
/// particular) can hold secrets, and `serde_json` error text can quote them.
#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    #[error("profile store I/O error: {0}")]
    Io(#[from] io::Error),
    /// The file is not valid JSON, or not shaped like a profile store.
    #[error("profile store file is corrupt (not a valid profile list)")]
    Corrupt,
    #[error("unsupported profile store version {0} (this build reads versions 1 and 2)")]
    UnsupportedVersion(u64),
    #[error("profile store contains the same profile id twice")]
    DuplicateId,
    /// Serializing failed; in practice a `key_file` path that is not valid UTF-8.
    #[error("profiles could not be written as JSON (is a key file path not valid UTF-8?)")]
    Serialize,
    #[error("invalid profile: {0}")]
    Invalid(#[from] ProfileError),
}

/// All profiles, live and tombstoned, backed by one JSON file.
///
/// `Debug` prints every profile, startup commands included (they can hold
/// secrets): do not log a store.
#[derive(Debug)]
pub struct ProfileStore {
    path: PathBuf,
    /// Keyed by profile id.
    profiles: BTreeMap<String, Profile>,
    sync: SyncSwitch,
}

impl ProfileStore {
    /// `$XDG_CONFIG_HOME/moniker/profiles.json`, or `$HOME/.config/moniker/profiles.json`
    /// when `XDG_CONFIG_HOME` is unset, empty or not an absolute path.
    pub fn default_path() -> PathBuf {
        config_path_from(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// Load the store at `path`. A missing file gives an empty store (and
    /// writes nothing). Anything unreadable, unparseable, of another version,
    /// holding a duplicate id or an invalid profile is an error, and the file
    /// is left untouched: a damaged store never silently becomes an empty one.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let Contents { profiles, sync } = load(path)?;
        Ok(ProfileStore {
            path: path.to_path_buf(),
            profiles,
            sync,
        })
    }

    /// Re-read the file into memory, picking up changes made by another
    /// process (for example after a sync). Takes no lock: writers replace the
    /// file atomically, so this sees a complete file. On any error the
    /// in-memory snapshot is left as it was.
    pub fn reload(&mut self) -> Result<(), StoreError> {
        let Contents { profiles, sync } = load(&self.path)?;
        self.profiles = profiles;
        self.sync = sync;
        Ok(())
    }

    /// The sync switch from the in-memory snapshot (see [`SyncSwitch`]).
    pub fn sync_switch(&self) -> SyncSwitch {
        self.sync
    }

    /// Turn sync on or off and save, as a locked read-modify-write like every
    /// mutation, so the switch and the format version change in one atomic write
    /// with the profiles exactly as the file has them now. Nothing is written when
    /// the switch already says so; an `Unreadable` switch counts as a change either
    /// way and is replaced by what was asked for.
    pub fn set_sync(&mut self, on: bool) -> Result<(), StoreError> {
        let want = if on { SyncSwitch::On } else { SyncSwitch::Off };
        self.mutate(move |contents| {
            let changed = contents.sync != want;
            contents.sync = want;
            Ok((changed, ()))
        })
    }

    /// Non-deleted profiles from the in-memory snapshot, sorted by name without
    /// regard to case, then by exact name, then by id.
    pub fn live(&self) -> Vec<&Profile> {
        let mut v: Vec<&Profile> = self.profiles.values().filter(|p| !p.deleted).collect();
        v.sort_by_cached_key(|p| (p.name.to_lowercase(), p.name.clone(), p.id.clone()));
        v
    }

    /// The live profile with this id. A deleted profile is not returned; use
    /// [`all`](Self::all) to see tombstones.
    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.get(id).filter(|p| !p.deleted)
    }

    /// Every profile including tombstones, sorted by id.
    pub fn all(&self) -> Vec<Profile> {
        self.profiles.values().cloned().collect()
    }

    /// Validate `p`, stamp `updated_at`, and save it on top of the file's
    /// current contents (see the module docs).
    ///
    /// The stamp is the current time, but never less than one past the stored
    /// stamp of the same id (see `next_stamp`), so an edit always beats the
    /// version it replaces in a later merge, even if that version came from a
    /// machine whose clock runs ahead.
    ///
    /// A profile with `deleted == true` is stored as a canonical tombstone
    /// (only its id is kept), so the store never holds a tombstone with a host
    /// or commands in it.
    pub fn upsert(&mut self, p: Profile) -> Result<(), StoreError> {
        p.validate()?;
        self.mutate(move |contents| {
            let profiles = &mut contents.profiles;
            let now = next_stamp(profiles.get(&p.id));
            let p = if p.deleted {
                p.tombstone(now)
            } else {
                Profile {
                    updated_at: now,
                    ..p
                }
            };
            profiles.insert(p.id.clone(), p);
            Ok((true, ()))
        })
    }

    /// Replace the profile `id` with a tombstone and save. The tombstone is
    /// stamped like an edit (see [`upsert`](Self::upsert)): never older than the
    /// profile it deletes. Works on the file's current contents, so it also
    /// deletes a profile added by another process after this store was opened.
    /// An unknown or already-deleted id is `Ok(())` and writes no profile data.
    pub fn delete(&mut self, id: &str) -> Result<(), StoreError> {
        self.mutate(|contents| {
            let profiles = &mut contents.profiles;
            let Some(existing) = profiles.get(id).filter(|p| !p.deleted) else {
                return Ok((false, ()));
            };
            let tombstone = existing.tombstone(next_stamp(Some(existing)));
            profiles.insert(tombstone.id.clone(), tombstone);
            Ok((true, ()))
        })
    }

    /// Replace the whole store with `ps` and save. The profiles keep their
    /// `updated_at`.
    ///
    /// A sync does not use this: it merges with what is on disk through
    /// [`update`](Self::update), so an edit made by another process meanwhile is
    /// kept. Nothing in the product calls `replace_all`; it is for setting the
    /// store to a known state (tests, tools).
    ///
    /// Live profiles must be fully valid. Tombstones are not field-validated
    /// but their ids must be, and each is rewritten to the canonical tombstone
    /// (id and `updated_at` only) so a hostile peer cannot park data in one.
    /// Any invalid profile or repeated id rejects the whole set: nothing is
    /// written and the store is unchanged. The file as it is on disk must still
    /// be readable and valid, or this fails like `open` would: a damaged file is
    /// never overwritten silently.
    ///
    /// This is the one mutation that discards what is on disk, so a caller that
    /// merges must not use it (use [`update`](Self::update), which hands the
    /// closure the file's current contents under the lock).
    pub fn replace_all(&mut self, ps: Vec<Profile>) -> Result<(), StoreError> {
        let next = index(ps, true)?;
        self.mutate(move |contents| {
            contents.profiles = next;
            Ok((true, ()))
        })
    }

    /// Locked read-merge-write: `f` gets every profile in the file as it is now
    /// (tombstones included, sorted by id) and returns the complete new list plus
    /// a value handed back to the caller. Use it to merge with what is on disk
    /// without losing a write another process made since this store was read.
    ///
    /// The returned list is checked exactly like [`replace_all`](Self::replace_all):
    /// live profiles fully validated, tombstone ids validated and the tombstones
    /// normalized, repeated ids rejected, `updated_at` kept. If `f` fails or the
    /// list is rejected, nothing is written and the snapshot is unchanged. A list
    /// equal to what is on disk is not rewritten.
    ///
    /// `f` runs while the store lock is held, so it must not mutate this store
    /// (from any handle, in this process or another); that would deadlock.
    ///
    /// The sync switch is not `f`'s to change: it stays as the file has it.
    pub fn update<R>(
        &mut self,
        f: impl FnOnce(&[Profile]) -> Result<(Vec<Profile>, R), StoreError>,
    ) -> Result<R, StoreError> {
        self.mutate(move |contents| {
            let current: Vec<Profile> = contents.profiles.values().cloned().collect();
            let (next, out) = f(&current)?;
            let next = index(next, true)?;
            let changed = next != contents.profiles;
            contents.profiles = next;
            Ok((changed, out))
        })
    }

    /// The path of the profile file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The locked read-modify-write behind every mutation. `change` gets the
    /// file's current contents (profiles and switch) and returns whether it
    /// changed them (`false` skips the write) and a value for the caller. The
    /// snapshot is replaced only once everything worked.
    fn mutate<R>(
        &mut self,
        change: impl FnOnce(&mut Contents) -> Result<(bool, R), StoreError>,
    ) -> Result<R, StoreError> {
        ensure_parent_dir(&self.path)?;
        let _lock = lock_exclusive(&self.path)?;
        let mut fresh = load(&self.path)?;
        let (changed, out) = change(&mut fresh)?;
        if changed {
            // A switch nobody could read is written as off, so off is what the
            // file says from now on and what the snapshot must say too.
            if fresh.sync == SyncSwitch::Unreadable {
                fresh.sync = SyncSwitch::Off;
            }
            write_atomic(&self.path, &encode(&fresh)?)?;
        }
        self.profiles = fresh.profiles;
        self.sync = fresh.sync;
        Ok(out)
    }
}

/// The `updated_at` for an edit of the entry now stored as `existing`: the
/// current time, but at least one past the stored stamp.
///
/// Merging keeps the newest `updated_at`. A profile that arrived from a machine
/// whose clock runs ahead has a stamp in this machine's future, and an edit
/// stamped with the plain clock would be older than the version it replaces:
/// the next sync would bring the old version back. A peer could also send
/// `i64::MAX` to pin a profile. Stamps therefore only ever go up. At `i64::MAX`
/// the stamp saturates, and a tombstone wins a tie on `updated_at` in
/// [`merge`](crate::bundle::merge), so a pinned profile can still be deleted.
fn next_stamp(existing: Option<&Profile>) -> i64 {
    let floor = existing.map_or(i64::MIN, |old| old.updated_at.saturating_add(1));
    now_ms().max(floor)
}

/// Read the file into validated contents. A missing file is an empty store
/// with sync on; any other problem is an error.
fn load(path: &Path) -> Result<Contents, StoreError> {
    match fs::read(path) {
        Ok(bytes) => {
            let (profiles, sync) = parse(&bytes)?;
            Ok(Contents {
                profiles: index(profiles, false)?,
                sync,
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Contents {
            profiles: BTreeMap::new(),
            sync: SyncSwitch::On,
        }),
        Err(e) => Err(e.into()),
    }
}

/// Validate every profile and key them by id. With `normalize_tombstones`,
/// each tombstone is rewritten to `Profile::tombstone(updated_at)`.
fn index(
    ps: Vec<Profile>,
    normalize_tombstones: bool,
) -> Result<BTreeMap<String, Profile>, StoreError> {
    let mut map = BTreeMap::new();
    for p in ps {
        // For a tombstone this checks the id only.
        p.validate()?;
        let p = if p.deleted && normalize_tombstones {
            p.tombstone(p.updated_at)
        } else {
            p
        };
        if map.insert(p.id.clone(), p).is_some() {
            return Err(StoreError::DuplicateId);
        }
    }
    Ok(map)
}

/// Parse the file. The version is checked before anything else is looked at,
/// so a newer format reports as unsupported rather than corrupt, and `sync_off`
/// is read in version 2 only (version 1 has no switch and reads as on).
fn parse(bytes: &[u8]) -> Result<(Vec<Profile>, SyncSwitch), StoreError> {
    // Error values from serde_json are dropped on purpose: they can quote file contents.
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| StoreError::Corrupt)?;
    let serde_json::Value::Object(mut file) = value else {
        return Err(StoreError::Corrupt);
    };
    let sync = match file.get("version").and_then(serde_json::Value::as_u64) {
        Some(FORMAT_V1) => SyncSwitch::On,
        Some(FORMAT_V2) => match file.get("sync_off") {
            Some(serde_json::Value::Bool(true)) => SyncSwitch::Off,
            Some(serde_json::Value::Bool(false)) => SyncSwitch::On,
            // Missing or not a boolean: not understood, not guessed at.
            _ => SyncSwitch::Unreadable,
        },
        Some(other) => return Err(StoreError::UnsupportedVersion(other)),
        None => return Err(StoreError::Corrupt),
    };
    let profiles = file.remove("profiles").ok_or(StoreError::Corrupt)?;
    let profiles = serde_json::from_value(profiles).map_err(|_| StoreError::Corrupt)?;
    Ok((profiles, sync))
}

/// The file's bytes: pretty-printed, profiles in id order, trailing newline.
///
/// Version 2 exactly when the switch is not on or a profile is kept on this
/// machine, else version 1 (module docs). An `Unreadable` switch is written as
/// off: whatever rewrites the file must not turn a switch nobody could read into
/// on.
fn encode(contents: &Contents) -> Result<Vec<u8>, StoreError> {
    #[derive(Serialize)]
    struct File<'a> {
        version: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        sync_off: Option<bool>,
        profiles: Vec<&'a Profile>,
    }
    let v2 = !contents.sync.is_on() || contents.profiles.values().any(|p| p.local_only);
    let file = File {
        version: if v2 { FORMAT_V2 } else { FORMAT_V1 },
        sync_off: v2.then_some(!contents.sync.is_on()),
        profiles: contents.profiles.values().collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&file).map_err(|_| StoreError::Serialize)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The directory that holds `path` (`.` for a bare file name).
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Create the directory that holds `path` with mode 0700 if it does not exist
/// (and any missing ancestors); an existing directory is left as it is.
pub(crate) fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent_dir(path))
}

/// `path` with `suffix` appended to its file name (`profiles.json` -> `profiles.json.tmp`).
fn sibling(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "store path has no file name"))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Take the exclusive advisory lock for the store at `path`, blocking until it
/// is free. The lock lives on `<file>.lock` (created with mode 0600, never
/// truncated or written) rather than on the data file, which is replaced by
/// rename. It is released when the returned handle is dropped. The parent
/// directory must exist.
pub(crate) fn lock_exclusive(path: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(sibling(path, ".lock")?)?;
    lock.lock()?;
    Ok(lock)
}

/// Replace `path` with `bytes` so readers see the old file or the new one,
/// never a partial one. The parent directory must exist.
///
/// The temp file `<name>.tmp` beside the target is created exclusively with
/// mode 0600 (a leftover one from a crashed save is removed first; a symlink
/// there is unlinked, not followed), fsynced, then renamed over the target. It
/// is removed again if anything after its creation fails. Callers hold the
/// lock of the file they write ([`lock_exclusive`]), so nothing else writes the
/// temp file meanwhile.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = sibling(path, ".tmp")?;
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `<xdg>/moniker/profiles.json` if `xdg` is a non-empty absolute path (the XDG
/// spec says to ignore empty and relative values), else
/// `<home>/.config/moniker/profiles.json`. With neither, the path is relative
/// to the working directory (`.config/moniker/profiles.json`).
fn config_path_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    let base = match (xdg, home) {
        (Some(x), _) if Path::new(x).is_absolute() => PathBuf::from(x),
        (_, Some(h)) if !h.is_empty() => Path::new(h).join(".config"),
        _ => PathBuf::from(".config"),
    };
    base.join("moniker").join("profiles.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{StartupSequence, now_ms};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Barrier;
    use tempfile::TempDir;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A canonical lowercase hyphenated UUID, distinct per `n`.
    fn pid(n: u32) -> String {
        format!("00000000-0000-4000-8000-{n:012x}")
    }

    fn profile(n: u32, name: &str) -> Profile {
        let mut p = Profile::new(name, "example.com", "alice");
        p.id = pid(n);
        p
    }

    fn setup() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        (dir, path)
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn ids_on_disk(path: &Path) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn assert_is_canonical_tombstone(p: &Profile, id: &str, updated_at: i64) {
        assert_eq!(*p, profile(0, "x").tombstone(updated_at).with_id(id));
    }

    trait WithId {
        fn with_id(self, id: &str) -> Self;
    }
    impl WithId for Profile {
        fn with_id(mut self, id: &str) -> Self {
            self.id = id.to_string();
            self
        }
    }

    // ----- the brief's named tests -----

    #[test]
    fn missing_file_opens_empty() {
        let (d, path) = setup();
        let s = ProfileStore::open(&path).unwrap();
        assert!(s.live().is_empty());
        assert!(s.all().is_empty());
        // Opening never writes.
        assert!(dir_entries(d.path()).is_empty());
    }

    #[test]
    fn upsert_persists_and_reloads_equal() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut p = Profile::new("Prod", "example.com", "alice");
        p.port = 2222;
        p.key_file = Some(PathBuf::from("/home/alice/.ssh/id_ed25519"));
        p.transcript = true;
        p.sequences.push(StartupSequence {
            name: "update".to_string(),
            commands: vec!["sudo apt update".to_string(), String::new()],
        });
        s.upsert(p.clone()).unwrap();

        let reloaded = ProfileStore::open(&path).unwrap();
        assert_eq!(reloaded.all(), s.all());
        let stored = reloaded.get(&p.id).unwrap();
        let mut expected = p;
        expected.updated_at = stored.updated_at;
        assert_eq!(*stored, expected);
    }

    #[test]
    fn file_is_0600_and_dir_is_0700() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg").join("moniker");
        let path = cfg.join("profiles.json");
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&cfg.join("profiles.json.lock")), 0o600);
        assert_eq!(mode(&cfg), 0o700);
    }

    #[test]
    fn invalid_profile_is_rejected_and_not_saved() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut bad = profile(1, "A");
        bad.host = "-oProxyCommand=x".to_string();
        let err = s.upsert(bad).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ProfileError::Host)));
        assert!(s.all().is_empty());
        assert!(dir_entries(d.path()).is_empty(), "nothing may be written");

        // With an existing file: it stays byte-for-byte the same.
        s.upsert(profile(2, "B")).unwrap();
        let before = fs::read(&path).unwrap();
        let mut bad = profile(3, "C");
        bad.port = 0;
        assert!(matches!(
            s.upsert(bad),
            Err(StoreError::Invalid(ProfileError::Port))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(s.all().len(), 1);
    }

    #[test]
    fn delete_leaves_tombstone() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut p = profile(1, "A");
        p.sequences.push(StartupSequence {
            name: "x".to_string(),
            commands: vec!["secret-ish".to_string()],
        });
        s.upsert(p).unwrap();
        s.upsert(profile(2, "B")).unwrap();

        let before = now_ms();
        s.delete(&pid(1)).unwrap();
        let after = now_ms();

        assert_eq!(s.live().len(), 1);
        assert_eq!(s.live()[0].id, pid(2));
        assert!(s.get(&pid(1)).is_none(), "get returns live profiles only");
        let all = s.all();
        assert_eq!(all.len(), 2);
        let t = all.iter().find(|p| p.id == pid(1)).unwrap();
        assert!(t.deleted);
        // A stamp is at least one past the previous edit of the same profile, so
        // it can be one millisecond beyond the clock when edits come that fast.
        assert!(t.updated_at >= before && t.updated_at <= after + 1);
        assert_is_canonical_tombstone(t, &pid(1), t.updated_at);

        // It persists.
        let reloaded = ProfileStore::open(&path).unwrap();
        assert_eq!(reloaded.all(), all);
        assert_eq!(reloaded.live().len(), 1);
    }

    #[test]
    fn corrupt_json_is_an_error_and_file_untouched() {
        for garbage in [
            "",
            "not json",
            "{",
            "[]",
            "{}",
            r#"{"version":1}"#,
            r#"{"version":1,"profiles":{}}"#,
            r#"{"version":"1","profiles":[]}"#,
            r#"{"version":1,"profiles":[{"id":"x"}]}"#,
            "\u{0}\u{1}\u{2}",
        ] {
            let (_d, path) = setup();
            fs::write(&path, garbage).unwrap();
            let err = ProfileStore::open(&path).unwrap_err();
            assert!(
                matches!(err, StoreError::Corrupt),
                "input {garbage:?} gave {err:?}"
            );
            assert_eq!(fs::read(&path).unwrap(), garbage.as_bytes());
        }
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        s.upsert(profile(2, "B")).unwrap();
        s.delete(&pid(1)).unwrap();
        s.replace_all(vec![profile(3, "C")]).unwrap();
        // The data file and its lock file, nothing else.
        assert_eq!(
            dir_entries(d.path()),
            vec!["profiles.json", "profiles.json.lock"]
        );
    }

    // ----- rulings -----

    #[test]
    fn corrupt_error_carries_no_file_contents() {
        let (_d, path) = setup();
        fs::write(
            &path,
            r#"{"version":1,"profiles":[{"id":"hunter2","port":"hunter2"}]}"#,
        )
        .unwrap();
        let err = ProfileStore::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt));
        assert!(!format!("{err} {err:?}").contains("hunter2"));
    }

    #[test]
    fn open_rejects_invalid_live_profile_and_leaves_file() {
        let (_d, path) = setup();
        let text = format!(
            r#"{{"version":1,"profiles":[{{"id":"{}","name":"x","host":"-oProxyCommand=x","port":22,"username":"alice","key_file":null,"sequences":[],"transcript":false,"updated_at":1,"deleted":false}}]}}"#,
            pid(1)
        );
        fs::write(&path, &text).unwrap();
        let err = ProfileStore::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ProfileError::Host)));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn open_rejects_bad_id_even_on_a_tombstone() {
        let (_d, path) = setup();
        let text = r#"{"version":1,"profiles":[{"id":"../x","name":"","host":"","port":0,"username":"","key_file":null,"sequences":[],"transcript":false,"updated_at":1,"deleted":true}]}"#;
        fs::write(&path, text).unwrap();
        let err = ProfileStore::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ProfileError::Id)));
    }

    #[test]
    fn open_rejects_wrong_version() {
        for (text, want) in [
            (r#"{"version":3,"profiles":[]}"#, 3),
            (r#"{"version":0,"profiles":[]}"#, 0),
            // A future format need not even have a compatible profile list.
            (r#"{"version":7,"profiles":"whatever"}"#, 7),
        ] {
            let (_d, path) = setup();
            fs::write(&path, text).unwrap();
            let err = ProfileStore::open(&path).unwrap_err();
            assert!(
                matches!(err, StoreError::UnsupportedVersion(v) if v == want),
                "input {text:?} gave {err:?}"
            );
            assert!(
                err.to_string().contains("versions 1 and 2"),
                "the message names what this build reads: {err}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    // ----- format version 2: the sync switch and profiles kept on this machine -----
    //
    // Older versions refuse a file of another version and leave it alone, so a
    // file that holds the switch off or a profile kept here is version 2: no
    // older `moniker-server` on this machine can serve it. Otherwise the file is
    // version 1, byte for byte what it always was.

    fn file_text(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn file_json(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn nothing_kept_here_stays_version_1() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(!text.contains("sync_off"), "{text}");
        assert!(!text.contains("local_only"), "{text}");
        assert_eq!(s.sync_switch(), SyncSwitch::On);
        assert!(s.sync_switch().is_on());
    }

    #[test]
    fn a_profile_kept_here_makes_version_2() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "syncs")).unwrap();
        s.upsert(Profile {
            local_only: true,
            ..profile(2, "stays")
        })
        .unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"sync_off\": false"), "{text}");
        assert!(text.contains("\"local_only\": true"), "{text}");
        let value = file_json(&path);
        let profiles = value["profiles"].as_array().unwrap();
        assert!(
            profiles[0].get("local_only").is_none(),
            "only the exception"
        );
        assert_eq!(profiles[1]["local_only"], true);

        let reopened = ProfileStore::open(&path).unwrap();
        assert!(reopened.get(&pid(2)).unwrap().local_only);
        assert!(!reopened.get(&pid(1)).unwrap().local_only);
        assert_eq!(reopened.sync_switch(), SyncSwitch::On);

        // Back to nothing kept here: the file goes back to version 1.
        s.upsert(Profile {
            local_only: false,
            ..profile(2, "stays")
        })
        .unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(!text.contains("sync_off") && !text.contains("local_only"));
    }

    #[test]
    fn turning_sync_off_and_on_moves_the_version() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        s.upsert(profile(2, "B")).unwrap();
        let profiles = s.all();

        s.set_sync(false).unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"sync_off\": true"), "{text}");
        assert_eq!(s.sync_switch(), SyncSwitch::Off);
        assert!(!s.sync_switch().is_on());
        assert_eq!(s.all(), profiles, "the profiles are as they were");
        let reopened = ProfileStore::open(&path).unwrap();
        assert_eq!(reopened.sync_switch(), SyncSwitch::Off);
        assert_eq!(reopened.all(), profiles);
        assert_eq!(mode(&path), 0o600);

        s.set_sync(true).unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(!text.contains("sync_off"), "{text}");
        assert_eq!(s.sync_switch(), SyncSwitch::On);
        assert_eq!(s.all(), profiles, "the profiles are as they were");
        let reopened = ProfileStore::open(&path).unwrap();
        assert_eq!(reopened.sync_switch(), SyncSwitch::On);
        assert_eq!(reopened.all(), profiles);
    }

    #[test]
    fn turning_sync_on_keeps_version_2_while_a_profile_is_kept_here() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(Profile {
            local_only: true,
            ..profile(1, "stays")
        })
        .unwrap();
        s.set_sync(false).unwrap();
        s.set_sync(true).unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"sync_off\": false"), "{text}");
        assert_eq!(s.sync_switch(), SyncSwitch::On);
    }

    #[test]
    fn a_switch_not_understood_reads_as_unreadable() {
        for text in [
            r#"{"version":2,"profiles":[]}"#,
            r#"{"version":2,"sync_off":"no","profiles":[]}"#,
            r#"{"version":2,"sync_off":null,"profiles":[]}"#,
            r#"{"version":2,"sync_off":0,"profiles":[]}"#,
            r#"{"version":2,"sync_off":[false],"profiles":[]}"#,
        ] {
            let (_d, path) = setup();
            fs::write(&path, text).unwrap();
            let s = ProfileStore::open(&path).expect(text);
            assert_eq!(s.sync_switch(), SyncSwitch::Unreadable, "{text}");
            assert!(!s.sync_switch().is_on(), "{text}");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                text,
                "open writes nothing"
            );
        }
    }

    #[test]
    fn a_switch_that_is_understood_reads_as_it_says() {
        for (text, want) in [
            (
                r#"{"version":2,"sync_off":true,"profiles":[]}"#,
                SyncSwitch::Off,
            ),
            (
                r#"{"version":2,"sync_off":false,"profiles":[]}"#,
                SyncSwitch::On,
            ),
        ] {
            let (_d, path) = setup();
            fs::write(&path, text).unwrap();
            assert_eq!(ProfileStore::open(&path).unwrap().sync_switch(), want);
        }
    }

    #[test]
    fn an_unreadable_switch_is_written_as_off() {
        let (_d, path) = setup();
        fs::write(&path, r#"{"version":2,"sync_off":"no","profiles":[]}"#).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        let text = file_text(&path);
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"sync_off\": true"), "{text}");
        assert_eq!(
            s.sync_switch(),
            SyncSwitch::Off,
            "the snapshot says what the file says"
        );
        assert_eq!(
            ProfileStore::open(&path).unwrap().sync_switch(),
            SyncSwitch::Off
        );
    }

    #[test]
    fn turning_sync_off_or_on_repairs_an_unreadable_switch() {
        let broken = r#"{"version":2,"sync_off":"no","profiles":[]}"#;
        let (_d, path) = setup();
        fs::write(&path, broken).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.set_sync(false).unwrap();
        assert_eq!(s.sync_switch(), SyncSwitch::Off);
        assert_eq!(file_json(&path)["sync_off"], true);

        fs::write(&path, broken).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        // Unreadable to On is a change, so it is written.
        s.set_sync(true).unwrap();
        assert_eq!(s.sync_switch(), SyncSwitch::On);
        let text = file_text(&path);
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(!text.contains("sync_off"), "{text}");
    }

    #[test]
    fn sync_off_on_a_fresh_machine_writes_an_empty_v2_file() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        assert!(!path.exists());
        s.set_sync(false).unwrap();
        let value = file_json(&path);
        assert_eq!(value["version"], 2);
        assert_eq!(value["sync_off"], true);
        assert_eq!(value["profiles"], serde_json::json!([]));
        assert_eq!(s.sync_switch(), SyncSwitch::Off);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_v1_file_reads_as_sync_on() {
        let (_d, path) = setup();
        fs::write(&path, r#"{"version":1,"profiles":[]}"#).unwrap();
        let s = ProfileStore::open(&path).unwrap();
        assert_eq!(s.sync_switch(), SyncSwitch::On);
        assert!(s.sync_switch().is_on());

        // The switch is read in version 2 only: a version-1 file that happens to
        // carry the key is still a version-1 file.
        fs::write(&path, r#"{"version":1,"sync_off":true,"profiles":[]}"#).unwrap();
        assert_eq!(
            ProfileStore::open(&path).unwrap().sync_switch(),
            SyncSwitch::On
        );

        // And no file at all is on too.
        let (_d, path) = setup();
        assert_eq!(
            ProfileStore::open(&path).unwrap().sync_switch(),
            SyncSwitch::On
        );
    }

    #[test]
    fn a_switch_that_does_not_change_is_not_written() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.set_sync(true).unwrap();
        assert!(!path.exists(), "on to on writes no file");
        assert!(!d.path().join("profiles.json.tmp").exists());

        // A compact hand-written file: any rewrite would pretty-print it.
        let text = r#"{"version":2,"sync_off":true,"profiles":[]}"#;
        fs::write(&path, text).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.set_sync(false).unwrap();
        assert_eq!(file_text(&path), text);

        let v1 = r#"{"version":1,"profiles":[]}"#;
        fs::write(&path, v1).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.set_sync(true).unwrap();
        assert_eq!(file_text(&path), v1);
    }

    #[test]
    fn the_switch_survives_other_mutations_and_stale_snapshots() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        b.set_sync(false).unwrap();
        assert_eq!(a.sync_switch(), SyncSwitch::On, "A's snapshot is stale");

        // A mutation starts from the file, not from A's snapshot, so it keeps the
        // switch B turned off.
        a.upsert(profile(1, "A")).unwrap();
        assert_eq!(a.sync_switch(), SyncSwitch::Off);
        assert_eq!(file_json(&path)["sync_off"], true);

        a.delete(&pid(1)).unwrap();
        assert_eq!(file_json(&path)["sync_off"], true);
        a.replace_all(vec![profile(2, "B")]).unwrap();
        assert_eq!(file_json(&path)["sync_off"], true);
        a.update(|fresh| Ok((fresh.to_vec(), ()))).unwrap();
        a.update(|fresh| {
            let mut next = fresh.to_vec();
            next.push(profile(3, "C"));
            Ok((next, ()))
        })
        .unwrap();
        let value = file_json(&path);
        assert_eq!(value["version"], 2);
        assert_eq!(value["sync_off"], true);
        assert_eq!(value["profiles"].as_array().unwrap().len(), 2);
        assert_eq!(a.sync_switch(), SyncSwitch::Off);
    }

    #[test]
    fn reload_picks_up_the_switch() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        b.set_sync(false).unwrap();
        assert_eq!(a.sync_switch(), SyncSwitch::On);
        a.reload().unwrap();
        assert_eq!(a.sync_switch(), SyncSwitch::Off);
        b.set_sync(true).unwrap();
        a.reload().unwrap();
        assert_eq!(a.sync_switch(), SyncSwitch::On);

        // A reload that fails keeps the switch as it was, like the profiles.
        b.set_sync(false).unwrap();
        a.reload().unwrap();
        fs::write(&path, "garbage").unwrap();
        assert!(matches!(a.reload(), Err(StoreError::Corrupt)));
        assert_eq!(a.sync_switch(), SyncSwitch::Off);
    }

    #[test]
    fn a_failed_switch_change_leaves_the_snapshot_and_the_file() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        fs::write(&path, "garbage").unwrap();
        assert!(matches!(s.set_sync(false), Err(StoreError::Corrupt)));
        assert_eq!(s.sync_switch(), SyncSwitch::On);
        assert_eq!(fs::read_to_string(&path).unwrap(), "garbage");
    }

    #[test]
    fn open_rejects_duplicate_ids() {
        let (_d, path) = setup();
        let one = format!(
            r#"{{"id":"{}","name":"x","host":"example.com","port":22,"username":"alice","key_file":null,"sequences":[],"transcript":false,"updated_at":1,"deleted":false}}"#,
            pid(1)
        );
        let text = format!(r#"{{"version":1,"profiles":[{one},{one}]}}"#);
        fs::write(&path, &text).unwrap();
        let err = ProfileStore::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::DuplicateId));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn open_accepts_a_compact_hand_written_file() {
        let (_d, path) = setup();
        let text = format!(
            r#"{{"version":1,"profiles":[{{"id":"{}","name":"Box","host":"10.0.0.5","port":22,"username":"root","key_file":null,"sequences":[],"transcript":false,"updated_at":5,"deleted":false}}]}}"#,
            pid(9)
        );
        fs::write(&path, text).unwrap();
        let s = ProfileStore::open(&path).unwrap();
        assert_eq!(s.live().len(), 1);
        assert_eq!(s.get(&pid(9)).unwrap().host, "10.0.0.5");
    }

    #[test]
    fn on_disk_format_is_exact() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let p = Profile {
            id: pid(1),
            name: "Prod web".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "alice".to_string(),
            key_file: None,
            sequences: vec![StartupSequence {
                name: "update".to_string(),
                commands: vec!["apt update".to_string()],
            }],
            transcript: false,
            local_only: false,
            updated_at: 1_700_000_000_000,
            deleted: false,
        };
        s.replace_all(vec![p]).unwrap();
        let expected = r#"{
  "version": 1,
  "profiles": [
    {
      "id": "00000000-0000-4000-8000-000000000001",
      "name": "Prod web",
      "host": "example.com",
      "port": 22,
      "username": "alice",
      "key_file": null,
      "sequences": [
        {
          "name": "update",
          "commands": [
            "apt update"
          ]
        }
      ],
      "transcript": false,
      "updated_at": 1700000000000,
      "deleted": false
    }
  ]
}
"#;
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn profiles_are_sorted_by_id_on_disk() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.replace_all(vec![profile(3, "c"), profile(1, "z"), profile(2, "a")])
            .unwrap();
        assert_eq!(ids_on_disk(&path), vec![pid(1), pid(2), pid(3)]);
        s.upsert(profile(0, "new")).unwrap();
        assert_eq!(ids_on_disk(&path), vec![pid(0), pid(1), pid(2), pid(3)]);
    }

    #[test]
    fn live_is_sorted_by_name_and_get_finds_live_only() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "charlie")).unwrap();
        s.upsert(profile(2, "alpha")).unwrap();
        s.upsert(profile(3, "bravo")).unwrap();
        let names: Vec<&str> = s.live().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
        assert_eq!(s.get(&pid(3)).unwrap().name, "bravo");
        assert!(s.get(&pid(99)).is_none());
    }

    #[test]
    fn upsert_stamps_updated_at_and_replaces_by_id() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut p = profile(1, "A");
        p.updated_at = 1;
        let before = now_ms();
        s.upsert(p.clone()).unwrap();
        let after = now_ms();
        let t = s.get(&pid(1)).unwrap().updated_at;
        assert!(t >= before && t <= after, "updated_at {t} not in range");

        p.name = "A renamed".to_string();
        s.upsert(p).unwrap();
        assert_eq!(s.all().len(), 1);
        assert_eq!(s.get(&pid(1)).unwrap().name, "A renamed");
    }

    #[test]
    fn upsert_over_a_tombstone_brings_the_profile_back() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        s.delete(&pid(1)).unwrap();
        s.upsert(profile(1, "A again")).unwrap();
        assert_eq!(s.live().len(), 1);
        assert_eq!(s.get(&pid(1)).unwrap().name, "A again");
    }

    #[test]
    fn upsert_of_a_deleted_profile_stores_a_canonical_tombstone() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut p = profile(1, "A");
        p.deleted = true; // but still carries host, name, ...
        s.upsert(p).unwrap();
        assert!(s.live().is_empty());
        let all = s.all();
        assert_eq!(all.len(), 1);
        assert_is_canonical_tombstone(&all[0], &pid(1), all[0].updated_at);
    }

    // ----- monotonic updated_at -----
    //
    // A profile that arrived from a machine whose clock runs ahead has a stamp
    // in this machine's future. A local edit stamped with the plain clock would
    // lose the next merge and be reverted by the sync, so a stamp is always at
    // least one past the stored one. (A peer could also send i64::MAX.)

    /// Store `p` with exactly its own `updated_at` (as a sync would).
    fn store_as_synced(s: &mut ProfileStore, p: Profile) {
        let mut all = s.all();
        all.retain(|q| q.id != p.id);
        all.push(p);
        s.replace_all(all).unwrap();
    }

    #[test]
    fn upsert_after_a_fast_clock_peer_gets_a_strictly_larger_stamp() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let future = now_ms() + 3_600_000;
        let mut p = profile(1, "from the future");
        p.updated_at = future;
        store_as_synced(&mut s, p.clone());

        s.upsert(Profile {
            name: "edited here".to_string(),
            ..p.clone()
        })
        .unwrap();
        let stamp = s.get(&pid(1)).unwrap().updated_at;
        assert!(stamp > future, "{stamp} must be newer than {future}");
        assert_eq!(stamp, future + 1, "one past, not far ahead");
        // And that is what reached the disk.
        let reloaded = ProfileStore::open(&path).unwrap();
        assert_eq!(reloaded.get(&pid(1)).unwrap().updated_at, future + 1);

        // Every further edit keeps climbing, even within the same millisecond.
        s.upsert(Profile {
            name: "edited again".to_string(),
            ..p
        })
        .unwrap();
        assert_eq!(s.get(&pid(1)).unwrap().updated_at, future + 2);
    }

    #[test]
    fn delete_after_a_fast_clock_peer_gets_a_strictly_larger_stamp() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let future = now_ms() + 3_600_000;
        let mut p = profile(1, "from the future");
        p.updated_at = future;
        store_as_synced(&mut s, p);

        s.delete(&pid(1)).unwrap();
        let all = s.all();
        assert_eq!(all.len(), 1);
        assert!(all[0].deleted);
        assert_eq!(all[0].updated_at, future + 1);
        assert_is_canonical_tombstone(&all[0], &pid(1), future + 1);
    }

    #[test]
    fn upserting_over_a_fast_clock_tombstone_wins_over_it() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let future = now_ms() + 3_600_000;
        store_as_synced(&mut s, profile(1, "x").tombstone(future));

        // Bringing a deleted profile back must beat the tombstone that deleted it...
        s.upsert(profile(1, "back")).unwrap();
        assert_eq!(s.get(&pid(1)).unwrap().updated_at, future + 1);
        // ...and deleting it again (as an upsert of a deleted profile) beats that.
        let mut gone = profile(1, "back");
        gone.deleted = true;
        s.upsert(gone).unwrap();
        assert_eq!(s.all()[0].updated_at, future + 2);
        assert!(s.all()[0].deleted);
    }

    #[test]
    fn a_peer_cannot_pin_a_profile_with_i64_max() {
        use crate::bundle::{BundleEntry, merge};
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut pinned = profile(1, "pinned");
        pinned.updated_at = i64::MAX;
        store_as_synced(&mut s, pinned.clone());

        // An edit saturates instead of overflowing.
        s.upsert(Profile {
            name: "edited".to_string(),
            ..pinned
        })
        .unwrap();
        assert_eq!(s.get(&pid(1)).unwrap().updated_at, i64::MAX);

        // A delete does too, and a tombstone wins a tie on updated_at, so the
        // user can still get rid of a pinned profile.
        let live = s.get(&pid(1)).unwrap().clone();
        s.delete(&pid(1)).unwrap();
        let tomb = s.all().remove(0);
        assert!(tomb.deleted);
        assert_eq!(tomb.updated_at, i64::MAX);
        let entry = |profile: Profile| BundleEntry {
            profile,
            password: None,
        };
        for (local, remote) in [
            (entry(tomb.clone()), entry(live.clone())),
            (entry(live), entry(tomb.clone())),
        ] {
            let merged = merge(vec![local], vec![remote]);
            assert_eq!(merged.len(), 1);
            assert!(merged[0].profile.deleted, "the tombstone must win the tie");
        }
    }

    #[test]
    fn stamps_in_the_normal_case_are_the_current_time() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        // An old stored stamp is no obstacle: the clock is used.
        let mut old = profile(1, "old");
        old.updated_at = 5;
        store_as_synced(&mut s, old.clone());

        let before = now_ms();
        s.upsert(old).unwrap();
        let after = now_ms();
        let stamp = s.get(&pid(1)).unwrap().updated_at;
        assert!(stamp >= before && stamp <= after, "{stamp} not in range");

        let before = now_ms();
        s.delete(&pid(1)).unwrap();
        let after = now_ms();
        let stamp = s.all()[0].updated_at;
        // At most one past the clock when the upsert above was in the same millisecond.
        assert!(
            stamp >= before && stamp <= after + 1,
            "{stamp} not in range"
        );
    }

    // ----- control characters that arrive from a peer or a hand edit -----

    /// Profiles that each break one of the control-character rules.
    fn control_char_offenders() -> Vec<(Profile, ProfileError)> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut nul_name = profile(3, "x");
        nul_name.name = "a\0b".to_string();
        let mut esc_name = profile(3, "x");
        esc_name.name = "a\x1b[2Jb".to_string();
        let mut seq_name = profile(3, "x");
        seq_name.sequences.push(StartupSequence {
            name: "up\0date".to_string(),
            commands: vec!["ls".to_string()],
        });
        let mut key_file = profile(3, "x");
        key_file.key_file = Some(PathBuf::from(OsStr::from_bytes(b"/k/id\0x")));
        vec![
            (nul_name, ProfileError::Name),
            (esc_name, ProfileError::Name),
            (seq_name, ProfileError::SequenceName),
            (key_file, ProfileError::KeyFile),
        ]
    }

    #[test]
    fn replace_all_rejects_the_whole_set_if_a_synced_profile_has_control_characters() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "keep")).unwrap();
        let before_file = fs::read(&path).unwrap();
        let before_all = s.all();

        for (bad, want) in control_char_offenders() {
            let err = s
                .replace_all(vec![profile(2, "fine"), bad, profile(4, "also fine")])
                .unwrap_err();
            assert!(
                matches!(&err, StoreError::Invalid(e) if *e == want),
                "wanted {want:?}, got {err:?}"
            );
            assert_eq!(s.all(), before_all, "nothing applied in memory");
            assert_eq!(fs::read(&path).unwrap(), before_file, "nothing written");
        }
    }

    #[test]
    fn update_rejects_the_whole_set_if_a_synced_profile_has_control_characters() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "keep")).unwrap();
        let before_file = fs::read(&path).unwrap();
        let before_all = s.all();

        for (bad, want) in control_char_offenders() {
            let err = s
                .update(|current| {
                    let mut next = current.to_vec();
                    next.push(profile(2, "fine"));
                    next.push(bad);
                    Ok((next, ()))
                })
                .unwrap_err();
            assert!(
                matches!(&err, StoreError::Invalid(e) if *e == want),
                "wanted {want:?}, got {err:?}"
            );
            assert_eq!(s.all(), before_all, "nothing applied in memory");
            assert_eq!(fs::read(&path).unwrap(), before_file, "nothing written");
        }
    }

    #[test]
    fn upsert_rejects_control_characters_too() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        for (bad, want) in control_char_offenders() {
            let err = s.upsert(bad).unwrap_err();
            assert!(
                matches!(&err, StoreError::Invalid(e) if *e == want),
                "{err:?}"
            );
        }
        assert!(s.all().is_empty());
        assert!(dir_entries(d.path()).is_empty(), "nothing may be written");
    }

    #[test]
    fn open_rejects_a_hand_edited_name_with_a_nul_and_leaves_the_file() {
        let (_d, path) = setup();
        // `\u0000` is JSON's spelling of NUL; it passes serde and must not pass us.
        let text = format!(
            r#"{{"version":1,"profiles":[{{"id":"{}","name":"a\u0000b","host":"example.com","port":22,"username":"alice","key_file":null,"sequences":[],"transcript":false,"updated_at":1,"deleted":false}}]}}"#,
            pid(1)
        );
        fs::write(&path, &text).unwrap();
        let err = ProfileStore::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ProfileError::Name)));
        assert_eq!(fs::read_to_string(&path).unwrap(), text, "file untouched");

        // The message is plain text a person can act on, and carries no NUL.
        let shown = err.to_string();
        assert!(
            shown.contains("name") && shown.contains("control"),
            "{shown}"
        );
        assert!(!shown.contains('\0'), "{shown:?}");

        // The same for a sequence name and a key file path.
        for (field, want) in [
            (
                r#""name":"x","sequences":[{"name":"a\u001bb","commands":["ls"]}]"#,
                "sequence",
            ),
            (r#""name":"x","key_file":"/k/\u0000id""#, "key file"),
        ] {
            let mut body = format!(
                r#"{{"version":1,"profiles":[{{"id":"{}","host":"example.com","port":22,"username":"alice","transcript":false,"updated_at":1,"deleted":false,{field}"#,
                pid(1)
            );
            // Fill in the fields the case does not set.
            if !field.contains("sequences") {
                body.push_str(r#","sequences":[]"#);
            }
            if !field.contains("key_file") {
                body.push_str(r#","key_file":null"#);
            }
            body.push_str("}]}");
            fs::write(&path, &body).unwrap();
            let err = ProfileStore::open(&path).unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
            assert!(err.to_string().contains(want), "{err}");
            assert_eq!(fs::read_to_string(&path).unwrap(), body, "file untouched");
        }
    }

    #[test]
    fn delete_of_unknown_id_is_ok_and_writes_nothing() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.delete(&pid(1)).unwrap();
        s.delete("../not-even-an-id").unwrap();
        assert!(s.all().is_empty());
        // No data file appears (only the lock file may).
        assert!(!path.exists());
        assert!(!d.path().join("profiles.json.tmp").exists());

        // Also when a file exists: it is not rewritten.
        s.upsert(profile(2, "B")).unwrap();
        let before = fs::read(&path).unwrap();
        s.delete(&pid(1)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(s.all().len(), 1);
    }

    #[test]
    fn delete_of_an_already_deleted_id_is_a_noop() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        s.delete(&pid(1)).unwrap();
        let before = fs::read(&path).unwrap();
        let t = s.all()[0].updated_at;
        s.delete(&pid(1)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(s.all()[0].updated_at, t);
    }

    #[test]
    fn replace_all_replaces_everything_and_keeps_updated_at() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "old")).unwrap();
        let mut a = profile(2, "A");
        a.updated_at = 42;
        let mut gone = profile(3, "B").tombstone(7);
        gone.id = pid(3);
        s.replace_all(vec![a.clone(), gone.clone()]).unwrap();
        assert_eq!(s.all(), vec![a.clone(), gone.clone()]);
        assert!(s.get(&pid(1)).is_none());

        let reloaded = ProfileStore::open(&path).unwrap();
        assert_eq!(reloaded.all(), vec![a, gone]);

        s.replace_all(Vec::new()).unwrap();
        assert!(ProfileStore::open(&path).unwrap().all().is_empty());
    }

    #[test]
    fn replace_all_rejects_the_whole_set_if_one_live_profile_is_invalid() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "keep")).unwrap();
        let before_file = fs::read(&path).unwrap();
        let before_all = s.all();

        let mut bad = profile(3, "bad");
        bad.host = "-oProxyCommand=x".to_string();
        let err = s
            .replace_all(vec![profile(2, "fine"), bad, profile(4, "also fine")])
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ProfileError::Host)));
        assert_eq!(s.all(), before_all);
        assert_eq!(fs::read(&path).unwrap(), before_file);
    }

    #[test]
    fn replace_all_rejects_bad_ids_on_live_profiles_and_tombstones() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();

        let mut live = profile(1, "A");
        live.id = "../x".to_string();
        assert!(matches!(
            s.replace_all(vec![live]),
            Err(StoreError::Invalid(ProfileError::Id))
        ));

        let mut tomb = profile(1, "A").tombstone(5);
        tomb.id = "not-a-uuid".to_string();
        assert!(matches!(
            s.replace_all(vec![tomb]),
            Err(StoreError::Invalid(ProfileError::Id))
        ));

        assert!(s.all().is_empty());
        assert!(dir_entries(d.path()).is_empty());
    }

    #[test]
    fn replace_all_rejects_duplicate_ids() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let err = s
            .replace_all(vec![profile(1, "A"), profile(2, "B"), profile(1, "A2")])
            .unwrap_err();
        assert!(matches!(err, StoreError::DuplicateId));
        // A live profile and a tombstone with the same id also clash.
        let err = s
            .replace_all(vec![profile(1, "A"), profile(1, "A").tombstone(3)])
            .unwrap_err();
        assert!(matches!(err, StoreError::DuplicateId));
        assert!(s.all().is_empty());
        assert!(dir_entries(d.path()).is_empty());
    }

    #[test]
    fn replace_all_normalizes_incoming_tombstones() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        // A hostile synced tombstone stuffed with junk it should not persist.
        let junk = Profile {
            id: pid(1),
            name: "junk name".to_string(),
            host: "-oProxyCommand=evil".to_string(),
            port: 22,
            username: "-bad user".to_string(),
            key_file: Some(PathBuf::from("/etc/shadow")),
            sequences: vec![StartupSequence {
                name: String::new(),
                commands: vec!["rm -rf ~\nls".to_string()],
            }],
            transcript: true,
            local_only: true,
            updated_at: 42,
            deleted: true,
        };
        s.replace_all(vec![junk]).unwrap();
        let all = s.all();
        assert_eq!(all.len(), 1);
        assert_is_canonical_tombstone(&all[0], &pid(1), 42);

        let reloaded = ProfileStore::open(&path).unwrap();
        assert_is_canonical_tombstone(&reloaded.all()[0], &pid(1), 42);
        assert!(!fs::read_to_string(&path).unwrap().contains("evil"));
    }

    #[test]
    fn failed_mutation_leaves_memory_unchanged() {
        let (d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        // A directory where the file belongs: it can be neither read nor replaced.
        fs::create_dir(&path).unwrap();
        let err = s.upsert(profile(1, "A")).unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "got {err:?}");
        assert!(s.all().is_empty(), "memory must match disk after a failure");
        assert_eq!(
            dir_entries(d.path()),
            vec!["profiles.json", "profiles.json.lock"]
        );
        assert!(path.is_dir());
    }

    #[test]
    fn failed_atomic_write_removes_its_temp_file() {
        let (d, path) = setup();
        // The final rename fails because a directory is in the way.
        fs::create_dir(&path).unwrap();
        assert!(write_atomic(&path, b"x").is_err());
        assert_eq!(dir_entries(d.path()), vec!["profiles.json"]);
    }

    #[test]
    fn stale_temp_file_is_replaced_not_reused() {
        let (d, path) = setup();
        let tmp = d.path().join("profiles.json.tmp");
        fs::write(&tmp, "junk from a crashed save").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        assert_eq!(
            dir_entries(d.path()),
            vec!["profiles.json", "profiles.json.lock"]
        );
        assert_eq!(mode(&path), 0o600);
        assert_eq!(ProfileStore::open(&path).unwrap().live().len(), 1);
    }

    #[test]
    fn temp_file_symlink_is_not_followed() {
        let (d, path) = setup();
        let victim = d.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, d.path().join("profiles.json.tmp")).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert_eq!(ProfileStore::open(&path).unwrap().live().len(), 1);
    }

    #[test]
    fn existing_directory_permissions_are_left_alone() {
        let (d, path) = setup();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "A")).unwrap();
        assert_eq!(mode(d.path()), 0o755);
        assert_eq!(mode(&path), 0o600);
    }

    // ----- locked read-modify-write, reload, sorting -----

    #[test]
    fn second_handle_does_not_lose_first_handles_write() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        a.upsert(profile(1, "one")).unwrap();
        b.upsert(profile(2, "two")).unwrap();
        assert_eq!(ids_on_disk(&path), vec![pid(1), pid(2)]);
        let names: Vec<&str> = b.live().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["one", "two"]);
    }

    #[test]
    fn concurrent_upserts_from_threads_all_persist() {
        let (_d, path) = setup();
        let barrier = Barrier::new(8);
        std::thread::scope(|scope| {
            for n in 1..=8u32 {
                let (path, barrier) = (&path, &barrier);
                scope.spawn(move || {
                    let mut s = ProfileStore::open(path).unwrap();
                    // Every thread now holds the same stale (empty) snapshot.
                    barrier.wait();
                    s.upsert(profile(n, &format!("host{n}"))).unwrap();
                });
            }
        });
        let expected: Vec<String> = (1..=8).map(pid).collect();
        assert_eq!(ids_on_disk(&path), expected);
        let reloaded = ProfileStore::open(&path).unwrap();
        assert_eq!(reloaded.live().len(), 8);
    }

    #[test]
    fn delete_uses_fresh_disk_state() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        b.upsert(profile(1, "one")).unwrap();
        b.upsert(profile(2, "two")).unwrap();
        assert!(a.get(&pid(1)).is_none(), "A's snapshot is stale");

        a.delete(&pid(1)).unwrap();

        let disk = ProfileStore::open(&path).unwrap();
        assert!(disk.get(&pid(1)).is_none());
        let t = disk.all().into_iter().find(|p| p.id == pid(1)).unwrap();
        assert!(t.deleted);
        assert_is_canonical_tombstone(&t, &pid(1), t.updated_at);
        assert_eq!(disk.get(&pid(2)).unwrap().name, "two");
        assert_eq!(a.all(), disk.all(), "A's snapshot is refreshed");
    }

    #[test]
    fn mutations_start_from_disk_state_not_the_snapshot() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        b.upsert(profile(1, "one")).unwrap();
        // replace_all replaces the lot, but still has to read the disk first.
        a.replace_all(vec![profile(2, "two")]).unwrap();
        assert_eq!(ids_on_disk(&path), vec![pid(2)]);
        // delete of an id only the disk knows about, when A's copy is stale.
        b.upsert(profile(3, "three")).unwrap();
        a.delete(&pid(3)).unwrap();
        let disk = ProfileStore::open(&path).unwrap();
        assert!(disk.all().iter().any(|p| p.id == pid(3) && p.deleted));
    }

    #[test]
    fn mutation_fails_if_the_file_on_disk_is_now_bad() {
        let valid_but_invalid_host = format!(
            r#"{{"version":1,"profiles":[{{"id":"{}","name":"x","host":"-oProxyCommand=x","port":22,"username":"alice","key_file":null,"sequences":[],"transcript":false,"updated_at":1,"deleted":false}}]}}"#,
            pid(9)
        );
        for (bad, want) in [
            ("not json".to_string(), "corrupt"),
            (r#"{"version":3,"profiles":[]}"#.to_string(), "version"),
            (valid_but_invalid_host, "invalid"),
        ] {
            let (_d, path) = setup();
            let mut s = ProfileStore::open(&path).unwrap();
            s.upsert(profile(1, "one")).unwrap();
            let snapshot = s.all();
            fs::write(&path, &bad).unwrap();

            let results = [
                s.upsert(profile(2, "two")),
                s.delete(&pid(1)),
                s.replace_all(vec![profile(3, "three")]),
            ];
            for r in results {
                let err = r.unwrap_err();
                let ok = match want {
                    "corrupt" => matches!(err, StoreError::Corrupt),
                    "version" => matches!(err, StoreError::UnsupportedVersion(3)),
                    _ => matches!(err, StoreError::Invalid(ProfileError::Host)),
                };
                assert!(ok, "{want}: got {err:?}");
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), bad, "file untouched");
            assert_eq!(s.all(), snapshot, "snapshot untouched");
        }
    }

    #[test]
    fn reload_picks_up_external_changes() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        b.upsert(profile(1, "one")).unwrap();
        assert!(a.live().is_empty(), "reads serve the snapshot");
        a.reload().unwrap();
        assert_eq!(a.all(), b.all());
        assert_eq!(a.live().len(), 1);

        // A file that has gone missing means empty, as with open.
        fs::remove_file(&path).unwrap();
        a.reload().unwrap();
        assert!(a.all().is_empty());
    }

    #[test]
    fn reload_error_keeps_the_snapshot_and_the_file() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "one")).unwrap();
        let snapshot = s.all();
        fs::write(&path, "garbage").unwrap();
        assert!(matches!(s.reload(), Err(StoreError::Corrupt)));
        assert_eq!(s.all(), snapshot);
        assert_eq!(fs::read_to_string(&path).unwrap(), "garbage");
    }

    #[test]
    fn live_sorts_case_insensitively() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        for (n, name) in [
            (1, "Zeta"),
            (2, "alpha"),
            (3, "Bravo"),
            (4, "charlie"),
            (5, "Alpha"),
            (8, "dup"),
            (7, "dup"),
        ] {
            s.upsert(profile(n, name)).unwrap();
        }
        let live = s.live();
        let names: Vec<&str> = live.iter().map(|p| p.name.as_str()).collect();
        // Case-insensitive first, then case-sensitive, then id.
        assert_eq!(
            names,
            vec!["Alpha", "alpha", "Bravo", "charlie", "dup", "dup", "Zeta"]
        );
        let dup_ids: Vec<&str> = live
            .iter()
            .filter(|p| p.name == "dup")
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(dup_ids, vec![pid(7), pid(8)]);
    }

    // ----- update (locked read-merge-write) -----

    #[test]
    fn update_sees_fresh_disk_state() {
        let (_d, path) = setup();
        let mut a = ProfileStore::open(&path).unwrap();
        let mut b = ProfileStore::open(&path).unwrap();
        b.upsert(profile(1, "one")).unwrap();
        assert!(a.all().is_empty(), "A's snapshot is stale");

        let seen = a
            .update(|fresh| {
                let ids: Vec<String> = fresh.iter().map(|p| p.id.clone()).collect();
                let mut next = fresh.to_vec();
                let mut two = profile(2, "two");
                two.updated_at = 77;
                next.push(two);
                Ok((next, ids))
            })
            .unwrap();

        assert_eq!(seen, vec![pid(1)], "the closure sees what is on disk");
        assert_eq!(ids_on_disk(&path), vec![pid(1), pid(2)]);
        assert_eq!(a.all(), ProfileStore::open(&path).unwrap().all());
        // Profiles keep their updated_at, as with replace_all.
        assert_eq!(a.get(&pid(2)).unwrap().updated_at, 77);
    }

    #[test]
    fn update_rejecting_closure_leaves_file_unchanged() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        s.upsert(profile(1, "one")).unwrap();
        let before_file = fs::read(&path).unwrap();
        let before_all = s.all();

        // The closure itself fails.
        let err = s
            .update(|_| -> Result<(Vec<Profile>, ()), StoreError> { Err(StoreError::Corrupt) })
            .unwrap_err();
        assert!(matches!(err, StoreError::Corrupt));

        // The closure returns an invalid profile among valid ones.
        let mut bad = profile(3, "bad");
        bad.host = "-oProxyCommand=x".to_string();
        let err = s
            .update(|fresh| {
                let mut next = fresh.to_vec();
                next.push(profile(2, "fine"));
                next.push(bad);
                Ok((next, ()))
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ProfileError::Host)));

        // The closure returns a duplicate id.
        let err = s
            .update(|fresh| {
                let mut next = fresh.to_vec();
                next.push(profile(1, "again"));
                Ok((next, ()))
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::DuplicateId));

        assert_eq!(fs::read(&path).unwrap(), before_file);
        assert_eq!(s.all(), before_all);
    }

    #[test]
    fn update_normalizes_tombstones() {
        let (_d, path) = setup();
        let mut s = ProfileStore::open(&path).unwrap();
        let mut junk = profile(1, "junk");
        junk.deleted = true;
        junk.updated_at = 42;
        s.update(|_| Ok((vec![junk], ()))).unwrap();
        assert_is_canonical_tombstone(&s.all()[0], &pid(1), 42);
        assert!(!fs::read_to_string(&path).unwrap().contains("junk"));
    }

    #[test]
    fn update_without_changes_does_not_rewrite() {
        let (_d, path) = setup();
        // A compact hand-written file: any rewrite would pretty-print it.
        let text = format!(
            r#"{{"version":1,"profiles":[{{"id":"{}","name":"Box","host":"10.0.0.5","port":22,"username":"root","key_file":null,"sequences":[],"transcript":false,"updated_at":5,"deleted":false}}]}}"#,
            pid(9)
        );
        fs::write(&path, &text).unwrap();
        let mut s = ProfileStore::open(&path).unwrap();
        let n = s.update(|fresh| Ok((fresh.to_vec(), fresh.len()))).unwrap();
        assert_eq!(n, 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn path_is_the_opened_path() {
        let (_d, path) = setup();
        assert_eq!(ProfileStore::open(&path).unwrap().path(), path.as_path());
    }

    // ----- default_path -----

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn config_path_prefers_xdg_config_home() {
        let got = config_path_from(
            Some(OsStr::new("/xdg/config")),
            Some(OsStr::new("/home/alice")),
        );
        assert_eq!(got, p("/xdg/config/moniker/profiles.json"));
    }

    #[test]
    fn config_path_falls_back_to_home_dot_config() {
        let got = config_path_from(None, Some(OsStr::new("/home/alice")));
        assert_eq!(got, p("/home/alice/.config/moniker/profiles.json"));
    }

    #[test]
    fn config_path_treats_empty_xdg_as_unset() {
        let got = config_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/alice")));
        assert_eq!(got, p("/home/alice/.config/moniker/profiles.json"));
    }

    #[test]
    fn config_path_ignores_relative_xdg() {
        // The XDG spec says relative paths in these variables are invalid.
        let got = config_path_from(Some(OsStr::new("rel/dir")), Some(OsStr::new("/home/alice")));
        assert_eq!(got, p("/home/alice/.config/moniker/profiles.json"));
    }

    #[test]
    fn config_path_without_any_base_is_relative_dot_config() {
        let want = p(".config/moniker/profiles.json");
        assert_eq!(config_path_from(None, None), want);
        assert_eq!(
            config_path_from(Some(OsStr::new("")), Some(OsStr::new(""))),
            want
        );
    }

    #[test]
    fn default_path_ends_with_moniker_profiles_json() {
        assert!(ProfileStore::default_path().ends_with("moniker/profiles.json"));
    }
}
