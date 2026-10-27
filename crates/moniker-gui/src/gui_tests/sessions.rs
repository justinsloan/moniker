//! Scenarios: saved sessions (issue #12), a window's tabs and panes captured as
//! records and added back to another window: the arrangement and sizes of the
//! splits, the names, text sizes and transcripts of the panes, profile panes
//! connecting by themselves (through the fake `ssh`, with their default
//! sequence, asking the keyring in pane order), local panes reopening in their
//! folders, a profile or a folder that has gone since, and what a pane still
//! waiting for the keyring, one whose session ended and a command tab are
//! saved as.
//!
//! The window suite has no `$SHELL`, so a local pane runs `/bin/sh`, which does
//! not report its folder by itself: a scenario feeds it the `printf` of an OSC 7
//! that bash and zsh send on Ubuntu (`vte-2.91.sh`), and reads `$PWD` back the
//! same way.
//!
//! The Layouts menu beside ＋ (the last scenarios) is how a person does it:
//! saving a window under a name, opening a saved layout in a new window (issue
//! #16), renaming and deleting, the questions each asks, a name in use, a file
//! that cannot be read, two windows saving at once, a layout gone since the
//! menu was built, and a write that fails.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::secrets::{MemorySecretStore, SecretError, SecretStore, password_key};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;
use zeroize::Zeroizing;

use super::main_window::{ctx_with, ctx_with_secrets, main_windows, test_app};
use super::windows::{list_in, main_window, named, row, ssh_args};
use super::{
    HeldKeyring, MARKUP_NAME, all_widgets, button_labeled, close_popover, fake_shell, fake_ssh,
    files_in, find_widget, labels_in, literal_labels, menu_button, open_popover, pane, profile,
    pump_for, pump_until, shown, wait_for_session, wait_for_text,
};
use crate::context::AppContext;
use crate::launch_args::{Run, Start};
use crate::panes::{self, Pane, TabLayout};
use crate::session_file::{self, Kind, Loaded, Node, PaneRecord, Session, TabRecord};
use crate::session_layout::{capture, restore};
use crate::sessions_menu;
use crate::terminal_tab::{
    self, PaneHolder, open_local, open_profile, rename_pane, set_test_ssh_program, shown_title,
    terminal_in,
};
use crate::{new_tab, preferences, settings, window, zoom};

/// `path` with every byte but the unreserved ones and `/` percent-encoded, as a
/// shell's OSC 7 sends it (`My Files` is `My%20Files`, `Ü` is `%C3%9C`).
fn percent_encoded(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Have the `/bin/sh` in `terminal` report `folder` as a shell does when it
/// changes folder (OSC 7, `file://HOST/PATH` percent-encoded, with this
/// machine's name as the host), and wait until the terminal has it.
fn report_folder(terminal: &Terminal, folder: &str) {
    report_folder_from(terminal, &terminal_tab::local_host_name(), folder);
}

/// [`report_folder`] with the host `host`.
fn report_folder_from(terminal: &Terminal, host: &str, folder: &str) {
    let encoded = percent_encoded(folder);
    assert!(
        encoded.contains("%20") && encoded.contains("%C3%9C"),
        "{encoded}"
    );
    // Input sent before the shell has started is lost.
    wait_for_text(terminal, "$");
    let uri = format!("file://{host}{encoded}");
    terminal.feed_child(format!("printf '\\033]7;%s\\007' '{uri}'\n").as_bytes());
    pump_until("the folder to be reported", || {
        terminal.current_directory_uri().as_deref() == Some(uri.as_str())
    });
}

/// A folder reported for another machine (a shell over `ssh` run by hand in a
/// local pane, or a printed escape sequence) is not saved: the pane is saved
/// with no folder. One reported for this machine afterwards is.
pub(super) fn a_folder_reported_for_another_machine_is_not_saved(dir: &Path) {
    let folder = dir.join("My Files").join("Ünï");
    fs::create_dir_all(&folder).expect("the folder");
    let there = canonical(&folder);
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let terminal = session_in(&pane(&main.view.selected_page().expect("the launch tab")));
    let local = |dir: Option<PathBuf>| Node::Pane(pane_record(Kind::Local { dir }, None, 1.0));
    report_folder_from(&terminal, "elsewhere.example.test", &there);
    assert_eq!(capture(&main.view)[0].root, local(None), "another host");
    report_folder(&terminal, &there);
    assert_eq!(
        capture(&main.view)[0].root,
        local(Some(PathBuf::from(&there))),
        "this machine"
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Wait until `terminal` shows the line `line`, exactly.
pub(super) fn wait_for_line(terminal: &Terminal, line: &str) {
    pump_until(&format!("the line {line:?}"), || {
        shown(terminal).iter().any(|shown| shown == line)
    });
}

/// The folder the `/bin/sh` in `terminal` is in, as its `$PWD` says: waits
/// for the line `PWD=` + `expected`.
fn assert_shell_in(terminal: &Terminal, expected: &str) {
    wait_for_text(terminal, "$");
    terminal.feed_child(b"echo PWD=$PWD\n");
    wait_for_line(terminal, &format!("PWD={expected}"));
}

pub(super) fn canonical(dir: &Path) -> String {
    fs::canonicalize(dir)
        .expect("an existing directory")
        .to_string_lossy()
        .into_owned()
}

fn layout_of(page: &adw::TabPage) -> TabLayout {
    panes::layout_of_page(page).expect("a tab made by terminal_tab has a layout")
}

/// The split at the top of `layout` and its two sides.
fn top_split(layout: &TabLayout) -> (gtk::Paned, gtk::Widget, gtk::Widget) {
    let split = layout
        .first_child()
        .and_downcast::<gtk::Paned>()
        .expect("a split at the top");
    let start = split.start_child().expect("a first side");
    let end = split.end_child().expect("a second side");
    (split, start, end)
}

/// Whether GTK has given `split` a size (before, its `max-position` is GTK's
/// largest number).
fn sized(split: &gtk::Paned) -> bool {
    let max = split.max_position();
    max > 0 && max < i32::MAX
}

/// Where the divider of `split` is, as a fraction of how far it can go.
fn fraction_now(split: &gtk::Paned) -> f64 {
    f64::from(split.position()) / f64::from(split.max_position())
}

/// Move the divider of `split` (once sized) `fraction` of the way along, as a
/// drag does.
fn drag_divider(split: &gtk::Paned, fraction: f64) {
    pump_until("the split to be sized", || sized(split));
    split.set_position((fraction * f64::from(split.max_position())).round() as i32);
    pump_until("the divider to move", || {
        (fraction_now(split) - fraction).abs() < 0.01
    });
}

/// Wait until `split` is sized with its divider within 0.02 of `fraction`,
/// and see that it stays there.
fn assert_fraction(split: &gtk::Paned, fraction: f64, what: &str) {
    pump_until(&format!("{what} at {fraction}"), || {
        sized(split) && (fraction_now(split) - fraction).abs() < 0.02
    });
    pump_for(Duration::from_millis(300));
    assert!(
        (fraction_now(split) - fraction).abs() < 0.02,
        "{what}: {} wanted {fraction}",
        fraction_now(split)
    );
}

/// The terminal of `holder` once its session has started (a profile's waits
/// for the keyring).
pub(super) fn session_in(holder: &PaneHolder) -> Terminal {
    pump_until("the session", || terminal_in(holder).is_some());
    terminal_in(holder).expect("the session")
}

/// The new-tab list in `holder`, if it shows one.
fn list_in_pane(holder: &PaneHolder) -> Option<gtk::ListBox> {
    find_widget::<gtk::ListBox>(holder).filter(|l| l.widget_name() == new_tab::LIST_NAME)
}

/// The commands of the startup sequence a fake `ssh` in `terminal` was given
/// (its arguments with "echo" in them), once it has started.
fn sequence_ran(terminal: &Terminal) -> Vec<String> {
    wait_for_text(terminal, "FAKE-END");
    ssh_args(terminal)
        .into_iter()
        .filter(|arg| arg.contains("echo"))
        .collect()
}

pub(super) fn pane_record(kind: Kind, name: Option<&str>, scale: f64) -> PaneRecord {
    PaneRecord {
        kind,
        name: name.map(str::to_owned),
        scale,
    }
}

/// A window saved and added back to another window: window A has a tab split
/// three ways (a local pane in a folder whose name has a space and letters
/// that are not ASCII, side by side with a profile pane renamed "Logs" at a
/// larger text size over a new-tab list) and a tab with a profile pane whose
/// profile saves its transcripts. Its capture (saved and read back as the file
/// does) is restored into window B: B gains two tabs after its own, the first
/// added selected; the splits have the same arrangement and fractions once
/// sized, the panes their names and sizes and the recorded pane the focus; the
/// local pane's shell starts in the folder; both profile panes connect through
/// the fake `ssh` with their default sequence; the second tab's pane saves a
/// transcript, as its profile's switch says. Nothing of a host, a user, a
/// command or a sequence is in the session.
pub(super) fn a_window_comes_back_as_saved(bin: &Path, dir: &Path) {
    let folder = dir.join("My Files").join("Ünï");
    fs::create_dir_all(&folder).expect("the folder");
    let there = canonical(&folder);
    let transcripts = dir.join("transcripts");
    let fake = fake_ssh(bin, "sessions-ssh", "echo FAKE-END\nexec sleep 600");
    let mut web = profile(&[("a", &["echo A"]), ("b", &["echo B"])]);
    web.name = "web".to_owned();
    web.startup_sequence = Some(1);
    let mut db = profile(&[("x", &["echo X"])]);
    db.name = "db".to_owned();
    db.host = "db.example.test".to_owned();
    db.startup_sequence = Some(0);
    db.transcript = true;
    let ctx = ctx_with(dir, &[web.clone(), db.clone()]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(transcripts.clone());
    })
    .expect("write the settings");
    let app = test_app();
    set_test_ssh_program(Some(fake));
    let a = main_window(&app, &ctx);
    let act = |main: &window::MainWindow, name: &str| {
        WidgetExt::activate_action(&main.window, &format!("win.{name}"), None)
            .unwrap_or_else(|err| panic!("win.{name}: {err}"));
    };

    // Tab 1: the launch tab's shell reports the folder.
    let first = a.view.selected_page().expect("the launch tab");
    let local = pane(&first);
    report_folder(&session_in(&local), &there);
    // Beside it a profile pane, picked from the list as a click does.
    act(&a, panes::SPLIT_RIGHT_ACTION);
    let logs = layout_of(&first).focused_holder().expect("the new pane");
    let list = list_in_pane(&logs).expect("the new pane's list");
    list.emit_by_name::<()>("row-activated", &[&row(&list, &web.id)]);
    assert_eq!(sequence_ran(&session_in(&logs)), ["echo B"], "in window A");
    // Below that, a pane that stays a list.
    act(&a, panes::SPLIT_DOWN_ACTION);
    let below = layout_of(&first).focused_holder().expect("the new pane");
    assert!(list_in_pane(&below).is_some());
    // Tab 2: a profile pane that saves its transcript, as its profile says.
    open_profile(&a.view, &ctx, &db, db.startup_choice(), true);
    let second = a.view.selected_page().expect("the second tab");
    assert_eq!(sequence_ran(&session_in(&pane(&second))), ["echo X"]);
    // Back on tab 1: "Logs" is renamed, at a larger size, and has the focus.
    a.view.set_selected_page(&first);
    panes::focus_pane(&logs);
    for _ in 0..4 {
        act(&a, "zoom-in");
    }
    assert_eq!(logs.scale().get(), 1.5);
    rename_pane(&logs, "Logs");
    let (outer, _, inner) = top_split(&layout_of(&first));
    let inner = inner
        .downcast::<gtk::Paned>()
        .expect("a split on the right");
    // The larger text needs more room once drawn, which can move a divider:
    // dragged after that.
    pump_for(Duration::from_millis(300));
    drag_divider(&outer, 0.3);
    drag_divider(&inner, 0.7);
    pump_for(Duration::from_millis(300));
    let (outer_fraction, inner_fraction) = (fraction_now(&outer), fraction_now(&inner));
    assert!((outer_fraction - 0.3).abs() < 0.02, "{outer_fraction}");
    assert!((inner_fraction - 0.7).abs() < 0.02, "{inner_fraction}");

    // Captured: what each pane was, its name and size, the splits and the
    // focused pane; and nothing else.
    let tabs = capture(&a.view);
    assert_eq!(tabs.len(), 2, "{tabs:?}");
    assert_eq!(tabs[0].focused, 1, "Logs, in reading order");
    let Node::Split {
        horizontal: true,
        fraction,
        start,
        end,
    } = &tabs[0].root
    else {
        panic!("side by side at the top: {:?}", tabs[0].root);
    };
    assert!((fraction - outer_fraction).abs() < 1e-9, "{fraction}");
    assert_eq!(
        **start,
        Node::Pane(pane_record(
            Kind::Local {
                dir: Some(PathBuf::from(&there))
            },
            None,
            1.0
        ))
    );
    let Node::Split {
        horizontal: false,
        fraction,
        start: upper,
        end: lower,
    } = &**end
    else {
        panic!("one above the other on the right: {end:?}");
    };
    assert!((fraction - inner_fraction).abs() < 1e-9, "{fraction}");
    let web_kind = Kind::Profile {
        id: web.id.clone(),
        name: web.name.clone(),
    };
    assert_eq!(
        **upper,
        Node::Pane(pane_record(web_kind, Some("Logs"), 1.5))
    );
    assert_eq!(**lower, Node::Pane(pane_record(Kind::List, None, 1.0)));
    let db_kind = Kind::Profile {
        id: db.id.clone(),
        name: db.name.clone(),
    };
    assert_eq!(
        tabs[1],
        TabRecord {
            root: Node::Pane(pane_record(db_kind, None, 1.0)),
            focused: 0,
        }
    );
    // Saved and read back as the sessions file does it.
    let file = dir.join(session_file::FILE_NAME);
    session_file::put(
        &file,
        Session {
            name: "Work".to_owned(),
            tabs: tabs.clone(),
        },
        false,
    )
    .expect("the session can be saved");
    let text = fs::read_to_string(&file).expect("the file");
    for secret in [&web.host, &web.username, &db.host, "echo", "sessions-ssh"] {
        assert!(!text.contains(secret), "{secret:?} in the session file");
    }
    let Loaded::Sessions(saved) = session_file::load(&file) else {
        panic!("the file reads back");
    };
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].tabs, tabs, "the file keeps it all");

    // Restored into window B, after its own tab; the first added is selected.
    let b = main_window(&app, &ctx);
    let own = b.view.selected_page().expect("B's own tab");
    restore(&b.view, &ctx, &saved[0].tabs);
    assert_eq!(b.view.n_pages(), 3, "B's own tab and the two");
    assert_eq!(b.view.nth_page(0), own, "B's own tab stays first");
    let first = b.view.nth_page(1);
    let second = b.view.nth_page(2);
    assert_eq!(b.view.selected_page().as_ref(), Some(&first));
    // Logs waits for the keyring (its answer needs the main loop): selecting
    // the tab gave the keyboard to the first pane that takes it, and that is
    // undone, so the keyboard is in no other pane.
    let layout = layout_of(&first);
    assert!(
        gtk::prelude::GtkWindowExt::focus(&b.window)
            .is_none_or(|focus| !focus.is_ancestor(&layout)),
        "the keyboard in a pane that is not the focused one"
    );

    // The arrangement, the fractions once sized, the panes and the focus.
    let (outer, left, right) = top_split(&layout);
    assert_eq!(outer.orientation(), gtk::Orientation::Horizontal);
    let left = left.downcast::<Pane>().expect("a pane on the left");
    let inner = right
        .downcast::<gtk::Paned>()
        .expect("a split on the right");
    assert_eq!(inner.orientation(), gtk::Orientation::Vertical);
    let upper = inner
        .start_child()
        .and_downcast::<Pane>()
        .expect("a pane above");
    let lower = inner
        .end_child()
        .and_downcast::<Pane>()
        .expect("a pane below");
    assert!(!outer.shrinks_start_child() && !outer.shrinks_end_child());
    assert!(!inner.shrinks_start_child() && !inner.shrinks_end_child());
    assert_fraction(&outer, outer_fraction, "the side by side split");
    assert_fraction(&inner, inner_fraction, "the split on the right");
    let (local, logs, below) = (left.holder(), upper.holder(), lower.holder());
    assert_eq!(layout.focused_holder().as_ref(), Some(logs), "Logs");
    assert_eq!(shown_title(logs), "Logs");
    assert_eq!(first.title(), "Logs", "the tab follows its focused pane");
    assert_eq!(local.scale().get(), 1.0);
    assert_eq!(logs.scale().get(), 1.5);
    assert_eq!(below.scale().get(), 1.0);

    // The local pane's shell starts in the folder it reported.
    let terminal = session_in(local);
    assert_shell_in(&terminal, &there);
    assert!(!saves_a_transcript(&terminal));
    // Both profile panes connect, each running its default sequence; Logs has
    // the keyboard once its session is there.
    let terminal = session_in(logs);
    assert_eq!(terminal.font_scale(), 1.5);
    assert_eq!(sequence_ran(&terminal), ["echo B"], "web's default");
    assert!(ssh_args(&terminal).contains(&format!("{}@{}", web.username, web.host)));
    pump_until("the keyboard in Logs", || {
        terminal
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| focus == *terminal.upcast_ref::<gtk::Widget>())
    });
    assert!(!saves_a_transcript(&terminal), "web's switch is off");
    assert!(list_in_pane(below).is_some(), "the list pane is the list");
    let db_pane = pane(&second);
    let terminal = session_in(&db_pane);
    assert_eq!(sequence_ran(&terminal), ["echo X"], "db's default");
    assert!(ssh_args(&terminal).contains(&format!("{}@{}", db.username, db.host)));
    // The second tab's pane saves its transcript, as db's switch says.
    assert!(saves_a_transcript(&terminal));
    assert_eq!(files_in(&transcripts), Vec::<PathBuf>::new());
    b.view.close_page(&second);
    pump_until("the transcript", || files_in(&transcripts).len() == 1);
    let written = files_in(&transcripts);
    assert!(
        written[0].to_string_lossy().contains(&db.host),
        "{written:?}"
    );

    set_test_ssh_program(None);
    a.window.destroy();
    b.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A restored tab that is not selected has no size yet: its split takes its
