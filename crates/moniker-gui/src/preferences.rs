//! The Preferences dialog (☰ → Preferences, Ctrl+comma): an
//! `adw::PreferencesDialog` over the window it was opened from, so, like every
//! dialog, it pauses that window's tab and terminal shortcuts while it is open.
//!
//! * **Terminal**: the font (a font chooser that offers monospace fonts only;
//!   with none chosen the button shows the system monospace font, and while one
//!   is chosen a reset button beside it goes back to that), used at once by
//!   every open terminal and by new ones (`terminal_tab::apply_font`); and the
//!   scrollback of ordinary tabs, 1,000 to 1,000,000 lines, for tabs opened
//!   after the change (`terminal_tab::set_scrollback`). Tabs that save a
//!   transcript keep unlimited scrollback whatever it says.
//! * **Transcripts**, under the warning that transcripts can hold secrets:
//!   "Save transcripts of all sessions", and the transcripts folder with a
//!   Choose… button ([`change_folder`]).
//! * **Sync**: Open shows the Connection Profiles window on its sync page.
//!
//! Each change is saved at once through `settings::update`, which writes only
//! that setting. A change that cannot be saved (a damaged or unwritable
//! settings file) is not used: a notice says why and the row goes back to what
//! the file still says.
//!
//! Choosing another transcripts folder moves the transcripts there on a worker
//! thread ([`folder_chosen`]); only the files that are transcripts move, the
//! rest of the old folder stays, and the notice says how many were left (and,
//! whenever something stays behind, which folder that is). The
//! application is held until the move ends, so closing the last window
//! meanwhile does not cut a copy short.
//!
//! The folder is shown as plain text (markup off): a folder name can hold any
//! character.

use std::cell::Cell;
use std::fmt::Display;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib, pango};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::transcript::{MoveReport, move_transcripts, prepare_folder};

use crate::context::AppContext;
use crate::notify;
use crate::settings::{self, GuiSettings};
use crate::{profiles, recordings, terminal_tab, window};

/// Widget name of the dialog, how [`present`] finds an open one.
const DIALOG_NAME: &str = "moniker-preferences";

/// Widget names of the folder row and its Choose… button, how the tests'
/// `change_folder` finds them in a dialog.
const FOLDER_ROW_NAME: &str = "preferences-folder";
const CHOOSE_NAME: &str = "preferences-choose-folder";

/// Widget name of the Font row's reset button, how the tests find it.
const FONT_RESET_NAME: &str = "preferences-font-reset";

const FONT_SUBTITLE: &str = "Used by every terminal; changes apply at once";
const FONT_RESET_TOOLTIP: &str = "Use the System Font";
const SCROLLBACK_SUBTITLE: &str = "Lines kept for scrolling back in ordinary tabs";
const SAVE_ALL_TITLE: &str = "Save transcripts of all sessions";
const SAVE_ALL_SUBTITLE: &str = "Every new tab, including the first at launch. Tabs that save a \
transcript keep all their scrollback.";
const SYNC_SUBTITLE: &str = "Sync key, the other machine, automatic sync";

/// The font a font setting of `None` stands for, when nothing else says: the
/// family VTE uses by default, at a common size.
const FALLBACK_FONT: &str = "Monospace 11";

thread_local! {
    /// Transcripts are being moved to a newly chosen folder. Another folder
    /// cannot be chosen until that is over: a second move would list the
    /// folder the first one is still filling. GTK runs on one thread.
    static MOVING: Cell<bool> = const { Cell::new(false) };
}

/// Keeps the application running while transcripts move to a newly chosen
/// folder, even if the last window closes meanwhile (the application quits when
/// its last window goes).
///
/// A move across file systems copies each file under its final name, so a
/// process that exits in the middle of a copy leaves a truncated file in the new
/// folder, which the Transcripts window would list as a whole recording (the
/// original, whole, is still in the old folder). Holding the application until
/// the move is over rules that out for the ordinary ways to quit. A signal or a
/// logout that kills the process outright still cannot be helped.
///
/// Released by dropping it, which the move's completion does.
struct MoveHold {
    _guard: gio::ApplicationHoldGuard,
}

impl MoveHold {
    fn new(app: &adw::Application) -> Self {
        #[cfg(test)]
        HELD.with(|held| held.set(held.get() + 1));
        Self { _guard: app.hold() }
    }
}

#[cfg(test)]
impl Drop for MoveHold {
    fn drop(&mut self) {
        HELD.with(|held| held.set(held.get() - 1));
    }
}

