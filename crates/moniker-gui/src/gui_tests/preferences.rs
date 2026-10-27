//! Scenarios: the Preferences dialog (☰ → Preferences, Ctrl+comma). Its four
//! settings (terminal font, scrollback, "Save transcripts of all sessions", the
//! transcripts folder) and its way to the sync settings.
//!
//! Every scenario has its own settings file and transcripts folders in the
//! temporary directory it is given (`ctx_with` puts the settings file beside the
//! profile file); the real data directory is never used. The font and the
//! scrollback are kept for the whole app (`terminal_tab`), so the scenarios that
//! change them put them back before they end.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, pango};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::transcript::transcript_path;
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{ctx_with, menu_items, test_app};
use super::save_transcripts::listed_in_transcripts_window;
use super::windows::{main_window, named};
use super::{
    MARKUP_NAME, all_widgets, assert_plain_row, button_labeled, find_widget, labels_in,
    literal_labels, pump_for, pump_until,
};
use crate::context::AppContext;
use crate::terminal_tab::{self, TranscriptSpec, open_local_shell, terminal_of};
use crate::{preferences, profiles, recordings, settings, window, zoom};

/// The Preferences dialog over `main`, as ☰ → Preferences opens it.
fn open(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    main: &window::MainWindow,
) -> adw::PreferencesDialog {
    let dialog = preferences::present(&main.window, app, ctx);
    pump_until("the Preferences dialog", || {
        main.window.visible_dialog().as_ref() == Some(dialog.upcast_ref())
    });
    pump_for(Duration::from_millis(200));
    dialog
}

fn close_dialog(main: &window::MainWindow, dialog: &adw::PreferencesDialog) {
    dialog.force_close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
}

/// The group of `dialog` titled `title`.
fn group(dialog: &adw::PreferencesDialog, title: &str) -> adw::PreferencesGroup {
    all_widgets::<adw::PreferencesGroup>(dialog)
        .into_iter()
        .find(|g| g.title() == title)
        .unwrap_or_else(|| panic!("no {title:?} group"))
}

/// The action row of `root` titled `title` (switch and spin rows are action
/// rows too, but they are looked for by their own types).
fn action_row(root: &impl IsA<gtk::Widget>, title: &str) -> adw::ActionRow {
    all_widgets::<adw::ActionRow>(root)
        .into_iter()
        .find(|r| r.title() == title)
        .unwrap_or_else(|| panic!("no {title:?} row"))
}

fn font_button(dialog: &adw::PreferencesDialog) -> gtk::FontDialogButton {
    find_widget(dialog).expect("the font button")
}

fn scrollback_row(dialog: &adw::PreferencesDialog) -> adw::SpinRow {
    find_widget(dialog).expect("the Scrollback row")
}

fn save_all_row(dialog: &adw::PreferencesDialog) -> adw::SwitchRow {
    find_widget(dialog).expect("the save-all switch")
}

/// Make the settings file of `ctx` unusable (not JSON), so that every change
/// to it is refused; returns what it holds, to compare later.
fn damage_settings(ctx: &AppContext) -> String {
    let damaged = "{ not json".to_owned();
    fs::write(&ctx.settings_path, &damaged).expect("damage the settings file");
    damaged
}

/// Wait for the notice `text` to have been shown.
fn wait_for_notice(ctx: &AppContext, text: &str) {
    pump_until(&format!("the notice {text:?}"), || {
        ctx.notice.last_text().as_deref() == Some(text)
    });
}

/// The last notice starts with `start`.
fn assert_notice_starts(ctx: &AppContext, start: &str) {
    let last = ctx.notice.last_text().unwrap_or_default();
    assert!(last.starts_with(start), "{last:?} starts with {start:?}");
}

