//! The Transcripts window: a look back at the session transcripts in the
//! transcripts folder (the one chosen in the Preferences, else
//! `$XDG_DATA_HOME/moniker/transcripts`; [`moniker_core::transcript`]).
//!
//! Laid out like the Connection Profiles window (GNOME Settings): a sidebar
//! listing the recordings and, beside it, the chosen one's text; below 600 sp
//! one pane at a time. One window per application ([`present`]); it closes with
//! the last main window (`window.rs`). When another folder is chosen in the
//! Preferences, an open window switches to it ([`set_dir`]).
//!
//! * **The list** holds the transcripts in the folder: the regular files
//!   directly in it named the way Moniker names transcripts
//!   ([`list_recordings`]); any other file there is never listed, read,
//!   searched, exported or deleted, as the folder may be one the person uses
//!   for other things too. Newest or oldest first, optionally of one host,
//!   grouped under day headings (Today, Yesterday, then dates). It follows the
//!   directory with a `gio::FileMonitor`, so a session that ends while the
//!   window is open appears in it. Listing only reads directory entries, so it
//!   runs on the main thread.
//! * **Searching all recordings** (the sidebar's search entry, 250 ms after
//!   typing stops) reads every recording on a worker thread and narrows the
//!   list to those holding the text, each row with its match count. The host
//!   filter and the order still apply.
//! * **The viewer** reads the chosen file on a worker thread and streams the
//!   text into a read-only monospace `gtk::TextView` a chunk per idle callback,
//!   so a huge recording (unlimited scrollback) never freezes the window. A
//!   line of more than [`search::DISPLAY_LINE_MAX`] characters is shown broken
//!   into display lines of that length (also done on the worker), because the
//!   text view lays out one line of megabytes very slowly; the file, Export
//!   and every search keep the original text, and the viewer maps a match's
//!   offsets onto the display text ([`search::DisplayText`]). Its
//!   search bar highlights every match (found on a worker too, highlighted a
//!   batch per idle callback) and steps
//!   through them with ↓/Enter and ↑, showing "N of M"; Ctrl+F focuses it.
//!   Opening a recording while the list is searched carries the search over
//!   and jumps to the first match. Copying a selection (Ctrl+C, the context
//!   menu's Copy) copies the recording's own text, mapped back from the view's
//!   ([`search::DisplayText::to_original`]), so no display break reaches the
//!   clipboard ([`copy_selection`](State::copy_selection)). **Export…** saves
//!   a copy ([`export_to`]); **Delete** asks, then removes the file.
//!
//! Every result from a worker carries the generation it was started for and is
//! dropped if something newer has started since (another recording opened,
//! the search changed), so a slow answer never overwrites a newer one.
//!
//! Times are local: each start time is converted once
//! (`glib::DateTime::from_unix_local`, with the offset in force at that
//! instant) and the row's time, its day heading and the viewer's title all
//! come from that one conversion ([`local_time`]).
//!
//! Recordings are untrusted text: a file name (and so the host shown) or the
//! text itself can hold `<`, `&`, control or bidi characters, invalid UTF-8.
//! Everything is shown as plain text, never markup, and decoded lossily
//! ([`search::display_text`]). A file is opened without following a link and
//! without blocking, and read only if the opened file is a regular file
//! ([`read_recording`]), so nothing outside the directory is ever read even if
//! a listed file is swapped for a link to one.
//!
//! [`search`] is the display-free logic (searching, sorting, filtering and
//! grouping by day); it uses no GTK types, so it is unit-tested without a
//! display.

pub(crate) mod search;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::transcript::{Recording, list_recordings};
use time::{Date, Month, OffsetDateTime};

use crate::notify::NotificationArea;
use search::{Day, Order};

/// Widget name of the Transcripts window, how [`present`] finds an open
/// one.
const WINDOW_NAME: &str = "moniker-recordings";

/// Widget names tests look for: the sidebar list, its search entry and its two
/// drop-downs; the viewer's title, text view, search entry, "N of M" label and
/// error label; and the day heading rows of the list.
pub(crate) const LIST_NAME: &str = "recordings-list";
pub(crate) const SEARCH_NAME: &str = "recordings-search";
pub(crate) const HOST_FILTER_NAME: &str = "recordings-host";
pub(crate) const ORDER_NAME: &str = "recordings-order";
pub(crate) const TITLE_NAME: &str = "recording-title";
pub(crate) const VIEW_NAME: &str = "recording-view";
pub(crate) const FIND_NAME: &str = "recording-find";
pub(crate) const COUNTER_NAME: &str = "recording-counter";
pub(crate) const ERROR_NAME: &str = "recording-error";
pub(crate) const HEADING_NAME: &str = "recordings-day";

/// The window's action that Ctrl+F triggers: focus the viewer's search (the
/// list's while nothing is open). A shortcut of this window only, not an
/// application accelerator, so Ctrl+F still reaches programs in terminals.
pub(crate) const FIND_ACTION: &str = "recordings.find";

/// What the window says about every recording, under the list.
pub(crate) const SECRETS_WARNING: &str = "Transcripts hold whatever was on screen, which may include passwords or secrets typed or pasted.";

/// The host filter's first entry.
const ALL_HOSTS: &str = "All hosts";

/// The order drop-down's entries: index 0 is newest first, 1 oldest first.
const ORDERS: [&str; 2] = ["Newest first", "Oldest first"];

/// The sidebar search runs this long after typing stops.
const SEARCH_DELAY_MS: u32 = 250;

/// Changes in the directory come in bursts (a file created, then written);
/// the list is read again once, this long after the first.
const RELIST_DELAY: Duration = Duration::from_millis(200);

/// How much text goes into the viewer per idle callback, in bytes.
pub(crate) const CHUNK: usize = 256 * 1024;

/// How many matches are highlighted per idle callback.
const HIGHLIGHTS_PER_BATCH: usize = 2000;

/// Refusal of an export onto the recording itself.
const SAME_FILE: &str =
    "That is the transcript itself. Choose another name or folder for the copy.";

/// Show the Transcripts window of `app`, listing the recordings in
/// `dir`: the open one if there is one (it keeps the directory it was opened
/// with), else a new one.
pub fn present(app: &adw::Application, dir: PathBuf) -> adw::Window {
    let window = open_window(app).unwrap_or_else(|| build(app, dir));
    window.present();
    window
}

/// Have the open Transcripts window of `app`, if there is one, list `dir` from
/// now on (the Preferences call this when the person chooses another
/// transcripts folder): it follows that folder for changes instead of the old
/// one, and a transcript open in the viewer is closed, since it has moved or is
/// about to. Given the folder it already lists, it reads it again.
pub fn set_dir(app: &adw::Application, dir: PathBuf) {
    let Some(window) = open_window(app) else {
        return;
    };
    let state = OPEN.with_borrow_mut(|open| {
        open.retain(|state| state.upgrade().is_some());
        open.iter()
            .filter_map(std::rc::Weak::upgrade)
            .find(|state| state.window.upgrade().as_ref() == Some(&window))
    });
    if let Some(state) = state {
        state.switch_to(dir);
    }
}

