//! The "Jump Host" row (issue #9): the profile another profile's connection
//! goes through, last in the profile page's Connection group and after Port in
//! the New Profile dialog.
//!
//! It is a combo row: "None" first, then the profiles that can be this one's
//! jump host ([`jump::usable_jump_hosts`]: live, not this profile, and none
//! that goes through a jump host of its own, as Moniker uses one jump host
//! only), by name. Two of them that share a name are shown with their
//! `user@host:port` as well ([`form::subtitle`]), so that the person can tell
//! which is which, and so is one named like an entry of the row's own ("None",
//! "No longer saved"), which would otherwise pass for it. A jump host the
//! profile names that is not among them is one more entry, selected: "No
//! longer saved" for one that is not a live profile here, or its name for one
//! that goes through a jump host of its own (a merge of two machines' edits
//! can make that). The list is made again only when what it shows changes.
//!
//! Only a pick by the person is passed on (`on_pick`); showing what the file
//! says does not count as one. The profile page saves it on the keyring worker
//! with the row's [`Saving`] ([`form::apply_jump_host`], which judges the
//! choice again under the store's lock: a sync may have changed the profiles
//! since the row listed them). While a pick is being saved the row keeps
//! showing it, and once it is back the next
//! [`show_profile`](JumpRow::show_profile) shows the file's choice, which puts
//! a refused or failed pick back. In New Profile the pick is kept until Create.
//!
//! Its subtitle carries one note at a time, the most urgent first:
//! 1. the jump host is not saved here: [`form::JUMP_CHOOSE_ANOTHER`];
//! 2. it goes through a jump host of its own ([`jump::chain_text`]);
//! 3. it is kept on this machine only and this profile is not
//!    ([`form::kept_here_note`]);
//! 4. `~/.ssh/config` sends this profile's own connection through another host
//!    ([`form::config_route_note`]): `ssh -G` says so, without Moniker's route
//!    (the context's `config_route`, run on a worker thread of its own, never
//!    the main thread). It is asked when the row shows a profile whose
//!    connection (`ssh -G`'s arguments, [`ssh::resolve_command`]) is not the
//!    one it last asked about, and only an answer about the connection the
//!    profile still has is used. Only the profile page asks: New Profile's host
//!    is still being typed.
//!
//! No note: no subtitle. On a profile that others go through, the row is grayed
//! out instead, with a line that names them ([`form::going_through_line`]); if
//! the profile has a jump host anyway (from a merge), the line adds that it
//! goes through a jump host of its own, which is why those others are refused.
//!
//! Names are data from other machines: plain text everywhere (Pango markup off,
//! and the sync page's plain-text factory for the list and the choice shown).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::thread;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::jump;
use moniker_core::profile::Profile;
use moniker_core::ssh;

use super::saving::Saving;
use super::sync_page::plain_text_factory;
use crate::context::AppContext;
use crate::form;

/// The row's title.
pub(crate) const ROW_TITLE: &str = "Jump Host";

/// The first entry of the row: no jump host.
const NONE: &str = "None";

/// The row (module docs).
pub(super) struct JumpRow {
    /// Itself, for the answer of a check that comes back later: held weakly.
    me: Weak<JumpRow>,
    ctx: Rc<AppContext>,
    row: adw::ComboRow,
    /// The profile whose jump host this is; `None` in New Profile, where the
    /// profile has no id yet (and nobody goes through it).
    for_id: Option<String>,
    /// What each entry of the list means: the id of the profile it names, or
    /// `None` for "None".
    choices: RefCell<Vec<Option<String>>>,
    /// The entries' labels, to make the list again only when they change.
    labels: RefCell<Vec<String>>,
    /// The jump host the file has, or will have once the saves asked for are
    /// back: the last pick, while one is being saved. In New Profile, the pick.
    stored: RefCell<Option<String>>,
    saving: Rc<Saving>,
    /// The row is being set to what the file says (or its list is being made
    /// again, which moves the selection), not picked by the person: that saves
    /// nothing.
    loading: Cell<bool>,
    on_pick: Box<dyn Fn(Option<String>)>,
    /// The notes, as the last show worked them out.
    notes: RefCell<Notes>,
    /// The connection the config check was last asked about (`ssh -G`'s
    /// arguments), and its answer once it is back.
    config: RefCell<Option<(Vec<String>, Option<bool>)>>,
}

/// What the subtitle can say, as the last show worked it out (module docs).
#[derive(Default)]
struct Notes {
    /// Others go through this profile: the row is grayed out, with this line.
    grayed: Option<String>,
    /// A note that needs no `ssh -G`: not saved, a chain, kept here.
    urgent: Option<String>,
    /// The jump host's name, when the `~/.ssh/config` note would be the one
    /// shown, if `ssh -G` says so.
    config_for: Option<String>,
}

