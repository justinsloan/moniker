//! Scenarios: which sessions save a transcript, and where. "Save transcripts of
//! all sessions" (the setting), the "Save transcript" box under the "›" of a
//! profile and of Local terminal, the tab a window opens with, and the
//! transcripts folder of the settings.
//!
//! Every scenario writes its own settings file and transcripts folder in a
//! temporary directory it is given (`ctx_with` puts the settings file beside the
//! profile file); the real data directory is never used. Connections run a fake
//! `ssh`, local tabs run `/bin/sh`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::transcript::{default_dir, transcript_path};
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{choices_parts, ctx_with, press_key, test_app};
use super::windows::{list_in, main_window, named, row};
use super::{
    all_widgets, fake_ssh, files_in, find_widget, pump_for, pump_until, wait_for_session,
    wait_for_text,
};
use crate::context::AppContext;
use crate::terminal_tab::{local_host_name, set_test_ssh_program, terminal_of};
use crate::{connect_menu, new_tab, preferences, recordings, settings, window};

/// Put `transcripts` and `save_all` into the settings file of `ctx`.
fn configure(ctx: &AppContext, transcripts: &Path, save_all: bool) {
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(transcripts.to_path_buf());
        s.save_all_transcripts = save_all;
    })
    .expect("write the settings");
}

fn set_save_all(ctx: &AppContext, on: bool) {
    settings::update(&ctx.settings_path, |s| s.save_all_transcripts = on)
        .expect("write the settings");
}

/// A new tab in `main` that shows the new-tab list.
fn new_list(main: &window::MainWindow, ctx: &Rc<AppContext>) -> (adw::TabPage, gtk::ListBox) {
    let page = new_tab::open(&main.view, ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the new-tab list");
    (page, list)
}

/// A click (or Enter) on the row `name`.
fn click(list: &gtk::ListBox, name: &str) {
    list.emit_by_name::<()>("row-activated", &[&row(list, name)]);
}

/// Open the choices behind the "›" of the row `name` with the Right arrow, as
/// the keyboard does, and return the popover and the column of choices
/// (`column_name`). Looking at the choices opens nothing.
fn open_choices(list: &gtk::ListBox, name: &str, column_name: &str) -> (gtk::Popover, gtk::Widget) {
    let row = row(list, name);
    let more: gtk::MenuButton = find_widget(&row).expect("the › button");
    let popover = more.popover().expect("its popover");
    row.grab_focus();
    press_key(list, gdk::Key::Right);
    pump_until("Right to open the choices", || popover.is_visible());
    pump_for(Duration::from_millis(200));
    let column = named(&popover, column_name).expect("the choices");
    (popover, column)
}

/// What the Local terminal's choices show.
struct LocalParts {
    open: gtk::Button,
    open_label: String,
    record: gtk::CheckButton,
}

fn local_parts(widget: &gtk::Widget) -> LocalParts {
    let mut open = None;
    let mut record = None;
    let mut separators = 0;
    let mut child = widget.first_child();
    while let Some(w) = child {
        child = w.next_sibling();
        if let Some(button) = w.downcast_ref::<gtk::Button>() {
            assert!(open.replace(button.clone()).is_none(), "one button");
        } else if w.is::<gtk::Separator>() {
            separators += 1;
        } else if let Ok(check) = w.downcast::<gtk::CheckButton>() {
            assert!(record.replace(check).is_none(), "one check box");
        } else {
            panic!("unexpected widget in the local choices");
        }
    }
    assert_eq!(separators, 1, "a separator sets the check box apart");
    let open = open.expect("the Open button");
    let label = open.child().and_downcast::<gtk::Label>().expect("a label");
    assert!(!label.uses_markup());
    let record = record.expect("the check box");
    assert_eq!(record.label().as_deref(), Some(connect_menu::RECORD_LABEL));
    LocalParts {
        open,
        open_label: label.label().to_string(),
        record,
    }
}

/// Have `/bin/sh` in `terminal` print a line with `word` in it and wait for it.
/// Input sent before the shell has started is lost, so this waits for the prompt.
fn say(terminal: &Terminal, word: &str) {
    wait_for_text(terminal, "$");
    terminal.feed_child(format!("echo {word}-$((40+2))\n").as_bytes());
    wait_for_text(terminal, &format!("{word}-42"));
}

/// Close the tab `page`, as its close button does, and let the main loop catch up.
fn close(main: &window::MainWindow, page: &adw::TabPage) {
    main.view.close_page(page);
    pump_for(Duration::from_millis(300));
}

/// The transcripts in `dir` whose file names start with `prefix` and `_`.
fn files_of(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let start = format!("{prefix}_");
    files_in(dir)
        .into_iter()
        .filter(|f| {
            f.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&start))
        })
        .collect()
}

