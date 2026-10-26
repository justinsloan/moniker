//! The profile page: the content pane of the Connection Profiles window for one
//! profile.
//!
//! Every field saves on its own ([`form::apply_field`]): a text row only when
//! its ✓ is clicked or Enter is pressed, the port once it has been left alone
//! for a moment or focus moves on, the switch and the key file as soon as they
//! change. Nothing half-typed reaches the disk or the keyring, and leaving a
//! row with an unapplied edit discards it ([`ProfilePage::focus_moved`]). A
//! failed save marks the row with the `error` style, explains in a
//! notification, and writes nothing.
//!
//! The saves run on the keyring worker ([`keyring::write_profiles`]), never on
//! the main thread: a sync that waits for the keyring holds the profile file,
//! and the window must not wait with it (issue #7). They run in the order
//! asked. While a row's save is under way ([`Saving`]) the row keeps the focus
//! and what it shows, a refresh does not put the file's value back, a text row
//! takes no more typing, and after a moment a spinner says it waits; the port,
//! the switch and the key file can change again, which is a further save after
//! it. When a save is back, every list and this page show the file as it is
//! then, so a failed save's switch or port goes back to what is stored. A
//! failure is still told when the page has gone.
//!
//! "Sync to other machines" is the one switch that asks first (issue #10):
//! keeping a profile on this machine only deletes it, password included, from
//! the other machines at their next sync, and syncing it again copies it out
//! with its password. The row waits (insensitive) from the flip until the
//! question is answered and, on yes, its save is back ([`local_only`]).
//!
//! The startup sequences are listed here and edited on a sub-page of their own
//! ([`sequence_page`]) that opens over this one. Below them a combo row, "Default
//! sequence", names the one that runs every time the profile connects (issue
//! #12; the page does not show it for a profile with no sequence). Its list is
//! "None" and then the sequences by name, made again whenever they change (a
//! sequence added, renamed, moved or deleted on the sub-page, or arriving by
//! sync), and only a pick by the person saves; showing what the file says does
//! not (`default_loading`). A pick saves like any other field.
//!
//! The page never reads the keyring: it cannot show whether a password is
//! stored. A Secret Service call on every selection could prompt with a
//! locked keyring. What it does write there (a new password, removing the
//! stored one, deleting the profile's) goes to the keyring worker
//! ([`keyring::change`]), never the main thread, and the outcome is shown when it
//! is back.
//!
//! Profile data (names, hosts, user names, sequence names, key-file paths) is
//! shown with Pango markup off everywhere: it can come from another machine.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::secrets::password_key;
use zeroize::Zeroizing;

use super::saving::Saving;
use super::sequence_page;
use super::sync_page::plain_text_factory;
use crate::context::AppContext;
use crate::form::{self, Field};
use crate::keyring;
use crate::local_only;

/// The startup-sequence group's description: what can leak.
pub(crate) const SECRETS_WARNING: &str = "\
Startup commands are saved as plain text in the profile file, and a transcript \
records whatever appears in the terminal, including anything you type or paste. \
Keep passwords out of both.";

/// The subtitle of the "Save transcript" switch.
pub(crate) const TRANSCRIPT_WARNING: &str = "\
Saves the session as plain text when its tab closes, including anything you \
type or paste. Keep passwords out of it.";

/// The Authentication group's description: an empty password field keeps the
/// stored password (the page cannot show whether there is one).
pub(crate) const PASSWORD_HINT: &str = "Leave the password empty to keep the stored one.";

/// How long the port must stay unchanged before it is saved.
const PORT_SETTLE: Duration = Duration::from_secs(1);

/// The first entry of the "Default sequence" row: no default.
const DEFAULT_NONE: &str = "None";

/// The page for one profile. Dropping it drops its state; the widgets go
/// with the page widget.
pub(super) struct ProfilePage {
    pub widget: adw::PreferencesPage,
    inner: Rc<Inner>,
}

/// A text row and the value it stands for.
struct TextRow {
    row: adw::EntryRow,
    /// The stored value: what the row shows when it has no unapplied edit.
    stored: RefCell<String>,
    value: fn(&Profile) -> String,
    field: fn(String) -> Field,
    saving: Rc<Saving>,
}

/// Which text row.
#[derive(Clone, Copy)]
enum Text {
    Name,
    Host,
    Username,
}