// Test hooks. An application cannot be asked how many holds it has, so the
// tests count the `MoveHold`s alive instead; and the worker of a move can be
// made to wait, so that a test can do things while a move is certainly still
// running.

#[cfg(test)]
thread_local! {
    /// The [`MoveHold`]s alive.
    static HELD: Cell<usize> = const { Cell::new(0) };
}

/// What the worker of a move waits for, if a test set it.
#[cfg(test)]
static MOVE_GATE: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>> =
    std::sync::Mutex::new(None);

/// How many moves are holding the application now.
#[cfg(test)]
pub(crate) fn moves_holding_the_app() -> usize {
    HELD.with(Cell::get)
}

/// Make the worker of the next move wait before it starts, until the returned
/// sender sends (or is dropped).
#[cfg(test)]
pub(crate) fn hold_back_the_next_move() -> std::sync::mpsc::Sender<()> {
    let (release, wait) = std::sync::mpsc::channel();
    *MOVE_GATE.lock().expect("the gate") = Some(wait);
    release
}

/// On the worker thread: wait at the gate if a test set one.
#[cfg(test)]
fn wait_at_the_gate() {
    let gate = MOVE_GATE.lock().expect("the gate").take();
    if let Some(wait) = gate {
        let _ = wait.recv();
    }
}

/// Show the Preferences dialog over the window of `parent`, or the one already
/// open in `app` (one at a time), and return it. An open one may be over
/// another window than `parent` (Ctrl+comma pressed in a second main window):
/// that window is raised, so that the dialog is seen.
pub fn present(
    parent: &impl IsA<gtk::Widget>,
    app: &adw::Application,
    ctx: &Rc<AppContext>,
) -> adw::PreferencesDialog {
    if let Some((over, dialog)) = open_dialog(app) {
        over.present();
        return dialog;
    }
    let dialog = build(app, ctx);
    dialog.present(Some(parent));
    dialog
}

/// The Preferences dialog open over one of `app`'s windows, if any, with the
/// window it is over.
fn open_dialog(app: &adw::Application) -> Option<(gtk::Window, adw::PreferencesDialog)> {
    app.windows().into_iter().find_map(|window| {
        notify::visible_dialog(&window)
            .filter(|dialog| dialog.widget_name() == DIALOG_NAME)
            .and_downcast::<adw::PreferencesDialog>()
            .map(|dialog| (window, dialog))
    })
}

