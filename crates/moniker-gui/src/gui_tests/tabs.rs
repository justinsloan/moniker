//! Scenarios: terminal tabs (argv and environment, exit handling, transcripts,
//! the local shell, titles and renaming, colors) and the notifications that
//! report on them.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::askpass::{TOKEN_ENV, claim_marker, marker_dir};
use moniker_core::secrets::{MemorySecretStore, SecretStore, password_key};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{ctx_with, menu_items, test_app};
use super::{
    HELPER, MARKUP_NAME, all_widgets, button_labeled, fake_command, fake_shell, fake_ssh, files_in,
    find_widget, fixture, labels_in, launch_ctx, make_helper, page_of, pane, profile, pump_for,
    pump_until, shown, tab_window, wait_for_session, wait_for_text, widgets_of_type,
};
use crate::context::{AppContext, Notice};
use crate::notify::NotificationArea;
use crate::terminal_tab::{
    ASKPASS_MISSING_NOTICE, TranscriptSpec, after_the_menu_lets_go, ask_rename,
    end_sessions_on_close, launch, new_holder_page, new_holder_page_at, open_command,
    open_local_shell, rename_page, scale_of, set_test_ssh_program, start_command, start_profile,
    start_shell_in, terminal_of, terminal_text, write_transcripts,
};
use crate::theme;
use crate::window;
use crate::zoom;

fn spec(dir: &Path, notice: &Notice) -> TranscriptSpec {
    TranscriptSpec {
        host: "app.example.test".to_owned(),
        dir: dir.to_path_buf(),
        notice: notice.clone(),
    }
}

pub(super) fn argv_and_environment_reach_the_child(bin: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "plain", "echo FAKE-END\nexec sleep 600");
    let p = profile(&[]);
    let secrets = MemorySecretStore::default();
    secrets.set(&password_key(&p.id), "hunter2").unwrap();

    let cmd = fake_command(&fake, &p, None, &secrets);
    let token = cmd
        .env
        .iter()
        .find(|(k, _)| k == TOKEN_ENV)
        .map(|(_, v)| v.clone())
        .expect("a one-time token");
    let terminal = open_command(&fx.view, "Prod <web>", cmd, None);
    assert_eq!(fx.tabs(), 1);
    let page = fx.page_of(&terminal);
    assert_eq!(fx.view.page_position(&page), 1);
    assert_eq!(
        fx.view.selected_page().as_ref(),
        Some(&page),
        "the new tab is the selected one"
    );
    wait_for_text(&terminal, "FAKE-END");

    assert_eq!(
        shown(&terminal),
        [
            "ARG:-p",
            "ARG:22",
            "ARG:--",
            "ARG:alice@app.example.test",
            &format!("SSH_ASKPASS={}", bin.join(HELPER).display()),
            "SSH_ASKPASS_REQUIRE=force",
            &format!("MONIKER_PROFILE_ID={}", p.id),
            &format!("MONIKER_PID={}", std::process::id()),
            &format!("MONIKER_ASKPASS_TOKEN={token}"),
            // ssh is a child of the Moniker process itself, which the askpass
            // helper checks against MONIKER_PID (a hop's ssh is not).
            &format!("PARENT={}", std::process::id()),
            "FAKE-END",
        ]
    );
    assert!(!terminal_text(&terminal).contains("hunter2"));

    // The tab title is the profile name as plain text, in the page and where the
    // tab bar draws it; the tooltip is markup for GTK, so it is escaped.
    assert_eq!(page.title(), "Prod <web>");
    assert_eq!(page.tooltip().as_deref(), Some("Prod &lt;web&gt;"));
    pump_for(Duration::from_millis(300));
    let drawn = labels_in(&fx.bar)
        .into_iter()
        .find(|l| l.label() == "Prod <web>")
        .expect("the tab bar draws the title");
    assert!(!drawn.uses_markup());

    // Without a transcript the scrollback is bounded: 10,000 lines, not VTE's 512.
    assert_eq!(terminal.scrollback_lines(), 10_000);

    fx.click_close(&terminal);
    assert_eq!(fx.tabs(), 0, "the close button removes the tab");
    fx.window.destroy();
}

pub(super) fn startup_sequence_is_one_literal_argument(bin: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "seq", "echo FAKE-END\nexec sleep 600");
    let p = profile(&[("update", &["uptime", "echo $HOME"])]);
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, Some(0), &MemorySecretStore::default()),
        None,
    );
    wait_for_text(&terminal, "FAKE-END");

    // The remote command is a single argument: its newlines show as line
    // breaks and `$SHELL` and `$HOME` stay literal.
    assert_eq!(
        shown(&terminal),
        [
            "ARG:-p",
            "ARG:22",
            "ARG:-t",
            "ARG:--",
            "ARG:alice@app.example.test",
            "ARG:uptime",
            "echo $HOME",
            "exec \"$SHELL\" -l",
            "SSH_ASKPASS=unset",
            "SSH_ASKPASS_REQUIRE=unset",
            "MONIKER_PROFILE_ID=unset",
            "MONIKER_PID=unset",
            "MONIKER_ASKPASS_TOKEN=unset",
            &format!("PARENT={}", std::process::id()),
            "FAKE-END",
        ]
    );
    fx.click_close(&terminal);
    fx.window.destroy();
}

pub(super) fn clean_exit_closes_the_tab(bin: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "exit0", "echo FAKE-END\nexit 0");
    let p = profile(&[]);
    open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        None,
    );
    assert_eq!(fx.tabs(), 1);
    pump_until("the tab to close after exit 0", || fx.tabs() == 0);
    fx.window.destroy();
}

pub(super) fn failed_exit_keeps_the_tab(bin: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "exit3", "echo FAKE-END\nexit 3");
    let p = profile(&[]);
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        None,
    );
    wait_for_text(&terminal, "[session ended: status 3]");
    pump_for(Duration::from_millis(300));
    assert_eq!(fx.tabs(), 1, "a failed session stays open");
    let lines = shown(&terminal);
    assert!(lines.iter().any(|l| l == "FAKE-END"), "{lines:?}");
    assert_eq!(
        lines.last().map(String::as_str),
        Some("[session ended: status 3]")
    );
    fx.click_close(&terminal);
    assert_eq!(fx.tabs(), 0);

    // A program that cannot be started is reported in the tab, which stays.
    let missing = fx.view.n_pages();
    let mut cmd = fake_command(&fake, &p, None, &MemorySecretStore::default());
    cmd.program = "/nonexistent/moniker-test-ssh".to_owned();
    let terminal = open_command(&fx.view, "x", cmd, None);
    wait_for_text(&terminal, "[could not start /nonexistent/moniker-test-ssh:");
    assert_eq!(fx.view.n_pages(), missing + 1);
    fx.click_close(&terminal);
    fx.window.destroy();
}

pub(super) fn transcript_on_clean_exit(bin: &Path, dir: &Path) {
    let fx = fixture();
    let area = super::bare_area();
    let notice = Notice::default();
    notice.attach(&area);
    // Enough output to push the arguments out of the visible rows.
    let fake = fake_ssh(
        bin,
        "tr-exit",
        "printf 'LONG:%0150d\\n' 7\nseq 1 2000\necho FAKE-END\nexit 0",
    );
    let p = profile(&[]);
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        Some(spec(dir, &notice)),
    );
    let lines = terminal.scrollback_lines();
    assert!(
        !(0..=1_000_000_000).contains(&lines),
        "unlimited, got {lines}"
    );
    pump_until("the tab to close after exit 0", || fx.tabs() == 0);

    let files = files_in(dir);
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("app.example.test_") && name.ends_with(".txt"),
        "{name}"
    );
    let mode = fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let body = fs::read_to_string(&files[0]).unwrap();
    assert!(
        body.starts_with("ARG:-p\nARG:22\nARG:--\n"),
        "the scrolled-off start is included: {:?}",
        &body[..body.len().min(60)]
    );
    assert!(body.contains("\n1\n2\n3\n"), "scrollback rows are in order");
    assert!(
        body.ends_with("\n1999\n2000\nFAKE-END\n"),
        "the end of the output"
    );
    // A line longer than the terminal is wide comes back as one line.
    let long = format!("LONG:{:0>150}\n", 7);
    assert!(body.contains(&long), "soft-wrapped rows are joined");
    assert!(!body.contains('\u{1b}'), "no escape sequences");
    assert!(!body.contains("session ended"));
    // A saved transcript says nothing: only a failure is worth a notification.
    assert_eq!(notice.last_text(), None);
    fx.window.destroy();
}