/// The Preferences dialog has the groups and rows of the design: Terminal (Font,
/// a font button limited to monospace fonts; Scrollback, 1,000 to 1,000,000 in
/// steps of 1,000), Transcripts (with the warning; the save-all switch, off; the
/// folder, plain text, with Choose…) and Sync (Open). It is a dialog over the
/// main window, so the window's tab and terminal actions pause while it is open.
pub(super) fn preferences_has_the_four_settings_and_sync(dir: &Path) {
    let folder = dir.join("<b>transcripts</b> &amp;");
    let ctx = ctx_with(dir, &[]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(folder.clone())
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);
    assert_eq!(dialog.title(), "Preferences");
    assert!(
        main.window
            .lookup_action(terminal_tab::COPY_ACTION)
            .and_downcast::<gio::SimpleAction>()
            .is_some_and(|a| !a.is_enabled()),
        "the window's actions pause while the dialog is open"
    );

    // Terminal.
    let terminal = group(&dialog, "Terminal");
    let font = action_row(&terminal, "Font");
    assert_eq!(
        font.subtitle().as_deref(),
        Some("Used by every terminal; changes apply at once")
    );
    let button = font_button(&dialog);
    assert!(
        button.is_ancestor(&font),
        "the font button is on the Font row"
    );
    let filter = button
        .dialog()
        .and_then(|d| d.filter())
        .expect("the font chooser has a filter");
    let families = main.window.pango_context().list_families();
    let mono: Vec<&pango::FontFamily> = families.iter().filter(|f| f.is_monospace()).collect();
    let proportional: Vec<&pango::FontFamily> =
        families.iter().filter(|f| !f.is_monospace()).collect();
    assert!(!mono.is_empty(), "the test display has monospace fonts");
    for family in &mono {
        assert!(filter.match_(*family), "{} is offered", family.name());
        if let Some(face) = family.face(None) {
            assert!(
                filter.match_(&face),
                "{}'s faces are offered",
                family.name()
            );
        }
    }
    for family in &proportional {
        assert!(!filter.match_(*family), "{} is not offered", family.name());
        if let Some(face) = family.face(None) {
            assert!(!filter.match_(&face), "{}'s faces are not", family.name());
        }
    }
    let shown = button.font_desc().expect("a font is shown");
    assert!(
        shown
            .family()
            .is_some_and(|f| f.eq_ignore_ascii_case("monospace")),
        "no font setting shows the system monospace font: {shown}"
    );

    let scrollback = scrollback_row(&dialog);
    assert!(scrollback.is_ancestor(&terminal));
    assert_eq!(scrollback.title(), "Scrollback");
    assert_eq!(
        scrollback.subtitle().as_deref(),
        Some("Lines kept for scrolling back in ordinary tabs")
    );
    let range = scrollback.adjustment();
    assert_eq!(
        (range.lower(), range.upper(), range.step_increment()),
        (1_000.0, 1_000_000.0, 1_000.0)
    );
    assert_eq!(scrollback.value(), 10_000.0);
    assert_eq!(scrollback.digits(), 0);

    // Transcripts: the warning, the switch, the folder.
    let transcripts = group(&dialog, "Transcripts");
    assert_eq!(
        transcripts.description().as_deref(),
        Some(
            "Transcripts hold whatever was on screen, which may include passwords or secrets \
             typed or pasted."
        )
    );
    let save_all = save_all_row(&dialog);
    assert!(save_all.is_ancestor(&transcripts));
    assert_eq!(save_all.title(), "Save transcripts of all sessions");
    assert_eq!(
        save_all.subtitle().as_deref(),
        Some(
            "Every new tab, including the first at launch. Tabs that save a transcript keep \
             all their scrollback."
        )
    );
    assert!(!save_all.is_active(), "off by default");
    let folder_row = action_row(&transcripts, "Transcripts folder");
    let folder_text = folder.to_str().expect("utf-8");
    assert_plain_row(
        "folder row",
        &folder_row,
        "Transcripts folder",
        Some(folder_text),
    );
    literal_labels("folder row", &folder_row, folder_text);
    assert!(
        button_labeled(&folder_row, "Choose…").is_some(),
        "a Choose… button"
    );

    // Sync.
    let sync = group(&dialog, "Sync");
    let sync_row = action_row(&sync, "Sync settings");
    assert_eq!(
        sync_row.subtitle().as_deref(),
        Some("Sync key, the other machine, automatic sync")
    );
    assert!(
        button_labeled(&sync_row, "Open").is_some(),
        "an Open button"
    );

    // Exactly these groups, on one page.
    let page = dialog.visible_page().expect("the page");
    let titles: Vec<String> = all_widgets::<adw::PreferencesGroup>(&page)
        .iter()
        .map(|g| g.title().to_string())
        .collect();
    assert_eq!(
        titles,
        [
            "Terminal",
            "Sessions",
            "Connection Profiles",
            "Transcripts",
            "Sync"
        ]
    );

    // Sessions: the switch that shows the Sessions button, on.
    let sessions = all_widgets::<adw::SwitchRow>(&group(&dialog, "Sessions"))
        .into_iter()
        .next()
        .expect("the Sessions switch");
    assert_eq!(sessions.title(), "Show the Sessions menu");
    assert!(sessions.is_active(), "on by default");
    assert!(!sessions.uses_markup());

    // Connection Profiles: the switch for the host marks' pings, on.
    let pings = all_widgets::<adw::SwitchRow>(&group(&dialog, "Connection Profiles"))
        .into_iter()
        .next()
        .expect("the ping switch");
    assert_eq!(pings.title(), "Show whether hosts are reachable");
    assert_eq!(
        pings.subtitle().as_deref(),
        Some(
            "Pings each saved host from this machine now and then, and marks it in the \
             lists. Turned off, no pings are sent."
        )
    );
    assert!(pings.is_active(), "on by default");
    assert!(!pings.uses_markup());

    close_dialog(&main, &dialog);
    assert!(
        main.window
            .lookup_action(terminal_tab::COPY_ACTION)
            .and_downcast::<gio::SimpleAction>()
            .is_some_and(|a| a.is_enabled()),
        "the actions come back"
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The family of the font `terminal` uses.
fn family_of(terminal: &Terminal) -> String {
    terminal
        .font()
        .and_then(|f| f.family())
        .map(|f| f.to_string())
        .unwrap_or_default()
}

/// A font chosen in the dialog is used at once by every open terminal, in every
/// main window, and by new tabs; it is saved. A font that cannot be saved (a
/// damaged settings file) is not used: the notice says why, the button goes back.
pub(super) fn changing_the_font_applies_to_open_terminals(dir: &Path) {
    const CHOSEN: &str = "DejaVu Sans Mono 13";
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let first = main_window(&app, &ctx);
    let second = main_window(&app, &ctx);
    let open_terminals: Vec<Terminal> = [&first, &second]
        .iter()
        .map(|main| terminal_of(&main.view.selected_page().expect("a tab")).expect("a shell"))
        .collect();
    let before = family_of(&open_terminals[0]);
    assert_ne!(before, "DejaVu Sans Mono");

    let dialog = open(&app, &ctx, &first);
    font_button(&dialog).set_font_desc(&pango::FontDescription::from_string(CHOSEN));
    pump_for(Duration::from_millis(100));
    for terminal in &open_terminals {
        assert_eq!(family_of(terminal), "DejaVu Sans Mono", "an open terminal");
        assert_eq!(terminal.font().map(|f| f.size()), Some(13 * pango::SCALE));
    }
    let new_tab = open_local_shell(&second.view, zoom::DEFAULT, None);
    assert_eq!(family_of(&new_tab), "DejaVu Sans Mono", "a new tab");
    assert_eq!(ctx.settings().font.as_deref(), Some(CHOSEN), "saved");

    // A font that cannot be saved is not used, and the button shows the last one.
    let damaged = damage_settings(&ctx);
    let shown = ctx.notice.delivered();
    font_button(&dialog).set_font_desc(&pango::FontDescription::from_string("Courier 9"));
    pump_for(Duration::from_millis(100));
    assert_notice_starts(&ctx, "Could not save the preferences: ");
    assert_eq!(ctx.notice.delivered(), shown + 1, "one notice");
    assert_eq!(
        font_button(&dialog)
            .font_desc()
            .map(|f| f.to_str().to_string()),
        Some(CHOSEN.to_owned()),
        "the button went back"
    );
    for terminal in open_terminals.iter().chain([&new_tab]) {
        assert_eq!(family_of(terminal), "DejaVu Sans Mono", "unchanged");
    }
    assert_eq!(
        fs::read_to_string(&ctx.settings_path).unwrap(),
        damaged,
        "the damaged file is left as it is"
    );

    close_dialog(&first, &dialog);
    terminal_tab::apply_font(None);
    assert_ne!(
        family_of(&new_tab),
        "DejaVu Sans Mono",
        "back to the default"
    );
    for main in [first, second] {
        main.window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// The Font row's reset button, shown only while a font is chosen, goes back to
/// the system monospace font: every open terminal and new tabs use it at once,
/// the setting is removed, the button shows the system font again and the reset
/// button hides. A reset that cannot be saved (a damaged settings file) changes
/// nothing: the notice says why and the chosen font stays.
pub(super) fn the_font_can_go_back_to_the_system_font(dir: &Path) {
    const CHOSEN: &str = "DejaVu Sans Mono 13";
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let open_terminal = terminal_of(&main.view.selected_page().expect("a tab")).expect("a shell");
    let system = family_of(&open_terminal);
    assert_ne!(system, "DejaVu Sans Mono");

    let dialog = open(&app, &ctx, &main);
    let reset = preferences::font_reset_button(&dialog);
    assert!(
        !reset.is_visible(),
        "nothing to reset while no font is chosen"
    );
    font_button(&dialog).set_font_desc(&pango::FontDescription::from_string(CHOSEN));
    pump_for(Duration::from_millis(100));
    assert!(reset.is_visible(), "a chosen font can be reset");
    assert!(
        reset.is_ancestor(&action_row(&group(&dialog, "Terminal"), "Font")),
        "on the Font row"
    );
    assert_eq!(reset.tooltip_text().as_deref(), Some("Use the System Font"));

    // A reset that cannot be saved changes nothing.
    let damaged = damage_settings(&ctx);
    let shown = ctx.notice.delivered();
    reset.emit_clicked();
    pump_for(Duration::from_millis(100));
    assert_notice_starts(&ctx, "Could not save the preferences: ");
    assert_eq!(ctx.notice.delivered(), shown + 1, "one notice");
    assert_eq!(
        family_of(&open_terminal),
        "DejaVu Sans Mono",
        "still chosen"
    );
    assert_eq!(
        font_button(&dialog)
            .font_desc()
            .map(|f| f.to_str().to_string()),
        Some(CHOSEN.to_owned()),
        "the button still shows the chosen font"
    );
    assert!(reset.is_visible(), "still something to reset");
    assert_eq!(
        fs::read_to_string(&ctx.settings_path).unwrap(),
        damaged,
        "the damaged file is left as it is"
    );

    // Repaired, the reset goes through.
    fs::remove_file(&ctx.settings_path).expect("remove the damaged file");
    settings::update(&ctx.settings_path, |s| s.font = Some(CHOSEN.to_owned()))
        .expect("write the settings");
    reset.emit_clicked();
    pump_for(Duration::from_millis(100));
    assert_eq!(family_of(&open_terminal), system, "an open terminal");
    let new_tab = open_local_shell(&main.view, zoom::DEFAULT, None);
    assert_eq!(family_of(&new_tab), system, "a new tab");
    assert_eq!(ctx.settings().font, None, "the setting is removed");
    let shown = font_button(&dialog).font_desc().expect("a font is shown");
    assert!(
        shown
            .family()
            .is_some_and(|f| f.eq_ignore_ascii_case("monospace")),
        "the button shows the system monospace font: {shown}"
    );
    assert!(!reset.is_visible(), "nothing left to reset");

    // Choosing a font afterwards is saved as before.
    font_button(&dialog).set_font_desc(&pango::FontDescription::from_string(CHOSEN));
    pump_for(Duration::from_millis(100));
    assert_eq!(ctx.settings().font.as_deref(), Some(CHOSEN), "saved");
    assert_eq!(family_of(&new_tab), "DejaVu Sans Mono");
    assert!(reset.is_visible());

    close_dialog(&main, &dialog);
    terminal_tab::apply_font(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A transcript spec for a test session saving into `dir`.
fn spec(ctx: &AppContext, dir: &Path) -> TranscriptSpec {
    TranscriptSpec {
        host: "scrollback.example.test".to_owned(),
        dir: dir.to_path_buf(),
        notice: ctx.notice.clone(),
    }
}

/// The scrollback chosen applies to ordinary tabs opened after the change; a
/// tab already open keeps its own, and a tab that saves a transcript keeps
/// everything. It is saved; one that cannot be saved is not used.
pub(super) fn scrollback_applies_to_new_ordinary_tabs(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let existing = terminal_of(&main.view.selected_page().expect("a tab")).expect("a shell");
    assert_eq!(existing.scrollback_lines(), 10_000);

    let dialog = open(&app, &ctx, &main);
    scrollback_row(&dialog).set_value(2_000.0);
    pump_for(Duration::from_millis(100));
    assert_eq!(ctx.settings().scrollback_lines, 2_000, "saved");
    let ordinary = open_local_shell(&main.view, zoom::DEFAULT, None);
    assert_eq!(ordinary.scrollback_lines(), 2_000, "a new ordinary tab");
    assert_eq!(
        existing.scrollback_lines(),
        10_000,
        "an open tab keeps its own"
    );
    let saving = open_local_shell(
        &main.view,
        zoom::DEFAULT,
        Some(spec(&ctx, &dir.join("transcripts"))),
    );
    let lines = saving.scrollback_lines();
    assert!(
        !(0..=1_000_000_000).contains(&lines),
        "a tab that saves a transcript keeps everything, got {lines}"
    );

    // One that cannot be saved is not used, and the row goes back.
    damage_settings(&ctx);
    let shown = ctx.notice.delivered();
    scrollback_row(&dialog).set_value(5_000.0);
    pump_for(Duration::from_millis(100));
    assert_notice_starts(&ctx, "Could not save the preferences: ");
    assert_eq!(ctx.notice.delivered(), shown + 1, "one notice");
    assert_eq!(
        scrollback_row(&dialog).value(),
        2_000.0,
        "the row went back"
    );
    let another = open_local_shell(&main.view, zoom::DEFAULT, None);
    assert_eq!(another.scrollback_lines(), 2_000);

    close_dialog(&main, &dialog);
    terminal_tab::set_scrollback(settings::SCROLLBACK_DEFAULT);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The save-all switch is saved both ways; a change that cannot be saved puts
/// the switch back and says why.
pub(super) fn the_save_all_switch_is_saved(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);
    let switch = save_all_row(&dialog);

    switch.set_active(true);
    pump_for(Duration::from_millis(50));
    assert!(ctx.settings().save_all_transcripts, "on is saved");
    switch.set_active(false);
    pump_for(Duration::from_millis(50));
    assert!(!ctx.settings().save_all_transcripts, "off is saved");

    // Putting the switch back is not taken for another change: one notice, and
    // it stays off (a switch set back from inside its own change handler is
    // notified after the handler returns, which once looped forever).
    damage_settings(&ctx);
    let shown = ctx.notice.delivered();
    switch.set_active(true);
    pump_for(Duration::from_millis(50));
    assert_notice_starts(&ctx, "Could not save the preferences: ");
    assert_eq!(ctx.notice.delivered(), shown + 1, "one notice");
    assert!(!switch.is_active(), "the switch went back");

    close_dialog(&main, &dialog);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Two transcripts in `dir`, with the names sessions give them.
fn two_transcripts(dir: &Path) -> Vec<String> {
    fs::create_dir_all(dir).expect("the old folder");
    let names: Vec<String> = [
        ("web.example.test", 1_790_000_000),
        ("db.example.test", 1_790_000_100),
    ]
    .iter()
    .map(|(host, secs)| {
        let path = transcript_path(dir, host, *secs);
        fs::write(&path, format!("{host}\n")).expect("a transcript");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        path.file_name().unwrap().to_string_lossy().into_owned()
    })
    .collect();
    let mut names = names;
    names.sort();
    names
}

/// Sorted file names in `dir`.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Choosing another folder (the handler the file chooser calls) saves it, moves
/// the transcripts into it on a worker, says how many moved, and switches an
/// open Transcripts window to it. A folder that cannot take transcripts (not
/// absolute, a file) is refused before anything changes; the same folder does
/// nothing. While the move runs the application is held (so that closing the
/// last window cannot cut a copy short) and Choose… waits, also in a dialog
/// closed and opened again meanwhile; when it ends both are back.
pub(super) fn choosing_a_folder_moves_the_transcripts(dir: &Path) {
    let old = dir.join("old");
    let new = dir.join("new").join("transcripts");
    let names = two_transcripts(&old);
    let ctx = ctx_with(dir, &[]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(old.clone())
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    recordings::present(&app, ctx.transcripts_dir(), &ctx.settings_path);
    pump_for(Duration::from_millis(300));
    assert_eq!(listed_in_transcripts_window(&app), names);
    let dialog = open(&app, &ctx, &main);
    let folder_row = action_row(&dialog, "Transcripts folder");
    let choose = button_labeled(&folder_row, "Choose…").expect("Choose…");

    // Refused: nothing saved, nothing moved, nothing made.
    let shown = ctx.notice.delivered();
    preferences::change_folder(&dialog, &app, &ctx, PathBuf::from("relative/folder"));
    assert_eq!(ctx.notice.delivered(), shown + 1);
    assert_notice_starts(&ctx, "Could not use that folder: ");
    let file = dir.join("a-file");
    fs::write(&file, "x").expect("a file");
    preferences::change_folder(&dialog, &app, &ctx, file.clone());
    assert_notice_starts(&ctx, "Could not use that folder: ");
    assert_eq!(fs::read_to_string(&file).unwrap(), "x");
    assert_eq!(ctx.transcripts_dir(), old, "the old folder is kept");
    assert_eq!(names_in(&old), names, "nothing moved");
    assert_eq!(folder_row.subtitle().as_deref(), old.to_str());

    // The same folder: nothing happens at all.
    let shown = ctx.notice.delivered();
    preferences::change_folder(&dialog, &app, &ctx, old.clone());
    pump_for(Duration::from_millis(200));
    assert_eq!(ctx.notice.delivered(), shown, "nothing to say");
    assert_eq!(names_in(&old), names);

    // A new folder: saved at once, the transcripts follow. The move is held
    // back, so that what is true while it runs can be looked at.
    assert_eq!(preferences::moves_holding_the_app(), 0);
    let release = preferences::hold_back_the_next_move();
    preferences::change_folder(&dialog, &app, &ctx, new.clone());
    assert_eq!(ctx.transcripts_dir(), new, "saved before the move");
    assert_eq!(folder_row.subtitle().as_deref(), new.to_str());
    assert!(!choose.is_sensitive(), "Choose… waits for the move");
    assert_eq!(
        preferences::moves_holding_the_app(),
        1,
        "the application is held while the move runs"
    );

    // The dialog is closed and opened again meanwhile: its Choose… waits too,
    // and another folder chosen now is refused.
    close_dialog(&main, &dialog);
    let dialog = open(&app, &ctx, &main);
    let reopened =
        button_labeled(&action_row(&dialog, "Transcripts folder"), "Choose…").expect("Choose…");
    assert!(!reopened.is_sensitive(), "the new dialog waits as well");
    let shown = ctx.notice.delivered();
    let third = dir.join("third");
    preferences::change_folder(&dialog, &app, &ctx, third.clone());
    assert_eq!(ctx.notice.delivered(), shown + 1);
    assert_notice_starts(&ctx, "Transcripts are still being moved");
    assert!(!third.exists(), "nothing made for the refused folder");
    assert_eq!(ctx.transcripts_dir(), new, "and nothing saved");
    assert_eq!(names_in(&old), names, "the move has not started");
    assert_eq!(preferences::moves_holding_the_app(), 1);

    release.send(()).expect("let the move go");
    wait_for_notice(&ctx, "Moved 2 transcripts.");
    assert_eq!(
        ctx.notice.last_dialog().as_ref(),
        Some(&dialog),
        "the result is told in the dialog that is open when the move ends"
    );
    pump_for(Duration::from_millis(100));
    assert!(
        reopened.is_sensitive(),
        "Choose… of the dialog open now is back once the move is over"
    );
    assert!(choose.is_sensitive(), "and that of the one it began in");
    assert_eq!(
        preferences::moves_holding_the_app(),
        0,
        "the application is let go when the move ends"
    );
    assert_eq!(names_in(&new), names, "both moved");
    assert!(names_in(&old).is_empty(), "none left behind");
    assert_eq!(
        fs::metadata(&new).unwrap().permissions().mode() & 0o777,
        0o700,
        "a folder Moniker makes is private"
    );
    pump_until("the Transcripts window to list the new folder", || {
        listed_in_transcripts_window(&app) == names
    });
    let window = app
        .windows()
        .into_iter()
        .find(|w| {
            all_widgets::<gtk::ListBox>(w)
                .iter()
                .any(|l| l.widget_name() == recordings::LIST_NAME)
        })
        .expect("the Transcripts window");
    // A file that arrives in the new folder shows there; one in the old does not.
    fs::write(old.join("stray_20261017-000000.txt"), "old").expect("a stray file");
    let arrived = transcript_path(&new, "late.example.test", 1_790_000_200);
    fs::write(&arrived, "late").expect("a late transcript");
    let late = arrived.file_name().unwrap().to_string_lossy().into_owned();
    pump_until("the window to follow the new folder", || {
        listed_in_transcripts_window(&app).contains(&late)
    });
    assert!(
        !listed_in_transcripts_window(&app)
            .iter()
            .any(|n| n.starts_with("stray")),
        "the old folder is no longer listed"
    );

    close_dialog(&main, &dialog);
    window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The settings file as it is, or nothing if there is none.
fn settings_bytes(ctx: &AppContext) -> Option<Vec<u8>> {
    fs::read(&ctx.settings_path).ok()
}

/// A move carries the transcripts and nothing else: files in the old folder
/// whose names are not transcript names (the folder was once the home folder or
/// Documents) stay, untouched, and the notice says how many were left and in
/// which folder, as the Transcripts window lists only the new one now.
pub(super) fn a_move_leaves_other_files_where_they_are(dir: &Path) {
    let old = dir.join("old");
    let new = dir.join("new");
    let names = two_transcripts(&old);
    let others = [(".bashrc", "export A=1\n"), ("notes.txt", "call back")];
    for (name, text) in others {
        fs::write(old.join(name), text).expect("a file of the person's");
    }
    let ctx = ctx_with(dir, &[]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(old.clone())
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);

    preferences::change_folder(&dialog, &app, &ctx, new.clone());
    wait_for_notice(
        &ctx,
        &format!(
            "Moved 2 transcripts. 2 other files were left in the old folder. Old folder: {}",
            old.display()
        ),
    );

    assert_eq!(names_in(&new), names, "only the transcripts moved");
    assert_eq!(names_in(&old), [".bashrc", "notes.txt"]);
    for (name, text) in others {
        assert_eq!(fs::read_to_string(old.join(name)).unwrap(), text, "{name}");
    }

    close_dialog(&main, &dialog);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A folder whose name cannot be saved (it is not valid UTF-8) is refused with a
/// notice before anything changes: nothing saved, nothing created.
pub(super) fn a_folder_that_cannot_be_saved_is_refused(dir: &Path) {
    let old = dir.join("old");
    fs::create_dir_all(&old).expect("the old folder");
    let ctx = ctx_with(dir, &[]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(old.clone())
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);
    let folder_row = action_row(&dialog, "Transcripts folder");

    let before = settings_bytes(&ctx);
    let listing = names_in(dir);
    let bad = dir.join(OsStr::from_bytes(b"not-utf8-\xff"));
    assert!(bad.to_str().is_none(), "the test path is not UTF-8");
    let shown = ctx.notice.delivered();
    preferences::change_folder(&dialog, &app, &ctx, bad.clone());

    assert_eq!(ctx.notice.delivered(), shown + 1, "one notice");
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Could not use that folder: its name cannot be saved (it is not valid UTF-8)")
    );
    assert!(!bad.exists(), "the folder was not made");
    assert_eq!(names_in(dir), listing, "nothing was created anywhere");
    assert_eq!(settings_bytes(&ctx), before, "nothing was saved");
    assert_eq!(ctx.transcripts_dir(), old);
    assert_eq!(folder_row.subtitle().as_deref(), old.to_str());
    assert_eq!(preferences::moves_holding_the_app(), 0, "no move started");
    assert!(
        button_labeled(&folder_row, "Choose…").is_some_and(|b| b.is_sensitive()),
        "Choose… is still there to use"
    );

    close_dialog(&main, &dialog);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The folder in use, chosen again under another name (a path through `.` or
/// `..`, a link to it), changes nothing: no notice, nothing saved, no move.
pub(super) fn the_same_folder_by_another_name_changes_nothing(dir: &Path) {
    let old = dir.join("old");
    let names = two_transcripts(&old);
    let link = dir.join("link-to-old");
    std::os::unix::fs::symlink(&old, &link).expect("a link to the folder");
    let ctx = ctx_with(dir, &[]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(old.clone())
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);
    let folder_row = action_row(&dialog, "Transcripts folder");

    let before = settings_bytes(&ctx);
    let listing = names_in(dir);
    let shown = ctx.notice.delivered();
    for same in [
        old.join("."),
        old.join("..").join("old"),
        link.clone(),
        link.join("."),
    ] {
        preferences::change_folder(&dialog, &app, &ctx, same.clone());
        pump_for(Duration::from_millis(100));
        assert_eq!(ctx.notice.delivered(), shown, "{same:?}: nothing to say");
        assert_eq!(settings_bytes(&ctx), before, "{same:?}: nothing saved");
        assert_eq!(ctx.transcripts_dir(), old, "{same:?}");
        assert_eq!(folder_row.subtitle().as_deref(), old.to_str(), "{same:?}");
        assert_eq!(names_in(&old), names, "{same:?}: nothing moved");
        assert_eq!(names_in(dir), listing, "{same:?}: nothing made");
        assert_eq!(preferences::moves_holding_the_app(), 0, "{same:?}");
    }
    assert!(
        button_labeled(&folder_row, "Choose…").is_some_and(|b| b.is_sensitive()),
        "no move was started"
    );

    close_dialog(&main, &dialog);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Asking for the Preferences while they are open (Ctrl+comma in another main
/// window) opens no second dialog and raises the window the open one is over.
pub(super) fn asking_again_raises_the_open_preferences(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let first = main_window(&app, &ctx);
    let second = main_window(&app, &ctx);
    first.window.present();
    pump_until("the first window to be active", || first.window.is_active());
    let dialog = open(&app, &ctx, &first);

    second.window.present();
    pump_until("the second window to be active", || {
        second.window.is_active()
    });
    let again = preferences::present(&second.window, &app, &ctx);
    assert_eq!(again, dialog, "the open dialog, not a second one");
    pump_until("the first window to be raised", || first.window.is_active());
    assert!(
        second.window.visible_dialog().is_none(),
        "nothing opened over the second window"
    );
    assert_eq!(
        first.window.visible_dialog().as_ref(),
        Some(dialog.upcast_ref())
    );

    close_dialog(&first, &dialog);
    for main in [first, second] {
        main.window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// "Open" on the Sync settings row opens the Connection Profiles window on its
/// sync page.
pub(super) fn the_sync_button_opens_the_sync_page(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);
    button_labeled(&action_row(&dialog, "Sync settings"), "Open")
        .expect("the Open button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    let profiles_window = app
        .windows()
        .into_iter()
        .find(|w| w.title().as_deref() == Some("Connection Profiles"))
        .and_downcast::<adw::Window>()
        .expect("the Connection Profiles window");
    let entry = named(&profiles_window, profiles::SYNC_ENTRY_NAME)
        .and_downcast::<gtk::ListBox>()
        .expect("the sidebar's Sync entry");
    assert!(entry.selected_row().is_some(), "on the sync page");
    assert!(
        find_widget::<adw::PreferencesPage>(&profiles_window).is_some(),
        "the sync page is shown"
    );
    assert!(
        main.window.visible_dialog().is_none(),
        "the Preferences make way for it"
    );

    profiles_window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Ctrl+comma is app.preferences, listed in Keyboard Shortcuts under General,
/// and ☰ has Preferences; both open the dialog over the main window, once.
pub(super) fn preferences_shortcut_exists(dir: &Path) {
    let general = crate::SHORTCUTS
        .iter()
        .find(|(title, _)| *title == "General")
        .map(|(_, items)| *items)
        .expect("a General section");
    assert!(
        general.contains(&("Preferences", "app.preferences", "<Control>comma")),
        "{general:?}"
    );

    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    crate::set_accels(&app);
    let main = main_window(&app, &ctx);
    let parse = |a: &str| gtk::accelerator_parse(a).expect("an accelerator");
    let set: Vec<_> = app
        .accels_for_action("app.preferences")
        .iter()
        .map(|a| parse(a))
        .collect();
    assert_eq!(set, [parse("<Control>comma")]);

    let header: adw::HeaderBar = find_widget(&main.window).expect("the header bar");
    let menu = all_widgets::<gtk::MenuButton>(&header)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Main Menu"))
        .and_then(|b| b.menu_model())
        .expect("the ☰ menu");
    assert!(
        menu_items(&menu).contains(&("Preferences".to_owned(), "app.preferences".to_owned())),
        "☰ has Preferences"
    );

    main.window.present();
    app.activate_action("preferences", None);
    pump_until("the Preferences dialog", || {
        main.window
            .visible_dialog()
            .is_some_and(|d| d.is::<adw::PreferencesDialog>())
    });
    let dialog = main.window.visible_dialog().expect("the dialog");
    // Asking again while it is open opens no second one.
    app.activate_action("preferences", None);
    pump_for(Duration::from_millis(200));
    assert_eq!(main.window.visible_dialog(), Some(dialog.clone()));
    dialog.force_close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The Preferences dialog covers the window's notification area, so its own
/// messages (here a setting that cannot be saved) are shown in the dialog, as a
/// toast with markup off, and not in the window; once the dialog is closed a
/// message goes to the window's area again.
pub(super) fn the_dialogs_messages_show_in_the_dialog(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let dialog = open(&app, &ctx, &main);

    damage_settings(&ctx);
    let shown = ctx.notice.delivered();
    scrollback_row(&dialog).set_value(5_000.0);
    pump_for(Duration::from_millis(100));
    assert_notice_starts(&ctx, "Could not save the preferences: ");
    assert_eq!(ctx.notice.delivered(), shown + 1, "one notice");
    assert_eq!(
        ctx.notice.last_dialog().as_ref(),
        Some(&dialog),
        "told in the dialog"
    );
    assert!(ctx.notice.last_area().is_none(), "not in a window's area");
    let text = ctx.notice.last_text().expect("the notice");
    pump_until("the toast in the dialog", || {
        labels_in(&dialog).iter().any(|l| l.label() == text)
    });
    literal_labels("preferences toast", &dialog, &text);
    assert_eq!(
        main.notifications.shown_text(),
        None,
        "the window's area shows nothing"
    );
    assert_eq!(main.notifications.queued(), 0);

    // Markup characters are shown as they are (a message can quote a folder).
    ctx.notice.show_over(&dialog, MARKUP_NAME);
    assert_eq!(ctx.notice.last_dialog().as_ref(), Some(&dialog));
    pump_until("the toast in the dialog", || {
        labels_in(&dialog).iter().any(|l| l.label() == MARKUP_NAME)
    });
    literal_labels("preferences toast", &dialog, MARKUP_NAME);

    // The dialog is closed: the window's area is where a message goes now.
    close_dialog(&main, &dialog);
    ctx.notice.show_over(&dialog, "after the dialog");
    assert_eq!(ctx.notice.last_text().as_deref(), Some("after the dialog"));
    assert!(ctx.notice.last_dialog().is_none());
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&main.notifications));
    assert_eq!(
        main.notifications.shown_text().as_deref(),
        Some("after the dialog")
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}