/// The widget of type `T` named `name` at or below `root`.
fn named<T: IsA<gtk::Widget>>(root: &impl IsA<gtk::Widget>, name: &str) -> Option<T> {
    let mut stack = vec![root.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == name
            && let Ok(found) = widget.clone().downcast::<T>()
        {
            return Some(found);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    None
}

fn build(app: &adw::Application, ctx: &Rc<AppContext>) -> adw::PreferencesDialog {
    let now = ctx.settings();
    let page = adw::PreferencesPage::new();
    page.add(&terminal_group(ctx, &now));
    page.add(&transcripts_group(app, ctx, &now));
    page.add(&sync_group(app, ctx));
    let dialog = adw::PreferencesDialog::builder()
        .name(DIALOG_NAME)
        .title("Preferences")
        .search_enabled(false)
        .build();
    dialog.add(&page);
    dialog
}

/// Tell the person `text` in the Preferences dialog that `near` (a row or a
/// button in it) is in, where it is seen: the dialog covers the window's own
/// notification area. A widget that is in no open dialog gets what any message
/// gets ([`crate::context::Notice::show_over`]).
fn say(ctx: &AppContext, near: &impl IsA<gtk::Widget>, text: &str) {
    match near
        .ancestor(adw::PreferencesDialog::static_type())
        .and_downcast::<adw::PreferencesDialog>()
    {
        Some(dialog) => ctx.notice.show_over(&dialog, text),
        None => ctx.notice.show(text),
    }
}

/// Save one change to the settings file ([`settings::update`]); if it cannot be
/// saved, say why (in the dialog `near` is in) and return false, so the row can
/// go back. The reason is an I/O or parse error, which quotes no setting.
fn save(
    ctx: &AppContext,
    near: &impl IsA<gtk::Widget>,
    change: impl FnOnce(&mut GuiSettings),
) -> bool {
    match settings::update(&ctx.settings_path, change) {
        Ok(()) => true,
        Err(err) => {
            say(ctx, near, &format!("Could not save the preferences: {err}"));
            false
        }
    }
}

// Each row below keeps the value last saved and ignores a change to that value.
// That is also how putting a row back after a failed save stays unsaved: not a
// flag set around the call, because the notification of a change made inside a
// row's own change handler can arrive after the handler has returned (an
// `AdwSwitchRow` does that), when such a flag is already off, and the put-back
// value then looked like the person's change, failed to save in turn and was
// put back again, forever.

// ----- Terminal -----

fn terminal_group(ctx: &Rc<AppContext>, now: &GuiSettings) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Terminal").build();
    group.add(&font_row(ctx, now));
    group.add(&scrollback_row(ctx, now));
    group
}

/// A filter that keeps monospace fonts only. The font chooser filters font
/// families and, at the level that shows styles, their faces; it has to handle
/// both.
fn monospace_filter() -> gtk::CustomFilter {
    gtk::CustomFilter::new(|item| {
        if let Some(family) = item.downcast_ref::<pango::FontFamily>() {
            family.is_monospace()
        } else if let Some(face) = item.downcast_ref::<pango::FontFace>() {
            face.family().is_monospace()
        } else {
            false
        }
    })
}

/// The font to show for `font` (the setting): itself, or for no setting the
/// system monospace font as VTE picks it (the "monospace" family at the size of
/// the interface font of `widget`).
fn shown_font(font: Option<&str>, widget: &impl IsA<gtk::Widget>) -> pango::FontDescription {
    if let Some(font) = terminal_tab::font_description(font) {
        return font;
    }
    let mut desc = widget
        .pango_context()
        .font_description()
        .filter(|desc| desc.size() > 0)
        .unwrap_or_else(|| pango::FontDescription::from_string(FALLBACK_FONT));
    desc.set_family("Monospace");
    desc
}

fn font_row(ctx: &Rc<AppContext>, now: &GuiSettings) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Font")
        .subtitle(FONT_SUBTITLE)
        .use_markup(false)
        .build();
    let chooser = gtk::FontDialog::builder()
        .title("Terminal Font")
        .modal(true)
        .filter(&monospace_filter())
        .build();
    let button = gtk::FontDialogButton::builder()
        .dialog(&chooser)
        .valign(gtk::Align::Center)
        .build();
    button.set_font_desc(&shown_font(now.font.as_deref(), &row));
    // Back to the system monospace font; only there while a font is chosen.
    let reset = gtk::Button::builder()
        .name(FONT_RESET_NAME)
        .icon_name("edit-undo-symbolic")
        .tooltip_text(FONT_RESET_TOOLTIP)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .visible(now.font.is_some())
        .build();
    row.add_suffix(&reset);
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));

    // The font the button showed after the last save, to go back to.
    let last = Rc::new(std::cell::RefCell::new(button.font_desc()));
    button.connect_font_desc_notify(glib::clone!(
        #[strong]
        ctx,
        #[strong]
        last,
        #[weak]
        reset,
        move |button| {
            let Some(font) = button.font_desc() else {
                return;
            };
            if last.borrow().as_ref() == Some(&font) {
                return;
            }
            let text = font.to_str().to_string();
            if save(&ctx, button, |s| s.font = Some(text)) {
                terminal_tab::apply_font(Some(&font));
                last.replace(Some(font));
                reset.set_visible(true);
            } else {
                // Cloned out: putting it back notifies, which borrows `last`.
                let previous = last.borrow().clone();
                if let Some(previous) = previous {
                    button.set_font_desc(&previous);
                }
            }
        }
    ));
    reset.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        #[weak]
        button,
        #[weak]
        row,
        move |reset| {
            if !save(&ctx, reset, |s| s.font = None) {
                return;
            }
            terminal_tab::apply_font(None);
            let system = shown_font(None, &row);
            // Recorded first: showing it on the button notifies, and the
            // system font must not then be saved as a chosen one.
            last.replace(Some(system.clone()));
            button.set_font_desc(&system);
            // The focus goes to the font button rather than nowhere.
            button.grab_focus();
            reset.set_visible(false);
        }
    ));
    row
}