pub(super) fn transcript_on_tab_close_is_written_once(bin: &Path, dir: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "tr-close", "echo FAKE-END\nexec sleep 600");
    let p = profile(&[]);
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        Some(spec(dir, &Notice::default())),
    );
    wait_for_text(&terminal, "FAKE-END");
    assert!(files_in(dir).is_empty(), "nothing is written while it runs");

    fx.click_close(&terminal);
    assert_eq!(fx.tabs(), 0);
    let files = files_in(dir);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(
        fs::read_to_string(&files[0])
            .unwrap()
            .ends_with("FAKE-END\n")
    );

    // The window closing afterwards finds nothing left to write.
    write_transcripts(&fx.view);
    pump_for(Duration::from_millis(300));
    assert_eq!(files_in(dir).len(), 1);
    fx.window.destroy();
}

pub(super) fn transcript_after_failed_exit_is_written_once(bin: &Path, dir: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "tr-fail", "echo FAKE-END\nexit 2");
    let p = profile(&[]);
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        Some(spec(dir, &Notice::default())),
    );
    wait_for_text(&terminal, "[session ended: status 2]");
    pump_for(Duration::from_millis(300));
    assert_eq!(fx.tabs(), 1, "the tab stays after a failure");
    let files = files_in(dir);
    assert_eq!(files.len(), 1, "written when the child exited: {files:?}");
    let body = fs::read_to_string(&files[0]).unwrap();
    assert!(body.ends_with("FAKE-END\n"), "the note is not part of it");

    // Closing the tab, and then the window, do not write it again.
    fx.click_close(&terminal);
    write_transcripts(&fx.view);
    assert_eq!(files_in(dir), files);
    fx.window.destroy();
}

pub(super) fn transcript_on_window_close(bin: &Path, dir: &Path) {
    let fx = fixture();
    end_sessions_on_close(&fx.window, &fx.view);
    let fake = fake_ssh(bin, "tr-win", "echo FAKE-END\nexec sleep 600");
    let p = profile(&[]);
    let terminals: Vec<Terminal> = (0..2)
        .map(|_| {
            open_command(
                &fx.view,
                "x",
                fake_command(&fake, &p, None, &MemorySecretStore::default()),
                Some(spec(dir, &Notice::default())),
            )
        })
        .collect();
    for terminal in &terminals {
        wait_for_text(terminal, "FAKE-END");
    }
    assert!(files_in(dir).is_empty());

    fx.window.close();
    pump_for(Duration::from_millis(300));
    // Two sessions started in the same second get two different files.
    let files = files_in(dir);
    assert_eq!(files.len(), 2, "{files:?}");
    for file in &files {
        assert!(fs::read_to_string(file).unwrap().ends_with("FAKE-END\n"));
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // Asking again writes nothing more.
    write_transcripts(&fx.view);
    assert_eq!(files_in(dir), files);
}

pub(super) fn no_transcript_without_a_spec(bin: &Path, dir: &Path) {
    let fx = fixture();
    end_sessions_on_close(&fx.window, &fx.view);
    let fake = fake_ssh(bin, "no-tr", "echo FAKE-END\nexec sleep 600");
    let p = profile(&[]);
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        None,
    );
    wait_for_text(&terminal, "FAKE-END");
    fx.click_close(&terminal);
    fx.window.close();
    pump_for(Duration::from_millis(200));
    assert!(files_in(dir).is_empty());
}

pub(super) fn missing_helper_falls_back_to_the_terminal_and_says_so(dir: &Path) {
    let p = profile(&[]);
    let transcripts = dir.join("transcripts");
    // The tab being opened, in a window of its own.
    let view = adw::TabView::new();
    crate::terminal_tab::setup_tab_view(&view);
    view.append(&gtk::Label::new(Some("keeper")));
    let (window, area) = window_with_area(&Notice::default(), &view);
    let page = new_holder_page(&view, "x");

    // A password is stored but the helper is not there: the command is the plain
    // one (ssh asks in the terminal) and a notification says why, in the tab's
    // window.
    let (ctx, _attached, _secrets) = launch_ctx(dir, dir.join("no-such-helper"));
    let launched = launch(&pane(&page), &p, None, false, &ctx, &transcripts, true);
    assert!(
        launched.command.env.is_empty(),
        "{:?}",
        launched.command.env
    );
    let said = ctx.notice.last_text().expect("a notification");
    assert_eq!(said, ASKPASS_MISSING_NOTICE);
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&area));
    assert!(!said.contains("hunter2"));

    // The helper is there: ssh is pointed at it and nothing is said.
    let (ctx, _area, _secrets) = launch_ctx(dir, make_helper(dir));
    let launched = launch(&pane(&page), &p, None, false, &ctx, &transcripts, true);
    let names: Vec<&str> = launched
        .command
        .env
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "SSH_ASKPASS",
            "SSH_ASKPASS_REQUIRE",
            "MONIKER_PROFILE_ID",
            "MONIKER_PID",
            "MONIKER_ASKPASS_TOKEN"
        ]
    );
    assert_eq!(ctx.notice.last_text(), None);

    // This context has no runtime directory: there is no marker to remove
    // (the helper, in the same environment, cannot make one and asks instead).
    assert!(launched.marker.is_none());

    // No password stored and no helper: nothing to explain.
    let (ctx, _area, _secrets) = launch_ctx(dir, dir.join("no-such-helper"));
    let launched = launch(&pane(&page), &p, None, false, &ctx, &transcripts, false);
    assert!(launched.command.env.is_empty());
    assert!(launched.marker.is_none());
    assert_eq!(ctx.notice.last_text(), None);
    window.destroy();
}

/// A fake ssh's last command: wait until hung up, then note it in `hup`.
fn run_until_hangup(hup: &Path) -> String {
    format!(
        "trap 'touch \"{}\"; exit 129' HUP\nsleep 600 &\nwait $!",
        hup.display()
    )
}

/// Run the main loop until `terminal` has been freed.
fn pump_until_freed(what: &str, terminal: glib::WeakRef<Terminal>) {
    pump_until(what, || terminal.upgrade().is_none());
}

