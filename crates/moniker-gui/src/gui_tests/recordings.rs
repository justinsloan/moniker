//! Scenarios: the Transcripts window ([`crate::recordings`]). Every one
//! lists a temporary directory of fixture transcripts that it passes in; the
//! real transcripts directory is never read.
//!
//! Exporting is covered without a file chooser by the unit tests of
//! `recordings::export_to`, which the chooser's callback calls, and by a look at
//! the chooser's settings (`recordings::export_dialog`).

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::transcript::{transcript_path, write_transcript};

use super::main_window::{ctx_with, test_app};
use super::{
    all_widgets, button_labeled, find_widget, labels_in, literal_labels, pump_for, pump_until,
};
use crate::notify::NotificationArea;
use crate::recordings::{
    self, CHUNK, COUNTER_NAME, ERROR_NAME, FIND_ACTION, FIND_NAME, HEADING_NAME, HOST_FILTER_NAME,
    LIST_NAME, ORDER_NAME, SEARCH_NAME, SECRETS_WARNING, TITLE_NAME, VIEW_NAME,
};
use crate::window;

/// 2026-10-01 12:00:00 UTC.
const OCT_1: i64 = 1_791_288_000;
const DAY: i64 = 86_400;

/// A transcript of a session with `host` that started at `secs`, holding `text`.
fn record(dir: &Path, host: &str, secs: i64, text: &str) -> PathBuf {
    write_transcript(&transcript_path(dir, host, secs), text).expect("write a fixture")
}

/// A file called `name` in `dir` holding `bytes` (any bytes, unlike
/// [`write_transcript`]), mode 0600 as transcripts are.
fn raw_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, bytes))
        .expect("write a raw fixture");
    path
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned()
}

fn now() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    i64::try_from(secs).expect("a sane clock")
}

/// The Transcripts window over `dir`, presented, on a fresh application.
fn open(dir: &Path) -> (adw::Application, adw::Window) {
    fs::create_dir_all(dir).expect("dir");
    let app = test_app();
    let window = recordings::present(&app, dir.to_path_buf());
    pump_for(Duration::from_millis(300));
    (app, window)
}

fn close(window: &adw::Window) {
    window.close();
    pump_for(Duration::from_millis(200));
}

/// The widget of type `T` named `name` in `window`.
fn named<T: IsA<gtk::Widget>>(window: &adw::Window, name: &str) -> T {
    all_widgets::<gtk::Widget>(window)
        .into_iter()
        .find(|w| w.widget_name() == name)
        .unwrap_or_else(|| panic!("no widget named {name}"))
        .downcast()
        .unwrap_or_else(|_| panic!("{name} is not the expected type"))
}

/// The recording rows of the list, top to bottom (day headings left out).
fn rows(window: &adw::Window) -> Vec<adw::ActionRow> {
    let list: gtk::ListBox = named(window, LIST_NAME);
    let mut rows = Vec::new();
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if let Ok(row) = row.downcast::<adw::ActionRow>() {
            rows.push(row);
        }
        index += 1;
    }
    rows
}

/// The rows' widget names: each recording's file name.
fn row_names(window: &adw::Window) -> Vec<String> {
    rows(window)
        .iter()
        .map(|r| r.widget_name().to_string())
        .collect()
}

/// The day headings of the list, top to bottom.
fn headings(window: &adw::Window) -> Vec<String> {
    let list: gtk::ListBox = named(window, LIST_NAME);
    let mut found = Vec::new();
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if row.widget_name() == HEADING_NAME {
            let label = labels_in(&row).pop().expect("a heading label");
            assert!(!label.uses_markup(), "a heading parses markup");
            found.push(label.label().to_string());
        }
        index += 1;
    }
    found
}

/// Open the recording whose file is `path`, as a click on its row does.
fn open_row(window: &adw::Window, path: &Path) {
    let name = file_name(path);
    let row = rows(window)
        .into_iter()
        .find(|r| r.widget_name() == name.as_str())
        .unwrap_or_else(|| panic!("no row for {name}"));
    let list: gtk::ListBox = named(window, LIST_NAME);
    list.emit_by_name::<()>("row-activated", &[&row]);
}

/// What the viewer shows.
fn viewer_text(window: &adw::Window) -> String {
    let view: gtk::TextView = named(window, VIEW_NAME);
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string()
}

fn counter(window: &adw::Window) -> String {
    named::<gtk::Label>(window, COUNTER_NAME)
        .label()
        .to_string()
}

fn strings(drop: &gtk::DropDown) -> Vec<String> {
    let model = drop
        .model()
        .and_downcast::<gtk::StringList>()
        .expect("a string list");
    (0..model.n_items())
        .map(|i| model.string(i).expect("an item").to_string())
        .collect()
}

