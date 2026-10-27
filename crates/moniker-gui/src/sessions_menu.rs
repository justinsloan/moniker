//! The Sessions menu beside ＋ in a main window (issue #12): the layouts of
//! windows that were saved ([`crate::session_file`]), added back to this window
//! with a click, and the questions that save this window's layout, rename a
//! saved one and delete it.
//!
//! The button reads "Sessions", then an icon of several windows, then the
//! drop-down arrow (user, 2026-10-26). The Preferences can hide it (on to
//! start with, user): [`show_buttons`] hides or shows it in every main window
//! at once, and a new window reads the setting. Hidden, the saved sessions stay
//! in their file, untouched.
//!
//! The button opens a popover that is built again from the file every time it
//! shows, so it lists what is saved now, whichever window saved it: each
//! session by name (sorted ignoring case) with a › for Rename… and Delete…, or
//! "No saved sessions", or why the file cannot be read; then Save Session…. A
//! click on a session adds its tabs to this window ([`session_layout::restore`]).
//!
//! Nothing here keeps what the file said for later: the file is read again at
//! every decision, because another window may have changed it since the menu
//! was built. A pick reads the session as it is then (a replaced one adds the
//! layout now saved, a deleted one says so); Save asks "Replace" when the name
//! is in use or the file unreadable *as it is when Save is answered*, and
//! captures the window when the answer is final, not when the dialog opened.
//! A file that cannot be read is never written over without asking.
//!
//! A pick and every question wait for the closing menu to hand the keyboard
//! focus back (`terminal_tab::after_the_menu_lets_go`): a dialog opened too soon
//! loses the keyboard to one of its buttons, and a restored pane that takes the
//! keyboard would have it taken back by the closing menu.
//!
//! When a question closes, whatever the answer (its action, Cancel or Escape),
//! the keyboard goes back to the terminal: the focused pane of the tab in front
//! (user, 2026-10-26), not the Sessions button, where libadwaita puts it back
//! for a menu button. Only if that pane may have it (`panes::in_front`): not
//! while another question is open, as the next one of a chain is. A pick keeps
//! its own rule: the restored tab's recorded pane takes it
//! ([`session_layout::restore`]).
//!
//! Session names are chosen by the person but may come from a file that was
//! edited or written by another Moniker: every label and dialog here shows
//! them as plain text, never as Pango markup. A message goes to the
//! notification area of the window the button is in (the button itself is in
//! the header bar, outside the area, so the area is held by the menu).

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use crate::connect_menu::entry_button;
use crate::context::AppContext;
use crate::new_tab::more_button;
use crate::notify::NotificationArea;
use crate::panes;
use crate::session_file::{self, Loaded, Session};
use crate::session_layout;
use crate::terminal_tab::{self, after_the_menu_lets_go};
use crate::window::MainWindow;

/// Widget name of the box that holds one saved session's button and its › in
/// the menu (tests look for it).
pub(crate) const ROW_NAME: &str = "session-row";

/// Widget name of the Sessions button beside ＋ (tests look for it).
pub(crate) const BUTTON_NAME: &str = "sessions-button";
/// The word on the button beside ＋. Being on it, it needs no tooltip.
const LABEL: &str = "Sessions";
/// The icon after the word: several windows (user, 2026-10-26).
const ICON: &str = "focus-windows-symbolic";
/// The entry that saves this window.
const SAVE_ENTRY: &str = "Save Session\u{2026}";
/// What the menu says with nothing saved.
const NONE_SAVED: &str = "No saved sessions";
/// What the Save question says it saves.
const SAVE_BODY: &str = "Saves this window's tabs and panes to add them to a window later. \
     No passwords, and nothing shown in the panes, are saved.";
/// What the Replace question says is replaced.
const REPLACE_BODY: &str = "The saved session with this name is replaced by this window's layout.";
/// What the Delete question says is deleted.
const DELETE_BODY: &str = "The saved layout is deleted. Windows that are open are not changed.";

/// The sessions file of this machine: beside the GUI settings.
pub(crate) fn sessions_path(ctx: &AppContext) -> PathBuf {
    ctx.settings_path.with_file_name(session_file::FILE_NAME)
}