/// A connection's one-time askpass marker is removed when its `ssh` has exited:
/// by itself (even if the tab stays open to show why), or after its tab or
/// window was closed, which hangs it up. An `ssh` that ignores the hangup keeps
/// its marker until it does exit. A tab moved to another window keeps its
/// marker and its session. A closed tab's terminal is freed once its child has
/// gone.
pub(super) fn the_marker_is_released_when_the_session_ends(bin: &Path, dir: &Path) {
    let runtime = dir.join("run");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let secrets = Arc::new(MemorySecretStore::default());
    let ctx = AppContext::with_parts(
        ProfileStore::open(&dir.join("profiles.json")).expect("store"),
        Arc::clone(&secrets) as Arc<dyn SecretStore>,
        make_helper(dir),
    )
    .with_runtime_dir(runtime.clone());
    let p = profile(&[]);
    secrets.set(&password_key(&p.id), "hunter2").unwrap();

    // Connect in a new tab of `view`, with a fake ssh that prints FAKE-END and
    // then runs `then`, and make the marker as the helper does for the first
    // password prompt. Returns the terminal and the marker's path.
    let connect = |view: &adw::TabView, name: &str, then: &str| -> (Terminal, PathBuf) {
        let fake = fake_ssh(bin, name, &format!("echo FAKE-END\n{then}"));
        let page = new_holder_page(view, "x");
        let mut launched = launch(
            &pane(&page),
            &p,
            None,
            false,
            &ctx,
            &dir.join("transcripts"),
            true,
        );
        launched.command.program = fake;
        let token = launched
            .command
            .env
            .iter()
            .find(|(k, _)| k == TOKEN_ENV)
            .map(|(_, v)| v.clone())
            .expect("a one-time token");
        assert!(launched.marker.is_some(), "the tab is told what to remove");
        assert!(claim_marker(&runtime, &token).unwrap());
        let marker = marker_dir(&runtime).join(&token);
        let terminal = start_command(
            &pane(&page),
            &launched.title,
            launched.command,
            launched.transcript,
            launched.marker,
        );
        wait_for_text(&terminal, "FAKE-END");
        (terminal, marker)
    };

    // ssh fails (a wrong password, say): the tab stays, the marker goes.
    let fx = fixture();
    let (terminal, marker) = connect(&fx.view, "m-fail", "exit 255");
    wait_for_text(&terminal, "[session ended: status 255]");
    assert!(!marker.exists(), "removed when ssh exits");
    assert_eq!(fx.tabs(), 1, "the tab stays to show why");
    let weak = terminal.downgrade();
    fx.click_close(&terminal);
    drop(terminal);
    pump_until_freed("the failed tab's terminal to be freed", weak);

    // A clean exit closes the tab, and the marker goes.
    let (terminal, marker) = connect(&fx.view, "m-exit", "exit 0");
    let weak = terminal.downgrade();
    drop(terminal);
    pump_until("the tab to close after exit 0", || fx.tabs() == 0);
    assert!(!marker.exists());
    pump_until_freed("the exited tab's terminal to be freed", weak);

    // The tab is closed while ssh runs: ssh is hung up, exits, and only then
    // does the marker go; the terminal is freed.
    let hup = dir.join("m-close.hup");
    let (terminal, marker) = connect(&fx.view, "m-close", &run_until_hangup(&hup));
    pump_for(Duration::from_millis(100));
    assert!(marker.exists(), "kept while the session runs");
    let weak = terminal.downgrade();
    fx.click_close(&terminal);
    drop(terminal);
    pump_until("the closed tab's ssh to be hung up", || hup.exists());
    pump_until("the closed tab's marker to go", || !marker.exists());
    pump_until_freed("the closed tab's terminal to be freed", weak);

    // An ssh that ignores the hangup (Moniker started under nohup, say) lives
    // on: its marker stays until it does exit.
    let pid_file = dir.join("m-deaf.pid");
    let (terminal, marker) = connect(
        &fx.view,
        "m-deaf",
        &format!(
            "trap '' HUP\necho $$ > \"{}\"\nexec sleep 600",
            pid_file.display()
        ),
    );
    pump_until("the deaf ssh's pid", || {
        fs::read_to_string(&pid_file).is_ok_and(|s| s.ends_with('\n'))
    });
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let weak = terminal.downgrade();
    fx.click_close(&terminal);
    drop(terminal);
    pump_for(Duration::from_millis(500));
    assert!(
        marker.exists(),
        "an ssh that may still ask keeps its marker"
    );
    // End it (this test started it: its own session and process group).
    let group = rustix::process::Pid::from_raw(pid).expect("a pid");
    rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
        .expect("kill the fake ssh");
    pump_until("the deaf ssh's marker to go", || !marker.exists());
    pump_until_freed("the deaf tab's terminal to be freed", weak);

    // Moved to another window, the session goes on and keeps its marker; it is
    // removed when the tab is closed over there.
    let other = fixture();
    let hup = dir.join("m-move.hup");
    let (terminal, marker) = connect(&fx.view, "m-move", &run_until_hangup(&hup));
    let page = fx.page_of(&terminal);
    fx.view.transfer_page(&page, &other.view, 0);
    drop(page);
    pump_for(Duration::from_millis(300));
    assert_eq!(other.tabs(), 1);
    assert!(marker.exists(), "moving a tab removes nothing");
    assert!(!hup.exists(), "and ends nothing");
    other.click_close(&terminal);
    drop(terminal);
    pump_until("the moved tab's ssh to be hung up", || hup.exists());
    pump_until("the moved tab's marker to go", || !marker.exists());
    other.window.destroy();

    // The window is closed with the session running, as the app's windows
    // close: ssh is hung up and the marker goes.
    end_sessions_on_close(&fx.window, &fx.view);
    let hup = dir.join("m-window.hup");
    let (terminal, marker) = connect(&fx.view, "m-window", &run_until_hangup(&hup));
    drop(terminal);
    pump_for(Duration::from_millis(100));
    assert!(marker.exists());
    fx.window.close();
    pump_until("the closed window's ssh to be hung up", || hup.exists());
    pump_until("the closed window's marker to go", || !marker.exists());
}

/// Closing a local tab whose shell runs a job in the foreground (in a process
/// group of its own) hangs up both the job and the shell, and frees the
/// terminal: nothing of the tab runs on unseen.
pub(super) fn a_closed_local_tab_hangs_up_its_shell_and_its_job(bin: &Path, dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let shell_hup = dir.join("shell.hup");
    let job_hup = dir.join("job.hup");
    // `set -m`: job control, so the job runs in a group of its own and is the
    // pty's foreground group, as in an interactive shell.
    let shell = fake_shell(
        bin,
        "job-shell",
        &format!(
            "set -m\n\
             trap 'touch \"{shell}\"; exit 129' HUP\n\
             sh -c 'trap \"touch \\\"{job}\\\"; exit 129\" HUP; echo JOB-READY; sleep 600 & wait $!'\n\
             echo JOB-ENDED\n\
             sleep 600 &\n\
             wait $!",
            shell = shell_hup.display(),
            job = job_hup.display(),
        ),
    );
    let fx = fixture();
    let page = new_holder_page(&fx.view, "x");
    let terminal = start_shell_in(&pane(&page), &shell, dir, None);
    drop(page);
    wait_for_text(&terminal, "JOB-READY");
    let weak = terminal.downgrade();
    fx.click_close(&terminal);
    drop(terminal);
    pump_until("the job to be hung up", || job_hup.exists());
    pump_until("the shell to be hung up", || shell_hup.exists());
    pump_until_freed("the closed local tab's terminal to be freed", weak);
    fx.window.destroy();
}

/// The "moniker-askpass not found" notification is about the tab being opened,
/// so it appears in that tab's window, not in whichever window is active or was
/// attached last: connecting from the Connection Profiles window opens (and
/// raises) the tab in a main window.
pub(super) fn the_missing_helper_notification_lands_in_the_tabs_window(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "askpass-toast", "echo FAKE-END\nexec sleep 600");
    let p = profile(&[]);
    let (ctx, _first, secrets) = launch_ctx(dir, dir.join("no-such-helper"));
    secrets.set(&password_key(&p.id), "hunter2").unwrap();

    let view = adw::TabView::new();
    crate::terminal_tab::setup_tab_view(&view);
    view.append(&gtk::Label::new(Some("keeper")));
    let (window, tab_area) = window_with_area(&ctx.notice, &view);
    // Another window, attached later and presented last: the Connection Profiles window.
    let (profiles_window, profiles_area) =
        window_with_area(&ctx.notice, &gtk::Label::new(Some("profiles")));
    assert_ne!(tab_area, profiles_area);

    set_test_ssh_program(Some(fake));
    let page = new_holder_page(&view, "x");
    start_profile(&pane(&page), &ctx, &p, None, false);
    let terminal = wait_for_session(&page);
    set_test_ssh_program(None);

    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some(ASKPASS_MISSING_NOTICE)
    );
    assert_eq!(
        ctx.notice.last_area().as_ref(),
        Some(&tab_area),
        "the notification is in the tab's window"
    );
    wait_for_text(&terminal, "FAKE-END");
    window.destroy();
    profiles_window.destroy();
    pump_for(Duration::from_millis(200));
}

pub(super) fn local_shell_has_the_bounded_default_scrollback() {
    let fx = fixture();
    // The environment has no $SHELL: the tab runs /bin/sh.
    let terminal = open_local_shell(&fx.view, zoom::DEFAULT, None);
    assert_eq!(fx.tabs(), 1);
    assert_eq!(fx.view.selected_page(), Some(fx.page_of(&terminal)));
    assert_eq!(terminal.scrollback_lines(), 10_000);
    // Closed at once, very likely before the shell has started: it is hung up
    // when it does start, exits, and the terminal is freed.
    let weak = terminal.downgrade();
    fx.click_close(&terminal);
    drop(terminal);
    assert_eq!(fx.tabs(), 0);
    pump_until_freed("the terminal closed while starting to be freed", weak);
    fx.window.destroy();
}

/// The shell's working directory, as the canonical path the shell reports.
fn canonical(dir: &Path) -> String {
    fs::canonicalize(dir)
        .expect("an existing directory")
        .to_string_lossy()
        .into_owned()
}