fn select_string(drop: &gtk::DropDown, text: &str) {
    let index = strings(drop)
        .iter()
        .position(|s| s == text)
        .unwrap_or_else(|| panic!("no {text:?} in the drop-down"));
    drop.set_selected(u32::try_from(index).expect("a small index"));
    pump_for(Duration::from_millis(100));
}

fn button_with_tooltip(window: &adw::Window, tooltip: &str) -> gtk::Button {
    all_widgets::<gtk::Button>(window)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some(tooltip))
        .unwrap_or_else(|| panic!("no {tooltip:?} button"))
}

/// Newest first by default, oldest first on request; grouped under day
/// headings (today's under "Today"); the secrets warning is shown; and a
/// second `present` brings back the same window.
pub(super) fn lists_newest_first_and_can_flip(dir: &Path) {
    let old = record(dir, "web", OCT_1, "one");
    let mid = record(dir, "db", OCT_1 + 4 * DAY, "two");
    let late = record(dir, "web", OCT_1 + 8 * DAY, "three");
    let today = record(dir, "app", now(), "four");
    let (app, window) = open(dir);

    let newest: Vec<String> = [&today, &late, &mid, &old]
        .iter()
        .map(|p| file_name(p))
        .collect();
    assert_eq!(row_names(&window), newest);
    let shown = headings(&window);
    assert_eq!(shown.first().map(String::as_str), Some("Today"));
    assert_eq!(shown.len(), 4, "four recordings days apart: {shown:?}");
    for row in rows(&window) {
        let subtitle = row.subtitle().expect("the time");
        assert!(
            subtitle.len() == 5 && subtitle.as_bytes()[2] == b':',
            "a local time HH:MM, not {subtitle:?}"
        );
    }
    let warning = labels_in(&window)
        .into_iter()
        .find(|l| l.label() == SECRETS_WARNING)
        .expect("the secrets warning");
    assert!(warning.is_visible());

    let order: gtk::DropDown = named(&window, ORDER_NAME);
    assert_eq!(strings(&order), ["Newest first", "Oldest first"]);
    select_string(&order, "Oldest first");
    let oldest: Vec<String> = newest.iter().rev().cloned().collect();
    assert_eq!(row_names(&window), oldest);
    assert_eq!(headings(&window).last().map(String::as_str), Some("Today"));
    select_string(&order, "Newest first");
    assert_eq!(row_names(&window), newest);

    let again = recordings::present(&app, dir.to_path_buf());
    assert_eq!(again, window, "one Transcripts window per app");
    close(&window);
    assert!(app.windows().is_empty());
}

/// The host filter offers "All hosts" and each host that has recordings;
/// choosing one lists only its recordings.
pub(super) fn host_filter_lists_only_that_host(dir: &Path) {
    let web1 = record(dir, "web", OCT_1, "a");
    let db = record(dir, "db", OCT_1 + DAY, "b");
    let web2 = record(dir, "web", OCT_1 + 2 * DAY, "c");
    let (_app, window) = open(dir);

    let hosts: gtk::DropDown = named(&window, HOST_FILTER_NAME);
    assert_eq!(strings(&hosts), ["All hosts", "db", "web"]);
    assert_eq!(hosts.selected(), 0);
    select_string(&hosts, "web");
    assert_eq!(row_names(&window), [file_name(&web2), file_name(&web1)]);
    select_string(&hosts, "db");
    assert_eq!(row_names(&window), [file_name(&db)]);
    select_string(&hosts, "All hosts");
    assert_eq!(row_names(&window).len(), 3);
    close(&window);
}

/// Searching narrows the list to recordings holding the text, each with its
/// match count; the host filter still applies; an empty search lists all.
pub(super) fn search_across_all_narrows_and_counts(dir: &Path) {
    let two = record(dir, "web", OCT_1, "apt update\nApt upgrade\n");
    let one = record(dir, "db", OCT_1 + DAY, "APT once\n");
    let none = record(dir, "web", OCT_1 + 2 * DAY, "nothing here\n");
    let (_app, window) = open(dir);
    assert_eq!(rows(&window).len(), 3);

    let search: gtk::SearchEntry = named(&window, SEARCH_NAME);
    search.set_text("apt");
    pump_until("the search to narrow the list", || rows(&window).len() == 2);
    assert_eq!(row_names(&window), [file_name(&one), file_name(&two)]);
    let subtitles: Vec<String> = rows(&window)
        .iter()
        .map(|r| r.subtitle().expect("a subtitle").to_string())
        .collect();
    assert!(subtitles[0].ends_with(" · 1 match"), "{subtitles:?}");
    assert!(subtitles[1].ends_with(" · 2 matches"), "{subtitles:?}");

    let hosts: gtk::DropDown = named(&window, HOST_FILTER_NAME);
    select_string(&hosts, "web");
    assert_eq!(row_names(&window), [file_name(&two)]);
    select_string(&hosts, "All hosts");

    search.set_text("");
    pump_until("an empty search to list everything", || {
        rows(&window).len() == 3
    });
    assert_eq!(
        row_names(&window),
        [file_name(&none), file_name(&one), file_name(&two)]
    );
    for row in rows(&window) {
        let subtitle = row.subtitle().expect("a subtitle");
        assert!(!subtitle.contains("match"), "{subtitle:?}");
    }
    close(&window);
}

