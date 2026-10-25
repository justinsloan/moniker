//! When this machine last completed a sync (issue #8), kept beside the profile
//! file as `sync-state.json`: `{"version":1,"last_sync":<ms since the epoch>}`.
//!
//! Two things read it: pruning, which forgets a tombstone only once it is older
//! than the last completed sync (so it has left this machine), and the question
//! a sync asks when this machine has been away for long ([`crate::sync`]). A
//! missing, unreadable or unrecognized file means "unknown", which prunes
//! nothing and asks nothing. The file is written atomically, mode 0600, under
//! its own lock (`sync-state.json.lock`), and the recorded time never moves
//! backwards, so two syncs that end out of order keep the later one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::store;

/// The file's name, in the directory of the profile file.
pub const STATE_FILE: &str = "sync-state.json";

/// Where the sync state of the profile file at `profile_file` is kept.
pub fn state_path(profile_file: &Path) -> PathBuf {
    profile_file.with_file_name(STATE_FILE)
}

/// When the machine whose profile file is `profile_file` last completed a
/// sync, or `None` if that is unknown.
pub fn last_sync(profile_file: &Path) -> Option<i64> {
    read(&state_path(profile_file))
}

/// Record that a sync completed at `at_ms`, unless a later one is recorded.
pub fn record_sync(profile_file: &Path, at_ms: i64) -> io::Result<()> {
    let path = state_path(profile_file);
    store::ensure_parent_dir(&path)?;
    let _lock = store::lock_exclusive(&path)?;
    if read(&path).is_some_and(|recorded| recorded >= at_ms) {
        return Ok(());
    }
    let state = serde_json::json!({ "version": VERSION, "last_sync": at_ms });
    let mut bytes = serde_json::to_vec_pretty(&state).map_err(io::Error::other)?;
    bytes.push(b'\n');
    store::write_atomic(&path, &bytes)
}

/// The only format version this code reads or writes.
const VERSION: u64 = 1;

/// The recorded time in the file at `path`, if it is there and understood.
fn read(path: &Path) -> Option<i64> {
    let bytes = fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("version")?.as_u64()? != VERSION {
        return None;
    }
    value.get("last_sync")?.as_i64().filter(|at| *at >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn profile_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("moniker").join("profiles.json")
    }

    #[test]
    fn the_state_sits_beside_the_profile_file() {
        assert_eq!(
            state_path(Path::new("/x/moniker/profiles.json")),
            Path::new("/x/moniker/sync-state.json")
        );
    }

    #[test]
    fn nothing_recorded_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_sync(&profile_file(&dir)), None);
    }

    #[test]
    fn a_recorded_sync_is_read_back_from_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = profile_file(&dir);
        record_sync(&profiles, 1_790_000_000_000).unwrap();
        assert_eq!(last_sync(&profiles), Some(1_790_000_000_000));
        let mode = fs::metadata(state_path(&profiles))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_recorded_time_never_moves_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = profile_file(&dir);
        record_sync(&profiles, 2_000).unwrap();
        record_sync(&profiles, 1_000).unwrap();
        assert_eq!(last_sync(&profiles), Some(2_000));
        record_sync(&profiles, 3_000).unwrap();
        assert_eq!(last_sync(&profiles), Some(3_000));
    }

    #[test]
    fn a_file_that_is_not_understood_is_unknown_and_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = profile_file(&dir);
        fs::create_dir_all(profiles.parent().unwrap()).unwrap();
        for text in [
            "",
            "{ not json",
            "[1]",
            r#"{"version":2,"last_sync":5}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"last_sync":"5"}"#,
            r#"{"version":1,"last_sync":-5}"#,
        ] {
            fs::write(state_path(&profiles), text).unwrap();
            assert_eq!(last_sync(&profiles), None, "{text}");
        }
        record_sync(&profiles, 7).unwrap();
        assert_eq!(last_sync(&profiles), Some(7));
    }
}