thread_local! {
    /// The state of every Transcripts window, held weakly (each window owns its
    /// own), so [`set_dir`] can reach the one of an application. GTK runs on
    /// one thread.
    static OPEN: RefCell<Vec<std::rc::Weak<State>>> = const { RefCell::new(Vec::new()) };
}

/// Close the Transcripts window of `app`, if it is open.
pub fn close(app: &adw::Application) {
    if let Some(window) = open_window(app) {
        window.close();
    }
}

fn open_window(app: &adw::Application) -> Option<adw::Window> {
    app.windows()
        .into_iter()
        .find(|window| window.widget_name() == WINDOW_NAME)
        .and_downcast::<adw::Window>()
}

// ----- files -----

/// Open `path` for reading only if it is a regular file, without following a
/// link and without blocking: checked by name first (so a FIFO or a device is
/// not even opened), then opened with `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY`
/// and checked again on the opened file (`fstat`), which is what is read. A
/// file swapped for a link to somewhere else between the listing and here is
/// refused, not followed. The error says why, without the path.
fn open_regular(path: &Path) -> Result<File, String> {
    const NOT_REGULAR: &str = "it is not a regular file";
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(NOT_REGULAR.to_owned()),
        Err(err) => return Err(err.to_string()),
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|err| {
            if err.raw_os_error() == Some(libc::ELOOP) {
                NOT_REGULAR.to_owned()
            } else {
                err.to_string()
            }
        })?;
    let meta = file.metadata().map_err(|err| err.to_string())?;
    if !meta.file_type().is_file() {
        return Err(NOT_REGULAR.to_owned());
    }
    Ok(file)
}

/// The bytes of the recording at `path`, read as [`open_regular`] allows.
/// The error is a sentence to show as it is (it names no path).
pub(crate) fn read_recording(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    open_regular(path)
        .and_then(|mut file| file.read_to_end(&mut bytes).map_err(|err| err.to_string()))
        .map_err(|why| format!("This transcript cannot be opened: {why}."))?;
    Ok(bytes)
}

/// Copy the recording `source` to `target`, byte for byte, as a file only its
/// owner can read (mode 0600, like the original: it may hold secrets).
///
/// The source is opened as [`read_recording`] opens it. The target may be new
/// or an existing file (the file chooser has asked before replacing it). It is
/// opened without truncating and refused if it is the recording itself: the
/// same file by device and inode, so the same path, a path through a link and
/// a hard link all count, and the original is never emptied. A target that is
/// not a regular file (a FIFO, a device) is refused without blocking. Then the
/// target is made 0600 before anything is written, emptied, and filled. A
/// target this call created is removed again if the copy fails.
///
/// The error is a sentence to show as it is.
pub(crate) fn export_to(source: &Path, target: &Path) -> Result<(), String> {
    let mut from = open_regular(source).map_err(|why| export_failed(&why))?;
    let original = from.metadata().map_err(|err| export_failed(&err))?;
    let open = |create_new: bool| {
        OpenOptions::new()
            .write(true)
            .create_new(create_new)
            .mode(0o600)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(target)
    };
    let (mut to, created) = match open(true) {
        Ok(file) => (file, true),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            (open(false).map_err(|err| export_failed(&err))?, false)
        }
        Err(err) => return Err(export_failed(&err)),
    };
    let result = copy_into(&mut from, &original, &mut to);
    if result.is_err() && created {
        let _ = fs::remove_file(target);
    }
    result
}

/// The part of [`export_to`] after both files are open.
fn copy_into(from: &mut File, original: &Metadata, to: &mut File) -> Result<(), String> {
    let meta = to.metadata().map_err(|err| export_failed(&err))?;
    if (meta.dev(), meta.ino()) == (original.dev(), original.ino()) {
        return Err(SAME_FILE.to_owned());
    }
    if !meta.file_type().is_file() {
        return Err(export_failed(&"choose a regular file to save the copy in"));
    }
    to.set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| to.set_len(0))
        .and_then(|()| io::copy(from, to))
        .map_err(|err| export_failed(&err))?;
    Ok(())
}

fn export_failed(why: &dyn std::fmt::Display) -> String {
    format!("Could not export the transcript: {why}.")
}

// ----- background work -----

/// Run `work` on a worker thread and call `done` with its result on the main
/// thread, from the GLib main loop (`None` if the worker could not start or
/// panicked). Never blocks the caller. Only `work` crosses to the worker;
/// `done` stays here and may hold GTK objects. The same approach as
/// `sync::start_sync`. (The Preferences move transcripts this way too.)
pub(crate) fn in_background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(Option<T>) + 'static,
) {
    let (tx, rx) = async_channel::bounded::<T>(1);
    let spawned = std::thread::Builder::new()
        .name("moniker-recordings".to_owned())
        .spawn(move || {
            // A closed channel means the main loop is gone.
            let _ = tx.send_blocking(work());
        });
    if spawned.is_err() {
        done(None);
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        done(rx.recv().await.ok());
    });
}

// ----- local time -----

/// The instant `secs` (Unix seconds) on the local clock, with the UTC offset in
/// force at that instant (so a recording from before a daylight-saving change
/// shows the time the clock showed then). A time glib cannot show (outside
/// years 1 to 9999: a file name can say year 0000) is the epoch.
///
/// The row's time, the day heading above it ([`local_date`]) and the viewer's
/// title all come from this one conversion, so they always agree.
fn local_time(secs: i64) -> glib::DateTime {
    glib::DateTime::from_unix_local(secs)
        .or_else(|_| glib::DateTime::from_unix_utc(0))
        .expect("glib can show the epoch")
}

/// The calendar date of a local time, for [`search::day_of`].
fn local_date(when: &glib::DateTime) -> Date {
    u8::try_from(when.month())
        .ok()
        .and_then(|month| Month::try_from(month).ok())
        .zip(u8::try_from(when.day_of_month()).ok())
        .and_then(|(month, day)| Date::from_calendar_date(when.year(), month, day).ok())
        .unwrap_or(OffsetDateTime::UNIX_EPOCH.date())
}

/// Today's local date.
fn today() -> Date {
    glib::DateTime::now_local()
        .map(|now| local_date(&now))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH.date())
}

