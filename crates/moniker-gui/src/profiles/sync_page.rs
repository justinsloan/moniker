//! The sync page: the content pane of the Connection Profiles window when the sidebar's
//! Sync entry is chosen. Turn sync on or off, set the sync key, pick the other
//! machine's profile, turn automatic sync on or off, and sync.
//!
//! The first group is the switch for sync as a whole (issue #10), which lives
//! in the profile file where the core and `moniker-server` read it. Turning it
//! off is saved at once and asks nothing; turning it on asks first
//! ([`ask_to_turn_sync_on`]), because the profiles that sync, those added while
//! it was off included, then go to the other machines with their passwords. The
//! save runs on the keyring worker like every write of the profile file
//! ([`keyring::write_profiles`]), and the row follows the file afterwards and
//! whenever anyone else changes it. While it is off, Sync Automatically and the
//! Sync button are off and the hint says why; the key and the other machine can
//! still be chosen, as they send nothing.
//!
//! The logic and the worker thread are in [`crate::sync`]; this file is the
//! widgets. The page starts its syncs with [`sync::begin_app_sync`], like Sync
//! Now and the sync on launch, and listens to every sync of the app
//! ([`AppContext::on_sync_event`]): while any of them runs, the page says so
//! and Sync is off, and it shows the result of each.
//!
//! The first sync that succeeds from here records its profile as the sync
//! partner in the GUI settings ([`sync::record_partner`]), which offers Sync
//! Now and turns on the sync on launch (unless Sync Automatically is off). The
//! first sync is always started by hand, so a mistyped key or a wrong host is
//! seen on this page rather than as a notification at launch.
//!
//! The Sync Partner row, between Sync Automatically and the hint, names that
//! profile while there is one ([`sync::live_partner`], the test Sync Now uses)
//! and has a Forget button (issue #11). Forget clears only the partner in the
//! settings file ([`sync::forget_partner`]) and tells the app
//! ([`AppContext::partner_changed`]): Sync Now goes and the next launch does not
//! sync, until a sync from this page succeeds again. It keeps the sync key, the
//! Sync switch, Sync Automatically, the profile choice and every profile, and
//! the line it leaves says that other machines can still start a sync with this
//! one. It waits for a sync that runs, which would record the partner again, but
//! not for Sync being off, as it sends nothing. Whether it worked or not,
//! everything that shows the partner reads the file again.
//!
//! The sync key is read from its field into a `Zeroizing` buffer, never shown
//! again, and the field is emptied after a save and when the page goes. Every
//! text is plain (Pango markup off): profile names come from other machines.
//!
//! The page's keyring calls (whether a key is stored, saving one) run on the
//! keyring worker ([`keyring::read`], [`keyring::change`]), never the main thread; until the answer
//! is back the status line says the keyring is being read and Sync waits.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{glib, pango};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::store::SyncSwitch;
use moniker_core::sync::StaleSync;
use zeroize::Zeroizing;

use crate::context::AppContext;
use crate::form;
use crate::keyring;
use crate::settings;
use crate::sync::{self, KeyState, SyncEvent, SyncFailure};

/// Widget names of the page's message lines (tests look for them).
pub(crate) const KEY_STATUS_NAME: &str = "sync-key-status";
pub(crate) const KEY_MESSAGE_NAME: &str = "sync-key-message";
pub(crate) const HINT_NAME: &str = "sync-hint";
pub(crate) const RESULT_NAME: &str = "sync-result";
pub(crate) const PARTNER_ROW_NAME: &str = "sync-partner";
pub(crate) const PARTNER_MESSAGE_NAME: &str = "sync-partner-message";

/// The explanation of the "Other machine" group.
const MACHINE_NOTE: &str = "Pick the saved profile of your other machine. Moniker connects \
    with ssh and runs moniker-server there; afterwards both machines have the same profiles.";

/// The automatic-sync switch's subtitle before any sync from this page has
/// succeeded (or after its profile was deleted).
pub(crate) const AUTO_SYNC_NO_PARTNER: &str = "Once a sync from this page succeeds, sync \
    with that machine each time Moniker starts.";

/// The automatic-sync switch's subtitle once there is a partner.
pub(crate) fn auto_sync_with(name: &str) -> String {
    format!("Sync with {name} each time Moniker starts.")
}

