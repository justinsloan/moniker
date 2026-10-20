//! Bundle entries and the merge that reconciles two machines' profile sets.
//!
//! A [`BundleEntry`] is a [`Profile`] plus its optional password: the unit that
//! is encrypted and exchanged during sync. [`merge`] combines the entries held
//! locally with the entries received from the remote machine.
//!
//! Handling rules for the password:
//!
//! * [`BundleEntry`]'s `Debug` output redacts the password, but that does NOT
//!   make an entry safe to log: it still prints the whole [`Profile`], and
//!   `Profile`'s `Debug` includes the startup commands, which can hold secrets
//!   (a typed `sudo` password, a token). Never log an entry or a profile; the
//!   redaction only keeps the password out of test failure output.
//! * It is zeroized when the entry is dropped, which also means an entry
//!   cannot be destructured by move: borrow its fields or clone it.
//! * Tombstones leave [`merge`] without one, so a deleted profile cannot keep
//!   a secret alive in a bundle.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::profile::Profile;

/// A profile and its password, the unit that is encrypted and exchanged during
/// sync.
///
/// The password is zeroized on drop and never appears in `Debug` output (the
/// profile's startup commands do: see the module docs).
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct BundleEntry {
    pub profile: Profile,
    /// The profile's password, if it has one. Always `None` for a tombstone
    /// that came out of [`merge`].
    pub password: Option<String>,
}

impl fmt::Debug for BundleEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let password = self.password.as_ref().map(|_| "<redacted>");
        f.debug_struct("BundleEntry")
            .field("profile", &self.profile)
            .field("password", &password)
            .finish()
    }
}

