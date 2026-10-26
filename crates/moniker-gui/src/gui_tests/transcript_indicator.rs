//! Scenarios: the REC indicator of a tab whose session saves a transcript. The
//! tab shows it with libadwaita's indicator (the record icon at its start),
//! REC before its name and a last line of its tooltip; the header bar shows
//! the icon for the tab in front; a pane's header shows it in a split. It
//! comes when the session starts recording and goes when its transcript is
//! written, and it is never part of the name.
//!
//! Connections run a fake `ssh`, local tabs `/bin/sh`; the transcripts go to a
//! folder in the temporary directory each scenario is given.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gio;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::secrets::MemorySecretStore;
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{ctx_with, ctx_with_secrets, test_app};
use super::save_transcripts::{click, configure, connect_plainly, new_list, set_save_all};
use super::windows::main_window;
use super::{
    HeldKeyring, all_widgets, button_labeled, fake_ssh, files_in, find_widget, pane, pump_for,
    pump_until, wait_for_session, wait_for_text,
};
use crate::launch_args::{Run, Start};
use crate::panes::{self, Pane, REC_PREFIX, RECORDING_ICON, RECORDING_TOOLTIP, TabLayout};
use crate::session_file::Node;
use crate::terminal_tab::{
    self, PaneHolder, local_transcript_spec, set_test_ssh_program, shown_title, terminal_in,
    terminal_of,
};
use crate::{new_tab, session_layout, window};

/// The name of the icon `page`'s indicator shows, if it shows one.
fn indicator_icon(page: &adw::TabPage) -> Option<String> {
    let icon = page.indicator_icon()?.downcast::<gio::ThemedIcon>().ok()?;
    icon.names().first().map(ToString::to_string)
}

/// Whether one line of `page`'s tooltip is [`RECORDING_TOOLTIP`].
fn tooltip_says_it_records(page: &adw::TabPage) -> bool {
    page.tooltip()
        .unwrap_or_default()
        .lines()
        .any(|line| line == RECORDING_TOOLTIP)
}

/// `page` shows that it saves a transcript: the record icon (not clickable,
/// "Saving a transcript"), REC before its focused pane's title, and the last
/// line of its tooltip.
#[track_caller]
fn assert_rec(page: &adw::TabPage) {
    assert_eq!(indicator_icon(page).as_deref(), Some(RECORDING_ICON));
    assert_eq!(page.indicator_tooltip(), RECORDING_TOOLTIP);
    assert!(
        !page.is_indicator_activatable(),
        "the icon is not clickable"
    );
    let name = shown_title(&pane(page));
    assert_eq!(page.title(), format!("{REC_PREFIX}{name}"));
    let tooltip = page.tooltip().unwrap_or_default();
    assert_eq!(
        tooltip.lines().last(),
        Some(RECORDING_TOOLTIP),
        "{tooltip:?}"
    );
}

/// `page` shows nothing of the sort: no icon, its focused pane's title as it
/// is, no such line in its tooltip.
#[track_caller]
fn assert_plain(page: &adw::TabPage) {
    assert_eq!(indicator_icon(page), None);
    assert_eq!(page.title(), shown_title(&pane(page)));
    assert!(!tooltip_says_it_records(page), "{:?}", page.tooltip());
}

/// The record icon in `main`'s header bar, which shows while the tab in front
/// saves a transcript.
fn header_icon(main: &window::MainWindow) -> gtk::Image {
    let header: adw::HeaderBar = find_widget(&main.window).expect("the header bar");
    all_widgets::<gtk::Image>(&header)
        .into_iter()
        .find(|image| image.widget_name() == window::RECORDING_IMAGE_NAME)
        .expect("the header bar's record icon")
}

fn layout_of(page: &adw::TabPage) -> TabLayout {
    panes::layout_of_page(page).expect("a tab made by terminal_tab has a layout")
}

fn pane_of(holder: &PaneHolder) -> Pane {
    panes::pane_of(holder).expect("a holder is in a pane")
}

/// The focused pane of `page`.
fn focused(page: &adw::TabPage) -> PaneHolder {
    layout_of(page).focused_holder().expect("a focused pane")
}

