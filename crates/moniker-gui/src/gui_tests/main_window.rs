//! Scenarios: the main window (header bar, ☰ menu, About), the connect choices
//! ([`connect_menu::choices`], which the new-tab list shows behind its "›") and
//! where a session started from the Connection Profiles window opens.
//!
//! The choices are tested on their own with callbacks that only record what
//! they were asked to do, so nothing connects anywhere. Where a session really
//! starts, it is a fake `ssh`.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib::translate::IntoGlib;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::{Profile, StartupSequence};
use moniker_core::secrets::{MemorySecretStore, SecretStore};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;

use super::windows::main_window;
use super::{
    MARKUP_NAME, all_widgets, button_labeled, fake_shell, fake_ssh, files_in, find_widget, fixture,
    labels_in, make_helper, pane, pump_for, pump_until, shown, wait_for_text,
};
use crate::connect_menu::{self, RECORD_LABEL};
use crate::context::AppContext;
use crate::launch_args::{Refused, Run, Start, decode, encode};
use crate::terminal_tab::{
    launch, new_holder_page, rename_page, start_command, tab_title, terminal_of,
};
use crate::{ABOUT_COPYRIGHT, REPOSITORY_URL, preferences, profiles, window};

fn sequence(name: &str) -> StartupSequence {
    StartupSequence {
        name: name.to_owned(),
        commands: vec!["uptime".to_owned()],
    }
}

fn web() -> Profile {
    Profile::new("web", "web.example.test", "alice")
}

/// A context over a store in `dir` holding `profiles`, an in-memory keyring and
/// a stand-in askpass helper.
pub(super) fn ctx_with(dir: &Path, profiles: &[Profile]) -> Rc<AppContext> {
    ctx_with_secrets(dir, profiles, Arc::new(MemorySecretStore::default()))
}

/// [`ctx_with`], with `secrets` as the keyring.
pub(super) fn ctx_with_secrets(
    dir: &Path,
    profiles: &[Profile],
    secrets: Arc<dyn SecretStore>,
) -> Rc<AppContext> {
    fs::create_dir_all(dir).expect("dir");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("store");
    for p in profiles {
        store.upsert(p.clone()).expect("save");
    }
    Rc::new(AppContext::with_parts(store, secrets, make_helper(dir)))
}

/// A registered application to hang main windows on, as the real one does.
pub(super) fn test_app() -> adw::Application {
    let app = adw::Application::builder()
        .application_id("org.moniker.GuiTest.Main")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gio::Cancellable::NONE)
        .expect("register the test application");
    app
}

/// Every widget of type `T` at or below `root`, in tree order, leaving out
/// whatever sits in a popover (a popover belongs to its button, not to the
/// window's layout).
fn widgets_outside_popovers<T: IsA<gtk::Widget>>(root: &impl IsA<gtk::Widget>) -> Vec<T> {
    fn walk<T: IsA<gtk::Widget>>(widget: &gtk::Widget, found: &mut Vec<T>) {
        if widget.is::<gtk::Popover>() {
            return;
        }
        if let Ok(hit) = widget.clone().downcast::<T>() {
            found.push(hit);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            walk(&c, found);
            child = c.next_sibling();
        }
    }
    let mut found = Vec::new();
    walk(root.upcast_ref(), &mut found);
    found
}

/// (label, action) of every item of `menu`, sections flattened, in order.
pub(super) fn menu_items(menu: &gio::MenuModel) -> Vec<(String, String)> {
    let mut items = Vec::new();
    for i in 0..menu.n_items() {
        if let Some(section) = menu.item_link(i, gio::MENU_LINK_SECTION) {
            items.extend(menu_items(&section));
            continue;
        }
        let attribute = |name: &str| {
            menu.item_attribute_value(i, name, None)
                .and_then(|v| v.get::<String>())
                .unwrap_or_default()
        };
        // A custom item (the zoom row) has no label or action; the widget that
        // takes its place is named by its `custom` attribute.
        let custom = attribute("custom");
        let action = if custom.is_empty() {
            attribute(gio::MENU_ATTRIBUTE_ACTION)
        } else {
            format!("custom:{custom}")
        };
        items.push((attribute(gio::MENU_ATTRIBUTE_LABEL), action));
    }
    items
}

/// Press `key` on whatever key controller `widget` has, as the keyboard does.
pub(super) fn press_key(widget: &impl IsA<gtk::Widget>, key: gdk::Key) {
    let controllers = widget.observe_controllers();
    let controller = (0..controllers.n_items())
        .filter_map(|i| {
            controllers
                .item(i)
                .and_downcast::<gtk::EventControllerKey>()
        })
        .next()
        .expect("a key controller");
    let handled = controller.emit_by_name::<bool>(
        "key-pressed",
        &[&key.into_glib(), &0u32, &gdk::ModifierType::empty()],
    );
    assert!(handled, "the key {key:?} is handled");
    pump_for(Duration::from_millis(100));
}