/// The Sessions button for `main`: "Sessions", an icon of several windows and
/// the drop-down arrow, shown unless the Preferences turned it off
/// ([`show_buttons`]). Its popover lists the saved sessions (module docs), and
/// a pick adds one's tabs to `main.view`. It holds the window's view and
/// notification area weakly: the window owns the button.
pub(crate) fn button(main: &MainWindow, ctx: &Rc<AppContext>) -> gtk::MenuButton {
    let popover = gtk::Popover::new();
    let face = gtk::Box::builder().spacing(6).build();
    face.append(&gtk::Label::builder().label(LABEL).use_markup(false).build());
    face.append(&gtk::Image::from_icon_name(ICON));
    let button = gtk::MenuButton::builder()
        .name(BUTTON_NAME)
        .child(&face)
        .always_show_arrow(true)
        .visible(ctx.settings().show_sessions)
        .popover(&popover)
        .build();
    BUTTONS.with_borrow_mut(|all| all.push((button.downgrade(), main.view.downgrade())));
    let menu = Rc::new(Menu {
        ctx: Rc::clone(ctx),
        view: main.view.downgrade(),
        area: main.notifications.downgrade(),
        button: button.downgrade(),
    });
    popover.connect_show(move |popover| popover.set_child(Some(&menu.content(popover))));
    button
}