/// Split the selected tab of `main` to the right and open a local terminal in
/// the new pane from its list (saving a transcript when "Save transcripts of
/// all sessions" is on, as a plain click does). Returns the new pane.
fn split_with_local(main: &window::MainWindow, page: &adw::TabPage) -> PaneHolder {
    WidgetExt::activate_action(&main.window, "win.split-right", None).expect("win.split-right");
    let holder = focused(page);
    let list = find_widget::<gtk::ListBox>(&holder)
        .filter(|l| l.widget_name() == new_tab::LIST_NAME)
        .expect("the new pane's list");
    click(&list, new_tab::LOCAL_ROW);
    pump_until("the new pane's shell", || terminal_in(&holder).is_some());
    holder
}

/// Have `/bin/sh` in `terminal` show `word` once its prompt is there (input
/// sent before the shell has started is lost).
fn say(terminal: &Terminal, word: &str) {
    wait_for_text(terminal, "$");
    terminal.feed_child(format!("echo {word}-$((40+2))\n").as_bytes());
    wait_for_text(terminal, &format!("{word}-42"));
}

/// The Rename dialog over `main` and its field, once it shows.
fn rename_dialog(main: &window::MainWindow) -> (adw::AlertDialog, gtk::Entry) {
    pump_until("the Rename dialog", || {
        find_widget::<adw::AlertDialog>(&main.window).is_some()
    });
    let dialog: adw::AlertDialog = find_widget(&main.window).expect("the Rename dialog");
    let entry = dialog
        .extra_child()
        .and_downcast::<gtk::Entry>()
        .expect("its name field");
    (dialog, entry)
}

/// Press `label` in `dialog` and wait for it to go.
fn answer(main: &window::MainWindow, dialog: &adw::AlertDialog, label: &str) {
    button_labeled(dialog, label)
        .unwrap_or_else(|| panic!("no {label} button"))
        .emit_clicked();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
}

