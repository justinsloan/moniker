//! Jump hosts: which profile another profile goes through.
//!
//! A profile names its jump host by id (`Profile::jump_host`). This module is
//! the part that only reads profiles: where a profile's connection goes
//! ([`route`]) and who is affected when a profile is picked or deleted as
//! somebody's jump host. Moniker uses one jump host only, and never a direct
//! connection the person did not choose: a profile whose jump host is gone or
//! has a jump host of its own is refused, with a text that says why.

use crate::profile::Profile;

/// The refusal for a profile whose jump host is not saved (here), as a tab or a
/// page says it.
pub const MISSING_TEXT: &str = "This profile's jump host is no longer saved.";

/// The refusal for a profile whose jump host has a jump host of its own.
/// `name` is the name of that jump host, the one that goes through another.
pub fn chain_text(name: &str) -> String {
    format!("{name} goes through a jump host of its own. Moniker uses only one jump host.")
}

/// Where a profile's connection goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route<'a> {
    /// Straight to its own host: no jump host is set.
    Direct,
    /// Through this profile, which has no jump host of its own.
    Through(&'a Profile),
    /// A jump host is set and nothing live has its id.
    Missing,
    /// Through this profile, which goes through a jump host of its own.
    Chained(&'a Profile),
}

impl Route<'_> {
    /// Why the connection must not start, when it must not: `Missing` and
    /// `Chained` never fall back to a direct connection.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Route::Direct | Route::Through(_) => None,
            Route::Missing => Some(MISSING_TEXT.to_string()),
            Route::Chained(jump) => Some(chain_text(&jump.name)),
        }
    }

    /// The profile the connection goes through, if one is saved: `Through` and
    /// `Chained` have one, though a `Chained` one is refused.
    pub fn jump(&self) -> Option<&Profile> {
        match self {
            Route::Through(jump) | Route::Chained(jump) => Some(jump),
            Route::Direct | Route::Missing => None,
        }
    }
}

/// Where `p`'s connection goes, with `find` looking up a profile by id
/// (`ProfileStore::get` gives live profiles only; a tombstone that `find` does
/// give is treated as not saved).
pub fn route<'a>(p: &Profile, find: impl Fn(&str) -> Option<&'a Profile>) -> Route<'a> {
    let Some(id) = &p.jump_host else {
        return Route::Direct;
    };
    match find(id).filter(|jump| !jump.deleted) {
        None => Route::Missing,
        Some(jump) if jump.jump_host.is_some() => Route::Chained(jump),
        Some(jump) => Route::Through(jump),
    }
}

/// The live profiles that go through the profile `id`, by name without regard
/// to case, then by id. The question deleting a jump host asks, and what a
/// profile that others go through is grayed out for.
pub fn going_through<'a>(id: &str, all: impl IntoIterator<Item = &'a Profile>) -> Vec<&'a Profile> {
    in_name_order(
        all.into_iter()
            .filter(|p| !p.deleted && p.jump_host.as_deref() == Some(id)),
    )
}

/// The live profiles that can be the jump host of the profile `for_id`: not
/// itself, and none that goes through a jump host of its own. In name order
/// like [`going_through`].
pub fn usable_jump_hosts<'a>(
    for_id: &str,
    all: impl IntoIterator<Item = &'a Profile>,
) -> Vec<&'a Profile> {
    in_name_order(
        all.into_iter()
            .filter(|p| !p.deleted && p.jump_host.is_none() && p.id != for_id),
    )
}

/// Whether any live profile goes through a jump host. A sync that carries one
/// must keep out an older Moniker, which would connect it straight to its host.
pub fn holds_jump_host<'a>(all: impl IntoIterator<Item = &'a Profile>) -> bool {
    all.into_iter().any(|p| !p.deleted && p.jump_host.is_some())
}

