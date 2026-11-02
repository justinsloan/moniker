//! Scenarios: replacing a pane's session (issue #20). The New Session window
//! ([`new_tab::choose_new_session`]) shows the new-tab list over a pane; a pick
//! ends the pane's session exactly as closing the pane would (its transcript
//! written once, its child hung up and kept until its exit is seen, its
//! one-time marker released only then, its shared connection to a jump host
//! ended) and starts the new one in the same pane, where it was, at the same
//! text size, with the new session's own title. What the old session's
//! terminal does afterwards (its exit, a title) never reaches the pane.
//! Closing the window, or Escape, changes nothing; a pick after the pane went,
//! or a stale one, starts nothing.
//!
//! The scenarios open the window directly, as the menu items that offer it
//! do. Local shells are `/bin/sh` (the window suite has no `$SHELL`),
//! connections run a fake `ssh` through the test-only program hook of
//! [`set_test_ssh_program`]. A shell that must show it was hung up runs a
//! command in the foreground under a `trap` on `HUP` that writes a file of the
//! scenario's and exits.

use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::askpass::{TOKEN_ENV, claim_marker, marker_dir};
use moniker_core::secrets::{MemorySecretStore, SecretStore, password_key};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;

use super::jump_hosts::{
    Connected, Fake, bastion, connect, connected, ctx_for, learned, runtime, settled, through,
};
use super::main_window::{ctx_with, ctx_with_secrets, test_app};
use super::save_transcripts::{click, configure, new_list, say, set_save_all};
use super::tabs::{pump_until_freed, run_until_hangup};
use super::transcript_indicator::{layout_of, pane_of, split_with_local};
use super::windows::{main_window, named, ssh_args};
use super::{
    HeldKeyring, all_widgets, fake_ssh, files_in, find_widget, make_helper, pane, profile,
    pump_for, pump_until, shown, wait_for_session, wait_for_text,
};
use crate::context::AppContext;
use crate::launch_args::{Run, Start};
use crate::panes::REC_PREFIX;
use crate::terminal_tab::{
    PaneHolder, PaneKind, close_pane, holds_session, open_local, rename_pane, set_test_ssh_program,
    shown_title, tab_title, terminal_in, zoom_selected,
};
use crate::{keyring, new_tab, window, zoom};

/// The New Session window over `main`, once it shows.
fn new_session_window(main: &window::MainWindow) -> adw::Dialog {
    pump_until("the New Session window", || {
        main.window.visible_dialog().is_some()
    });
    let dialog = main.window.visible_dialog().expect("the window");
    assert_eq!(dialog.title(), new_tab::NEW_SESSION_TITLE);
    dialog
}

/// The list in the New Session window `dialog`.
fn list_of(dialog: &adw::Dialog) -> gtk::ListBox {
    named(dialog, new_tab::LIST_NAME)
        .and_downcast()
        .expect("the list in the window")
}

/// Wait until `dialog` is no longer shown over `main`.
fn gone(main: &window::MainWindow, dialog: &adw::Dialog) {
    pump_until("the New Session window to close", || {
        main.window.visible_dialog().as_ref() != Some(dialog)
    });
}

/// Open the New Session window over `holder`'s pane and pick the row `name`.
fn replace(main: &window::MainWindow, holder: &PaneHolder, ctx: &Rc<AppContext>, name: &str) {
    new_tab::choose_new_session(holder, ctx);
    let dialog = new_session_window(main);
    click(&list_of(&dialog), name);
    gone(main, &dialog);
}

/// The terminal of `holder`, once it is another than `old`.
fn new_terminal_in(holder: &PaneHolder, old: &Terminal) -> Terminal {
    pump_until("the new session's terminal", || {
        terminal_in(holder).is_some_and(|t| &t != old)
    });
    terminal_in(holder).expect("the new session")
}