/// What a transcript of a local terminal is named after: this machine's name,
/// cleaned the way every host is (read off a name made for a known time).
fn local_prefix() -> String {
    let sample = transcript_path(Path::new("/"), &local_host_name(), 0);
    let name = sample.file_name().expect("a file name").to_string_lossy();
    let prefix = name
        .strip_suffix("_19700101-000000.txt")
        .expect("<host>_<date>-<time>.txt");
    assert!(!prefix.is_empty());
    prefix.to_owned()
}

/// `file` is a transcript saved with mode 0600 whose text holds `needle`.
fn assert_transcript(file: &Path, needle: &str) {
    let name = file.file_name().expect("a name").to_string_lossy();
    assert!(name.ends_with(".txt"), "{name}");
    assert_eq!(
        fs::metadata(file).expect("the file").permissions().mode() & 0o777,
        0o600,
        "{name}"
    );
    let body = fs::read_to_string(file).expect("the text");
    assert!(body.contains(needle), "{name}: {body:?}");
}

/// Connect to `id` with a plain click in a new tab of `main` and wait for the
/// fake `ssh`'s output; returns the tab.
fn connect_plainly(main: &window::MainWindow, ctx: &Rc<AppContext>, id: &str) -> adw::TabPage {
    let (page, list) = new_list(main, ctx);
    click(&list, id);
    let terminal = wait_for_session(&page);
    wait_for_text(&terminal, "FAKE-END");
    page
}