thread_local! {
    /// Every Sessions button made, with the tabs of its window, so that the
    /// Preferences switch reaches the one in each main window at once
    /// ([`show_buttons`]). GTK runs on one thread.
    static BUTTONS: RefCell<Vec<(glib::WeakRef<gtk::MenuButton>, glib::WeakRef<adw::TabView>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Show or hide the Sessions button of every main window (the Preferences
/// switch, `settings::GuiSettings::show_sessions`); a window opened later reads
/// the setting itself. The saved sessions are not touched. A button hidden
/// while it has the keyboard gives it to the terminal of its window
/// ([`keyboard_to_the_terminal`]), so the keys never go nowhere.
pub(crate) fn show_buttons(shown: bool) {
    // Collected first, so no borrow is held while GTK runs.
    let live: Vec<_> = BUTTONS.with_borrow_mut(|all| {
        all.retain(|(button, _)| button.upgrade().is_some());
        all.iter()
            .filter_map(|(button, view)| Some((button.upgrade()?, view.clone())))
            .collect()
    });
    for (button, view) in live {
        let had_keyboard = button
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| focus.is_ancestor(&button));
        button.set_visible(shown);
        if had_keyboard && !shown {
            keyboard_to_the_terminal(&view);
        }
    }
}

/// Give the keyboard to the focused pane of the tab in front of `view` (its
/// terminal, or the first thing in it that takes the keyboard: the new-tab
/// list's search entry), if it may have it (`panes::in_front`: never a tab out
/// of sight, never while a dialog is open over the window).
fn keyboard_to_the_terminal(view: &glib::WeakRef<adw::TabView>) {
    let holder = view
        .upgrade()
        .and_then(|view| view.selected_page())
        .as_ref()
        .and_then(terminal_tab::holder_of);
    if let Some(holder) = holder
        && panes::in_front(&holder)
    {
        panes::focus_pane(&holder);
    }
}

/// What the menu's closures share. Nothing here is a snapshot of the file.
struct Menu {
    ctx: Rc<AppContext>,
    /// The tabs a pick adds to.
    view: glib::WeakRef<adw::TabView>,
    /// Where messages appear: the window's own.
    area: glib::WeakRef<NotificationArea>,
    /// The button, which dialogs are shown over (its window).
    button: glib::WeakRef<gtk::MenuButton>,
}

impl Menu {
    fn path(&self) -> PathBuf {
        sessions_path(&self.ctx)
    }

    /// The window the button is in, if it is still shown.
    fn window(&self) -> Option<gtk::Window> {
        self.button
            .upgrade()
            .and_then(|button| button.root())
            .and_downcast::<gtk::Window>()
    }

    /// Once `dialog` has closed, whatever the answer, give the keyboard to the
    /// terminal of the tab in front ([`keyboard_to_the_terminal`]; not while
    /// another dialog is open, such as the next question of a chain, whose own
    /// closing does it). From an idle callback: by then the dialog has gone
    /// from the window, and libadwaita has given the keyboard back to the
    /// widget that had it before the dialog opened, which this then overrides.
    fn give_the_keyboard_back_when_closed(&self, dialog: &adw::AlertDialog) {
        let view = self.view.clone();
        dialog.connect_closed(move |_| {
            let view = view.clone();
            glib::idle_add_local_once(move || keyboard_to_the_terminal(&view));
        });
    }

    /// Show `text` (plain) in this window.
    fn tell(&self, text: &str) {
        match self.area.upgrade() {
            Some(area) => self.ctx.notice.show_near(&area, text),
            None => self.ctx.notice.show(text),
        }
    }

    /// The popover's content, from the file as it is now.
    fn content(self: &Rc<Self>, popover: &gtk::Popover) -> gtk::Box {
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        match session_file::load(&self.path()) {
            Loaded::Sessions(sessions) if sessions.is_empty() => {
                column.append(&note(NONE_SAVED, true));
            }
            Loaded::Sessions(sessions) => {
                column.append(&self.list(popover, &names_in_order(&sessions)));
            }
            Loaded::Unreadable(why) => {
                column.append(&note(&unreadable_text(&why), false));
            }
        }
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let save = entry_button(SAVE_ENTRY);
        let menu = Rc::clone(self);
        save.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| menu.leave(&[&popover], |menu| menu.ask_save())
        ));
        column.append(&save);
        column
    }

    /// A row for each of `names`, scrolling when there are many.
    fn list(self: &Rc<Self>, popover: &gtk::Popover, names: &[String]) -> gtk::ScrolledWindow {
        let rows = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        for name in names {
            rows.append(&self.row(popover, name));
        }
        gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .max_content_height(320)
            .child(&rows)
            .build()
    }

    /// The row of the session `name`: its button, which adds it, and a › with
    /// Rename… and Delete….
    fn row(self: &Rc<Self>, outer: &gtk::Popover, name: &str) -> gtk::Box {
        let row = gtk::Box::builder().name(ROW_NAME).spacing(2).build();
        let name = name.to_owned();

        // The button cuts a long name short: the tooltip has all of it (plain
        // text: `set_tooltip_text` is never markup).
        let pick = entry_button(&name);
        pick.set_hexpand(true);
        pick.set_tooltip_text(Some(&name));
        let menu = Rc::clone(self);
        pick.connect_clicked(glib::clone!(
            #[weak]
            outer,
            #[strong]
            name,
            move |_| menu.pick(&outer, &name)
        ));
        row.append(&pick);

        let inner = gtk::Popover::new();
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let rename = entry_button("Rename\u{2026}");
        let menu = Rc::clone(self);
        rename.connect_clicked(glib::clone!(
            #[weak]
            inner,
            #[weak]
            outer,
            #[strong]
            name,
            move |_| {
                let name = name.clone();
                menu.leave(&[&inner, &outer], move |menu| menu.ask_rename(&name));
            }
        ));
        let delete = entry_button("Delete\u{2026}");
        let menu = Rc::clone(self);
        delete.connect_clicked(glib::clone!(
            #[weak]
            inner,
            #[weak]
            outer,
            #[strong]
            name,
            move |_| {
                let name = name.clone();
                menu.leave(&[&inner, &outer], move |menu| menu.ask_delete(&name));
            }
        ));
        actions.append(&rename);
        actions.append(&delete);
        inner.set_child(Some(&actions));
        row.append(&more_button("Session Menu", &inner));
        row
    }

    /// Close `popovers`, and once the keyboard focus has left the menu run
    /// `then`.
    fn leave(self: &Rc<Self>, popovers: &[&gtk::Popover], then: impl FnOnce(&Rc<Menu>) + 'static) {
        for popover in popovers {
            popover.popdown();
        }
        let menu = Rc::clone(self);
        after_the_menu_lets_go(self.window().as_ref(), move || then(&menu));
    }

    /// Add the session `name` to this window's tabs.
    fn pick(self: &Rc<Self>, popover: &gtk::Popover, name: &str) {
        let name = name.to_owned();
        self.leave(&[popover], move |menu| menu.add(&name));
    }

    /// Add the session `name`, as the file has it now.
    fn add(&self, name: &str) {
        match session_file::load(&self.path()) {
            Loaded::Unreadable(why) => self.tell(&unreadable_text(&why)),
            Loaded::Sessions(sessions) => match sessions.iter().find(|s| s.name == name) {
                Some(session) => {
                    if let Some(view) = self.view.upgrade() {
                        session_layout::restore(&view, &self.ctx, &session.tabs);
                    }
                }
                None => self.tell(session_file::GONE),
            },
        }
    }

    /// Ask for a name to save this window under.
    fn ask_save(self: &Rc<Self>) {
        let Some(window) = self.window() else {
            return;
        };
        // From the file as it is now: another window may have saved meanwhile.
        let start = match session_file::load(&self.path()) {
            Loaded::Sessions(sessions) => session_file::next_default_name(&sessions),
            Loaded::Unreadable(_) => session_file::next_default_name(&[]),
        };
        let (dialog, entry) = name_dialog(
            "Save Session",
            Some(SAVE_BODY),
            ("save", "Save"),
            &start,
            Vec::new(),
        );
        self.give_the_keyboard_back_when_closed(&dialog);
        let menu = Rc::clone(self);
        dialog.choose(
            Some(&window),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak]
                entry,
                move |response| {
                    if response == "save"
                        && let Ok(name) = session_file::check_name(&entry.text())
                    {
                        menu.save(name);
                    }
                }
            ),
        );
    }

    /// Save this window as `name`, if the file as it is now allows it without
    /// asking: asks first to replace a session of that name or a file that
    /// cannot be read.
    fn save(self: &Rc<Self>, name: String) {
        match session_file::load(&self.path()) {
            Loaded::Unreadable(why) => self.ask_to_replace_the_file(name, &why),
            Loaded::Sessions(sessions) if sessions.iter().any(|s| s.name == name) => {
                self.ask_to_replace(name);
            }
            Loaded::Sessions(_) => self.put(name, false),
        }
    }

    /// Write this window, as it is now, as the session `name`.
    /// `replace_unreadable` says the person chose to start a new file.
    fn put(&self, name: String, replace_unreadable: bool) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let session = Session {
            name,
            tabs: session_layout::capture(&view),
        };
        if let Err(err) = session_file::put(&self.path(), session, replace_unreadable) {
            self.tell(&write_error(&err));
        }
    }

    /// Ask `heading` and `body` over the window, with Cancel (the default and
    /// the answer to Escape) and the destructive `action` (id, label); `then`
    /// runs only on that answer.
    fn confirm(
        self: &Rc<Self>,
        heading: &str,
        body: &str,
        action: (&str, &str),
        then: impl FnOnce(&Rc<Menu>) + 'static,
    ) {
        let Some(window) = self.window() else {
            return;
        };
        let dialog = question(heading, body, action);
        self.give_the_keyboard_back_when_closed(&dialog);
        let menu = Rc::clone(self);
        let id = action.0.to_owned();
        dialog.choose(Some(&window), gio::Cancellable::NONE, move |response| {
            if response == id {
                then(&menu);
            }
        });
    }

    /// Ask before a session of the same name is replaced.
    fn ask_to_replace(self: &Rc<Self>, name: String) {
        self.confirm(
            &format!("Replace \u{201c}{name}\u{201d}?"),
            REPLACE_BODY,
            ("replace", "Replace"),
            move |menu| menu.put(name, false),
        );
    }

    /// Ask before a file that cannot be read (`why`) is replaced by a new one.
    fn ask_to_replace_the_file(self: &Rc<Self>, name: String, why: &str) {
        self.confirm(
            "Replace the Saved Sessions File?",
            &format!(
                "Moniker could not read it: {why}. Saving starts a new file without the \
                 sessions that were in it."
            ),
            ("replace", "Replace"),
            move |menu| menu.replace_the_file(name),
        );
    }

    /// The person said to start a new file. The file is read again: another
    /// window may have repaired it while the question was open, and a file that
    /// can be read is never replaced on that answer, so it goes through the
    /// ordinary save (which asks about a name already in it); only a file that
    /// still cannot be read is started anew.
    fn replace_the_file(self: &Rc<Self>, name: String) {
        match session_file::load(&self.path()) {
            Loaded::Sessions(_) => self.save(name),
            Loaded::Unreadable(_) => self.put(name, true),
        }
    }

    /// Ask for a new name for the session `from`.
    fn ask_rename(self: &Rc<Self>, from: &str) {
        let Some(window) = self.window() else {
            return;
        };
        // The names of the other sessions, as the file has them now; the file
        // is checked again when Rename is answered.
        let taken = match session_file::load(&self.path()) {
            Loaded::Sessions(sessions) => sessions
                .into_iter()
                .map(|session| session.name)
                .filter(|name| name != from)
                .collect(),
            Loaded::Unreadable(_) => Vec::new(),
        };
        let (dialog, entry) =
            name_dialog("Rename Session", None, ("rename", "Rename"), from, taken);
        self.give_the_keyboard_back_when_closed(&dialog);
        let menu = Rc::clone(self);
        let from = from.to_owned();
        dialog.choose(
            Some(&window),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak]
                entry,
                move |response| {
                    if response == "rename"
                        && let Err(text) = session_file::rename(&menu.path(), &from, &entry.text())
                    {
                        menu.tell(&text);
                    }
                }
            ),
        );
    }

    /// Ask before the session `name` is deleted.
    fn ask_delete(self: &Rc<Self>, name: &str) {
        let name_to_delete = name.to_owned();
        self.confirm(
            &format!("Delete \u{201c}{name}\u{201d}?"),
            DELETE_BODY,
            ("delete", "Delete"),
            move |menu| {
                if let Err(err) = session_file::delete(&menu.path(), &name_to_delete) {
                    menu.tell(&write_error(&err));
                }
            },
        );
    }
}