struct Inner {
    ctx: Rc<AppContext>,
    id: String,
    name: TextRow,
    host: TextRow,
    username: TextRow,
    port: adw::SpinRow,
    /// The port the file has, or will have once the saves asked for are back.
    port_stored: Cell<u16>,
    /// The pending save of a changed port, if one is waiting to settle.
    port_timer: RefCell<Option<glib::SourceId>>,
    port_saving: Rc<Saving>,
    password: adw::PasswordEntryRow,
    key_file: adw::ActionRow,
    key_clear: gtk::Button,
    key_saving: Rc<Saving>,
    sequences: adw::PreferencesGroup,
    sequence_rows: RefCell<Vec<adw::ActionRow>>,
    /// What the sequence rows show (each name and its number of commands), so
    /// that they are made again only when that changes, and a refresh that
    /// brings nothing new leaves the rows, and the keyboard on them, alone.
    sequences_shown: RefCell<Option<Vec<(String, usize)>>>,
    /// "Default sequence": "None", then each sequence by name. In the group
    /// between the sequence rows and "Add sequence" while there is a sequence.
    default_row: adw::ComboRow,
    /// The sequence names the row offers after "None", to make its list again
    /// only when they change.
    default_names: RefCell<Vec<String>>,
    /// What the file says, or will say once the saves asked for are back: the
    /// last pick, while one is being saved.
    default_stored: Cell<Option<usize>>,
    default_saving: Rc<Saving>,
    /// The row is being set to what the file says (or its list is being made
    /// again, which moves the selection), not picked by the person: that saves
    /// nothing.
    default_loading: Cell<bool>,
    transcript: adw::SwitchRow,
    /// What the file says, or will say once the saves asked for are back.
    transcript_stored: Cell<bool>,
    transcript_saving: Rc<Saving>,
    /// "Sync to other machines" (issue #10). Off means the profile is kept on
    /// this machine only, which deletes it from the others: both ways ask first.
    sync_here: adw::SwitchRow,
    /// Whether the profile is kept here, as the file said at the last refresh:
    /// what a cancelled question goes back to.
    kept_stored: Cell<bool>,
    /// The row is being set to what the file says, not flipped by the person.
    sync_loading: Cell<bool>,
    /// What the person asked for ("kept here" or not), from the flip until its
    /// save is back (the question, then the save). The row is insensitive all
    /// that time, a flip that gets in anyway is put back, and a refresh leaves
    /// the row alone.
    sync_asked: Cell<Option<bool>>,
    sync_saving: Rc<Saving>,
    /// The question on screen, if there is one. A page that goes away closes
    /// it ([`ProfilePage`]'s `Drop`): its answer would have no page to act on.
    sync_question: RefCell<Option<adw::AlertDialog>>,
    /// Where the keyboard is while the row waits (see [`RowFocus`]).
    sync_focus: RowFocus,
}

/// The keyboard while "Sync to other machines" waits. The question that opens
/// over the row takes the keyboard, and the row goes insensitive, which does
/// too, so the page notes whether the row had it and gives it back
/// ([`Inner::settle_focus`]) once the row can take it: the question has closed
/// and the row is sensitive again (Cancel, or the save is back).
#[derive(Default)]
struct RowFocus {
    /// The keyboard was on the row when it was flipped.
    had: Cell<bool>,
    /// The question is still on screen.
    question_open: Cell<bool>,
    /// Where the keyboard was left when the question closed, noted only when
    /// the row had it (`None` otherwise: no widget is kept alive for nothing).
    /// The row gets it back only if it is still there: a person who moved on
    /// while the save was out is not pulled back.
    parked: RefCell<Option<gtk::Widget>>,
}