/// The text of a day heading: "Today", "Yesterday", or the date ("October
/// 12", with the year when it is not this year's), the month's name in the
/// person's language.
fn heading_text(day: Day, today: Date) -> String {
    match day {
        Day::Today => "Today".to_owned(),
        Day::Yesterday => "Yesterday".to_owned(),
        Day::Date(year, month, day) => {
            let format = if year == today.year() {
                "%B %-d"
            } else {
                "%B %-d, %Y"
            };
            glib::DateTime::from_utc(year, month.into(), day.into(), 0, 0, 0.0)
                .and_then(|date| date.format(format))
                .map(String::from)
                .unwrap_or_else(|_| format!("{year:04}-{month:02}-{day:02}"))
        }
    }
}

/// The local time of day: `HH:MM`, or `HH:MM:SS`.
fn clock(when: &glib::DateTime, seconds: bool) -> String {
    when.format(if seconds { "%H:%M:%S" } else { "%H:%M" })
        .map(String::from)
        .unwrap_or_default()
}

/// When a recording started, for the viewer's title and the delete dialog:
/// its day heading and the time to the second.
fn started_text(started: i64) -> String {
    let when = local_time(started);
    let today = today();
    let day = heading_text(search::day_of(local_date(&when), today), today);
    format!("{day}, {}", clock(&when, true))
}

fn matches_text(count: usize) -> String {
    if count == 1 {
        "1 match".to_owned()
    } else {
        format!("{count} matches")
    }
}

// ----- the window -----

/// The window's state. Owned by the window (see [`build`]); every handler holds
/// it weakly.
struct State {
    /// The folder listed: the one the window was opened for, until
    /// [`set_dir`] switches it.
    dir: RefCell<PathBuf>,
    window: glib::WeakRef<adw::Window>,
    split: adw::NavigationSplitView,
    /// Where messages (an export's result, a failed delete) appear.
    notifications: NotificationArea,

    // The sidebar.
    search: gtk::SearchEntry,
    hosts: gtk::DropDown,
    host_names: gtk::StringList,
    order: gtk::DropDown,
    list: gtk::ListBox,
    placeholder: gtk::Label,

    // The viewer.
    content: adw::NavigationPage,
    title: adw::WindowTitle,
    export: gtk::Button,
    delete: gtk::Button,
    find_bar: gtk::Box,
    find: gtk::SearchEntry,
    counter: gtk::Label,
    previous: gtk::Button,
    next: gtk::Button,
    pages: gtk::Stack,
    view: gtk::TextView,
    error: gtk::Label,
    match_tag: gtk::TextTag,
    current_tag: gtk::TextTag,
    current_mark: gtk::TextMark,

    /// Every recording in the directory, as last listed.
    all: RefCell<Vec<Recording>>,
    /// Why the directory could not be listed, if it could not.
    listing_problem: RefCell<Option<String>>,
    /// The recording rows shown, with what each stands for.
    listed: RefCell<Vec<(gtk::ListBoxRow, Recording)>>,
    /// Match counts by file from the last sidebar search that finished; `None`
    /// while the search entry is empty. Kept while a newer search runs, so the
    /// list does not flash back to everything between keystrokes.
    counts: RefCell<Option<HashMap<PathBuf, usize>>>,
    /// The sidebar search's generation; shared with its worker, which stops
    /// reading files once a newer search has started.
    search_gen: Arc<AtomicU64>,
    /// The recording in the viewer.
    open: RefCell<Option<Recording>>,
    /// Its text as the file holds it (read lossily), from the moment it starts
    /// going into the view: what a copied selection is taken from and, once
    /// all of it is in ([`filled`](Self::filled)), what the viewer's search
    /// runs on. The view shows [`display`](Self::display)'s text, with the long
    /// lines broken.
    text: RefCell<Option<Arc<str>>>,
    /// Where the view's text differs from `text`: its line breaks, mapping a
    /// match's offsets to the view's and a selection's back. The text itself
    /// is not kept here, only in the view. Empty while no recording is shown.
    display: RefCell<search::DisplayText>,
    /// All of `text` is in the view.
    filled: Cell<bool>,
    load_gen: Cell<u64>,
    find_gen: Cell<u64>,
    /// The viewer search the matches are (or are being found, or will be
    /// found once the text is in) for.
    found_query: RefCell<String>,
    /// Whether the matches for `found_query` are in.
    found: Cell<bool>,
    matches: RefCell<Vec<Range<usize>>>,
    current: Cell<usize>,
    /// The host filter is being refilled: its selection signals are not the
    /// person's.
    rebuilding: Cell<bool>,
    relist_pending: Cell<bool>,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

fn build(app: &adw::Application, dir: PathBuf) -> adw::Window {
    // Sidebar: header, search and the two drop-downs, the list, the warning.
    let sidebar_header = adw::HeaderBar::new();
    let search = gtk::SearchEntry::builder()
        .name(SEARCH_NAME)
        .placeholder_text("Search all transcripts")
        .build();
    search.set_search_delay(SEARCH_DELAY_MS);
    let host_names = gtk::StringList::new(&[ALL_HOSTS]);
    let hosts = gtk::DropDown::builder()
        .name(HOST_FILTER_NAME)
        .model(&host_names)
        .tooltip_text("Host")
        .hexpand(true)
        .build();
    let order = gtk::DropDown::builder()
        .name(ORDER_NAME)
        .model(&gtk::StringList::new(&ORDERS))
        .tooltip_text("Order")
        .hexpand(true)
        .build();
    let choices = gtk::Box::builder().spacing(6).homogeneous(true).build();
    choices.append(&hosts);
    choices.append(&order);
    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    controls.append(&search);
    controls.append(&choices);
    let list = gtk::ListBox::builder()
        .name(LIST_NAME)
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["navigation-sidebar"])
        .build();
    let placeholder = gtk::Label::builder()
        .use_markup(false)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    list.set_placeholder(Some(&placeholder));
    let warning = gtk::Label::builder()
        .label(SECRETS_WARNING)
        .use_markup(false)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    let sidebar_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_column.append(&controls);
    sidebar_column.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build(),
    );
    sidebar_column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_column.append(&warning);
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.set_content(Some(&sidebar_column));
    let sidebar = adw::NavigationPage::builder()
        .title("Transcripts")
        .child(&sidebar_view)
        .build();

    // Content: header with the recording and its actions, the search bar, and
    // the text (or nothing chosen, or why it cannot be shown).
    let title = adw::WindowTitle::builder().name(TITLE_NAME).build();
    let export = gtk::Button::builder().label("Export…").build();
    let delete = gtk::Button::builder()
        .label("Delete")
        .css_classes(["destructive-action"])
        .build();
    let content_header = adw::HeaderBar::builder().title_widget(&title).build();
    content_header.pack_end(&delete);
    content_header.pack_end(&export);
    let find = gtk::SearchEntry::builder()
        .name(FIND_NAME)
        .placeholder_text("Search this transcript")
        .hexpand(true)
        .build();
    let counter = gtk::Label::builder()
        .name(COUNTER_NAME)
        .use_markup(false)
        .width_chars(10)
        .css_classes(["dim-label", "numeric"])
        .build();
    let previous = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Previous Match")
        .build();
    let next = gtk::Button::builder()
        .icon_name("go-down-symbolic")
        .tooltip_text("Next Match")
        .build();
    let find_bar = gtk::Box::builder()
        .spacing(6)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    find_bar.append(&find);
    find_bar.append(&counter);
    find_bar.append(&previous);
    find_bar.append(&next);
    let view = gtk::TextView::builder()
        .name(VIEW_NAME)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let buffer = view.buffer();
    let match_tag = gtk::TextTag::builder()
        .name("match")
        .background_rgba(&gdk::RGBA::new(0.97, 0.83, 0.18, 0.45))
        .build();
    // Added after `match_tag`, so it wins where both apply.
    let current_tag = gtk::TextTag::builder()
        .name("current-match")
        .background_rgba(&gdk::RGBA::new(1.0, 0.47, 0.0, 0.85))
        .build();
    buffer.tag_table().add(&match_tag);
    buffer.tag_table().add(&current_tag);
    let current_mark = buffer.create_mark(Some("current-match"), &buffer.start_iter(), true);
    let error = gtk::Label::builder()
        .name(ERROR_NAME)
        .use_markup(false)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .valign(gtk::Align::Center)
        .margin_start(24)
        .margin_end(24)
        .build();
    let pages = gtk::Stack::new();
    pages.add_named(
        &adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No transcript selected")
            .description("Choose a transcript in the list.")
            .build(),
        Some("empty"),
    );
    pages.add_named(
        &gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build(),
        Some("text"),
    );
    pages.add_named(&error, Some("error"));
    let body = adw::ToolbarView::new();
    body.add_top_bar(&content_header);
    body.add_top_bar(&find_bar);
    body.set_content(Some(&pages));
    let content = adw::NavigationPage::builder()
        .title("Transcript")
        .child(&body)
        .build();

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(260.0)
        .build();
    // Over the whole window, header bars included: the messages go below them.
    let notifications = NotificationArea::new(&split);
    notifications.below_header_bar();
    let window = adw::Window::builder()
        .application(app)
        .name(WINDOW_NAME)
        .title("Transcripts")
        .default_width(900)
        .default_height(640)
        .width_request(360)
        .height_request(400)
        .content(&notifications)
        .build();
    // Narrow: one pane at a time with back navigation, as Settings does.
    let narrow = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 600sp").expect("a valid breakpoint condition"),
    );
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);

    let state = Rc::new(State {
        dir: RefCell::new(dir),
        window: window.downgrade(),
        split,
        notifications,
        search,
        hosts,
        host_names,
        order,
        list,
        placeholder,
        content,
        title,
        export,
        delete,
        find_bar,
        find,
        counter,
        previous,
        next,
        pages,
        view,
        error,
        match_tag,
        current_tag,
        current_mark,
        all: RefCell::new(Vec::new()),
        listing_problem: RefCell::new(None),
        listed: RefCell::new(Vec::new()),
        counts: RefCell::new(None),
        search_gen: Arc::new(AtomicU64::new(0)),
        open: RefCell::new(None),
        text: RefCell::new(None),
        display: RefCell::new(search::DisplayText::default()),
        filled: Cell::new(false),
        load_gen: Cell::new(0),
        find_gen: Cell::new(0),
        found_query: RefCell::new(String::new()),
        found: Cell::new(false),
        matches: RefCell::new(Vec::new()),
        current: Cell::new(0),
        rebuilding: Cell::new(false),
        relist_pending: Cell::new(false),
        monitor: RefCell::new(None),
    });
    OPEN.with_borrow_mut(|open| {
        open.retain(|state| state.upgrade().is_some());
        open.push(Rc::downgrade(&state));
    });
    state.wire(&window);
    state.watch();
    state.show_nothing();
    state.relist();
    window
}