/// A line of plain text for the menu: dimmed when it only says there is nothing,
/// wrapping when it says why the file cannot be read.
fn note(text: &str, dim: bool) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(40)
        .margin_start(6)
        .margin_end(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    if dim {
        label.add_css_class("dim-label");
    }
    label
}

/// What the menu and a pick say of a file that cannot be read.
fn unreadable_text(why: &str) -> String {
    format!("Could not read the saved sessions: {why}")
}

/// The sessions' names as the menu lists them: sorted ignoring case.
fn names_in_order(sessions: &[Session]) -> Vec<String> {
    let mut names: Vec<String> = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect();
    names.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
    names
}

/// What to say of a write that failed, by what failed: a session the file would
/// not read back ([`io::ErrorKind::InvalidInput`], a plain sentence), a file that
/// cannot be read (`InvalidData`, which already says "Could not read the saved
/// sessions"), or the write itself.
fn write_error(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::InvalidInput => format!("Could not save the session: {err}"),
        io::ErrorKind::InvalidData => err.to_string(),
        _ => format!("Could not save the sessions: {err}"),
    }
}

/// Why `raw` cannot be the name of a session, if it cannot: a name
/// [`session_file::check_name`] refuses, or one of the `taken` names.
fn name_error(raw: &str, taken: &[String]) -> Option<&'static str> {
    match session_file::check_name(raw) {
        Err(why) => Some(why),
        Ok(name) => taken.contains(&name).then_some(session_file::NAME_TAKEN),
    }
}