/// What a connect-choices widget shows: the entry buttons' labels (with
/// whether they use markup), the buttons themselves, and the transcript box.
pub(super) struct Choices {
    pub(super) labels: Vec<(String, bool)>,
    pub(super) buttons: Vec<gtk::Button>,
    pub(super) record: gtk::CheckButton,
}

pub(super) fn choices_parts(widget: &gtk::Widget) -> Choices {
    let column = widget.clone().downcast::<gtk::Box>().expect("a box");
    let mut buttons = Vec::new();
    let mut record = None;
    let mut separators = 0;
    let mut child = column.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            assert!(record.is_none() && separators == 0, "buttons come first");
            buttons.push(button.clone());
        } else if widget.is::<gtk::Separator>() {
            separators += 1;
        } else if let Ok(check) = widget.downcast::<gtk::CheckButton>() {
            assert!(record.replace(check).is_none(), "one check box");
        } else {
            panic!("unexpected widget in the connect choices");
        }
    }
    assert_eq!(separators, 1, "a separator sets the check box apart");
    let labels = buttons
        .iter()
        .map(|b| {
            let l = b.child().and_downcast::<gtk::Label>().expect("a label");
            (l.label().to_string(), l.uses_markup())
        })
        .collect();
    let record = record.expect("the transcript check box comes last");
    assert_eq!(record.label().as_deref(), Some(RECORD_LABEL));
    Choices {
        labels,
        buttons,
        record,
    }
}

/// What a connect-choices widget was asked for: (sequence, record).
type Picked = Rc<RefCell<Vec<(Option<usize>, bool)>>>;

/// Launch: one window, one local tab, no profile column; the header bar has
/// ＋, the tab's title and ☰, whose entries are the app's actions.
pub(super) fn launch_has_one_local_tab_and_no_sidebar(dir: &Path) {
    let ctx = ctx_with(dir, &[web()]);
    let app = test_app();
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(400));

    assert_eq!(main.view.n_pages(), 1, "one tab");
    let page = main.view.selected_page().expect("a selected tab");
    assert!(terminal_of(&page).is_some(), "the tab is a local terminal");
    assert!(
        widgets_outside_popovers::<gtk::ListBox>(&main.window).is_empty(),
        "no profile column"
    );
    let bar: adw::TabBar = find_widget(&main.window).expect("the tab bar");
    assert!(bar.is_autohide(), "a single tab shows no tab bar");
    assert_eq!(bar.view().as_ref(), Some(&main.view));
    assert_eq!(
        main.notifications.widget().child().as_ref(),
        Some(main.view.upcast_ref::<gtk::Widget>()),
        "the notifications overlay the tabs"
    );

    // The header bar.
    let header: adw::HeaderBar = find_widget(&main.window).expect("the header bar");
    let buttons = widgets_outside_popovers::<gtk::Button>(&header);
    let new_tab = buttons
        .iter()
        .find(|b| b.tooltip_text().as_deref() == Some("New Tab"))
        .expect("the ＋ button");
    let menus = widgets_outside_popovers::<gtk::MenuButton>(&header);
    let main_menu = menus
        .iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Main Menu"))
        .expect("the ☰ button");
    assert!(
        !main_menu.is_primary(),
        "F10 stays with the terminal (htop, mc)"
    );
    let model = main_menu.menu_model().expect("the main menu");
    assert_eq!(
        menu_items(&model),
        [
            (String::new(), "custom:zoom".to_owned()),
            ("New Window".to_owned(), "app.new-window".to_owned()),
            ("Connection Profiles".to_owned(), "app.profiles".to_owned()),
            ("Transcripts".to_owned(), "app.recordings".to_owned()),
            ("Sync Now".to_owned(), "app.sync-now".to_owned()),
            ("Preferences".to_owned(), "app.preferences".to_owned()),
            ("Keyboard Shortcuts".to_owned(), "app.shortcuts".to_owned()),
            ("About Moniker".to_owned(), "app.about".to_owned()),
        ]
    );

    // ＋ opens another tab here, showing the new-tab list.
    new_tab.emit_clicked();
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), 2);
    let second = main.view.selected_page().expect("the new tab is selected");
    assert!(terminal_of(&second).is_none(), "a list, no session yet");
    assert_eq!(second.title(), crate::new_tab::TITLE);

    // The window title is the selected tab's title, literally.
    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    rename_page(&second, MARKUP_NAME);
    pump_for(Duration::from_millis(100));
    assert_eq!(title.title(), MARKUP_NAME);
    let shown = labels_in(&title)
        .into_iter()
        .find(|l| l.label() == MARKUP_NAME)
        .expect("the title's label");
    assert!(!shown.uses_markup());
    main.view.set_selected_page(&page);
    pump_for(Duration::from_millis(100));
    assert_eq!(title.title(), page.title());
    assert_eq!(main.window.title().as_deref(), Some(page.title().as_str()));

    // Notifications land in this window; the app finds it as its main window.
    ctx.notice.show("hello");
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&main.notifications));
    let found = window::last_active(&app).expect("a main window");
    assert_eq!(found.window, main.window);
    assert_eq!(found.view, main.view);
    assert_eq!(found.notifications, main.notifications);

    // The menu's actions exist and do what they say.
    crate::add_actions(&app, &ctx);
    for action in ["new-window", "profiles", "recordings", "shortcuts", "about"] {
        assert!(app.has_action(action), "app.{action}");
    }
    // Sync Now exists only while there is a sync partner (none here), so that
    // its menu item is hidden; the sync scenarios cover it.
    assert!(!app.has_action("sync-now"));
    app.activate_action("profiles", None);
    pump_for(Duration::from_millis(300));
    let profiles_window = app
        .windows()
        .into_iter()
        .find(|w| w.title().as_deref() == Some("Connection Profiles"))
        .expect("the Connection Profiles window");
    assert!(
        window::last_active(&app).is_some_and(|m| m.window == main.window),
        "the Connection Profiles window is no main window"
    );
    profiles_window.close();
    pump_for(Duration::from_millis(200));

    main.window.present();
    pump_for(Duration::from_millis(200));
    app.activate_action("about", None);
    pump_until("the About dialog", || {
        find_widget::<adw::AboutDialog>(&main.window).is_some()
    });
    let about: adw::AboutDialog = find_widget(&main.window).expect("the About dialog");
    assert_eq!(about.application_name(), "Moniker");
    assert_eq!(about.version(), env!("CARGO_PKG_VERSION"));
    assert_eq!(about.copyright(), ABOUT_COPYRIGHT);
    assert_eq!(about.license_type(), gtk::License::MitX11);
    about.close();
    pump_for(Duration::from_millis(300));

    app.activate_action("shortcuts", None);
    pump_until("the Keyboard Shortcuts dialog", || {
        find_widget::<adw::ShortcutsDialog>(&main.window).is_some()
    });
    let shortcuts: adw::ShortcutsDialog = find_widget(&main.window).expect("the dialog");
    let listed: Vec<String> = all_widgets::<adw::ShortcutLabel>(&shortcuts)
        .iter()
        .map(|l| l.accelerator().to_string())
        .collect();
    for (_, items) in crate::SHORTCUTS {
        for (_, _, accel) in *items {
            assert!(listed.iter().any(|l| l == accel), "{accel} in {listed:?}");
        }
    }
    shortcuts.close();
    pump_for(Duration::from_millis(300));

    main.window.close();
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty());
}