impl Drop for BundleEntry {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// Merge the entries held locally with the entries received from the remote.
///
/// All entries from both sides form one pool that is reduced per
/// `profile.id`, so duplicate ids within a single side are resolved by the
/// same rule as ids present on both sides. The winner for an id is the entry
/// with:
///
/// 1. the higher `profile.updated_at`; the entry carries its own password;
/// 2. on equal `updated_at`, a tombstone (`profile.deleted`) over a live entry;
/// 3. otherwise the entry whose `serde_json::to_string` is byte-wise greater.
///
/// Rules 1 to 3 order the entries totally (apart from entries that cannot be
/// serialized, which only happens for a non-UTF-8 `key_file` and could not be
/// exchanged anyway), so the result does not depend on argument or element
/// order: both machines compute the same bundle. Merging the result
/// with either input again changes nothing.
///
/// Tombstones in the output have `password: None`. The output is sorted by
/// `profile.id`.
pub fn merge(local: Vec<BundleEntry>, remote: Vec<BundleEntry>) -> Vec<BundleEntry> {
    let mut winners: BTreeMap<String, BundleEntry> = BTreeMap::new();
    for entry in local.into_iter().chain(remote) {
        match winners.entry(entry.profile.id.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            Entry::Occupied(mut slot) => {
                if precedence(&entry, slot.get()) == Ordering::Greater {
                    // The displaced entry is dropped here, which zeroizes it.
                    slot.insert(entry);
                }
            }
        }
    }
    winners
        .into_values()
        .map(|mut entry| {
            if entry.profile.deleted {
                entry.password.zeroize();
            }
            entry
        })
        .collect()
}

/// Which of two entries for the same profile wins: `Greater` means `a`.
fn precedence(a: &BundleEntry, b: &BundleEntry) -> Ordering {
    a.profile
        .updated_at
        .cmp(&b.profile.updated_at)
        .then_with(|| a.profile.deleted.cmp(&b.profile.deleted))
        .then_with(|| tie_key(a).as_deref().cmp(&tie_key(b).as_deref()))
}

/// The final tie-break: the entry's JSON text, compared byte-wise.
///
/// The text includes the password, so it is zeroized when dropped. `None` when
/// the entry cannot be serialized (a `key_file` that is not valid UTF-8); that
/// sorts below every serializable entry. Two such entries compare equal, so
/// their order would matter, but they could not be exchanged in a bundle
/// anyway.
fn tie_key(entry: &BundleEntry) -> Option<Zeroizing<String>> {
    serde_json::to_string(entry).ok().map(Zeroizing::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed canonical UUIDs, in ascending string order.
    const ID_A: &str = "00000000-0000-4000-8000-00000000000a";
    const ID_B: &str = "00000000-0000-4000-8000-00000000000b";
    const ID_C: &str = "00000000-0000-4000-8000-00000000000c";

    /// A live entry for `id`, named `name`, last edited at `updated_at`.
    fn live(id: &str, name: &str, updated_at: i64, password: Option<&str>) -> BundleEntry {
        let mut profile = Profile::new(name, "example.com", "alice");
        profile.id = id.to_string();
        profile.updated_at = updated_at;
        assert_eq!(profile.validate(), Ok(()), "test fixture must be valid");
        BundleEntry {
            profile,
            password: password.map(str::to_string),
        }
    }

    /// A tombstone entry for `id` stamped `updated_at`, optionally (wrongly)
    /// still carrying a password, as an older peer might send.
    fn tomb(id: &str, updated_at: i64, password: Option<&str>) -> BundleEntry {
        BundleEntry {
            profile: live(id, "gone", 0, None).profile.tombstone(updated_at),
            password: password.map(str::to_string),
        }
    }

    /// The byte string the tie-break compares.
    fn wire(entry: &BundleEntry) -> String {
        serde_json::to_string(entry).unwrap()
    }

    #[test]
    fn disjoint_ids_union() {
        let a = live(ID_A, "a", 10, Some("pa"));
        let c = live(ID_C, "c", 30, None);
        let b = live(ID_B, "b", 20, Some("pb"));

        let merged = merge(vec![a.clone(), c.clone()], vec![b.clone()]);
        assert_eq!(merged, vec![a.clone(), b.clone(), c.clone()]);

        assert_eq!(merge(vec![], vec![]), vec![]);
        assert_eq!(merge(vec![a.clone()], vec![]), vec![a.clone()]);
        assert_eq!(merge(vec![], vec![a.clone()]), vec![a]);
    }

    #[test]
    fn newer_update_wins_with_its_password() {
        let old = live(ID_A, "old", 100, Some("old-pass"));
        let new = live(ID_A, "new", 200, Some("new-pass"));

        assert_eq!(
            merge(vec![old.clone()], vec![new.clone()]),
            vec![new.clone()]
        );
        assert_eq!(
            merge(vec![new.clone()], vec![old.clone()]),
            vec![new.clone()]
        );

        // The winner carries its own password even when that is "none": the
        // loser's password must not leak into the winner.
        let newer_no_pass = live(ID_A, "newest", 300, None);
        let merged = merge(vec![old], vec![newer_no_pass.clone()]);
        assert_eq!(merged, vec![newer_no_pass]);
        assert_eq!(merged[0].password, None);
    }

    #[test]
    fn newer_tombstone_beats_live_and_newer_live_beats_tombstone() {
        // Deleted after the last edit: the deletion wins, from either side.
        let live_old = live(ID_A, "a", 100, Some("pa"));
        let tomb_new = tomb(ID_A, 200, None);
        assert_eq!(
            merge(vec![live_old.clone()], vec![tomb_new.clone()]),
            vec![tomb_new.clone()]
        );
        assert_eq!(
            merge(vec![tomb_new.clone()], vec![live_old]),
            vec![tomb_new]
        );

        // Edited (re-created) after the deletion: the live entry wins.
        let tomb_old = tomb(ID_B, 100, None);
        let live_new = live(ID_B, "b", 200, Some("pb"));
        assert_eq!(
            merge(vec![tomb_old.clone()], vec![live_new.clone()]),
            vec![live_new.clone()]
        );
        assert_eq!(
            merge(vec![live_new.clone()], vec![tomb_old]),
            vec![live_new]
        );
    }

    #[test]
    fn tie_tombstone_wins() {
        let live_entry = live(ID_A, "a", 500, Some("pa"));
        let tomb_entry = tomb(ID_A, 500, None);
        assert_eq!(
            merge(vec![live_entry.clone()], vec![tomb_entry.clone()]),
            vec![tomb_entry.clone()]
        );
        assert_eq!(
            merge(vec![tomb_entry.clone()], vec![live_entry]),
            vec![tomb_entry]
        );
    }

    #[test]
    fn tie_between_live_entries_is_commutative() {
        // Equal updated_at, different contents.
        let a = live(ID_A, "alpha", 500, Some("pw-1"));
        let b = live(ID_A, "bravo", 500, Some("pw-2"));
        // Equal updated_at, same profile, different passwords.
        let c = live(ID_A, "alpha", 500, Some("pw-3"));
        // Equal updated_at, same profile, one has no password.
        let d = live(ID_A, "alpha", 500, None);

        for (x, y) in [(&a, &b), (&a, &c), (&a, &d), (&b, &c), (&c, &d)] {
            let forward = merge(vec![x.clone()], vec![y.clone()]);
            let backward = merge(vec![y.clone()], vec![x.clone()]);
            assert_eq!(forward, backward, "merge must not depend on argument order");
            assert_eq!(forward.len(), 1);

            // The winner is the one whose serialization is byte-wise greater.
            let expected = if wire(x) > wire(y) { x } else { y };
            assert_eq!(&forward[0], expected);
        }
    }

    #[test]
    fn merge_is_idempotent() {
        let local = vec![
            live(ID_A, "a-local", 100, Some("pa-local")),
            live(ID_B, "b-local", 300, Some("pb-local")),
            tomb(ID_C, 50, None),
        ];
        let remote = vec![
            live(ID_A, "a-remote", 200, Some("pa-remote")),
            tomb(ID_B, 300, Some("stale-secret")),
            live(ID_C, "c-remote", 60, None),
        ];

        let once = merge(local.clone(), remote.clone());
        assert_eq!(merge(once.clone(), remote.clone()), once);
        assert_eq!(merge(local, once.clone()), once);
        assert_eq!(merge(once.clone(), once.clone()), once);
    }

    #[test]
    fn debug_never_prints_the_password() {
        let entry = live(ID_A, "a", 1, Some("hunter2-very-secret"));
        let shown = format!("{entry:?}");
        assert!(!shown.contains("hunter2-very-secret"), "leaked: {shown}");
        assert!(shown.contains("password: Some(\"<redacted>\")"), "{shown}");

        let pretty = format!("{entry:#?}");
        assert!(!pretty.contains("hunter2-very-secret"), "leaked: {pretty}");

        let none = format!("{:?}", live(ID_A, "a", 1, None));
        assert!(none.contains("password: None"), "{none}");
    }

    #[test]
    fn tombstone_entries_carry_no_password() {
        // A tombstone that arrives with a password loses it, whichever side
        // it came from and whether or not it beat a live entry.
        let merged = merge(vec![tomb(ID_A, 10, Some("secret-a"))], vec![]);
        assert_eq!(merged[0].password, None);
        assert!(merged[0].profile.deleted);

        let merged = merge(vec![], vec![tomb(ID_A, 10, Some("secret-a"))]);
        assert_eq!(merged[0].password, None);

        let merged = merge(
            vec![live(ID_B, "b", 5, Some("secret-b"))],
            vec![tomb(ID_B, 10, Some("secret-b2"))],
        );
        assert_eq!(merged.len(), 1);
        assert!(merged[0].profile.deleted);
        assert_eq!(merged[0].password, None);

        // Live entries keep theirs.
        let merged = merge(vec![live(ID_C, "c", 5, Some("keep"))], vec![]);
        assert_eq!(merged[0].password.as_deref(), Some("keep"));
    }

    #[test]
    fn duplicate_ids_on_one_side_are_reduced() {
        let v1 = live(ID_A, "v1", 1, Some("p1"));
        let v3 = live(ID_A, "v3", 3, Some("p3"));
        let v2 = live(ID_A, "v2", 2, Some("p2"));

        assert_eq!(
            merge(vec![v1.clone(), v3.clone(), v2.clone()], vec![]),
            vec![v3.clone()]
        );
        assert_eq!(
            merge(vec![], vec![v2.clone(), v1.clone(), v3.clone()]),
            vec![v3.clone()]
        );
        // Spread across both sides too, and in every order.
        assert_eq!(
            merge(vec![v3.clone(), v1.clone()], vec![v2.clone()]),
            vec![v3.clone()]
        );

        // Duplicates tied on updated_at resolve deterministically.
        let t1 = live(ID_B, "one", 9, Some("x"));
        let t2 = live(ID_B, "two", 9, Some("y"));
        let t3 = tomb(ID_B, 9, None);
        let orders = [
            vec![t1.clone(), t2.clone(), t3.clone()],
            vec![t3.clone(), t2.clone(), t1.clone()],
            vec![t2.clone(), t1.clone(), t3.clone()],
        ];
        for order in orders {
            assert_eq!(merge(order, vec![]), vec![t3.clone()]);
        }
    }

    #[test]
    fn output_is_sorted_by_id() {
        let merged = merge(
            vec![live(ID_C, "c", 1, None), live(ID_A, "a", 1, None)],
            vec![tomb(ID_B, 1, None), live(ID_C, "c2", 2, None)],
        );
        let ids: Vec<&str> = merged.iter().map(|e| e.profile.id.as_str()).collect();
        assert_eq!(ids, [ID_A, ID_B, ID_C]);
    }

    #[test]
    fn entry_serde_round_trip() {
        for entry in [
            live(ID_A, "with password", 42, Some("p\"w\\d")),
            live(ID_B, "without password", 43, None),
            tomb(ID_C, 44, None),
        ] {
            let json = serde_json::to_string(&entry).unwrap();
            let back: BundleEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(back, entry);
        }
    }
}
