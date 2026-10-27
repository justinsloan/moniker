//! Checks that need a display and a main loop: they run the real tab code with
//! a fake `ssh` (a `/bin/sh` script) and read back what the terminal shows, and
//! they drive the sync page, Sync Now and the sync on launch with fake worker
//! functions (no ssh, no keyring).
//!
//! They are `#[ignore]`d. GTK can be used from one thread only, so everything
//! is ONE test function that runs the scenarios in turn (the test harness
//! would otherwise give each test its own thread). Run them with
//! `scripts/gui-tests.sh`, which builds the test binary and runs it under a
//! virtual display, in an environment that cannot reach the user's desktop,
//! keyring or data directories (`env -i`, a throwaway `HOME` and `XDG_*`
//! directories, a session bus address that points nowhere). It starts no bus
//! and no keyring and kills nothing. Never run these tests any other way.
//!
//! The scenarios live in one file per area ([`tabs`], [`main_window`],
//! [`windows`], [`save_transcripts`], [`transcript_indicator`],
//! [`preferences`], [`notifications`], [`sync`], [`profiles`], [`recordings`],
//! [`sessions`], [`local_terminal`]);
//! this file holds the test function that calls them, the helpers they
//! share, and [`markup_is_shown_literally_everywhere`], the one scenario that
//! spans the areas. The transcripts go to a temporary directory the tests pass
//! in; the real data directory is never used.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::{Profile, StartupSequence};
use moniker_core::secrets::{MemorySecretStore, SecretStore};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;

use crate::context::AppContext;
use crate::notify::NotificationArea;
use crate::session_file::{Kind, Node, PaneRecord, TabRecord};
use crate::terminal_tab::{
    connection_command, has_stored_password, set_test_ssh_program, tab_title, terminal_of,
    terminal_text,
};
use crate::{form, new_tab, window};

mod hosts;
mod local_terminal;
mod main_window;
mod notifications;
mod panes;
mod preferences;
mod profiles;
mod recordings;
mod save_transcripts;
mod sessions;
mod sync;
mod tabs;
mod transcript_indicator;
mod windows;

/// A profile name (and tab title, sequence name ...) full of markup characters.
/// Widgets that show profile data must show it literally; scenarios use this to
/// check that.
pub(crate) const MARKUP_NAME: &str = "<b>x</b> &amp;";

/// How long to wait for the main loop to produce something.
const WAIT: Duration = Duration::from_secs(15);

/// A keyring around `inner` that can be held, as a locked keyring waiting for
/// its unlock prompt is: every call waits until it is released. It can also
/// refuse every change. It panics when called on the thread that made it (the
/// main thread, in the window tests) instead of waiting there forever: the GUI
/// must never call the keyring on the main thread (issue #4).
pub(crate) struct HeldKeyring {
    inner: Arc<dyn SecretStore>,
    main: std::thread::ThreadId,
    held: std::sync::Mutex<bool>,
    released: std::sync::Condvar,
    refuse_changes: std::sync::atomic::AtomicBool,
}

impl HeldKeyring {
    pub(crate) fn new(inner: Arc<dyn SecretStore>) -> HeldKeyring {
        HeldKeyring {
            inner,
            main: std::thread::current().id(),
            held: std::sync::Mutex::new(false),
            released: std::sync::Condvar::new(),
            refuse_changes: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn hold(&self) {
        *self.held.lock().unwrap() = true;
    }

    pub(crate) fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.released.notify_all();
    }

    /// Make every `set` and `delete` fail (or succeed again).
    pub(crate) fn refuse_changes(&self, refuse: bool) {
        self.refuse_changes
            .store(refuse, std::sync::atomic::Ordering::SeqCst);
    }

    fn wait(&self) {
        assert_ne!(
            std::thread::current().id(),
            self.main,
            "the keyring was called on the main thread"
        );
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.released.wait(held).unwrap();
        }
    }

    fn change(&self) -> Result<(), moniker_core::secrets::SecretError> {
        self.wait();
        if self
            .refuse_changes
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(moniker_core::secrets::SecretError::Backend(
                "the keyring is locked".to_owned(),
            ));
        }
        Ok(())
    }
}

impl SecretStore for HeldKeyring {
    fn get(
        &self,
        key: &str,
    ) -> Result<Option<zeroize::Zeroizing<String>>, moniker_core::secrets::SecretError> {
        self.wait();
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), moniker_core::secrets::SecretError> {
        self.change()?;
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), moniker_core::secrets::SecretError> {
        self.change()?;
        self.inner.delete(key)
    }
    fn probe(&self) -> Result<(), moniker_core::secrets::SecretError> {
        self.wait();
        self.inner.probe()
    }
}

/// How long [`HeldProfileFile`] holds the lock at most: a save that waits for
/// it on the main thread then fails the test instead of hanging it.
const HELD_AT_MOST: Duration = Duration::from_secs(10);

/// The profile file's lock, held by another thread as a sync that waits for
/// the keyring holds it (issue #7), until released (or dropped, or
/// [`HELD_AT_MOST`] has passed).
struct HeldProfileFile {
    release: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HeldProfileFile {
    fn hold(dir: &Path) -> HeldProfileFile {
        HeldProfileFile::hold_then_write(dir, None)
    }

    /// [`hold`](Self::hold), and replace the file with `contents` (if any)
    /// just before letting go, as a sync does.
    fn hold_then_write(dir: &Path, contents: Option<Vec<u8>>) -> HeldProfileFile {
        let lock = fs::OpenOptions::new()
            .write(true)
            .open(dir.join("profiles.json.lock"))
            .expect("the profile file's lock file");
        let file = dir.join("profiles.json");
        let (held, is_held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            lock.lock().expect("take the lock");
            held.send(()).expect("say it is held");
            let _ = released.recv_timeout(HELD_AT_MOST);
            if let Some(contents) = contents {
                let temp = file.with_extension("json.by-sync");
                fs::write(&temp, contents).expect("write the new file");
                fs::rename(&temp, &file).expect("put it in place");
            }
            lock.unlock().expect("let go of the lock");
        });
        is_held.recv().expect("the lock taken");
        HeldProfileFile {
            release: Some(release),
            thread: Some(thread),
        }
    }

    fn release(self) {
        drop(self);
    }
}