/// What the page says after the partner was forgotten (`name` is plain text):
/// a forget only stops this machine from starting syncs, so the way to stop the
/// others is said too.
pub(crate) fn forgotten(name: &str) -> String {
    format!(
        "{name} is no longer the sync partner. Other machines can still start a sync with \
         this one while Sync is on."
    )
}

/// What the "Turn Sync On?" alert says: every profile that syncs goes out with
/// its stored password, including any added while sync was off, and the way to
/// keep one here is to turn off its own switch first.
const TURN_ON_BODY: &str = "At the next sync, every profile on this machine that syncs, \
    including any added while sync was off, will be copied to your other machines with its \
    stored password. To keep a profile on this machine only, cancel and turn off \"Sync to \
    other machines\" on its page first.";

/// The page's widgets and state. The Connection Profiles window holds it while the page
/// is shown; everything else holds it weakly, so it goes when the page does.
pub(crate) struct SyncPage {
    ctx: Rc<AppContext>,
    pub(crate) widget: adw::PreferencesPage,
    key_entry: adw::PasswordEntryRow,
    key_status: gtk::Label,
    key_message: gtk::Label,
    /// Sync as a whole (issue #10), first on the page.
    sync_switch: adw::SwitchRow,
    /// The other machine's profile; its subtitle says where it connects.
    machine: adw::ComboRow,
    auto_sync: adw::SwitchRow,
    /// The sync partner, while there is one (issue #11), with its Forget button
    /// and the line a forget leaves.
    partner_row: adw::ActionRow,
    forget_button: gtk::Button,
    partner_message: gtk::Label,
    /// Why Sync is unavailable, if it is.
    hint: gtk::Label,
    sync_button: gtk::Button,
    result: gtk::Label,
    /// Profile id of each `machine` entry, by position.
    ids: RefCell<Vec<String>>,
    key_set: Cell<bool>,
    /// The keyring has said whether a key is stored, once.
    key_known: Cell<bool>,
    /// A switch is being set to what a file says, not by the person.
    loading: Cell<bool>,
}

impl SyncPage {
    pub(crate) fn new(ctx: &Rc<AppContext>) -> Rc<SyncPage> {
        // Its subtitle and state are set from the file by `refresh_sync_switch`.
        let sync_switch = adw::SwitchRow::builder()
            .title("Sync")
            .use_markup(false)
            .build();
        let switch_group = adw::PreferencesGroup::builder().title("Sync").build();
        switch_group.add(&sync_switch);

        let key_entry = adw::PasswordEntryRow::builder()
            .title(sync::KEY_PLACEHOLDER)
            .show_apply_button(true)
            .build();
        // The keyring's error can be long (it names the bus address): wrap it.
        let key_status = gtk::Label::builder()
            .name(KEY_STATUS_NAME)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .margin_top(12)
            .css_classes(["dim-label"])
            .build();
        let key_message = inline_message(KEY_MESSAGE_NAME);
        let key_group = adw::PreferencesGroup::builder()
            .title("Sync Key")
            .description(sync::KEY_NOTE)
            .build();
        key_group.add(&key_entry);
        key_group.add(&key_status);
        key_group.add(&key_message);

        let machine = adw::ComboRow::builder()
            .title("Profile")
            .use_markup(false)
            .factory(&plain_text_factory())
            .model(&gtk::StringList::new(&[]))
            .build();
        let auto_sync = adw::SwitchRow::builder()
            .title("Sync Automatically")
            .use_markup(false)
            .build();
        // The subtitle, a profile's name, is set by `refresh_partner`: plain
        // text, as the profile can come from another machine.
        let forget_button = gtk::Button::builder()
            .label("Forget")
            .tooltip_text("Forget the sync partner")
            .valign(gtk::Align::Center)
            .build();
        let partner_row = adw::ActionRow::builder()
            .name(PARTNER_ROW_NAME)
            .title("Sync Partner")
            .use_markup(false)
            .visible(false)
            .build();
        partner_row.add_suffix(&forget_button);
        let hint = gtk::Label::builder()
            .name(HINT_NAME)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .margin_top(12)
            .css_classes(["dim-label"])
            .visible(false)
            .build();
        let partner_message = inline_message(PARTNER_MESSAGE_NAME);
        let machine_group = adw::PreferencesGroup::builder()
            .title("Other Machine")
            .description(MACHINE_NOTE)
            .build();
        machine_group.add(&machine);
        machine_group.add(&auto_sync);
        machine_group.add(&partner_row);
        machine_group.add(&hint);
        machine_group.add(&partner_message);

        let sync_button = gtk::Button::builder()
            .label("Sync")
            .halign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        let result = inline_message(RESULT_NAME);
        result.set_halign(gtk::Align::Center);
        result.set_justify(gtk::Justification::Center);
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        actions.append(&sync_button);
        actions.append(&result);
        let sync_group = adw::PreferencesGroup::new();
        sync_group.add(&actions);

        let widget = adw::PreferencesPage::new();
        widget.add(&switch_group);
        widget.add(&key_group);
        widget.add(&machine_group);
        widget.add(&sync_group);

        let page = Rc::new(SyncPage {
            ctx: Rc::clone(ctx),
            widget,
            key_entry,
            key_status,
            key_message,
            sync_switch,
            machine,
            auto_sync,
            partner_row,
            forget_button,
            partner_message,
            hint,
            sync_button,
            result,
            ids: RefCell::new(Vec::new()),
            key_set: Cell::new(false),
            key_known: Cell::new(false),
            loading: Cell::new(false),
        });
        page.wire();
        page.refresh_key_status();
        page.refresh_targets();
        page.refresh_auto_sync();
        page.refresh_sync_switch();
        page.refresh_partner();
        // A sync started elsewhere (Sync Now, on launch) may be running.
        if ctx.sync_running.get() {
            page.show_result(sync::SYNCING, false);
        }
        page
    }