fn scrollback_row(ctx: &Rc<AppContext>, now: &GuiSettings) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(
        settings::SCROLLBACK_MIN as f64,
        settings::SCROLLBACK_MAX as f64,
        1_000.0,
    );
    row.set_title("Scrollback");
    row.set_subtitle(SCROLLBACK_SUBTITLE);
    row.set_use_markup(false);
    row.set_digits(0);
    row.set_value(now.scrollback_lines as f64);

    let last = Rc::new(Cell::new(row.value()));
    row.connect_value_notify(glib::clone!(
        #[strong]
        ctx,
        move |row| {
            if row.value() == last.get() {
                return;
            }
            // Within the range already (the adjustment's), and whole.
            let lines = row.value().round() as i64;
            if save(&ctx, row, |s| s.scrollback_lines = lines) {
                terminal_tab::set_scrollback(lines);
                last.set(row.value());
            } else {
                row.set_value(last.get());
            }
        }
    ));
    row
}

// ----- Transcripts -----

fn transcripts_group(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    now: &GuiSettings,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Transcripts")
        .description(recordings::SECRETS_WARNING)
        .build();

    let save_all = adw::SwitchRow::builder()
        .title(SAVE_ALL_TITLE)
        .subtitle(SAVE_ALL_SUBTITLE)
        .use_markup(false)
        .active(now.save_all_transcripts)
        .build();
    let saved = Rc::new(Cell::new(now.save_all_transcripts));
    save_all.connect_active_notify(glib::clone!(
        #[strong]
        ctx,
        move |row| {
            let on = row.is_active();
            if on == saved.get() {
                return;
            }
            if save(&ctx, row, |s| s.save_all_transcripts = on) {
                saved.set(on);
            } else {
                row.set_active(saved.get());
            }
        }
    ));
    group.add(&save_all);

    let folder = adw::ActionRow::builder()
        .name(FOLDER_ROW_NAME)
        .title("Transcripts folder")
        .subtitle(folder_text(&ctx.transcripts_dir()))
        .use_markup(false)
        .build();
    let choose = gtk::Button::builder()
        .name(CHOOSE_NAME)
        .label("Choose…")
        .valign(gtk::Align::Center)
        .sensitive(!MOVING.get())
        .build();
    choose.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[weak]
        folder,
        #[strong]
        ctx,
        move |choose| ask_for_folder(&app, &ctx, &folder, choose)
    ));
    folder.add_suffix(&choose);
    group.add(&folder);
    group
}

/// How a folder is shown: its path, made readable if it is not valid UTF-8.
fn folder_text(folder: &Path) -> String {
    folder.display().to_string()
}

/// Choose…: a folder chooser that starts in the current folder (if it exists);
/// the folder chosen goes to [`folder_chosen`]. Dismissing it changes nothing.
fn ask_for_folder(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    row: &adw::ActionRow,
    choose: &gtk::Button,
) {
    let chooser = gtk::FileDialog::builder()
        .title("Transcripts Folder")
        .accept_label("Choose")
        .modal(true)
        .build();
    let current = ctx.transcripts_dir();
    if current.is_dir() {
        chooser.set_initial_folder(Some(&gio::File::for_path(&current)));
    }
    let parent = choose.root().and_downcast::<gtk::Window>();
    chooser.select_folder(
        parent.as_ref(),
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            app,
            #[weak]
            row,
            #[weak]
            choose,
            #[strong]
            ctx,
            move |result| {
                // An error is the chooser dismissed (or failing to open): nothing
                // was chosen.
                let Ok(file) = result else {
                    return;
                };
                match file.path() {
                    Some(folder) => folder_chosen(&app, &ctx, &row, &choose, folder),
                    None => refuse(&ctx, &choose, "it is not a folder on this computer"),
                }
            }
        ),
    );
}

/// Use `folder` as the transcripts folder, as if the person had chosen it with
/// Choose… in `dialog`: what the file chooser does with the folder chosen
/// ([`folder_chosen`]), for tests, which cannot work a file chooser.
#[cfg(test)]
pub(crate) fn change_folder(
    dialog: &adw::PreferencesDialog,
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    folder: PathBuf,
) {
    let row = named::<adw::ActionRow>(dialog, FOLDER_ROW_NAME).expect("the folder row");
    let choose = named::<gtk::Button>(dialog, CHOOSE_NAME).expect("the Choose… button");
    folder_chosen(app, ctx, &row, &choose, folder);
}

/// The Font row's reset button in `dialog`.
#[cfg(test)]
pub(crate) fn font_reset_button(dialog: &adw::PreferencesDialog) -> gtk::Button {
    named(dialog, FONT_RESET_NAME).expect("the font reset button")
}

/// Say that the folder chosen cannot be used, and why (a reason, never the
/// content of a file).
fn refuse(ctx: &AppContext, near: &impl IsA<gtk::Widget>, why: impl Display) {
    say(ctx, near, &format!("Could not use that folder: {why}"));
}

