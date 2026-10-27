//! Scenarios: startup sequences for the local terminal (issue #13). A plain
//! pick of Local terminal in the new-tab list runs its default sequence and
//! leaves an interactive shell; the choices behind its "›" offer "Just open"
//! and each sequence, built from Local terminal's file each time they open,
//! with a "Save transcript" box that starts as Local terminal's own switch and
//! "Save transcripts of all sessions" say; a sequence's tab ends as a
//! connection's does; a shell whose path has a space still runs one; a
//! window's first tab runs none but follows the switch; a restored local pane
//! runs the default in its folder.
//!
//! Every scenario writes Local terminal's file (`local-terminal.json`, beside
//! its own profile file) before it opens anything, and some change it while a
//! list is up, to see that the list reads the file at the pick. The window
//! suite has no `$SHELL`, so the shell is `/bin/sh`. Markers are printed with
//! `printf 'X-%s\n' y`, so the command line the terminal echoes never holds
//! the text waited for.

use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::StartupSequence;
use moniker_core::ssh::local_command;
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{choices_parts, ctx_with, test_app};
use super::save_transcripts::{click, configure, new_list, open_choices, set_save_all};
use super::sessions::{canonical, pane_record, save_as, saved, session_in, wait_for_line};
use super::windows::main_window;
use super::{
    files_in, find_widget, fixture, page_of, pane, press_enter_in, pump_for, pump_until,
    wait_for_text,
};
use crate::context::AppContext;
use crate::local_terminal::{self, LocalTerminal};
use crate::session_file::{self, Kind, Node, TabRecord};
use crate::session_layout::{capture, restore};
use crate::terminal_tab::{
    new_holder_page, open_local_pick, start_sequence_in, terminal_of, terminal_text,
};
use crate::{connect_menu, new_tab, window};