    /// Connect the widgets' signals and the app's notifications. Every
    /// handler holds the page weakly.
    fn wire(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.sync_switch.connect_active_notify(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.sync_switch_changed();
                }
            }
        ));
        self.key_entry.connect_apply(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.save_key();
                }
            }
        ));
        self.machine.connect_selected_notify(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.update_target();
                }
            }
        ));
        self.auto_sync.connect_active_notify(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.switch_changed();
                }
            }
        ));
        self.sync_button.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(page) = weak.upgrade() {
                    begin_sync(&page);
                }
            }
        ));
        self.forget_button.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.forget();
                }
            }
        ));
        self.ctx.on_profiles_changed(
            &self.widget,
            glib::clone!(
                #[strong]
                weak,
                move || {
                    if let Some(page) = weak.upgrade() {
                        page.refresh_targets();
                        page.refresh_auto_sync();
                        page.refresh_sync_switch();
                        page.refresh_partner();
                    }
                }
            ),
        );
        self.ctx.on_partner_changed(
            &self.widget,
            glib::clone!(
                #[strong]
                weak,
                move || {
                    if let Some(page) = weak.upgrade() {
                        page.refresh_partner();
                        page.refresh_auto_sync();
                    }
                }
            ),
        );
        self.ctx.on_sync_event(&self.widget, move |event| {
            if let Some(page) = weak.upgrade() {
                page.sync_event(event);
            }
        });
    }

    /// Empty the key field: nothing typed stays in a page that goes.
    /// Emptying it is not an edit: the ✓ stays hidden.
    pub(crate) fn clear_secret(&self) {
        self.key_entry.set_show_apply_button(false);
        self.key_entry.set_text("");
        self.key_entry.set_show_apply_button(true);
    }

    /// Id of the profile the "Profile" row shows.
    fn selected_id(&self) -> Option<String> {
        let index = self.machine.selected();
        if index == gtk::INVALID_LIST_POSITION {
            return None;
        }
        self.ids.borrow().get(usize::try_from(index).ok()?).cloned()
    }

    /// Ask the keyring (on its worker) whether a sync key is stored, and show
    /// the answer in the "Sync key: ..." line when it is back. Until the first
    /// answer the line says the keyring is being read, and Sync waits as for no
    /// key; a later refresh keeps showing the previous answer until it has the
    /// new one. Answers come back in the order they were asked for, so the last
    /// one shown is the latest.
    fn refresh_key_status(self: &Rc<Self>) {
        if self.key_status.label().is_empty() {
            self.key_status.set_label(sync::KEY_CHECKING);
        }
        let weak = Rc::downgrade(self);
        keyring::read(&self.ctx, sync::key_state, move |state| {
            let Some(page) = weak.upgrade() else {
                return;
            };
            let state = state.unwrap_or_else(|| KeyState::Unreadable(keyring::STOPPED.to_owned()));
            page.key_status.set_label(&sync::key_status_text(&state));
            page.key_set.set(state == KeyState::Set);
            page.key_known.set(true);
            page.refresh_controls();
        });
    }

    /// Rebuild the profile choice from the store as it is now, keeping the
    /// chosen profile if it is still there, else choosing the partner, else
    /// the first.
    fn refresh_targets(&self) {
        let previous = self.selected_id();
        let (names, ids): (Vec<String>, Vec<String>) = {
            let store = self.ctx.store.borrow();
            store
                .live()
                .into_iter()
                .map(|p| (p.name.clone(), p.id.clone()))
                .unzip()
        };
        let partner = settings::load(&self.ctx.settings_path).sync_partner;
        let position = |id: &String| ids.iter().position(|i| i == id);
        let keep = previous
            .as_ref()
            .and_then(position)
            .or_else(|| partner.as_ref().and_then(position))
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0);
        let empty = ids.is_empty();
        // Ids first: replacing the model fires `notify::selected`.
        *self.ids.borrow_mut() = ids;
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.machine.set_model(Some(&gtk::StringList::new(&names)));
        if !empty {
            self.machine.set_selected(keep);
        }
        self.update_target();
        self.refresh_controls();
    }

    /// "Connects to user@host:port" under the profile's name, so a duplicate
    /// name is still told apart.
    fn update_target(&self) {
        let subtitle = self
            .selected_id()
            .and_then(|id| self.ctx.store.borrow().get(&id).map(form::subtitle));
        let text = subtitle.map_or_else(String::new, |s| format!("Connects to {s}"));
        self.machine.set_subtitle(&text);
    }

    /// The name of the sync partner `settings` record, if its profile exists.
    fn partner_name(&self, settings: &settings::GuiSettings) -> Option<String> {
        let store = self.ctx.store.borrow();
        sync::live_partner(settings, &store).and_then(|id| store.get(&id).map(|p| p.name.clone()))
    }

    /// Set the switch, and its subtitle, to what the settings file says.
    fn refresh_auto_sync(&self) {
        let settings = settings::load(&self.ctx.settings_path);
        let partner_name = self.partner_name(&settings);
        self.auto_sync.set_subtitle(
            &partner_name.map_or_else(|| AUTO_SYNC_NO_PARTNER.to_owned(), |n| auto_sync_with(&n)),
        );
        self.loading.set(true);
        self.auto_sync.set_active(settings.auto_sync);
        self.loading.set(false);
    }

    /// Show the Sync Partner row, with the partner's name, while the settings
    /// file records one whose profile exists; hide it otherwise. A row that
    /// hides while it has the keyboard gives it away ([`Self::keyboard_away`]),
    /// so the keys never go nowhere. It runs when something may have changed
    /// the partner or its profile, never per frame.
    fn refresh_partner(&self) {
        let settings = settings::load(&self.ctx.settings_path);
        let Some(name) = self.partner_name(&settings) else {
            let row: &gtk::Widget = self.partner_row.upcast_ref();
            let had_keyboard = row
                .root()
                .and_then(|root| root.focus())
                .is_some_and(|focus| &focus == row || focus.is_ancestor(row));
            self.partner_row.set_visible(false);
            if had_keyboard {
                self.keyboard_away();
            }
            return;
        };
        self.partner_row.set_subtitle(&name);
        self.partner_row.set_visible(true);
    }

    /// Give the keyboard to the Profile row if it can be used, else to the Sync
    /// switch at the top. Never to the Sync button: Enter pressed again there
    /// would sync with the profile still chosen, the machine just forgotten, and
    /// its success would record it as the partner again.
    fn keyboard_away(&self) {
        if self.machine.is_sensitive() {
            self.machine.grab_focus();
        } else {
            self.sync_switch.grab_focus();
        }
    }

    /// The Forget button: clear the sync partner in the settings file, keeping
    /// every other setting (and leaving a file that cannot be read as it is),
    /// then tell the app, so Sync Now and the rows follow. If the file named a
    /// partner, say which one was forgotten and what that leaves; a second click
    /// (a double click) finds none, and says nothing. A failure is a
    /// notification near the page. Whether it worked or not, everything that
    /// shows the partner reads the file again: a file that cannot be read reads
    /// as the defaults, so they show no partner, as a page opened now would.
    fn forget(&self) {
        // As `begin_sync_with`: the button is grayed out while a sync runs, but a
        // sync's success would record the partner again right after a forget.
        if self.ctx.sync_running.get() {
            return;
        }
        let path = &self.ctx.settings_path;
        let name = self.partner_name(&settings::load(path));
        match sync::forget_partner(path) {
            Ok(()) => {
                if let Some(name) = name {
                    show_message(&self.partner_message, &forgotten(&name), false);
                }
            }
            Err(err) => {
                self.ctx.notice.show_near(
                    &self.widget,
                    &format!("Could not save the sync settings: {err}"),
                );
            }
        }
        self.ctx.partner_changed();
    }

    /// The person flipped the switch: save it. If that fails, say so and put
    /// the switch back to what the file says.
    fn switch_changed(&self) {
        if self.loading.get() {
            return;
        }
        let path = &self.ctx.settings_path;
        let settings = settings::GuiSettings {
            auto_sync: self.auto_sync.is_active(),
            ..settings::load(path)
        };
        if let Err(err) = settings::save(path, &settings) {
            self.ctx.notice.show_near(
                &self.widget,
                &format!("Could not save the sync settings: {err}"),
            );
            self.refresh_auto_sync();
        }
    }

    /// Set the sync switch, and its subtitle, to what the profile file says (as
    /// the context last read it). A switch that could not be read shows off and
    /// says so. While a save of the row is out the row shows what the person
    /// asked for last, not what the file still says
    /// ([`AppContext::ask_sync_switch`]), and the line under it what is so
    /// meanwhile.
    fn refresh_sync_switch(&self) {
        let (active, subtitle) = match self.ctx.asked_sync_switch() {
            Some(asked) => {
                let meanwhile = if self.ctx.sync_is_on() {
                    SyncSwitch::On
                } else {
                    SyncSwitch::Off
                };
                (asked, sync::switch_subtitle(meanwhile))
            }
            None => {
                let switch = self.ctx.store.borrow().sync_switch();
                (switch.is_on(), sync::switch_subtitle(switch))
            }
        };
        self.sync_switch.set_subtitle(subtitle);
        self.loading.set(true);
        self.sync_switch.set_active(active);
        self.loading.set(false);
    }

    /// The person flipped the sync switch. Off is saved at once: it only stops
    /// data leaving. On asks first ([`ask_to_turn_sync_on`]); Cancel, or closing
    /// the alert, puts the row back to what the file says, or to what was asked
    /// for last if a save is still out.
    fn sync_switch_changed(self: &Rc<Self>) {
        if self.loading.get() {
            return;
        }
        if !self.sync_switch.is_active() {
            self.save_sync_switch(false);
            return;
        }
        let (go, cancel) = (Rc::downgrade(self), Rc::downgrade(self));
        ask_to_turn_sync_on(
            &self.sync_switch,
            move || {
                if let Some(page) = go.upgrade() {
                    page.save_sync_switch(true);
                }
            },
            move || {
                if let Some(page) = cancel.upgrade() {
                    page.refresh_sync_switch();
                }
            },
        );
    }

    /// Save the switch in the profile file on the keyring worker (the main
    /// thread never writes it: a sync can hold the file's lock at the keyring).
    /// The row shows `on` meanwhile, and the app counts the request at once
    /// ([`AppContext::ask_sync_switch`]): Sync, Sync Automatically and Sync Now
    /// follow it before the file does. When the save is back, a failure is told
    /// near the page, or in a window if the page has gone, and everything
    /// showing the switch re-reads the file, so the row ends up as the file has
    /// it whether the save worked or not.
    fn save_sync_switch(self: &Rc<Self>, on: bool) {
        self.loading.set(true);
        self.sync_switch.set_active(on);
        self.loading.set(false);
        self.ctx.ask_sync_switch(on);
        let weak = Rc::downgrade(self);
        let ctx = Rc::clone(&self.ctx);
        keyring::write_profiles(
            &self.ctx,
            move |store| {
                store
                    .borrow_mut()
                    .set_sync(on)
                    .map_err(|err| format!("Could not save the sync setting: {err}"))
            },
            move |saved| {
                ctx.sync_switch_saved();
                if let Some(page) = weak.upgrade() {
                    if let Err(message) = &saved {
                        ctx.notice.show_near(&page.widget, message);
                    }
                } else if let Err(message) = &saved {
                    ctx.notice.show(message);
                }
                ctx.profiles_changed();
            },
        );
    }

    /// Enable or disable the controls and show why Sync is unavailable. Until
    /// the keyring has said whether a key is stored, nothing is promised: no
    /// hint, and Sync waits; it waits too while a new key is being saved. A
    /// switch that is off is the exception: the hint says so whatever the
    /// keyring has said, since the file already does.
    fn refresh_controls(&self) {
        let profiles = self.ids.borrow().len();
        let known = self.key_known.get();
        let key_set = self.key_set.get();
        // The switch is in the profile file, which the context keeps current,
        // except that a request to turn it off counts before its save is back.
        let sync_on = self.ctx.sync_is_on();
        let hint = (known || !sync_on)
            .then(|| sync::sync_hint(sync_on, key_set, profiles))
            .flatten();
        self.hint.set_label(hint.unwrap_or(""));
        self.hint.set_visible(hint.is_some());
        // Whether everything else is in place for a sync, as if it were on.
        let set_up = known
            && sync::sync_hint(true, key_set, profiles).is_none()
            && self.key_entry.is_sensitive();
        let idle = !self.ctx.sync_running.get();
        // The whole "Other machine" part waits for what the hint asks for. It
        // does not wait for the switch: choosing sends nothing.
        self.machine.set_sensitive(idle && set_up);
        self.sync_button.set_sensitive(idle && set_up && sync_on);
        self.auto_sync.set_sensitive(sync_on);
        // A page sync's success would record the partner again right after;
        // Sync being off does not matter, as a forget sends nothing.
        self.forget_button.set_sensitive(idle);
    }

    /// Any sync of the app started or ended.
    fn sync_event(&self, event: &SyncEvent) {
        match event {
            SyncEvent::Started => {
                self.show_result(sync::SYNCING, false);
                // This sync may record a partner: the line about the old one
                // must not sit beside it.
                show_message(&self.partner_message, "", false);
            }
            SyncEvent::Finished(result) => {
                let (text, is_error) = sync::result_text(result);
                self.show_result(&text, is_error);
                // The worker found no key: the status line catches up without
                // reading the keyring here.
                if matches!(result, Err(SyncFailure::NoSyncKey)) {
                    self.key_status
                        .set_label(&sync::key_status_text(&KeyState::NotSet));
                    self.key_set.set(false);
                }
                // The sync may have added or removed profiles, or recorded a
                // partner.
                self.refresh_targets();
                self.refresh_auto_sync();
                self.refresh_partner();
            }
        }
        self.refresh_controls();
    }

    /// Save what is in the key field as the sync key: checked at once (a key
    /// that is too short is refused here), stored on the keyring worker. The
    /// field is off until that is back.
    fn save_key(self: &Rc<Self>) {
        // GTK's own copy of the text is out of our hands; ours is wiped on drop.
        let typed = Zeroizing::new(self.key_entry.text().to_string());
        if let Err(message) = sync::check_sync_key(&typed) {
            show_message(&self.key_message, &message, true);
            // The status line catches up with the keyring all the same.
            self.refresh_key_status();
            return;
        }
        // Sync waits too: it would use the key being replaced.
        self.key_entry.set_sensitive(false);
        self.refresh_controls();
        let weak = Rc::downgrade(self);
        let ctx = Rc::clone(&self.ctx);
        keyring::change(
            &self.ctx,
            move |secrets| sync::save_sync_key(secrets, &typed),
            move |saved| {
                let saved = saved.unwrap_or_else(|| {
                    Err(format!(
                        "Could not save the sync key: {}.",
                        keyring::STOPPED
                    ))
                });
                let Some(page) = weak.upgrade() else {
                    // The page has gone: a failure is still told.
                    if let Err(message) = saved {
                        ctx.notice.show(&message);
                    }
                    return;
                };
                page.key_entry.set_sensitive(true);
                match saved {
                    Ok(()) => {
                        page.clear_secret();
                        show_message(&page.key_message, "Sync key saved.", false);
                    }
                    Err(message) => show_message(&page.key_message, &message, true),
                }
                page.refresh_key_status();
            },
        );
    }

    /// Show `text` as the result line (plain text), red for an error.
    fn show_result(&self, text: &str, is_error: bool) {
        show_message(&self.result, text, is_error);
    }
}