/// A local shell starts in the person's home directory, not in the directory the
/// app was started from; a home directory that is not there falls back to that.
pub(super) fn local_shell_starts_in_home(bin: &Path, dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let here = canonical(&std::env::current_dir().expect("a working directory"));
    let home = dir.join("home");
    fs::create_dir_all(&home).expect("home");
    let home_path = canonical(&home);
    assert_ne!(home_path, here, "the test needs a home that is elsewhere");

    let fx = fixture();
    let shell = fake_shell(bin, "pwd-shell", "echo \"CWD:$(pwd -P)\"\nexec sleep 600");

    let page = new_holder_page(&fx.view, "x");
    let terminal = start_shell_in(&pane(&page), &shell, &home, None);
    wait_for_text(&terminal, &format!("CWD:{home_path}"));
    fx.click_close(&terminal);

    let page = new_holder_page(&fx.view, "x");
    let terminal = start_shell_in(&pane(&page), &shell, &dir.join("no-such-home"), None);
    wait_for_text(&terminal, &format!("CWD:{here}"));
    fx.click_close(&terminal);

    // The real thing: no shell configured, so /bin/sh, in `glib::home_dir()`.
    let real_home = canonical(&glib::home_dir());
    assert_ne!(real_home, here, "the test needs a home that is elsewhere");
    let terminal = open_local_shell(&fx.view, zoom::DEFAULT, None);
    // Wait for the prompt: input sent before the shell has started is lost.
    wait_for_text(&terminal, "$");
    terminal.feed_child(b"echo REAL:$(pwd -P)\n");
    wait_for_text(&terminal, &format!("REAL:{real_home}"));
    fx.click_close(&terminal);
    fx.window.destroy();
}

/// A local tab takes the title its shell sets, until the person renames it; a
/// rename that is cleared goes back to the shell's latest title.
pub(super) fn a_local_tab_follows_its_shell_until_renamed(bin: &Path) {
    let fx = fixture();
    let shell = fake_shell(
        bin,
        "title-shell",
        "printf '\\033]0;from the shell\\007'\necho READY\nread line\n\
         printf '\\033]0;later\\007'\necho DONE\nexec sleep 600",
    );
    let page = new_holder_page(&fx.view, "x");
    let terminal = start_shell_in(&pane(&page), &shell, Path::new("/"), None);
    assert_eq!(page.title(), "Local terminal", "until the shell says");
    pump_until("the shell's title", || page.title() == "from the shell");
    assert_eq!(page.tooltip().as_deref(), Some("from the shell"));

    rename_page(&page, "mine");
    wait_for_text(&terminal, "READY");
    terminal.feed_child(b"go\n");
    wait_for_text(&terminal, "DONE");
    assert_eq!(terminal.window_title().as_deref(), Some("later"));
    assert_eq!(page.title(), "mine", "a renamed tab keeps its name");

    rename_page(&page, "");
    assert_eq!(page.title(), "later");
    fx.click_close(&terminal);
    fx.window.destroy();
}

/// A remote tab is named after its profile whatever the remote side says its
/// title is.
pub(super) fn remote_tab_keeps_its_name_when_the_shell_sets_a_title(bin: &Path) {
    let fx = fixture();
    let fake = fake_ssh(
        bin,
        "evil-title",
        "printf '\\033]0;evil\\007'\necho FAKE-END\nexec sleep 600",
    );
    let p = profile(&[]);
    let terminal = open_command(
        &fx.view,
        "Prod web",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        None,
    );
    wait_for_text(&terminal, "FAKE-END");
    assert_eq!(
        terminal.window_title().as_deref(),
        Some("evil"),
        "the terminal did receive the title"
    );
    pump_for(Duration::from_millis(200));
    let page = fx.page_of(&terminal);
    assert_eq!(page.title(), "Prod web");
    assert_eq!(page.tooltip().as_deref(), Some("Prod web"));
    fx.click_close(&terminal);
    fx.window.destroy();
}

/// Closing the last tab closes the window, by the tab being closed or by its
/// session ending; closing one of several does not.
pub(super) fn closing_the_last_tab_closes_the_window(bin: &Path) {
    let tw = tab_window();
    let first = open_local_shell(&tw.view, zoom::DEFAULT, None);
    let second = open_local_shell(&tw.view, zoom::DEFAULT, None);
    pump_for(Duration::from_millis(200));
    assert_eq!(tw.view.n_pages(), 2);

    tw.view.close_page(&page_of(&tw.view, &first));
    pump_for(Duration::from_millis(200));
    assert_eq!(tw.view.n_pages(), 1);
    assert!(tw.window.is_visible(), "one tab is left");

    tw.view.close_page(&page_of(&tw.view, &second));
    pump_until("the window to close", || !tw.window.is_visible());
    assert_eq!(tw.view.n_pages(), 0);

    // The same when the last session ends by itself.
    let tw = tab_window();
    let fake = fake_ssh(bin, "last-exit", "echo FAKE-END\nexit 0");
    open_command(
        &tw.view,
        "x",
        fake_command(&fake, &profile(&[]), None, &MemorySecretStore::default()),
        None,
    );
    assert!(tw.window.is_visible());
    pump_until("the window to close after the session ended", || {
        !tw.window.is_visible()
    });
}

/// `rename_page` shows the new name in the tab, with its tooltip escaped; a blank
/// name goes back to the automatic one.
pub(super) fn rename_sets_title_and_escaped_tooltip() {
    let fx = fixture();
    let page = new_holder_page(&fx.view, "Plain");
    assert_eq!(page.title(), "Plain");
    rename_page(&page, MARKUP_NAME);
    assert_eq!(page.title(), MARKUP_NAME);
    assert_eq!(
        page.tooltip().as_deref(),
        Some(glib::markup_escape_text(MARKUP_NAME).as_str()),
        "the tooltip is markup for GTK"
    );
    // And the tab bar draws the name as it is.
    pump_for(Duration::from_millis(300));
    let drawn = labels_in(&fx.bar)
        .into_iter()
        .find(|l| l.label() == MARKUP_NAME)
        .expect("the tab bar draws the new name");
    assert!(!drawn.uses_markup());

    rename_page(&page, " \t");
    assert_eq!(page.title(), "Plain");
    assert_eq!(page.tooltip().as_deref(), Some("Plain"));
    rename_page(&page, "a\x1b[31mb\n");
    assert_eq!(page.title(), "a [31mb");
    fx.window.destroy();
}

/// The rename dialog open over `window`, if there is one.
fn rename_dialog(window: &impl IsA<gtk::Widget>) -> Option<(adw::AlertDialog, gtk::Entry)> {
    let dialog: adw::AlertDialog = find_widget(window)?;
    let entry = dialog.extra_child().and_downcast::<gtk::Entry>()?;
    Some((dialog, entry))
}