impl Drop for HeldProfileFile {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Run the main loop until `done` returns true; panic after [`WAIT`], at the
/// caller's line.
#[track_caller]
fn pump_until(what: &str, mut done: impl FnMut() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + WAIT;
    loop {
        while context.iteration(false) {}
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The terminal of `page` once its session has started: connecting to a
/// profile asks the keyring, on its worker, first.
fn wait_for_session(page: &adw::TabPage) -> Terminal {
    pump_until("the session", || terminal_of(page).is_some());
    terminal_of(page).expect("the session")
}

/// Run the main loop for `duration`.
fn pump_for(duration: Duration) {
    let context = glib::MainContext::default();
    let end = Instant::now() + duration;
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Write an executable fake `ssh`: it prints each argument after `ARG:` (an
/// argument holding newlines spreads over several lines), then the five
/// variables Moniker sets for the askpass helper and its own parent's process
/// id (`PARENT=`), then runs `tail`.
fn fake_ssh(dir: &Path, name: &str, tail: &str) -> String {
    let path = dir.join(name);
    let script = format!(
        "#!/bin/sh\n\
         for a in \"$@\"; do printf 'ARG:%s\\n' \"$a\"; done\n\
         printf 'SSH_ASKPASS=%s\\n' \"${{SSH_ASKPASS-unset}}\"\n\
         printf 'SSH_ASKPASS_REQUIRE=%s\\n' \"${{SSH_ASKPASS_REQUIRE-unset}}\"\n\
         printf 'MONIKER_PROFILE_ID=%s\\n' \"${{MONIKER_PROFILE_ID-unset}}\"\n\
         printf 'MONIKER_PID=%s\\n' \"${{MONIKER_PID-unset}}\"\n\
         printf 'MONIKER_ASKPASS_TOKEN=%s\\n' \"${{MONIKER_ASKPASS_TOKEN-unset}}\"\n\
         printf 'PARENT=%s\\n' \"$PPID\"\n\
         {tail}\n"
    );
    fs::write(&path, script).expect("write the fake ssh");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod the fake ssh");
    path.to_str().expect("utf-8 temp path").to_owned()
}

/// The launch as a Moniker started with no arguments does it: the first window
/// a shell in the home folder, then the automatic sync.
fn launch_at_home(app: &adw::Application, ctx: &Rc<AppContext>) {
    crate::launch(
        app,
        ctx,
        &crate::launch_args::Start::home(&glib::home_dir()),
    );
}

/// Write an executable stand-in for a login shell that runs `body`.
fn fake_shell(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the fake shell");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod the fake shell");
    path.to_str().expect("utf-8 temp path").to_owned()
}

/// A presented window holding tabs, set up the way the app's windows are, except
/// that the tab bar is always shown: a tab view, a tab bar for it, and no pages.
struct TabWindow {
    window: gtk::Window,
    bar: adw::TabBar,
    view: adw::TabView,
}

fn tab_window() -> TabWindow {
    let view = adw::TabView::builder().vexpand(true).build();
    crate::terminal_tab::setup_tab_view(&view);
    let bar = adw::TabBar::builder().view(&view).autohide(false).build();
    crate::terminal_tab::setup_tab_bar(&bar);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&bar);
    column.append(&view);
    // An `AdwWindow`, so dialogs (the rename dialog) can open over it.
    let window = adw::Window::builder()
        .default_width(900)
        .default_height(500)
        .content(&column)
        .build();
    window.present();
    TabWindow {
        window: window.upcast(),
        bar,
        view,
    }
}

/// A [`TabWindow`] with one permanent page, so closing the tabs under test never
/// closes the window.
struct Fixture {
    window: gtk::Window,
    bar: adw::TabBar,
    view: adw::TabView,
}

fn fixture() -> Fixture {
    let TabWindow { window, bar, view } = tab_window();
    view.append(&gtk::Label::new(Some("keeper")));
    Fixture { window, bar, view }
}

impl Fixture {
    /// Tabs other than the permanent one.
    fn tabs(&self) -> i32 {
        self.view.n_pages() - 1
    }

    /// The page that shows `terminal`.
    fn page_of(&self, terminal: &Terminal) -> adw::TabPage {
        page_of(&self.view, terminal)
    }

    /// Close the tab of `terminal`, as its close button does.
    fn click_close(&self, terminal: &Terminal) {
        self.view.close_page(&self.page_of(terminal));
    }
}

/// The page of `view` that shows `terminal` (in one of its panes).
fn page_of(view: &adw::TabView, terminal: &Terminal) -> adw::TabPage {
    let layout = crate::panes::layout_of(terminal).expect("a terminal in a tab is in its layout");
    view.page(&layout)
}

/// The pane of `page` that has the focus: its only one, in a tab with one.
fn pane(page: &adw::TabPage) -> crate::terminal_tab::PaneHolder {
    crate::terminal_tab::holder_of(page).expect("a page made by new_holder_page has a pane")
}

/// A profile with a fixed name, host and user, plus the given sequences.
fn profile(sequences: &[(&str, &[&str])]) -> Profile {
    let mut p = Profile::new("Prod <web>", "app.example.test", "alice");
    p.sequences = sequences
        .iter()
        .map(|(name, commands)| StartupSequence {
            name: (*name).to_owned(),
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
        })
        .collect();
    p
}

/// Name of the stand-in askpass helper that [`gui_integration`] puts next to
/// the fake `ssh` programs, as the real one sits next to `moniker`.
const HELPER: &str = "moniker-askpass";

/// An existing file to stand for the askpass helper in `dir`.
fn make_helper(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).expect("helper dir");
    let path = dir.join(HELPER);
    fs::write(&path, "#!/bin/sh\n").expect("write the stand-in helper");
    path
}

/// The command `open_profile` would run for `p`, with `program` swapped for
/// the fake. The helper is the one next to the fake. The one-time token is in
/// the command's environment; nothing here removes a marker for it.
fn fake_command(
    program: &str,
    p: &Profile,
    sequence: Option<usize>,
    secrets: &dyn SecretStore,
) -> moniker_core::ssh::SshCommand {
    let askpass = Path::new(program).with_file_name(HELPER);
    let (mut cmd, _token) =
        connection_command(p, sequence, &askpass, has_stored_password(p, secrets));
    assert_eq!(cmd.program, "ssh", "the real program is the system ssh");
    cmd.program = program.to_owned();
    cmd
}

/// What the terminal shows, as trimmed lines without the empty rows below.
fn shown(terminal: &Terminal) -> Vec<String> {
    terminal_text(terminal)
        .trim_end()
        .lines()
        .map(|l| l.trim_end().to_owned())
        .collect()
}

fn wait_for_text(terminal: &Terminal, needle: &str) {
    pump_until(&format!("{needle:?} in the terminal"), || {
        terminal_text(terminal).contains(needle)
    });
}

/// Press Enter in `search`, as the keyboard does: the key binding is on the
/// entry's inner text widget, whose `activate` the entry passes on. Nothing runs
/// the main loop here, so GTK's delayed `search-changed` for text just typed has
/// not fired yet, which is what a fast typist's Enter does.
fn press_enter_in(search: &gtk::SearchEntry) {
    let text: gtk::Text = find_widget(search).expect("the search entry's text widget");
    text.emit_activate();
}

/// File names in `dir`, sorted. Empty if it does not exist.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
    files.sort();
    files
}

#[test]
#[ignore = "needs a display: run it with scripts/gui-tests.sh"]
fn gui_integration() {
    adw::init().expect("no display: run this test with scripts/gui-tests.sh");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).expect("bin dir");
    make_helper(&bin);