impl Drop for SyncPage {
    fn drop(&mut self) {
        self.clear_secret();
    }
}

/// Start a sync with the chosen profile (the Sync button).
///
/// Does nothing while any sync of the app runs (the button is off then). If
/// the profile is gone the page says so at once and shows the profiles as
/// they are now; what the worker finds missing (the key, the askpass helper)
/// comes back as the sync's result. A success records
/// the profile as the sync partner. If the page has gone by the time the sync
/// ends, the result is a notification instead, as nothing else would show it.
fn begin_sync(page: &Rc<SyncPage>) {
    begin_sync_with(page, StaleSync::Refuse);
}

/// [`begin_sync`], going ahead or not when a machine has been away; one that
/// stopped for that asks whether to sync anyway ([`ask_to_sync_anyway`]).
fn begin_sync_with(page: &Rc<SyncPage>, stale: StaleSync) {
    if page.ctx.sync_running.get() {
        return;
    }
    let Some(id) = page.selected_id() else {
        page.show_result("Choose the other machine first.", true);
        return;
    };
    let ctx = Rc::clone(&page.ctx);
    let weak = Rc::downgrade(page);
    let partner = id.clone();
    let started = sync::begin_app_sync(
        &page.ctx,
        &id,
        stale,
        sync::runner(&page.ctx),
        move |result| {
            if result.is_ok()
                && let Err(err) = sync::record_partner(&ctx, &partner)
            {
                ctx.notice
                    .show(&format!("Could not save the sync settings: {err}"));
            }
            if let (Some(page), Err(failure)) = (weak.upgrade(), result)
                && stale == StaleSync::Refuse
                && sync::is_away(failure)
            {
                let again = Rc::downgrade(&page);
                ask_to_sync_anyway(&page.sync_button, &sync::failure_text(failure), move || {
                    if let Some(page) = again.upgrade() {
                        begin_sync_with(&page, StaleSync::Allow);
                    }
                });
            } else if weak.upgrade().is_none() {
                let (text, is_error) = sync::result_text(result);
                ctx.notice.show(&if is_error {
                    format!("Sync failed: {text}")
                } else {
                    text
                });
            }
        },
    );
    if let Err(message) = started {
        page.show_result(&message, true);
        // The profile list may be stale (that profile is gone).
        page.refresh_targets();
    }
}