/// Every terminal shown in a window of this process.
fn terminals_shown() -> usize {
    gtk::Window::list_toplevels()
        .iter()
        .map(|window| all_widgets::<Terminal>(window).len())
        .sum()
}

/// Have the `/bin/sh` in `terminal` set a trap that writes `hup` and exits when
/// it is hung up, then wait in the foreground (a `sleep`, which the hang-up of
/// the pty's foreground group ends too).
fn trap_hangup(terminal: &Terminal, hup: &Path) {
    trap_hangup_then(terminal, hup, "");
}

/// [`trap_hangup`], with the trap running `first` (shell commands ending with
/// `;`) before it writes `hup`.
fn trap_hangup_then(terminal: &Terminal, hup: &Path, first: &str) {
    wait_for_text(terminal, "$");
    terminal.feed_child(
        format!(
            "trap '{first} echo HUP > \"{}\"; exit 129' HUP; echo TRAP-$((40+2)); sleep 600\n",
            hup.display()
        )
        .as_bytes(),
    );
    wait_for_text(terminal, "TRAP-42");
}

/// Press Escape where `widget` is, as the keyboard does there: the key goes up
/// from `widget` through its ancestors, nearest first, and the first shortcut
/// for Escape alone is activated, the window's own included. Panics if none
/// takes it.
fn press_escape(widget: &gtk::Widget) {
    let mut at = Some(widget.clone());
    while let Some(widget) = at {
        let controllers = widget.observe_controllers();
        for i in 0..controllers.n_items() {
            let Some(controller) = controllers
                .item(i)
                .and_downcast::<gtk::ShortcutController>()
            else {
                continue;
            };
            for j in 0..controller.n_items() {
                let Some(shortcut) = controller.item(j).and_downcast::<gtk::Shortcut>() else {
                    continue;
                };
                let escape = shortcut
                    .trigger()
                    .and_downcast::<gtk::KeyvalTrigger>()
                    .is_some_and(|t| t.keyval() == gdk::Key::Escape && t.modifiers().is_empty());
                if escape
                    && let Some(action) = shortcut.action()
                    && action.activate(
                        gtk::ShortcutActionFlags::empty(),
                        &widget,
                        shortcut.arguments().as_ref(),
                    )
                {
                    pump_for(Duration::from_millis(100));
                    return;
                }
            }
        }
        at = widget.parent();
    }
    panic!("nothing takes Escape");
}

/// The one-time askpass token the fake `ssh` in `terminal` was given.
fn token_in(terminal: &Terminal) -> String {
    let prefix = format!("{TOKEN_ENV}=");
    shown(terminal)
        .iter()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
        .filter(|token| token != "unset")
        .expect("a one-time token")
}