/// The connect choices: "Just connect", each sequence, and the transcript box
/// (defaulting to the profile's setting), for profiles with and without
/// sequences; and the choice made there decides whether a transcript is
/// written. (The new-tab list shows them behind a profile's "›":
/// `windows::the_new_tab_choices_reach_profiles_without_sequences`.)
pub(super) fn the_connect_choices_offer_sequences_and_transcript(bin: &Path, dir: &Path) {
    the_choices_widget_offers_each_sequence();
    the_connect_time_choice_decides_whether_a_transcript_is_written(bin, dir);
}

/// [`connect_menu::choices`] on its own: its buttons and box.
fn the_choices_widget_offers_each_sequence() {
    let fx = fixture();
    let mut p = web();
    p.sequences = vec![sequence("update"), sequence("<b>bold</b> & _x_")];
    let chosen: Picked = Rc::default();
    let record_choice = || {
        let chosen = Rc::clone(&chosen);
        move |choice, record| chosen.borrow_mut().push((choice, record))
    };
    let widget = connect_menu::choices(&p, p.transcript, record_choice());
    fx.view.append(&widget);
    pump_for(Duration::from_millis(200));

    let parts = choices_parts(&widget);
    assert_eq!(
        parts.labels,
        [
            ("Just connect".to_owned(), false),
            ("update".to_owned(), false),
            ("<b>bold</b> & _x_".to_owned(), false),
        ]
    );
    assert!(!p.transcript);
    assert!(!parts.record.is_active(), "the profile does not record");
    parts.buttons[2].emit_clicked();
    parts.buttons[0].emit_clicked();
    parts.buttons[1].emit_clicked();
    assert_eq!(
        *chosen.borrow(),
        [(Some(1), false), (None, false), (Some(0), false)]
    );

    // Checking the box overrides the profile (off -> record).
    chosen.borrow_mut().clear();
    parts.record.set_active(true);
    parts.buttons[1].emit_clicked();
    parts.buttons[0].emit_clicked();
    assert_eq!(*chosen.borrow(), [(Some(0), true), (None, true)]);

    // A profile that records starts checked; unchecking overrides it, and the box
    // is read when a button is clicked.
    let mut recording = p.clone();
    recording.transcript = true;
    chosen.borrow_mut().clear();
    let widget = connect_menu::choices(&recording, recording.transcript, record_choice());
    fx.view.append(&widget);
    let parts = choices_parts(&widget);
    assert!(parts.record.is_active(), "the profile records");
    parts.buttons[0].emit_clicked();
    parts.record.set_active(false);
    parts.buttons[0].emit_clicked();
    assert_eq!(*chosen.borrow(), [(None, true), (None, false)]);

    // No sequences: Just connect and the box.
    let widget = connect_menu::choices(&web(), false, |_, _| {});
    let parts = choices_parts(&widget);
    assert_eq!(parts.labels, [("Just connect".to_owned(), false)]);
    assert!(!parts.record.is_active());
    fx.window.destroy();
}