impl ProfilePage {
    /// Build the page for `profile`.
    pub fn new(ctx: &Rc<AppContext>, profile: &Profile) -> ProfilePage {
        let widget = adw::PreferencesPage::new();
        let text_row = |title: &str, value: fn(&Profile) -> String, field: fn(String) -> Field| {
            let row = adw::EntryRow::builder()
                .title(title)
                .use_markup(false)
                .show_apply_button(true)
                .text(value(profile))
                .build();
            let saving = Saving::new();
            row.add_suffix(&saving.spinner);
            TextRow {
                row,
                stored: RefCell::new(value(profile)),
                value,
                field,
                saving,
            }
        };
        let name = text_row("Name", |p| p.name.clone(), Field::Name);
        let host = text_row("Host", |p| p.host.clone(), Field::Host);
        let username = text_row("Username", |p| p.username.clone(), Field::Username);
        let port = adw::SpinRow::builder()
            .title("Port")
            .adjustment(&gtk::Adjustment::new(
                f64::from(profile.port),
                1.0,
                65535.0,
                1.0,
                10.0,
                0.0,
            ))
            .numeric(true)
            .build();
        let port_saving = Saving::new();
        port.add_suffix(&port_saving.spinner);
        let connection = adw::PreferencesGroup::builder().title("Connection").build();
        connection.add(&name.row);
        connection.add(&host.row);
        connection.add(&username.row);
        connection.add(&port);
        widget.add(&connection);

        let password = adw::PasswordEntryRow::builder()
            .title("Password")
            .use_markup(false)
            .show_apply_button(true)
            .build();
        let remove = gtk::Button::builder()
            .label("Remove")
            .valign(gtk::Align::Center)
            .build();
        let remove_row = adw::ActionRow::builder()
            .title(form::REMOVE_PASSWORD_LABEL)
            .subtitle(form::REMOVE_PASSWORD_NOTE)
            .use_markup(false)
            .build();
        remove_row.add_suffix(&remove);
        let choose = gtk::Button::builder()
            .label("Choose\u{2026}")
            .valign(gtk::Align::Center)
            .build();
        let key_clear = gtk::Button::builder()
            .label("Clear")
            .valign(gtk::Align::Center)
            .build();
        let key_file = adw::ActionRow::builder()
            .title("Key file")
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        let key_saving = Saving::new();
        key_file.add_suffix(&key_saving.spinner);
        key_file.add_suffix(&choose);
        key_file.add_suffix(&key_clear);
        let authentication = adw::PreferencesGroup::builder()
            .title("Authentication")
            .description(PASSWORD_HINT)
            .build();
        authentication.add(&password);
        authentication.add(&remove_row);
        authentication.add(&key_file);
        widget.add(&authentication);

        // One row per sequence, each opening its sub-page, and a row to add one.
        let sequences = adw::PreferencesGroup::builder()
            .title("Startup sequences")
            .description(SECRETS_WARNING)
            .build();
        widget.add(&sequences);
        // Its list is made by `show_default`; names are data: plain text.
        let default_row = adw::ComboRow::builder()
            .title("Default sequence")
            .subtitle("Runs every time this profile connects")
            .use_markup(false)
            .factory(&plain_text_factory())
            .model(&gtk::StringList::new(&[DEFAULT_NONE]))
            .build();
        let default_saving = Saving::new();
        default_row.add_suffix(&default_saving.spinner);

        let transcript = adw::SwitchRow::builder()
            .title("Save transcript")
            .subtitle(TRANSCRIPT_WARNING)
            .use_markup(false)
            .active(profile.transcript)
            .build();
        let transcript_saving = Saving::new();
        transcript.add_suffix(&transcript_saving.spinner);
        let session = adw::PreferencesGroup::builder().title("Session").build();
        session.add(&transcript);
        widget.add(&session);

        // Its subtitle and state are set by `set_sync_here`.
        let sync_here = adw::SwitchRow::builder()
            .title(local_only::ROW_TITLE)
            .use_markup(false)
            .build();
        let sync_saving = Saving::new();
        sync_here.add_suffix(&sync_saving.spinner);
        let sync = adw::PreferencesGroup::builder().title("Sync").build();
        sync.add(&sync_here);
        widget.add(&sync);

        let delete = gtk::Button::builder()
            .label("Delete Profile")
            .halign(gtk::Align::Center)
            .css_classes(["destructive-action", "pill"])
            .build();
        let danger = adw::PreferencesGroup::new();
        danger.add(&delete);
        widget.add(&danger);

        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            id: profile.id.clone(),
            name,
            host,
            username,
            port,
            port_stored: Cell::new(profile.port),
            port_timer: RefCell::new(None),
            port_saving,
            password,
            key_file,
            key_clear,
            key_saving,
            sequences,
            sequence_rows: RefCell::new(Vec::new()),
            sequences_shown: RefCell::new(None),
            default_row,
            default_names: RefCell::new(Vec::new()),
            default_stored: Cell::new(profile.startup_choice()),
            default_saving,
            default_loading: Cell::new(false),
            transcript,
            transcript_stored: Cell::new(profile.transcript),
            transcript_saving,
            sync_here,
            kept_stored: Cell::new(profile.local_only),
            sync_loading: Cell::new(false),
            sync_asked: Cell::new(None),
            sync_saving,
            sync_question: RefCell::new(None),
            sync_focus: RowFocus::default(),
        });
        inner.set_sync_here(profile.local_only);
        inner.show_key_file(profile.key_file.as_ref());
        inner.show_sequences(profile);
        inner.connect(&remove, &choose, &delete);
        ProfilePage { widget, inner }
    }

    /// Show `profile` (the same one, as it is in the file now) in every row
    /// that has no unapplied edit and no save under way. A text row with an
    /// unapplied edit (or one being saved) keeps its text; what it would go
    /// back to becomes the new stored value. A port, switch or key file being
    /// saved keeps what it shows, which is what the file will hold.
    pub fn refresh(&self, profile: &Profile) {
        let inner = &self.inner;
        for text in inner.text_rows() {
            let shows_stored = text.row.text() == *text.stored.borrow();
            let value = (text.value)(profile);
            if shows_stored {
                set_text_quietly(&text.row, &value);
            }
            text.stored.replace(value);
        }
        if !inner.port_saving.busy() {
            inner.port_stored.set(profile.port);
            if inner.port_timer.borrow().is_none() {
                inner.port.set_value(f64::from(profile.port));
            }
        }
        if !inner.transcript_saving.busy() {
            inner.transcript_stored.set(profile.transcript);
            inner.transcript.set_active(profile.transcript);
        }
        if !inner.key_saving.busy() {
            inner.show_key_file(profile.key_file.as_ref());
        }
        // A question or a save of the row under way keeps what the person
        // asked for on show; a question that is cancelled meanwhile goes back
        // to what the file says now.
        inner.kept_stored.set(profile.local_only);
        if inner.sync_asked.get().is_none() {
            inner.set_sync_here(profile.local_only);
        }
        inner.show_sequences(profile);
    }

    /// The window's focus moved to `focus`. A row that focus has left drops
    /// its unapplied edit (the password row empties); a changed port that
    /// focus has left is saved now.
    pub fn focus_moved(&self, focus: Option<&gtk::Widget>) {
        let inner = &self.inner;
        let left = |row: &gtk::Widget| !focus.is_some_and(|f| f == row || f.is_ancestor(row));
        for text in inner.text_rows() {
            // A row being saved has no unapplied edit: it was applied.
            if left(text.row.upcast_ref()) && !text.saving.busy() {
                set_text_quietly(&text.row, &text.stored.borrow());
                text.row.remove_css_class("error");
            }
        }
        if left(inner.password.upcast_ref()) {
            inner.clear_password();
        }
        if left(inner.port.upcast_ref()) && inner.port_timer.borrow().is_some() {
            inner.save_port();
        }
    }

    /// Empty the password row (the window is closing or this page goes away).
    pub fn clear_secret(&self) {
        self.inner.clear_password();
    }

    /// Save a port change that is still settling, now: the window is closing
    /// or the page is going away, and a changed port is a choice the person
    /// already made (it has no ✓ to wait for). It is back once the page has
    /// gone, from the main loop: the windows then refresh, or the message goes
    /// to whichever window is still there to show it.
    pub fn flush(&self) {
        if self.inner.port_timer.borrow().is_some() {
            self.inner.save_port();
        }
    }
}