/// Opening a recording while the list is searched carries the search into the
/// viewer and jumps to the first match; ↓, ↑ and Enter move through the
/// matches, wrapping around; Ctrl+F's action focuses the viewer's search.
pub(super) fn opening_with_a_search_jumps_to_the_first_match(dir: &Path) {
    let text = "x apt\napt y\nAPT z\n";
    let path = record(dir, "web", OCT_1, text);
    record(dir, "db", OCT_1 + DAY, "nothing\n");
    let (_app, window) = open(dir);

    let search: gtk::SearchEntry = named(&window, SEARCH_NAME);
    search.set_text("apt");
    pump_until("the search to narrow the list", || rows(&window).len() == 1);
    open_row(&window, &path);
    pump_until("the first match", || counter(&window) == "1 of 3");
    assert_eq!(viewer_text(&window), text);
    let find: gtk::SearchEntry = named(&window, FIND_NAME);
    assert_eq!(find.text(), "apt");

    let view: gtk::TextView = named(&window, VIEW_NAME);
    let buffer = view.buffer();
    let current = buffer
        .tag_table()
        .lookup("current-match")
        .expect("the current-match tag");
    let all = buffer.tag_table().lookup("match").expect("the match tag");
    let has = |tag: &gtk::TextTag, offset: i32| buffer.iter_at_offset(offset).has_tag(tag);
    assert!(
        has(&current, 2) && has(&current, 4),
        "the first match is current"
    );
    assert!(!has(&current, 6), "only the first match is current");
    assert!(has(&all, 6) && has(&all, 12) && !has(&all, 0));

    let next = button_with_tooltip(&window, "Next Match");
    let previous = button_with_tooltip(&window, "Previous Match");
    next.emit_clicked();
    assert_eq!(counter(&window), "2 of 3");
    assert!(has(&current, 6) && !has(&current, 2));
    previous.emit_clicked();
    assert_eq!(counter(&window), "1 of 3");
    previous.emit_clicked();
    assert_eq!(counter(&window), "3 of 3", "↑ wraps to the last match");
    super::press_enter_in(&find);
    assert_eq!(counter(&window), "1 of 3", "Enter is next, and wraps");
    // The search entry's own delayed signal for the text set on opening must
    // not reset the position.
    next.emit_clicked();
    pump_for(Duration::from_millis(500));
    assert_eq!(counter(&window), "2 of 3");

    find.set_text("zzz");
    pump_until("no matches", || counter(&window) == "No matches");
    assert!(!next.is_sensitive() && !previous.is_sensitive());

    search.grab_focus();
    WidgetExt::activate_action(&window, FIND_ACTION, None).expect("the find action");
    pump_for(Duration::from_millis(100));
    let focus = gtk::prelude::RootExt::focus(&window).expect("a focused widget");
    assert!(
        focus == *find.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&find),
        "Ctrl+F focuses the viewer's search"
    );
    close(&window);
}