impl JumpRow {
    /// A row for the profile `for_id` (`None`: the profile New Profile makes).
    /// Nothing is in it until the first show. `on_pick` is held by the row and
    /// must not hold its owner strongly.
    ///
    /// `on_pick` runs inside the row's `notify::selected`, so it must not show
    /// the row again synchronously: a show whose entries changed gives the row
    /// a new list, which the combo row does not survive while it is still
    /// telling of the pick (as for the Default sequence row,
    /// `sequence_rows`). The page's save is back later anyway; New Profile
    /// shows it again from the main loop.
    pub(super) fn new(
        ctx: &Rc<AppContext>,
        for_id: Option<&str>,
        on_pick: impl Fn(Option<String>) + 'static,
    ) -> Rc<JumpRow> {
        // Its list is made by the shows; names are data: plain text.
        let row = adw::ComboRow::builder()
            .title(ROW_TITLE)
            .use_markup(false)
            .factory(&plain_text_factory())
            .model(&gtk::StringList::new(&[NONE]))
            .build();
        let saving = Saving::new();
        row.add_suffix(&saving.spinner);
        Rc::new_cyclic(|me: &Weak<JumpRow>| {
            let picked = me.clone();
            row.connect_selected_notify(move |_| {
                if let Some(jump) = picked.upgrade() {
                    jump.picked();
                }
            });
            JumpRow {
                me: me.clone(),
                ctx: Rc::clone(ctx),
                row,
                for_id: for_id.map(str::to_owned),
                choices: RefCell::new(vec![None]),
                labels: RefCell::new(vec![NONE.to_owned()]),
                stored: RefCell::new(None),
                saving,
                loading: Cell::new(false),
                on_pick: Box::new(on_pick),
                notes: RefCell::new(Notes::default()),
                config: RefCell::new(None),
            }
        })
    }

    /// The combo row: what a pick is saved against (the owner marks it
    /// `error` and shows its message near it).
    pub(super) fn row(&self) -> &adw::ComboRow {
        &self.row
    }

    /// The row's saves under way, and its spinner.
    pub(super) fn saving(&self) -> &Rc<Saving> {
        &self.saving
    }

    /// The jump host chosen: the file's, or the last pick.
    pub(super) fn choice(&self) -> Option<String> {
        self.stored.borrow().clone()
    }

    /// Show the jump host of `profile` (the profile page's, as the file has it
    /// now) among the profiles as the window last read them. While a pick is
    /// being saved the row keeps showing the pick.
    pub(super) fn show_profile(&self, profile: &Profile) {
        if !self.saving.busy() {
            self.stored.replace(profile.jump_host.clone());
        }
        // The note on `~/.ssh/config` matters only with a jump host chosen.
        if self.stored.borrow().is_some() {
            self.ask_config(profile);
        }
        self.show(profile.local_only);
    }

    /// Show the pick of New Profile among the profiles as the window last read
    /// them; `local_only`: the new profile is to be kept on this machine only.
    pub(super) fn show_new(&self, local_only: bool) {
        self.show(local_only);
    }

    /// Make the list (if what it shows changed), select the choice, and work
    /// out the notes (module docs).
    fn show(&self, local_only: bool) {
        // Bind first so the store borrow ends here.
        let all = self.ctx.store.borrow().all();
        let for_id = self.for_id.as_deref();
        let stored = self.stored.borrow().clone();
        let chosen = stored
            .as_deref()
            .and_then(|id| all.iter().find(|p| p.id == id && !p.deleted));

        // None, the usable jump hosts, and the one chosen if it is not among
        // them: the profile each names, if it is saved.
        let usable = jump::usable_jump_hosts(for_id.unwrap_or_default(), &all);
        let mut entries: Vec<(Option<String>, Option<&Profile>)> = vec![(None, None)];
        entries.extend(usable.iter().map(|p| (Some(p.id.clone()), Some(*p))));
        if let Some(id) = &stored
            && !usable.iter().any(|p| p.id == *id)
        {
            entries.push((Some(id.clone()), chosen));
        }
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|(_, p)| p.map(|p| p.name.as_str()))
            .collect();
        let labels: Vec<String> = entries
            .iter()
            .map(|entry| match entry {
                (None, _) => NONE.to_owned(),
                (Some(_), None) => form::JUMP_NO_LONGER_SAVED.to_owned(),
                (Some(_), Some(p)) => {
                    let shared = [NONE, form::JUMP_NO_LONGER_SAVED].contains(&p.name.as_str())
                        || names.iter().filter(|name| **name == p.name).count() > 1;
                    if shared {
                        format!("{} ({})", p.name, form::subtitle(p))
                    } else {
                        p.name.clone()
                    }
                }
            })
            .collect();
        let at = entries
            .iter()
            .position(|(id, _)| *id == stored)
            .unwrap_or(0);