    // Issue #13, startup sequences for the local terminal.
    local_terminal::a_plain_pick_runs_the_default_and_leaves_a_shell(&tmp.path().join("ls-pick"));
    local_terminal::the_choices_offer_just_open_and_each_sequence(&tmp.path().join("ls-choices"));
    local_terminal::a_sequence_s_tab_ends_like_a_connection(&tmp.path().join("ls-ends"));
    local_terminal::a_shell_path_with_a_space_runs_the_sequence(&tmp.path().join("ls-space"));
    local_terminal::a_window_s_first_tab_runs_no_sequence(&tmp.path().join("ls-first"));
    local_terminal::a_restored_local_pane_runs_the_default_in_its_folder(
        &tmp.path().join("ls-restored"),
    );
    // The REC indicator of a tab that saves a transcript.
    transcript_indicator::a_tab_saving_a_transcript_shows_rec(&bin, &tmp.path().join("rec-shows"));
    transcript_indicator::rec_goes_when_the_transcript_is_written(&tmp.path().join("rec-goes"));
    transcript_indicator::a_split_shows_rec_on_the_pane_that_saves(&tmp.path().join("rec-split"));
    transcript_indicator::rec_is_not_part_of_the_name(&tmp.path().join("rec-name"));
    // Issue #12, the Sessions menu beside ＋.
    sessions::the_menu_with_no_sessions(&tmp.path().join("sm-none"));
    sessions::the_sessions_button_can_be_hidden(&tmp.path().join("sm-hidden"));
    sessions::saving_and_adding_from_the_menu(&bin, &tmp.path().join("sm-save"));
    sessions::saving_under_a_name_in_use_asks(&tmp.path().join("sm-replace"));
    sessions::rename_and_delete_ask_first(&tmp.path().join("sm-rename"));
    sessions::a_damaged_file_is_told_and_kept(&tmp.path().join("sm-damaged"));
    sessions::two_windows_saving_keep_both(&tmp.path().join("sm-two"));
    sessions::a_session_changed_since_the_menu_opened_is_read_again(&tmp.path().join("sm-gone"));
    sessions::a_failed_write_is_told(&tmp.path().join("sm-fail"));
    sessions::a_file_repaired_while_asking_still_asks_about_a_name(&tmp.path().join("sm-repaired"));
    sessions::every_answer_gives_the_keyboard_back_to_the_terminal(&tmp.path().join("sm-keyboard"));
    sessions::a_pick_after_the_file_went_bad_is_told(&tmp.path().join("sm-bad-pick"));
    sessions::a_long_name_has_its_whole_name_in_a_tooltip(&tmp.path().join("sm-tooltip"));
    // Issue #12, saved sessions: a window captured and added back.
    sessions::a_window_comes_back_as_saved(&bin, &tmp.path().join("sess-saved"));
    sessions::a_tab_not_shown_yet_gets_its_split_when_shown(&tmp.path().join("sess-hidden"));
    sessions::a_restored_divider_is_put_within_its_limits(&tmp.path().join("sess-limits"));
    sessions::a_profile_gone_shows_the_list_with_a_note(&tmp.path().join("sess-gone"));
    sessions::a_renamed_profile_still_connects(&bin, &tmp.path().join("sess-renamed"));
    sessions::a_folder_gone_starts_at_home(&tmp.path().join("sess-folder"));
    sessions::a_folder_reported_for_another_machine_is_not_saved(&tmp.path().join("sess-host"));
    sessions::restored_panes_save_a_transcript_as_a_plain_click_would(
        &bin,
        &tmp.path().join("sess-transcripts"),
    );
    sessions::a_command_tab_is_saved_as_a_shell_in_its_folder(
        &bin,
        &tmp.path().join("sess-command"),
    );
    sessions::a_pane_waiting_or_ended_is_still_its_profile(&bin, &tmp.path().join("sess-ended"));
    sessions::restored_connections_ask_the_keyring_in_pane_order(
        &bin,
        &tmp.path().join("sess-order"),
    );
    // Issue #15, the command line, also when Moniker is already running.
    main_window::glib_leaves_the_handed_over_arguments_alone(&tmp.path().join("c-glib"));
    main_window::the_command_line_opens_a_window_where_asked(&bin, &tmp.path().join("c-where"));
    main_window::a_launch_while_a_dialog_is_open_still_opens(&tmp.path().join("c-dialog-open"));
    main_window::a_launch_as_the_last_window_closes_still_opens(&tmp.path().join("c-last"));
    sync::a_command_line_launches_once_and_then_opens_windows(&tmp.path().join("c-once"));
    sync::the_launch_sync_holds_the_application_until_it_ends(&tmp.path().join("c-hold"));
    // Issue #15, a local tab that starts in a folder or runs a command.
    tabs::a_local_tab_starts_in_its_folder(&bin, &tmp.path().join("l-folder"));
    tabs::a_command_tab_closes_on_success_and_keeps_failures(&bin, &tmp.path().join("l-command"));
    tabs::a_title_given_stays(&bin, &tmp.path().join("l-title"));
    // Issue #12, a profile's default sequence.
    profiles::the_default_is_chosen_on_the_profile_page(&tmp.path().join("d-page"));
    profiles::the_default_row_follows_the_file_and_saves_only_a_pick(&tmp.path().join("d-file"));
    windows::a_plain_connection_runs_the_default_sequence(&bin, &tmp.path().join("d-run"));
    // Issue #10, a profile kept on this machine only.
    profiles::keeping_a_profile_here_asks_first(&tmp.path().join("k-keep"));
    profiles::syncing_it_again_asks_too(&tmp.path().join("k-again"));
    profiles::a_profile_that_cannot_sync_again_says_so(&tmp.path().join("k-max"));
    profiles::a_second_flip_waits_for_the_first(&tmp.path().join("k-second"));
    profiles::a_question_closes_with_its_profile(&tmp.path().join("k-gone"));
    profiles::the_lists_mark_profiles_kept_here(&tmp.path().join("k-mark"));
    profiles::a_new_profile_can_be_kept_here(&tmp.path().join("k-new"));
    // Issue #10, sync turned off.
    sync::sync_off_grays_out_sync_now_and_skips_launch(&tmp.path().join("s-off"));
    sync::the_sync_page_turns_sync_off_and_asks_to_turn_it_on(&tmp.path().join("s-sw-ask"));
    sync::the_sync_page_follows_a_switch_changed_elsewhere(&tmp.path().join("s-sw-else"));
    sync::a_damaged_switch_shows_off_and_says_so(&tmp.path().join("s-sw-damaged"));
    sync::a_switch_that_cannot_be_saved_goes_back_and_says_so(&tmp.path().join("s-sw-fail"));
    sync::the_sync_switch_does_not_wait_for_the_keyring(&tmp.path().join("s-sw-held"));
    sync::a_sync_started_on_a_stale_snapshot_stops_before_the_keyring(&tmp.path().join("s-stale"));
    sync::an_off_request_counts_before_its_save_is_back(&tmp.path().join("s-asked"));
    sync::cancelling_the_question_leaves_the_row_at_the_last_request(&tmp.path().join("s-last"));
    // Issue #6, the panes of a tab.
    panes::splitting_puts_a_new_pane_beside_the_focused_one(&bin, &tmp.path().join("s-split"));
    panes::the_tab_follows_its_focused_pane(&bin, &tmp.path().join("s-focus"));
    panes::closing_a_pane_gives_its_room_to_the_others(&bin, &tmp.path().join("s-close"));
    panes::panes_move_between_tabs_and_windows(&bin, &tmp.path().join("s-move"));
    panes::the_terminal_menu_acts_on_its_own_pane(&bin, &tmp.path().join("s-menu"));
    panes::a_tab_never_gives_the_focus_to_a_pane_that_left_it(&bin, &tmp.path().join("s-left"));
    panes::a_late_session_leaves_the_focus_in_the_pane_in_use(&bin, &tmp.path().join("s-late"));
    panes::the_header_acts_on_its_own_pane(&bin, &tmp.path().join("s-header"));
    panes::a_pane_that_closes_out_of_sight_moves_no_keyboard(&bin, &tmp.path().join("s-sight"));
    panes::drops_reach_panes_tabs_and_the_tab_bar(&bin, &tmp.path().join("s-drops"));
    panes::a_pane_dropped_outside_every_window_gets_its_own_window(
        &bin,
        &tmp.path().join("s-outside"),
    );
    panes::a_pane_dropped_on_nothing_gets_its_own_window(&bin, &tmp.path().join("s-nothing"));
    panes::a_tab_whose_panes_were_taken_goes_quietly(&tmp.path().join("s-back"));
    panes::closing_another_pane_and_a_stray_focus_leave_the_keyboard(
        &bin,
        &tmp.path().join("s-stray"),
    );
    panes::a_list_in_a_split_tab_leaves_the_keyboard_alone(&tmp.path().join("s-list"));
    hosts::both_lists_mark_each_hosts_status(&tmp.path().join("p-both"));
    hosts::a_mark_waits_for_its_check_and_keeps_its_state(&tmp.path().join("p-wait"));
    hosts::turned_off_there_is_no_mark_and_no_ping(&tmp.path().join("p-off"));
    hosts::turning_off_calls_off_a_check_under_way(&tmp.path().join("p-call-off"));
    hosts::a_host_that_answers_is_not_kept_waiting(&tmp.path().join("p-fast"));
    hosts::at_most_sixteen_checks_run_at_once(&tmp.path().join("p-sixteen"));
    // Issue #7 first: it fails fast when the profile file is written on the
    // main thread.
    profiles::saving_waits_for_the_profile_file_not_the_window(&tmp.path().join("f-held"));
    profiles::a_failed_save_puts_the_switch_and_the_port_back(&tmp.path().join("f-fail"));
    windows::connecting_uses_the_profile_as_saved_just_before(&bin, &tmp.path().join("f-connect"));
    // Issue #4 first: they fail fast when a keyring call is on the main thread.
    sync::the_sync_page_uses_the_keyring_off_the_main_thread(&tmp.path().join("k-sync"));
    sync::the_sync_page_waits_for_the_keyring(&tmp.path().join("k-sync-wait"));
    profiles::the_profile_window_uses_the_keyring_off_the_main_thread(&tmp.path().join("k-prof"));
    profiles::the_keyring_workers_answers_are_never_lost(&tmp.path().join("k-lost"));
    windows::connecting_waits_for_the_keyring_off_the_main_thread(
        &bin,
        &tmp.path().join("k-connect"),
    );
    tabs::argv_and_environment_reach_the_child(&bin);
    tabs::startup_sequence_is_one_literal_argument(&bin);
    tabs::clean_exit_closes_the_tab(&bin);
    tabs::failed_exit_keeps_the_tab(&bin);
    tabs::transcript_on_clean_exit(&bin, &tmp.path().join("t-exit"));
    tabs::transcript_on_tab_close_is_written_once(&bin, &tmp.path().join("t-close"));
    tabs::transcript_after_failed_exit_is_written_once(&bin, &tmp.path().join("t-fail"));
    tabs::transcript_on_window_close(&bin, &tmp.path().join("t-window"));
    tabs::no_transcript_without_a_spec(&bin, &tmp.path().join("t-none"));
    tabs::missing_helper_falls_back_to_the_terminal_and_says_so(&tmp.path().join("h-missing"));
    tabs::the_missing_helper_notification_lands_in_the_tabs_window(
        &bin,
        &tmp.path().join("h-toast"),
    );
    tabs::the_marker_is_released_when_the_session_ends(&bin, &tmp.path().join("h-marker"));
    tabs::a_closed_local_tab_hangs_up_its_shell_and_its_job(&bin, &tmp.path().join("l-hup"));
    tabs::local_shell_has_the_bounded_default_scrollback();
    tabs::local_shell_starts_in_home(&bin, &tmp.path().join("l-home"));
    tabs::a_local_tab_follows_its_shell_until_renamed(&bin);
    tabs::remote_tab_keeps_its_name_when_the_shell_sets_a_title(&bin);
    tabs::closing_the_last_tab_closes_the_window(&bin);
    tabs::rename_sets_title_and_escaped_tooltip();
    tabs::ask_rename_renames_the_page();
    tabs::renaming_an_unchanged_title_keeps_following_the_shell(&bin);
    tabs::the_tab_menu_renames_the_tab_it_was_opened_for();
    tabs::the_tab_menus_rename_puts_the_cursor_in_the_name_field();
    tabs::opening_after_the_menu_waits_for_the_focus_to_leave_it();
    tabs::double_clicking_a_tab_asks_for_a_new_name();
    tabs::the_tab_bars_close_button_closes_the_tab(&bin, &tmp.path().join("t-bar"));
    tabs::terminal_colors_follow_light_and_dark();
    tabs::copy_and_paste_round_trip(&bin, &tmp.path().join("c-trip"));
    tabs::paste_is_bracketed_when_the_program_asks(&bin, &tmp.path().join("c-bracket"));
    tabs::the_copy_key_is_handled_without_a_selection(&bin, &tmp.path().join("c-select"));
    tabs::the_window_actions_wait_while_a_dialog_is_open(&bin, &tmp.path().join("c-dialog"));
    tabs::the_fallback_text_size_is_asked_for_only_when_needed();
    tabs::terminals_have_the_copy_paste_menu(&bin, &tmp.path().join("c-menu"));
    tabs::the_menus_copy_follows_the_selection(&bin, &tmp.path().join("c-menu-copy"));
    tabs::shortcuts_include_copy_and_paste();
    main_window::launch_has_one_local_tab_and_no_sidebar(&tmp.path().join("w-launch"));
    main_window::about_shows_copyright_and_mit();
    main_window::header_has_no_connect_button(&tmp.path().join("w-header"));
    main_window::wording_uses_transcripts_and_connection_profiles(&tmp.path().join("w-words"));
    main_window::the_connect_choices_offer_sequences_and_transcript(
        &bin,
        &tmp.path().join("w-choices"),
    );
    main_window::profiles_connect_opens_in_the_last_active_window(&tmp.path().join("w-last"));
    windows::plus_opens_the_new_tab_list_with_local_first(&tmp.path().join("n-plus"));
    windows::a_main_window_opens_at_the_size_the_last_one_closed_at(&tmp.path().join("n-size"));
    windows::the_side_windows_open_at_the_size_they_closed_at(&tmp.path().join("n-side-size"));
    windows::picking_a_profile_turns_the_tab_into_the_session(&bin, &tmp.path().join("n-pick"));
    windows::the_new_tab_choices_reach_profiles_without_sequences(
        &bin,
        &tmp.path().join("n-plain"),
    );
    windows::enter_right_after_typing_picks_the_typed_profile(&bin, &tmp.path().join("n-fast"));
    windows::closing_an_unpicked_new_tab_writes_nothing(&tmp.path().join("n-close"));
    windows::new_window_opens_with_one_local_tab(&tmp.path().join("n-window"));
    windows::moved_recording_tab_writes_once_in_its_new_window(&bin, &tmp.path().join("n-move"));
    windows::moving_the_last_tab_out_closes_the_source_window(&tmp.path().join("n-last"));
    windows::closing_the_last_main_window_closes_the_profiles_window(&tmp.path().join("n-quit"));
    windows::shortcut_actions_exist(&tmp.path().join("n-keys"));
    windows::zoom_is_per_tab(&tmp.path().join("z-tab"));
    windows::a_new_tab_starts_at_the_active_tabs_size(&bin, &tmp.path().join("z-new"));
    windows::the_menu_row_shows_and_resets_the_size(&tmp.path().join("z-row"));
    windows::quitting_saves_the_last_windows_active_tab(&tmp.path().join("z-quit"));
    windows::launch_starts_at_the_saved_size(&bin, &tmp.path().join("z-launch"));
    windows::zoom_shortcuts_exist();
    save_transcripts::a_local_tab_can_save_its_transcript(&tmp.path().join("v-local"));
    save_transcripts::save_all_overrides_a_profile_that_saves_nothing(
        &bin,
        &tmp.path().join("v-all"),
    );
    save_transcripts::unchecking_skips_one_session(&bin, &tmp.path().join("v-uncheck"));
    save_transcripts::the_launch_tab_saves_when_save_all_is_on(&tmp.path().join("v-launch"));
    save_transcripts::transcripts_go_to_the_chosen_folder(&bin, &tmp.path().join("v-folder"));
    preferences::preferences_has_the_four_settings_and_sync(&tmp.path().join("f-rows"));
    preferences::changing_the_font_applies_to_open_terminals(&tmp.path().join("f-font"));
    preferences::the_font_can_go_back_to_the_system_font(&tmp.path().join("f-reset"));
    preferences::scrollback_applies_to_new_ordinary_tabs(&tmp.path().join("f-scroll"));
    preferences::the_save_all_switch_is_saved(&tmp.path().join("f-all"));
    preferences::choosing_a_folder_moves_the_transcripts(&tmp.path().join("f-folder"));
    preferences::a_move_leaves_other_files_where_they_are(&tmp.path().join("f-other"));
    preferences::a_folder_that_cannot_be_saved_is_refused(&tmp.path().join("f-utf8"));
    preferences::the_same_folder_by_another_name_changes_nothing(&tmp.path().join("f-same"));
    preferences::asking_again_raises_the_open_preferences(&tmp.path().join("f-raise"));
    preferences::the_dialogs_messages_show_in_the_dialog(&tmp.path().join("f-toast"));
    preferences::the_sync_button_opens_the_sync_page(&tmp.path().join("f-sync"));
    preferences::preferences_shortcut_exists(&tmp.path().join("f-keys"));
    profiles::sidebar_lists_live_profiles_sorted_and_filters(&tmp.path().join("p-list"));
    profiles::selecting_a_profile_reads_no_secret(&tmp.path().join("p-noread"));
    profiles::entry_apply_saves_only_that_field(&tmp.path().join("p-apply"));
    profiles::leaving_a_row_discards_the_edit(&tmp.path().join("p-leave"));
    profiles::password_row_clears_after_apply_and_on_leave(&tmp.path().join("p-pass"));
    profiles::switch_port_and_key_file_save_when_changed(&tmp.path().join("p-other"));
    profiles::a_settling_port_is_saved_when_the_page_goes(&tmp.path().join("p-port"));
    profiles::refresh_keeps_an_unapplied_edit(&tmp.path().join("p-refresh"));
    profiles::deleted_by_sync_shows_the_empty_state(&tmp.path().join("p-gone"));
    profiles::the_file_watch_refreshes_the_window(&tmp.path().join("p-watch"));
    profiles::double_click_and_connect_button_connect(&tmp.path().join("p-connect"));
    profiles::new_profile_dialog_creates_and_selects(&tmp.path().join("p-new"));
    profiles::delete_asks_then_removes_profile_and_password(&tmp.path().join("p-delete"));
    profiles::the_page_says_password_removal_is_local(&tmp.path().join("p-remove"));
    profiles::sidebar_and_title_show_markup_literally(&tmp.path().join("p-markup"));
    profiles::add_edit_move_delete_a_sequence(&tmp.path().join("p-seq"));
    profiles::cancel_discards_sequence_edits(&tmp.path().join("p-seq-cancel"));
    profiles::sequence_errors_stay_on_the_sub_page(&tmp.path().join("p-seq-err"));
    profiles::sequence_rows_show_names_literally(&tmp.path().join("p-seq-markup"));
    profiles::a_refresh_keeps_the_sub_pages_text(&tmp.path().join("p-seq-refresh"));
    profiles::saving_after_the_profile_was_deleted_fails_cleanly(&tmp.path().join("p-seq-gone"));
    profiles::a_sequence_changed_elsewhere_is_not_overwritten(&tmp.path().join("p-seq-stale"));
    profiles::selecting_another_profile_closes_the_sub_page(&tmp.path().join("p-seq-switch"));
    profiles::the_narrow_window_keeps_its_back_button(&tmp.path().join("p-narrow"));
    profiles::present_reuses_the_open_window(&tmp.path().join("p-single"));
    tabs::a_failed_transcript_is_reported_in_the_tabs_window(&bin, &tmp.path().join("t-toast"));
    tabs::notice_is_a_plain_text_notification();
    tabs::show_near_picks_the_widgets_window();
    notifications::notifications_appear_at_the_top_center(&tmp.path().join("m-top"));
    notifications::the_side_windows_keep_notifications_below_their_header_bars(
        &tmp.path().join("m-side"),
    );
    notifications::long_notifications_wrap_and_stay_longer();
    notifications::notifications_queue_and_show_literally();
    notifications::closing_the_window_with_a_notification_up();
    notifications::notifications_wait_while_a_dialog_is_open(&tmp.path().join("m-dialog"));
    sync::sync_page_key_and_availability(&tmp.path().join("s-key"));
    sync::sync_page_without_profiles(&tmp.path().join("s-none"));
    sync::sync_page_success(&tmp.path().join("s-ok"));
    sync::sync_page_errors(&tmp.path().join("s-err"));
    sync::sync_page_profiles_change_during_a_sync(&tmp.path().join("s-chg"));
    sync::sync_page_closed_before_completion(&tmp.path().join("s-close"));
    sync::first_manual_success_enables_sync_now(&tmp.path().join("s-first"));
    sync::sync_now_while_running_says_so(&tmp.path().join("s-busy"));
    sync::auto_sync_starts_after_the_window_is_shown(&tmp.path().join("s-launch"));
    sync::auto_sync_failure_is_one_notification(&tmp.path().join("s-launch-fail"));
    sync::sync_now_asks_a_machine_that_has_been_away(&tmp.path().join("s-away-now"));
    sync::the_sync_page_asks_a_machine_that_has_been_away(&tmp.path().join("s-away-page"));
    sync::the_launch_sync_of_a_machine_that_has_been_away_only_says_so(
        &tmp.path().join("s-away-launch"),
    );
    sync::switch_off_stops_auto_sync(&tmp.path().join("s-switch"));
    sync::a_sync_refreshes_the_sidebar_and_new_tab_list(&tmp.path().join("s-refresh"));
    sync::syncs_read_the_keyring_on_the_worker_only(&tmp.path().join("s-worker"));
    recordings::lists_newest_first_and_can_flip(&tmp.path().join("r-order"));
    recordings::host_filter_lists_only_that_host(&tmp.path().join("r-host"));
    recordings::search_across_all_narrows_and_counts(&tmp.path().join("r-search"));
    recordings::opening_with_a_search_jumps_to_the_first_match(&tmp.path().join("r-jump"));
    recordings::odd_recordings_show_literally(&tmp.path().join("r-odd"));
    recordings::deleting_the_open_recording_clears_the_viewer(&tmp.path().join("r-delete"));
    recordings::a_large_recording_loads_whole(&tmp.path().join("r-large"));
    recordings::a_huge_one_line_transcript_loads_promptly(&tmp.path().join("r-huge"));
    recordings::display_breaks_survive_chunk_boundaries(&tmp.path().join("r-chunks"));
    recordings::a_match_across_a_display_break_is_highlighted_whole(&tmp.path().join("r-break"));
    recordings::copying_across_a_display_break_copies_the_original(&tmp.path().join("r-copy"));
    recordings::export_copies_the_original_bytes(&tmp.path().join("r-export"));
    recordings::a_new_recording_appears_live(&tmp.path().join("r-live"));
    recordings::a_failed_delete_is_told_in_the_transcripts_window(
        &tmp.path().join("r-undeletable"),
    );
    recordings::the_export_dialog_starts_in_the_home_directory();
    recordings::closing_the_last_main_window_closes_it(&tmp.path().join("r-quit"));
    markup_is_shown_literally_everywhere(&bin, &tmp.path().join("markup"));
    // Every scenario, not only the three above: the GUI never called the
    // keyring on the main thread (issue #4). Tests seed and read the keyring
    // through their own handles, which are not counted.
    assert_eq!(
        crate::keyring::main_thread_calls(),
        0,
        "keyring calls on the main thread"
    );
}