impl Drop for ProfilePage {
    /// A page that goes away (another profile selected, the window closed, its
    /// profile deleted by a sync) saves a settling port change, keeps no typed
    /// password, and takes the "Sync to other machines" question with it, so
    /// that no button is left on screen that does nothing.
    fn drop(&mut self) {
        self.flush();
        self.inner.clear_password();
        let question = self.inner.sync_question.take();
        if let Some(question) = question {
            question.force_close();
        }
    }
}

impl Inner {
    fn text_rows(&self) -> [&TextRow; 3] {
        [&self.name, &self.host, &self.username]
    }

    fn text(&self, which: Text) -> &TextRow {
        match which {
            Text::Name => &self.name,
            Text::Host => &self.host,
            Text::Username => &self.username,
        }
    }

    /// Wire the rows and buttons. Every handler holds the page weakly: the
    /// page owns the widgets the handlers are attached to.
    fn connect(self: &Rc<Self>, remove: &gtk::Button, choose: &gtk::Button, delete: &gtk::Button) {
        let weak = Rc::downgrade(self);
        let with = move |f: fn(&Rc<Inner>)| {
            let weak: Weak<Inner> = weak.clone();
            move || {
                if let Some(inner) = weak.upgrade() {
                    f(&inner);
                }
            }
        };

        let apply = with(|inner| inner.apply_text(Text::Name));
        self.name.row.connect_apply(move |_| apply());
        let apply = with(|inner| inner.apply_text(Text::Host));
        self.host.row.connect_apply(move |_| apply());
        let apply = with(|inner| inner.apply_text(Text::Username));
        self.username.row.connect_apply(move |_| apply());
        let apply = with(|inner| inner.apply_password());
        self.password.connect_apply(move |_| apply());

        let changed = with(|inner| inner.port_changed());
        self.port.connect_value_notify(move |_| changed());
        let changed = with(|inner| inner.transcript_changed());
        self.transcript.connect_active_notify(move |_| changed());
        let changed = with(|inner| inner.sync_here_changed());
        self.sync_here.connect_active_notify(move |_| changed());
        let picked = with(|inner| inner.default_picked());
        self.default_row.connect_selected_notify(move |_| picked());

        let ask = with(|inner| inner.ask_remove_password());
        remove.connect_clicked(move |_| ask());
        let ask = with(|inner| inner.choose_key_file());
        choose.connect_clicked(move |_| ask());
        let clear = with(|inner| inner.save_key_file(None));
        self.key_clear.connect_clicked(move |_| clear());
        let ask = with(|inner| inner.ask_delete());
        delete.connect_clicked(move |_| ask());
    }

    /// Save one field on the keyring worker, after every change asked for
    /// before it, while `saving` says that `row` waits (module docs). When it
    /// is back: on failure `row` is marked and the message shown near it, or
    /// in a window if the page has gone; `then` gets the result if the page is
    /// still there; and everything showing profiles refreshes.
    fn save(
        self: &Rc<Self>,
        row: &impl IsA<gtk::Widget>,
        saving: &Rc<Saving>,
        field: Field,
        then: impl FnOnce(&Rc<Inner>, &Result<Profile, String>) + 'static,
    ) {
        saving.begin();
        let id = self.id.clone();
        let ctx = Rc::clone(&self.ctx);
        let weak = Rc::downgrade(self);
        let row = row.clone().upcast::<gtk::Widget>();
        let saving = Rc::clone(saving);
        keyring::write_profiles(
            &self.ctx,
            move |store| form::apply_field(store, &id, field),
            move |result| {
                saving.end();
                match weak.upgrade() {
                    Some(inner) => {
                        match &result {
                            Ok(_) => row.remove_css_class("error"),
                            Err(message) => {
                                row.add_css_class("error");
                                ctx.notice.show_near(&row, message);
                            }
                        }
                        then(&inner, &result);
                    }
                    // The page has gone (another profile selected, the window
                    // closed) while the save waited: a failure is still told.
                    None => {
                        if let Err(message) = &result {
                            ctx.notice.show(message);
                        }
                    }
                }
                ctx.profiles_changed();
            },
        );
    }