impl State {
    /// Connect the window's signals. Handlers hold the state weakly; the one
    /// strong reference lives in the window's destroy handler, so the state
    /// goes when the window does.
    fn wire(self: &Rc<Self>, window: &adw::Window) {
        // `with(f)`: a handler that calls `f` on the state while it lives.
        let weak = Rc::downgrade(self);
        let with = move |f: fn(&Rc<State>)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    f(&state);
                }
            }
        };

        let search = with(State::start_search);
        self.search.connect_search_changed(move |_| search());
        for choice in [&self.hosts, &self.order] {
            let rebuild = with(|state| {
                if !state.rebuilding.get() {
                    state.rebuild_list();
                }
            });
            choice.connect_selected_notify(move |_| rebuild());
        }
        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, row| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let rec = state
                .listed
                .borrow()
                .iter()
                .find(|(listed, _)| listed == row)
                .map(|(_, rec)| rec.clone());
            if let Some(rec) = rec {
                state.open_recording(rec);
            }
        });

        // Copying out of the viewer takes the recording's own text, never the
        // view's ([`State::copy_selection`]). Ctrl+C, Ctrl+Insert and the
        // context menu's Copy (the view's `clipboard.copy` action) all come
        // here as `copy-clipboard`; Ctrl+X and Shift+Delete come as
        // `cut-clipboard`, which in a read-only view copies just the same (and
        // removes nothing). The view's own handler, which would copy the
        // display text with its breaks, never runs.
        let copy = with(State::copy_selection);
        self.view.connect_copy_clipboard(move |view| {
            copy();
            view.stop_signal_emission_by_name("copy-clipboard");
        });
        let copy = with(State::copy_selection);
        self.view.connect_cut_clipboard(move |view| {
            copy();
            view.stop_signal_emission_by_name("cut-clipboard");
        });

        let export = with(State::ask_export);
        self.export.connect_clicked(move |_| export());
        let delete = with(State::ask_delete);
        self.delete.connect_clicked(move |_| delete());

        // The viewer's search: a changed text is found again; Enter is "next"
        // unless the text changed and has not been searched yet (a fast
        // typist's Enter); ↓ and Ctrl+G step forward, ↑ and Ctrl+Shift+G back.
        let changed = with(|state| {
            if state.find.text() != *state.found_query.borrow() {
                state.run_find();
            }
        });
        self.find.connect_search_changed(move |_| changed());
        let enter = with(|state| {
            if state.find.text() == *state.found_query.borrow() {
                state.step(true);
            } else {
                state.run_find();
            }
        });
        self.find.connect_activate(move |_| enter());
        let forward = with(|state| state.step(true));
        let back = with(|state| state.step(false));
        self.next.connect_clicked({
            let forward = forward.clone();
            move |_| forward()
        });
        self.find.connect_next_match(move |_| forward());
        self.previous.connect_clicked({
            let back = back.clone();
            move |_| back()
        });
        self.find.connect_previous_match(move |_| back());

        // Ctrl+F: the viewer's search, or the list's while nothing is open.
        let focus_find = with(|state| {
            if state.open.borrow().is_some() {
                state.split.set_show_content(true);
                state.find.grab_focus();
            } else {
                state.split.set_show_content(false);
                state.search.grab_focus();
            }
        });
        let find_action = gio::SimpleAction::new("find", None);
        find_action.connect_activate(move |_, _| focus_find());
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&find_action);
        window.insert_action_group("recordings", Some(&actions));
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::NamedAction::new(FIND_ACTION)),
        ));
        window.add_controller(shortcuts);

        let keep = Rc::clone(self);
        window.connect_destroy(move |_| {
            if let Some(monitor) = keep.monitor.take() {
                monitor.cancel();
            }
            // Work still running finds nothing to update.
            keep.load_gen.set(keep.load_gen.get() + 1);
            keep.find_gen.set(keep.find_gen.get() + 1);
            keep.search_gen.fetch_add(1, Ordering::SeqCst);
        });
    }

    /// Follow the directory: list it again shortly after anything in it
    /// changes. A watch that cannot be set up is reported on stderr; the
    /// window works without it, only without live updates.
    fn watch(self: &Rc<Self>) {
        let monitor = match gio::File::for_path(&*self.dir.borrow())
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => monitor,
            Err(err) => {
                eprintln!("moniker: cannot watch the recordings for changes: {err}");
                return;
            }
        };
        // Weak: the state owns the monitor, which owns this closure.
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, _| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.relist_pending.replace(true) {
                return;
            }
            let weak = Rc::downgrade(&state);
            glib::timeout_add_local_once(RELIST_DELAY, move || {
                if let Some(state) = weak.upgrade() {
                    state.relist_pending.set(false);
                    state.relist();
                }
            });
        });
        self.monitor.replace(Some(monitor));
    }

    /// Tell the person `text`, at the top of the window.
    fn tell(&self, text: &str) {
        self.notifications.show(text);
    }

    /// List `dir` from now on ([`set_dir`]): watch it instead of the old
    /// folder, close the viewer if the folder changed, and read the listing
    /// again either way.
    fn switch_to(self: &Rc<Self>, dir: PathBuf) {
        if *self.dir.borrow() != dir {
            if let Some(monitor) = self.monitor.take() {
                monitor.cancel();
            }
            self.dir.replace(dir);
            self.show_nothing();
            self.watch();
        }
        self.relist();
    }

    // ----- the list -----

    /// Read the directory again and show what is there now. A recording in the
    /// viewer that is gone (deleted here or elsewhere) leaves the viewer empty.
    /// An active search runs again, since files may have come or changed.
    fn relist(self: &Rc<Self>) {
        // Bound first so the borrow ends here.
        let listed = list_recordings(&self.dir.borrow());
        let (recordings, problem) = match listed {
            Ok(recordings) => (recordings, None),
            Err(err) => (
                Vec::new(),
                Some(format!("The transcripts folder cannot be read: {err}.")),
            ),
        };
        let gone = self
            .open
            .borrow()
            .as_ref()
            .is_some_and(|open| !recordings.iter().any(|r| r.path == open.path));
        self.all.replace(recordings);
        self.listing_problem.replace(problem);
        self.refresh_hosts();
        if gone {
            self.show_nothing();
        }
        if !self.search.text().is_empty() {
            self.start_search();
        }
        self.rebuild_list();
    }

    /// Refill the host filter from the listing, keeping the chosen host while
    /// it still has recordings (else back to "All hosts").
    fn refresh_hosts(&self) {
        let wanted = search::hosts(&self.all.borrow());
        let shown: Vec<String> = (1..self.host_names.n_items())
            .filter_map(|i| self.host_names.string(i))
            .map(String::from)
            .collect();
        if shown == wanted {
            return;
        }
        let chosen = self.chosen_host();
        let items: Vec<&str> = std::iter::once(ALL_HOSTS)
            .chain(wanted.iter().map(String::as_str))
            .collect();
        self.rebuilding.set(true);
        self.host_names.splice(0, self.host_names.n_items(), &items);
        let index = chosen
            .and_then(|host| wanted.iter().position(|h| *h == host))
            .map_or(0, |i| i + 1);
        self.hosts.set_selected(u32::try_from(index).unwrap_or(0));
        self.rebuilding.set(false);
    }

    /// The host the list is narrowed to, `None` for all.
    fn chosen_host(&self) -> Option<String> {
        match self.hosts.selected() {
            0 | gtk::INVALID_LIST_POSITION => None,
            index => self.host_names.string(index).map(String::from),
        }
    }

    /// Start the search across all recordings for the sidebar's text, or with
    /// an empty text show everything again. Each file is read on a worker, as
    /// the viewer reads it (one it cannot open counts as no match); the worker
    /// stops early once a newer search has started.
    fn start_search(self: &Rc<Self>) {
        let generation = self.search_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let query = self.search.text().to_string();
        if query.is_empty() {
            self.counts.replace(None);
            self.rebuild_list();
            return;
        }
        let paths: Vec<PathBuf> = self.all.borrow().iter().map(|r| r.path.clone()).collect();
        let latest = Arc::clone(&self.search_gen);
        let weak = Rc::downgrade(self);
        in_background(
            move || {
                let mut counts = HashMap::new();
                for path in paths {
                    if latest.load(Ordering::SeqCst) != generation {
                        return None;
                    }
                    let count = read_recording(&path).map_or(0, |bytes| {
                        search::count_matches(&search::display_text(&bytes), &query)
                    });
                    counts.insert(path, count);
                }
                Some(counts)
            },
            move |result| {
                let (Some(state), Some(Some(counts))) = (weak.upgrade(), result) else {
                    return;
                };
                if state.search_gen.load(Ordering::SeqCst) == generation {
                    state.counts.replace(Some(counts));
                    state.rebuild_list();
                }
            },
        );
    }

    /// Show the recordings again: the chosen host's, in the chosen order,
    /// narrowed by the last search results while the search entry has text,
    /// under a heading for each local day. The open recording's row stays
    /// selected.
    fn rebuild_list(&self) {
        let order = if self.order.selected() == 1 {
            Order::OldestFirst
        } else {
            Order::NewestFirst
        };
        let mut shown = search::arrange(&self.all.borrow(), self.chosen_host().as_deref(), order);
        let searching = !self.search.text().is_empty();
        let counts = self.counts.borrow();
        let counts = counts.as_ref().filter(|_| searching);
        if let Some(counts) = counts {
            shown.retain(|rec| counts.get(&rec.path).is_some_and(|&n| n > 0));
        }
        let placeholder = if let Some(problem) = self.listing_problem.borrow().as_ref() {
            problem.clone()
        } else if self.all.borrow().is_empty() {
            "No transcripts yet. Turn on Save transcript when connecting to save a session's transcript."
                .to_owned()
        } else if counts.is_some() {
            "No transcript holds this text.".to_owned()
        } else {
            "No transcripts".to_owned()
        };
        self.placeholder.set_label(&placeholder);

        // Not `remove_all`: that would take the placeholder with it.
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let open = self.open.borrow().as_ref().map(|r| r.path.clone());
        let today = today();
        let mut last_day = None;
        let mut listed = Vec::new();
        for rec in shown {
            let when = local_time(rec.started);
            let day = search::day_of(local_date(&when), today);
            if last_day != Some(day) {
                self.list.append(&heading_row(&heading_text(day, today)));
                last_day = Some(day);
            }
            let mut subtitle = clock(&when, false);
            if let Some(counts) = counts {
                let count = counts.get(&rec.path).copied().unwrap_or(0);
                subtitle = format!("{subtitle} · {}", matches_text(count));
            }
            let name = rec
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let row = adw::ActionRow::builder()
                .name(name.as_str())
                .title(rec.host.as_str())
                .subtitle(subtitle)
                .use_markup(false)
                .title_lines(1)
                .subtitle_lines(1)
                .activatable(true)
                .build();
            self.list.append(&row);
            if open.as_ref() == Some(&rec.path) {
                self.list.select_row(Some(&row));
            }
            listed.push((row.upcast::<gtk::ListBoxRow>(), rec));
        }
        self.listed.replace(listed);
    }

    // ----- the viewer -----

    /// Show `rec` in the viewer: its title at once, its text once a worker
    /// has read it. With a search in the sidebar, the viewer's search takes it
    /// over and the first match is shown.
    fn open_recording(self: &Rc<Self>, rec: Recording) {
        let generation = self.load_gen.get() + 1;
        self.load_gen.set(generation);
        self.clear_text();
        self.title.set_title(&rec.host);
        self.title.set_subtitle(&started_text(rec.started));
        self.content.set_title(&rec.host);
        self.export.set_visible(true);
        self.delete.set_visible(true);
        self.find_bar.set_visible(true);
        self.error.set_label("");
        self.pages.set_visible_child_name("text");
        let carried = self.search.text();
        if !carried.is_empty() {
            self.find.set_text(&carried);
        }
        let path = rec.path.clone();
        self.open.replace(Some(rec));
        self.split.set_show_content(true);

        let weak = Rc::downgrade(self);
        // The text is made an `Arc<str>` on the worker too, and so are its
        // display lines: doing either here would copy the whole recording (up
        // to the size limit) on the main thread.
        in_background(
            move || {
                read_recording(&path).map(|bytes| {
                    let text = Arc::<str>::from(search::display_text(&bytes));
                    let display = search::for_display(&text);
                    (text, display)
                })
            },
            move |result| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                if state.load_gen.get() != generation {
                    return;
                }
                match result {
                    Some(Ok((text, display))) => state.fill(generation, text, display),
                    Some(Err(why)) => state.show_error(&why),
                    None => state.show_error(
                        "This transcript cannot be opened: reading it stopped unexpectedly.",
                    ),
                }
            },
        );
    }

    /// Put the recording into the view a chunk per idle callback, then find
    /// the viewer's search in it. `text` is the recording as the file holds it
    /// and `display` the same with its long lines broken: the view gets
    /// `display`'s text, whose breaks are already in it, so a chunk boundary
    /// anywhere (a break right at one included) neither loses nor doubles one.
    /// The search runs on `text`. Stops if another recording is opened
    /// meanwhile.
    ///
    /// `text` and where the breaks are go into the state at once, so a
    /// selection copied while the rest is still coming in maps back too; the
    /// display text itself goes only into the view (this callback holds it
    /// until then), never a second copy into the state.
    fn fill(self: &Rc<Self>, generation: u64, text: Arc<str>, mut display: search::DisplayText) {
        let shown = std::mem::take(&mut display.text);
        self.display.replace(display);
        self.text.replace(Some(text));
        let weak = Rc::downgrade(self);
        let mut at = 0;
        glib::idle_add_local(move || {
            let Some(state) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if state.load_gen.get() != generation {
                return glib::ControlFlow::Break;
            }
            let mut end = (at + CHUNK).min(shown.len());
            while !shown.is_char_boundary(end) {
                end += 1;
            }
            let buffer = state.view.buffer();
            buffer.insert(&mut buffer.end_iter(), &shown[at..end]);
            at = end;
            if at < shown.len() {
                return glib::ControlFlow::Continue;
            }
            buffer.place_cursor(&buffer.start_iter());
            state.filled.set(true);
            state.run_find();
            glib::ControlFlow::Break
        });
    }

    fn show_error(&self, why: &str) {
        self.clear_text();
        self.error.set_label(why);
        self.pages.set_visible_child_name("error");
    }

    /// Nothing in the viewer: no recording, no text, no matches.
    fn show_nothing(&self) {
        self.load_gen.set(self.load_gen.get() + 1);
        self.clear_text();
        self.open.replace(None);
        self.title.set_title("");
        self.title.set_subtitle("");
        self.content.set_title("Transcript");
        self.export.set_visible(false);
        self.delete.set_visible(false);
        self.find_bar.set_visible(false);
        self.error.set_label("");
        self.pages.set_visible_child_name("empty");
        self.list.unselect_all();
        self.split.set_show_content(false);
    }

    /// Empty the view, dropping the text and its matches (and any search for
    /// them still running).
    fn clear_text(&self) {
        self.find_gen.set(self.find_gen.get() + 1);
        self.text.replace(None);
        self.display.replace(search::DisplayText::default());
        self.filled.set(false);
        self.matches.replace(Vec::new());
        self.current.set(0);
        self.found.set(false);
        self.view.buffer().set_text("");
        self.show_counter();
    }

    /// Find the viewer's search text in the open recording, on a worker, then
    /// highlight every match and show the first. Before the text is in, only
    /// remembers the search: [`fill`](Self::fill) runs it once it is.
    fn run_find(self: &Rc<Self>) {
        let query = self.find.text().to_string();
        self.found_query.replace(query.clone());
        let generation = self.find_gen.get() + 1;
        self.find_gen.set(generation);
        let buffer = self.view.buffer();
        let (start, end) = buffer.bounds();
        buffer.remove_tag(&self.match_tag, &start, &end);
        buffer.remove_tag(&self.current_tag, &start, &end);
        self.matches.replace(Vec::new());
        self.current.set(0);
        self.found.set(false);
        self.show_counter();
        let text = self.text.borrow().clone();
        let Some(text) = text.filter(|_| self.filled.get() && !query.is_empty()) else {
            return;
        };
        let weak = Rc::downgrade(self);
        in_background(
            move || search::find_matches(&text, &query),
            move |found| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                if state.find_gen.get() != generation {
                    return;
                }
                state.matches.replace(found.unwrap_or_default());
                state.found.set(true);
                state.show_current();
                state.show_counter();
                state.highlight(generation);
            },
        );
    }

    /// Highlight every match, a batch now and a batch per idle callback after
    /// that, so a search that matches all over a huge recording does not
    /// freeze the window. Stops if the search changes meanwhile.
    fn highlight(self: &Rc<Self>, generation: u64) {
        let weak = Rc::downgrade(self);
        let mut done = 0;
        let mut batch = move || {
            let Some(state) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if state.find_gen.get() != generation {
                return glib::ControlFlow::Break;
            }
            let matches = state.matches.borrow();
            let end = (done + HIGHLIGHTS_PER_BATCH).min(matches.len());
            let buffer = state.view.buffer();
            for range in &matches[done..end] {
                let (start, end) = state.iters(range);
                buffer.apply_tag(&state.match_tag, &start, &end);
            }
            done = end;
            if done < matches.len() {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        };
        if matches!(batch(), glib::ControlFlow::Continue) {
            glib::idle_add_local(batch);
        }
    }

    /// Buffer positions of a match: a character range of the recording's own
    /// text, mapped onto the view's, which has the long lines broken
    /// ([`search::DisplayText::range_to_display`]). A match across a break is
    /// one range with the inserted newline inside it.
    fn iters(&self, range: &Range<usize>) -> (gtk::TextIter, gtk::TextIter) {
        let range = self.display.borrow().range_to_display(range);
        let buffer = self.view.buffer();
        let at = |offset: usize| buffer.iter_at_offset(i32::try_from(offset).unwrap_or(i32::MAX));
        (at(range.start), at(range.end))
    }

    /// Put the view's selection on the clipboard as plain text taken from the
    /// recording's own text: the selection's offsets in the view mapped back
    /// to the original ([`search::DisplayText::to_original`]), so a selection
    /// across a display break gets no newline that the file does not have.
    /// Nothing selected, or no text shown, copies nothing and leaves the
    /// clipboard alone.
    ///
    /// Only copying goes through here (Ctrl+C, the context menu, Ctrl+X; see
    /// [`wire`](Self::wire)). The primary selection (middle-click paste) and
    /// dragging the selection somewhere are the text view's own and still
    /// carry the view's text, display breaks included; Export gives the exact
    /// file.
    fn copy_selection(self: &Rc<Self>) {
        let buffer = self.view.buffer();
        let Some((start, end)) = buffer.selection_bounds() else {
            return;
        };
        let Some(text) = self.text.borrow().clone() else {
            return;
        };
        let offset = |iter: &gtk::TextIter| usize::try_from(iter.offset()).unwrap_or(0);
        let display = self.display.borrow();
        let range = display.to_original(offset(&start))..display.to_original(offset(&end));
        self.view
            .clipboard()
            .set_text(search::char_slice(&text, range));
    }

    /// Mark the current match and scroll to it.
    fn show_current(&self) {
        let matches = self.matches.borrow();
        let Some(range) = matches.get(self.current.get()) else {
            return;
        };
        let (start, end) = self.iters(range);
        let buffer = self.view.buffer();
        buffer.apply_tag(&self.current_tag, &start, &end);
        buffer.move_mark(&self.current_mark, &start);
        self.view
            .scroll_to_mark(&self.current_mark, 0.1, true, 0.0, 0.3);
    }

    /// The next (or previous) match becomes the current one, wrapping round.
    fn step(&self, forward: bool) {
        let count = self.matches.borrow().len();
        if count == 0 {
            return;
        }
        if let Some(range) = self.matches.borrow().get(self.current.get()) {
            let (start, end) = self.iters(range);
            self.view
                .buffer()
                .remove_tag(&self.current_tag, &start, &end);
        }
        let current = self.current.get();
        self.current.set(if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        });
        self.show_current();
        self.show_counter();
    }

    /// "N of M", "No matches", or nothing while there is no search (or its
    /// matches are not in yet); ↑ and ↓ only while there is somewhere to go.
    fn show_counter(&self) {
        let count = self.matches.borrow().len();
        let text = if !self.found.get() || self.found_query.borrow().is_empty() {
            String::new()
        } else if count == 0 {
            "No matches".to_owned()
        } else {
            format!("{} of {count}", self.current.get() + 1)
        };
        self.counter.set_label(&text);
        self.previous.set_sensitive(count > 0);
        self.next.set_sensitive(count > 0);
    }

    // ----- export and delete -----

    /// Ask where to save a copy of the open recording (its own file name by
    /// default), then copy it there on a worker ([`export_to`]).
    fn ask_export(self: &Rc<Self>) {
        let Some(rec) = self.open.borrow().clone() else {
            return;
        };
        let name = rec
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dialog = export_dialog(&name);
        let weak = Rc::downgrade(self);
        dialog.save(
            self.window.upgrade().as_ref(),
            gio::Cancellable::NONE,
            move |chosen| {
                // An error here is the person canceling.
                let (Ok(file), Some(state)) = (chosen, weak.upgrade()) else {
                    return;
                };
                let Some(target) = file.path() else {
                    state
                        .tell("Could not export the transcript: choose a folder on this computer.");
                    return;
                };
                let source = rec.path.clone();
                let weak = Rc::downgrade(&state);
                in_background(
                    move || export_to(&source, &target).map(|()| target),
                    move |result| {
                        let Some(state) = weak.upgrade() else {
                            return;
                        };
                        match result {
                            Some(Ok(target)) => {
                                state.tell(&format!("Exported a copy to {}", target.display()))
                            }
                            Some(Err(why)) => state.tell(&why),
                            None => state.tell("Could not export the transcript."),
                        }
                    },
                );
            },
        );
    }

    /// Ask before deleting the open recording, naming its host and start time
    /// as plain text.
    fn ask_delete(self: &Rc<Self>) {
        let Some(rec) = self.open.borrow().clone() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Delete this transcript?")
            .heading_use_markup(false)
            .body(format!(
                "{}\n{}\n\nThe file is removed from this computer. This cannot be undone.",
                rec.host,
                started_text(rec.started)
            ))
            .body_use_markup(false)
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(state) = weak.upgrade() {
                state.delete_recording(&rec);
            }
        });
        dialog.present(Some(&self.delete));
    }

    /// Remove `rec`'s file. The viewer empties and the list is read again at
    /// once (the directory watch would do it a moment later).
    fn delete_recording(self: &Rc<Self>, rec: &Recording) {
        if let Err(err) = fs::remove_file(&rec.path) {
            self.tell(&format!("Could not delete the transcript: {err}."));
            return;
        }
        let was_open = self
            .open
            .borrow()
            .as_ref()
            .is_some_and(|open| open.path == rec.path);
        if was_open {
            self.show_nothing();
        }
        self.relist();
    }
}