/// fraction when the tab is first shown, not the middle; saved again before
/// that, it is saved as it will be. After that the split is as any other:
/// the divider stays where the person puts it as the window grows.
pub(super) fn a_tab_not_shown_yet_gets_its_split_when_shown(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let list = || Node::Pane(pane_record(Kind::List, None, 1.0));
    let split = Node::Split {
        horizontal: true,
        fraction: 0.3,
        start: Box::new(list()),
        end: Box::new(list()),
    };
    let tabs = [
        TabRecord {
            root: list(),
            focused: 0,
        },
        TabRecord {
            root: split.clone(),
            focused: 1,
        },
    ];
    restore(&main.view, &ctx, &tabs);
    assert_eq!(main.view.n_pages(), 3);
    let shown = main.view.nth_page(1);
    let hidden = main.view.nth_page(2);
    assert_eq!(main.view.selected_page().as_ref(), Some(&shown));
    pump_for(Duration::from_millis(300));
    let layout = layout_of(&hidden);
    let (divided, _, _) = top_split(&layout);
    assert!(!sized(&divided), "a tab not shown has no size yet");
    assert_eq!(
        layout.focused_holder().as_ref(),
        Some(&layout.holders()[1]),
        "the recorded focused pane"
    );
    // Saved again now, it is what it will be.
    let again = capture(&main.view);
    assert_eq!(again.len(), 3);
    assert_eq!(again[2].root, split, "not the middle");

    main.view.set_selected_page(&hidden);
    assert_fraction(&divided, 0.3, "the split, once shown");

    // Then it no longer listens: dragged to 0.6, it stays there as the
    // window grows (GTK keeps the proportion), not back to 0.3.
    drag_divider(&divided, 0.6);
    let before = divided.max_position();
    let (width, height) = main.window.default_size();
    main.window.set_default_size(width + 300, height + 100);
    pump_until("the window to grow", || divided.max_position() > before);
    pump_for(Duration::from_millis(300));
    assert!(
        (fraction_now(&divided) - 0.6).abs() < 0.02,
        "{}",
        fraction_now(&divided)
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A split saved in a wide window and restored into a narrow one: the divider
/// is not put nearer the start than the first side can go (the fraction of the
/// narrow window's room would leave the side less than its least width). Every
/// position the split is given while it takes its fraction is within its limits.
pub(super) fn a_restored_divider_is_put_within_its_limits(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    main.window.set_default_size(420, 300);
    pump_for(Duration::from_millis(300));
    let list = || Node::Pane(pane_record(Kind::List, None, 1.0));
    // Both orientations, at the nearest the start the file reader allows.
    for (horizontal, fraction) in [(true, 0.05), (false, 0.05)] {
        let tabs = [TabRecord {
            root: Node::Split {
                horizontal,
                fraction,
                start: Box::new(list()),
                end: Box::new(list()),
            },
            focused: 0,
        }];
        restore(&main.view, &ctx, &tabs);
        let page = main.view.selected_page().expect("the restored tab");
        let (split, _, _) = top_split(&layout_of(&page));
        let given = Rc::new(RefCell::new(Vec::new()));
        split.connect_position_notify(glib::clone!(
            #[strong]
            given,
            move |split| given.borrow_mut().push((
                split.position(),
                split.min_position(),
                split.max_position()
            ))
        ));
        pump_until("the split to be sized", || sized(&split));
        pump_for(Duration::from_millis(300));
        let what = format!(
            "{} split at {fraction}",
            if horizontal {
                "side by side"
            } else {
                "stacked"
            }
        );
        let (min, max) = (split.min_position(), split.max_position());
        let wanted = (fraction * f64::from(max)).round() as i32;
        assert!(
            wanted < min,
            "{what}: the window is narrow enough for the fraction to be out of reach \
             ({wanted} of {min}..={max})"
        );
        let given = given.borrow();
        assert!(
            !given.is_empty(),
            "{what}: the divider was given a position"
        );
        for (position, min, max) in given.iter() {
            assert!(
                position >= min && position <= max,
                "{what}: given {position}, limits {min}..={max}"
            );
        }
        let final_position = split.position();
        assert!(
            (min..=max).contains(&final_position),
            "{what}: ended at {final_position}, limits {min}..={max}"
        );
        main.view.close_page(&page);
    }
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A profile pane whose profile has been deleted since the session was saved
/// comes back as the new-tab list with a note above the search entry naming
/// the profile as the session recorded it, as plain text. Nothing connects. A
/// record with no profile name (only a hand-edited file has one) gets a note
/// that does not begin with a space.
pub(super) fn a_profile_gone_shows_the_list_with_a_note(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let gone = Kind::Profile {
        id: "no-longer-saved".to_owned(),
        name: MARKUP_NAME.to_owned(),
    };
    let tabs = [TabRecord {
        root: Node::Pane(pane_record(gone, None, 1.0)),
        focused: 0,
    }];
    restore(&main.view, &ctx, &tabs);
    let page = main.view.nth_page(1);
    assert_eq!(main.view.selected_page().as_ref(), Some(&page));
    pump_for(Duration::from_millis(300));
    assert!(list_in(&page).is_some(), "the new-tab list");
    assert_eq!(page.title(), new_tab::TITLE);
    let note = format!("{MARKUP_NAME} is no longer saved.");
    let labels = literal_labels("the note", &page.child(), &note);
    assert_eq!(labels.len(), 1);
    let label = &labels[0];
    assert!(label.wraps(), "a long name wraps");
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    assert_eq!(
        label.next_sibling().as_ref(),
        Some(search.upcast_ref::<gtk::Widget>()),
        "the note is right above the search entry"
    );
    assert!(
        terminal_tab::terminal_of(&page).is_none(),
        "nothing connects"
    );
    assert_eq!(crate::keyring::pending(), 0, "nothing asked of the keyring");
    // It is the list now: saved again, it is saved as one.
    let again = capture(&main.view);
    assert_eq!(
        again[1].root,
        Node::Pane(pane_record(Kind::List, None, 1.0))
    );

    // No name recorded: the note says whose pane it is without one.
    for name in ["", "  "] {
        let nameless = Kind::Profile {
            id: "no-longer-saved".to_owned(),
            name: name.to_owned(),
        };
        let tabs = [TabRecord {
            root: Node::Pane(pane_record(nameless, None, 1.0)),
            focused: 0,
        }];
        restore(&main.view, &ctx, &tabs);
        let page = main.view.selected_page().expect("the restored tab");
        pump_for(Duration::from_millis(300));
        assert!(list_in(&page).is_some(), "the new-tab list");
        literal_labels(
            "the note without a name",
            &page.child(),
            "This pane's profile is no longer saved.",
        );
        main.view.close_page(&page);
    }
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A profile renamed since the session was saved still connects (by id): a
/// pane the person named keeps its name, the other follows the profile's new
/// name.
pub(super) fn a_renamed_profile_still_connects(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "sessions-renamed", "echo FAKE-END\nexec sleep 600");
    let mut web: Profile = profile(&[("a", &["echo A"])]);
    web.name = "web".to_owned();
    web.startup_sequence = Some(0);
    let ctx = ctx_with(dir, std::slice::from_ref(&web));
    let app = test_app();
    let main = main_window(&app, &ctx);
    let recorded = |name: Option<&str>| {
        Node::Pane(pane_record(
            Kind::Profile {
                id: web.id.clone(),
                name: "web".to_owned(),
            },
            name,
            1.0,
        ))
    };
    let tabs = [TabRecord {
        root: Node::Split {
            horizontal: true,
            fraction: 0.5,
            start: Box::new(recorded(None)),
            end: Box::new(recorded(Some("Kept"))),
        },
        focused: 0,
    }];
    let mut renamed = web.clone();
    renamed.name = "web, renamed".to_owned();
    ProfileStore::open(&dir.join("profiles.json"))
        .expect("the store")
        .upsert(renamed)
        .expect("save");
    ctx.profiles_changed();

    set_test_ssh_program(Some(fake));
    restore(&main.view, &ctx, &tabs);
    let page = main.view.nth_page(1);
    let holders = layout_of(&page).holders();
    assert_eq!(holders.len(), 2);
    for holder in &holders {
        let terminal = session_in(holder);
        assert_eq!(sequence_ran(&terminal), ["echo A"]);
        assert!(ssh_args(&terminal).contains(&format!("{}@{}", web.username, web.host)));
    }
    assert_eq!(
        shown_title(&holders[0]),
        "web, renamed",
        "follows the profile"
    );
    assert_eq!(shown_title(&holders[1]), "Kept", "the name given stays");
    assert_eq!(page.title(), "web, renamed");
    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A local pane whose folder no longer exists starts in the home folder, as
/// one with no folder does, and so does one whose folder is still there but
/// cannot be entered (no search permission); nothing is said about either. A
/// recorded name of nothing is no name: the pane is named as a local terminal
/// is, and saved again with none.
pub(super) fn a_folder_gone_starts_at_home(dir: &Path) {
    let gone = dir.join("gone");
    fs::create_dir_all(&gone).expect("the folder");
    fs::remove_dir(&gone).expect("the folder gone");
    // Root enters any folder, so the locked one is only tried as another user.
    // Its mode is given back however the scenario ends.
    struct Unlock(PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let locked = (!rustix::process::geteuid().is_root()).then(|| dir.join("locked"));
    let _unlock = locked.as_ref().map(|locked| {
        fs::create_dir_all(locked).expect("the folder");
        fs::set_permissions(locked, fs::Permissions::from_mode(0o000)).expect("lock it");
        Unlock(locked.clone())
    });
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let local = |dir: Option<PathBuf>, name: Option<&str>| {
        Node::Pane(pane_record(Kind::Local { dir }, name, 1.0))
    };
    let mut tabs = vec![
        TabRecord {
            root: local(Some(gone), Some("")),
            focused: 0,
        },
        TabRecord {
            root: local(None, None),
            focused: 0,
        },
    ];
    if let Some(locked) = &locked {
        tabs.push(TabRecord {
            root: local(Some(locked.clone()), None),
            focused: 0,
        });
    }
    let (delivered, printed) = (ctx.notice.delivered(), ctx.notice.printed().len());
    restore(&main.view, &ctx, &tabs);
    let home = canonical(&glib::home_dir());
    assert_eq!(main.view.n_pages() as usize, 1 + tabs.len());
    for index in 1..main.view.n_pages() {
        let terminal = wait_for_session(&main.view.nth_page(index));
        let failed = |terminal: &Terminal| {
            shown(terminal)
                .iter()
                .any(|line| line.contains("could not start"))
        };
        pump_until("the shell or its failure", || {
            failed(&terminal) || shown(&terminal).iter().any(|line| line.contains('$'))
        });
        assert!(!failed(&terminal), "tab {index}: {:?}", shown(&terminal));
        assert_shell_in(&terminal, &home);
    }
    pump_for(Duration::from_millis(200));
    assert_eq!(ctx.notice.delivered(), delivered, "no message");
    assert_eq!(ctx.notice.printed().len(), printed, "no message");
    assert_eq!(main.view.nth_page(1).title(), "Local terminal");
    let again = capture(&main.view);
    assert_eq!(again[1].root, local(None, None));
    assert_eq!(again[2].root, local(None, None));
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Whether the session in `terminal` saves a transcript: one that does keeps all
/// of its scrollback (`terminal_tab::start_recording`), an ordinary one the
/// 10,000 lines of the settings.
fn saves_a_transcript(terminal: &Terminal) -> bool {
    !(0..=1_000_000_000).contains(&terminal.scrollback_lines())
}

/// Restored panes save a transcript exactly as a plain click would now (user,
/// 2026-10-26): a profile's pane when its profile's "Save transcript" switch or
/// "Save transcripts of all sessions" is on, a local pane only with the latter.
/// What a pane did when the session was saved does not count: a session saved
/// while a pane was saving one, with its profile's switch turned off since,
/// restores that pane without one.
pub(super) fn restored_panes_save_a_transcript_as_a_plain_click_would(bin: &Path, dir: &Path) {
    let transcripts = dir.join("transcripts");
    let fake = fake_ssh(bin, "sessions-transcripts", "echo FAKE-END\nexec sleep 600");
    let mut on = profile(&[]);
    on.name = "on".to_owned();
    on.host = "on.example.test".to_owned();
    on.transcript = true;
    let mut off = profile(&[]);
    off.name = "off".to_owned();
    off.host = "off.example.test".to_owned();
    let ctx = ctx_with(dir, &[on.clone(), off.clone()]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(transcripts.clone());
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));
    let of = |p: &Profile| {
        Node::Pane(pane_record(
            Kind::Profile {
                id: p.id.clone(),
                name: p.name.clone(),
            },
            None,
            1.0,
        ))
    };
    let local = || Node::Pane(pane_record(Kind::Local { dir: None }, None, 1.0));
    let tabs = |roots: Vec<Node>| -> Vec<TabRecord> {
        roots
            .into_iter()
            .map(|root| TabRecord { root, focused: 0 })
            .collect()
    };
    // The sessions of the tabs `restore` adds after the `before` tabs there were.
    let restored = |records: &[TabRecord]| -> Vec<Terminal> {
        let before = main.view.n_pages();
        restore(&main.view, &ctx, records);
        (before..main.view.n_pages())
            .map(|index| session_in(&pane(&main.view.nth_page(index))))
            .collect()
    };
    let close_all_but_the_first = || {
        for index in (1..main.view.n_pages()).rev() {
            main.view.close_page(&main.view.nth_page(index));
        }
        pump_until("the tabs to close", || main.view.n_pages() == 1);
    };

    // "Save transcripts of all sessions" off: only the profile whose switch is
    // on.
    let terminals = restored(&tabs(vec![of(&on), of(&off), local()]));
    wait_for_text(&terminals[0], "FAKE-END");
    wait_for_text(&terminals[1], "FAKE-END");
    wait_for_text(&terminals[2], "$");
    assert!(saves_a_transcript(&terminals[0]), "its switch is on");
    assert!(!saves_a_transcript(&terminals[1]), "its switch is off");
    assert!(!saves_a_transcript(&terminals[2]), "a local pane");
    close_all_but_the_first();
    pump_until("the transcript", || !files_in(&transcripts).is_empty());
    pump_for(Duration::from_millis(300));
    let written = files_in(&transcripts);
    assert_eq!(written.len(), 1, "only one: {written:?}");
    assert!(
        written[0].to_string_lossy().contains(&on.host),
        "{written:?}"
    );

    // With it on, every restored pane saves one.
    settings::update(&ctx.settings_path, |s| s.save_all_transcripts = true)
        .expect("write the settings");
    let terminals = restored(&tabs(vec![of(&off), local()]));
    assert!(saves_a_transcript(&terminals[0]), "save all: a profile's");
    assert!(saves_a_transcript(&terminals[1]), "save all: a local pane");
    close_all_but_the_first();
    pump_until("the two transcripts", || files_in(&transcripts).len() == 3);
    settings::update(&ctx.settings_path, |s| s.save_all_transcripts = false)
        .expect("write the settings");

    // Saved while it was saving one (its switch on), then the switch turned
    // off: restored without one.
    open_profile(&main.view, &ctx, &on, None, true);
    let saving = session_in(&pane(&main.view.nth_page(1)));
    assert!(saves_a_transcript(&saving));
    let file = dir.join(session_file::FILE_NAME);
    save_as(&file, "Work", capture(&main.view)[1..].to_vec());
    close_all_but_the_first();
    let mut switched_off = on.clone();
    switched_off.transcript = false;
    ProfileStore::open(&dir.join("profiles.json"))
        .expect("the store")
        .upsert(switched_off)
        .expect("save");
    ctx.profiles_changed();
    let terminals = restored(&saved(&file)[0].tabs);
    assert!(!saves_a_transcript(&terminals[0]), "its switch is off now");

    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A tab started by `moniker -- COMMAND` (issue #15) is saved as a local
/// terminal in the folder the command was started in (it reports none), never
/// as its command, and comes back as a shell there.
pub(super) fn a_command_tab_is_saved_as_a_shell_in_its_folder(bin: &Path, dir: &Path) {
    let folder = dir.join("work");
    fs::create_dir_all(&folder).expect("the folder");
    let there = canonical(&folder);
    let program = fake_shell(bin, "sessions-command", "echo COMMAND-UP\nexec sleep 600");
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let start = Start {
        dir: PathBuf::from(&there),
        title: None,
        run: Run::Command {
            argv: vec![program.clone(), "--flag".to_owned()],
            hold: false,
        },
        dir_refused: None,
    };
    let terminal = open_local(&main.view, zoom::DEFAULT, &start, None);
    wait_for_text(&terminal, "COMMAND-UP");
    let tabs = capture(&main.view);
    assert_eq!(tabs.len(), 2);
    assert_eq!(
        tabs[1].root,
        Node::Pane(pane_record(
            Kind::Local {
                dir: Some(PathBuf::from(&there))
            },
            None,
            1.0
        ))
    );
    let file = dir.join(session_file::FILE_NAME);
    session_file::put(
        &file,
        Session {
            name: "S".to_owned(),
            tabs: tabs.clone(),
        },
        false,
    )
    .expect("saved");
    let text = fs::read_to_string(&file).expect("the file");
    assert!(
        !text.contains("sessions-command") && !text.contains("--flag"),
        "{text}"
    );

    restore(&main.view, &ctx, &tabs[1..]);
    let page = main.view.nth_page(2);
    let terminal = wait_for_session(&page);
    assert_shell_in(&terminal, &there);
    assert!(!shown(&terminal).iter().any(|line| line == "COMMAND-UP"));
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A profile pane that still waits for the keyring is saved as its profile,
/// and so is one whose session has ended (the pane stays with the note). Names
/// longer than the file keeps are cut to fit, so the session can still be
/// saved.
pub(super) fn a_pane_waiting_or_ended_is_still_its_profile(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "sessions-ends", "echo FAKE-END\nexit 1");
    let mut web = profile(&[]);
    web.name = "w".repeat(300);
    let keyring = Arc::new(HeldKeyring::new(Arc::new(MemorySecretStore::default())));
    let ctx = ctx_with_secrets(dir, std::slice::from_ref(&web), Arc::clone(&keyring) as _);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(dir.join("transcripts"));
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    keyring.hold();
    open_profile(&main.view, &ctx, &web, None, true);
    let page = main.view.selected_page().expect("the new tab");
    let holder = pane(&page);
    rename_pane(&holder, &"n".repeat(250));
    pump_for(Duration::from_millis(200));
    assert!(terminal_in(&holder).is_none(), "still waiting");
    let kept = |text: &str| text.chars().take(200).collect::<String>();
    let expected = Node::Pane(pane_record(
        Kind::Profile {
            id: web.id.clone(),
            name: kept(&web.name),
        },
        Some(&kept(&"n".repeat(250))),
        1.0,
    ));
    let tabs = capture(&main.view);
    assert_eq!(tabs[1].root, expected, "waiting");
    session_file::put(
        &dir.join(session_file::FILE_NAME),
        Session {
            name: "S".to_owned(),
            tabs,
        },
        false,
    )
    .expect("a session with long names can be saved");

    keyring.release();
    let terminal = session_in(&holder);
    wait_for_text(&terminal, "[session ended: status 1]");
    assert_eq!(capture(&main.view)[1].root, expected, "ended");
    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A keyring that writes down the keys it is asked for, in order.
#[derive(Default)]
struct AskedKeyring {
    inner: MemorySecretStore,
    asked: Mutex<Vec<String>>,
}

impl SecretStore for AskedKeyring {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
        self.asked.lock().unwrap().push(key.to_owned());
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.inner.delete(key)
    }
    fn probe(&self) -> Result<(), SecretError> {
        self.inner.probe()
    }
}

/// Restored connections ask the keyring worker in pane order: the panes of a
/// tab in reading order, tab after tab (issue #4).
pub(super) fn restored_connections_ask_the_keyring_in_pane_order(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "sessions-order", "echo FAKE-END\nexec sleep 600");
    let named = |name: &str| {
        let mut p = profile(&[]);
        p.name = name.to_owned();
        p
    };
    let (one, two, three) = (named("one"), named("two"), named("three"));
    let keyring = Arc::new(AskedKeyring::default());
    let ctx = ctx_with_secrets(
        dir,
        &[one.clone(), two.clone(), three.clone()],
        Arc::clone(&keyring) as _,
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));
    let of = |p: &Profile| {
        Node::Pane(pane_record(
            Kind::Profile {
                id: p.id.clone(),
                name: p.name.clone(),
            },
            None,
            1.0,
        ))
    };
    let tabs = [
        TabRecord {
            root: Node::Split {
                horizontal: false,
                fraction: 0.5,
                start: Box::new(of(&two)),
                end: Box::new(of(&one)),
            },
            focused: 1,
        },
        TabRecord {
            root: of(&three),
            focused: 0,
        },
    ];
    keyring.asked.lock().unwrap().clear();
    restore(&main.view, &ctx, &tabs);
    let mut holders = layout_of(&main.view.nth_page(1)).holders();
    holders.extend(layout_of(&main.view.nth_page(2)).holders());
    for holder in &holders {
        wait_for_text(&session_in(holder), "FAKE-END");
    }
    let asked = keyring.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [&two, &one, &three].map(|p| password_key(&p.id)),
        "pane order"
    );
    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

// The Layouts menu

/// The menu's last entry.
const SAVE_ENTRY: &str = "Save Layout\u{2026}";

/// The Layouts button of `main`.
fn sessions_button(main: &window::MainWindow) -> gtk::MenuButton {
    named(&main.window, sessions_menu::BUTTON_NAME)
        .and_downcast()
        .expect("the Layouts button")
}

/// What `button` shows, in order: the text of its labels, the icon names of its
/// images ("first or fallback" for a themed icon of several, symbolic names
/// only: GLib adds the full-color ones itself), and "arrow" for a drop-down
/// arrow; only what is on screen.
fn face_of(button: &gtk::MenuButton) -> Vec<String> {
    all_widgets::<gtk::Widget>(button)
        .into_iter()
        .filter(|widget| widget.is_mapped())
        .filter_map(|widget| {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                Some(label.label().to_string())
            } else if let Some(image) = widget.downcast_ref::<gtk::Image>() {
                image.icon_name().map(|name| name.to_string()).or_else(|| {
                    let icon = image.gicon().and_downcast::<gio::ThemedIcon>()?;
                    let names: Vec<String> = icon
                        .names()
                        .iter()
                        .filter(|name| name.ends_with("-symbolic"))
                        .map(|name| name.to_string())
                        .collect();
                    Some(names.join(" or "))
                })
            } else if widget.css_name() == "arrow" {
                Some("arrow".to_owned())
            } else {
                None
            }
        })
        .collect()
}

/// The Preferences switch that shows or hides the Layouts button.
fn sessions_switch(dialog: &adw::PreferencesDialog) -> adw::SwitchRow {
    all_widgets::<adw::SwitchRow>(dialog)
        .into_iter()
        .find(|row| row.title() == "Show the Layouts menu")
        .expect("the Layouts switch")
}

/// Give the Layouts button of `main` the keyboard, and wait until it has it.
fn keyboard_on_the_sessions_button(main: &window::MainWindow) {
    let button = sessions_button(main);
    assert!(button.grab_focus(), "the button takes the keyboard");
    pump_until("the Layouts button to have the keyboard", || {
        gtk::prelude::GtkWindowExt::focus(&main.window).is_some_and(|f| f.is_ancestor(&button))
    });
}

/// Open the Preferences over `main`, set the Layouts switch to `on` and close
/// them again.
fn set_sessions_switch(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    main: &window::MainWindow,
    on: bool,
) {
    let dialog = preferences::present(&main.window, app, ctx);
    pump_until("the Preferences dialog", || {
        main.window.visible_dialog().is_some()
    });
    let switch = sessions_switch(&dialog);
    assert_eq!(switch.is_active(), !on, "the dialog shows what was saved");
    switch.set_active(on);
    assert_eq!(ctx.settings().show_sessions, on, "saved");
    dialog.force_close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
}

/// The text of `button`, one the menu made: a label is its only child.
fn text_of(button: &gtk::Button) -> Option<String> {
    button
        .child()
        .and_downcast::<gtk::Label>()
        .map(|label| label.label().to_string())
}

/// The button below `root` whose text is `text`.
pub(super) fn menu_entry(root: &impl IsA<gtk::Widget>, text: &str) -> gtk::Button {
    all_widgets::<gtk::Button>(root)
        .into_iter()
        .find(|button| text_of(button).as_deref() == Some(text))
        .unwrap_or_else(|| panic!("no {text:?} entry"))
}

/// The texts the menu `popover` shows, sorted (GTK's own empty label for a
/// tooltip is no text).
fn texts_in(popover: &gtk::Popover) -> Vec<String> {
    let mut texts: Vec<String> = labels_in(popover)
        .iter()
        .map(|label| label.label().to_string())
        .filter(|text| !text.is_empty())
        .collect();
    texts.sort();
    texts
}

/// The boxes of the saved sessions in `popover`, in the order shown.
fn rows_in(popover: &gtk::Popover) -> Vec<gtk::Box> {
    all_widgets::<gtk::Box>(popover)
        .into_iter()
        .filter(|row| row.widget_name() == sessions_menu::ROW_NAME)
        .collect()
}

/// The name on the button of `row`.
fn row_name(row: &gtk::Box) -> String {
    let button = row
        .first_child()
        .and_downcast::<gtk::Button>()
        .expect("the session's button");
    text_of(&button).expect("a name on it")
}

/// The names `popover` lists, in the order shown.
pub(super) fn listed(popover: &gtk::Popover) -> Vec<String> {
    rows_in(popover).iter().map(row_name).collect()
}

/// The row of the session `name` in `popover`.
pub(super) fn row_of(popover: &gtk::Popover, name: &str) -> gtk::Box {
    rows_in(popover)
        .into_iter()
        .find(|row| row_name(row) == name)
        .unwrap_or_else(|| panic!("no session {name:?} in the menu"))
}

/// `main`'s Layouts menu, open. It is one popover that is rebuilt each time
/// it shows, so first wait for it to be closed.
pub(super) fn open_menu(main: &window::MainWindow) -> gtk::Popover {
    let button = sessions_button(main);
    let popover = button.popover().expect("the menu");
    pump_until("the menu to be closed", || !popover.is_visible());
    open_popover(&button)
}

/// Click the session `name` in the open `popover`.
pub(super) fn pick_session(popover: &gtk::Popover, name: &str) {
    row_of(popover, name)
        .first_child()
        .and_downcast::<gtk::Button>()
        .expect("the session's button")
        .emit_clicked();
}

/// Open the › of the session `name` in the open `popover` and click `entry`
/// ("Rename\u{2026}" or "Delete\u{2026}") in what it shows.
pub(super) fn from_session_menu(popover: &gtk::Popover, name: &str, entry: &str) {
    let more = menu_button(&row_of(popover, name), "Layout Menu");
    let inner = open_popover(&more);
    menu_entry(&inner, entry).emit_clicked();
    let window = more.root().and_downcast::<adw::ApplicationWindow>();
    assert!(
        window.is_some_and(|window| window.visible_dialog().is_none()),
        "the question waits for the menu to let go of the keyboard"
    );
}

/// The dialog open over `main`'s window, once there is one.
pub(super) fn dialog_over(main: &window::MainWindow, what: &str) -> adw::AlertDialog {
    pump_until(what, || main.window.visible_dialog().is_some());
    main.window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("an alert dialog")
}

/// The name field of a Save or Rename dialog.
pub(super) fn name_entry(dialog: &adw::AlertDialog) -> gtk::Entry {
    find_widget::<gtk::Entry>(dialog).expect("the name field")
}

/// The reason a Save or Rename dialog gives for refusing the name, if it shows
/// one.
fn error_in(dialog: &adw::AlertDialog) -> Option<String> {
    labels_in(dialog)
        .into_iter()
        .find(|label| label.has_css_class("error") && label.is_visible())
        .map(|label| label.label().to_string())
}

/// Whether the button `response` of `dialog` can be pressed.
fn can_answer(dialog: &adw::AlertDialog, response: &str) -> bool {
    button_labeled(dialog, response)
        .unwrap_or_else(|| panic!("no {response} button"))
        .is_sensitive()
}

/// Press the button `response` of `dialog` and wait until the dialog is gone
/// (another may be shown by then).
pub(super) fn answer(main: &window::MainWindow, dialog: &adw::AlertDialog, response: &str) {
    button_labeled(dialog, response)
        .unwrap_or_else(|| panic!("no {response} button"))
        .emit_clicked();
    pump_until("the dialog to go", || {
        main.window.visible_dialog().as_ref() != Some(dialog.upcast_ref::<adw::Dialog>())
    });
}

/// Press Enter in `entry`.
fn press_enter(entry: &gtk::Entry) {
    entry
        .delegate()
        .expect("the entry's text widget")
        .emit_by_name::<()>("activate", &[]);
}

/// Open `main`'s Layouts menu, press Save Layout\u{2026} and wait for the
/// dialog.
pub(super) fn save_dialog(main: &window::MainWindow) -> adw::AlertDialog {
    let popover = open_menu(main);
    menu_entry(&popover, SAVE_ENTRY).emit_clicked();
    assert!(
        main.window.visible_dialog().is_none(),
        "the dialog waits for the menu to let go of the keyboard"
    );
    dialog_over(main, "the Save dialog")
}

/// The sessions in the file at `file`; panics if it cannot be read.
pub(super) fn saved(file: &Path) -> Vec<Session> {
    match session_file::load(file) {
        Loaded::Sessions(sessions) => sessions,
        Loaded::Unreadable(why) => panic!("the sessions file cannot be read: {why}"),
    }
}

/// The names in the file at `file`, in file order.
fn names_saved(file: &Path) -> Vec<String> {
    saved(file)
        .into_iter()
        .map(|session| session.name)
        .collect()
}

/// Save `tabs` in the file at `file` as another window would.
pub(super) fn save_as(file: &Path, name: &str, tabs: Vec<TabRecord>) {
    session_file::put(
        file,
        Session {
            name: name.to_owned(),
            tabs,
        },
        false,
    )
    .expect("the session is saved");
}

/// A session of one tab that shows the new-tab list.
fn one_list_tab() -> Vec<TabRecord> {
    vec![TabRecord {
        root: Node::Pane(pane_record(Kind::List, None, 1.0)),
        focused: 0,
    }]
}

/// What the one pane of `tab` was.
fn kind_of(tab: &TabRecord) -> &Kind {
    match &tab.root {
        Node::Pane(pane) => &pane.kind,
        split => panic!("a tab of one pane: {split:?}"),
    }
}

/// Wait for the message that comes after `since` messages (`ctx.notice.delivered()`)
/// and give its text; it must be the only one, in `main`'s own window.
pub(super) fn told(ctx: &Rc<AppContext>, main: &window::MainWindow, since: usize) -> String {
    pump_until("a message", || ctx.notice.delivered() > since);
    assert_eq!(ctx.notice.delivered(), since + 1, "one message");
    assert_eq!(
        ctx.notice.last_area().as_ref(),
        Some(&main.notifications),
        "in the window it is about"
    );
    ctx.notice.last_text().expect("its text")
}

/// The Layouts button is right after ＋ and reads "Layouts", then an icon of
/// several windows, then the drop-down arrow (user, 2026-10-26); the word is on
/// it, so no tooltip repeats it. A menu with nothing saved says so and offers
/// only to save. Looking at it writes nothing.
pub(super) fn the_menu_with_no_sessions(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let plus = all_widgets::<gtk::Button>(&main.window)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("New Tab"))
        .expect("the ＋ button");
    let button = sessions_button(&main);
    assert_eq!(
        plus.next_sibling().as_ref(),
        Some(button.upcast_ref::<gtk::Widget>()),
        "right after ＋"
    );
    assert_eq!(
        face_of(&button),
        [
            "Layouts",
            "focus-windows-symbolic or view-grid-symbolic",
            "arrow"
        ],
        "the word, the icon (with a fallback for a theme without it), the arrow"
    );
    let icon = all_widgets::<gtk::Image>(&button)
        .into_iter()
        .next()
        .expect("the icon");
    assert_eq!(
        icon.accessible_role(),
        gtk::AccessibleRole::Presentation,
        "the word names the button, the icon is not read"
    );
    assert!(button.tooltip_text().is_none(), "no tooltip");

    let popover = open_menu(&main);
    assert_eq!(texts_in(&popover), ["No saved layouts", SAVE_ENTRY]);
    literal_labels("the menu", &popover, "No saved layouts");
    assert!(rows_in(&popover).is_empty());
    assert!(
        !dir.join(session_file::FILE_NAME).exists(),
        "looking writes nothing"
    );
    close_popover(&popover);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The Preferences switch "Show the Layouts menu" (on to start with) hides the
/// Layouts button in every main window at once and in windows opened while it
/// is off, and shows it again in all of them; the saved layouts are kept
/// meanwhile. A change that cannot be saved (a damaged settings file) changes
/// nothing: the row goes back and the buttons stay as they were. The keyboard
/// never stays on a hidden button: hidden from another window, or given back
/// to it by the closing Preferences, it goes to the terminal.
pub(super) fn the_sessions_button_can_be_hidden(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let file = sessions_menu::sessions_path(&ctx);
    save_as(&file, "Work", one_list_tab());
    let kept = fs::read(&file).expect("the sessions file");
    let app = test_app();
    let a = main_window(&app, &ctx);
    let b = main_window(&app, &ctx);
    assert!(
        sessions_button(&a).is_visible() && sessions_button(&b).is_visible(),
        "shown to start with"
    );

    let dialog = preferences::present(&a.window, &app, &ctx);
    pump_until("the Preferences dialog", || {
        a.window.visible_dialog().is_some()
    });
    let switch = sessions_switch(&dialog);
    assert!(switch.is_active(), "on to start with");
    assert!(!switch.uses_markup());
    assert_eq!(
        switch.subtitle().as_deref(),
        Some(
            "The button beside \u{FF0B} that saves a window's tabs and panes and opens them \
             again in a new window. Turned off, the saved layouts are kept."
        )
    );
    switch.set_active(false);
    assert!(!ctx.settings().show_sessions, "saved");
    assert!(!sessions_button(&a).is_visible(), "hidden in this window");
    assert!(
        !sessions_button(&b).is_visible(),
        "and in the other, at once"
    );

    // A change that cannot be saved is not used.
    let good = fs::read(&ctx.settings_path).expect("the settings file");
    fs::write(&ctx.settings_path, "{ not json").expect("damage the settings file");
    switch.set_active(true);
    pump_until("the row to go back", || !switch.is_active());
    pump_for(Duration::from_millis(200));
    assert!(!switch.is_active(), "it stays back");
    assert!(
        !sessions_button(&a).is_visible() && !sessions_button(&b).is_visible(),
        "still hidden"
    );
    assert_eq!(
        fs::read_to_string(&ctx.settings_path).expect("the settings file"),
        "{ not json",
        "the damaged file is left alone"
    );
    fs::write(&ctx.settings_path, good).expect("repair the settings file");
    dialog.force_close();
    pump_until("the dialog to close", || {
        a.window.visible_dialog().is_none()
    });

    // A window opened while it is off has none either; the sessions are kept.
    let c = main_window(&app, &ctx);
    assert!(!sessions_button(&c).is_visible(), "a new window too");
    assert_eq!(
        fs::read(&file).expect("the sessions file"),
        kept,
        "the sessions are kept"
    );

    // On again: every window shows it, and its menu lists what was saved.
    set_sessions_switch(&app, &ctx, &c, true);
    for main in [&a, &b, &c] {
        assert!(sessions_button(main).is_visible(), "shown again");
    }
    let popover = open_menu(&b);
    assert_eq!(listed(&popover), ["Work"]);
    close_popover(&popover);
    let d = main_window(&app, &ctx);
    assert!(sessions_button(&d).is_visible(), "and in a new window");

    // Off from another window hides it in every window; one whose button had
    // the keyboard gives it to its terminal, so typing goes on reaching it.
    keyboard_on_the_sessions_button(&b);
    set_sessions_switch(&app, &ctx, &d, false);
    for main in [&a, &b, &c, &d] {
        assert!(!sessions_button(main).is_visible(), "hidden everywhere");
    }
    assert_keyboard_back(&b, "hiding the button that had it");

    // Off from the Preferences over the window whose button has the keyboard:
    // the dialog gives the keyboard back to the button it took it from, hidden
    // by then, and it goes on to the terminal.
    set_sessions_switch(&app, &ctx, &a, true);
    keyboard_on_the_sessions_button(&a);
    set_sessions_switch(&app, &ctx, &a, false);
    assert!(!sessions_button(&a).is_visible());
    assert_keyboard_back(&a, "closing the Preferences that hid the button");
    for main in [a, b, c, d] {
        main.window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// The one main window of `app` that is none of `known`, once it is there.
pub(super) fn the_new_window(
    app: &adw::Application,
    known: &[&window::MainWindow],
) -> window::MainWindow {
    let new = || {
        let mut found: Vec<_> = app
            .windows()
            .iter()
            .filter(|window| {
                known
                    .iter()
                    .all(|main| main.window.upcast_ref::<gtk::Window>() != *window)
            })
            .filter_map(window::MainWindow::of)
            .collect();
        if found.len() == 1 { found.pop() } else { None }
    };
    pump_until("exactly one new main window", || new().is_some());
    new().expect("the new window")
}

/// A window saved from the menu and opened from another window's menu (issue
/// #16): Save Layout… names it (the first free "Layout N" to start with, a
/// name of nothing refused with the reason, Enter answering Save) and saves the
/// window as it is when Save is answered; the other window's menu, rebuilt each
/// time it opens, then lists it, and picking it opens a new window holding
/// exactly the layout's tabs, the first selected and its recorded pane with the
/// keyboard, connecting the profile's pane, presented, while the window the menu
/// was in is as it was with its keyboard back in its terminal. Picked twice, it
/// opens two windows.
pub(super) fn saving_and_opening_from_the_menu(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "menu-ssh", "echo FAKE-END\nexec sleep 600");
    let mut web = profile(&[]);
    web.name = "web".to_owned();
    let ctx = ctx_with(dir, std::slice::from_ref(&web));
    let app = test_app();
    set_test_ssh_program(Some(fake));
    let a = main_window(&app, &ctx);
    let b = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);

    // Window A: its own shell and a connection to web.
    open_profile(&a.view, &ctx, &web, None, false);
    let connected = pane(&a.view.selected_page().expect("web's tab"));
    wait_for_text(&session_in(&connected), "FAKE-END");

    // B's menu has nothing yet.
    let popover = open_menu(&b);
    assert!(rows_in(&popover).is_empty());
    close_popover(&popover);

    let dialog = save_dialog(&a);
    assert_eq!(dialog.heading().as_deref(), Some("Save Layout"));
    assert_eq!(
        dialog.body().as_str(),
        "Saves this window's tabs and panes, to open them in a new window later. No \
         passwords, and nothing shown in the panes, are saved."
    );
    assert!(!dialog.is_heading_use_markup() && !dialog.is_body_use_markup());
    assert_eq!(dialog.close_response().as_str(), "cancel");
    let entry = name_entry(&dialog);
    assert_eq!(entry.text(), "Layout 1");
    let focus = dialog
        .focus()
        .expect("the keyboard is somewhere in the dialog");
    assert!(
        &focus == entry.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&entry),
        "the keyboard is in the name field"
    );

    // A name of nothing cannot be saved, and the field says why; nor can one
    // with a control character; Enter does not answer Save then.
    entry.set_text("");
    assert_eq!(error_in(&dialog).as_deref(), Some("Enter a name."));
    assert!(!can_answer(&dialog, "Save"));
    press_enter(&entry);
    entry.set_text("a\tb");
    assert_eq!(
        error_in(&dialog).as_deref(),
        Some("A name cannot contain control characters.")
    );
    assert!(!can_answer(&dialog, "Save"));
    press_enter(&entry);
    pump_for(Duration::from_millis(300));
    assert!(a.window.visible_dialog().is_some(), "still asking");
    assert!(!file.exists(), "nothing saved");
    literal_labels(
        "the reason",
        &dialog,
        "A name cannot contain control characters.",
    );

    // A good name: the reason goes. A tab opened after the dialog was is in the
    // session: the window is captured when Save is answered.
    entry.set_text("  Work ");
    assert_eq!(error_in(&dialog), None);
    assert!(can_answer(&dialog, "Save"));
    new_tab::open(&a.view, &ctx);
    press_enter(&entry);
    pump_until("the dialog to go", || a.window.visible_dialog().is_none());
    let sessions = saved(&file);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, "Work", "trimmed");
    assert_eq!(sessions[0].tabs.len(), 3, "the shell, web and the new tab");
    assert!(matches!(kind_of(&sessions[0].tabs[0]), Kind::Local { .. }));
    assert!(matches!(
        kind_of(&sessions[0].tabs[1]),
        Kind::Profile { id, name } if *id == web.id && name == "web"
    ));
    assert_eq!(*kind_of(&sessions[0].tabs[2]), Kind::List);
    assert_eq!(a.view.n_pages(), 3, "A is as it was");

    // B's menu is rebuilt as it opens: it lists the layout now, and picking it
    // opens a new window holding exactly its tabs.
    let popover = open_menu(&b);
    assert_eq!(listed(&popover), ["Work"]);
    literal_labels("the menu", &popover, "Work");
    let before = main_windows(&app);
    pick_session(&popover, "Work");
    assert_eq!(
        main_windows(&app),
        before,
        "opened once the menu has let go of the keyboard"
    );
    pump_until("the layout's window", || main_windows(&app) == before + 1);
    let w = the_new_window(&app, &[&a, &b]);
    assert!(w.window.is_visible(), "presented");
    assert_eq!(w.view.n_pages(), 3, "the layout's tabs and nothing else");
    assert_eq!(w.view.selected_page().as_ref(), Some(&w.view.nth_page(0)));
    assert!(
        keyboard_in_the_focused_pane(&w),
        "the keyboard in the recorded focused pane of its first tab"
    );
    let terminal = session_in(&pane(&w.view.nth_page(1)));
    wait_for_text(&terminal, "FAKE-END");
    assert!(ssh_args(&terminal).contains(&format!("{}@{}", web.username, web.host)));
    assert!(list_in(&w.view.nth_page(2)).is_some(), "the list tab");
    assert_eq!(b.view.n_pages(), 1, "B is as it was");
    assert_eq!(a.view.n_pages(), 3, "A is as it was");
    pump_until("the menu to close", || !popover.is_visible());
    assert_keyboard_back(&b, "a pick");

    // A layout picked twice opens two windows.
    let popover = open_menu(&b);
    pick_session(&popover, "Work");
    pump_until("a second window", || main_windows(&app) == before + 2);
    assert_eq!(b.view.n_pages(), 1, "B is still as it was");
    assert_eq!(w.view.n_pages(), 3, "the first window of the layout is too");
    assert_keyboard_back(&b, "a second pick");

    set_test_ssh_program(None);
    for window in app.windows() {
        window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// A layout with no tab (the file's reader never gives one, a caller could)
/// opens no window: none is left empty.
pub(super) fn opening_no_tabs_opens_no_window(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let before = main_windows(&app);
    assert!(window::open_layout(&app, &ctx, &[]).is_none());
    pump_for(Duration::from_millis(200));
    assert_eq!(main_windows(&app), before, "no window");
    assert_eq!(a.view.n_pages(), 1, "A is as it was");
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A name already saved is asked about, naming it, before it is replaced:
/// Cancel keeps the old layout byte for byte; Replace puts this window's layout
/// as it is when Replace is answered in the old one's place. A file that goes
/// bad while the question is open is not replaced silently.
pub(super) fn saving_under_a_name_in_use_asks(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    let old = one_list_tab();
    save_as(&file, "Work", old.clone());
    save_as(&file, "Other", old.clone());
    new_tab::open(&a.view, &ctx);
    let before = fs::read(&file).expect("the file");

    // The question, and Cancel.
    let dialog = save_dialog(&a);
    assert_eq!(name_entry(&dialog).text(), "Layout 1", "a name not in use");
    name_entry(&dialog).set_text("Work");
    assert_eq!(error_in(&dialog), None, "asked about, not refused");
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    assert_eq!(
        ask.heading().as_deref(),
        Some("Replace \u{201c}Work\u{201d}?")
    );
    assert_eq!(
        ask.body().as_str(),
        "The saved layout with this name is replaced by this window's tabs and panes."
    );
    assert!(!ask.is_heading_use_markup() && !ask.is_body_use_markup());
    assert_eq!(ask.default_response().as_deref(), Some("cancel"));
    assert_eq!(ask.close_response().as_str(), "cancel");
    assert_eq!(
        ask.response_appearance("replace"),
        adw::ResponseAppearance::Destructive
    );
    assert_eq!(fs::read(&file).expect("the file"), before, "nothing yet");
    answer(&a, &ask, "Cancel");
    assert_eq!(fs::read(&file).expect("the file"), before, "kept");

    // Replace: the window as it is when Replace is answered, in the old
    // one's place.
    let dialog = save_dialog(&a);
    name_entry(&dialog).set_text("Work");
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    new_tab::open(&a.view, &ctx);
    answer(&a, &ask, "Replace");
    assert_eq!(names_saved(&file), ["Work", "Other"], "in its place");
    let sessions = saved(&file);
    assert_eq!(sessions[0].tabs.len(), 3, "the three tabs of the window");
    assert_eq!(sessions[1].tabs, old, "the other is as it was");

    // The file going bad while the question is open: not replaced silently.
    let dialog = save_dialog(&a);
    name_entry(&dialog).set_text("Work");
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    fs::write(&file, "not json").expect("damage the file");
    let since = ctx.notice.delivered();
    answer(&a, &ask, "Replace");
    let said = told(&ctx, &a, since);
    assert!(
        said.starts_with("Could not read the saved layouts: it is not JSON"),
        "{said}"
    );
    assert_eq!(fs::read(&file).expect("the file"), b"not json", "kept");
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Each saved session has a › with Rename\u{2026} and Delete\u{2026}; the menu lists the
/// sessions by name ignoring case. Rename asks for a name (a used one refused
/// with the reason, a name of nothing too), Cancel changes nothing and Rename
/// keeps the session's place in the file; a name taken meanwhile by another
/// window is told. Delete names the session and asks first.
pub(super) fn rename_and_delete_ask_first(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    for name in ["beta", "Zed", "Alpha"] {
        save_as(&file, name, one_list_tab());
    }

    let popover = open_menu(&a);
    assert_eq!(listed(&popover), ["Alpha", "beta", "Zed"], "ignoring case");

    // Rename: the field starts as the name; the reasons; Cancel.
    from_session_menu(&popover, "beta", "Rename\u{2026}");
    let dialog = dialog_over(&a, "the Rename dialog");
    assert_eq!(dialog.heading().as_deref(), Some("Rename Layout"));
    assert!(!dialog.is_heading_use_markup() && !dialog.is_body_use_markup());
    assert_eq!(dialog.close_response().as_str(), "cancel");
    let entry = name_entry(&dialog);
    assert_eq!(entry.text(), "beta");
    assert_eq!(error_in(&dialog), None, "its own name is no clash");
    entry.set_text("Alpha");
    assert_eq!(
        error_in(&dialog).as_deref(),
        Some("A layout with this name exists.")
    );
    assert!(!can_answer(&dialog, "Rename"));
    entry.set_text(" ");
    assert_eq!(error_in(&dialog).as_deref(), Some("Enter a name."));
    assert!(!can_answer(&dialog, "Rename"));
    entry.set_text("gamma");
    assert_eq!(error_in(&dialog), None);
    assert!(can_answer(&dialog, "Rename"));
    answer(&a, &dialog, "Cancel");
    assert_eq!(names_saved(&file), ["beta", "Zed", "Alpha"], "Cancel");

    // Rename: in its place.
    let popover = open_menu(&a);
    from_session_menu(&popover, "beta", "Rename\u{2026}");
    let dialog = dialog_over(&a, "the Rename dialog");
    name_entry(&dialog).set_text("gamma");
    answer(&a, &dialog, "Rename");
    assert_eq!(
        names_saved(&file),
        ["gamma", "Zed", "Alpha"],
        "in its place"
    );
    let popover = open_menu(&a);
    assert_eq!(listed(&popover), ["Alpha", "gamma", "Zed"]);

    // Another window takes the name while the dialog is open: told, and
    // nothing changes.
    from_session_menu(&popover, "gamma", "Rename\u{2026}");
    let dialog = dialog_over(&a, "the Rename dialog");
    name_entry(&dialog).set_text("Fresh");
    save_as(&file, "Fresh", one_list_tab());
    let since = ctx.notice.delivered();
    answer(&a, &dialog, "Rename");
    assert_eq!(
        told(&ctx, &a, since),
        "A layout with this name exists.",
        "the clash is told"
    );
    assert_eq!(names_saved(&file), ["gamma", "Zed", "Alpha", "Fresh"]);

    // Delete names the session and asks first.
    let popover = open_menu(&a);
    from_session_menu(&popover, "gamma", "Delete\u{2026}");
    let ask = dialog_over(&a, "the Delete question");
    assert_eq!(
        ask.heading().as_deref(),
        Some("Delete \u{201c}gamma\u{201d}?")
    );
    assert_eq!(
        ask.body().as_str(),
        "The saved layout is deleted. Windows that are open are not changed."
    );
    assert!(!ask.is_heading_use_markup() && !ask.is_body_use_markup());
    assert_eq!(ask.default_response().as_deref(), Some("cancel"));
    assert_eq!(ask.close_response().as_str(), "cancel");
    assert_eq!(
        ask.response_appearance("delete"),
        adw::ResponseAppearance::Destructive
    );
    assert_eq!(names_saved(&file).len(), 4, "nothing before the answer");
    answer(&a, &ask, "Cancel");
    assert_eq!(
        names_saved(&file),
        ["gamma", "Zed", "Alpha", "Fresh"],
        "kept"
    );
    // Escape is Cancel.
    let popover = open_menu(&a);
    from_session_menu(&popover, "gamma", "Delete\u{2026}");
    let ask = dialog_over(&a, "the Delete question");
    ask.close();
    pump_until("the question to go", || a.window.visible_dialog().is_none());
    assert_eq!(
        names_saved(&file),
        ["gamma", "Zed", "Alpha", "Fresh"],
        "Escape"
    );
    let popover = open_menu(&a);
    from_session_menu(&popover, "gamma", "Delete\u{2026}");
    let ask = dialog_over(&a, "the Delete question");
    answer(&a, &ask, "Delete");
    assert_eq!(names_saved(&file), ["Zed", "Alpha", "Fresh"], "deleted");
    assert_eq!(a.view.n_pages(), 1, "open windows are not changed");
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A file that cannot be read is said so in the menu, which still offers to
/// save; saving asks before starting a new file (the reason in the question),
/// Cancel leaves the bytes alone and Replace writes a new file.
pub(super) fn a_damaged_file_is_told_and_kept(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    fs::write(&file, "not json").expect("damage the file");
    let Loaded::Unreadable(why) = session_file::load(&file) else {
        panic!("the file cannot be read");
    };

    let popover = open_menu(&a);
    let said = format!("Could not read the saved layouts: {why}");
    let labels = literal_labels("the menu", &popover, &said);
    assert!(labels[0].wraps(), "a long reason wraps");
    assert!(rows_in(&popover).is_empty());
    assert_eq!(
        texts_in(&popover),
        [said.as_str(), SAVE_ENTRY],
        "only that and Save"
    );
    close_popover(&popover);

    // Save asks, with the reason; Cancel keeps the file as it is.
    let dialog = save_dialog(&a);
    assert_eq!(name_entry(&dialog).text(), "Layout 1");
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    assert_eq!(
        ask.heading().as_deref(),
        Some("Replace the Saved Layouts File?")
    );
    assert_eq!(
        ask.body().as_str(),
        format!(
            "Moniker could not read it: {why}. Saving starts a new file without the \
             layouts that were in it."
        )
    );
    assert!(!ask.is_heading_use_markup() && !ask.is_body_use_markup());
    assert_eq!(ask.default_response().as_deref(), Some("cancel"));
    assert_eq!(
        ask.response_appearance("replace"),
        adw::ResponseAppearance::Destructive
    );
    answer(&a, &ask, "Cancel");
    assert_eq!(fs::read(&file).expect("the file"), b"not json", "Cancel");

    // Replace: a new file with this window.
    let dialog = save_dialog(&a);
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    answer(&a, &ask, "Replace");
    let sessions = saved(&file);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, "Layout 1");
    assert_eq!(sessions[0].tabs.len(), 1);
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Two windows saving: a menu that was built before another window saved still
/// saves its own session beside it (the file is read again with every save), the
/// name offered is chosen from the file as it is when the dialog opens, and a
/// name the other window took meanwhile is asked about, not replaced from an
/// old snapshot.
pub(super) fn two_windows_saving_keep_both(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let b = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);

    // A's menu is open, built while nothing was saved, when B saves.
    let popover_a = open_menu(&a);
    assert!(rows_in(&popover_a).is_empty());
    let dialog = save_dialog(&b);
    assert_eq!(name_entry(&dialog).text(), "Layout 1");
    answer(&b, &dialog, "Save");
    assert_eq!(names_saved(&file), ["Layout 1"]);

    // A saves from that menu: the name offered is the next one free, and both
    // sessions are in the file.
    assert!(popover_a.is_visible(), "A's menu stayed open");
    menu_entry(&popover_a, SAVE_ENTRY).emit_clicked();
    let dialog = dialog_over(&a, "A's Save dialog");
    assert_eq!(name_entry(&dialog).text(), "Layout 2");
    name_entry(&dialog).set_text("A");
    answer(&a, &dialog, "Save");
    assert_eq!(names_saved(&file), ["Layout 1", "A"], "both kept");

    // B takes "B" while A is asking for it: A is asked, not silent.
    let dialog = save_dialog(&a);
    name_entry(&dialog).set_text("B");
    save_as(&file, "B", one_list_tab());
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    assert_eq!(ask.heading().as_deref(), Some("Replace \u{201c}B\u{201d}?"));
    answer(&a, &ask, "Cancel");
    assert_eq!(names_saved(&file), ["Layout 1", "A", "B"]);
    assert_eq!(saved(&file)[2].tabs, one_list_tab(), "B's is as B left it");
    a.window.destroy();
    b.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A session gone or changed since the menu was built: picking reads the file
/// again, so a replaced layout opens a window with the layout now saved, and one
/// that is gone says so and opens no window; renaming a gone one says so and
/// writes nothing. What is told appears in the window the menu is in, also when
/// another window that was opened earlier has the keyboard too.
pub(super) fn a_session_changed_since_the_menu_opened_is_read_again(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let other = main_window(&app, &ctx);
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    save_as(&file, "Work", one_list_tab());
    save_as(&file, "Keep", one_list_tab());
    let two_tabs = {
        let mut tabs = one_list_tab();
        tabs.extend(one_list_tab());
        tabs
    };

    // Replaced by another window while the menu is open: a new window opens
    // with the layout now saved, and A is as it was.
    let popover = open_menu(&a);
    assert_eq!(listed(&popover), ["Keep", "Work"]);
    save_as(&file, "Keep", two_tabs);
    let windows = main_windows(&app);
    pick_session(&popover, "Keep");
    pump_until("the layout's window", || main_windows(&app) == windows + 1);
    let opened = the_new_window(&app, &[&other, &a]);
    assert_eq!(opened.view.n_pages(), 2, "the layout now saved, two tabs");
    assert_eq!(a.view.n_pages(), 1, "A is as it was");
    opened.window.destroy();
    pump_until("the window to go", || main_windows(&app) == windows);
    assert_keyboard_back(&a, "the pick");

    // Deleted meanwhile, with the new-tab list in front: told, no window, and
    // the keyboard goes to the list's pane, not to the Layouts button.
    new_tab::open(&a.view, &ctx);
    assert!(list_in(&a.view.nth_page(1)).is_some(), "the list in front");
    let popover = open_menu(&a);
    session_file::delete(&file, "Work").expect("another window deletes it");
    let since = ctx.notice.delivered();
    pick_session(&popover, "Work");
    assert_eq!(told(&ctx, &a, since), "That layout is no longer saved.");
    pump_for(Duration::from_millis(300));
    assert_eq!(main_windows(&app), windows, "no window opened");
    assert_eq!(a.view.n_pages(), 2, "nothing added");
    assert_keyboard_back(&a, "the pick of a layout that has gone");
    a.view.close_page(&a.view.nth_page(1));
    pump_until("the list tab to close", || a.view.n_pages() == 1);

    // Renaming one that is gone: told, nothing written.
    let popover = open_menu(&a);
    assert_eq!(listed(&popover), ["Keep"]);
    from_session_menu(&popover, "Keep", "Rename\u{2026}");
    let dialog = dialog_over(&a, "the Rename dialog");
    name_entry(&dialog).set_text("Kept");
    session_file::delete(&file, "Keep").expect("another window deletes it");
    let before = fs::read(&file).expect("the file");
    let since = ctx.notice.delivered();
    answer(&a, &dialog, "Rename");
    assert_eq!(told(&ctx, &a, since), "That layout is no longer saved.");
    assert_eq!(fs::read(&file).expect("the file"), before);

    // A message appears in the window the menu is in, not in the one that has
    // the keyboard: the other window is the active one while the question
    // over A is answered, and the file has gone bad before it is.
    save_as(&file, "Work", one_list_tab());
    let dialog = save_dialog(&a);
    name_entry(&dialog).set_text("Work");
    answer(&a, &dialog, "Save");
    let ask = dialog_over(&a, "the Replace question");
    fs::write(&file, "not json").expect("damage the file");
    other.window.present();
    pump_until("the other window to be active", || other.window.is_active());
    let since = ctx.notice.delivered();
    answer(&a, &ask, "Replace");
    let said = told(&ctx, &a, since);
    assert!(
        said.starts_with("Could not read the saved layouts: "),
        "{said}"
    );
    a.window.destroy();
    other.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A write that fails (the folder is read-only) is told, as plain text, in the
/// window it is about: saving, renaming and deleting each say "Could not save the
/// sessions: \u{2026}", and the file stays as it was.
pub(super) fn a_failed_write_is_told(dir: &Path) {
    if rustix::process::geteuid().is_root() {
        return; // root can write to a read-only folder
    }
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    save_as(&file, "Work", one_list_tab());
    let before = fs::read(&file).expect("the file");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("make the folder read-only");

    let dialog = save_dialog(&a);
    name_entry(&dialog).set_text("Other");
    let since = ctx.notice.delivered();
    answer(&a, &dialog, "Save");
    let said = told(&ctx, &a, since);
    assert!(said.starts_with("Could not save the layouts: "), "{said}");
    literal_labels("a failed write", &a.notifications, &said);

    let popover = open_menu(&a);
    from_session_menu(&popover, "Work", "Rename\u{2026}");
    let dialog = dialog_over(&a, "the Rename dialog");
    name_entry(&dialog).set_text("Moved");
    let since = ctx.notice.delivered();
    answer(&a, &dialog, "Rename");
    let said = told(&ctx, &a, since);
    assert!(said.starts_with("Could not save the layouts: "), "{said}");

    let popover = open_menu(&a);
    from_session_menu(&popover, "Work", "Delete\u{2026}");
    let ask = dialog_over(&a, "the Delete question");
    let since = ctx.notice.delivered();
    answer(&a, &ask, "Delete");
    let said = told(&ctx, &a, since);
    assert!(said.starts_with("Could not save the layouts: "), "{said}");

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("make it writable again");
    assert_eq!(fs::read(&file).expect("the file"), before, "as it was");
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Whether the keyboard is in the focused pane of `main`'s selected tab: its
/// terminal, or what its new-tab list gives it to.
fn keyboard_in_the_focused_pane(main: &window::MainWindow) -> bool {
    let Some(holder) = main
        .view
        .selected_page()
        .as_ref()
        .and_then(terminal_tab::holder_of)
    else {
        return false;
    };
    gtk::prelude::GtkWindowExt::focus(&main.window).is_some_and(|focus| {
        focus == *holder.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&holder)
    })
}

/// Wait until the keyboard is back in the terminal after the answer to a
/// question: in the focused pane of the selected tab (user, 2026-10-26), not
/// on the Layouts button where a menu button leaves it; and see that it stays
/// there.
fn assert_keyboard_back(main: &window::MainWindow, what: &str) {
    pump_until(
        &format!("the keyboard back in the terminal after {what}"),
        || keyboard_in_the_focused_pane(main),
    );
    pump_for(Duration::from_millis(300));
    assert!(
        keyboard_in_the_focused_pane(main),
        "the keyboard left the terminal again after {what}"
    );
}

/// Answer `dialog` with `response`, or with Escape for `None`.
fn answer_or_escape(main: &window::MainWindow, dialog: &adw::AlertDialog, response: Option<&str>) {
    match response {
        Some(response) => answer(main, dialog, response),
        None => {
            dialog.close();
            pump_until("the dialog to go", || {
                main.window.visible_dialog().as_ref() != Some(dialog.upcast_ref::<adw::Dialog>())
            });
        }
    }
}

/// While a question chained to another is open, the keyboard is not given to
/// the terminal behind it.
fn assert_keyboard_stays_out_of_the_terminal(main: &window::MainWindow, what: &str) {
    pump_for(Duration::from_millis(300));
    assert!(
        main.window.visible_dialog().is_some(),
        "{what} is still open"
    );
    assert!(
        !keyboard_in_the_focused_pane(main),
        "the keyboard in the terminal behind {what}"
    );
}

/// After every answer to the Layouts menu's questions (user, 2026-10-26): Save,
/// Rename and Delete, each answered with its action, Cancel and Escape, and the
/// chained "Replace “name”?" and "Replace the Saved Layouts File?" likewise,
/// the keyboard goes back to the terminal: the focused pane of the tab in
/// front, not the Layouts button. While a chained question is open it stays
/// out of the terminal. In a split tab it goes to the focused pane, here one
/// that shows the new-tab list.
pub(super) fn every_answer_gives_the_keyboard_back_to_the_terminal(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    pump_until("the keyboard in the terminal to begin with", || {
        keyboard_in_the_focused_pane(&a)
    });
    let answers = |action: &'static str| {
        [
            (Some("Cancel"), "Cancel"),
            (None, "Escape"),
            (Some(action), action),
        ]
    };

    // Save.
    for (response, what) in answers("Save") {
        let dialog = save_dialog(&a);
        name_entry(&dialog).set_text("Work");
        answer_or_escape(&a, &dialog, response);
        assert_keyboard_back(&a, &format!("{what} on Save"));
    }
    assert_eq!(names_saved(&file), ["Work"]);

    // A name in use: "Replace “Work”?".
    for (response, what) in answers("Replace") {
        let dialog = save_dialog(&a);
        name_entry(&dialog).set_text("Work");
        answer(&a, &dialog, "Save");
        let ask = dialog_over(&a, "the Replace question");
        assert_eq!(
            ask.heading().as_deref(),
            Some("Replace \u{201c}Work\u{201d}?")
        );
        assert_keyboard_stays_out_of_the_terminal(&a, "the Replace question");
        answer_or_escape(&a, &ask, response);
        assert_keyboard_back(&a, &format!("{what} on the Replace question"));
    }

    // A file that cannot be read: "Replace the Saved Layouts File?".
    for (response, what) in answers("Replace") {
        fs::write(&file, "not json").expect("damage the file");
        let dialog = save_dialog(&a);
        answer(&a, &dialog, "Save");
        let ask = dialog_over(&a, "the question about the file");
        assert_eq!(
            ask.heading().as_deref(),
            Some("Replace the Saved Layouts File?")
        );
        assert_keyboard_stays_out_of_the_terminal(&a, "the question about the file");
        answer_or_escape(&a, &ask, response);
        assert_keyboard_back(&a, &format!("{what} on the question about the file"));
    }
    assert_eq!(names_saved(&file), ["Layout 1"], "a new file");

    // Rename.
    for (response, what) in answers("Rename") {
        let popover = open_menu(&a);
        from_session_menu(&popover, "Layout 1", "Rename\u{2026}");
        let dialog = dialog_over(&a, "the Rename dialog");
        if response == Some("Rename") {
            name_entry(&dialog).set_text("Renamed");
        }
        answer_or_escape(&a, &dialog, response);
        assert_keyboard_back(&a, &format!("{what} on Rename"));
    }
    assert_eq!(names_saved(&file), ["Renamed"]);

    // Delete.
    for (response, what) in answers("Delete") {
        let popover = open_menu(&a);
        from_session_menu(&popover, "Renamed", "Delete\u{2026}");
        let ask = dialog_over(&a, "the Delete question");
        answer_or_escape(&a, &ask, response);
        assert_keyboard_back(&a, &format!("{what} on Delete"));
    }
    assert_eq!(names_saved(&file), Vec::<String>::new());

    // A split tab: the focused pane is the new one, which shows the list.
    WidgetExt::activate_action(
        &a.window,
        &format!("win.{}", panes::SPLIT_RIGHT_ACTION),
        None,
    )
    .expect("split right");
    let page = a.view.selected_page().expect("the tab");
    let right = layout_of(&page).focused_holder().expect("the new pane");
    let left = layout_of(&page).holders()[0].clone();
    assert_ne!(left, right);
    assert!(list_in_pane(&right).is_some());
    pump_until("the keyboard in the new pane", || {
        keyboard_in_the_focused_pane(&a)
    });
    let dialog = save_dialog(&a);
    answer(&a, &dialog, "Cancel");
    assert_keyboard_back(&a, "Cancel in a split tab");
    assert_eq!(layout_of(&page).focused_holder().as_ref(), Some(&right));

    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// "Replace the Saved Layouts File?" is the last question when Save finds the
/// file damaged, and it is answered after another window repaired the file: if
/// that file now has a session of the name being saved, "Replace the name?" is
/// still asked (Cancel keeps it all; Replace puts this window's layout in the
/// old one's place and keeps the others); if not, the session is added to the
/// repaired file; if the file is still damaged, a new file starts. After each
/// answer the keyboard is back in the terminal.
pub(super) fn a_file_repaired_while_asking_still_asks_about_a_name(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    let old = one_list_tab();
    pump_until("the keyboard in the terminal to begin with", || {
        keyboard_in_the_focused_pane(&a)
    });

    // Save "Work" over a damaged file; the question about the file is open.
    let ask_about_the_file = || {
        fs::write(&file, "not json").expect("damage the file");
        let dialog = save_dialog(&a);
        name_entry(&dialog).set_text("Work");
        answer(&a, &dialog, "Save");
        let ask = dialog_over(&a, "the question about the file");
        assert_eq!(
            ask.heading().as_deref(),
            Some("Replace the Saved Layouts File?")
        );
        ask
    };
    // Another window repairs it.
    let repair = |names: &[&str]| {
        fs::remove_file(&file).expect("remove the damaged file");
        for name in names {
            save_as(&file, name, old.clone());
        }
    };

    // Repaired with a session of this name: asked about it; Cancel keeps all.
    let ask = ask_about_the_file();
    repair(&["Work", "Keep"]);
    let before = fs::read(&file).expect("the file");
    answer(&a, &ask, "Replace");
    let ask = dialog_over(&a, "the question about the name");
    assert_eq!(
        ask.heading().as_deref(),
        Some("Replace \u{201c}Work\u{201d}?")
    );
    assert_eq!(fs::read(&file).expect("the file"), before, "not yet");
    answer(&a, &ask, "Cancel");
    pump_for(Duration::from_millis(300));
    assert_eq!(fs::read(&file).expect("the file"), before, "kept");
    assert_keyboard_back(&a, "Cancel");

    // The same, answered Replace: this window's layout in the old one's place.
    let ask = ask_about_the_file();
    repair(&["Work", "Keep"]);
    answer(&a, &ask, "Replace");
    let ask = dialog_over(&a, "the question about the name");
    answer(&a, &ask, "Replace");
    assert_eq!(names_saved(&file), ["Work", "Keep"], "in its place");
    let sessions = saved(&file);
    assert!(
        matches!(kind_of(&sessions[0].tabs[0]), Kind::Local { .. }),
        "this window's layout"
    );
    assert_eq!(sessions[1].tabs, old, "the other is as it was");
    assert_keyboard_back(&a, "Replace");

    // Repaired without that name: added, with no further question.
    let ask = ask_about_the_file();
    repair(&["Keep"]);
    answer(&a, &ask, "Replace");
    pump_for(Duration::from_millis(300));
    assert!(a.window.visible_dialog().is_none(), "nothing more to ask");
    assert_eq!(names_saved(&file), ["Keep", "Work"], "added");
    assert_keyboard_back(&a, "Replace");

    // Still damaged: a new file starts, as it was said.
    let ask = ask_about_the_file();
    answer(&a, &ask, "Replace");
    assert_eq!(names_saved(&file), ["Work"], "a new file");
    assert_keyboard_back(&a, "Replace");
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A layout picked after the file went bad (the menu was built before) opens no
/// window: what is wrong with the file is told in the window the menu is in, as
/// for a menu opened then, and that window's keyboard goes back to its pane, the
/// new-tab list's here, not to the Layouts button.
pub(super) fn a_pick_after_the_file_went_bad_is_told(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    save_as(&file, "Work", one_list_tab());
    new_tab::open(&a.view, &ctx);
    assert!(list_in(&a.view.nth_page(1)).is_some(), "the list in front");
    let popover = open_menu(&a);
    assert_eq!(listed(&popover), ["Work"]);
    fs::write(&file, "not json").expect("damage the file");
    let Loaded::Unreadable(why) = session_file::load(&file) else {
        panic!("the file cannot be read");
    };
    let windows = main_windows(&app);
    let since = ctx.notice.delivered();
    pick_session(&popover, "Work");
    assert_eq!(
        told(&ctx, &a, since),
        format!("Could not read the saved layouts: {why}")
    );
    pump_for(Duration::from_millis(300));
    assert_eq!(main_windows(&app), windows, "no window opened");
    assert_eq!(a.view.n_pages(), 2, "nothing added");
    assert_keyboard_back(&a, "the pick of a file that cannot be read");
    assert_eq!(fs::read(&file).expect("the file"), b"not json", "kept");
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A session's button in the menu cuts a long name short, so its tooltip has
/// the whole name, as plain text.
pub(super) fn a_long_name_has_its_whole_name_in_a_tooltip(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let a = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    let long = "A session named for what it is for, which is a good deal too long to fit";
    assert!(long.chars().count() > 40 && long.chars().count() <= 100);
    for name in ["short", long] {
        save_as(&file, name, one_list_tab());
    }
    let popover = open_menu(&a);
    for name in ["short", long] {
        let button = row_of(&popover, name)
            .first_child()
            .and_downcast::<gtk::Button>()
            .expect("the session's button");
        assert_eq!(button.tooltip_text().as_deref(), Some(name));
        assert_eq!(
            button.tooltip_markup().as_deref(),
            Some(glib::markup_escape_text(name).as_str()),
            "plain text"
        );
    }
    close_popover(&popover);
    a.window.destroy();
    pump_for(Duration::from_millis(200));
}