/// Local terminal has a "›" like a profile: its choices are "Open" and the
/// "Save transcript" box, unchecked unless "save all" is on. A plain click saves
/// nothing; checking the box and Opening saves the session when its tab closes,
/// in a mode 0600 file named after this machine, in the transcripts folder.
pub(super) fn a_local_tab_can_save_its_transcript(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, false);
    let app = test_app();
    let main = main_window(&app, &ctx);

    // A plain click saves nothing.
    let (page, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    let terminal = terminal_of(&page).expect("a local shell");
    say(&terminal, "plain");
    close(&main, &page);
    assert!(
        files_in(&transcripts).is_empty(),
        "a plain click saves nothing"
    );

    // The Right arrow on the row opens the choices; the box is unchecked.
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) =
        open_choices(&list, new_tab::LOCAL_ROW, connect_menu::LOCAL_CHOICES_NAME);
    assert!(
        terminal_of(&page).is_none(),
        "looking at the choices opens nothing"
    );
    let parts = local_parts(&column);
    assert_eq!(parts.open_label, "Open");
    assert!(!parts.record.is_active(), "nothing asks for a transcript");
    parts.record.set_active(true);
    parts.open.emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    let terminal = terminal_of(&page).expect("the Open button opens a local shell in that tab");
    say(&terminal, "saved");
    assert!(
        files_in(&transcripts).is_empty(),
        "nothing is written while it runs"
    );
    close(&main, &page);

    let files = files_in(&transcripts);
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().expect("a name").to_string_lossy();
    assert!(
        name.starts_with(&format!("{}_", local_prefix())) && name.ends_with(".txt"),
        "named after this machine: {name}"
    );
    assert_transcript(&files[0], "saved-42");
    assert_eq!(
        fs::metadata(&transcripts).unwrap().permissions().mode() & 0o777,
        0o700,
        "a folder Moniker makes is private"
    );

    // A shell that exits on its own closes its tab and saves what it showed.
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) =
        open_choices(&list, new_tab::LOCAL_ROW, connect_menu::LOCAL_CHOICES_NAME);
    local_parts(&column).record.set_active(true);
    local_parts(&column).open.emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    let terminal = terminal_of(&page).expect("a local shell");
    say(&terminal, "exited");
    terminal.feed_child(b"exit\n");
    pump_until("the tab to close", || main.view.n_pages() == 1);
    let files = files_in(&transcripts);
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(
        files
            .iter()
            .any(|f| fs::read_to_string(f).is_ok_and(|t| t.contains("exited-42")))
    );

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// With "Save transcripts of all sessions" on, a profile whose own setting is
/// off is saved all the same by a plain click, and its box under "›" starts
/// checked, even when the list was made before the setting changed. With it off
/// each profile follows its own setting and local tabs save nothing.
pub(super) fn save_all_overrides_a_profile_that_saves_nothing(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "save-all", "echo FAKE-END\nexec sleep 600");
    let transcripts = dir.join("transcripts");
    let mut quiet = Profile::new("quiet", "quiet.example.test", "quinn");
    quiet.transcript = false;
    let mut loud = Profile::new("loud", "loud.example.test", "lou");
    loud.transcript = true;
    let ctx = ctx_with(dir, &[quiet.clone(), loud.clone()]);
    configure(&ctx, &transcripts, false);
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    // Off: each profile follows its own setting.
    let page = connect_plainly(&main, &ctx, &quiet.id);
    close(&main, &page);
    assert!(
        files_in(&transcripts).is_empty(),
        "the profile saves nothing"
    );
    let page = connect_plainly(&main, &ctx, &loud.id);
    close(&main, &page);
    assert_eq!(files_of(&transcripts, "loud.example.test").len(), 1);
    assert_eq!(files_in(&transcripts).len(), 1);

    // The list is made while the setting is off; the setting is then turned on.
    let (page, list) = new_list(&main, &ctx);
    set_save_all(&ctx, true);
    let (popover, column) = open_choices(&list, &quiet.id, connect_menu::CHOICES_NAME);
    assert!(
        choices_parts(&column).record.is_active(),
        "the box starts checked, whatever the profile says"
    );
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());
    assert!(
        terminal_of(&page).is_none(),
        "looking at the choices opens nothing"
    );

    // A plain click on the profile that saves nothing: saved.
    click(&list, &quiet.id);
    let terminal = wait_for_session(&page);
    wait_for_text(&terminal, "FAKE-END");
    close(&main, &page);
    let files = files_of(&transcripts, "quiet.example.test");
    assert_eq!(files.len(), 1, "{:?}", files_in(&transcripts));
    assert_transcript(&files[0], "FAKE-END");

    // Local terminal too.
    let (page, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    say(&terminal_of(&page).expect("a local shell"), "everywhere");
    close(&main, &page);
    let files = files_of(&transcripts, &local_prefix());
    assert_eq!(files.len(), 1, "{:?}", files_in(&transcripts));
    assert_transcript(&files[0], "everywhere-42");

    // Off again: the boxes and the clicks follow the profiles once more.
    set_save_all(&ctx, false);
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) = open_choices(&list, &quiet.id, connect_menu::CHOICES_NAME);
    assert!(!choices_parts(&column).record.is_active());
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());
    click(&list, &quiet.id);
    wait_for_text(&wait_for_session(&page), "FAKE-END");
    close(&main, &page);
    assert_eq!(files_in(&transcripts).len(), 3, "nothing more was saved");

    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Unchecking the box under "›" skips the transcript of that one session, for a