/// The file chooser of Export…: a copy named `name`, to be saved in the home
/// directory unless the person goes elsewhere. The chooser would otherwise start
/// where it last was, or in the folder of the file it was opened for, and the
/// default target of an export must never be the transcripts directory the
/// recording came from.
pub(crate) fn export_dialog(name: &str) -> gtk::FileDialog {
    let dialog = gtk::FileDialog::builder()
        .title("Export Transcript")
        .initial_name(name)
        .modal(true)
        .build();
    dialog.set_initial_folder(Some(&gio::File::for_path(glib::home_dir())));
    dialog
}

/// A day heading in the list: a row that cannot be selected or activated.
fn heading_row(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .xalign(0.0)
        .css_classes(["heading", "dim-label"])
        .margin_start(6)
        .margin_top(12)
        .margin_bottom(3)
        .build();
    gtk::ListBoxRow::builder()
        .name(HEADING_NAME)
        .child(&label)
        .selectable(false)
        .activatable(false)
        .focusable(false)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::time::Instant;

    use moniker_core::transcript::{transcript_path, write_transcript};
    use tempfile::TempDir;

    const TEXT: &str = "uptime\n 12:00 up 1 day\n";

    fn recording(dir: &TempDir) -> PathBuf {
        write_transcript(&transcript_path(dir.path(), "web", 1_791_288_000), TEXT)
            .expect("a recording")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn mkfifo(path: &Path) {
        let name = CString::new(path.as_os_str().as_bytes()).expect("no NUL");
        // SAFETY: `name` is a valid NUL-terminated path for the call's duration.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo");
    }

    #[test]
    fn export_writes_a_0600_copy() {
        let dir = TempDir::new().unwrap();
        let source = recording(&dir);
        let out = TempDir::new().unwrap();
        let target = out.path().join("copy.txt");
        export_to(&source, &target).expect("exported");
        assert_eq!(fs::read(&target).unwrap(), TEXT.as_bytes());
        assert_eq!(mode(&target), 0o600);

        // Over an existing, longer, world-readable file: replaced, and private.
        let existing = out.path().join("existing.txt");
        fs::write(&existing, "x".repeat(4 * TEXT.len())).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o644)).unwrap();
        export_to(&source, &existing).expect("exported over a file");
        assert_eq!(fs::read(&existing).unwrap(), TEXT.as_bytes());
        assert_eq!(mode(&existing), 0o600);
        assert_eq!(fs::read(&source).unwrap(), TEXT.as_bytes(), "untouched");
    }

    #[test]
    fn export_onto_the_original_is_refused() {
        let dir = TempDir::new().unwrap();
        let source = recording(&dir);
        let name = source.file_name().unwrap();
        let link = dir.path().join("link.txt");
        symlink(&source, &link).unwrap();
        let hard = dir.path().join("hard.txt");
        fs::hard_link(&source, &hard).unwrap();
        let roundabout = dir.path().join(".").join(name);
        for target in [&source, &link, &hard, &roundabout] {
            assert_eq!(
                export_to(&source, target),
                Err(SAME_FILE.to_owned()),
                "{target:?}"
            );
            assert_eq!(fs::read(&source).unwrap(), TEXT.as_bytes(), "{target:?}");
        }
        assert!(link.exists() && hard.exists(), "nothing removed");
    }

    #[test]
    fn opening_never_follows_a_link_or_blocks() {
        let dir = TempDir::new().unwrap();
        let source = recording(&dir);
        let link = dir.path().join("web_20261001-120001.txt");
        symlink(&source, &link).unwrap();
        let outside = dir.path().join("web_20261001-120004.txt");
        symlink("/etc/passwd", &outside).unwrap();
        let fifo = dir.path().join("web_20261001-120002.txt");
        mkfifo(&fifo);
        let sub = dir.path().join("web_20261001-120003.txt");
        fs::create_dir(&sub).unwrap();

        let started = Instant::now();
        for path in [&link, &outside, &fifo, &sub] {
            let err = read_recording(path).expect_err("refused");
            assert_eq!(
                err, "This transcript cannot be opened: it is not a regular file.",
                "{path:?}"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "nothing blocked"
        );
        assert_eq!(read_recording(&source).unwrap(), TEXT.as_bytes());
    }

    #[test]
    fn a_link_swapped_in_after_the_name_check_is_not_followed() {
        // `open_regular` checks the name, then opens with O_NOFOLLOW and checks
        // the opened file. Opening a link with those flags shows the second
        // half on its own: a link put in place between the two is refused.
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("swapped.txt");
        symlink("/etc/passwd", &link).unwrap();
        let err = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(&link)
            .expect_err("a link is not followed");
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
    }

    #[test]
    fn exporting_a_bad_source_or_to_a_fifo_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let source = recording(&dir);
        let fifo_source = dir.path().join("fifo.txt");
        mkfifo(&fifo_source);
        let out = TempDir::new().unwrap();
        let target = out.path().join("copy.txt");
        let err = export_to(&fifo_source, &target).expect_err("refused");
        assert!(err.contains("not a regular file"), "{err}");
        assert!(!target.exists(), "no target created");

        let fifo_target = out.path().join("fifo-target");
        mkfifo(&fifo_target);
        let started = Instant::now();
        assert!(export_to(&source, &fifo_target).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "nothing blocked"
        );
    }

    #[test]
    fn headings_name_today_and_yesterday_and_years_elsewhere() {
        let today = Date::from_calendar_date(2026, Month::October, 17).unwrap();
        assert_eq!(heading_text(Day::Today, today), "Today");
        assert_eq!(heading_text(Day::Yesterday, today), "Yesterday");
        let this_year = heading_text(Day::Date(2026, 3, 4), today);
        assert!(
            this_year.ends_with(" 4") && !this_year.contains("2026"),
            "{this_year}"
        );
        let last_year = heading_text(Day::Date(2025, 3, 4), today);
        assert!(last_year.ends_with(" 4, 2025"), "{last_year}");
    }

    #[test]
    fn the_row_time_and_its_day_come_from_one_conversion() {
        for secs in [0, 1_791_288_000, 1_800_000_000, -1, i64::MAX, i64::MIN] {
            let when = local_time(secs);
            let date = local_date(&when);
            assert_eq!(date.year(), when.year());
            assert_eq!(i32::from(u8::from(date.month())), when.month());
            assert_eq!(i32::from(date.day()), when.day_of_month());
            assert_eq!(clock(&when, false).len(), 5);
        }
    }

    #[test]
    fn match_counts_read_well() {
        assert_eq!(matches_text(1), "1 match");
        assert_eq!(matches_text(0), "0 matches");
        assert_eq!(matches_text(12), "12 matches");
    }
}