/// A question with Cancel and the `action` response (id, label) as the
/// destructive one. Cancel (also Escape) is the default. Plain text.
fn question(heading: &str, body: &str, action: (&str, &str)) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .heading_use_markup(false)
        .body(body)
        .body_use_markup(false)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response(action.0, action.1);
    dialog.set_response_appearance(action.0, adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog
}

/// A dialog that asks for a session's name, in a field that starts as `start`,
/// with Cancel (also Escape) and the `action` response (id, label), which Enter
/// in the field answers. A plain label under the field gives the reason the name
/// cannot be used (as [`name_error`] says, with `taken` the names of other
/// sessions) and the response is off meanwhile. Plain text.
fn name_dialog(
    heading: &str,
    body: Option<&str>,
    action: (&'static str, &str),
    start: &str,
    taken: Vec<String>,
) -> (adw::AlertDialog, gtk::Entry) {
    let entry = gtk::Entry::builder()
        .text(start)
        .activates_default(true)
        .build();
    let error = gtk::Label::builder()
        .use_markup(false)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["error"])
        .visible(false)
        .build();
    // A screen reader says the reason with the field, not only to someone who
    // finds the label.
    let reason = [error.upcast_ref::<gtk::Accessible>()];
    entry.update_relation(&[gtk::accessible::Relation::DescribedBy(&reason)]);
    let extra = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    extra.append(&entry);
    extra.append(&error);
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .heading_use_markup(false)
        .body_use_markup(false)
        .extra_child(&extra)
        .build();
    if let Some(body) = body {
        dialog.set_body(body);
    }
    dialog.add_response("cancel", "Cancel");
    dialog.add_response(action.0, action.1);
    dialog.set_response_appearance(action.0, adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some(action.0));
    dialog.set_close_response("cancel");
    dialog.set_focus(Some(&entry));
    let check = glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |entry: &gtk::Entry| {
            let problem = name_error(&entry.text(), &taken);
            error.set_label(problem.unwrap_or_default());
            error.set_visible(problem.is_some());
            dialog.set_response_enabled(action.0, problem.is_none());
        }
    );
    check(&entry);
    entry.connect_changed(check);
    (dialog, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_file::TabRecord;

    fn named(name: &str) -> Session {
        Session {
            name: name.to_owned(),
            tabs: Vec::<TabRecord>::new(),
        }
    }

    #[test]
    fn names_are_listed_ignoring_case() {
        let sessions = [named("beta"), named("Zed"), named("alpha"), named("Alpha")];
        assert_eq!(
            names_in_order(&sessions),
            ["Alpha", "alpha", "beta", "Zed"],
            "ignoring case, and the same name in another case by its own order"
        );
        assert!(names_in_order(&[]).is_empty());
    }

    #[test]
    fn a_name_is_refused_with_the_reason_or_taken_by_another() {
        let taken = ["Work".to_owned()];
        assert_eq!(name_error("Home", &taken), None);
        assert_eq!(name_error("  Home ", &taken), None, "trimmed");
        assert_eq!(name_error("", &taken), Some("Enter a name."));
        assert_eq!(name_error("   ", &taken), Some("Enter a name."));
        assert_eq!(
            name_error("a\tb", &taken),
            Some("A name cannot contain control characters.")
        );
        assert_eq!(
            name_error(&"x".repeat(101), &taken),
            Some("A name can be at most 100 characters.")
        );
        assert_eq!(name_error(" Work ", &taken), Some(session_file::NAME_TAKEN));
        assert_eq!(name_error("work", &taken), None, "names are case-sensitive");
        assert_eq!(
            name_error("Work", &[]),
            None,
            "Save asks about a name in use"
        );
    }

    #[test]
    fn a_write_error_is_told_by_what_failed() {
        let refused = io::Error::new(io::ErrorKind::InvalidInput, "the session has no tab");
        assert_eq!(
            write_error(&refused),
            "Could not save the session: the session has no tab"
        );
        // Already says what it is.
        let unreadable = io::Error::new(
            io::ErrorKind::InvalidData,
            "Could not read the saved sessions: it is not JSON (x)",
        );
        assert_eq!(
            write_error(&unreadable),
            "Could not read the saved sessions: it is not JSON (x)"
        );
        let failed = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(
            write_error(&failed),
            format!("Could not save the sessions: {failed}")
        );
    }
}