/// With "Save transcripts of all sessions" on, a window's first tab (a local
/// shell) shows REC: the icon and its tooltip, the prefix, the tooltip's last
/// line, and the header bar's icon (the tab bar is hidden with one tab), in the
/// title's own color. With it off a new local tab shows none of them, and
/// selecting one tab or the other shows or hides the header bar's icon. A
/// profile tab whose Save transcript switch is on shows REC once its session
/// records, not while it waits for the keyring; one whose switch is off never
/// does.
pub(super) fn a_tab_saving_a_transcript_shows_rec(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "rec-shows", "echo FAKE-END\nexec sleep 600");
    let transcripts = dir.join("transcripts");
    let mut saves = Profile::new("saves", "saves.example.test", "sam");
    saves.transcript = true;
    let mut quiet = Profile::new("quiet", "quiet.example.test", "quinn");
    quiet.transcript = false;
    let keyring = Arc::new(HeldKeyring::new(Arc::new(MemorySecretStore::default())));
    let ctx = ctx_with_secrets(
        dir,
        &[saves.clone(), quiet.clone()],
        Arc::clone(&keyring) as _,
    );
    configure(&ctx, &transcripts, true);
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    // The header bar's icon: the record icon with its own tooltip, before the
    // title, and drawn in the title's color (no color class of its own).
    let icon = header_icon(&main);
    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    assert_eq!(icon.icon_name().as_deref(), Some(RECORDING_ICON));
    assert_eq!(icon.tooltip_text().as_deref(), Some(RECORDING_TOOLTIP));
    assert_eq!(
        icon.next_sibling()
            .and_downcast::<adw::WindowTitle>()
            .as_ref(),
        Some(&title),
        "the icon comes before the title"
    );
    assert!(
        icon.css_classes().is_empty(),
        "no color of its own: {:?}",
        icon.css_classes()
    );

    // Save all on: the first tab records, and says so.
    let first = main.view.selected_page().expect("the launch tab");
    say(&terminal_of(&first).expect("a local shell"), "first");
    assert_rec(&first);
    pump_until("the header bar's icon", || icon.is_visible());
    assert_eq!(title.title(), first.title(), "the header bar's title");
    assert_eq!(
        main.window.title(),
        Some(first.title()),
        "the window's title"
    );

    // Off: a new local tab saves nothing and shows nothing; the header bar's
    // icon goes with it in front.
    set_save_all(&ctx, false);
    let (second, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    say(&terminal_of(&second).expect("a local shell"), "second");
    assert_plain(&second);
    pump_until("the header bar's icon to hide", || !icon.is_visible());

    // Selecting one tab or the other shows or hides it.
    main.view.set_selected_page(&first);
    pump_until("the icon with the recording tab", || icon.is_visible());
    main.view.set_selected_page(&second);
    pump_until("no icon with the plain tab", || !icon.is_visible());

    // A profile whose switch is on: nothing while it waits for the keyring
    // (nothing is recorded yet), REC once its session starts.
    keyring.hold();
    let (third, list) = new_list(&main, &ctx);
    click(&list, &saves.id);
    pump_until("the tab named after the profile", || {
        shown_title(&pane(&third)) == "saves"
    });
    pump_for(Duration::from_millis(300));
    assert!(terminal_of(&third).is_none(), "it still waits");
    assert_plain(&third);
    assert!(!icon.is_visible());
    keyring.release();
    wait_for_text(&wait_for_session(&third), "FAKE-END");
    assert_rec(&third);
    pump_until("the icon with the recording connection", || {
        icon.is_visible()
    });

    // One whose switch is off: none of it.
    let fourth = connect_plainly(&main, &ctx, &quiet.id);
    assert_plain(&fourth);
    pump_until("no icon with the plain connection", || !icon.is_visible());
    assert_rec(&first);
    assert_rec(&third);

    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// REC goes when the transcript is written. A local command tab running a
/// shell (what `moniker -- sh` opens; a local shell's own pane closes whenever
/// its shell exits) whose shell exits with status 3 keeps its pane and the
/// note, its transcript is written, and the tab and the header bar no longer
/// show REC.
pub(super) fn rec_goes_when_the_transcript_is_written(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, true);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let icon = header_icon(&main);
    let start = Start {
        dir: dir.to_path_buf(),
        title: None,
        run: Run::Command {
            argv: vec!["/bin/sh".to_owned()],
            hold: false,
        },
        dir_refused: None,
    };
    let terminal =
        terminal_tab::open_local(&main.view, 1.0, &start, local_transcript_spec(&ctx, true));
    let page = main.view.selected_page().expect("the command tab");
    assert_eq!(terminal_of(&page).as_ref(), Some(&terminal));
    say(&terminal, "saved");
    assert_rec(&page);
    pump_until("the header bar's icon", || icon.is_visible());
    let before = files_in(&transcripts).len();

    terminal.feed_child(b"exit 3\n");
    wait_for_text(&terminal, "[session ended: status 3]");
    assert_eq!(
        terminal_of(&page).as_ref(),
        Some(&terminal),
        "the pane stays, with its note"
    );
    assert_plain(&page);
    pump_until("the header bar's icon to hide", || !icon.is_visible());
    let files = files_in(&transcripts);
    assert_eq!(files.len(), before + 1, "its transcript: {files:?}");
    assert!(
        files
            .iter()
            .any(|f| std::fs::read_to_string(f).is_ok_and(|t| t.contains("saved-42")))
    );

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A split tab with a recording pane and a plain one shows REC whichever pane
/// is focused (before that pane's title), and only the recording pane's header
/// has the badge, dimmed with its title when the other pane is focused.
/// Closing the recording pane takes REC from the tab. A recording pane moved
/// to a tab of its own (the header's Move to New Tab) takes REC along, and the
/// tab it left loses it.
pub(super) fn a_split_shows_rec_on_the_pane_that_saves(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, false);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let plain = pane(&page);
    assert_plain(&page);

    // The second pane is a local terminal picked with "save all" on.
    set_save_all(&ctx, true);
    let recording = split_with_local(&main, &page);
    assert_eq!(focused(&page), recording);
    assert_rec(&page);
    let badge = pane_of(&recording).badge().clone();
    assert!(pane_of(&recording).header().is_visible());
    assert!(badge.is_visible(), "the recording pane's badge");
    assert!(
        !pane_of(&plain).badge().is_visible(),
        "no badge on the plain pane"
    );
    assert_eq!(badge.tooltip_text().as_deref(), Some(RECORDING_TOOLTIP));
    let image: gtk::Image = find_widget(&badge).expect("the badge's icon");
    assert_eq!(image.icon_name().as_deref(), Some(RECORDING_ICON));
    let label: gtk::Label = find_widget(&badge).expect("the badge's label");
    assert_eq!(label.label(), "REC");
    assert!(!label.uses_markup());
    assert_eq!(
        badge.next_sibling().as_ref(),
        Some(pane_of(&recording).title().upcast_ref::<gtk::Widget>()),
        "the badge comes before the pane's title"
    );
    assert!(!badge.has_css_class("dim-label"), "focused: not dimmed");

    // The plain pane focused: the tab still shows REC, before its title, and
    // the recording pane's badge is dimmed with its title.
    panes::focus_pane(&plain);
    assert_eq!(focused(&page), plain);
    assert_rec(&page);
    assert!(badge.is_visible());
    assert!(badge.has_css_class("dim-label"), "not focused: dimmed");
    assert!(pane_of(&recording).title().has_css_class("dim-label"));

    // Closing the recording pane writes its transcript and takes REC away.
    terminal_tab::close_pane(&recording);
    pump_until("the pane to close", || layout_of(&page).panes().len() == 1);
    assert_plain(&page);
    assert_eq!(files_in(&transcripts).len(), 1, "its transcript");

    // A recording pane moved to a tab of its own takes REC along.
    let moving = split_with_local(&main, &page);
    assert_rec(&page);
    WidgetExt::activate_action(pane_of(&moving).header(), "pane.pane-to-tab", None)
        .expect("pane.pane-to-tab");
    pump_until("the pane's own tab", || main.view.n_pages() == 2);
    let moved = main.view.selected_page().expect("its tab");
    assert_eq!(layout_of(&moved).holders(), std::slice::from_ref(&moving));
    assert_rec(&moved);
    assert_plain(&page);
    assert!(!pane_of(&moving).header().is_visible(), "alone: no header");

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// REC is not part of the name. Renaming a recording tab to "Build" shows
/// "REC  Build" while the pane is called "Build", and the Rename dialog asked
/// again holds "Build"; a saved session records "Build". A local shell that
/// sets its own title (in a tab not renamed) gets REC before that title.
pub(super) fn rec_is_not_part_of_the_name(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, true);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    assert_rec(&page);

    // Rename: the field starts with the name alone.
    terminal_tab::ask_rename(&page);
    let (dialog, entry) = rename_dialog(&main);
    assert_eq!(entry.text(), shown_title(&pane(&page)));
    assert!(!entry.text().starts_with("REC"), "{}", entry.text());
    entry.set_text("Build");
    answer(&main, &dialog, "Rename");
    assert_eq!(shown_title(&pane(&page)), "Build");
    assert_eq!(page.title(), "REC  Build");
    assert_rec(&page);
    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    assert_eq!(title.title(), "REC  Build");

    // Asked again, the field holds the name, without REC.
    terminal_tab::ask_rename(&page);
    let (dialog, entry) = rename_dialog(&main);
    assert_eq!(entry.text(), "Build");
    answer(&main, &dialog, "Cancel");
    assert_eq!(page.title(), "REC  Build");

    // A saved session records the name, not what the tab shows.
    let tabs = session_layout::capture(&main.view);
    assert_eq!(tabs.len(), 1);
    match &tabs[0].root {
        Node::Pane(record) => assert_eq!(record.name.as_deref(), Some("Build")),
        Node::Split { .. } => panic!("one pane"),
    }

    // A shell that sets its own title: REC stays before it.
    let (other, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    let terminal = terminal_of(&other).expect("a local shell");
    say(&terminal, "titled");
    assert_rec(&other);
    terminal.feed_child(b"printf '\\033]0;plain\\007'\n");
    pump_until("the shell's title", || {
        shown_title(&pane(&other)) == "plain"
    });
    assert_eq!(other.title(), "REC  plain");
    assert_rec(&other);

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}
