//! Scenarios: the new-tab list (＋ and Ctrl+Shift+T), several main windows,
//! tabs moving between windows, closing, the keyboard shortcuts, and the text
//! size of tabs (its actions, the ☰ menu's zoom row, and what is remembered).
//!
//! Sessions picked from the new-tab list run a fake `ssh` through the
//! test-only program hook of [`terminal_tab::set_test_ssh_program`]; nothing
//! here starts the real one. A tab is moved between windows with
//! `TabView::transfer_page` (what a drop does) and a drop outside every window
//! with the view's `create-window` signal, as `AdwTabBar` emits it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::{Profile, StartupSequence};
use moniker_core::secrets::MemorySecretStore;
use moniker_core::store::ProfileStore;
use moniker_core::transcript::default_dir;
use vte4::prelude::*;

use super::main_window::{choices_parts, ctx_with, press_key, test_app};
use super::{
    MARKUP_NAME, all_widgets, close_popover, fake_command, fake_ssh, files_in, find_widget,
    labels_in, menu_button, open_popover, press_enter_in, profile, pump_for, pump_until,
    wait_for_text,
};
use crate::context::{AppContext, Notice};
use crate::terminal_tab::{self, TranscriptSpec, open_command, tab_title, terminal_of};
use crate::{connect_menu, new_tab, settings, window, zoom};

/// A presented main window with one local tab.
pub(super) fn main_window(app: &adw::Application, ctx: &Rc<AppContext>) -> window::MainWindow {
    let main = window::build(app, ctx);
    main.window.present();
    pump_for(Duration::from_millis(300));
    main
}

fn nas() -> Profile {
    Profile::new("NAS box", "nas.lan", "admin")
}

/// The new-tab list shown in `page`, if it still shows one.
pub(super) fn list_in(page: &adw::TabPage) -> Option<gtk::ListBox> {
    find_widget::<gtk::ListBox>(&page.child()).filter(|l| l.widget_name() == new_tab::LIST_NAME)
}

fn rows(list: &gtk::ListBox) -> Vec<adw::ActionRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        rows.push(row.downcast().expect("an action row"));
        index += 1;
    }
    rows
}

fn titles(list: &gtk::ListBox) -> Vec<String> {
    rows(list).iter().map(|r| r.title().to_string()).collect()
}

fn visible_titles(list: &gtk::ListBox) -> Vec<String> {
    rows(list)
        .iter()
        .filter(|r| r.is_child_visible())
        .map(|r| r.title().to_string())
        .collect()
}

pub(super) fn row(list: &gtk::ListBox, name: &str) -> adw::ActionRow {
    rows(list)
        .into_iter()
        .find(|r| r.widget_name() == name)
        .unwrap_or_else(|| panic!("no row {name}"))
}

/// Whether the keyboard focus of `widget`'s window is on `widget`.
fn focused(widget: &impl IsA<gtk::Widget>) -> bool {
    widget
        .root()
        .and_then(|root| root.focus())
        .is_some_and(|f| &f == widget.upcast_ref::<gtk::Widget>())
}