/// The labels below `root` that show exactly `text`, none of which may parse
/// markup. `what` names the surface in the failure message. Panics if no label
/// shows `text`: a surface that silently stopped showing it is not "safe".
fn literal_labels(what: &str, root: &impl IsA<gtk::Widget>, text: &str) -> Vec<gtk::Label> {
    let shown: Vec<gtk::Label> = labels_in(root)
        .into_iter()
        .filter(|l| l.label() == text)
        .collect();
    assert!(!shown.is_empty(), "{what}: no label shows {text:?}");
    for label in &shown {
        assert!(!label.uses_markup(), "{what}: {text:?} is parsed as markup");
    }
    shown
}

/// A row whose title (and subtitle, if any) are `title` and `subtitle` as they
/// are, drawn by labels that do not parse markup.
fn assert_plain_row(what: &str, row: &adw::ActionRow, title: &str, subtitle: Option<&str>) {
    assert_eq!(row.title(), title, "{what}: title");
    assert_eq!(row.subtitle().as_deref(), subtitle, "{what}: subtitle");
    assert!(!row.uses_markup(), "{what}: the row parses markup");
    literal_labels(what, row, title);
    if let Some(subtitle) = subtitle {
        literal_labels(what, row, subtitle);
    }
}

/// The alert in `window`, once it shows.
fn alert_in(window: &impl IsA<gtk::Widget>) -> adw::AlertDialog {
    pump_until("the alert", || {
        !all_widgets::<adw::AlertDialog>(window).is_empty()
    });
    all_widgets::<adw::AlertDialog>(window)
        .into_iter()
        .next()
        .expect("the alert")
}