/// Odd things in the directory: hosts with markup, control and bidi
/// characters, non-UTF-8 and NUL bytes in the text, a link and a subdirectory,
/// a file that cannot be read, and files that are not transcripts. Links,
/// non-files and files whose names are not transcript names (a dotfile, a note,
/// a name with a date that does not exist, a non-UTF-8 name) are not listed;
/// everything else is shown as it is, never as markup; nothing panics; a file
/// that cannot be opened shows a plain error.
pub(super) fn odd_recordings_show_literally(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    fs::create_dir_all(dir).expect("dir");
    let markup_host = "h<b>x";
    let bytes = b"\xff\xfe <b>bold</b> &amp; \x00 tail\n";
    let markup = raw_file(dir, "h<b>x_20261017-000000.txt", bytes);
    let control_host = "c\u{7}t\u{202e}l";
    let control = raw_file(dir, "c\u{7}t\u{202e}l_20261017-000001.txt", b"plain\n");
    let entity_host = "<i>odd&lt; & name";
    let entity = raw_file(dir, "<i>odd&lt; & name_20261017-000004.txt", b"odd\n");
    symlink("/etc/passwd", dir.join("link_20261017-000002.txt")).expect("a link");
    fs::create_dir(dir.join("sub_20261017-000003.txt")).expect("a subdirectory");
    let locked = record(dir, "locked", OCT_1, "secret\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    // The person's own files, in a folder they also use for other things.
    for name in [
        ".bashrc",
        "<i>odd&lt; & name.txt",
        "notes.txt",
        "x_20261317-000000.txt",
    ] {
        raw_file(dir, name, b"not a transcript\n");
    }
    let non_utf8 = dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9_20261017-000005.txt"));
    fs::write(&non_utf8, "not a transcript\n").expect("a non-UTF-8 name");
    let (_app, window) = open(dir);

    let mut names = row_names(&window);
    names.sort();
    let mut expected: Vec<String> = [&markup, &control, &entity, &locked]
        .iter()
        .map(|p| file_name(p))
        .collect();
    expected.sort();
    assert_eq!(
        names, expected,
        "links, directories and files that are not transcripts are not listed"
    );
    let hosts: gtk::DropDown = named(&window, HOST_FILTER_NAME);
    assert!(
        !strings(&hosts)
            .iter()
            .any(|h| h.contains("notes") || h.contains("caf")),
        "nor offered as hosts: {:?}",
        strings(&hosts)
    );
    for host in [markup_host, control_host, entity_host] {
        let row = rows(&window)
            .into_iter()
            .find(|r| r.title() == host)
            .unwrap_or_else(|| panic!("no row titled {host:?}"));
        assert!(!row.uses_markup(), "{host:?}: the row parses markup");
        literal_labels("recording row", &row, host);
    }
    select_string(&hosts, markup_host);
    literal_labels("host filter", &hosts, markup_host);
    assert_eq!(rows(&window).len(), 1);
    select_string(&hosts, "All hosts");

    open_row(&window, &markup);
    let shown = crate::recordings::search::display_text(bytes);
    pump_until("the text", || viewer_text(&window) == shown);
    assert!(shown.contains("<b>bold</b> &amp;"));
    let title: adw::WindowTitle = named(&window, TITLE_NAME);
    assert_eq!(title.title(), markup_host);
    literal_labels("viewer title", &title, markup_host);

    // The delete confirmation names it as it is. Cancel keeps the file.
    button_labeled(&window, "Delete")
        .expect("the Delete button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("the delete confirmation");
    assert_eq!(dialog.heading().as_deref(), Some("Delete this transcript?"));
    assert!(!dialog.is_heading_use_markup() && !dialog.is_body_use_markup());
    let body = dialog.body();
    assert!(body.contains(markup_host), "{body:?}");
    literal_labels("delete dialog", &dialog, &body);
    button_labeled(&dialog, "Cancel")
        .expect("Cancel")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    assert!(markup.exists());

    open_row(&window, &control);
    pump_until("the text", || viewer_text(&window) == "plain\n");
    literal_labels("viewer title", &title, control_host);

    // A file that cannot be read: a plain error, an empty viewer.
    open_row(&window, &locked);
    let error: gtk::Label = named(&window, ERROR_NAME);
    pump_until("the error", || {
        error.is_mapped() && !error.label().is_empty()
    });
    assert!(!error.uses_markup());
    assert_eq!(viewer_text(&window), "");
    assert!(!error.label().contains("secret"));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).expect("chmod back");
    close(&window);
}

/// Delete asks, then removes the file; the viewer empties and the row goes.
pub(super) fn deleting_the_open_recording_clears_the_viewer(dir: &Path) {
    let keep = record(dir, "db", OCT_1, "keep\n");
    let gone = record(dir, "web", OCT_1 + DAY, "delete me\n");
    let (_app, window) = open(dir);
    open_row(&window, &gone);
    pump_until("the text", || viewer_text(&window) == "delete me\n");

    button_labeled(&window, "Delete")
        .expect("the Delete button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("the delete confirmation");
    button_labeled(&dialog, "Delete")
        .expect("the dialog's Delete")
        .emit_clicked();
    pump_for(Duration::from_millis(300));

    assert!(!gone.exists(), "the file is removed");
    assert!(keep.exists(), "only that one");
    assert_eq!(viewer_text(&window), "");
    assert_eq!(row_names(&window), [file_name(&keep)]);
    let title: adw::WindowTitle = named(&window, TITLE_NAME);
    assert_ne!(title.title(), "web");
    assert!(
        !button_labeled(&window, "Delete").is_some_and(|b| b.is_visible()),
        "nothing open, nothing to delete"
    );
    close(&window);
}

/// A delete that fails (the folder cannot be written to) is told in the
/// Transcripts window's own notification area, which waits for the confirmation
/// dialog to go; the file stays.
pub(super) fn a_failed_delete_is_told_in_the_transcripts_window(dir: &Path) {
    if rustix::process::geteuid().is_root() {
        return; // root can delete from a read-only folder
    }
    let kept = record(dir, "web", OCT_1, "keep me\n");
    let (_app, window) = open(dir);
    open_row(&window, &kept);
    pump_until("the text", || viewer_text(&window) == "keep me\n");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("make the folder read-only");

    button_labeled(&window, "Delete")
        .expect("the Delete button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("the delete confirmation");
    button_labeled(&dialog, "Delete")
        .expect("the dialog's Delete")
        .emit_clicked();
    let area: NotificationArea = find_widget(&window).expect("the window's notification area");
    pump_until("the failure to be told", || area.shown_text().is_some());
    let said = area.shown_text().expect("a message");
    assert!(
        said.starts_with("Could not delete the transcript: "),
        "{said}"
    );
    literal_labels("failed delete", &window, &said);
    assert!(kept.exists(), "the file stays");

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("make it writable again");
    close(&window);
}

/// A recording larger than one chunk of the viewer streams in whole, and a
/// search matching everywhere in it is counted and highlighted throughout.
pub(super) fn a_large_recording_loads_whole(dir: &Path) {
    let lines = 100_000;
    let text: String = (0..lines).map(|i| format!("line {i} apt\n")).collect();
    assert!(text.len() > 4 * 256 * 1024, "several chunks");
    let path = record(dir, "big", OCT_1, &text);
    let (_app, window) = open(dir);
    open_row(&window, &path);
    pump_until("the whole text", || {
        viewer_text(&window).len() == text.len()
    });
    assert_eq!(viewer_text(&window), text);

    let find: gtk::SearchEntry = named(&window, FIND_NAME);
    find.set_text("APT");
    pump_until("the matches", || {
        counter(&window) == format!("1 of {lines}")
    });
    let view: gtk::TextView = named(&window, VIEW_NAME);
    let buffer = view.buffer();
    let all = buffer.tag_table().lookup("match").expect("the match tag");
    let last = i32::try_from(text.chars().count()).expect("fits") - 2;
    pump_until("the last match highlighted", || {
        buffer.iter_at_offset(last).has_tag(&all)
    });
    close(&window);
}

/// The text of a recording that is one long line, as the viewer shows it:
/// a newline before every 2,000th character. Written out here, not taken from
/// `search::for_display`, so the viewer is checked against the rule itself.
fn broken_every_2000(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let pieces: Vec<String> = chars.chunks(2000).map(|c| c.iter().collect()).collect();
    pieces.join("\n")
}

/// A transcript that is one line of 5 MB (a program that never printed a
/// newline) opens in about a second, shown as display lines of at most 2,000
/// characters with every character still there once, and a word at its very
/// end is found: "1 of 1". Before the long line was broken for display this
/// took 17 s here (about 6 s on a release build on a desktop).
pub(super) fn a_huge_one_line_transcript_loads_promptly(dir: &Path) {
    let word = "needle";
    let mut text = "abcdefghi jklmnopqr stuvwxyz ".repeat(5_000_000 / 29 + 1);
    text.push_str(word);
    assert!(text.len() >= 5_000_000 && !text.contains('\n'));
    fs::create_dir_all(dir).expect("dir");
    let path = raw_file(dir, "huge_20261017-000000.txt", text.as_bytes());
    let (_app, window) = open(dir);

    let view: gtk::TextView = named(&window, VIEW_NAME);
    let buffer = view.buffer();
    let chars = i32::try_from(text.chars().count()).expect("fits");
    let lines = (chars - 1) / 2000 + 1;
    let started = std::time::Instant::now();
    open_row(&window, &path);
    pump_until("the whole text", || buffer.char_count() >= chars);
    let loaded = started.elapsed();
    let find: gtk::SearchEntry = named(&window, FIND_NAME);
    find.set_text(word);
    pump_until("the word at the end", || counter(&window) == "1 of 1");
    let found = started.elapsed();
    eprintln!("5 MB on one line: shown in {loaded:?}, word at the end found in {found:?}");
    assert!(
        found < Duration::from_secs(10),
        "a 5 MB line took {found:?} to open and search"
    );

    assert_eq!(buffer.line_count(), lines, "display lines of 2,000");
    assert_eq!(
        buffer.char_count(),
        chars + lines - 1,
        "one newline per break"
    );
    let shown = viewer_text(&window);
    assert_eq!(
        shown.replace('\n', ""),
        text,
        "no character lost or doubled"
    );
    assert!(shown.split('\n').all(|l| l.chars().count() <= 2000));
    // The match is highlighted where it is now, one break further on per 2,000.
    let all = buffer.tag_table().lookup("match").expect("the match tag");
    let at = chars - 6 + (chars - 6) / 2000;
    assert!(buffer.iter_at_offset(at).has_tag(&all));
    assert!(!buffer.iter_at_offset(at - 1).has_tag(&all));
    close(&window);
}

/// A display break is neither lost nor doubled where the viewer's filling
/// changes chunk: the text shown is the rule's (a newline before every 2,000th
/// character), with a break as the last byte of a chunk and, in another
/// recording, as the first byte of the next.
pub(super) fn display_breaks_survive_chunk_boundaries(dir: &Path) {
    // The viewer fills `CHUNK` bytes at a time. After a prefix of `p`
    // characters and a newline, the k-th break is the character p + 2001 k of
    // the shown text (all ASCII, so bytes are characters): with k the number of
    // whole display lines in a chunk, `CHUNK - 1 - 2001 k` puts that break on
    // the last byte of the first chunk and one more on the first byte of the
    // second.
    let lines = CHUNK / 2001;
    let prefixes = [CHUNK - 1 - 2001 * lines, CHUNK - 2001 * lines];
    let long: String = (0..600_000)
        .map(|i| char::from(b'a' + u8::try_from(i % 26).expect("small")))
        .collect();
    fs::create_dir_all(dir).expect("dir");
    let mut expected = Vec::new();
    let mut paths = Vec::new();
    for (n, prefix) in prefixes.into_iter().enumerate() {
        let text = format!("{}\n{long}", "p".repeat(prefix));
        let shown = format!("{}\n{}", "p".repeat(prefix), broken_every_2000(&long));
        assert_eq!(
            shown.as_bytes()[CHUNK - 1 + n],
            b'\n',
            "a break at the chunk boundary"
        );
        let name = format!("chunk{prefix}_20261017-0000{prefix}.txt");
        paths.push(raw_file(dir, &name, text.as_bytes()));
        expected.push(shown);
    }
    let (_app, window) = open(dir);
    for (path, shown) in paths.iter().zip(&expected) {
        open_row(&window, path);
        let chars = i32::try_from(shown.chars().count()).expect("fits");
        let view: gtk::TextView = named(&window, VIEW_NAME);
        pump_until("the whole text", || view.buffer().char_count() >= chars);
        pump_for(Duration::from_millis(100));
        assert_eq!(view.buffer().char_count(), chars);
        assert!(viewer_text(&window) == *shown, "{}", file_name(path));
    }
    close(&window);
}

/// A match across a display break is one match, highlighted across the
/// inserted newline, and the matches before and after it, with multi-byte
/// characters among the text, are highlighted where they are in the shown
/// text: the offsets are characters, not bytes.
pub(super) fn a_match_across_a_display_break_is_highlighted_whole(dir: &Path) {
    let word = "ñandú";
    let mut text = String::from("é😀 ");
    text.push_str(&"ß".repeat(1_995));
    // Starts at 1,998: its first two characters end the first display line.
    let straddling = text.chars().count();
    text.push_str("ÑANDÚ ");
    let whole = text.chars().count();
    text.push_str(word);
    text.push_str(&"é".repeat(4_000));
    let last = text.chars().count();
    text.push_str("ñANDú tail");
    assert_eq!(straddling, 1_998);
    fs::create_dir_all(dir).expect("dir");
    let path = raw_file(dir, "odd_20261017-000000.txt", text.as_bytes());
    let (_app, window) = open(dir);

    open_row(&window, &path);
    let shown = broken_every_2000(&text);
    pump_until("the text", || viewer_text(&window) == shown);
    let find: gtk::SearchEntry = named(&window, FIND_NAME);
    find.set_text(word);
    pump_until("three matches", || counter(&window) == "1 of 3");

    let view: gtk::TextView = named(&window, VIEW_NAME);
    let buffer = view.buffer();
    let lookup = |name: &str| buffer.tag_table().lookup(name).expect("a tag");
    let (all, current) = (lookup("match"), lookup("current-match"));
    let has = |tag: &gtk::TextTag, offset: usize| {
        buffer
            .iter_at_offset(i32::try_from(offset).expect("fits"))
            .has_tag(tag)
    };
    // Where a character of `text` is in the shown text: one more for each
    // multiple of 2,000 at or before it.
    let shown_at = |offset: usize| offset + offset / 2000;
    let is_highlighted = |start: usize, tag: &gtk::TextTag| {
        let first = shown_at(start);
        let covered: Vec<usize> = (first..shown_at(start + 5 - 1) + 1).collect();
        assert!(covered.iter().all(|&o| has(tag, o)), "{start}: {covered:?}");
        !has(tag, first - 1) && !has(tag, shown_at(start + 5 - 1) + 1)
    };

    assert!(is_highlighted(straddling, &all));
    assert!(has(&all, 2000), "the inserted newline is inside the match");
    assert!(
        is_highlighted(straddling, &current),
        "it is the current one"
    );
    assert!(is_highlighted(whole, &all) && !has(&current, shown_at(whole)));
    assert!(is_highlighted(last, &all) && !has(&current, shown_at(last)));

    let next = button_with_tooltip(&window, "Next Match");
    next.emit_clicked();
    assert_eq!(counter(&window), "2 of 3");
    assert!(is_highlighted(whole, &current) && !has(&current, shown_at(straddling)));
    next.emit_clicked();
    assert_eq!(counter(&window), "3 of 3");
    assert!(is_highlighted(last, &current) && !has(&current, shown_at(whole)));
    next.emit_clicked();
    assert_eq!(
        counter(&window),
        "1 of 3",
        "wraps round to the one across the break"
    );
    assert!(is_highlighted(straddling, &current));

    // The sidebar's search counts it once too.
    let search: gtk::SearchEntry = named(&window, SEARCH_NAME);
    search.set_text(word);
    pump_until("the count", || {
        rows(&window)
            .first()
            .and_then(|r| r.subtitle())
            .is_some_and(|s| s.ends_with(" · 3 matches"))
    });
    close(&window);
}

/// What the clipboard of `widget`'s display holds as text, read back through
/// the main loop as another application pasting it would.
fn clipboard_text(widget: &impl IsA<gtk::Widget>) -> Option<String> {
    let read: Rc<RefCell<Option<Option<String>>>> = Rc::default();
    let slot = Rc::clone(&read);
    widget
        .clipboard()
        .read_text_async(gtk::gio::Cancellable::NONE, move |text| {
            slot.replace(Some(text.ok().flatten().map(String::from)));
        });
    pump_until("the clipboard to be read", || read.borrow().is_some());
    read.take().flatten()
}

/// Copying a selection out of the viewer puts the transcript's own text on the
/// clipboard, not the view's: across a display break there is no newline that
/// is not in the file, with multi-byte characters on both sides of it. Ctrl+C
/// and Ctrl+Insert are the view's `copy-clipboard`, and so is the context
/// menu's Copy (its `clipboard.copy` action); Ctrl+X and Shift+Delete are its
/// `cut-clipboard`, which copies in a read-only view too. The view keeps its
/// text, and with nothing selected the clipboard is left alone.
pub(super) fn copying_across_a_display_break_copies_the_original(dir: &Path) {
    // The first line: 3 + 1,992 characters, then "ÑANDÚ ñandú" from the
    // 1,995th, then 2,500 more: 4,506 characters, broken after the 2,000th and
    // the 4,000th. Then a short line.
    let mut long = String::from("é😀 ");
    long.push_str(&"ß".repeat(1_992));
    long.push_str("ÑANDÚ ñandú");
    long.push_str(&"é".repeat(2_500));
    assert_eq!(long.chars().count(), 4_506);
    let text = format!("{long}\nlast line\n");
    fs::create_dir_all(dir).expect("dir");
    let path = raw_file(dir, "copy_20261017-000000.txt", text.as_bytes());
    let (_app, window) = open(dir);
    open_row(&window, &path);
    let shown = format!("{}\nlast line\n", broken_every_2000(&long));
    pump_until("the text", || viewer_text(&window) == shown);

    let view: gtk::TextView = named(&window, VIEW_NAME);
    let buffer = view.buffer();
    let chars: Vec<char> = text.chars().collect();
    let original = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
    // Where a character of the first line is shown: one further on per break
    // at or before it.
    let shown_at = |offset: usize| offset + offset / 2000;
    let at = |offset: usize| buffer.iter_at_offset(i32::try_from(offset).expect("fits"));
    let select = |from: usize, to: usize| buffer.select_range(&at(from), &at(to));
    let selected = || {
        let (start, end) = buffer.selection_bounds().expect("a selection");
        buffer.text(&start, &end, true).to_string()
    };
    let clipboard = view.clipboard();

    // Across the first break: the view shows a newline in the selection, the
    // clipboard gets the file's characters.
    select(shown_at(1_997), shown_at(2_003));
    assert_eq!(selected(), "NDÚ\n ña");
    clipboard.set_text("before");
    view.emit_copy_clipboard();
    assert_eq!(clipboard_text(&view).as_deref(), Some("NDÚ ña"));
    assert_eq!(original(1_997, 2_003), "NDÚ ña");

    // A selection that starts on the inserted newline and one that ends on it.
    select(2_000, shown_at(2_002));
    view.emit_copy_clipboard();
    assert_eq!(clipboard_text(&view).as_deref(), Some(" ñ"));
    select(shown_at(1_997), 2_000);
    view.emit_copy_clipboard();
    assert_eq!(clipboard_text(&view).as_deref(), Some("NDÚ"));

    // Cut, in a read-only view: across both breaks and into the next line, the
    // same as a copy, and nothing is taken out of the view.
    select(shown_at(10), shown_at(4_506) + 5);
    clipboard.set_text("before");
    view.emit_cut_clipboard();
    assert_eq!(clipboard_text(&view), Some(original(10, 4_511)));
    assert_eq!(
        viewer_text(&window),
        shown,
        "nothing is cut from a read-only view"
    );

    // Everything: the whole file, byte for byte.
    buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
    view.emit_copy_clipboard();
    assert_eq!(clipboard_text(&view).as_deref(), Some(text.as_str()));

    // The context menu's Copy.
    select(shown_at(1_997), shown_at(2_003));
    clipboard.set_text("before");
    pump_for(Duration::from_millis(50));
    WidgetExt::activate_action(&view, "clipboard.copy", None).expect("the view's Copy");
    assert_eq!(clipboard_text(&view).as_deref(), Some("NDÚ ña"));

    // Nothing selected: nothing copied.
    buffer.place_cursor(&buffer.start_iter());
    clipboard.set_text("before");
    view.emit_copy_clipboard();
    assert_eq!(clipboard_text(&view).as_deref(), Some("before"));
    close(&window);
}

/// Export copies the file as it is, not what the viewer shows: a one-line
/// transcript of 10,000 characters, shown in five display lines, exports as
/// that one line, byte for byte.
pub(super) fn export_copies_the_original_bytes(dir: &Path) {
    let text: String = (0..10_000)
        .map(|i| if i % 7 == 0 { 'é' } else { 'x' })
        .collect();
    fs::create_dir_all(dir).expect("dir");
    let path = raw_file(dir, "one_20261017-000000.txt", text.as_bytes());
    let (_app, window) = open(dir);
    open_row(&window, &path);
    let view: gtk::TextView = named(&window, VIEW_NAME);
    pump_until("the five display lines", || view.buffer().line_count() == 5);
    assert_ne!(viewer_text(&window), text, "the viewer shows breaks");

    let out = dir.with_file_name("r-export-out");
    fs::create_dir_all(&out).expect("the folder to export to");
    let copy = out.join("copy.txt");
    recordings::export_to(&path, &copy).expect("exported");
    assert_eq!(fs::read(&copy).expect("the copy"), text.as_bytes());
    assert_eq!(fs::read(&path).expect("the original"), text.as_bytes());
    assert!(!fs::read_to_string(&copy).expect("text").contains("\n"));
    close(&window);
}

/// A recording written while the window is open appears without reopening it.
pub(super) fn a_new_recording_appears_live(dir: &Path) {
    let first = record(dir, "web", OCT_1, "one\n");
    let (_app, window) = open(dir);
    assert_eq!(row_names(&window), [file_name(&first)]);
    let second = record(dir, "db", OCT_1 + DAY, "two\n");
    pump_until("the new recording to be listed", || {
        rows(&window).len() == 2
    });
    assert_eq!(row_names(&window), [file_name(&second), file_name(&first)]);
    let hosts: gtk::DropDown = named(&window, HOST_FILTER_NAME);
    assert_eq!(strings(&hosts), ["All hosts", "db", "web"]);
    close(&window);
}

/// Export… offers the home directory to save in, and the recording's own file
/// name: never the transcripts directory, where a copy would land among the
/// recordings.
pub(super) fn the_export_dialog_starts_in_the_home_directory() {
    let dialog = recordings::export_dialog("web-2026-10-01-120000.txt");
    let folder = dialog.initial_folder().expect("an initial folder");
    assert_eq!(folder.path(), Some(gtk::glib::home_dir()));
    assert_eq!(
        dialog.initial_name().as_deref(),
        Some("web-2026-10-01-120000.txt")
    );
    assert!(dialog.is_modal());
}

/// When the last main window closes, the Transcripts window goes too,
/// so the app quits.
pub(super) fn closing_the_last_main_window_closes_it(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    window::close_side_windows_with_last_main_window(&app);
    let main = window::build(&app, &ctx);
    main.window.present();
    let recordings_window = recordings::present(&app, dir.join("transcripts"));
    pump_for(Duration::from_millis(300));
    assert!(recordings_window.is_visible());

    main.window.close();
    pump_until("the Transcripts window to close", || {
        !recordings_window.is_visible()
    });
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty(), "nothing keeps the app running");
}