fn seq(name: &str, commands: &[&str]) -> StartupSequence {
    StartupSequence {
        name: name.to_owned(),
        commands: commands.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// A sequence that prints `SEQ-ran`.
fn hello() -> StartupSequence {
    seq("Hello", &["printf 'SEQ-%s\\n' ran"])
}

/// A sequence that prints `OTHER-ran`.
fn other() -> StartupSequence {
    seq("Other", &["printf 'OTHER-%s\\n' ran"])
}

/// Put Local terminal's `sequences`, its `default` and its own "Save
/// transcript" switch into its file beside the profile file of `ctx`.
fn set_local(
    ctx: &AppContext,
    sequences: &[StartupSequence],
    default: Option<usize>,
    transcript: bool,
) {
    local_terminal::update(&ctx.local_terminal_path, |local| {
        *local = LocalTerminal {
            sequences: sequences.to_vec(),
            startup_sequence: default,
            transcript,
        };
    })
    .expect("write Local terminal's file");
}

/// A new tab in `view` showing the new-tab list, and its search entry.
fn open_list(view: &adw::TabView, ctx: &Rc<AppContext>) -> (adw::TabPage, gtk::SearchEntry) {
    let page = new_tab::open(view, ctx);
    let search = find_widget::<gtk::SearchEntry>(&page.child()).expect("the search entry");
    (page, search)
}

/// Enter in the search entry of a fresh list: Local terminal, picked plainly.
fn pick_plainly(page: &adw::TabPage, search: &gtk::SearchEntry) -> Terminal {
    press_enter_in(search);
    terminal_of(page).expect("a plain pick opens a local terminal in that tab")
}

fn holds(terminal: &Terminal, needle: &str) -> bool {
    terminal_text(terminal).contains(needle)
}

/// `terminal` does not show `needle`; a failure says what it does show.
#[track_caller]
fn assert_lacks(terminal: &Terminal, needle: &str) {
    let text = terminal_text(terminal);
    assert!(!text.contains(needle), "{needle:?} in {text:?}");
}

/// Open Local terminal's choices on `list` with the Right arrow, as the
/// keyboard does, and wait for their first button to have the keyboard, so
/// that Enter opens it: every opening builds the column again.
fn open_local_choices(list: &gtk::ListBox) -> (gtk::Popover, gtk::Widget) {
    let (popover, column) =
        open_choices(list, new_tab::LOCAL_ROW, connect_menu::LOCAL_CHOICES_NAME);
    let first = choices_parts(&column).buttons[0].clone();
    pump_until("the first choice to have the keyboard", || first.is_focus());
    (popover, column)
}

/// Once the shell in `terminal` shows its prompt, type a command that prints
/// `M-word` and wait for it: the shell is interactive, and whatever a sequence
/// printed before it started is on the screen by then.
fn alive(terminal: &Terminal, word: &str) {
    wait_for_text(terminal, "$");
    terminal.feed_child(format!("printf 'M-%s\\n' {word}\n").as_bytes());
    wait_for_text(terminal, &format!("M-{word}"));
}

/// A plain pick of Local terminal (Enter in the list's search entry) runs its
/// default sequence and leaves an interactive shell, the person's own (here
/// `/bin/sh`, with `$SHELL` set to it although Moniker has none). The pane is
/// named as a plain local tab is, and a saved session keeps it as a local pane
/// in the folder it started in (the home folder), never its commands.
/// The default is read from the file at the pick, not when the list was made;
/// one that points past the end of the list (a hand edit) runs nothing.
pub(super) fn a_plain_pick_runs_the_default_and_leaves_a_shell(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    set_local(&ctx, &[hello(), other()], Some(0), false);
    let fx = fixture();

    let (page, search) = open_list(&fx.view, &ctx);
    let terminal = pick_plainly(&page, &search);
    wait_for_text(&terminal, "SEQ-ran");
    terminal.feed_child(b"printf 'ALIVE-%s\\n' yes\n");
    wait_for_text(&terminal, "ALIVE-yes");
    terminal.feed_child(b"echo \"SH=$SHELL\"\n");
    wait_for_text(&terminal, "SH=/bin/sh");
    assert_lacks(&terminal, "OTHER-ran");
    assert_eq!(page.title(), "Local terminal", "named as a plain local tab");
    let records = capture(&fx.view);
    let last = records.last().expect("the tab");
    assert_eq!(
        last.root,
        Node::Pane(pane_record(
            Kind::Local {
                dir: Some(glib::home_dir())
            },
            None,
            1.0
        )),
        "a local pane in the folder it started in, never its commands"
    );
    fx.click_close(&terminal);
    pump_until("the tab to close", || fx.tabs() == 0);

    // The default changes while a second list is up: the pick reads the file.
    let (page, search) = open_list(&fx.view, &ctx);
    set_local(&ctx, &[hello(), other()], Some(1), false);
    let terminal = pick_plainly(&page, &search);
    wait_for_text(&terminal, "OTHER-ran");
    alive(&terminal, "second");
    assert_lacks(&terminal, "SEQ-ran");
    fx.click_close(&terminal);
    pump_until("the tab to close", || fx.tabs() == 0);

    // A default past the end of the list runs nothing: a plain shell.
    set_local(&ctx, &[hello(), other()], Some(7), false);
    let (page, search) = open_list(&fx.view, &ctx);
    let terminal = pick_plainly(&page, &search);
    alive(&terminal, "plain");
    assert_lacks(&terminal, "SEQ-ran");
    assert_lacks(&terminal, "OTHER-ran");
    fx.click_close(&terminal);
    pump_until("the tab to close", || fx.tabs() == 0);

    fx.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The labels of the buttons of a choices column, none using markup.
fn labels(column: &gtk::Widget) -> Vec<String> {
    choices_parts(column)
        .labels
        .into_iter()
        .map(|(label, markup)| {
            assert!(!markup, "{label}: plain text");
            label
        })
        .collect()
}

/// The choices behind Local terminal's "›": with sequences, "Just open" (a
/// shell, no sequence, whatever the default) and each sequence by name (that
/// one only); with none, "Open". They are built from Local terminal's file
/// each time they open, so a change made while the list is up shows, and the
/// "Save transcript" box starts checked when Local terminal's own switch or
/// "Save transcripts of all sessions" is on. With its own switch on, a plain
/// pick saves a transcript too. The new tab the Connection Profiles window's
/// Open will make (`open_local_pick`) runs the sequence it is given and saves
/// a transcript when asked.
pub(super) fn the_choices_offer_just_open_and_each_sequence(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, false);
    set_local(&ctx, &[hello(), other()], Some(0), false);
    let app = test_app();
    let main = main_window(&app, &ctx);

    // "Just open": a shell, and the default does not run.
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) = open_local_choices(&list);
    assert_eq!(labels(&column), ["Just open", "Hello", "Other"]);
    let parts = choices_parts(&column);
    assert!(!parts.record.is_active(), "both switches off");
    parts.buttons[0].emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    let terminal = terminal_of(&page).expect("Just open opens a local shell in that tab");
    alive(&terminal, "just");
    assert_lacks(&terminal, "SEQ-ran");
    assert_lacks(&terminal, "OTHER-ran");
    main.view.close_page(&page);
    pump_until("the tab to close", || main.view.n_pages() == 1);

    // A sequence by name runs that one, not the default.
    let (page, list) = new_list(&main, &ctx);
    let (popover, column) = open_local_choices(&list);
    choices_parts(&column).buttons[2].emit_clicked();
    pump_until("the choices to close", || !popover.is_visible());
    let terminal = terminal_of(&page).expect("a local terminal in that tab");
    wait_for_text(&terminal, "OTHER-ran");
    alive(&terminal, "other");
    assert_lacks(&terminal, "SEQ-ran");
    main.view.close_page(&page);
    pump_until("the tab to close", || main.view.n_pages() == 1);

    // The sequences go while the list is up: the next opening offers "Open".
    let (page, list) = new_list(&main, &ctx);
    set_local(&ctx, &[], None, false);
    let (popover, column) = open_local_choices(&list);
    assert_eq!(labels(&column), [connect_menu::OPEN_LABEL]);
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());

    // The box: Local terminal's own switch, then "save all" alone.
    set_local(&ctx, &[hello(), other()], Some(0), true);
    let (popover, column) = open_local_choices(&list);
    assert_eq!(labels(&column), ["Just open", "Hello", "Other"]);
    assert!(choices_parts(&column).record.is_active(), "its own switch");
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());
    set_local(&ctx, &[hello(), other()], Some(0), false);
    set_save_all(&ctx, true);
    let (popover, column) = open_local_choices(&list);
    assert!(choices_parts(&column).record.is_active(), "save all");
    popover.popdown();
    pump_until("the choices to close", || !popover.is_visible());
    set_save_all(&ctx, false);
    assert!(
        terminal_of(&page).is_none(),
        "looking at the choices opens nothing"
    );
    main.view.close_page(&page);
    pump_until("the tab to close", || main.view.n_pages() == 1);
    assert!(files_in(&transcripts).is_empty(), "nothing saved yet");

    // A plain pick with Local terminal's own switch on saves a transcript.
    set_local(&ctx, &[hello(), other()], Some(0), true);
    let (page, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    let terminal = terminal_of(&page).expect("a local terminal in that tab");
    wait_for_text(&terminal, "SEQ-ran");
    alive(&terminal, "switch");
    main.view.close_page(&page);
    pump_until("its transcript", || files_in(&transcripts).len() == 1);
    set_local(&ctx, &[hello(), other()], Some(0), false);

    // A tab opened for a pick (the Connection Profiles window's Open).
    let terminal = open_local_pick(&main.view, &ctx, Some(&other()), true);
    wait_for_text(&terminal, "OTHER-ran");
    alive(&terminal, "picked");
    main.view.close_page(&page_of(&main.view, &terminal));
    pump_until("its transcript", || files_in(&transcripts).len() == 2);
    assert!(
        files_in(&transcripts)
            .iter()
            .any(|f| fs::read_to_string(f).is_ok_and(|t| t.contains("OTHER-ran"))),
        "{:?}",
        files_in(&transcripts)
    );

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A sequence's tab ends as a connection's does, not as a plain shell's: a
/// sequence that fails keeps its tab with the note, a clean exit of the shell
/// it leaves closes it, and Ctrl+C during a long command of the sequence (which
/// ends the shell running it too) keeps the tab and what it showed.
pub(super) fn a_sequence_s_tab_ends_like_a_connection(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let fx = fixture();

    set_local(
        &ctx,
        &[seq("Fails", &["printf 'BEFORE-%s\\n' x", "exit 3"])],
        Some(0),
        false,
    );
    let (page, search) = open_list(&fx.view, &ctx);
    let terminal = pick_plainly(&page, &search);
    wait_for_text(&terminal, "BEFORE-x");
    wait_for_text(&terminal, "[session ended: status 3]");
    pump_for(Duration::from_millis(300));
    assert_eq!(fx.tabs(), 1, "a failed sequence keeps its tab");
    fx.click_close(&terminal);
    pump_until("the tab to close", || fx.tabs() == 0);

    set_local(
        &ctx,
        &[seq("Marks", &["printf 'X-%s\\n' y"])],
        Some(0),
        false,
    );
    let (page, search) = open_list(&fx.view, &ctx);
    let terminal = pick_plainly(&page, &search);
    wait_for_text(&terminal, "X-y");
    wait_for_text(&terminal, "$");
    terminal.feed_child(b"exit\n");
    pump_until("the tab to close on a clean exit", || fx.tabs() == 0);

    set_local(
        &ctx,
        &[seq("Long", &["printf 'LONG-%s\\n' start", "sleep 600"])],
        Some(0),
        false,
    );
    let (page, search) = open_list(&fx.view, &ctx);
    let terminal = pick_plainly(&page, &search);
    wait_for_text(&terminal, "LONG-start");
    terminal.feed_child(b"\x03");
    wait_for_text(&terminal, "[session ended:");
    pump_for(Duration::from_millis(300));
    assert_eq!(
        fx.tabs(),
        1,
        "Ctrl+C keeps the tab: {:?}",
        terminal_text(&terminal)
    );
    assert!(holds(&terminal, "LONG-start"), "what it showed stays");
    fx.click_close(&terminal);
    pump_until("the tab to close", || fx.tabs() == 0);

    fx.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A shell whose path has a space (Review Focus 1): the path travels in the
/// environment, never in the script, so the sequence runs, in that shell (its
/// `$0` is the path), and the shell it leaves is that one too, with `$SHELL`
/// (and its `$0`) that path.
pub(super) fn a_shell_path_with_a_space_runs_the_sequence(dir: &Path) {
    let shells = dir.join("My Shells");
    fs::create_dir_all(&shells).expect("the folder");
    let shell = shells.join("sh");
    fs::copy("/bin/sh", &shell).expect("a copy of /bin/sh");
    let shell = shell.to_str().expect("utf-8 temp path").to_owned();
    let fx = fixture();
    let page = new_holder_page(&fx.view, "x");
    let script = local_command(Some(&seq(
        "Hi",
        &["printf 'SPACE-%s\\n' ok", "printf 'RAN-BY:%s\\n' \"$0\""],
    )))
    .expect("a script");
    let terminal = start_sequence_in(&pane(&page), &shell, &glib::home_dir(), &script, None);
    wait_for_text(&terminal, "SPACE-ok");
    wait_for_line(&terminal, &format!("RAN-BY:{shell}"));
    terminal.feed_child(b"echo \"SH=$SHELL\"; echo \"ZERO=$0\"\n");
    wait_for_text(&terminal, &format!("SH={shell}"));
    wait_for_text(&terminal, &format!("ZERO={shell}"));
    fx.click_close(&terminal);
    pump_until("the tab to close", || fx.tabs() == 0);
    fx.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A window's first tab (the launch's, New Window's) runs no sequence, but
/// saves a transcript when Local terminal's own switch is on, as every local
/// tab does; with that switch and "save all" off, none.
pub(super) fn a_window_s_first_tab_runs_no_sequence(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, false);
    set_local(&ctx, &[hello()], Some(0), true);
    let app = test_app();

    let built = main_window(&app, &ctx);
    let made = window::new_window(&app, &ctx);
    for (what, main) in [("build", &built), ("New Window", &made)] {
        let page = main.view.selected_page().expect("its tab");
        let terminal = terminal_of(&page).expect("a local shell");
        alive(&terminal, "first");
        assert!(
            !holds(&terminal, "SEQ-ran"),
            "{what}: {:?}",
            terminal_text(&terminal)
        );
    }
    assert!(files_in(&transcripts).is_empty(), "written when they close");
    built.window.close();
    made.window.close();
    pump_until("both transcripts", || files_in(&transcripts).len() == 2);

    set_local(&ctx, &[hello()], Some(0), false);
    let third = window::new_window(&app, &ctx);
    let page = third.view.selected_page().expect("its tab");
    let terminal = terminal_of(&page).expect("a local shell");
    alive(&terminal, "third");
    third.window.close();
    pump_until("the window to close", || !third.window.is_visible());
    pump_for(Duration::from_millis(300));
    assert_eq!(files_in(&transcripts).len(), 2, "nothing more was saved");
}

/// A local pane a saved session adds back runs Local terminal's default
/// sequence in its folder, as a profile pane runs its profile's (in the home
/// folder when its folder has gone, as a plain shell would start), and saves a
/// transcript when Local terminal's own switch is on. Saved again while a long
/// command of its sequence runs (the shell running it reports no folder), it
/// keeps the folder it started in.
pub(super) fn a_restored_local_pane_runs_the_default_in_its_folder(dir: &Path) {
    let folder = dir.join("Fold er");
    fs::create_dir_all(&folder).expect("the folder");
    let there = canonical(&folder);
    let gone = dir.join("gone");
    let home = canonical(&glib::home_dir());
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, false);
    set_local(
        &ctx,
        &[seq("Where", &["printf 'SEQ-IN:%s\\n' \"$(pwd -P)\""])],
        Some(0),
        true,
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    let file = dir.join(session_file::FILE_NAME);
    let local_in = |dir: &Path| TabRecord {
        root: Node::Pane(pane_record(
            Kind::Local {
                dir: Some(dir.to_path_buf()),
            },
            None,
            1.0,
        )),
        focused: 0,
    };
    save_as(&file, "Work", vec![local_in(&folder), local_in(&gone)]);

    restore(&main.view, &ctx, &saved(&file)[0].tabs);
    assert_eq!(main.view.n_pages(), 3);
    for (index, expected) in [(1, &there), (2, &home)] {
        let terminal = session_in(&pane(&main.view.nth_page(index)));
        wait_for_line(&terminal, &format!("SEQ-IN:{expected}"));
        alive(&terminal, "restored");
    }
    for index in [2, 1] {
        main.view.close_page(&main.view.nth_page(index));
    }
    pump_until("their transcripts", || files_in(&transcripts).len() == 2);
    let texts: Vec<String> = files_in(&transcripts)
        .iter()
        .map(|f| fs::read_to_string(f).expect("the transcript"))
        .collect();
    assert!(
        texts.iter().any(|t| t.contains(&format!("SEQ-IN:{there}"))),
        "{texts:?}"
    );

    set_local(
        &ctx,
        &[seq("Long", &["printf 'M-%s\\n' x", "sleep 600"])],
        Some(0),
        false,
    );
    let before = main.view.n_pages();
    restore(&main.view, &ctx, &saved(&file)[0].tabs[..1]);
    let terminal = session_in(&pane(&main.view.nth_page(before)));
    wait_for_text(&terminal, "M-x");
    assert_eq!(
        capture(&main.view).last().expect("the restored tab").root,
        Node::Pane(pane_record(
            Kind::Local {
                dir: Some(folder.clone())
            },
            None,
            1.0
        )),
        "saved while its sequence runs: {:?}",
        terminal_text(&terminal)
    );
    main.view.close_page(&main.view.nth_page(before));
    pump_until("the tab to close", || main.view.n_pages() == before);

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}