/// The widget at or below `root` whose widget name is `name`.
pub(super) fn named(root: &impl IsA<gtk::Widget>, name: &str) -> Option<gtk::Widget> {
    let mut stack = vec![root.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == name {
            return Some(widget);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    None
}

/// The ＋ button of `main`.
fn plus(main: &window::MainWindow) -> gtk::Button {
    let mut stack = vec![main.window.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Some(button) = widget.downcast_ref::<gtk::Button>()
            && button.tooltip_text().as_deref() == Some("New Tab")
        {
            return button.clone();
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    panic!("no ＋ button");
}

/// ＋ opens a tab called "New Tab" that shows the list: "Local terminal" first
/// (selected, with the focus), then every live profile, plain text. Enter turns
/// that same tab into a local shell. Ctrl+Shift+T does the same as ＋. A change
/// to the profiles shows while the tab is still a list.
pub(super) fn plus_opens_the_new_tab_list_with_local_first(dir: &Path) {
    let mut marked = Profile::new(MARKUP_NAME, "m.example.test", "mark");
    marked.sequences = vec![StartupSequence {
        name: "update".to_owned(),
        commands: vec!["uptime".to_owned()],
    }];
    let nas = nas();
    let ctx = ctx_with(dir, &[nas.clone(), marked.clone()]);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let main = main_window(&app, &ctx);

    plus(&main).emit_clicked();
    pump_for(Duration::from_millis(300));
    assert_eq!(main.view.n_pages(), 2, "＋ adds one tab");
    let page = main.view.selected_page().expect("the new tab is selected");
    assert_eq!(page.title(), new_tab::TITLE);
    assert!(terminal_of(&page).is_none(), "no session yet");
    let list = list_in(&page).expect("the new-tab list");

    let expected: Vec<String> = std::iter::once(new_tab::LOCAL_LABEL.to_owned())
        .chain(ctx.store.borrow().live().iter().map(|p| p.name.clone()))
        .collect();
    assert_eq!(titles(&list), expected);
    let local = row(&list, new_tab::LOCAL_ROW);
    assert_eq!(list.row_at_index(0).as_ref(), Some(local.upcast_ref()));
    assert_eq!(list.selected_row().as_ref(), Some(local.upcast_ref()));
    pump_until("Local terminal to have the focus", || focused(&local));
    let marked_row = row(&list, &marked.id);
    assert_eq!(
        marked_row.subtitle().as_deref(),
        Some(crate::form::subtitle(&marked).as_str())
    );
    assert!(rows(&list).iter().all(|r| !r.uses_markup()));
    let shown: Vec<gtk::Label> = labels_in(&marked_row)
        .into_iter()
        .filter(|l| l.label() == MARKUP_NAME)
        .collect();
    assert_eq!(shown.len(), 1, "the name, literally");
    assert!(!shown[0].uses_markup());

    // The search filters the profiles, by name, host or user; Local terminal
    // stays on top whatever the query. While searching, the first matching
    // profile is selected; with no match, or no query, Local terminal.
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    let selected = || list.selected_row().map(|r| r.widget_name().to_string());
    search.set_text("nas");
    pump_until("the filter to apply", || {
        visible_titles(&list) == [new_tab::LOCAL_LABEL, "NAS box"]
    });
    assert_eq!(selected(), Some(nas.id.clone()), "the matching profile");
    search.set_text("no such thing");
    pump_until("the filter to apply", || {
        visible_titles(&list) == [new_tab::LOCAL_LABEL]
    });
    assert_eq!(selected().as_deref(), Some(new_tab::LOCAL_ROW));
    search.set_text("");
    pump_until("the filter to clear", || visible_titles(&list) == expected);
    assert_eq!(selected().as_deref(), Some(new_tab::LOCAL_ROW));

    // A profile added elsewhere shows while it is a list; the selection stays.
    let mut other = ProfileStore::open(&dir.join("profiles.json")).expect("store");
    other
        .upsert(Profile::new("omega", "o.example.test", "olga"))
        .expect("save");
    ctx.profiles_changed();
    pump_for(Duration::from_millis(200));
    assert!(
        titles(&list).contains(&"omega".to_owned()),
        "{:?}",
        titles(&list)
    );
    assert_eq!(titles(&list)[0], new_tab::LOCAL_LABEL);
    let local = row(&list, new_tab::LOCAL_ROW);
    assert_eq!(list.selected_row().as_ref(), Some(local.upcast_ref()));
    local.grab_focus();

    // Ctrl+Shift+T is the same list, in another new tab.
    WidgetExt::activate_action(&main.window, "win.new-tab", None).expect("win.new-tab");
    pump_for(Duration::from_millis(300));
    assert_eq!(main.view.n_pages(), 3);
    let third = main.view.selected_page().expect("selected");
    assert_ne!(third, page);
    assert_eq!(third.title(), new_tab::TITLE);
    let third_list = list_in(&third).expect("its list");
    assert_eq!(titles(&third_list)[0], new_tab::LOCAL_LABEL);
    assert_eq!(
        third_list
            .selected_row()
            .map(|r| r.widget_name().to_string()),
        Some(new_tab::LOCAL_ROW.to_owned())
    );
    main.view.close_page(&third);
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 2);

    // Enter on the list: Local terminal, in this same tab.
    main.view.set_selected_page(&page);
    pump_for(Duration::from_millis(200));
    row(&list, new_tab::LOCAL_ROW).grab_focus();
    list.emit_by_name::<()>("activate-cursor-row", &[]);
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 2, "no other tab");
    assert!(terminal_of(&page).is_some(), "the tab is now a local shell");
    assert!(list_in(&page).is_none(), "the list is gone");
    assert_ne!(page.title(), new_tab::TITLE);

    // A later change of the profiles leaves the session alone.
    ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert!(terminal_of(&page).is_some());

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A profile picked in the list connects in that same tab: a click (or Enter)
/// just connects; "›" offers the sequences and the transcript box; Enter in the
/// search entry takes the first match.
pub(super) fn picking_a_profile_turns_the_tab_into_the_session(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "new-tab", "echo FAKE-END\nexec sleep 600");
    let mut web = profile(&[("update", &["uptime"])]);
    web.name = "web".to_owned();
    let ctx = ctx_with(dir, &[web.clone(), nas()]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    terminal_tab::set_test_ssh_program(Some(fake));

    // A click on the row.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");
    list.emit_by_name::<()>("row-activated", &[&row(&list, &web.id)]);
    assert_eq!(main.view.n_pages(), 2, "the same tab");
    let terminal = terminal_of(&page).expect("the session");
    assert_eq!(page.title(), tab_title(&web));
    wait_for_text(&terminal, "FAKE-END");
    let text = terminal_tab::terminal_text(&terminal);
    assert!(text.contains("ARG:alice@app.example.test"), "{text}");
    assert!(!text.contains("uptime"), "Just connect: {text}");
    // A stale activation of the old list starts nothing more.
    list.emit_by_name::<()>("row-activated", &[&row(&list, &web.id)]);
    assert_eq!(terminal_of(&page).as_ref(), Some(&terminal));

    // "›" (or Right on the row): the connect choices; the sequence runs.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");
    let more: gtk::MenuButton = find_widget(&row(&list, &web.id)).expect("the › button");
    let popover = more.popover().expect("its popover");
    row(&list, &web.id).grab_focus();
    press_key(&list, gdk::Key::Right);
    pump_until("Right to open the choices", || popover.is_visible());
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());
    assert!(
        terminal_of(&page).is_none(),
        "showing the choices starts nothing"
    );
    more.popup();
    pump_for(Duration::from_millis(300));
    assert!(popover.is_visible());
    let column: gtk::Widget =
        named(&popover, connect_menu::CHOICES_NAME).expect("the connect choices");
    let buttons: Vec<gtk::Button> = {
        let mut found = Vec::new();
        let mut child = column.first_child();
        while let Some(w) = child {
            child = w.next_sibling();
            if let Ok(b) = w.downcast::<gtk::Button>() {
                found.push(b);
            }
        }
        found
    };
    assert_eq!(buttons.len(), 2, "Just connect and update");
    buttons[1].emit_clicked();
    pump_until("the popover to close", || !popover.is_visible());
    assert_eq!(main.view.n_pages(), 3);
    let terminal = terminal_of(&page).expect("the session");
    wait_for_text(&terminal, "FAKE-END");
    let text = terminal_tab::terminal_text(&terminal);
    assert!(text.contains("uptime"), "the sequence: {text}");

    // Enter in the search entry: the first profile it leaves.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    search.set_text("nas");
    pump_until("the filter to apply", || {
        visible_titles(&list) == [new_tab::LOCAL_LABEL, "NAS box"]
    });
    search.emit_activate();
    let terminal = terminal_of(&page).expect("the session");
    assert_eq!(page.title(), "NAS box");
    wait_for_text(&terminal, "ARG:admin@nas.lan");

    // No profile matches: Enter in the search gives a local shell.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    search.set_text("no such thing");
    pump_until("the filter to apply", || {
        visible_titles(&list) == [new_tab::LOCAL_LABEL]
    });
    search.emit_activate();
    let terminal = terminal_of(&page).expect("the session");
    pump_for(Duration::from_millis(300));
    assert!(
        !terminal_tab::terminal_text(&terminal).contains("ARG:"),
        "not the fake ssh"
    );
    assert_ne!(page.title(), "NAS box");
    assert_ne!(page.title(), new_tab::TITLE);

    terminal_tab::set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A profile without startup sequences has connect choices too, behind its "›"
/// or the Right arrow on its row: "Just connect" and the transcript box, checked
/// as the profile says. This is the keyboard route to the connect-time
/// transcript choice. Looking at the choices connects nothing; "Just connect"
/// there connects in that same tab and closes the popover.
pub(super) fn the_new_tab_choices_reach_profiles_without_sequences(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "new-tab-plain", "echo FAKE-END\nexec sleep 600");
    let plain = Profile::new("plain", "plain.example.test", "bob");
    let mut recording = nas();
    recording.transcript = true;
    let ctx = ctx_with(dir, &[plain.clone(), recording.clone()]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    terminal_tab::set_test_ssh_program(Some(fake));

    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");

    // Right on a row opens its choices; a profile that records starts checked.
    let choices_of = |profile: &Profile| -> (gtk::Popover, gtk::Widget) {
        let more: gtk::MenuButton = find_widget(&row(&list, &profile.id)).expect("the › button");
        let popover = more.popover().expect("its popover");
        row(&list, &profile.id).grab_focus();
        press_key(&list, gdk::Key::Right);
        pump_until("Right to open the choices", || popover.is_visible());
        let column = named(&popover, connect_menu::CHOICES_NAME).expect("the connect choices");
        (popover, column)
    };
    let (popover, column) = choices_of(&recording);
    let parts = choices_parts(&column);
    assert_eq!(parts.labels, [("Just connect".to_owned(), false)]);
    assert!(parts.record.is_active(), "the profile records");
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());
    assert!(
        terminal_of(&page).is_none(),
        "showing the choices connects nothing"
    );

    // A profile that does not record starts unchecked; Just connect connects.
    let (popover, column) = choices_of(&plain);
    let parts = choices_parts(&column);
    assert_eq!(parts.labels, [("Just connect".to_owned(), false)]);
    assert!(!parts.record.is_active(), "plain does not record");
    assert!(
        parts.buttons[0].has_focus(),
        "Just connect is ready for Enter"
    );
    parts.buttons[0].emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    assert_eq!(main.view.n_pages(), 2, "the same tab");
    let terminal = terminal_of(&page).expect("the session");
    assert_eq!(page.title(), tab_title(&plain));
    wait_for_text(&terminal, "FAKE-END");
    let text = terminal_tab::terminal_text(&terminal);
    assert!(text.contains("ARG:bob@plain.example.test"), "{text}");

    terminal_tab::set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Enter in the search entry straight after typing picks the profile that was
/// typed, not whatever the list showed before GTK's delayed `search-changed`
/// filtered it, and the selection follows it. (The first row, `alpha-web`, with
/// `zeta-db` typed would log the person into the wrong host with its stored
/// password.) With a query nothing matches, Enter gives a local shell.
pub(super) fn enter_right_after_typing_picks_the_typed_profile(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "new-tab-fast", "echo FAKE-END\nexec sleep 600");
    let alpha = Profile::new("alpha-web", "alpha.example.test", "alice");
    let zeta = Profile::new("zeta-db", "zeta.example.test", "zed");
    let ctx = ctx_with(dir, &[alpha.clone(), zeta.clone()]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    terminal_tab::set_test_ssh_program(Some(fake));

    // "zeta", Enter, no main loop in between.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    assert_eq!(
        titles(&list),
        [new_tab::LOCAL_LABEL, "alpha-web", "zeta-db"],
        "alpha is the first profile row"
    );
    search.set_text("zeta");
    press_enter_in(&search);
    let terminal = terminal_of(&page).expect("the session");
    assert_eq!(page.title(), tab_title(&zeta), "zeta-db, not alpha-web");
    wait_for_text(&terminal, "FAKE-END");
    let text = terminal_tab::terminal_text(&terminal);
    assert!(text.contains("ARG:zed@zeta.example.test"), "{text}");
    assert!(!text.contains("alpha"), "{text}");
    assert_eq!(
        list.selected_row().map(|r| r.widget_name().to_string()),
        Some(zeta.id.clone()),
        "the selection followed the typed query"
    );

    // The same when the loop runs between typing and Enter (the usual case).
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list");
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    search.set_text("zeta");
    pump_until("the filter to apply", || {
        visible_titles(&list) == [new_tab::LOCAL_LABEL, "zeta-db"]
    });
    press_enter_in(&search);
    assert_eq!(page.title(), tab_title(&zeta));

    // A query nothing matches: a local shell, not the first profile.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    search.set_text("no such thing");
    press_enter_in(&search);
    let terminal = terminal_of(&page).expect("the session");
    pump_for(Duration::from_millis(300));
    assert!(
        !terminal_tab::terminal_text(&terminal).contains("ARG:"),
        "not the fake ssh"
    );
    assert_ne!(page.title(), tab_title(&alpha));
    assert_ne!(page.title(), new_tab::TITLE);

    terminal_tab::set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A new tab closed before anything was picked just closes: no transcript, no
/// notification; a profile change afterwards finds nothing to refresh.
pub(super) fn closing_an_unpicked_new_tab_writes_nothing(dir: &Path) {
    let mut recording = nas();
    recording.transcript = true;
    let ctx = ctx_with(dir, &[recording]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let transcripts = default_dir();
    let before = files_in(&transcripts);

    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 2);
    let list = list_in(&page).expect("the list").downgrade();
    main.view.close_page(&page);
    drop(page);
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 1);
    ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert!(list.upgrade().is_none(), "the list is freed with its tab");

    // Picked, the list is freed too, though its tab stays.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(200));
    let list = list_in(&page).expect("the list").downgrade();
    {
        let shown = list.upgrade().expect("the list");
        shown.emit_by_name::<()>("row-activated", &[&row(&shown, new_tab::LOCAL_ROW)]);
    }
    pump_for(Duration::from_millis(200));
    assert!(terminal_of(&page).is_some());
    ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert!(list.upgrade().is_none(), "the list is freed once replaced");
    main.view.close_page(&page);
    pump_for(Duration::from_millis(200));

    // The window's only tab a list: closing the window writes nothing either.
    let lone = window::build(&app, &ctx);
    let local = lone.view.selected_page().expect("its local tab");
    new_tab::open(&lone.view, &ctx);
    lone.view.close_page(&local);
    lone.window.present();
    pump_for(Duration::from_millis(200));
    assert_eq!(lone.view.n_pages(), 1);
    lone.window.close();
    pump_until("the window to close", || !lone.window.is_visible());

    assert_eq!(files_in(&transcripts), before, "nothing was written");
    assert_eq!(ctx.notice.last_text(), None, "nothing was said");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// ☰ → New Window (`app.new-window`) opens another main window, presented,
/// with one local tab.
pub(super) fn new_window_opens_with_one_local_tab(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let first = main_window(&app, &ctx);
    assert!(app.has_action("new-window"));
    app.activate_action("new-window", None);
    pump_until("a second main window", || {
        window::last_active(&app).is_some_and(|m| m.window != first.window)
    });
    let second = window::last_active(&app).expect("the new window");
    assert!(second.window.is_visible());
    assert_eq!(second.view.n_pages(), 1);
    let page = second.view.selected_page().expect("a tab");
    assert!(terminal_of(&page).is_some(), "a local shell");
    assert_eq!(first.view.n_pages(), 1, "the first window is unchanged");

    let made = window::new_window(&app, &ctx);
    assert!(made.window.is_visible());
    assert_eq!(made.view.n_pages(), 1);
    for w in [first, second, made] {
        w.window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// A recording tab moved to another window keeps its session and writes its
/// transcript once, when it ends over there; the window it left stays open
/// with its other tab and has nothing to write.
pub(super) fn moved_recording_tab_writes_once_in_its_new_window(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let b = main_window(&app, &ctx);
    let keeper = a.view.selected_page().expect("A's local tab");
    let fake = fake_ssh(bin, "moved", "echo FAKE-END\nread line\nexit 0");
    let out = dir.join("transcripts");
    let terminal = open_command(
        &a.view,
        "moved",
        fake_command(&fake, &profile(&[]), None, &MemorySecretStore::default()),
        Some(TranscriptSpec {
            host: "app.example.test".to_owned(),
            dir: out.clone(),
            notice: Notice::default(),
        }),
    );
    wait_for_text(&terminal, "FAKE-END");
    let page = super::page_of(&a.view, &terminal);
    assert_eq!(a.view.n_pages(), 2);

    a.view.transfer_page(&page, &b.view, 0);
    pump_for(Duration::from_millis(300));
    assert_eq!(a.view.n_pages(), 1);
    assert_eq!(b.view.n_pages(), 2);
    assert_eq!(b.view.page_position(&page), 0);
    assert_eq!(
        terminal_of(&page).as_ref(),
        Some(&terminal),
        "the same session"
    );
    assert_eq!(page.title(), "moved", "the same title");
    assert!(files_in(&out).is_empty(), "moving writes nothing");

    // A's window closing has nothing of it to write.
    terminal_tab::write_transcripts(&a.view);
    assert!(files_in(&out).is_empty());

    // The session ends by itself in B: written once, and the tab closes.
    terminal.feed_child(b"\n");
    pump_until("the moved tab to close", || b.view.n_pages() == 1);
    pump_for(Duration::from_millis(200));
    let files = files_in(&out);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(
        fs::read_to_string(&files[0])
            .unwrap()
            .contains("FAKE-END\n")
    );
    terminal_tab::write_transcripts(&b.view);
    b.window.close();
    pump_for(Duration::from_millis(200));
    assert_eq!(files_in(&out).len(), 1, "still once");
    assert!(a.window.is_visible(), "A stays open");
    assert_eq!(a.view.n_pages(), 1);
    assert_eq!(a.view.selected_page().as_ref(), Some(&keeper));
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A window whose last tab moves away closes; a tab dropped outside every window
/// gets a new main window of its own (`create-window`), presented.
pub(super) fn moving_the_last_tab_out_closes_the_source_window(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();

    // Into another window.
    let a = main_window(&app, &ctx);
    let b = main_window(&app, &ctx);
    let page = a.view.selected_page().expect("A's only tab");
    let terminal = terminal_of(&page).expect("a local shell");
    a.view.transfer_page(&page, &b.view, 1);
    pump_until("A to close", || !a.window.is_visible());
    assert_eq!(b.view.n_pages(), 2);
    assert_eq!(terminal_of(&page).as_ref(), Some(&terminal));
    assert!(b.window.is_visible());

    // Outside every window: a new main window gets it.
    let made: Option<adw::TabView> = b.view.emit_by_name("create-window", &[]);
    let made = made.expect("a view for the dropped tab");
    // Emitted from Rust, the signal drops a reference the handler never gave:
    // libadwaita's marshaller takes the returned view as owned, which balances
    // only for its own (C) emission at a drop. Keep one back for it.
    std::mem::forget(made.clone());
    assert_eq!(made.n_pages(), 0);
    b.view.transfer_page(&page, &made, 0);
    pump_for(Duration::from_millis(300));
    let window = made
        .root()
        .and_downcast::<gtk::Window>()
        .expect("the view is in a window");
    assert!(window.is_visible(), "the new window is presented");
    assert_ne!(&window, b.window.upcast_ref::<gtk::Window>());
    assert_eq!(made.n_pages(), 1);
    assert_eq!(
        terminal_of(&page).as_ref(),
        Some(&terminal),
        "the same session"
    );
    let found = app
        .windows()
        .into_iter()
        .find(|w| w == &window)
        .expect("a window of the app");
    assert_eq!(found.widget_name(), b.window.widget_name(), "a main window");
    assert_eq!(b.view.n_pages(), 1);

    // Its last tab gone, that one closes too.
    made.transfer_page(&page, &b.view, 0);
    pump_until("the made window to close", || !window.is_visible());
    assert_eq!(b.view.n_pages(), 2);
    b.window.destroy();
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty());
}

/// The Connection Profiles window closes with the last main window, so the app quits; not
/// before.
pub(super) fn closing_the_last_main_window_closes_the_profiles_window(dir: &Path) {
    let ctx = ctx_with(dir, &[nas()]);
    let app = test_app();
    window::close_side_windows_with_last_main_window(&app);
    let a = main_window(&app, &ctx);
    let b = main_window(&app, &ctx);
    let profiles_window = window::open_profiles(&app, &ctx);
    pump_for(Duration::from_millis(300));
    assert!(profiles_window.is_visible());

    a.window.close();
    pump_for(Duration::from_millis(300));
    assert!(!a.window.is_visible());
    assert!(profiles_window.is_visible(), "a main window is left");

    // Closing the Connection Profiles window alone closes nothing else.
    profiles_window.close();
    pump_for(Duration::from_millis(200));
    assert!(b.window.is_visible());
    let profiles_window = window::open_profiles(&app, &ctx);
    pump_for(Duration::from_millis(300));

    b.window.close();
    pump_until("the Connection Profiles window to close", || {
        !profiles_window.is_visible()
    });
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty(), "nothing keeps the app running");
}

/// Each shortcut the Keyboard Shortcuts dialog lists is the accelerator of an
/// action of the main window or the app, and the actions do what they say. The
/// tab view keeps only Ctrl+Tab and Ctrl+Shift+Tab of its own shortcuts.
pub(super) fn shortcut_actions_exist(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    crate::set_accels(&app);
    let main = main_window(&app, &ctx);

    let mut seen = Vec::new();
    for (_, items) in crate::SHORTCUTS {
        for (name, action, accel) in *items {
            let (scope, short) = action.split_once('.').expect("scope.name");
            match scope {
                "win" => assert!(
                    main.window.lookup_action(short).is_some(),
                    "{name}: {action} on the main window"
                ),
                "app" => assert!(app.has_action(short), "{name}: {action} on the app"),
                other => panic!("{name}: unexpected scope {other}"),
            }
            // Compared parsed: GTK writes the modifiers in its own order.
            let parse = |a: &str| gtk::accelerator_parse(a).expect("an accelerator");
            let set: Vec<_> = app
                .accels_for_action(action)
                .iter()
                .map(|a| parse(a))
                .collect();
            let listed: Vec<_> = accel.split_whitespace().map(parse).collect();
            assert_eq!(set, listed, "{name}: the accelerators of {action}");
            seen.push(*action);
        }
    }
    for action in [
        "win.new-tab",
        "win.close-tab",
        "win.next-tab",
        "win.previous-tab",
        "win.copy",
        "win.paste",
        "win.zoom-in",
        "win.zoom-out",
        "win.zoom-reset",
        "app.new-window",
    ] {
        assert!(seen.contains(&action), "{action} is listed");
    }
    assert_eq!(
        main.view.shortcuts(),
        adw::TabViewShortcuts::NONE,
        "the tab view takes no key from the terminal"
    );

    // The tab actions.
    let first = main.view.selected_page().expect("a tab");
    WidgetExt::activate_action(&main.window, "win.new-tab", None).unwrap();
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 2);
    let second = main.view.selected_page().expect("the new tab");
    assert!(list_in(&second).is_some(), "the new-tab list");
    WidgetExt::activate_action(&main.window, "win.previous-tab", None).unwrap();
    assert_eq!(main.view.selected_page().as_ref(), Some(&first));
    WidgetExt::activate_action(&main.window, "win.next-tab", None).unwrap();
    assert_eq!(main.view.selected_page().as_ref(), Some(&second));
    WidgetExt::activate_action(&main.window, "win.close-tab", None).unwrap();
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 1);
    assert_eq!(main.view.selected_page().as_ref(), Some(&first));
    WidgetExt::activate_action(&main.window, "win.close-tab", None).unwrap();
    pump_until("the window to close with its last tab", || {
        !main.window.is_visible()
    });
    pump_for(Duration::from_millis(200));
}

// ----- text size -----

/// The text size the terminal of `page` draws at.
fn drawn_scale(page: &adw::TabPage) -> f64 {
    terminal_of(page).expect("a terminal").font_scale()
}

/// Run the window action `win.<name>` of `main`, as its accelerator does.
fn act(main: &window::MainWindow, name: &str) {
    WidgetExt::activate_action(&main.window, &format!("win.{name}"), None)
        .unwrap_or_else(|err| panic!("win.{name}: {err}"));
}

/// More presses of zoom-in than it takes to go from the smallest to the
/// largest size.
const PRESSES: usize = 32;

/// Step the selected tab of `main` up until it is at `scale` (a step of the
/// table in `zoom`).
fn zoom_to(main: &window::MainWindow, scale: f64) {
    let page = main.view.selected_page().expect("a tab");
    for _ in 0..PRESSES {
        if drawn_scale(&page) >= scale {
            break;
        }
        act(main, "zoom-in");
    }
    assert_eq!(drawn_scale(&page), scale, "{scale} is a step");
}

/// The percent button of the zoom row in the ☰ menu of `main`, and the − and
/// + buttons beside it.
fn zoom_buttons(main: &window::MainWindow) -> (gtk::Button, gtk::Button, gtk::Button) {
    let popover = menu_button(&main.window, "Main Menu")
        .popover()
        .expect("the ☰ popover");
    let button = |tooltip: &str| {
        all_widgets::<gtk::Button>(&popover)
            .into_iter()
            .find(|b| b.tooltip_text().as_deref() == Some(tooltip))
            .unwrap_or_else(|| panic!("no {tooltip:?} button in the ☰ menu"))
    };
    (
        button("Smaller Text"),
        all_widgets::<gtk::Button>(&popover)
            .into_iter()
            .find(|b| b.widget_name() == window::ZOOM_PERCENT_NAME)
            .expect("the percent button"),
        button("Larger Text"),
    )
}

/// Zoom acts on the selected tab only: two zoom-ins make the first tab 1.2
/// and leave the second at 1.0; the tabs keep their own sizes as the selection
/// moves; reset puts the selected one back to normal. While the selected tab
/// shows no terminal (the new-tab list) the actions are off.
pub(super) fn zoom_is_per_tab(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the first tab");
    assert_eq!(drawn_scale(&a), zoom::DEFAULT);
    terminal_tab::open_local_shell(&main.view, zoom::DEFAULT, None);
    let b = main.view.selected_page().expect("the second tab");
    assert_ne!(a, b);

    main.view.set_selected_page(&a);
    act(&main, "zoom-in");
    act(&main, "zoom-in");
    assert_eq!(drawn_scale(&a), 1.2);
    assert_eq!(terminal_tab::scale_of(&a), Some(1.2));
    assert_eq!(drawn_scale(&b), 1.0, "the other tab is not touched");
    assert_eq!(terminal_tab::scale_of(&b), Some(1.0));

    main.view.set_selected_page(&b);
    act(&main, "zoom-out");
    assert_eq!(drawn_scale(&b), 0.9);
    assert_eq!(drawn_scale(&a), 1.2, "and neither is this one");

    main.view.set_selected_page(&a);
    act(&main, "zoom-reset");
    assert_eq!(drawn_scale(&a), zoom::DEFAULT);
    assert_eq!(drawn_scale(&b), 0.9);

    // The ends: the smallest and the largest size are where the steps stop.
    for _ in 0..PRESSES {
        act(&main, "zoom-out");
    }
    assert_eq!(drawn_scale(&a), zoom::MIN);
    for _ in 0..PRESSES {
        act(&main, "zoom-in");
    }
    assert_eq!(drawn_scale(&a), zoom::MAX);
    act(&main, "zoom-reset");

    // A tab that shows the new-tab list has no terminal: the actions are off,
    // so their keys stay with the list's search entry.
    act(&main, "new-tab");
    pump_for(Duration::from_millis(200));
    let list_tab = main.view.selected_page().expect("the new-tab list");
    assert!(terminal_of(&list_tab).is_none());
    for name in ["zoom-in", "zoom-out", "zoom-reset"] {
        let action = main
            .window
            .lookup_action(name)
            .unwrap_or_else(|| panic!("win.{name}"));
        assert!(!action.is_enabled(), "win.{name} with no terminal");
    }
    main.view.set_selected_page(&a);
    for name in ["zoom-in", "zoom-out", "zoom-reset"] {
        let action = main.window.lookup_action(name).expect("the action");
        assert!(action.is_enabled(), "win.{name} with a terminal");
    }

    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A tab opened in a window starts at the size of the tab that was selected
/// there: from ＋ and the new-tab list, and from the Connection Profiles window's connect.
pub(super) fn a_new_tab_starts_at_the_active_tabs_size(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let first = main.view.selected_page().expect("the first tab");
    zoom_to(&main, 1.3);

    // ＋: the list has the size already, and the session picked from it too.
    plus(&main).emit_clicked();
    pump_for(Duration::from_millis(300));
    let page = main.view.selected_page().expect("the new tab");
    assert_ne!(page, first);
    assert!(terminal_of(&page).is_none(), "the list");
    assert_eq!(terminal_tab::scale_of(&page), Some(1.3));
    let list = list_in(&page).expect("the new-tab list");
    row(&list, new_tab::LOCAL_ROW).grab_focus();
    list.emit_by_name::<()>("activate-cursor-row", &[]);
    pump_for(Duration::from_millis(300));
    assert_eq!(drawn_scale(&page), 1.3, "the shell drawn at the size");
    assert_eq!(drawn_scale(&first), 1.3);

    // The new tab is its own: zooming it leaves the first alone.
    act(&main, "zoom-in");
    assert_eq!(drawn_scale(&page), 1.5);
    assert_eq!(drawn_scale(&first), 1.3);

    // A tab from the Connection Profiles window's connect takes the selected tab's size.
    let p = profile(&[]);
    set_test_ssh_program_for(bin, "zoom-ssh");
    let connect = window::connect_fn(&app, &ctx);
    connect(&p, None, false);
    pump_for(Duration::from_millis(300));
    let connected = main.view.selected_page().expect("the session tab");
    assert_ne!(connected, page);
    assert_eq!(drawn_scale(&connected), 1.5);
    terminal_tab::set_test_ssh_program(None);

    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// Make the sessions the app starts run a fake `ssh` called `name` in `bin`.
fn set_test_ssh_program_for(bin: &Path, name: &str) {
    let fake = fake_ssh(bin, name, "echo FAKE-END\nexec sleep 600");
    terminal_tab::set_test_ssh_program(Some(fake));
}

/// The ☰ menu's first row shows the selected tab's size as a percent, follows
/// it as the size and the tab change, and its buttons make the tab larger,
/// smaller and normal.
pub(super) fn the_menu_row_shows_and_resets_the_size(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the first tab");
    let popover = open_popover(&menu_button(&main.window, "Main Menu"));
    let (smaller, percent, larger) = zoom_buttons(&main);
    assert_eq!(percent.label().as_deref(), Some("100%"));

    // The row follows a change made by the keys.
    act(&main, "zoom-in");
    assert_eq!(percent.label().as_deref(), Some("110%"));
    assert_eq!(drawn_scale(&a), 1.1);

    // Its buttons are the actions.
    larger.emit_clicked();
    assert_eq!(percent.label().as_deref(), Some("120%"));
    assert_eq!(drawn_scale(&a), 1.2);
    smaller.emit_clicked();
    smaller.emit_clicked();
    assert_eq!(percent.label().as_deref(), Some("100%"));
    larger.emit_clicked();
    larger.emit_clicked();
    larger.emit_clicked();
    assert_eq!(percent.label().as_deref(), Some("130%"));
    percent.emit_clicked();
    assert_eq!(percent.label().as_deref(), Some("100%"));
    assert_eq!(drawn_scale(&a), zoom::DEFAULT, "the percent button resets");
    close_popover(&popover);

    // Another tab has its own size, and the row shows whichever is selected.
    terminal_tab::open_local_shell(&main.view, zoom::DEFAULT, None);
    let b = main.view.selected_page().expect("the second tab");
    zoom_to(&main, 1.5);
    assert_eq!(percent.label().as_deref(), Some("150%"));
    main.view.set_selected_page(&a);
    assert_eq!(percent.label().as_deref(), Some("100%"));
    main.view.set_selected_page(&b);
    assert_eq!(percent.label().as_deref(), Some("150%"));

    // The new-tab list has no terminal: it shows the size the tab will start
    // at, and the buttons are off.
    act(&main, "new-tab");
    pump_for(Duration::from_millis(200));
    assert_eq!(percent.label().as_deref(), Some("150%"));
    for button in [&smaller, &percent, &larger] {
        assert!(!button.is_sensitive(), "no terminal to zoom");
    }
    main.view.set_selected_page(&b);
    for button in [&smaller, &percent, &larger] {
        assert!(button.is_sensitive(), "a terminal to zoom");
    }

    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// What the last window to close saves: its active tab's size, or the last
/// remaining tab's if its tabs were closed one by one. Windows that are not
/// the last save nothing.
pub(super) fn quitting_saves_the_last_windows_active_tab(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let path = ctx.settings_path.clone();
    let app = test_app();
    let a = main_window(&app, &ctx);
    zoom_to(&a, 1.2);
    let b = main_window(&app, &ctx);
    zoom_to(&b, 2.0);
    let b_first = b.view.selected_page().expect("B's first tab");
    // The second tab starts at the first one's size; two steps down.
    terminal_tab::open_local_shell(&b.view, zoom::DEFAULT, None);
    act(&b, "zoom-out");
    act(&b, "zoom-out");
    let b_second = b.view.selected_page().expect("B's second tab");
    assert_eq!(drawn_scale(&b_first), 2.0);
    assert_eq!(drawn_scale(&b_second), 1.5, "B's active tab");

    // A is not the last window: nothing is saved yet.
    a.window.close();
    pump_for(Duration::from_millis(300));
    assert!(!path.exists(), "no file yet");
    assert!(b.window.is_visible());

    // B's active tab is the second one, at 1.5 (the first is at 2.0).
    b.window.close();
    pump_for(Duration::from_millis(300));
    assert_eq!(settings::load(&path).font_scale, 1.5);
    assert!(app.windows().is_empty());

    // Tabs closed one by one: the last remaining tab's size counts, not the
    // size of the one closed before it.
    let c = main_window(&app, &ctx);
    zoom_to(&c, 1.7);
    terminal_tab::open_local_shell(&c.view, zoom::DEFAULT, None);
    act(&c, "zoom-out");
    act(&c, "zoom-out");
    assert_eq!(drawn_scale(&c.view.selected_page().unwrap()), 1.3);
    act(&c, "close-tab");
    pump_for(Duration::from_millis(300));
    assert_eq!(c.view.n_pages(), 1);
    assert!(c.window.is_visible());
    assert_eq!(settings::load(&path).font_scale, 1.5, "not saved before");
    act(&c, "close-tab");
    pump_until("the window to close with its last tab", || {
        !c.window.is_visible()
    });
    assert_eq!(settings::load(&path).font_scale, 1.7);

    // The file is private.
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

/// The launch starts its first tab at the saved size (a damaged file is
/// clamped), and so does any window with no tab to take a size from: a new
/// window, and a window the Connection Profiles window opens when none is left.
pub(super) fn launch_starts_at_the_saved_size(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    fs::write(&ctx.settings_path, r#"{"font_scale": 1.5}"#).expect("write the settings");
    crate::launch(&app, &ctx);
    pump_for(Duration::from_millis(400));
    let first = window::last_active(&app).expect("the launch window");
    let page = first.view.selected_page().expect("its tab");
    assert_eq!(drawn_scale(&page), 1.5, "the launch tab");
    // The ☰ row says so too.
    let (_, percent, _) = zoom_buttons(&first);
    assert_eq!(percent.label().as_deref(), Some("150%"));

    // Zooming it changes nothing for a window opened later: that one starts at
    // the saved size, which only quitting changes.
    act(&first, "zoom-in");
    let second = window::new_window(&app, &ctx);
    pump_for(Duration::from_millis(300));
    let second_page = second.view.selected_page().expect("its tab");
    assert_eq!(drawn_scale(&second_page), 1.5, "a new window");
    assert_eq!(drawn_scale(&page), 1.7);

    // A file with a size out of range is brought into range.
    fs::write(&ctx.settings_path, r#"{"font_scale": 100.0}"#).expect("write the settings");
    let third = window::new_window(&app, &ctx);
    pump_for(Duration::from_millis(300));
    assert_eq!(drawn_scale(&third.view.selected_page().unwrap()), zoom::MAX);
    first.window.destroy();
    second.window.destroy();
    third.window.destroy();
    pump_for(Duration::from_millis(300));
    assert!(window::last_active(&app).is_none());

    // The Connection Profiles window connecting with no main window left opens one at
    // the saved size.
    fs::write(&ctx.settings_path, r#"{"font_scale": 0.8}"#).expect("write the settings");
    set_test_ssh_program_for(bin, "launch-ssh");
    let connect = window::connect_fn(&app, &ctx);
    connect(&profile(&[]), None, false);
    pump_for(Duration::from_millis(400));
    let made = window::last_active(&app).expect("a window made for the session");
    assert_eq!(drawn_scale(&made.view.selected_page().unwrap()), 0.8);
    terminal_tab::set_test_ssh_program(None);
    made.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Ctrl+plus (also Ctrl+= and keypad +), Ctrl+minus (keypad −) and Ctrl+0
/// (keypad 0) are the accelerators of the zoom actions, one line each in the
/// Keyboard Shortcuts table.
pub(super) fn zoom_shortcuts_exist() {
    let app = test_app();
    crate::set_accels(&app);
    let parse = |a: &str| gtk::accelerator_parse(a).expect("an accelerator");
    for (action, accels) in [
        (
            "win.zoom-in",
            ["<Control>plus", "<Control>equal", "<Control>KP_Add"].as_slice(),
        ),
        (
            "win.zoom-out",
            ["<Control>minus", "<Control>KP_Subtract"].as_slice(),
        ),
        ("win.zoom-reset", ["<Control>0", "<Control>KP_0"].as_slice()),
    ] {
        let set: Vec<_> = app
            .accels_for_action(action)
            .iter()
            .map(|a| parse(a))
            .collect();
        let expected: Vec<_> = accels.iter().map(|a| parse(a)).collect();
        assert_eq!(set, expected, "the accelerators of {action}");
        let lines: Vec<_> = crate::SHORTCUTS
            .iter()
            .flat_map(|(_, items)| items.iter())
            .filter(|(_, a, _)| *a == action)
            .collect();
        assert_eq!(lines.len(), 1, "one line for {action} in the dialog");
        assert_eq!(
            lines[0].2.split_whitespace().map(parse).collect::<Vec<_>>(),
            expected,
            "the dialog lists the same keys for {action}"
        );
    }
}