    /// Save what was typed in a text row. The row keeps the focus but takes no
    /// more typing until the save is back.
    fn apply_text(self: &Rc<Self>, which: Text) {
        let text = self.text(which);
        if text.saving.busy() {
            return;
        }
        let typed = text.row.text().to_string();
        text.row.set_editable(false);
        let field = (text.field)(typed);
        self.save(&text.row, &text.saving, field, move |inner, result| {
            let text = inner.text(which);
            text.row.set_editable(true);
            if let Ok(profile) = result {
                // Show what was stored (trimmed) before anyone refreshes.
                let value = (text.value)(profile);
                set_text_quietly(&text.row, &value);
                text.stored.replace(value);
            }
        });
    }

    /// Store the typed password ([`form::apply_password`]: the keyring first,
    /// then the profile file again) on the keyring worker. The row is off until
    /// that is back; empty means keep the stored one, and nothing is done.
    fn apply_password(self: &Rc<Self>) {
        // Copied once out of GTK into a wrapper that wipes it on drop.
        let typed = Zeroizing::new(self.password.text().to_string());
        if typed.is_empty() {
            self.clear_password();
            return;
        }
        self.password.set_sensitive(false);
        let store_path = self.ctx.store.borrow().path().to_path_buf();
        let id = self.id.clone();
        let ctx = Rc::clone(&self.ctx);
        let weak = Rc::downgrade(self);
        keyring::change(
            &self.ctx,
            move |secrets| {
                let store = keyring::own_store(&store_path)?;
                form::apply_password(&store, secrets, &id, &typed)
            },
            move |result| {
                let result = result.unwrap_or_else(|| {
                    Err(format!(
                        "Could not store the password in the keyring: {}.",
                        keyring::STOPPED
                    ))
                });
                if result.is_ok() {
                    // The file was written again, page or no page.
                    ctx.profiles_changed();
                }
                let Some(inner) = weak.upgrade() else {
                    // The page has gone (the person moved on while the
                    // keyring waited): a failure is still told.
                    if let Err(message) = result {
                        ctx.notice.show(&message);
                    }
                    return;
                };
                inner.password.set_sensitive(true);
                match result {
                    Ok(()) => inner.clear_password(),
                    Err(message) => {
                        inner.password.add_css_class("error");
                        inner.ctx.notice.show_near(&inner.password, &message);
                    }
                }
            },
        );
    }

    fn clear_password(&self) {
        set_text_quietly(&self.password, "");
        self.password.remove_css_class("error");
    }