/// profile and for Local terminal; the next plain click saves again.
pub(super) fn unchecking_skips_one_session(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "uncheck", "echo FAKE-END\nexec sleep 600");
    let transcripts = dir.join("transcripts");
    let mut quiet = Profile::new("quiet", "quiet.example.test", "quinn");
    quiet.transcript = false;
    let ctx = ctx_with(dir, &[quiet.clone()]);
    configure(&ctx, &transcripts, true);
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    // Local terminal: checked because of "save all"; unchecked, nothing is saved.
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) =
        open_choices(&list, new_tab::LOCAL_ROW, connect_menu::LOCAL_CHOICES_NAME);
    let parts = local_parts(&column);
    assert!(parts.record.is_active(), "save all checks the box");
    parts.record.set_active(false);
    parts.open.emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    say(&terminal_of(&page).expect("a local shell"), "skipped");
    close(&main, &page);
    assert!(
        files_in(&transcripts).is_empty(),
        "this session was skipped"
    );

    // A profile: the same.
    let (page, list) = new_list(&main, &ctx);
    let (_popover, column) = open_choices(&list, &quiet.id, connect_menu::CHOICES_NAME);
    let parts = choices_parts(&column);
    assert!(parts.record.is_active(), "save all checks the box");
    parts.record.set_active(false);
    parts.buttons[0].emit_clicked();
    wait_for_text(&wait_for_session(&page), "FAKE-END");
    close(&main, &page);
    assert!(
        files_in(&transcripts).is_empty(),
        "this session was skipped"
    );

    // Only those sessions: the next plain clicks save again.
    let page = connect_plainly(&main, &ctx, &quiet.id);
    close(&main, &page);
    assert_eq!(files_of(&transcripts, "quiet.example.test").len(), 1);
    let (page, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    say(&terminal_of(&page).expect("a local shell"), "again");
    close(&main, &page);
    assert_eq!(files_of(&transcripts, &local_prefix()).len(), 1);

    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The tab a window opens with is saved too when "save all" is on: at launch
/// and in a window of ☰ → New Window. With it off, neither is.
pub(super) fn the_launch_tab_saves_when_save_all_is_on(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, true);
    let app = test_app();

    // The launch.
    super::launch_at_home(&app, &ctx);
    pump_for(Duration::from_millis(400));
    let launched = window::last_active(&app).expect("the launch window");
    let page = launched.view.selected_page().expect("its tab");
    say(&terminal_of(&page).expect("a local shell"), "launch");
    assert!(
        files_in(&transcripts).is_empty(),
        "nothing is written while it runs"
    );
    close(&launched, &page);
    let files = files_in(&transcripts);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].file_name().is_some_and(|n| {
        n.to_string_lossy()
            .starts_with(&format!("{}_", local_prefix()))
    }));
    assert_transcript(&files[0], "launch-42");

    // New Window; this one is written when the window closes.
    let second = window::new_window(&app, &ctx);
    pump_for(Duration::from_millis(400));
    let page = second.view.selected_page().expect("its tab");
    say(&terminal_of(&page).expect("a local shell"), "second");
    assert_eq!(files_in(&transcripts).len(), 1);
    second.window.close();
    pump_for(Duration::from_millis(300));
    let files = files_in(&transcripts);
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(
        files
            .iter()
            .any(|f| fs::read_to_string(f).is_ok_and(|t| t.contains("second-42")))
    );

    // Off: the first tab of a window saves nothing.
    set_save_all(&ctx, false);
    let third = window::new_window(&app, &ctx);
    pump_for(Duration::from_millis(400));
    let page = third.view.selected_page().expect("its tab");
    say(&terminal_of(&page).expect("a local shell"), "third");
    close(&third, &page);
    assert_eq!(files_in(&transcripts).len(), 2, "nothing more was saved");
    pump_for(Duration::from_millis(200));
}