/// `ps` by name without regard to case, then by id.
fn in_name_order<'a>(ps: impl Iterator<Item = &'a Profile>) -> Vec<&'a Profile> {
    let mut v: Vec<&Profile> = ps.collect();
    v.sort_by_cached_key(|p| (p.name.to_lowercase(), p.id.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live profile named `name` with the canonical id `n`.
    fn profile(n: u32, name: &str) -> Profile {
        let mut p = Profile::new(name, "example.com", "alice");
        p.id = format!("00000000-0000-4000-8000-{n:012x}");
        p
    }

    fn via(p: Profile, jump: &Profile) -> Profile {
        Profile {
            jump_host: Some(jump.id.clone()),
            ..p
        }
    }

    fn names<'a>(ps: &[&'a Profile]) -> Vec<&'a str> {
        ps.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn routes() {
        let bastion = profile(1, "bastion");
        let db = via(profile(2, "db"), &bastion);
        let all = [bastion.clone(), db.clone()];
        let find = |id: &str| all.iter().find(|p| p.id == id);

        assert_eq!(route(&bastion, find), Route::Direct);
        assert_eq!(route(&db, find), Route::Through(&bastion));

        // A jump host nobody has.
        let lost = Profile {
            jump_host: Some(profile(9, "x").id),
            ..profile(3, "lost")
        };
        assert_eq!(route(&lost, find), Route::Missing);

        // A tombstone found by id is as good as none: `ProfileStore::get` never
        // gives one, but another caller's lookup may.
        let gone = [bastion.tombstone(5)];
        let find_gone = |id: &str| gone.iter().find(|p| p.id == id);
        assert_eq!(route(&db, find_gone), Route::Missing);

        // The jump host has a jump host of its own: one hop only.
        let cycle = [
            via(profile(4, "a"), &profile(5, "b")),
            via(profile(5, "b"), &profile(4, "a")),
        ];
        let find_cycle = |id: &str| cycle.iter().find(|p| p.id == id);
        assert_eq!(route(&cycle[0], find_cycle), Route::Chained(&cycle[1]));
        assert_eq!(route(&cycle[1], find_cycle), Route::Chained(&cycle[0]));
    }

    #[test]
    fn a_route_knows_its_jump_host_and_whether_it_refuses() {
        let bastion = profile(1, "bastion");
        let outer = via(profile(2, "outer"), &bastion);
        assert_eq!(Route::Direct.jump(), None);
        assert_eq!(Route::Missing.jump(), None);
        assert_eq!(Route::Through(&bastion).jump(), Some(&bastion));
        assert_eq!(Route::Chained(&outer).jump(), Some(&outer));
        assert_eq!(Route::Direct.refusal(), None);
        assert_eq!(Route::Through(&bastion).refusal(), None);
        assert!(Route::Missing.refusal().is_some());
        assert!(Route::Chained(&outer).refusal().is_some());
    }

    #[test]
    fn refusals_name_the_jump_host() {
        assert_eq!(MISSING_TEXT, "This profile's jump host is no longer saved.");
        assert_eq!(Route::Missing.refusal().as_deref(), Some(MISSING_TEXT));
        assert_eq!(
            chain_text("bastion"),
            "bastion goes through a jump host of its own. Moniker uses only one jump host."
        );
        // The name is the jump host's, the profile that has a jump host of its own.
        let inner = via(profile(1, "inner"), &profile(2, "outer"));
        assert_eq!(Route::Chained(&inner).refusal(), Some(chain_text("inner")));
    }

    #[test]
    fn going_through_lists_live_profiles_in_name_order() {
        let bastion = profile(1, "bastion");
        let all = vec![
            via(profile(2, "web"), &bastion),
            via(profile(3, "Db"), &bastion),
            profile(4, "direct"),
            via(profile(5, "app"), &bastion),
            via(profile(6, "other"), &profile(7, "somebody else")),
            via(profile(8, "gone"), &bastion).tombstone(3),
            // The same name once more: the id decides.
            via(profile(9, "web"), &bastion),
            bastion.clone(),
        ];
        let through = going_through(&bastion.id, &all);
        assert_eq!(names(&through), ["app", "Db", "web", "web"]);
        assert!(through[2].id < through[3].id);
        assert!(going_through(&all[4].id, &all).is_empty());
        assert!(going_through("not an id", &all).is_empty());
    }

    #[test]
    fn usable_jump_hosts_leave_out_self_chains_and_tombstones() {
        let bastion = profile(1, "Bastion");
        let edge = profile(2, "edge");
        let behind = via(profile(3, "behind"), &bastion);
        let gone = profile(4, "gone").tombstone(3);
        let me = profile(5, "me");
        let all = vec![me.clone(), behind, gone, edge, bastion];
        let usable = usable_jump_hosts(&me.id, &all);
        assert_eq!(names(&usable), ["Bastion", "edge"]);
        // A profile that has a jump host of its own can be nobody's.
        assert!(usable.iter().all(|p| p.jump_host.is_none()));
        // For a profile that is not in the list, only the tombstone and the
        // profile with a jump host are left out.
        let usable = usable_jump_hosts("someone else", &all);
        assert_eq!(names(&usable), ["Bastion", "edge", "me"]);
    }

    #[test]
    fn holds_jump_host_looks_at_live_profiles() {
        let bastion = profile(1, "bastion");
        let db = via(profile(2, "db"), &bastion);
        assert!(!holds_jump_host(std::slice::from_ref(&bastion)));
        assert!(holds_jump_host(&[bastion.clone(), db.clone()]));
        assert!(!holds_jump_host(&[bastion, db.tombstone(4)]));
        assert!(!holds_jump_host(&[]));
    }
}