    /// The port changed (by the person or a refresh): save it once it has
    /// stayed the same for [`PORT_SETTLE`].
    fn port_changed(self: &Rc<Self>) {
        if let Some(timer) = self.port_timer.take() {
            timer.remove();
        }
        if self.port_value() == self.port_stored.get() {
            return;
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(PORT_SETTLE, move || {
            if let Some(inner) = weak.upgrade() {
                // It fired: forget it before saving, so nothing removes it.
                inner.port_timer.take();
                inner.save_port();
            }
        });
        self.port_timer.replace(Some(timer));
    }

    fn save_port(self: &Rc<Self>) {
        if let Some(timer) = self.port_timer.take() {
            timer.remove();
        }
        let port = self.port_value();
        if port == self.port_stored.get() {
            return;
        }
        self.port_stored.set(port);
        self.save(&self.port, &self.port_saving, Field::Port(port), |_, _| {});
    }

    /// The spin row is limited to 1..=65535; 0 would fail validation anyway.
    fn port_value(&self) -> u16 {
        u16::try_from(self.port.value().round() as i64).unwrap_or(0)
    }

    /// The switch changed: save it. If that fails, the refresh once it is back
    /// puts the switch back to what is stored: a switch that shows an unsaved
    /// state would mislead.
    fn transcript_changed(self: &Rc<Self>) {
        let on = self.transcript.is_active();
        if on == self.transcript_stored.get() {
            return;
        }
        self.transcript_stored.set(on);
        self.save(
            &self.transcript,
            &self.transcript_saving,
            Field::Transcript(on),
            |_, _| {},
        );
    }

    /// Show the profile as kept here (`kept`, the switch off) or syncing, with
    /// the subtitle that says so, without that counting as a flip.
    fn set_sync_here(&self, kept: bool) {
        self.sync_loading.set(true);
        self.sync_here.set_active(!kept);
        self.sync_loading.set(false);
        self.sync_here.set_subtitle(if kept {
            local_only::KEPT_SUBTITLE
        } else {
            local_only::SYNCS_SUBTITLE
        });
    }

    /// The person flipped "Sync to other machines". Either way changes what
    /// the other machines hold, and a copied-out password cannot be called back,
    /// so nothing is saved until the question is answered
    /// ([`local_only::keep_here_dialog`], [`local_only::sync_again_dialog`]). The
    /// row is insensitive from here until the answer and, on yes, its save are
    /// back: a second flip waits for the first. Cancel, or closing the question,
    /// puts the switch back.
    fn sync_here_changed(self: &Rc<Self>) {
        if self.sync_loading.get() {
            return;
        }
        let kept = !self.sync_here.is_active();
        if let Some(asked) = self.sync_asked.get() {
            // The row is insensitive, so only code gets here: put it back.
            self.set_sync_here(asked);
            return;
        }
        if kept == self.kept_stored.get() {
            return;
        }
        // Bind first so the store borrow ends here.
        let name = self
            .ctx
            .store
            .borrow()
            .get(&self.id)
            .map(|p| p.name.clone());
        let Some(name) = name else {
            self.ctx.notice.show_near(&self.sync_here, form::GONE);
            self.set_sync_here(self.kept_stored.get());
            return;
        };
        let (dialog, yes) = if kept {
            (local_only::keep_here_dialog(&name), "keep")
        } else {
            (local_only::sync_again_dialog(&name), "sync")
        };
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some(yes), {
            let weak = weak.clone();
            move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.save_sync_here(kept);
                }
            }
        });
        dialog.connect_response(Some("cancel"), {
            let weak = weak.clone();
            move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.sync_asked.set(None);
                    inner.set_sync_here(inner.kept_stored.get());
                    inner.sync_here.set_sensitive(true);
                    inner.settle_focus();
                }
            }
        });
        // `closed` and the response come in an order that is the dialog's
        // business (a response follows `closed`), so each of them settles the
        // keyboard once everything it waits for has happened.
        dialog.connect_closed(move |_| {
            if let Some(inner) = weak.upgrade() {
                let focus = &inner.sync_focus;
                focus.question_open.set(false);
                inner.sync_question.take();
                // Only a row that had the keyboard is given it back, so only
                // then is there anything to note (and to keep alive).
                focus
                    .parked
                    .replace(focus.had.get().then(|| inner.focus_widget()).flatten());
                inner.settle_focus();
            }
        });
        // The question takes the keyboard, so note first where it was (the
        // window's focus widget, whether or not the window is the active one).
        let row: &gtk::Widget = self.sync_here.upcast_ref();
        self.sync_focus.had.set(
            row.root()
                .and_then(|root| root.focus())
                .is_some_and(|focus| &focus == row || focus.is_ancestor(row)),
        );
        self.sync_focus.question_open.set(true);
        self.sync_asked.set(Some(kept));
        self.sync_question.replace(Some(dialog.clone()));
        dialog.present(Some(&self.sync_here));
        self.sync_here.set_sensitive(false);
    }

    /// Save the answer "yes" on the keyring worker like any other field. The
    /// row shows it meanwhile and stays insensitive until the save is back;
    /// then it shows what the file has, so a save that failed (and told why)
    /// puts the switch back.
    fn save_sync_here(self: &Rc<Self>, kept: bool) {
        self.set_sync_here(kept);
        self.save(
            &self.sync_here,
            &self.sync_saving,
            Field::LocalOnly(kept),
            |inner, _| {
                inner.sync_asked.set(None);
                inner.sync_here.set_sensitive(true);
                inner.settle_focus();
            },
        );
    }

    /// The window's focus widget (`None`: nothing has the keyboard).
    fn focus_widget(&self) -> Option<gtk::Widget> {
        self.sync_here.root().and_then(|root| root.focus())
    }

    /// Give the keyboard back to the row, once every one of these holds: it
    /// had it before the question, the question has closed, and the row is
    /// sensitive again (no answer or save is out). It gets it only if it is
    /// still where the closing question left it. Called whenever one of them
    /// has just become true.
    fn settle_focus(&self) {
        let focus = &self.sync_focus;
        if !focus.had.get() || focus.question_open.get() || self.sync_asked.get().is_some() {
            return;
        }
        focus.had.set(false);
        if self.focus_widget() == focus.parked.take() {
            self.sync_here.grab_focus();
        }
    }

    fn show_key_file(&self, key_file: Option<&PathBuf>) {
        let shown = key_file.map_or_else(|| "None".to_owned(), |p| p.display().to_string());
        self.key_file.set_subtitle(&shown);
        self.key_clear.set_sensitive(key_file.is_some());
    }

    /// Show the key file chosen (or none) and save it.
    fn save_key_file(self: &Rc<Self>, path: Option<PathBuf>) {
        self.show_key_file(path.as_ref());
        self.save(
            &self.key_file,
            &self.key_saving,
            Field::KeyFile(path),
            |_, _| {},
        );
    }

    /// Let the person pick a key file, then save it.
    fn choose_key_file(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose a private key file")
            .modal(true)
            .build();
        let ssh_dir = glib::home_dir().join(".ssh");
        if ssh_dir.is_dir() {
            dialog.set_initial_folder(Some(&gio::File::for_path(ssh_dir)));
        }
        let parent = self.key_file.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.open(parent.as_ref(), gio::Cancellable::NONE, move |result| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let problem = match result {
                Ok(file) => match file.path() {
                    Some(path) if path.to_str().is_some() => {
                        inner.save_key_file(Some(path));
                        return;
                    }
                    Some(_) => "That file's path is not valid UTF-8, so it cannot be saved.",
                    None => "Please choose a file on this computer.",
                },
                // Closing the chooser without picking is not an error.
                Err(err)
                    if err.matches(gtk::DialogError::Dismissed)
                        || err.matches(gtk::DialogError::Cancelled) =>
                {
                    return;
                }
                Err(err) => {
                    let message = format!("Could not choose the file: {err}");
                    inner.ctx.notice.show_near(&inner.key_file, &message);
                    return;
                }
            };
            inner.ctx.notice.show_near(&inner.key_file, problem);
        });
    }

    /// The sequence rows and the default sequence row for `profile`. The rows
    /// are made again only when what they show has changed.
    fn show_sequences(self: &Rc<Self>, profile: &Profile) {
        let shown: Vec<(String, usize)> = profile
            .sequences
            .iter()
            .map(|sequence| (sequence.name.clone(), sequence.commands.len()))
            .collect();
        if self.sequences_shown.borrow().as_ref() != Some(&shown) {
            self.make_sequence_rows(profile);
            self.sequences_shown.replace(Some(shown));
        }
        self.show_default(profile);
    }

    /// One row per sequence (name and number of commands), each opening the
    /// sequence's sub-page, then, if there is a sequence, the "Default
    /// sequence" row, then a row that adds one.
    fn make_sequence_rows(self: &Rc<Self>, profile: &Profile) {
        for row in self.sequence_rows.take() {
            self.sequences.remove(&row);
        }
        // The group knows only its own children: the default row is one of
        // them only if it was put there.
        if self.default_row.parent().is_some() {
            self.sequences.remove(&self.default_row);
        }
        let weak = Rc::downgrade(self);
        let opens = |row: &adw::ActionRow, index: Option<usize>| {
            let weak: Weak<Inner> = weak.clone();
            row.connect_activated(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.open_sequence(index);
                }
            });
        };
        let mut rows = Vec::new();
        for (index, sequence) in profile.sequences.iter().enumerate() {
            let count = sequence.commands.len();
            let subtitle = if count == 1 {
                "1 command".to_owned()
            } else {
                format!("{count} commands")
            };
            let row = adw::ActionRow::builder()
                .title(sequence.name.as_str())
                .subtitle(subtitle)
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            opens(&row, Some(index));
            rows.push(row);
        }
        if rows.is_empty() {
            rows.push(
                adw::ActionRow::builder()
                    .title("None: connecting gives a plain shell")
                    .use_markup(false)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        let add = adw::ActionRow::builder()
            .title("Add sequence")
            .use_markup(false)
            .activatable(true)
            .build();
        add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        opens(&add, None);
        rows.push(add);
        // The sequences (or the note that there are none), the default row
        // when there is a sequence to choose, and the row that adds one.
        let last = rows.len() - 1;
        for (place, row) in rows.iter().enumerate() {
            if place == last && !profile.sequences.is_empty() {
                self.sequences.add(&self.default_row);
            }
            self.sequences.add(row);
        }
        self.sequence_rows.replace(rows);
    }

    /// Show the profile's default sequence in its row, and make the row's list
    /// again if the sequence names changed. Nothing here counts as a pick: the
    /// row saves only what the person chooses
    /// ([`default_picked`](Self::default_picked)). While a pick is being saved
    /// the row keeps showing it; once it is back the refresh shows the file's
    /// choice, which puts a failed pick back.
    fn show_default(&self, profile: &Profile) {
        if !self.default_saving.busy() {
            self.default_stored.set(profile.startup_choice());
        }
        let names: Vec<String> = profile
            .sequences
            .iter()
            .map(|sequence| sequence.name.clone())
            .collect();
        self.default_loading.set(true);
        if *self.default_names.borrow() != names {
            let items: Vec<&str> = std::iter::once(DEFAULT_NONE)
                .chain(names.iter().map(String::as_str))
                .collect();
            self.default_row
                .set_model(Some(&gtk::StringList::new(&items)));
            self.default_names.replace(names);
        }
        // A pick that names a place the list no longer has shows as "None".
        let place = self
            .default_stored
            .get()
            .filter(|&index| index < self.default_names.borrow().len())
            .map_or(0, |index| index + 1);
        let place = u32::try_from(place).unwrap_or(0);
        if self.default_row.selected() != place {
            self.default_row.set_selected(place);
        }
        self.default_loading.set(false);
    }

    /// The person picked an entry of the "Default sequence" row: save it, as
    /// the sequence's place in the list (`None` for "None"). The file has the
    /// last word: a place it does not have (a sync took the sequence away) is
    /// refused ([`Field::StartupSequence`]) and the row goes back to what the
    /// file says.
    fn default_picked(self: &Rc<Self>) {
        if self.default_loading.get() {
            return;
        }
        let choice = match self.default_row.selected() {
            gtk::INVALID_LIST_POSITION => return,
            0 => None,
            entry => Some(entry as usize - 1),
        };
        if choice == self.default_stored.get() {
            return;
        }
        self.default_stored.set(choice);
        self.save(
            &self.default_row,
            &self.default_saving,
            Field::StartupSequence(choice),
            |_, _| {},
        );
    }

    /// Open the sub-page for the sequence at `index` (`None`: a new one) over
    /// this page. If it cannot be read (the profile or the sequence changed
    /// under the rows on show) the person is told and the rows are brought up
    /// to date.
    fn open_sequence(&self, index: Option<usize>) {
        let nav = self
            .sequences
            .ancestor(adw::NavigationView::static_type())
            .and_downcast::<adw::NavigationView>();
        let Some(nav) = nav else {
            return;
        };
        if let Err(message) = sequence_page::push(&nav, &self.ctx, &self.id, index) {
            // Tell first: refreshing may drop this page.
            self.ctx.notice.show_near(&self.sequences, &message);
            self.ctx.profiles_changed();
        }
    }

    fn ask_remove_password(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::builder()
            .heading("Remove the Stored Password?")
            .heading_use_markup(false)
            .body(format!(
                "Connecting from this machine will ask for the password. {}",
                form::REMOVE_PASSWORD_NOTE
            ))
            .body_use_markup(false)
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some("remove"), move |_, _| {
            if let Some(inner) = weak.upgrade() {
                inner.remove_password();
            }
        });
        dialog.present(Some(&self.password));
    }

    /// Remove the stored password ([`form::remove_password`]) on the keyring
    /// worker; a failure is shown when it is back.
    fn remove_password(&self) {
        let id = self.id.clone();
        let ctx = Rc::clone(&self.ctx);
        // Held for the message: the page may have gone by then.
        let near = self.password.clone();
        keyring::change(
            &self.ctx,
            move |secrets| form::remove_password(secrets, &id),
            move |result| {
                let result = result.unwrap_or_else(|| {
                    Err(format!(
                        "Could not remove the stored password: {}.",
                        keyring::STOPPED
                    ))
                });
                if let Err(message) = result {
                    ctx.notice.show_near(&near, &message);
                }
            },
        );
    }

    /// Ask before deleting the profile, naming it literally.
    fn ask_delete(self: &Rc<Self>) {
        // Bind first so the store borrow ends here.
        let name = self
            .ctx
            .store
            .borrow()
            .get(&self.id)
            .map(|p| p.name.clone());
        let Some(name) = name else {
            self.ctx.notice.show_near(&self.key_file, form::GONE);
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Delete \u{201c}{name}\u{201d}?"))
            .heading_use_markup(false)
            .body("This removes the profile and its stored password. It cannot be undone.")
            .body_use_markup(false)
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(inner) = weak.upgrade() {
                inner.delete();
            }
        });
        dialog.present(Some(&self.name.row));
    }

    /// Delete the password, then the profile, in one call on the keyring
    /// worker, so a profile that is gone never leaves a password behind: a
    /// password saved for it meanwhile (queued after this call) finds the
    /// profile gone and is not stored. A failure is shown and nothing further
    /// is done.
    fn delete(&self) {
        // Hold on to a widget for the messages: refreshing may drop this page.
        let near = self.key_file.clone();
        let ctx = Rc::clone(&self.ctx);
        let id = self.id.clone();
        let store_path = self.ctx.store.borrow().path().to_path_buf();
        keyring::change(
            &self.ctx,
            move |secrets| {
                secrets.delete(&password_key(&id)).map_err(|err| {
                    format!(
                        "Could not delete the stored password: {err}. The profile was not \
                         deleted."
                    )
                })?;
                let removed =
                    |err: String| format!("{err} Its stored password has already been removed.");
                let store = keyring::own_store(&store_path).map_err(removed)?;
                let deleted = store.borrow_mut().delete(&id);
                deleted.map_err(|err| removed(format!("Could not delete the profile: {err}.")))
            },
            move |result| {
                let result = result.unwrap_or_else(|| {
                    Err(format!(
                        "Could not delete the profile: {}.",
                        keyring::STOPPED
                    ))
                });
                if let Err(message) = result {
                    ctx.notice.show_near(&near, &message);
                }
                ctx.profiles_changed();
            },
        );
    }
}

/// Set an entry row's text without it counting as an edit: the ✓ only shows
/// for what the person types.
fn set_text_quietly(row: &impl IsA<adw::EntryRow>, text: &str) {
    let row = row.as_ref();
    if row.text() == text {
        return;
    }
    let shows_apply = row.shows_apply_button();
    row.set_show_apply_button(false);
    row.set_text(text);
    row.set_show_apply_button(shows_apply);
}