        self.loading.set(true);
        if *self.labels.borrow() != labels {
            let items: Vec<&str> = labels.iter().map(String::as_str).collect();
            self.row.set_model(Some(&gtk::StringList::new(&items)));
            self.labels.replace(labels);
        }
        self.choices
            .replace(entries.iter().map(|(id, _)| id.clone()).collect());
        let at = u32::try_from(at).unwrap_or(0);
        if self.row.selected() != at {
            self.row.set_selected(at);
        }
        self.loading.set(false);

        let others = for_id.map_or_else(Vec::new, |id| jump::going_through(id, &all));
        let grayed = (!others.is_empty()).then(|| {
            let names: Vec<&str> = others.iter().map(|p| p.name.as_str()).collect();
            let line = form::going_through_line(&names);
            let own = for_id.and_then(|id| all.iter().find(|p| p.id == id));
            match own {
                Some(own) if stored.is_some() => {
                    format!("{line} {}", jump::chain_text(&own.name))
                }
                _ => line,
            }
        });
        let urgent = match (&stored, chosen) {
            (None, _) => None,
            (Some(_), None) => Some(form::JUMP_CHOOSE_ANOTHER.to_owned()),
            (Some(_), Some(j)) if j.jump_host.is_some() => Some(jump::chain_text(&j.name)),
            (Some(_), Some(j)) if j.local_only && !local_only => {
                Some(form::kept_here_note(&j.name))
            }
            (Some(_), Some(_)) => None,
        };
        let config_for = chosen.filter(|_| urgent.is_none()).map(|j| j.name.clone());
        self.notes.replace(Notes {
            grayed,
            urgent,
            config_for,
        });
        self.render();
    }

    /// Show the notes worked out last, and the config check's answer if it is
    /// back (module docs).
    fn render(&self) {
        let notes = self.notes.borrow();
        let (sensitive, subtitle) = match &notes.grayed {
            Some(line) => (false, line.clone()),
            None => {
                let through = matches!(&*self.config.borrow(), Some((_, Some(true))));
                let config = notes
                    .config_for
                    .as_deref()
                    .filter(|_| through)
                    .map(form::config_route_note);
                (true, notes.urgent.clone().or(config).unwrap_or_default())
            }
        };
        self.row.set_sensitive(sensitive);
        self.row.set_subtitle(&subtitle);
    }

    /// Ask whether `~/.ssh/config` sends `profile`'s own connection through
    /// another host, unless that connection is the one last asked about. The
    /// check runs `ssh -G`, so on a thread of its own; its answer is used only
    /// if it is still about the connection last asked about, and the row is
    /// still there.
    fn ask_config(&self, profile: &Profile) {
        let asked = ssh::resolve_command(profile).args;
        if self
            .config
            .borrow()
            .as_ref()
            .is_some_and(|(before, _)| *before == asked)
        {
            return;
        }
        self.config.replace(Some((asked.clone(), None)));
        let check = Arc::clone(&self.ctx.config_route);
        let profile = profile.clone();
        let (sender, received) = async_channel::bounded::<bool>(1);
        let spawned = thread::Builder::new()
            .name("moniker-ssh-config".to_owned())
            .spawn(move || {
                let _ = sender.send_blocking(check(&profile));
            });
        if spawned.is_err() {
            // Asked again at the next show.
            self.config.replace(None);
            return;
        }
        let weak = self.me.clone();
        glib::MainContext::default().spawn_local(async move {
            // `None`: the check panicked.
            let through = received.recv().await.ok();
            if let Some(row) = weak.upgrade() {
                row.config_answered(&asked, through);
            }
        });
    }

    /// The config check about the connection `asked` answered `through`
    /// (`None`: it never answered, and is asked again at the next show).
    fn config_answered(&self, asked: &[String], through: Option<bool>) {
        {
            let mut config = self.config.borrow_mut();
            let Some((current, answer)) = config.as_mut() else {
                return;
            };
            if current.as_slice() != asked {
                // About a connection the profile no longer has.
                return;
            }
            match through {
                Some(through) => *answer = Some(through),
                None => {
                    *config = None;
                    return;
                }
            }
        }
        self.render();
    }

    /// The person picked an entry: pass it on, as the id of the profile it
    /// names (`None` for "None"). The owner's save judges it again under the
    /// store's lock (`form::jump_choice_error`): a refusal writes nothing, and
    /// the row goes back to what the file says.
    fn picked(&self) {
        if self.loading.get() {
            return;
        }
        let at = self.row.selected();
        if at == gtk::INVALID_LIST_POSITION {
            return;
        }
        let Some(choice) = self.choices.borrow().get(at as usize).cloned() else {
            return;
        };
        if choice == *self.stored.borrow() {
            return;
        }
        self.stored.replace(choice.clone());
        (self.on_pick)(choice);
    }
}