/// A session recorded or not, as the box says at connect time, whatever the
/// profile's own setting: the file is there (or not) after the tab closes.
fn the_connect_time_choice_decides_whether_a_transcript_is_written(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "override", "echo FAKE-END\nexec sleep 600");
    let ctx = ctx_with(dir, &[]);

    // (profile setting, box at connect time, a file is expected)
    for (profile_setting, record, expect_file) in [
        (false, true, true),   // the profile does not record, recorded anyway
        (true, false, false),  // the profile records, not this time
        (true, true, true),    // as the profile says
        (false, false, false), // as the profile says
    ] {
        let case = dir.join(format!("t-{profile_setting}-{record}"));
        let mut p = web();
        p.transcript = profile_setting;
        let chosen: Picked = Rc::default();
        let widget = connect_menu::choices(&p, p.transcript, {
            let chosen = Rc::clone(&chosen);
            move |choice, record| chosen.borrow_mut().push((choice, record))
        });
        let parts = choices_parts(&widget);
        parts.record.set_active(record);
        parts.buttons[0].emit_clicked();
        let (sequence, record) = chosen.borrow_mut().pop().expect("a choice");

        let fx = fixture();
        let page = new_holder_page(&fx.view, "x");
        let mut launched = launch(&pane(&page), &p, sequence, record, &ctx, &case, false);
        launched.command.program = fake.clone();
        assert_eq!(launched.transcript.is_some(), expect_file);

        let terminal = start_command(
            &pane(&page),
            &launched.title,
            launched.command,
            launched.transcript,
            launched.marker,
        );
        wait_for_text(&terminal, "FAKE-END");
        fx.click_close(&terminal);
        assert_eq!(fx.tabs(), 0);
        pump_for(Duration::from_millis(100));

        let files = files_in(&case);
        if expect_file {
            assert_eq!(files.len(), 1, "case {profile_setting}/{record}: {files:?}");
            assert!(
                fs::read_to_string(&files[0])
                    .unwrap()
                    .ends_with("FAKE-END\n")
            );
        } else {
            assert!(
                files.is_empty(),
                "case {profile_setting}/{record}: {files:?}"
            );
        }
        fx.window.destroy();
    }
}

/// About shows who owns Moniker, under which license and where it lives: the
/// copyright line, the MIT license and the repository as its website.
pub(super) fn about_shows_copyright_and_mit() {
    let about = crate::about_dialog();
    assert_eq!(
        about.copyright(),
        ABOUT_COPYRIGHT,
        "the copyright line of the About box"
    );
    assert_eq!(about.license_type(), gtk::License::MitX11);
    assert_eq!(about.website(), REPOSITORY_URL, "the About box's website");
}

/// The header bar has ＋, Sessions (saved layouts of windows, issue #12), the
/// tab's title and ☰ and no Connect button: new connections come from the
/// new-tab list and the Connection Profiles window.
pub(super) fn header_has_no_connect_button(dir: &Path) {
    let ctx = ctx_with(dir, &[web()]);
    let app = test_app();
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(300));

    let header: adw::HeaderBar = find_widget(&main.window).expect("the header bar");
    let menus = widgets_outside_popovers::<gtk::MenuButton>(&header);
    let tooltips: Vec<_> = menus.iter().map(|b| b.tooltip_text()).collect();
    assert!(
        !tooltips.iter().any(|t| t.as_deref() == Some("Connect")),
        "no Connect button: {tooltips:?}"
    );
    assert!(
        menus
            .iter()
            .all(|b| b.icon_name().as_deref() != Some("network-server-symbolic")),
        "no server-icon button either"
    );
    // Sessions (issue #12) and ☰, in that order, and nothing else.
    let tooltips: Vec<_> = tooltips.iter().map(|t| t.as_deref()).collect();
    assert_eq!(
        tooltips,
        [Some("Sessions"), Some("Main Menu")],
        "the header's menu buttons"
    );
    let new_tab = widgets_outside_popovers::<gtk::Button>(&header)
        .into_iter()
        .filter(|b| b.tooltip_text().as_deref() == Some("New Tab"))
        .count();
    assert_eq!(new_tab, 1, "the ＋ button stays");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty());
}