/// The folder in the settings is where new transcripts go (not the default one)
/// and what the Transcripts window lists. When the person chooses another folder
/// in the Preferences, a session still open saves into the new folder too, with
/// the transcripts moved there, so every transcript ends up where the person
/// asked; the next session takes the new folder at once.
///
/// (Why not the folder the session started with: the move lists the old folder
/// once, so a transcript written there during or after the move would be left
/// behind, or copied while still being written. `terminal_tab` points every
/// open session at the new folder before the move starts.)
pub(super) fn transcripts_go_to_the_chosen_folder(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "folder", "echo FAKE-END\nexec sleep 600");
    let chosen = dir.join("chosen");
    let later = dir.join("later");
    let mut loud = Profile::new("loud", "loud.example.test", "lou");
    loud.transcript = true;
    let ctx = ctx_with(dir, &[loud.clone()]);
    configure(&ctx, &chosen, false);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    // A connection and a local terminal: both land in the chosen folder.
    let page = connect_plainly(&main, &ctx, &loud.id);
    close(&main, &page);
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) =
        open_choices(&list, new_tab::LOCAL_ROW, connect_menu::LOCAL_CHOICES_NAME);
    let parts = local_parts(&column);
    parts.record.set_active(true);
    parts.open.emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    say(&terminal_of(&page).expect("a local shell"), "chosen");
    close(&main, &page);
    assert_eq!(files_in(&chosen).len(), 2, "{:?}", files_in(&chosen));
    assert!(
        files_in(&default_dir()).is_empty(),
        "nothing in the default folder"
    );

    // The Transcripts window lists that folder.
    app.activate_action("recordings", None);
    pump_for(Duration::from_millis(400));
    let listed = listed_in_transcripts_window(&app);
    let expected: Vec<String> = files_in(&chosen)
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(listed, expected);

    // A session that started before the folder changed saves into the new
    // folder, where the earlier transcripts have moved; the next session uses
    // the new folder at once.
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) =
        open_choices(&list, new_tab::LOCAL_ROW, connect_menu::LOCAL_CHOICES_NAME);
    let parts = local_parts(&column);
    parts.record.set_active(true);
    parts.open.emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    say(&terminal_of(&page).expect("a local shell"), "before");
    let dialog = preferences::present(&main.window, &app, &ctx);
    pump_for(Duration::from_millis(200));
    preferences::change_folder(&dialog, &app, &ctx, later.clone());
    pump_until("the move to finish", || {
        ctx.notice.last_text().as_deref() == Some("Moved 2 transcripts.")
    });
    dialog.force_close();
    close(&main, &page);
    assert!(
        files_in(&chosen).is_empty(),
        "nothing left in the old folder: {:?}",
        files_in(&chosen)
    );
    assert_eq!(files_in(&later).len(), 3, "{:?}", files_in(&later));
    assert!(
        files_in(&later)
            .iter()
            .any(|f| fs::read_to_string(f).is_ok_and(|t| t.contains("before-42"))),
        "the open session saved into the new folder"
    );
    let page = connect_plainly(&main, &ctx, &loud.id);
    close(&main, &page);
    assert_eq!(files_of(&later, "loud.example.test").len(), 2);
    assert_eq!(files_in(&later).len(), 4);
    assert!(files_in(&chosen).is_empty());
    pump_until("the Transcripts window to list the new folder", || {
        let mut names: Vec<String> = files_in(&later)
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        listed_in_transcripts_window(&app) == names
    });

    set_test_ssh_program(None);
    for window in app.windows() {
        window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// The file names the open Transcripts window lists, sorted.
pub(super) fn listed_in_transcripts_window(app: &adw::Application) -> Vec<String> {
    let list = app
        .windows()
        .iter()
        .flat_map(all_widgets::<gtk::ListBox>)
        .find(|l| l.widget_name() == recordings::LIST_NAME)
        .expect("the Transcripts window");
    let mut names = Vec::new();
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if row.downcast_ref::<adw::ActionRow>().is_some() {
            names.push(row.widget_name().to_string());
        }
        index += 1;
    }
    names.sort();
    names
}
