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
//! The startup sequences are listed here and edited on a sub-page of their own
//! ([`sequence_page`]) that opens over this one.
//!
//! The page never reads the keyring: it cannot show whether a password is
//! stored. A Secret Service call on every selection would run on the main
//! thread and could freeze or prompt with a locked keyring.
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

use super::sequence_page;
use crate::context::AppContext;
use crate::form::{self, Field};

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
}

struct Inner {
    ctx: Rc<AppContext>,
    id: String,
    name: TextRow,
    host: TextRow,
    username: TextRow,
    port: adw::SpinRow,
    port_stored: Cell<u16>,
    /// The pending save of a changed port, if one is waiting to settle.
    port_timer: RefCell<Option<glib::SourceId>>,
    password: adw::PasswordEntryRow,
    key_file: adw::ActionRow,
    key_clear: gtk::Button,
    sequences: adw::PreferencesGroup,
    sequence_rows: RefCell<Vec<adw::ActionRow>>,
    transcript: adw::SwitchRow,
    transcript_stored: Cell<bool>,
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
            TextRow {
                row,
                stored: RefCell::new(value(profile)),
                value,
                field,
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

        let transcript = adw::SwitchRow::builder()
            .title("Save transcript")
            .subtitle(TRANSCRIPT_WARNING)
            .use_markup(false)
            .active(profile.transcript)
            .build();
        let session = adw::PreferencesGroup::builder().title("Session").build();
        session.add(&transcript);
        widget.add(&session);

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
            password,
            key_file,
            key_clear,
            sequences,
            sequence_rows: RefCell::new(Vec::new()),
            transcript,
            transcript_stored: Cell::new(profile.transcript),
        });
        inner.show_key_file(profile.key_file.as_ref());
        inner.show_sequences(profile);
        inner.connect(&remove, &choose, &delete);
        ProfilePage { widget, inner }
    }

    /// Show `profile` (the same one, as it is in the file now) in every row
    /// that has no unapplied edit. A row with one keeps its text; what it
    /// would go back to becomes the new stored value.
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
        let port_pending = inner.port_timer.borrow().is_some();
        inner.port_stored.set(profile.port);
        if !port_pending {
            inner.port.set_value(f64::from(profile.port));
        }
        inner.transcript_stored.set(profile.transcript);
        inner.transcript.set_active(profile.transcript);
        inner.show_key_file(profile.key_file.as_ref());
        inner.show_sequences(profile);
    }

    /// The window's focus moved to `focus`. A row that focus has left drops
    /// its unapplied edit (the password row empties); a changed port that
    /// focus has left is saved now.
    pub fn focus_moved(&self, focus: Option<&gtk::Widget>) {
        let inner = &self.inner;
        let left = |row: &gtk::Widget| !focus.is_some_and(|f| f == row || f.is_ancestor(row));
        for text in inner.text_rows() {
            if left(text.row.upcast_ref()) {
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
    /// already made (it has no ✓ to wait for).
    ///
    /// The page may be going away in the middle of a refresh or a selection
    /// change, so the windows are told only once the main loop is idle: the
    /// refresh, or the message if the save failed (in whichever window is
    /// still there to show it).
    pub fn flush(&self) {
        let inner = &self.inner;
        let Some(timer) = inner.port_timer.take() else {
            return;
        };
        timer.remove();
        let port = inner.port_value();
        if port == inner.port_stored.get() {
            return;
        }
        let ctx = Rc::clone(&inner.ctx);
        match form::apply_field(&ctx.store, &inner.id, Field::Port(port)) {
            Ok(profile) => {
                inner.port_stored.set(profile.port);
                glib::idle_add_local_once(move || ctx.profiles_changed());
            }
            Err(message) => {
                glib::idle_add_local_once(move || ctx.notice.show(&message));
            }
        }
    }
}

impl Drop for ProfilePage {
    /// A page that goes away (another profile selected, the window closed)
    /// saves a settling port change and keeps no typed password.
    fn drop(&mut self) {
        self.flush();
        self.inner.clear_password();
    }
}

impl Inner {
    fn text_rows(&self) -> [&TextRow; 3] {
        [&self.name, &self.host, &self.username]
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

        let apply = with(|inner| inner.apply_text(&inner.name));
        self.name.row.connect_apply(move |_| apply());
        let apply = with(|inner| inner.apply_text(&inner.host));
        self.host.row.connect_apply(move |_| apply());
        let apply = with(|inner| inner.apply_text(&inner.username));
        self.username.row.connect_apply(move |_| apply());
        let apply = with(|inner| inner.apply_password());
        self.password.connect_apply(move |_| apply());

        let changed = with(|inner| inner.port_changed());
        self.port.connect_value_notify(move |_| changed());
        let changed = with(|inner| inner.transcript_changed());
        self.transcript.connect_active_notify(move |_| changed());

        let ask = with(|inner| inner.ask_remove_password());
        remove.connect_clicked(move |_| ask());
        let ask = with(|inner| inner.choose_key_file());
        choose.connect_clicked(move |_| ask());
        let clear = with(|inner| inner.save_key_file(None));
        self.key_clear.connect_clicked(move |_| clear());
        let ask = with(|inner| inner.ask_delete());
        delete.connect_clicked(move |_| ask());
    }

    /// Save one field. On success everything showing profiles refreshes; on
    /// failure `row` is marked and the message shown near it.
    fn save(&self, row: &impl IsA<gtk::Widget>, field: Field) -> Option<Profile> {
        match form::apply_field(&self.ctx.store, &self.id, field) {
            Ok(profile) => {
                row.remove_css_class("error");
                Some(profile)
            }
            Err(message) => {
                row.add_css_class("error");
                self.ctx.notice.show_near(row, &message);
                None
            }
        }
    }

    fn apply_text(&self, text: &TextRow) {
        let typed = text.row.text().to_string();
        if let Some(profile) = self.save(&text.row, (text.field)(typed)) {
            // Show what was stored (trimmed) before anyone refreshes.
            let value = (text.value)(&profile);
            set_text_quietly(&text.row, &value);
            text.stored.replace(value);
            self.ctx.profiles_changed();
        }
    }

    fn apply_password(&self) {
        // Copied once out of GTK into a wrapper that wipes it on drop.
        let typed = Zeroizing::new(self.password.text().to_string());
        match form::apply_password(&self.ctx.store, self.ctx.secrets.as_ref(), &self.id, &typed) {
            Ok(()) => {
                self.clear_password();
                if !typed.is_empty() {
                    self.ctx.profiles_changed();
                }
            }
            Err(message) => {
                self.password.add_css_class("error");
                self.ctx.notice.show_near(&self.password, &message);
            }
        }
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

    fn save_port(&self) {
        if let Some(timer) = self.port_timer.take() {
            timer.remove();
        }
        let port = self.port_value();
        if port == self.port_stored.get() {
            return;
        }
        if let Some(profile) = self.save(&self.port, Field::Port(port)) {
            self.port_stored.set(profile.port);
            self.ctx.profiles_changed();
        }
    }

    /// The spin row is limited to 1..=65535; 0 would fail validation anyway.
    fn port_value(&self) -> u16 {
        u16::try_from(self.port.value().round() as i64).unwrap_or(0)
    }

    fn transcript_changed(&self) {
        let on = self.transcript.is_active();
        if on == self.transcript_stored.get() {
            return;
        }
        match self.save(&self.transcript, Field::Transcript(on)) {
            Some(profile) => {
                self.transcript_stored.set(profile.transcript);
                self.ctx.profiles_changed();
            }
            // A switch that shows an unsaved state would mislead: put it back.
            None => self.transcript.set_active(self.transcript_stored.get()),
        }
    }

    fn show_key_file(&self, key_file: Option<&PathBuf>) {
        let shown = key_file.map_or_else(|| "None".to_owned(), |p| p.display().to_string());
        self.key_file.set_subtitle(&shown);
        self.key_clear.set_sensitive(key_file.is_some());
    }

    fn save_key_file(&self, path: Option<PathBuf>) {
        if let Some(profile) = self.save(&self.key_file, Field::KeyFile(path)) {
            self.show_key_file(profile.key_file.as_ref());
            self.ctx.profiles_changed();
        }
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

    /// One row per sequence (name and number of commands), each opening the
    /// sequence's sub-page, then a row that adds one.
    fn show_sequences(self: &Rc<Self>, profile: &Profile) {
        for row in self.sequence_rows.take() {
            self.sequences.remove(&row);
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
        for row in &rows {
            self.sequences.add(row);
        }
        self.sequence_rows.replace(rows);
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
            if let Some(inner) = weak.upgrade()
                && let Err(message) = form::remove_password(inner.ctx.secrets.as_ref(), &inner.id)
            {
                inner.ctx.notice.show_near(&inner.password, &message);
            }
        });
        dialog.present(Some(&self.password));
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

    /// Delete the password first, then the profile, so a profile that is gone
    /// never leaves a password behind. A failure is shown and nothing further
    /// is done.
    fn delete(&self) {
        // Hold on to a widget for the messages: refreshing may drop this page.
        let near = self.key_file.clone();
        let ctx = Rc::clone(&self.ctx);
        if let Err(err) = ctx.secrets.delete(&password_key(&self.id)) {
            ctx.notice.show_near(
                &near,
                &format!(
                    "Could not delete the stored password: {err}. The profile was not deleted."
                ),
            );
            return;
        }
        let result = ctx.store.borrow_mut().delete(&self.id);
        if let Err(err) = result {
            ctx.notice.show_near(
                &near,
                &format!(
                    "Could not delete the profile: {err}. Its stored password has already been removed."
                ),
            );
        }
        ctx.profiles_changed();
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