/// A session started from the Connection Profiles window opens in the main window that
/// was active last, which is raised; with none left, in a new one that holds
/// only that session.
pub(super) fn profiles_connect_opens_in_the_last_active_window(dir: &Path) {
    let p = web();
    let ctx = ctx_with(dir, std::slice::from_ref(&p));
    let app = test_app();
    let a = window::build(&app, &ctx);
    a.window.present();
    let b = window::build(&app, &ctx);
    b.window.present();
    pump_until("the second window to be active", || b.window.is_active());

    // Only the opening of the tab is swapped out (no ssh runs): the session's
    // tab, named after the profile, in the view the app picked.
    let connect = window::connect_with(&app, &ctx, |view, _ctx, p, _sequence, _record| {
        new_holder_page(view, &tab_title(p));
    });
    connect(&p, None, false);
    assert_eq!(
        b.view.n_pages(),
        2,
        "the session opens in the active window"
    );
    assert_eq!(a.view.n_pages(), 1);
    assert_eq!(
        b.view.selected_page().map(|page| page.title().to_string()),
        Some(tab_title(&p))
    );

    // From the Connection Profiles window, in front of both: the last active main window.
    a.window.present();
    pump_until("the first window to be active", || a.window.is_active());
    let profiles_window = profiles::present(&app, &ctx, Rc::clone(&connect));
    pump_until("the Connection Profiles window to be active", || {
        profiles_window.is_active()
    });
    profiles::select(&profiles_window, &p.id);
    pump_for(Duration::from_millis(100));
    button_labeled(&profiles_window, "Connect")
        .expect("the Connection Profiles window's Connect button")
        .emit_clicked();
    pump_for(Duration::from_millis(100));
    assert_eq!(
        a.view.n_pages(),
        2,
        "the session opens in the last main window"
    );
    assert_eq!(b.view.n_pages(), 2);
    pump_until("the main window to be raised", || a.window.is_active());

    // No main window left: a new one, with only the session in it.
    a.window.destroy();
    b.window.destroy();
    pump_for(Duration::from_millis(200));
    assert!(window::last_active(&app).is_none());
    connect(&p, Some(0), true);
    pump_for(Duration::from_millis(200));
    let c = window::last_active(&app).expect("a new main window");
    assert_eq!(c.view.n_pages(), 1, "no extra local tab");
    assert_eq!(
        c.view.selected_page().map(|page| page.title().to_string()),
        Some(tab_title(&p))
    );
    assert!(c.window.is_visible());
    profiles_window.close();
    c.window.destroy();
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty());
}

/// One vocabulary (design §3 and §4): "Transcripts" for the noun and "Save
/// transcript" for the verb, "Connection Profiles" for the profiles window and
/// its menu entry, on every surface that names them, written out here so that
/// a changed constant cannot change the test with it. The old names must be
/// gone, not merely joined by the new ones.
pub(super) fn wording_uses_transcripts_and_connection_profiles(dir: &Path) {
    let p = web();
    let ctx = ctx_with(dir, std::slice::from_ref(&p));
    let app = test_app();
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(400));

    // The ☰ menu.
    let header: adw::HeaderBar = find_widget(&main.window).expect("the header bar");
    let menus = widgets_outside_popovers::<gtk::MenuButton>(&header);
    let main_menu = menus
        .iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Main Menu"))
        .expect("the ☰ button");
    let items = menu_items(&main_menu.menu_model().expect("the main menu"));
    for entry in [
        ("Connection Profiles", "app.profiles"),
        ("Transcripts", "app.recordings"),
    ] {
        let entry = (entry.0.to_owned(), entry.1.to_owned());
        assert!(items.contains(&entry), "{entry:?} in {items:?}");
    }
    for old in ["Profiles", "Recorded Sessions"] {
        assert!(
            !items.iter().any(|(label, _)| label == old),
            "the old menu entry {old:?} is gone: {items:?}"
        );
    }
    main.window.close();
    pump_for(Duration::from_millis(200));

    // The profiles window: its title, its sidebar's title, and the switch.
    let connect: profiles::ConnectFn = Rc::new(|_, _, _| {});
    let profiles_window = profiles::present(&app, &ctx, connect);
    pump_for(Duration::from_millis(300));
    assert_eq!(
        profiles_window.title().as_deref(),
        Some("Connection Profiles")
    );
    let page_titles = |window: &adw::Window| -> Vec<String> {
        all_widgets::<adw::NavigationPage>(window)
            .iter()
            .map(|page| page.title().to_string())
            .collect()
    };
    assert!(
        page_titles(&profiles_window).contains(&"Connection Profiles".to_owned()),
        "the sidebar says Connection Profiles: {:?}",
        page_titles(&profiles_window)
    );
    profiles::select(&profiles_window, &p.id);
    pump_for(Duration::from_millis(100));
    let switches = all_widgets::<adw::SwitchRow>(&profiles_window);
    assert!(
        switches.iter().any(|row| row.title() == "Save transcript"),
        "a Save transcript row on the profile page"
    );
    assert!(
        !switches
            .iter()
            .any(|row| row.title() == "Record transcript"),
        "no Record transcript row"
    );
    profiles_window.close();
    pump_for(Duration::from_millis(200));

    // The › choices.
    let choices = connect_menu::choices(&p, p.transcript, |_, _| {});
    let fx = fixture();
    fx.view.append(&choices);
    pump_for(Duration::from_millis(100));
    assert_eq!(
        choices_parts(&choices).record.label().as_deref(),
        Some("Save transcript")
    );
    assert_eq!(RECORD_LABEL, "Save transcript");

    // The Transcripts window: title, warning and empty state.
    let transcripts = dir.join("transcripts");
    fs::create_dir_all(&transcripts).expect("dir");
    let window = crate::recordings::present(&app, transcripts, &ctx.settings_path);
    pump_for(Duration::from_millis(300));
    assert_eq!(window.title().as_deref(), Some("Transcripts"));
    let shown: Vec<String> = labels_in(&window)
        .iter()
        .map(|l| l.label().to_string())
        .collect();
    let want = |text: &str| {
        assert!(
            shown.iter().any(|label| label == text),
            "the Transcripts window says {text:?}: {shown:?}"
        );
    };
    want(
        "Transcripts hold whatever was on screen, which may include passwords or secrets \
         typed or pasted.",
    );
    want(
        "No transcripts yet. Turn on Save transcript when connecting to save a session's \
         transcript.",
    );
    for label in &shown {
        for old in ["Recorded Sessions", "Recording", "recording"] {
            assert!(!label.contains(old), "{old:?} left in {label:?}");
        }
    }
    window.close();
    pump_for(Duration::from_millis(200));
}

