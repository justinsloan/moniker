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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::secrets::{MemorySecretStore, SecretError, SecretStore, password_key};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;
use zeroize::Zeroizing;

use super::main_window::{ctx_with, ctx_with_secrets, test_app};
use super::windows::{list_in, main_window, row, ssh_args};
use super::{
    HeldKeyring, MARKUP_NAME, fake_shell, fake_ssh, files_in, find_widget, literal_labels, pane,
    profile, pump_for, pump_until, shown, wait_for_session, wait_for_text,
};
use crate::launch_args::{Run, Start};
use crate::panes::{self, Pane, TabLayout};
use crate::session_file::{self, Kind, Loaded, Node, PaneRecord, Session, TabRecord};
use crate::session_layout::{capture, restore};
use crate::terminal_tab::{
    self, PaneHolder, open_local, open_profile, rename_pane, set_test_ssh_program, shown_title,
    terminal_in,
};
use crate::{new_tab, settings, window, zoom};

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
    let encoded = percent_encoded(folder);
    assert!(
        encoded.contains("%20") && encoded.contains("%C3%9C"),
        "{encoded}"
    );
    // Input sent before the shell has started is lost.
    wait_for_text(terminal, "$");
    terminal.feed_child(
        format!("printf '\\033]7;file://%s%s\\007' \"$(uname -n)\" '{encoded}'\n").as_bytes(),
    );
    pump_until("the folder to be reported", || {
        terminal
            .current_directory_uri()
            .is_some_and(|uri| uri.ends_with(&encoded))
    });
}

/// Wait until `terminal` shows the line `line`, exactly.
fn wait_for_line(terminal: &Terminal, line: &str) {
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

fn canonical(dir: &Path) -> String {
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
fn session_in(holder: &PaneHolder) -> Terminal {
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

fn pane_record(kind: Kind, name: Option<&str>, scale: f64, transcript: bool) -> PaneRecord {
    PaneRecord {
        kind,
        name: name.map(str::to_owned),
        scale,
        transcript,
    }
}

/// A window saved and added back to another window: window A has a tab split
/// three ways (a local pane in a folder whose name has a space and letters
/// that are not ASCII, side by side with a profile pane renamed "Logs" at a
/// larger text size over a new-tab list) and a tab with a profile pane saving
/// its transcript. Its capture (saved and read back as the file does) is
/// restored into window B: B gains two tabs after its own, the first added
/// selected; the splits have the same arrangement and fractions once sized,
/// the panes their names and sizes and the recorded pane the focus; the local
/// pane's shell starts in the folder; both profile panes connect through the
/// fake `ssh` with their default sequence; the second tab's pane saves a
/// transcript again. Nothing of a host, a user, a command or a sequence is
/// in the session.
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
    // Tab 2: a profile pane that saves its transcript.
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

    // Captured: what each pane was, its name, size and transcript, the splits
    // and the focused pane; and nothing else.
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
            1.0,
            false
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
        Node::Pane(pane_record(web_kind, Some("Logs"), 1.5, false))
    );
    assert_eq!(
        **lower,
        Node::Pane(pane_record(Kind::List, None, 1.0, false))
    );
    let db_kind = Kind::Profile {
        id: db.id.clone(),
        name: db.name.clone(),
    };
    assert_eq!(
        tabs[1],
        TabRecord {
            root: Node::Pane(pane_record(db_kind, None, 1.0, true)),
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
    assert!(!local.records().get());
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
    assert!(!logs.records().get());
    assert!(list_in_pane(below).is_some(), "the list pane is the list");
    let db_pane = pane(&second);
    let terminal = session_in(&db_pane);
    assert_eq!(sequence_ran(&terminal), ["echo X"], "db's default");
    assert!(ssh_args(&terminal).contains(&format!("{}@{}", db.username, db.host)));
    // The second tab's pane saves its transcript, as it did.
    assert!(db_pane.records().get());
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
    let list = || Node::Pane(pane_record(Kind::List, None, 1.0, false));
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

/// A profile pane whose profile has been deleted since the session was saved
/// comes back as the new-tab list with a note above the search entry naming
/// the profile as the session recorded it, as plain text. Nothing connects.
pub(super) fn a_profile_gone_shows_the_list_with_a_note(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let gone = Kind::Profile {
        id: "no-longer-saved".to_owned(),
        name: MARKUP_NAME.to_owned(),
    };
    let tabs = [TabRecord {
        root: Node::Pane(pane_record(gone, None, 1.0, true)),
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
        Node::Pane(pane_record(Kind::List, None, 1.0, false))
    );
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
            false,
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
/// one with no folder does, and nothing is said about it. A recorded name of
/// nothing is no name: the pane is named as a local terminal is, and saved
/// again with none. A local pane that saved a transcript saves one again.
pub(super) fn a_folder_gone_starts_at_home(dir: &Path) {
    let gone = dir.join("gone");
    fs::create_dir_all(&gone).expect("the folder");
    fs::remove_dir(&gone).expect("the folder gone");
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(transcripts.clone());
    })
    .expect("write the settings");
    let app = test_app();
    let main = main_window(&app, &ctx);
    let local = |dir: Option<PathBuf>, name: Option<&str>, transcript: bool| {
        Node::Pane(pane_record(Kind::Local { dir }, name, 1.0, transcript))
    };
    let tabs = [
        TabRecord {
            root: local(Some(gone), Some(""), false),
            focused: 0,
        },
        TabRecord {
            root: local(None, None, true),
            focused: 0,
        },
    ];
    let (delivered, printed) = (ctx.notice.delivered(), ctx.notice.printed().len());
    restore(&main.view, &ctx, &tabs);
    let home = canonical(&glib::home_dir());
    for index in [1, 2] {
        let page = main.view.nth_page(index);
        let terminal = wait_for_session(&page);
        assert_shell_in(&terminal, &home);
    }
    pump_for(Duration::from_millis(200));
    assert_eq!(ctx.notice.delivered(), delivered, "no message");
    assert_eq!(ctx.notice.printed().len(), printed, "no message");
    assert_eq!(main.view.nth_page(1).title(), "Local terminal");
    assert!(!pane(&main.view.nth_page(1)).records().get());
    assert!(pane(&main.view.nth_page(2)).records().get());
    let again = capture(&main.view);
    assert_eq!(again[1].root, local(None, None, false));
    assert_eq!(again[2].root, local(None, None, true));
    main.view.close_page(&main.view.nth_page(2));
    pump_until("the transcript", || files_in(&transcripts).len() == 1);
    main.view.close_page(&main.view.nth_page(1));
    pump_for(Duration::from_millis(200));
    assert_eq!(files_in(&transcripts).len(), 1, "only the one that saved");

    // With "Save transcripts of all sessions" on, every restored pane saves
    // one, whatever the session says.
    settings::update(&ctx.settings_path, |s| s.save_all_transcripts = true)
        .expect("write the settings");
    restore(&main.view, &ctx, &tabs[..1]);
    let page = main.view.selected_page().expect("the restored tab");
    assert!(pane(&page).records().get(), "save all");
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
            1.0,
            false
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
/// recording a transcript if it will, and so is one whose session has ended
/// (the pane stays with the note). Names longer than the file keeps are cut to
/// fit, so the session can still be saved.
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
        true,
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
            false,
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