/// Whether the keyboard focus is in `row`.
fn has_focus_in(window: &adw::Window, row: &impl IsA<gtk::Widget>) -> bool {
    gtk::prelude::GtkWindowExt::focus(window)
        .is_some_and(|f| f.is_ancestor(row) || &f == row.upcast_ref())
}

/// The action row of `root` whose widget name is `id` (a profile's id).
fn row_for(root: &impl IsA<gtk::Widget>, id: &str) -> adw::ActionRow {
    all_widgets::<adw::ActionRow>(root)
        .into_iter()
        .find(|row| row.widget_name() == id)
        .unwrap_or_else(|| panic!("no row for {id}"))
}

/// The menu button below `root` whose tooltip is `tooltip`.
fn menu_button(root: &impl IsA<gtk::Widget>, tooltip: &str) -> gtk::MenuButton {
    all_widgets::<gtk::MenuButton>(root)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some(tooltip))
        .unwrap_or_else(|| panic!("no {tooltip:?} menu button"))
}

/// Open `button`'s popover and wait for it to show.
fn open_popover(button: &gtk::MenuButton) -> gtk::Popover {
    let popover = button.popover().expect("the button's popover");
    button.popup();
    pump_until("the popover to open", || popover.is_visible());
    pump_for(Duration::from_millis(200));
    popover
}