// ----- Issue #15: the command line -----

/// The real path of `dir`, as a shell's `pwd -P` prints it.
fn canonical(dir: &Path) -> String {
    fs::canonicalize(dir)
        .expect("an existing directory")
        .to_string_lossy()
        .into_owned()
}

/// How many main windows `app` has.
fn main_windows(app: &adw::Application) -> usize {
    app.windows()
        .iter()
        .filter(|w| w.widget_name() == "moniker-main")
        .count()
}

/// A shell in `dir`, nothing else asked for.
fn shell_in(dir: &Path) -> Start {
    Start {
        dir: dir.to_path_buf(),
        title: None,
        run: Run::Shell,
        dir_refused: None,
    }
}

/// What the application's `command-line` handler does, in a Moniker that is
/// already running (`launched`), when a launch asked for `start`: the launching
/// process hands over `encode(start)` and Moniker decodes it.
fn handle(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    launched: &Cell<bool>,
    start: &Start,
) -> (i32, Option<String>) {
    crate::handle_command_line(app, ctx, launched, &encode(start))
}

/// The shell in `terminal` is in the folder `real` (its real path): ask it.
fn assert_shell_is_in(terminal: &Terminal, real: &str) {
    wait_for_text(terminal, "$");
    terminal.feed_child(b"echo SHELL-IN:$(pwd -P)\n");
    wait_for_text(terminal, &format!("SHELL-IN:{real}"));
}

/// The terminal of the tab `main` opened with.
fn first_terminal(main: &window::MainWindow) -> Terminal {
    assert_eq!(main.view.n_pages(), 1, "one tab");
    terminal_of(&main.view.selected_page().expect("a tab")).expect("a local terminal")
}