pub(super) fn ask_rename_renames_the_page() {
    let fx = fixture();
    let page = new_holder_page(&fx.view, MARKUP_NAME);
    let other = new_holder_page(&fx.view, "Other");
    assert!(rename_dialog(&fx.window).is_none());

    // Cancel changes nothing, and the field starts out as the title shown.
    ask_rename(&page);
    pump_for(Duration::from_millis(300));
    let (dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    assert_eq!(dialog.heading().as_deref(), Some("Rename Tab"));
    assert_eq!(entry.text(), MARKUP_NAME);
    entry.set_text("scratch");
    button_labeled(&dialog, "Cancel")
        .expect("a Cancel button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    assert_eq!(page.title(), MARKUP_NAME);
    assert!(rename_dialog(&fx.window).is_none(), "Cancel closes it");

    // Rename applies the name.
    ask_rename(&page);
    pump_for(Duration::from_millis(300));
    let (dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    entry.set_text("prod");
    button_labeled(&dialog, "Rename")
        .expect("a Rename button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    assert_eq!(page.title(), "prod");
    assert_eq!(other.title(), "Other");
    assert!(rename_dialog(&fx.window).is_none());

    // Enter in the field is Rename.
    ask_rename(&page);
    pump_for(Duration::from_millis(300));
    let (_dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    assert_eq!(entry.text(), "prod");
    entry.set_text("staging");
    // Enter in the field: the entry's inner text widget emits "activate".
    entry
        .delegate()
        .expect("the entry's text widget")
        .emit_by_name::<()>("activate", &[]);
    pump_for(Duration::from_millis(300));
    assert_eq!(page.title(), "staging");
    fx.window.destroy();
}

/// Pressing Rename on a title left as it was does not freeze it: a local tab
/// goes on following its shell.
pub(super) fn renaming_an_unchanged_title_keeps_following_the_shell(bin: &Path) {
    let fx = fixture();
    let shell = fake_shell(
        bin,
        "title-shell-2",
        "printf '\\033]0;one\\007'\necho READY\nread line\n\
         printf '\\033]0;two\\007'\necho DONE\nexec sleep 600",
    );
    let page = new_holder_page(&fx.view, "x");
    let terminal = start_shell_in(&pane(&page), &shell, Path::new("/"), None);
    pump_until("the shell's title", || page.title() == "one");

    ask_rename(&page);
    pump_for(Duration::from_millis(300));
    let (dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    assert_eq!(entry.text(), "one");
    button_labeled(&dialog, "Rename")
        .expect("a Rename button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));

    wait_for_text(&terminal, "READY");
    terminal.feed_child(b"go\n");
    wait_for_text(&terminal, "DONE");
    pump_until("the new title", || page.title() == "two");
    fx.click_close(&terminal);
    fx.window.destroy();
}

/// The tab menu has "Rename…", which renames the tab the menu was opened for
/// (not the selected one).
pub(super) fn the_tab_menu_renames_the_tab_it_was_opened_for() {
    let fx = fixture();
    let first = new_holder_page(&fx.view, "First");
    let second = new_holder_page(&fx.view, "Second");
    assert_eq!(fx.view.selected_page().as_ref(), Some(&second));

    let menu = fx.view.menu_model().expect("a tab menu");
    assert_eq!(menu.n_items(), 1);
    let label = menu.item_attribute_value(0, "label", None);
    let action = menu.item_attribute_value(0, "action", None);
    assert_eq!(label.as_ref().and_then(|v| v.str()), Some("Rename\u{2026}"));
    assert_eq!(action.as_ref().and_then(|v| v.str()), Some("tab.rename"));

    // What the tab bar does when the menu opens on `first`, and then when it
    // closes again (before the action runs).
    fx.view.emit_by_name::<()>("setup-menu", &[&first]);
    fx.view
        .emit_by_name::<()>("setup-menu", &[&None::<adw::TabPage>]);
    fx.window
        .activate_action("tab.rename", None)
        .expect("the tab.rename action");
    pump_for(Duration::from_millis(300));
    let (dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    assert_eq!(entry.text(), "First");
    entry.set_text("one");
    button_labeled(&dialog, "Rename")
        .expect("a Rename button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    assert_eq!(first.title(), "one");
    assert_eq!(second.title(), "Second");
    fx.window.destroy();
}

/// Rename… from the tab's real menu leaves the cursor in the name field, so the
/// new name can be typed at once. (The menu closes after its action has run,
/// and closing hands the keyboard focus back; a dialog opened by the action
/// itself lost it to one of its buttons.)
pub(super) fn the_tab_menus_rename_puts_the_cursor_in_the_name_field() {
    let fx = fixture();
    let page = new_holder_page(&fx.view, "Named");
    let terminal = start_shell_in(&pane(&page), "/bin/sh", Path::new("/"), None);
    pump_for(Duration::from_millis(300));
    terminal.grab_focus();

    // The menu as the tab bar shows it: a popover menu from the view's model,
    // opened for `page`, whose item is clicked.
    fx.view.emit_by_name::<()>("setup-menu", &[&page]);
    let menu = gtk::PopoverMenu::from_model(fx.view.menu_model().as_ref());
    menu.set_parent(&fx.bar);
    menu.popup();
    pump_for(Duration::from_millis(300));
    let item = all_widgets::<gtk::Widget>(&menu)
        .into_iter()
        .find(|w| w.type_().name() == "GtkModelButton")
        .expect("the Rename… item");
    item.emit_by_name::<()>("clicked", &[]);

    // The dialog opens once the menu has handed the focus back, and takes the
    // focus after that, so wait for each instead of for a fixed time, which a
    // slow machine can overrun.
    pump_until("the rename dialog", || rename_dialog(&fx.window).is_some());
    let (dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    let in_name_field = || {
        gtk::prelude::RootExt::focus(&fx.window)
            .is_some_and(|f| f.is_ancestor(&entry) || f == *entry.upcast_ref::<gtk::Widget>())
    };
    pump_until("the focus to land in the name field", in_name_field);

    // And it stays there: the menu closing late is what used to take it away.
    pump_for(Duration::from_millis(300));
    let focus = gtk::prelude::RootExt::focus(&fx.window).expect("something has the focus");
    assert!(
        in_name_field(),
        "the focus is on {} instead of the name field",
        focus.type_().name()
    );
    button_labeled(&dialog, "Cancel")
        .expect("a Cancel button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    menu.unparent();
    fx.click_close(&terminal);
    fx.window.destroy();
}

/// What the tab menu's Rename… waits for before it opens its dialog: not while
/// the keyboard focus is still in the menu, soon after it has left, and after a
/// short wait if it never does. (The menu hands the focus back from an event of
/// its own, so in the real menu the order of that event and an idle callback
/// is down to timing; here the focus is moved by hand.)
pub(super) fn opening_after_the_menu_waits_for_the_focus_to_leave_it() {
    let fx = fixture();
    let page = new_holder_page(&fx.view, "Named");
    let terminal = start_shell_in(&pane(&page), "/bin/sh", Path::new("/"), None);
    pump_for(Duration::from_millis(300));
    let menu = gtk::PopoverMenu::from_model(fx.view.menu_model().as_ref());
    menu.set_parent(&fx.bar);
    let in_menu = || {
        gtk::prelude::RootExt::focus(&fx.window)
            .is_some_and(|f| f.ancestor(gtk::Popover::static_type()).is_some())
    };

    // The focus leaves the menu: then, and only then, it opens.
    menu.popup();
    pump_until("the focus to go into the menu", in_menu);
    let opened = Rc::new(Cell::new(false));
    after_the_menu_lets_go(Some(&fx.window), {
        let opened = Rc::clone(&opened);
        move || opened.set(true)
    });
    pump_for(Duration::from_millis(100));
    assert!(!opened.get(), "not while the focus is in the menu");
    menu.popdown();
    terminal.grab_focus();
    pump_until("it to open once the focus has left", || opened.get());

    // The focus stays in the menu: it opens after the fallback, and only once.
    menu.popup();
    pump_until("the focus to go into the menu again", in_menu);
    let runs = Rc::new(Cell::new(0));
    after_the_menu_lets_go(Some(&fx.window), {
        let runs = Rc::clone(&runs);
        move || runs.set(runs.get() + 1)
    });
    pump_until("the fallback", || runs.get() > 0);
    menu.popdown();
    terminal.grab_focus();
    pump_for(Duration::from_millis(300));
    assert_eq!(runs.get(), 1, "it opens once");

    // No menu: it opens from an idle callback, as before.
    let opened = Rc::new(Cell::new(false));
    after_the_menu_lets_go(Some(&fx.window), {
        let opened = Rc::clone(&opened);
        move || opened.set(true)
    });
    pump_until("it to open with no menu", || opened.get());

    menu.unparent();
    fx.click_close(&terminal);
    fx.window.destroy();
}

/// The primary-button gesture the tab bar got from `setup_tab_bar`.
fn double_click_gesture(bar: &adw::TabBar) -> gtk::GestureClick {
    let controllers = bar.observe_controllers();
    (0..controllers.n_items())
        .filter_map(|i| controllers.item(i).and_downcast::<gtk::GestureClick>())
        .find(|g| g.button() == gtk::gdk::BUTTON_PRIMARY)
        .expect("a click gesture on the tab bar")
}

/// The middle of `widget`, in the coordinates of `bar`.
fn center_in(bar: &adw::TabBar, widget: &gtk::Widget) -> (f64, f64) {
    let bounds = widget.compute_bounds(bar).expect("bounds");
    (
        f64::from(bounds.x() + bounds.width() / 2.0),
        f64::from(bounds.y() + bounds.height() / 2.0),
    )
}

/// A double click on a tab's title asks for a new name for the selected tab (the
/// one the first click selected); a single click, or a double click on the close
/// button, asks nothing.
pub(super) fn double_clicking_a_tab_asks_for_a_new_name() {
    let fx = fixture();
    let page = new_holder_page(&fx.view, "Plain");
    pump_for(Duration::from_millis(400));
    let tabs = widgets_of_type(&fx.bar, "AdwTab");
    assert_eq!(tabs.len(), 2, "the permanent tab and the new one");
    let tab = tabs.last().expect("a tab");
    let gesture = double_click_gesture(&fx.bar);
    assert_eq!(gesture.propagation_phase(), gtk::PropagationPhase::Capture);
    let press = |n_press: i32, (x, y): (f64, f64)| {
        gesture.emit_by_name::<()>("pressed", &[&n_press, &x, &y]);
    };

    // A single click: a tab is just selected.
    press(1, center_in(&fx.bar, tab));
    pump_for(Duration::from_millis(200));
    assert!(rename_dialog(&fx.window).is_none());

    // A double click on a button in the tab (the close button).
    let close = find_widget::<gtk::Button>(tab).expect("a button in the tab");
    press(2, center_in(&fx.bar, close.upcast_ref()));
    pump_for(Duration::from_millis(200));
    assert!(
        rename_dialog(&fx.window).is_none(),
        "buttons are not titles"
    );

    // A double click on the title.
    press(2, center_in(&fx.bar, tab));
    pump_for(Duration::from_millis(300));
    let (dialog, entry) = rename_dialog(&fx.window).expect("the rename dialog");
    assert_eq!(entry.text(), "Plain");
    entry.set_text("db");
    button_labeled(&dialog, "Rename")
        .expect("a Rename button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    assert_eq!(page.title(), "db");
    fx.window.destroy();
}

/// The tab bar's own close button closes the tab, and a recording session's
/// transcript is written by it.
pub(super) fn the_tab_bars_close_button_closes_the_tab(bin: &Path, dir: &Path) {
    let fx = fixture();
    let fake = fake_ssh(bin, "bar-close", "echo FAKE-END\nexec sleep 600");
    let terminal = open_command(
        &fx.view,
        "x",
        fake_command(&fake, &profile(&[]), None, &MemorySecretStore::default()),
        Some(spec(dir, &Notice::default())),
    );
    wait_for_text(&terminal, "FAKE-END");
    pump_for(Duration::from_millis(300));
    let tabs = widgets_of_type(&fx.bar, "AdwTab");
    let close = find_widget::<gtk::Button>(tabs.last().expect("a tab")).expect("a button");
    assert!(files_in(dir).is_empty());
    close.emit_clicked();
    pump_until("the tab to close", || fx.tabs() == 0);
    assert_eq!(files_in(dir).len(), 1, "the transcript was written");
    fx.window.destroy();
}

/// A terminal gets the palette of the current style when it is made, and the
/// palette of the other style when the application's style changes.
pub(super) fn terminal_colors_follow_light_and_dark() {
    let manager = adw::StyleManager::default();
    let fx = fixture();
    let wanted = |dark: bool| theme::palette(dark).background;

    manager.set_color_scheme(adw::ColorScheme::ForceDark);
    assert!(manager.is_dark());
    let first = open_local_shell(&fx.view, zoom::DEFAULT, None);
    assert_eq!(first.color_background_for_draw(), wanted(true));

    manager.set_color_scheme(adw::ColorScheme::ForceLight);
    assert!(!manager.is_dark());
    assert_eq!(first.color_background_for_draw(), wanted(false));
    let second = open_local_shell(&fx.view, zoom::DEFAULT, None);
    assert_eq!(second.color_background_for_draw(), wanted(false));

    // A terminal that is gone is not a problem for the next change.
    fx.click_close(&first);
    pump_for(Duration::from_millis(200));
    drop(first);
    manager.set_color_scheme(adw::ColorScheme::ForceDark);
    assert_eq!(second.color_background_for_draw(), wanted(true));

    manager.set_color_scheme(adw::ColorScheme::Default);
    fx.click_close(&second);
    fx.window.destroy();
}

/// A new tab takes its text size from the selected tab, and only a window with
/// none asks for the fallback, the size saved for the next launch, which is
/// read from a file: `open_profile` does not read it for every tab.
pub(super) fn the_fallback_text_size_is_asked_for_only_when_needed() {
    // No tab to take it from: asked for once, and the tab starts at it.
    let view = adw::TabView::new();
    let asked = Cell::new(0);
    let first = new_holder_page_at(&view, "first", || {
        asked.set(asked.get() + 1);
        1.3
    });
    assert_eq!(asked.get(), 1);
    assert_eq!(scale_of(&first), Some(1.3));

    // A selected tab: its size, and the fallback is not asked for.
    let second = new_holder_page_at(&view, "second", || {
        asked.set(asked.get() + 1);
        2.0
    });
    assert_eq!(asked.get(), 1, "the saved size was read for a second tab");
    assert_eq!(scale_of(&second), Some(1.3));
}

/// A presented main window (set up as the app's are, with its actions and
/// accelerators) whose selected tab runs a fake local shell with `body`; the
/// window's own first tab, a plain shell, is behind it.
fn main_window_with_shell(
    bin: &Path,
    dir: &Path,
    name: &str,
    body: &str,
) -> (adw::Application, window::MainWindow, Terminal) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    crate::set_accels(&app);
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(300));
    let page = new_holder_page(&main.view, "x");
    let terminal = start_shell_in(&pane(&page), &fake_shell(bin, name, body), dir, None);
    (app, main, terminal)
}

/// The window action `name` of `main`.
fn window_action(main: &window::MainWindow, name: &str) -> gio::SimpleAction {
    main.window
        .lookup_action(name)
        .and_downcast::<gio::SimpleAction>()
        .unwrap_or_else(|| panic!("the main window has no {name} action"))
}

/// The text on the clipboard, read the way an application reads it, with the
/// main loop running until the read is done.
fn clipboard_text(display: &gdk::Display) -> Option<String> {
    let clipboard = display.clipboard();
    let read = Rc::new(RefCell::new(None));
    glib::spawn_future_local(glib::clone!(
        #[strong]
        read,
        async move {
            let text = clipboard.read_text_future().await;
            *read.borrow_mut() = Some(text);
        }
    ));
    pump_until("the clipboard text", || read.borrow().is_some());
    let text = read.take().expect("the read is done");
    text.expect("the clipboard read").map(Into::into)
}

/// Copy and Paste: `win.copy` puts the selected tab's selection on the
/// clipboard as plain text, and `win.paste` sends the clipboard to the
/// terminal as typed input.
pub(super) fn copy_and_paste_round_trip(bin: &Path, dir: &Path) {
    let (_app, main, terminal) = main_window_with_shell(
        bin,
        dir,
        "copy-shell",
        "echo COPY-ME-123\nread line\necho \"GOT:$line\"\nexec sleep 600",
    );
    wait_for_text(&terminal, "COPY-ME-123");
    let display = gdk::Display::default().expect("a display");

    // Everything is selected, the empty rows below the line too (VTE gives
    // "COPY-ME-123\n\n"): the line is what matters.
    terminal.select_all();
    WidgetExt::activate_action(&main.window, "win.copy", None).expect("win.copy");
    assert_eq!(
        clipboard_text(&display).as_deref().map(str::trim),
        Some("COPY-ME-123"),
        "the selection, as text without escape codes"
    );

    display.clipboard().set_text("echo PASTED");
    terminal.unselect_all();
    WidgetExt::activate_action(&main.window, "win.paste", None).expect("win.paste");
    // The paste reads the clipboard asynchronously: it shows (echoed by the
    // terminal) before the Enter that ends the line.
    wait_for_text(&terminal, "echo PASTED");
    terminal.feed_child(b"\n");
    wait_for_text(&terminal, "GOT:echo PASTED");
    main.window.destroy();
}

/// VTE's own paste is what `win.paste` uses, so a program that asks for
/// bracketed paste (as shells with it do) gets the pasted text between the
/// markers, one block, instead of keystrokes.
pub(super) fn paste_is_bracketed_when_the_program_asks(bin: &Path, dir: &Path) {
    let (_app, main, terminal) = main_window_with_shell(
        bin,
        dir,
        "bracket-shell",
        "printf '\\033[?2004h'\necho READY\nexec cat -v",
    );
    wait_for_text(&terminal, "READY");
    gdk::Display::default()
        .expect("a display")
        .clipboard()
        .set_text("echo PASTED");
    WidgetExt::activate_action(&main.window, "win.paste", None).expect("win.paste");
    wait_for_text(&terminal, "^[[200~echo PASTED^[[201~");
    main.window.destroy();
}

/// Ctrl+Shift+C's action (`win.copy`) is enabled whenever the selected tab has
/// a terminal, with a selection or without, and does nothing without one.
/// Disabled, the key would go on to the terminal, which by xterm convention may
/// turn it into Ctrl+C and interrupt the program. A tab that is no terminal
/// (the new-tab list) disables Copy and Paste, so their keys reach its search
/// entry.
pub(super) fn the_copy_key_is_handled_without_a_selection(bin: &Path, dir: &Path) {
    let (_app, main, terminal) =
        main_window_with_shell(bin, dir, "select-shell", "echo SOMETHING\nexec sleep 600");
    wait_for_text(&terminal, "SOMETHING");
    let copy = window_action(&main, "copy");
    let paste = window_action(&main, "paste");
    assert!(!terminal.has_selection());
    assert!(copy.is_enabled(), "a terminal with no selection");
    assert!(paste.is_enabled(), "a terminal is selected");

    // With nothing selected it changes nothing.
    let display = gdk::Display::default().expect("a display");
    display.clipboard().set_text("KEEP-ME");
    WidgetExt::activate_action(&main.window, "win.copy", None).expect("win.copy");
    assert_eq!(clipboard_text(&display).as_deref(), Some("KEEP-ME"));

    // A selection coming and going makes no difference to it.
    terminal.select_all();
    assert!(copy.is_enabled());
    terminal.unselect_all();
    assert!(copy.is_enabled());

    // Another terminal tab: the same.
    main.view.set_selected_page(&main.view.nth_page(0));
    assert!(copy.is_enabled() && paste.is_enabled());

    // A tab that is not a terminal (the new-tab list) has neither.
    WidgetExt::activate_action(&main.window, "win.new-tab", None).expect("win.new-tab");
    pump_for(Duration::from_millis(200));
    assert!(terminal_of(&main.view.selected_page().unwrap()).is_none());
    assert!(!copy.is_enabled());
    assert!(!paste.is_enabled());
    main.window.destroy();
}

/// The main window's actions on its tabs, and on the selected tab's terminal.
/// (Spelled out here, not taken from `terminal_tab`: the test checks the names
/// the accelerators are bound to.)
const TAB_ACTIONS: [&str; 6] = [
    "new-tab",
    "close-pane",
    "next-tab",
    "previous-tab",
    "split-right",
    "split-down",
];
const TERMINAL_ACTIONS: [&str; 5] = ["copy", "paste", "zoom-in", "zoom-out", "zoom-reset"];

/// Assert that the tab actions of `window` are all enabled or all disabled
/// (`tabs`), and the same for its terminal actions (`terminal`).
fn assert_window_actions(window: &adw::ApplicationWindow, tabs: bool, terminal: bool, when: &str) {
    for (names, wanted) in [
        (TAB_ACTIONS.as_slice(), tabs),
        (&TERMINAL_ACTIONS, terminal),
    ] {
        for name in names {
            let action = window
                .lookup_action(name)
                .unwrap_or_else(|| panic!("the main window has no {name} action"));
            assert_eq!(action.is_enabled(), wanted, "win.{name} {when}");
        }
    }
}

/// Close `dialog`, open over `window`, the way a person does and wait for it to
/// be gone.
fn close_dialog_over(window: &adw::ApplicationWindow, dialog: &adw::Dialog) {
    dialog.close();
    pump_until("the dialog to close", || window.visible_dialog().is_none());
}

/// GTK runs the window's accelerators before the focused widget, and a dialog is
/// a widget of the window it is over, so with the tab and terminal actions
/// enabled Ctrl+Shift+V would paste into the terminal behind the Rename dialog,
/// Ctrl+plus would resize it and Ctrl+Shift+W would close its tab. While any
/// dialog is open over a main window those actions are disabled (a disabled
/// action leaves its key to the dialog), and when the dialog closes they are
/// what they were: a tab without a terminal still has no Copy, Paste or text
/// size actions. Checked for the Rename dialog, About, the Keyboard Shortcuts
/// dialog and a window made for a dragged-out tab, and by acting on the
/// disabled actions: nothing reaches the terminal behind.
pub(super) fn the_window_actions_wait_while_a_dialog_is_open(bin: &Path, dir: &Path) {
    let (_app, main, terminal) =
        main_window_with_shell(bin, dir, "dialog-shell", "echo UNDER\nexec sleep 600");
    wait_for_text(&terminal, "UNDER");
    let display = gdk::Display::default().expect("a display");
    assert_window_actions(&main.window, true, true, "with no dialog open");

    // The Rename dialog over a terminal tab: everything is disabled.
    let page = main.view.selected_page().expect("the shell's tab");
    ask_rename(&page);
    pump_until("the Rename dialog", || {
        main.window.visible_dialog().is_some()
    });
    assert_window_actions(
        &main.window,
        false,
        false,
        "while the Rename dialog is open",
    );

    // Their keys do nothing to the tab behind: acting on the actions, as the
    // accelerators do, neither pastes, resizes nor closes.
    let scale = terminal.font_scale();
    let pages = main.view.n_pages();
    // Whether GTK refuses a disabled action or it is ignored, what matters is
    // the effect, so the results are not looked at.
    display.clipboard().set_text("echo PASTED-BEHIND");
    let _ = WidgetExt::activate_action(&main.window, "win.paste", None);
    pump_for(Duration::from_millis(400));
    assert!(
        !terminal_text(&terminal).contains("PASTED-BEHIND"),
        "the paste reached the terminal behind the dialog"
    );
    let _ = WidgetExt::activate_action(&main.window, "win.zoom-in", None);
    assert_eq!(terminal.font_scale(), scale, "the text size changed");
    let _ = WidgetExt::activate_action(&main.window, "win.close-pane", None);
    pump_for(Duration::from_millis(200));
    assert_eq!(main.view.n_pages(), pages, "a tab was closed");

    // Cancel: the actions come back, and now they act.
    let (dialog, _) = rename_dialog(&main.window).expect("the rename dialog");
    button_labeled(&dialog, "Cancel")
        .expect("a Cancel button")
        .emit_clicked();
    pump_until("the Rename dialog to close", || {
        main.window.visible_dialog().is_none()
    });
    assert_window_actions(&main.window, true, true, "after the Rename dialog closed");
    WidgetExt::activate_action(&main.window, "win.zoom-in", None).expect("win.zoom-in");
    assert!(terminal.font_scale() > scale, "zoom-in works again");

    // About, and the Keyboard Shortcuts dialog.
    let about = crate::about_dialog();
    about.present(Some(&main.window));
    pump_until("the About dialog", || {
        main.window.visible_dialog().is_some()
    });
    assert_window_actions(&main.window, false, false, "while About is open");
    close_dialog_over(&main.window, about.upcast_ref());
    assert_window_actions(&main.window, true, true, "after About closed");

    let shortcuts = crate::shortcuts_dialog();
    shortcuts.present(Some(&main.window));
    pump_until("the Keyboard Shortcuts dialog", || {
        main.window.visible_dialog().is_some()
    });
    assert_window_actions(&main.window, false, false, "while the shortcuts are shown");
    close_dialog_over(&main.window, shortcuts.upcast_ref());
    assert_window_actions(&main.window, true, true, "after the shortcuts closed");

    // A tab that is no terminal (the new-tab list): the tab actions only, and
    // closing a dialog over it gives back exactly that.
    WidgetExt::activate_action(&main.window, "win.new-tab", None).expect("win.new-tab");
    pump_for(Duration::from_millis(200));
    assert!(terminal_of(&main.view.selected_page().unwrap()).is_none());
    assert_window_actions(&main.window, true, false, "on the new-tab list");
    let about = crate::about_dialog();
    about.present(Some(&main.window));
    pump_until("the About dialog", || {
        main.window.visible_dialog().is_some()
    });
    assert_window_actions(&main.window, false, false, "over the new-tab list");
    // Changing tabs behind the dialog does not enable anything either.
    main.view.set_selected_page(&main.view.nth_page(0));
    assert_window_actions(
        &main.window,
        false,
        false,
        "after a tab change behind the dialog",
    );
    close_dialog_over(&main.window, about.upcast_ref());
    assert_window_actions(
        &main.window,
        true,
        true,
        "on a terminal tab, after the dialog",
    );
    main.view
        .set_selected_page(&main.view.nth_page(main.view.n_pages() - 1));
    assert_window_actions(
        &main.window,
        true,
        false,
        "on the new-tab list, after the dialog",
    );

    // A window made for a tab dragged out of every window has it too.
    let made: Option<adw::TabView> = main.view.emit_by_name("create-window", &[]);
    let made = made.expect("a view for the dropped tab");
    // See `moving_the_last_tab_out_closes_the_source_window`.
    std::mem::forget(made.clone());
    main.view.transfer_page(&page, &made, 0);
    pump_for(Duration::from_millis(300));
    let dragged = made
        .root()
        .and_downcast::<adw::ApplicationWindow>()
        .expect("the view is in a window");
    assert_window_actions(&dragged, true, true, "in the new window");
    let about = crate::about_dialog();
    about.present(Some(&dragged));
    pump_until("the About dialog over the new window", || {
        dragged.visible_dialog().is_some()
    });
    assert_window_actions(&dragged, false, false, "in the new window with a dialog");
    about.close();
    pump_until("the dialog to close", || dragged.visible_dialog().is_none());
    assert_window_actions(&dragged, true, true, "in the new window afterwards");

    dragged.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Every terminal has the right-click menu: Copy and Paste, the terminal's own
/// actions (a right click does not focus its pane, so the window's Paste would
/// go to another pane), and Split Right and Split Down, its pane's.
pub(super) fn terminals_have_the_copy_paste_menu(bin: &Path, dir: &Path) {
    let (_app, main, terminal) =
        main_window_with_shell(bin, dir, "menu-shell", "echo MENU\nexec sleep 600");
    let expected = [
        ("Copy".to_owned(), "term.copy".to_owned()),
        ("Paste".to_owned(), "term.paste".to_owned()),
        ("Split Right".to_owned(), "pane.split-right".to_owned()),
        ("Split Down".to_owned(), "pane.split-down".to_owned()),
    ];
    let menu = terminal.context_menu_model().expect("a context menu");
    assert_eq!(menu_items(&menu), expected);
    let first = terminal_of(&main.view.nth_page(0)).expect("the window's own shell");
    let menu = first.context_menu_model().expect("a context menu");
    assert_eq!(menu_items(&menu), expected);
    main.window.destroy();
}

/// The menu's Copy is insensitive without a selection and follows the
/// selection of its own terminal, not of another one; chosen with a selection
/// it copies it. The menu here is the popover VTE makes from the terminal's
/// model: the same model, parented to the terminal.
pub(super) fn the_menus_copy_follows_the_selection(bin: &Path, dir: &Path) {
    let (_app, main, terminal) = main_window_with_shell(
        bin,
        dir,
        "menu-copy-shell",
        "echo MENU-COPY\nexec sleep 600",
    );
    wait_for_text(&terminal, "MENU-COPY");
    let other = terminal_of(&main.view.nth_page(0)).expect("the window's own shell");
    let popover = gtk::PopoverMenu::from_model(terminal.context_menu_model().as_ref());
    popover.set_parent(&terminal);
    let item_sensitive = |label: &str| {
        widgets_of_type(&popover, "GtkModelButton")
            .into_iter()
            .find(|b| b.property::<String>("text") == label)
            .unwrap_or_else(|| panic!("no {label:?} item in the menu"))
            .is_sensitive()
    };
    assert!(!item_sensitive("Copy"), "nothing is selected");
    assert!(item_sensitive("Paste"));

    // Another terminal's selection is not this one's.
    other.select_all();
    assert!(!item_sensitive("Copy"));
    other.unselect_all();

    terminal.select_all();
    assert!(item_sensitive("Copy"), "this terminal has a selection");
    let display = gdk::Display::default().expect("a display");
    display.clipboard().set_text("OLD");
    WidgetExt::activate_action(&terminal, "term.copy", None).expect("term.copy");
    assert_eq!(
        clipboard_text(&display).as_deref().map(str::trim),
        Some("MENU-COPY")
    );

    terminal.unselect_all();
    assert!(!item_sensitive("Copy"), "the selection is gone");
    popover.unparent();
    main.window.destroy();
}

/// Ctrl+Shift+C and Ctrl+Shift+V are listed in the Keyboard Shortcuts dialog's
/// table and are the accelerators of Copy and Paste.
pub(super) fn shortcuts_include_copy_and_paste() {
    let app = test_app();
    crate::set_accels(&app);
    let parse = |a: &str| gtk::accelerator_parse(a).expect("an accelerator");
    for (action, accel) in [
        ("win.copy", "<Control><Shift>c"),
        ("win.paste", "<Control><Shift>v"),
    ] {
        let set: Vec<_> = app
            .accels_for_action(action)
            .iter()
            .map(|a| parse(a))
            .collect();
        assert_eq!(set, [parse(accel)], "the accelerator of {action}");
        assert!(
            crate::SHORTCUTS
                .iter()
                .any(|(group, items)| *group == "Terminal"
                    && items.iter().any(|(_, a, k)| *a == action && *k == accel)),
            "{action} is in the Terminal group of the shortcuts"
        );
    }
}

/// A window holding `child` inside a notification area that is attached to
/// `notice`.
pub(super) fn window_with_area(
    notice: &Notice,
    child: &impl IsA<gtk::Widget>,
) -> (gtk::Window, NotificationArea) {
    let area = NotificationArea::new(child);
    let window = gtk::Window::builder()
        .default_width(400)
        .default_height(200)
        .child(&area)
        .build();
    window.present();
    notice.attach(&area);
    (window, area)
}

/// Attach an area to `notice` and let it go again straight away, the way a
/// closed window's area goes.
fn drop_attached_area(notice: &Notice) {
    notice.attach(&super::bare_area());
}

/// A message becomes a notification on an attached area, built as plain text:
/// markup characters in it are shown as they are.
pub(super) fn notice_is_a_plain_text_notification() {
    let notice = Notice::default();
    // No area at all: the message goes to stderr, without a fuss.
    notice.show("nobody listens");
    assert_eq!(notice.last_text(), None);
    // So does one for an area that is gone.
    drop_attached_area(&notice);
    notice.show("nobody listens either");
    assert_eq!(notice.last_text(), None);
    assert_eq!(
        notice.printed(),
        ["nobody listens", "nobody listens either"]
    );

    let (window, area) = window_with_area(&notice, &gtk::Label::new(Some("page")));
    notice.show(MARKUP_NAME);
    assert_eq!(notice.last_text().as_deref(), Some(MARKUP_NAME));
    assert_eq!(notice.last_area().as_ref(), Some(&area));

    // What the area displays: a label with the text, not interpreted, for the
    // time a short message gets.
    pump_until("the notification to show", || {
        labels_in(&area).iter().any(|l| l.label() == MARKUP_NAME)
    });
    let label = labels_in(&area)
        .into_iter()
        .find(|l| l.label() == MARKUP_NAME)
        .expect("the notification's label");
    assert!(!label.uses_markup());
    assert_eq!(area.shown_for(), Some(Duration::from_secs(8)));
    window.destroy();
}

/// With several windows, `show_near` picks the area around the widget, and
/// everything else falls back to the areas still alive.
pub(super) fn show_near_picks_the_widgets_window() {
    let notice = Notice::default();
    let first_label = gtk::Label::new(Some("first"));
    let second_label = gtk::Label::new(Some("second"));
    let (first_window, first) = window_with_area(&notice, &first_label);
    let (second_window, second) = window_with_area(&notice, &second_label);

    // The widget's own window decides, whichever area was attached last.
    notice.show_near(&first_label, "one");
    assert_eq!(notice.last_text().as_deref(), Some("one"));
    assert_eq!(notice.last_area().as_ref(), Some(&first));
    notice.show_near(&second_label, "two");
    assert_eq!(notice.last_text().as_deref(), Some("two"));
    assert_eq!(notice.last_area().as_ref(), Some(&second));
    notice.show_near(&first_label, "three");
    assert_eq!(notice.last_area().as_ref(), Some(&first));

    // A widget that is in no area gets what `show` does: an area that is
    // still alive, even though the one attached last is gone.
    drop_attached_area(&notice);
    let loose = gtk::Label::new(None);
    notice.show_near(&loose, "four");
    assert_eq!(notice.last_text().as_deref(), Some("four"));
    let used = notice.last_area().expect("an area that is alive");
    assert!(used == first || used == second);

    first_window.destroy();
    second_window.destroy();
}

/// A transcript that cannot be written is reported on the window of its own
/// tab, even when another window's area was attached later.
pub(super) fn a_failed_transcript_is_reported_in_the_tabs_window(bin: &Path, dir: &Path) {
    // A plain file where the transcript directory should be.
    fs::create_dir_all(dir).expect("dir");
    let blocker = dir.join("blocker");
    fs::write(&blocker, "").expect("blocker");

    let notice = Notice::default();
    let view = adw::TabView::new();
    crate::terminal_tab::setup_tab_view(&view);
    view.append(&gtk::Label::new(Some("keeper")));
    let (window, area) = window_with_area(&notice, &view);
    let (other_window, _other) = window_with_area(&notice, &gtk::Label::new(Some("other")));

    let fake = fake_ssh(bin, "tr-fail-toast", "echo FAKE-END\nexit 0");
    let p = profile(&[]);
    open_command(
        &view,
        "x",
        fake_command(&fake, &p, None, &MemorySecretStore::default()),
        Some(spec(&blocker.join("transcripts"), &notice)),
    );
    pump_until("the transcript failure to be reported", || {
        notice.last_text().is_some()
    });
    let said = notice.last_text().expect("a notification");
    assert!(
        said.starts_with("Could not save the transcript: "),
        "{said}"
    );
    assert_eq!(notice.last_area().as_ref(), Some(&area));
    window.destroy();
    other_window.destroy();
}