fn close_popover(popover: &gtk::Popover) {
    popover.popdown();
    pump_until("the popover to close", || !popover.is_visible());
}

/// Profile data comes from other machines, so every place that shows it must
/// show it as plain text. The scenarios of each area check their own surfaces;
/// this one puts a single profile (name and sequence name full of markup
/// characters, key file too) through all of them in one place. A surface added
/// later belongs here too, above all one built from a libadwaita widget that
/// parses markup unless told not to.
///
/// Surfaces: the new-tab list (row and the choices behind "›"), the tab (title,
/// tooltip, as the tab bar draws it), the window title, the same two and a
/// pane's header for a tab that saves a transcript (REC before the name), a
/// notification (and the toast of the Preferences dialog), the Sessions menu (a
/// saved session's row, its rename field and the questions of delete and
/// replace) and, from it, the note over the list of a restored pane whose
/// profile is gone, the sync
/// page's profile choice, and in the Connection Profiles window the sidebar row, the page
/// title, the key-file row, the sequence row, the Default sequence row (as chosen
/// and in its open list) and the delete confirmation. The session runs a fake
/// `ssh`.
fn markup_is_shown_literally_everywhere(bin: &Path, dir: &Path) {
    const KEY_FILE: &str = "/tmp/<i>k</i>";
    let mut p = Profile::new(MARKUP_NAME, "h", "u");
    p.key_file = Some(PathBuf::from(KEY_FILE));
    p.sequences = vec![StartupSequence {
        name: MARKUP_NAME.to_owned(),
        commands: vec!["uptime".to_owned()],
    }];
    // Chosen as the default sequence: the Default sequence row shows it.
    p.startup_sequence = Some(0);
    let subtitle = form::subtitle(&p);
    let ctx = main_window::ctx_with(dir, std::slice::from_ref(&p));
    let app = main_window::test_app();
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(300));

    // The new-tab list: the row, and the choices behind its "›".
    let plus = all_widgets::<gtk::Button>(&main.window)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("New Tab"))
        .expect("the ＋ button");
    plus.emit_clicked();
    pump_for(Duration::from_millis(300));
    let page = main.view.selected_page().expect("the new tab");
    let list = all_widgets::<gtk::ListBox>(&page.child())
        .into_iter()
        .find(|l| l.widget_name() == new_tab::LIST_NAME)
        .expect("the new-tab list");
    let row = row_for(&list, &p.id);
    assert_plain_row("new-tab list row", &row, MARKUP_NAME, Some(&subtitle));
    let popover = open_popover(&menu_button(&row, "Connect Options"));
    let shown = literal_labels("new-tab choices", &popover, MARKUP_NAME);
    assert_eq!(shown.len(), 1, "the sequence: this popover has no heading");
    close_popover(&popover);

    // Picking the profile turns that tab into the session, named after the
    // profile. The title is plain, the tooltip (markup for GTK) escaped, and
    // the tab bar and the window title draw it with markup off.
    let fake = fake_ssh(bin, "markup", "echo FAKE-END\nexec sleep 600");
    set_test_ssh_program(Some(fake));
    list.emit_by_name::<()>("row-activated", &[&row]);
    let terminal = wait_for_session(&page);
    wait_for_text(&terminal, "FAKE-END");
    assert_eq!(page.title(), tab_title(&p));
    assert_eq!(page.title(), MARKUP_NAME);
    assert_eq!(
        page.tooltip().as_deref(),
        Some(glib::markup_escape_text(MARKUP_NAME).as_str()),
        "the tooltip is markup for GTK"
    );
    pump_for(Duration::from_millis(300));
    let bar: adw::TabBar = find_widget(&main.window).expect("the tab bar");
    literal_labels("tab bar", &bar, MARKUP_NAME);
    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    assert_eq!(title.title(), MARKUP_NAME);
    literal_labels("window title", &title, MARKUP_NAME);
    set_test_ssh_program(None);

    // With a second pane the headers show: the pane's title there is plain
    // text too, and the tab's tooltip lists both titles, escaped.
    WidgetExt::activate_action(&main.window, "win.split-right", None).expect("win.split-right");
    let layout = crate::panes::layout_of_page(&page).expect("the tab's layout");
    let first = crate::panes::pane_of(&layout.holders()[0]).expect("the first pane");
    assert!(first.header().is_visible());
    literal_labels("pane header", first.header(), MARKUP_NAME);
    assert_eq!(
        page.tooltip().as_deref(),
        Some(
            format!(
                "{}\n{}",
                glib::markup_escape_text(MARKUP_NAME),
                crate::panes::NEW_PANE_TITLE
            )
            .as_str()
        ),
        "the tooltip is markup for GTK"
    );

    // A tab of the profile that saves a transcript shows REC before the name,
    // as plain text, in the tab bar and the window title; in a split its pane's
    // header shows the name as it is after the badge, and the tab's tooltip
    // (markup) ends with the fixed line.
    crate::settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(dir.join("transcripts"));
    })
    .expect("write the settings");
    set_test_ssh_program(Some(fake_ssh(
        bin,
        "markup-rec",
        "echo FAKE-END\nexec sleep 600",
    )));
    crate::terminal_tab::open_profile(&main.view, &ctx, &p, None, true);
    let recording = main.view.selected_page().expect("the recording tab");
    wait_for_text(&wait_for_session(&recording), "FAKE-END");
    set_test_ssh_program(None);
    let rec_title = format!("{}{MARKUP_NAME}", crate::panes::REC_PREFIX);
    assert_eq!(recording.title(), rec_title);
    pump_until("the tab bar to show the recording tab's title", || {
        labels_in(&bar).iter().any(|l| l.label() == rec_title)
    });
    literal_labels("recording tab in the tab bar", &bar, &rec_title);
    literal_labels("recording window title", &title, &rec_title);
    WidgetExt::activate_action(&main.window, "win.split-right", None).expect("win.split-right");
    let rec_layout = crate::panes::layout_of_page(&recording).expect("its layout");
    let rec_pane = crate::panes::pane_of(&rec_layout.holders()[0]).expect("its first pane");
    assert!(rec_pane.header().is_visible());
    assert!(rec_pane.badge().is_visible());
    literal_labels("recording pane header", rec_pane.header(), MARKUP_NAME);
    assert_eq!(
        recording.tooltip().as_deref(),
        Some(
            format!(
                "{}\n{}\n{}",
                glib::markup_escape_text(MARKUP_NAME),
                crate::panes::NEW_PANE_TITLE,
                crate::panes::RECORDING_TOOLTIP
            )
            .as_str()
        ),
        "the tooltip is markup for GTK"
    );
    main.view.close_page(&recording);
    pump_until("the recording tab to close", || {
        main.view.selected_page().as_ref() != Some(&recording)
    });

    // A notification made of this text (an error message built from a name has
    // the same shape) is shown as it is.
    ctx.notice.show(MARKUP_NAME);
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&main.notifications));
    pump_until("the notification to show", || {
        labels_in(&main.notifications)
            .iter()
            .any(|l| l.label() == MARKUP_NAME)
    });
    assert_eq!(
        main.notifications.shown_text().as_deref(),
        Some(MARKUP_NAME)
    );
    literal_labels("notification", &main.notifications, MARKUP_NAME);

    // A saved session named like this, added from the Sessions menu: its pane
    // whose profile is gone says so above the list, naming the profile as the
    // session recorded it.
    let gone = TabRecord {
        root: Node::Pane(PaneRecord {
            kind: Kind::Profile {
                id: "gone".to_owned(),
                name: MARKUP_NAME.to_owned(),
            },
            name: None,
            scale: 1.0,
        }),
        focused: 0,
    };
    let session_file = dir.join(crate::session_file::FILE_NAME);
    crate::session_file::put(
        &session_file,
        crate::session_file::Session {
            name: MARKUP_NAME.to_owned(),
            tabs: vec![gone.clone()],
        },
        false,
    )
    .expect("the session is saved");
    // The menu row shows the name as it is.
    let sessions_menu = sessions::open_menu(&main);
    assert_eq!(sessions::listed(&sessions_menu), [MARKUP_NAME]);
    literal_labels("sessions menu", &sessions_menu, MARKUP_NAME);
    // Its tooltip (the whole name, for a long one cut short) is plain text too.
    let pick = sessions::row_of(&sessions_menu, MARKUP_NAME)
        .first_child()
        .and_downcast::<gtk::Button>()
        .expect("the session's button");
    assert_eq!(pick.tooltip_text().as_deref(), Some(MARKUP_NAME));
    assert_eq!(
        pick.tooltip_markup().as_deref(),
        Some(glib::markup_escape_text(MARKUP_NAME).as_str()),
        "sessions menu tooltip"
    );
    let tabs = main.view.n_pages();
    sessions::pick_session(&sessions_menu, MARKUP_NAME);
    pump_until("the session's tab", || main.view.n_pages() == tabs + 1);
    let restored = main.view.selected_page().expect("the restored tab");
    literal_labels(
        "gone profile note",
        &restored.child(),
        &format!("{MARKUP_NAME} is no longer saved."),
    );
    main.view.close_page(&restored);
    // Its Rename field, and the questions of Delete and Replace, name it as it
    // is too.
    let sessions_menu = sessions::open_menu(&main);
    sessions::from_session_menu(&sessions_menu, MARKUP_NAME, "Rename\u{2026}");
    let dialog = sessions::dialog_over(&main, "the Rename dialog");
    assert_eq!(sessions::name_entry(&dialog).text(), MARKUP_NAME);
    sessions::answer(&main, &dialog, "Cancel");
    let sessions_menu = sessions::open_menu(&main);
    sessions::from_session_menu(&sessions_menu, MARKUP_NAME, "Delete\u{2026}");
    let dialog = sessions::dialog_over(&main, "the Delete question");
    let heading = format!("Delete \u{201c}{MARKUP_NAME}\u{201d}?");
    assert_eq!(dialog.heading().as_deref(), Some(heading.as_str()));
    assert!(!dialog.is_heading_use_markup(), "delete session heading");
    assert!(!dialog.is_body_use_markup(), "delete session body");
    literal_labels("delete session dialog", &dialog, &heading);
    sessions::answer(&main, &dialog, "Cancel");
    let dialog = sessions::save_dialog(&main);
    sessions::name_entry(&dialog).set_text(MARKUP_NAME);
    sessions::answer(&main, &dialog, "Save");
    let dialog = sessions::dialog_over(&main, "the Replace question");
    let heading = format!("Replace \u{201c}{MARKUP_NAME}\u{201d}?");
    assert_eq!(dialog.heading().as_deref(), Some(heading.as_str()));
    assert!(!dialog.is_heading_use_markup(), "replace session heading");
    assert!(!dialog.is_body_use_markup(), "replace session body");
    literal_labels("replace session dialog", &dialog, &heading);
    sessions::answer(&main, &dialog, "Cancel");
    assert_eq!(
        crate::session_file::load(&session_file),
        crate::session_file::Loaded::Sessions(vec![crate::session_file::Session {
            name: MARKUP_NAME.to_owned(),
            tabs: vec![gone],
        }]),
        "the questions changed nothing"
    );

    // The Preferences dialog shows its own messages in a toast of its own (they
    // can quote a folder name): the same text, as it is.
    let preferences = crate::preferences::present(&main.window, &app, &ctx);
    pump_until("the Preferences dialog", || {
        main.window.visible_dialog().as_ref() == Some(preferences.upcast_ref())
    });
    ctx.notice.show_over(&preferences, MARKUP_NAME);
    assert_eq!(ctx.notice.last_dialog().as_ref(), Some(&preferences));
    pump_until("the toast in the Preferences dialog", || {
        labels_in(&preferences)
            .iter()
            .any(|l| l.label() == MARKUP_NAME)
    });
    literal_labels("preferences toast", &preferences, MARKUP_NAME);
    preferences.force_close();
    pump_until("the Preferences dialog to close", || {
        main.window.visible_dialog().is_none()
    });

    // The Connection Profiles window, on the sync page first (nothing selected yet): the
    // choice of the other machine lists the profile by name.
    let nothing: crate::profiles::ConnectFn = Rc::new(|_, _, _| {});
    let profiles_window = crate::profiles::present_sync(&app, &ctx, nothing);
    pump_for(Duration::from_millis(300));
    let sync_page: adw::PreferencesPage = find_widget(&profiles_window).expect("the sync page");
    let machine: adw::ComboRow = find_widget(&sync_page).expect("the profile choice");
    assert!(
        !machine.uses_markup(),
        "sync page: the choice parses markup"
    );
    literal_labels("sync page choice", &machine, MARKUP_NAME);

    // Then the profile itself: sidebar row, page title, name field, key-file
    // row and sequence row.
    crate::profiles::select(&profiles_window, &p.id);
    pump_for(Duration::from_millis(100));
    let sidebar = all_widgets::<gtk::ListBox>(&profiles_window)
        .into_iter()
        .find(|l| l.widget_name() == crate::profiles::SIDEBAR_NAME)
        .expect("the sidebar");
    assert_plain_row(
        "sidebar row",
        &row_for(&sidebar, &p.id),
        MARKUP_NAME,
        Some(&subtitle),
    );
    let heading = all_widgets::<gtk::Label>(&profiles_window)
        .into_iter()
        .find(|l| l.widget_name() == crate::profiles::TITLE_NAME)
        .expect("the page title");
    assert_eq!(heading.label(), MARKUP_NAME);
    assert!(!heading.uses_markup(), "page title parses markup");
    let name = all_widgets::<adw::EntryRow>(&profiles_window)
        .into_iter()
        .find(|row| row.title() == "Name")
        .expect("the Name row");
    assert_eq!(name.text(), MARKUP_NAME);
    assert!(!name.uses_markup(), "Name row parses markup");
    let key_row = all_widgets::<adw::ActionRow>(&profiles_window)
        .into_iter()
        .find(|row| row.title() == "Key file")
        .expect("the Key file row");
    assert_plain_row("key-file row", &key_row, "Key file", Some(KEY_FILE));
    let sequences = all_widgets::<adw::PreferencesGroup>(&profiles_window)
        .into_iter()
        .find(|group| group.title() == "Startup sequences")
        .expect("the Startup sequences group");
    let sequence_row = all_widgets::<adw::ActionRow>(&sequences)
        .into_iter()
        .find(|row| row.title() == MARKUP_NAME)
        .expect("the sequence row");
    assert_plain_row(
        "sequence row",
        &sequence_row,
        MARKUP_NAME,
        Some("1 command"),
    );

    // The Default sequence row offers the sequence by name: as the choice shown
    // in the row, and as an entry of the list that opens from it, plain text
    // both times.
    let default_row = all_widgets::<adw::ComboRow>(&profiles_window)
        .into_iter()
        .find(|row| row.title() == "Default sequence")
        .expect("the Default sequence row");
    assert!(!default_row.uses_markup(), "default row parses markup");
    let chosen = default_row
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .expect("the chosen default");
    assert_eq!(chosen.string(), MARKUP_NAME, "the default is chosen");
    literal_labels("default row", &default_row, MARKUP_NAME);
    ActionRowExt::activate(&default_row);
    let list: gtk::Popover = find_widget(&default_row).expect("the row's list");
    pump_until("the default sequence list to open", || list.is_visible());
    pump_for(Duration::from_millis(200));
    literal_labels("default list", &list, MARKUP_NAME);
    literal_labels("default list", &list, "None");
    list.popdown();
    pump_until("the default sequence list to close", || !list.is_visible());

    // The delete confirmation names the profile as it is. Cancel leaves it be.
    button_labeled(&profiles_window, "Delete Profile")
        .expect("the delete button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = profiles_window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("the delete confirmation");
    let heading = format!("Delete \u{201c}{MARKUP_NAME}\u{201d}?");
    assert_eq!(dialog.heading().as_deref(), Some(heading.as_str()));
    assert!(
        !dialog.is_heading_use_markup(),
        "dialog heading parses markup"
    );
    literal_labels("delete dialog", &dialog, &heading);
    button_labeled(&dialog, "Cancel")
        .expect("the dialog's Cancel button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    assert!(ctx.store.borrow().get(&p.id).is_some(), "nothing deleted");

    // The two questions of "Sync to other machines" name the profile as it is
    // too: keeping it here, then (once it is kept) syncing it again.
    let sync_row = all_widgets::<adw::SwitchRow>(&profiles_window)
        .into_iter()
        .find(|row| row.title() == "Sync to other machines")
        .expect("the Sync to other machines switch");
    assert!(!sync_row.uses_markup(), "sync switch parses markup");
    sync_row.set_active(false);
    let dialog = alert_in(&profiles_window);
    let heading = format!("Keep \u{201c}{MARKUP_NAME}\u{201d} on This Machine Only?");
    assert_eq!(dialog.heading().as_deref(), Some(heading.as_str()));
    assert!(!dialog.is_heading_use_markup(), "keep dialog heading");
    assert!(!dialog.is_body_use_markup(), "keep dialog body");
    literal_labels("keep dialog", &dialog, &heading);
    button_labeled(&dialog, "Keep on This Machine Only")
        .expect("the Keep button")
        .emit_clicked();
    pump_until("the save", || {
        crate::keyring::pending() == 0 && sync_row.is_sensitive()
    });
    assert!(ctx.store.borrow().get(&p.id).expect("there").local_only);
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(&profiles_window).is_empty()
    });
    sync_row.set_active(true);
    let dialog = alert_in(&profiles_window);
    let heading = format!("Sync \u{201c}{MARKUP_NAME}\u{201d} to Your Other Machines?");
    assert_eq!(dialog.heading().as_deref(), Some(heading.as_str()));
    assert!(!dialog.is_heading_use_markup(), "sync dialog heading");
    assert!(!dialog.is_body_use_markup(), "sync dialog body");
    literal_labels("sync dialog", &dialog, &heading);
    button_labeled(&dialog, "Cancel")
        .expect("the dialog's Cancel button")
        .emit_clicked();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(&profiles_window).is_empty()
    });

    profiles_window.close();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
    assert!(app.windows().is_empty());
}