/// Handing a command line to a running Moniker opens a new window where it
/// says: a shell in a folder with a space and a non-ASCII name (what Files
/// sends), a command with its arguments as they are and a title that stays, a
/// folder that is not one (home, and the window says so in plain text). A
/// mistake in what was handed over says so for the launcher's stderr and opens
/// nothing.
pub(super) fn the_command_line_opens_a_window_where_asked(bin: &Path, dir: &Path) {
    let folder = dir.join("My Files").join("Ünï");
    fs::create_dir_all(&folder).expect("the folder");
    let there = canonical(&folder);
    let home = canonical(&glib::home_dir());
    assert_ne!(there, home);
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let launched = Cell::new(true);
    let first = main_window(&app, &ctx);
    assert_eq!(main_windows(&app), 1);
    let delivered = ctx.notice.delivered();

    // A folder: one more window, its shell there.
    assert_eq!(handle(&app, &ctx, &launched, &shell_in(&folder)), (0, None));
    assert_eq!(main_windows(&app), 2, "one more window");
    let second = window::last_active(&app).expect("the new window");
    assert_ne!(second.window, first.window);
    assert!(second.window.is_visible());
    assert_shell_is_in(&first_terminal(&second), &there);
    assert!(launched.get(), "still running");

    // A command: its arguments reach it as they are, in that folder, and the
    // title given stays.
    let args = fake_shell(
        bin,
        "handed-over",
        "for a in \"$@\"; do printf 'ARG:%s\\n' \"$a\"; done\n\
         printf 'PWD:%s\\n' \"$(pwd -P)\"\n\
         exec sleep 600",
    );
    let tricky = ["a b", "$HOME", "-e", "--", "--help", "x; echo INJECTED", ""];
    let mut argv = vec![args];
    argv.extend(tricky.iter().map(|a| a.to_string()));
    let command = Start {
        title: Some("A fixed name".into()),
        run: Run::Command { argv, hold: false },
        ..shell_in(&folder)
    };
    assert_eq!(handle(&app, &ctx, &launched, &command), (0, None));
    assert_eq!(main_windows(&app), 3);
    let third = window::last_active(&app).expect("the new window");
    let terminal = first_terminal(&third);
    wait_for_text(&terminal, &format!("PWD:{there}"));
    let lines = shown(&terminal);
    let printed: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .collect();
    assert_eq!(printed, tricky, "the arguments, one by one");
    assert!(!lines.iter().any(|l| l == "INJECTED"), "nothing ran twice");
    assert_eq!(
        third.view.selected_page().expect("a tab").title(),
        "A fixed name"
    );

    // A mistake in what was handed over: one line for the launcher's stderr,
    // status 2, and no window.
    for (words, text) in [
        (
            vec!["--moniker-frob=1"],
            "moniker: unexpected handover: --moniker-frob=1\n",
        ),
        (
            vec!["--moniker-dir=/tmp", "--moniker-dir=/var"],
            "moniker: unexpected handover: --moniker-dir=/var\n",
        ),
        (
            vec!["--moniker-hold"],
            "moniker: unexpected handover: --moniker-hold\n",
        ),
        (vec!["--", "ls"], "moniker: unexpected handover: --\n"),
        (vec!["--help"], "moniker: unexpected handover: --help\n"),
    ] {
        let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        assert_eq!(
            crate::handle_command_line(&app, &ctx, &launched, &words),
            (2, Some(text.to_owned())),
            "{words:?}"
        );
    }
    pump_for(Duration::from_millis(300));
    assert_eq!(main_windows(&app), 3, "a mistake opens no window");

    // A folder that is not one: the shell starts in home, and the window says
    // so, as plain text, in its own notification area.
    let file = dir.join("<b>x<b> &amp; a file");
    fs::write(&file, "x").expect("a file");
    let refused = Start {
        dir_refused: Some(Refused::not_a_folder(file.clone())),
        ..shell_in(&glib::home_dir())
    };
    assert_eq!(handle(&app, &ctx, &launched, &refused), (0, None));
    let fourth = window::last_active(&app).expect("the new window");
    assert_eq!(main_windows(&app), 4);
    assert_shell_is_in(&first_terminal(&fourth), &home);
    let text = format!(
        "Could not open {}: it is not a folder. The terminal started in your home folder.",
        file.display()
    );
    assert_eq!(ctx.notice.last_text().as_deref(), Some(text.as_str()));
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&fourth.notifications));
    assert_eq!(fourth.notifications.shown_text().as_deref(), Some(&*text));
    assert!(
        !fourth.notifications.label().uses_markup(),
        "the notification is plain text"
    );
    assert_eq!(
        ctx.notice.delivered(),
        delivered + 1,
        "the other windows said nothing"
    );

    // The folder `moniker` was started in has a name that is not valid UTF-8
    // (a `-w` value like that is a mistake, so this is the only way): it
    // cannot be handed over, the shell starts in home, and the note says why in
    // its own words, with the name shown lossily, as plain text.
    let odd = Start {
        dir: PathBuf::from(OsString::from_vec(b"/tmp/<b>b\xffd</b> &amp;".to_vec())),
        ..shell_in(&glib::home_dir())
    };
    assert_eq!(handle(&app, &ctx, &launched, &odd), (0, None));
    let fifth = window::last_active(&app).expect("the new window");
    assert_eq!(main_windows(&app), 5);
    assert_shell_is_in(&first_terminal(&fifth), &home);
    let text = "Could not open /tmp/<b>b\u{fffd}d</b> &amp;: its name is not valid UTF-8. \
                The terminal started in your home folder.";
    assert_eq!(ctx.notice.last_text().as_deref(), Some(text));
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&fifth.notifications));
    assert_eq!(fifth.notifications.shown_text().as_deref(), Some(text));
    assert!(
        !fifth.notifications.label().uses_markup(),
        "the notification is plain text"
    );
    assert_eq!(ctx.notice.delivered(), delivered + 2);

    for main in [first, second, third, fourth, fifth] {
        main.window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// A launch while a dialog is open in the running Moniker (Preferences, say)
/// still opens its window, which is presented and active; the dialog stays on
/// its own window.
pub(super) fn a_launch_while_a_dialog_is_open_still_opens(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let launched = Cell::new(true);
    let a = main_window(&app, &ctx);
    a.window.present();
    pump_until("the first window to be active", || a.window.is_active());
    let dialog = preferences::present(&a.window, &app, &ctx);
    pump_until("the Preferences dialog", || {
        a.window.visible_dialog().as_ref() == Some(dialog.upcast_ref())
    });

    assert_eq!(
        crate::handle_command_line(&app, &ctx, &launched, &[]),
        (0, None)
    );
    assert_eq!(main_windows(&app), 2);
    let b = window::last_active(&app).expect("the new window");
    assert_ne!(b.window, a.window);
    pump_until("the new window to be active", || b.window.is_active());
    assert!(b.window.is_visible());
    assert!(!a.window.is_active(), "the focus moved to the new window");
    assert!(b.window.visible_dialog().is_none(), "no dialog over it");
    assert_eq!(
        a.window.visible_dialog().as_ref(),
        Some(dialog.upcast_ref()),
        "the dialog stays with its own window"
    );
    assert!(terminal_of(&b.view.selected_page().expect("a tab")).is_some());

    dialog.force_close();
    pump_until("the dialog to close", || {
        a.window.visible_dialog().is_none()
    });
    for main in [a, b] {
        main.window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// A launch as the last window closes still opens a window: the handler opens
/// it before it returns, so the request is not lost with the window that went.
pub(super) fn a_launch_as_the_last_window_closes_still_opens(dir: &Path) {
    let folder = dir.join("My Files").join("Ünï");
    fs::create_dir_all(&folder).expect("the folder");
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    // As the running application does: the side windows go with the last main
    // window.
    window::close_side_windows_with_last_main_window(&app);
    let launched = Cell::new(true);
    let only = main_window(&app, &ctx);
    assert_eq!(main_windows(&app), 1);

    // In one turn of the main loop: the person closes the window, and the
    // command line arrives.
    only.window.close();
    assert_eq!(main_windows(&app), 0, "the last window went");
    assert_eq!(handle(&app, &ctx, &launched, &shell_in(&folder)), (0, None));
    assert_eq!(main_windows(&app), 1, "and one is open at once");
    pump_for(Duration::from_millis(300));
    assert_eq!(main_windows(&app), 1);
    let made = window::last_active(&app).expect("the new window");
    assert_ne!(made.window, only.window);
    assert!(made.window.is_visible());
    assert_shell_is_in(&first_terminal(&made), &canonical(&folder));

    made.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// GLib parses the arguments of an application before the application's
/// handler sees them, and the handover depends on it leaving `encode`'s words
/// alone: a command with `--`, `-e`, `--help`, GLib's own `--help-all` and
/// `--gapplication-service`, `-h` and an empty argument, a title with `-- x`,
/// a folder with spaces and non-ASCII letters, a refused folder. Run against
/// GLib itself with the flags the application has.
pub(super) fn glib_leaves_the_handed_over_arguments_alone(dir: &Path) {
    let folder = dir.join("My Files").join("Ünï");
    let awkward = [
        "prog",
        "--",
        "-e",
        "--help",
        "--help-all",
        "--help-gapplication",
        "--gapplication-service",
        "--version",
        "-h",
        "",
        "a b",
        "ünï",
        "--moniker-arg=x",
    ];
    let start = Start {
        title: Some("-- x".into()),
        run: Run::Command {
            argv: awkward.iter().map(|a| a.to_string()).collect(),
            hold: true,
        },
        dir_refused: Some(Refused::not_a_folder(dir.join("not a folder"))),
        ..shell_in(&folder)
    };
    let sent = encode(&start);
    let mut argv = vec!["moniker".to_owned()];
    argv.extend(sent.iter().cloned());

    let seen: Rc<RefCell<Option<Vec<OsString>>>> = Rc::default();
    let app = gio::Application::new(
        Some("org.moniker.GuiTest.Handover"),
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE,
    );
    app.connect_command_line({
        let seen = Rc::clone(&seen);
        move |_, cmdline| {
            *seen.borrow_mut() = Some(cmdline.arguments());
            glib::ExitCode::SUCCESS
        }
    });
    assert_eq!(app.run_with_args(&argv), glib::ExitCode::SUCCESS);

    let seen = seen.take().expect("the command-line handler ran");
    assert_eq!(seen[0], "moniker");
    let words: Vec<String> = seen[1..]
        .iter()
        .map(|w| w.to_str().expect("UTF-8 words").to_owned())
        .collect();
    assert_eq!(words, sent, "GLib changed the words");
    assert_eq!(decode(&words, &glib::home_dir()), Ok(start));
}