/// A local shell renamed `Old`, at a larger text size, is replaced by a
/// profile picked in the New Session window: the shell is hung up (its `trap`
/// writes its file), and the pane's terminal is a new one running the profile,
/// with the keyboard. The pane is named after the profile, not `Old`, keeps
/// its text size and is the profile's. The old shell's exit, seen later, leaves
/// the new session alone, and closing the tab hangs the new one up in turn.
pub(super) fn replacing_a_local_shell_with_a_profile(bin: &Path, dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let old_hup = dir.join("old.hup");
    let new_hup = dir.join("new.hup");
    let fake = fake_ssh(
        bin,
        "ns-profile",
        &format!("echo FAKE-END\n{}", run_until_hangup(&new_hup)),
    );
    let web = profile(&[]);
    let ctx = ctx_with(dir, std::slice::from_ref(&web));
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    let page = main.view.selected_page().expect("the window's tab");
    let holder = pane(&page);
    let old = terminal_in(&holder).expect("a local shell");
    assert!(holds_session(&holder), "a local shell is a session");
    trap_hangup(&old, &old_hup);
    rename_pane(&holder, "Old");
    zoom_selected(&main.view, zoom::larger);
    let scale = holder.scale().get();
    assert_ne!(scale, zoom::DEFAULT);
    assert_eq!(page.title(), "Old");

    new_tab::choose_new_session(&holder, &ctx);
    let dialog = new_session_window(&main);
    let list = list_of(&dialog);
    assert_eq!(
        list.row_at_index(0)
            .map(|row| row.widget_name().to_string()),
        Some(new_tab::LOCAL_ROW.to_owned()),
        "Local terminal first"
    );
    pump_for(Duration::from_millis(300));
    assert!(
        !old_hup.exists(),
        "the old shell runs while the window is open"
    );
    assert_eq!(terminal_in(&holder).as_ref(), Some(&old), "and stays shown");
    click(&list, &web.id);
    gone(&main, &dialog);
    pump_until("the old shell to be hung up", || old_hup.exists());
    let new = new_terminal_in(&holder, &old);
    wait_for_text(&new, "FAKE-END");
    assert!(
        ssh_args(&new).contains(&"alice@app.example.test".to_owned()),
        "{:?}",
        ssh_args(&new)
    );
    assert_eq!(page.title(), tab_title(&web), "the profile's name, not Old");
    assert_eq!(
        holder.title().borrow().renamed(),
        None,
        "the name given went"
    );
    assert_eq!(holder.scale().get(), scale, "the text size is kept");
    assert_eq!(new.font_scale(), scale);
    assert_eq!(
        *holder.kind().borrow(),
        PaneKind::Profile {
            id: web.id.clone(),
            name: web.name.clone(),
        }
    );
    pump_until("the new session to have the keyboard", || new.has_focus());
    // A second pick in the closed window's list (a stale event) starts nothing.
    click(&list, new_tab::LOCAL_ROW);
    pump_for(Duration::from_millis(300));
    assert_eq!(terminal_in(&holder).as_ref(), Some(&new), "one pick only");

    // The old shell's exit, seen now, changes nothing in the pane.
    let weak = old.downgrade();
    drop(old);
    pump_until_freed("the old shell's terminal to be freed", weak);
    pump_for(Duration::from_millis(300));
    assert_eq!(terminal_in(&holder).as_ref(), Some(&new), "the new session");
    assert_eq!(main.view.n_pages(), 1, "its tab is open");
    assert!(!new_hup.exists());
    // Closing the tab ends the new session as it ends any: it was not taken
    // for one whose child had gone.
    main.view.close_page(&page);
    pump_until("the new session to be hung up", || new_hup.exists());

    set_test_ssh_program(None);
    pump_for(Duration::from_millis(200));
}