/// After a sync stopped because this machine or the other one has been away
/// (issue #8): ask, over `near`, whether to sync anyway, saying why (`why`,
/// plain text); `go` runs on Sync Anyway. Sync Now uses it too.
pub(crate) fn ask_to_sync_anyway(near: &impl IsA<gtk::Widget>, why: &str, go: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::builder()
        .heading("Sync Anyway?")
        .heading_use_markup(false)
        .body(why)
        .body_use_markup(false)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("sync", "Sync Anyway");
    dialog.set_response_appearance("sync", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("sync"), move |_, _| go());
    dialog.present(Some(near));
}

/// Before turning sync back on (issue #10), over `near`: say what the next sync
/// sends, and ask. `go` runs on Turn Sync On, `cancel` on Cancel and when the
/// alert is closed. All text is plain.
pub(crate) fn ask_to_turn_sync_on(
    near: &impl IsA<gtk::Widget>,
    go: impl Fn() + 'static,
    cancel: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Turn Sync On?")
        .heading_use_markup(false)
        .body(TURN_ON_BODY)
        .body_use_markup(false)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("on", "Turn Sync On");
    dialog.set_response_appearance("on", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("on"), move |_, _| go());
    dialog.connect_response(Some("cancel"), move |_, _| cancel());
    dialog.present(Some(near));
}

/// A list item factory for a string list whose labels show the text as it is
/// (Pango markup off): the entries are profile names, or the names of a
/// profile's sequences (the profile page's default sequence row uses it too).
pub(super) fn plain_text_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::builder()
            .use_markup(false)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string())
            .unwrap_or_default();
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            label.set_label(&text);
        }
    });
    factory
}

fn show_message(label: &gtk::Label, text: &str, is_error: bool) {
    label.set_label(text);
    label.set_visible(!text.is_empty());
    if is_error {
        label.add_css_class("error");
    } else {
        label.remove_css_class("error");
    }
}

/// A selectable, wrapping, initially hidden line for messages.
fn inline_message(name: &str) -> gtk::Label {
    gtk::Label::builder()
        .name(name)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .selectable(true)
        .margin_top(12)
        .visible(false)
        .build()
}