/// A notification area around nothing in particular. Keep it alive for as long
/// as messages should reach it: `Notice` holds it weakly.
fn bare_area() -> NotificationArea {
    NotificationArea::new(&gtk::Label::new(None))
}

/// A context over a temp store and an in-memory keyring whose helper path is
/// `askpass_path`, with a notification area attached, and the test's own
/// handle on the keyring (to seed it without going through the context's).
/// Keep the area alive for as long as messages should reach it: `Notice`
/// holds it weakly.
fn launch_ctx(
    dir: &Path,
    askpass_path: PathBuf,
) -> (Rc<AppContext>, NotificationArea, Arc<MemorySecretStore>) {
    fs::create_dir_all(dir).expect("dir");
    let secrets = Arc::new(MemorySecretStore::default());
    let ctx = Rc::new(AppContext::with_parts(
        ProfileStore::open(&dir.join("profiles.json")).expect("store"),
        Arc::clone(&secrets) as Arc<dyn SecretStore>,
        askpass_path,
    ));
    let area = bare_area();
    ctx.notice.attach(&area);
    (ctx, area, secrets)
}

/// The first widget of type `T` at or below `root`.
fn find_widget<T: IsA<gtk::Widget>>(root: &impl IsA<gtk::Widget>) -> Option<T> {
    let mut stack = vec![root.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Ok(found) = widget.clone().downcast::<T>() {
            return Some(found);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    None
}

/// Every widget of type `T` at or below `root`, in tree order, popovers included.
fn all_widgets<T: IsA<gtk::Widget>>(root: &impl IsA<gtk::Widget>) -> Vec<T> {
    fn walk<T: IsA<gtk::Widget>>(widget: &gtk::Widget, found: &mut Vec<T>) {
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

/// Every label below `root`.
fn labels_in(root: &impl IsA<gtk::Widget>) -> Vec<gtk::Label> {
    let mut found = Vec::new();
    let mut stack = vec![root.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            found.push(label.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    found
}

/// Every widget below `root` whose GType is called `type_name`, in tree order. For
/// widgets a library keeps private, like the tabs of an `AdwTabBar`.
fn widgets_of_type(root: &impl IsA<gtk::Widget>, type_name: &str) -> Vec<gtk::Widget> {
    fn walk(widget: &gtk::Widget, type_name: &str, found: &mut Vec<gtk::Widget>) {
        if widget.type_().name() == type_name {
            found.push(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            walk(&c, type_name, found);
            child = c.next_sibling();
        }
    }
    let mut found = Vec::new();
    walk(root.upcast_ref(), type_name, &mut found);
    found
}

/// The button below `root` whose label is `label`.
fn button_labeled(root: &impl IsA<gtk::Widget>, label: &str) -> Option<gtk::Button> {
    let mut stack = vec![root.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Some(button) = widget.downcast_ref::<gtk::Button>()
            && button.label().as_deref() == Some(label)
        {
            return Some(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    None
}