/// Whether `a` and `b` are one directory (the same file, however named).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// The person chose `folder` as the transcripts folder.
///
/// In this order:
/// 1. The folder is checked before anything changes: it must be an absolute
///    path that the settings file can hold (valid UTF-8), and a folder this
///    process can write to, created (mode 0700) if missing
///    ([`prepare_folder`]). If it is not, a notice says why and nothing
///    changes. The folder in use already (by any name) changes nothing either.
/// 2. It is saved, so new sessions save into it from now on. A failed save
///    says why and changes nothing.
/// 3. Every session still open that will save a transcript is pointed at it
///    (`terminal_tab::save_open_transcripts_to`), before anything moves, so
///    that no transcript is written into the old folder while (or after) its
///    files leave.
/// 4. The row shows it, and an open Transcripts window lists it.
/// 5. The transcripts in the old folder move into it on a worker thread, and a
///    notice says how that went ([`move_result_text`]); files that are not
///    transcripts stay where they are. Choose… waits until then, in whichever
///    Preferences dialog is open at that moment. The application is held until
///    the move ends ([`MoveHold`]).
fn folder_chosen(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    row: &adw::ActionRow,
    choose: &gtk::Button,
    folder: PathBuf,
) {
    if MOVING.get() {
        say(
            ctx,
            choose,
            "Transcripts are still being moved to the folder chosen before. \
             Choose again when that is done.",
        );
        return;
    }
    let old = ctx.transcripts_dir();
    if folder == old {
        return;
    }
    if folder.to_str().is_none() {
        refuse(
            ctx,
            choose,
            "its name cannot be saved (it is not valid UTF-8)",
        );
        return;
    }
    if let Err(err) = prepare_folder(&folder) {
        refuse(ctx, choose, err);
        return;
    }
    if same_dir(&folder, &old) {
        return;
    }
    if !save(ctx, choose, |s| s.transcripts_dir = Some(folder.clone())) {
        return;
    }
    terminal_tab::save_open_transcripts_to(&folder);
    row.set_subtitle(&folder_text(&folder));
    recordings::set_dir(app, folder.clone());

    MOVING.set(true);
    choose.set_sensitive(false);
    let hold = MoveHold::new(app);
    let to = folder.clone();
    let old_folder = old.clone();
    // Weak: the dialog may be closed by the time the move is over (the app is
    // held, so it is not gone); the notice is still shown (in the Preferences
    // dialog open then, else in a window) and Choose… freed either way.
    let app = app.downgrade();
    let choose = choose.downgrade();
    let ctx = Rc::clone(ctx);
    recordings::in_background(
        move || {
            #[cfg(test)]
            wait_at_the_gate();
            move_transcripts(&old, &to)
        },
        move |result| {
            MOVING.set(false);
            // Choose… of the dialog the move began in, if it is still there, and
            // of the one open now: a dialog closed and opened again during the
            // move built its own Choose… while a move was running, insensitive.
            if let Some(choose) = choose.upgrade() {
                choose.set_sensitive(true);
            }
            if let Some(app) = app.upgrade()
                && let Some((_, open)) = open_dialog(&app)
                && let Some(choose) = named::<gtk::Button>(&open, CHOOSE_NAME)
            {
                choose.set_sensitive(true);
            }
            let text = move_result_text(result, &old_folder);
            match app.upgrade().and_then(|app| open_dialog(&app)) {
                Some((_, open)) => ctx.notice.show_over(&open, &text),
                None => ctx.notice.show(&text),
            }
            // Read again now that everything has arrived.
            if let Some(app) = app.upgrade() {
                recordings::set_dir(&app, folder);
            }
            // Last: with no window left the application ends now.
            drop(hold);
        },
    );
}

/// What to say when the move to a new folder has ended (`None`: its worker
/// stopped without an answer): how it went ([`moved_text`]), or that it could
/// not be done. Whatever says that something is still in the old folder names
/// that folder (`old`), since the Transcripts window lists only the new one
/// from now on. An error says why, never what a file holds.
fn move_result_text(result: Option<io::Result<MoveReport>>, old: &Path) -> String {
    match result {
        Some(Ok(report)) => moved_text(report, old),
        Some(Err(err)) => naming_old_folder(
            &format!("Could not move the transcripts; they are still in the old folder: {err}."),
            old,
        ),
        None => naming_old_folder(
            "Moving the transcripts stopped unexpectedly; some may still be in the old folder.",
            old,
        ),
    }
}