/// A local shell that saves a transcript ("Save transcripts of all sessions"
/// on) is replaced by a local shell that does not (the setting is off by
/// then): the old session's transcript is written at once, exactly one file
/// holding what it showed, and its REC goes. The old shell, hung up, sets a
/// title as it goes, which the new shell's pane (a local one, which follows
/// its own program's title) does not take. Closing the window later writes no
/// second file for the old session.
pub(super) fn replacing_writes_the_old_transcript_once(dir: &Path) {
    let hup = dir.join("old.hup");
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    // Before the setting is on, so the window's own first tab saves nothing.
    let main = main_window(&app, &ctx);
    configure(&ctx, &transcripts, true);

    let (page, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    let holder = pane(&page);
    let old = terminal_in(&holder).expect("a local shell");
    say(&old, "old");
    trap_hangup_then(&old, &hup, r#"printf "\033]0;stale\007";"#);
    assert!(holder.recording().get(), "it saves a transcript");
    assert!(page.title().starts_with(REC_PREFIX), "{}", page.title());
    assert!(files_in(&transcripts).is_empty(), "nothing written yet");

    set_save_all(&ctx, false);
    replace(&main, &holder, &ctx, new_tab::LOCAL_ROW);
    let new = new_terminal_in(&holder, &old);
    say(&new, "new");
    let files = files_in(&transcripts);
    assert_eq!(files.len(), 1, "{files:?}");
    let body = fs::read_to_string(&files[0]).expect("the transcript");
    assert!(body.contains("old-42"), "{body:?}");
    assert!(!body.contains("new-42"), "{body:?}");
    assert!(!holder.recording().get(), "REC went with the old session");
    assert!(!page.title().starts_with(REC_PREFIX), "{}", page.title());

    // The old shell's last words, seen once it is hung up, name nothing.
    pump_until("the old shell to be hung up", || hup.exists());
    let weak = old.downgrade();
    drop(old);
    pump_until_freed("the old shell's terminal to be freed", weak);
    pump_for(Duration::from_millis(300));
    assert_eq!(page.title(), shown_title(&holder));
    assert_ne!(page.title(), "stale", "the old program named the new pane");
    assert_eq!(terminal_in(&holder).as_ref(), Some(&new));

    main.window.close();
    pump_for(Duration::from_millis(500));
    assert_eq!(
        files_in(&transcripts).len(),
        1,
        "no second file for the old session"
    );
}

/// In a tab split in two, the second pane's session is replaced: the first
/// pane keeps its terminal, which goes on running, and the second pane stays
/// where it was, in the same split, with the new session in it.
pub(super) fn replacing_in_a_split_keeps_the_place(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "ns-split", "echo FAKE-END\nexec sleep 600");
    let web = profile(&[]);
    let ctx = ctx_with(dir, std::slice::from_ref(&web));
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    let page = main.view.selected_page().expect("the window's tab");
    let first = pane(&page);
    let first_terminal = terminal_in(&first).expect("the first pane's shell");
    let second = split_with_local(&main, &page);
    let second_old = terminal_in(&second).expect("the second pane's shell");
    say(&first_terminal, "first");
    let split = pane_of(&second).parent().expect("the split");
    assert_eq!(layout_of(&page).holders(), [first.clone(), second.clone()]);

    replace(&main, &second, &ctx, &web.id);
    let new = new_terminal_in(&second, &second_old);
    wait_for_text(&new, "FAKE-END");
    assert_eq!(layout_of(&page).holders(), [first.clone(), second.clone()]);
    assert_eq!(
        pane_of(&second).parent().as_ref(),
        Some(&split),
        "same split"
    );
    assert_eq!(
        split
            .downcast_ref::<gtk::Paned>()
            .expect("a split")
            .end_child()
            .as_ref(),
        Some(pane_of(&second).upcast_ref::<gtk::Widget>()),
        "on the same side"
    );
    assert_eq!(
        terminal_in(&first).as_ref(),
        Some(&first_terminal),
        "the other pane keeps its terminal"
    );
    say(&first_terminal, "still");

    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The New Session window closed without a pick changes nothing: closed, with
/// Escape where the keyboard is (on Local terminal), and with Escape in its
/// search while typing. The old terminal is the same, its shell was not hung
/// up and runs on, its name is kept, and the keyboard is back in it.
pub(super) fn cancel_changes_nothing(dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let hup = dir.join("cancel.hup");
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the window's tab");
    let holder = pane(&page);
    let old = terminal_in(&holder).expect("a local shell");
    trap_hangup(&old, &hup);
    rename_pane(&holder, "Old");
    let unchanged = |what: &str| {
        pump_for(Duration::from_millis(300));
        assert!(!hup.exists(), "{what}: the shell was hung up");
        assert_eq!(terminal_in(&holder).as_ref(), Some(&old), "{what}");
        assert_eq!(page.title(), "Old", "{what}");
        assert_eq!(
            *holder.kind().borrow(),
            PaneKind::Local { started_in: None },
            "{what}"
        );
        pump_until(
            &format!("the keyboard back in the shell after {what}"),
            || old.has_focus(),
        );
    };

    // Closed. A pick in its list afterwards (a stale event) starts nothing.
    new_tab::choose_new_session(&holder, &ctx);
    let dialog = new_session_window(&main);
    let list = list_of(&dialog);
    dialog.close();
    gone(&main, &dialog);
    click(&list, new_tab::LOCAL_ROW);
    unchanged("closing the window");

    // Escape on Local terminal, which has the keyboard when the window opens.
    new_tab::choose_new_session(&holder, &ctx);
    let dialog = new_session_window(&main);
    pump_until("Local terminal to have the keyboard", || {
        gtk::prelude::GtkWindowExt::focus(&main.window)
            .is_some_and(|f| f.widget_name() == new_tab::LOCAL_ROW)
    });
    press_escape(&gtk::prelude::GtkWindowExt::focus(&main.window).expect("the focus"));
    gone(&main, &dialog);
    unchanged("Escape");

    // Escape in the search, after typing.
    new_tab::choose_new_session(&holder, &ctx);
    let dialog = new_session_window(&main);
    let search: gtk::SearchEntry = find_widget(&dialog).expect("the search entry");
    search.set_text("zz");
    let text: gtk::Text = find_widget(&search).expect("the search entry's text");
    press_escape(text.upcast_ref());
    gone(&main, &dialog);
    unchanged("Escape in the search");

    // The shell runs on: it answers once its foreground command is stopped.
    old.feed_child(b"\x03");
    say(&old, "alive");

    // With the keyboard on a pane's header when the window opened (its menu
    // button, as when the header's menu asks for the window), it goes back to
    // the pane's terminal, not to the button.
    let second = split_with_local(&main, &page);
    let second_terminal = terminal_in(&second).expect("the second pane's shell");
    let header_menu: gtk::MenuButton =
        find_widget(pane_of(&second).header()).expect("the header's menu button");
    assert!(
        header_menu.grab_focus(),
        "the header's menu takes the keyboard"
    );
    pump_until("the keyboard on the header", || {
        !second_terminal.has_focus()
    });
    new_tab::choose_new_session(&second, &ctx);
    let dialog = new_session_window(&main);
    dialog.close();
    gone(&main, &dialog);
    pump_until("the keyboard back in the pane's terminal", || {
        second_terminal.has_focus()
    });
    assert_eq!(terminal_in(&second).as_ref(), Some(&second_terminal));

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A connection still waiting for the keyring is a session to replace: picked
/// for Local terminal, the pane runs the local shell, and once the keyring
/// answers the connection never starts there.
pub(super) fn a_waiting_connection_replaced_never_starts(bin: &Path, dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let ran = dir.join("ran");
    let fake = fake_ssh(
        bin,
        "ns-waiting",
        &format!("touch \"{}\"\necho FAKE-END\nexec sleep 600", ran.display()),
    );
    let web = profile(&[]);
    let keyring = Arc::new(HeldKeyring::new(Arc::new(MemorySecretStore::default())));
    let ctx = ctx_with_secrets(dir, std::slice::from_ref(&web), Arc::clone(&keyring) as _);
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    keyring.hold();
    let (page, list) = new_list(&main, &ctx);
    click(&list, &web.id);
    let holder = pane(&page);
    pump_until("the pane to wait for the keyring", || {
        keyring::pending() > 0
    });
    assert!(terminal_in(&holder).is_none(), "no session yet");
    assert!(holds_session(&holder), "a connection waiting is a session");

    replace(&main, &holder, &ctx, new_tab::LOCAL_ROW);
    pump_until("the local shell", || terminal_in(&holder).is_some());
    let local = terminal_in(&holder).expect("the local shell");
    keyring.release();
    pump_until("the keyring to answer", || keyring::pending() == 0);
    pump_for(Duration::from_millis(500));
    assert_eq!(
        terminal_in(&holder).as_ref(),
        Some(&local),
        "still the shell"
    );
    assert!(!ran.exists(), "the connection never started");
    assert_eq!(
        *holder.kind().borrow(),
        PaneKind::Local { started_in: None }
    );
    say(&local, "local");

    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A pick in the New Session window after its pane went starts nothing
/// anywhere, and closes the window: a pane of a split closed while the window
/// is open, then a tab closed while it is open. The window stays open until
/// the pick (it is over the main window, which stays).
pub(super) fn a_pick_after_the_pane_went_starts_nothing(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);

    // A pane of a split.
    let page = main.view.selected_page().expect("the window's tab");
    let second = split_with_local(&main, &page);
    new_tab::choose_new_session(&second, &ctx);
    let dialog = new_session_window(&main);
    close_pane(&second);
    drop(second);
    pump_for(Duration::from_millis(300));
    assert_eq!(layout_of(&page).holders().len(), 1, "the pane went");
    assert_eq!(
        main.window.visible_dialog().as_ref(),
        Some(&dialog),
        "the window stays open until a pick"
    );
    let before = terminals_shown();
    click(&list_of(&dialog), new_tab::LOCAL_ROW);
    gone(&main, &dialog);
    pump_for(Duration::from_millis(300));
    assert_eq!(terminals_shown(), before, "no new terminal anywhere");
    assert_eq!(layout_of(&page).holders().len(), 1);

    // A tab.
    let (other, list) = new_list(&main, &ctx);
    click(&list, new_tab::LOCAL_ROW);
    let holder = pane(&other);
    pump_until("its shell", || terminal_in(&holder).is_some());
    new_tab::choose_new_session(&holder, &ctx);
    let dialog = new_session_window(&main);
    main.view.close_page(&other);
    pump_for(Duration::from_millis(300));
    assert_eq!(main.view.n_pages(), 1, "the tab went");
    let before = terminals_shown();
    click(&list_of(&dialog), new_tab::LOCAL_ROW);
    gone(&main, &dialog);
    pump_for(Duration::from_millis(300));
    assert_eq!(terminals_shown(), before, "no new terminal anywhere");
    assert!(terminal_in(&holder).is_none(), "nothing in the closed pane");
    assert_eq!(main.view.n_pages(), 1);

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A session through a jump host is replaced by another through the same
/// jump host: the old shared connection is closed and its master ends, as on
/// close; the new session has a shared connection of its own, which the old
/// connection's exit, seen afterwards, leaves alone.
pub(super) fn replacing_a_jump_host_session_closes_its_link(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "ns-jump", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));

    let old = connect(&main.view, &ctx, &web);
    learned(&old);
    let old_pid = fake.pid_of(&old.socket);
    let Connected {
        page,
        holder,
        terminal: old_terminal,
        socket: old_socket,
        link: old_link,
    } = old;
    replace(&main, &holder, &ctx, &web.id);
    assert!(old_link.is_closed(), "the old shared connection is closed");
    pump_until("the old master to end", || !fake.sleeping(old_pid));
    settled(&old_link);

    let new = connected(page, holder.clone());
    assert_ne!(new.socket, old_socket, "a shared connection of its own");
    learned(&new);
    let new_pid = fake.pid_of(&new.socket);
    let weak = old_terminal.downgrade();
    drop(old_terminal);
    pump_until_freed("the old connection's terminal to be freed", weak);
    pump_for(Duration::from_millis(300));
    assert!(!new.link.is_closed(), "the new one is the new session's");
    assert!(fake.sleeping(new_pid), "and its master runs");
    assert!(
        holder
            .jump()
            .borrow()
            .as_ref()
            .is_some_and(|link| Rc::ptr_eq(link, &new.link)),
        "the pane keeps it"
    );
    assert_eq!(terminal_in(&holder).as_ref(), Some(&new.terminal));

    main.view.close_page(&new.page);
    pump_until("the new master to end", || !fake.sleeping(new_pid));
    settled(&new.link);
    fake.end();
    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A connection with a stored password is replaced by another one of the same
/// profile: the old `ssh` is hung up and its one-time marker goes once its exit
/// is seen; the new connection's marker, made meanwhile, stays until its own
/// `ssh` exits.
pub(super) fn replacing_a_connection_releases_only_its_own_marker(bin: &Path, dir: &Path) {
    let run = dir.join("run");
    fs::create_dir_all(&run).expect("the runtime folder");
    let fake = fake_ssh(
        bin,
        "ns-marker",
        &format!(
            "echo FAKE-END\n\
             trap 'touch \"{}/$MONIKER_ASKPASS_TOKEN.hup\"; exit 129' HUP\n\
             sleep 600 &\nwait $!",
            dir.display()
        ),
    );
    let web = profile(&[]);
    let secrets = Arc::new(MemorySecretStore::default());
    secrets
        .set(&password_key(&web.id), "pw-web")
        .expect("store the password");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("store");
    store.upsert(web.clone()).expect("save");
    let ctx = Rc::new(
        AppContext::with_parts(store, secrets as Arc<dyn SecretStore>, make_helper(dir))
            .with_runtime_dir(run.clone()),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake));

    let (page, list) = new_list(&main, &ctx);
    click(&list, &web.id);
    let old = wait_for_session(&page);
    wait_for_text(&old, "FAKE-END");
    // As the helper does for the first password prompt.
    let old_token = token_in(&old);
    assert!(claim_marker(&run, &old_token).expect("the marker"));
    let old_marker = marker_dir(&run).join(&old_token);
    let holder = pane(&page);

    replace(&main, &holder, &ctx, &web.id);
    let new = new_terminal_in(&holder, &old);
    wait_for_text(&new, "FAKE-END");
    let new_token = token_in(&new);
    assert_ne!(new_token, old_token);
    assert!(claim_marker(&run, &new_token).expect("the marker"));
    let new_marker = marker_dir(&run).join(&new_token);
    pump_until("the old ssh to be hung up", || {
        dir.join(format!("{old_token}.hup")).exists()
    });
    pump_until("the old marker to go", || !old_marker.exists());
    let weak = old.downgrade();
    drop(old);
    pump_until_freed("the old connection's terminal to be freed", weak);
    pump_for(Duration::from_millis(300));
    assert!(new_marker.exists(), "the new connection keeps its marker");
    assert_eq!(terminal_in(&holder).as_ref(), Some(&new));

    main.view.close_page(&page);
    pump_until("the new ssh to be hung up", || {
        dir.join(format!("{new_token}.hup")).exists()
    });
    pump_until("the new marker to go", || !new_marker.exists());
    set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A command tab (as `moniker -- COMMAND` opens one) whose command exits with
/// success when it is hung up is replaced by a local shell: its exit, seen
/// after the new shell began, would close the pane as a command's success
/// does, but the pane holds the new shell, which stays.
pub(super) fn a_replaced_command_s_exit_leaves_the_pane_alone(dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let hup = dir.join("command.hup");
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let script = format!(
        "trap 'touch \"{}\"; exit 0' HUP; echo READY-$((40+2)); sleep 600 & wait $!",
        hup.display()
    );
    let start = Start {
        dir: dir.to_path_buf(),
        title: None,
        run: Run::Command {
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            hold: false,
        },
        dir_refused: None,
    };
    let old = open_local(&main.view, zoom::DEFAULT, &start, None);
    wait_for_text(&old, "READY-42");
    let page = main.view.selected_page().expect("the command's tab");
    let holder = pane(&page);
    assert!(holds_session(&holder), "a command is a session");
    let tabs = main.view.n_pages();

    replace(&main, &holder, &ctx, new_tab::LOCAL_ROW);
    let new = new_terminal_in(&holder, &old);
    pump_until("the command to be hung up", || hup.exists());
    let weak = old.downgrade();
    drop(old);
    pump_until_freed("the command's terminal to be freed", weak);
    pump_for(Duration::from_millis(300));
    assert_eq!(main.view.n_pages(), tabs, "the tab stays");
    assert_eq!(
        terminal_in(&holder).as_ref(),
        Some(&new),
        "with the new shell"
    );
    say(&new, "after");

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}