/// `text`, which says that something is in the old folder, followed by which
/// folder that is. Plain text, like every message: a folder name can hold any
/// character.
fn naming_old_folder(text: &str, old: &Path) -> String {
    format!("{text} Old folder: {}", old.display())
}

/// What to say after moving the transcripts from `old` to a new folder.
fn moved_text(report: MoveReport, old: &Path) -> String {
    let moved = format!(
        "Moved {} {}",
        report.moved,
        if report.moved == 1 {
            "transcript"
        } else {
            "transcripts"
        }
    );
    let mut text = match report.failed {
        0 => format!("{moved}."),
        1 => format!("{moved}; 1 could not be moved and is still in the old folder."),
        n => format!("{moved}; {n} could not be moved and are still in the old folder."),
    };
    match report.other {
        0 => {}
        1 => text.push_str(" 1 other file was left in the old folder."),
        n => text.push_str(&format!(" {n} other files were left in the old folder.")),
    }
    if report.failed > 0 || report.other > 0 {
        text = naming_old_folder(&text, old);
    }
    text
}

// ----- Sync -----

fn sync_group(app: &adw::Application, ctx: &Rc<AppContext>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Sync").build();
    let row = adw::ActionRow::builder()
        .title("Sync settings")
        .subtitle(SYNC_SUBTITLE)
        .use_markup(false)
        .build();
    let open = gtk::Button::builder()
        .label("Open")
        .valign(gtk::Align::Center)
        .build();
    // The Preferences make way: the sync settings are in another window.
    open.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[strong]
        ctx,
        move |open| {
            if let Some(dialog) = open
                .ancestor(adw::PreferencesDialog::static_type())
                .and_downcast::<adw::PreferencesDialog>()
            {
                dialog.close();
            }
            profiles::present_sync(&app, &ctx, window::connect_fn(&app, &ctx));
        }
    ));
    row.add_suffix(&open);
    row.set_activatable_widget(Some(&open));
    group.add(&row);
    group
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(moved: usize, failed: usize, other: usize) -> MoveReport {
        MoveReport {
            moved,
            failed,
            other,
        }
    }

    /// The old folder of the tests below; `<b>` shows it is plain text.
    const OLD: &str = "/home/me/<b>old</b> transcripts";

    fn told(moved: usize, failed: usize, other: usize) -> String {
        moved_text(report(moved, failed, other), Path::new(OLD))
    }

    #[test]
    fn the_move_is_reported_in_words_that_agree_with_the_numbers() {
        assert_eq!(told(12, 0, 0), "Moved 12 transcripts.");
        assert_eq!(told(1, 0, 0), "Moved 1 transcript.");
        assert_eq!(told(0, 0, 0), "Moved 0 transcripts.");
        assert_eq!(
            told(11, 1, 0),
            "Moved 11 transcripts; 1 could not be moved and is still in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
        assert_eq!(
            told(1, 3, 0),
            "Moved 1 transcript; 3 could not be moved and are still in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
    }

    #[test]
    fn files_left_behind_are_counted_in_the_words_too() {
        assert_eq!(
            told(12, 0, 3),
            "Moved 12 transcripts. 3 other files were left in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
        assert_eq!(
            told(1, 0, 1),
            "Moved 1 transcript. 1 other file was left in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
        assert_eq!(
            told(0, 0, 2),
            "Moved 0 transcripts. 2 other files were left in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
        assert_eq!(
            told(11, 1, 3),
            "Moved 11 transcripts; 1 could not be moved and is still in the old folder. \
             3 other files were left in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
        assert_eq!(
            told(1, 3, 1),
            "Moved 1 transcript; 3 could not be moved and are still in the old folder. \
             1 other file was left in the old folder. \
             Old folder: /home/me/<b>old</b> transcripts"
        );
    }

    #[test]
    fn a_failed_or_cut_short_move_names_the_old_folder() {
        let old = Path::new(OLD);
        assert_eq!(
            move_result_text(Some(Ok(report(2, 0, 0))), old),
            "Moved 2 transcripts.",
            "a move that went well has nothing left in the old folder to point at"
        );
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            move_result_text(Some(Err(denied)), old),
            "Could not move the transcripts; they are still in the old folder: permission \
             denied. Old folder: /home/me/<b>old</b> transcripts"
        );
        assert_eq!(
            move_result_text(None, old),
            "Moving the transcripts stopped unexpectedly; some may still be in the old \
             folder. Old folder: /home/me/<b>old</b> transcripts"
        );
    }
}
