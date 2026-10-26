//! Moniker GUI: terminal tabs under a header bar, with saved connection
//! profiles one click away (the new-tab list) and managed in their own window.
//!
//! This file starts the application and defines its actions, the entries of
//! the main window's ☰ menu: New Window, Connection Profiles, Transcripts, Sync
//! Now, Preferences, Keyboard Shortcuts and About; and the keyboard shortcuts
//! ([`SHORTCUTS`]), which are the accelerators of those and of the main
//! window's tab, clipboard and text size actions.
//!
//! It reads the command line too ([`launch_args`], [`handle_command_line`]): the
//! launching process answers `--help`, `--version` and a mistake itself, and for
//! a window hands a [`launch_args::Start`] to the application, which gives it to
//! the Moniker already running (GApplication, `HANDLES_COMMAND_LINE`) or opens
//! the window itself; every launch opens a new window.
//!
//! It also starts the automatic sync on launch ([`launch`]). Sync Now and the
//! sync on launch both sync with the partner recorded in the GUI settings
//! ([`settings`]) by the first successful sync from the sync page; until then
//! neither exists for the person (Sync Now is hidden, nothing syncs on
//! launch). Once sync is set up but turned off (issue #10), Sync Now stays in
//! the menu grayed out, and nothing syncs on launch, with no notification.

mod connect_menu;
mod context;
mod form;
#[cfg(test)]
mod gui_tests;
mod hosts;
mod keyring;
mod launch_args;
mod local_only;
mod new_tab;
mod notify;
mod panes;
mod preferences;
mod profiles;
mod recordings;
mod session_file;
mod session_layout;
mod settings;
mod sync;
mod terminal_tab;
mod theme;
mod window;
mod window_size;
mod zoom;

use std::cell::Cell;
use std::ffi::OsString;
use std::io::Write;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use crate::context::AppContext;
use crate::launch_args::{Local, Start};
use crate::sync::SyncEvent;
use moniker_core::sync::StaleSync;

const APP_ID: &str = "org.moniker.Moniker";

/// The copyright line of the About box. `LICENSE` and the packages'
/// `copyright` file carry the same owner and year.
pub(crate) const ABOUT_COPYRIGHT: &str = "Copyright 2026 Justin M. Sloan";

/// The About box's website: the workspace's `repository` in Cargo.toml, which
/// the packages' Homepage field and the metainfo's homepage carry too.
pub(crate) const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// One keyboard shortcut: (title, action, accelerators). The accelerators are
/// in `gtk::accelerator_parse` form, several separated by spaces when one
/// action has more than one key (the Keyboard Shortcuts dialog shows them as
/// alternatives on one line).
pub(crate) type Shortcut = (&'static str, &'static str, &'static str);

/// The keyboard shortcuts, by section title. The Keyboard Shortcuts dialog
/// lists them and [`set_accels`] makes them the actions' accelerators, from
/// this one table, so the two cannot diverge.
///
/// They are Ctrl+Shift or Ctrl+Page Up/Down, Ctrl+comma for the Preferences,
/// and for the text size Ctrl+plus, Ctrl+minus and Ctrl+0 (and their keypad
/// keys), as GNOME Console has them: the terminal gets every other key
/// (application accelerators are handled before the focused terminal sees the
/// key). Ctrl+minus therefore no longer reaches programs in the terminal
/// (readline's undo; Ctrl+_ still does). The tab view's own shortcuts are
/// trimmed to match (`terminal_tab::setup_tab_view`).
///
/// Splitting is Ctrl+Shift+R (right) and Ctrl+Shift+D (down), letters that say
/// the direction. Ctrl+Shift+W closes the focused pane, which in a tab with one
/// pane is the tab. Ctrl+Shift+arrows go to another pane, and only in a tab
/// with two panes or more (`crate::panes`): elsewhere their actions are
/// disabled and VTE has them (Ctrl+Shift+Up/Down scroll a line, Left/Right
/// jump to the previous or next prompt).
pub(crate) const SHORTCUTS: &[(&str, &[Shortcut])] = &[
    (
        "General",
        &[("Preferences", "app.preferences", "<Control>comma")],
    ),
    (
        "Tabs",
        &[
            ("New Tab", "win.new-tab", "<Control><Shift>t"),
            ("Next Tab", "win.next-tab", "<Control>Page_Down"),
            ("Previous Tab", "win.previous-tab", "<Control>Page_Up"),
        ],
    ),
    (
        "Panes",
        &[
            ("Split Right", "win.split-right", "<Control><Shift>r"),
            ("Split Down", "win.split-down", "<Control><Shift>d"),
            ("Close Pane", "win.close-pane", "<Control><Shift>w"),
            (
                "Go to the Pane on the Left",
                "win.pane-left",
                "<Control><Shift>Left",
            ),
            (
                "Go to the Pane on the Right",
                "win.pane-right",
                "<Control><Shift>Right",
            ),
            ("Go to the Pane Above", "win.pane-up", "<Control><Shift>Up"),
            (
                "Go to the Pane Below",
                "win.pane-down",
                "<Control><Shift>Down",
            ),
        ],
    ),
    (
        "Terminal",
        &[
            ("Copy", "win.copy", "<Control><Shift>c"),
            ("Paste", "win.paste", "<Control><Shift>v"),
            (
                "Larger Text",
                "win.zoom-in",
                "<Control>plus <Control>equal <Control>KP_Add",
            ),
            (
                "Smaller Text",
                "win.zoom-out",
                "<Control>minus <Control>KP_Subtract",
            ),
            (
                "Normal Text Size",
                "win.zoom-reset",
                "<Control>0 <Control>KP_0",
            ),
        ],
    ),
    (
        "Windows",
        &[("New Window", "app.new-window", "<Control><Shift>n")],
    ),
];

fn main() -> glib::ExitCode {
    // What the command line asks for is decided first, before the profile file
    // is read or anything of GTK or the bus is started (see `launch_args`), so
    // `moniker --help` and a mistake in the options need none of them.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cwd = std::env::current_dir().ok();
    let words = match launch_args::local(&args, cwd.as_deref(), &glib::home_dir()) {
        Local::Print {
            status,
            to_stdout,
            text,
        } => {
            print_text(to_stdout, &text);
            return glib::ExitCode::from(exit_status(status));
        }
        Local::HandOver(words) => words,
    };
    // A window running on a broken store would be worse than no window.
    let ctx = match AppContext::new() {
        Ok(ctx) => Rc::new(ctx),
        Err(message) => {
            eprintln!("moniker: {message}");
            return glib::ExitCode::FAILURE;
        }
    };
    // Live updates: every window that shows profiles follows the file.
    ctx.watch_profile_file();
    // HANDLES_COMMAND_LINE: the window a launch asks for is opened by the
    // `command-line` handler, in this process if it is the first Moniker and in
    // the running one otherwise (GApplication hands the line over the session
    // bus and gives back the status and the text printed there).
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_startup(glib::clone!(
        #[strong]
        ctx,
        move |app| {
            add_actions(app, &ctx);
            set_accels(app);
            window::close_side_windows_with_last_main_window(app);
        }
    ));
    // The first command line (or activation) is the launch; a later one, while
    // this one runs, only opens another window.
    let launched = Rc::new(Cell::new(false));
    app.connect_command_line(glib::clone!(
        #[strong]
        ctx,
        #[strong]
        launched,
        move |app, cmdline| {
            // Not `cmdline.environ()` or `cmdline.cwd()`: the folder was chosen
            // in the launching process, and a launch handed to a running
            // Moniker passes its environment on to nobody (the new terminal
            // gets this process's own).
            let (status, stderr) = match handed_over_words(&cmdline.arguments()) {
                Ok(words) => handle_command_line(app, &ctx, &launched, &words),
                Err(why) => (2, Some(format!("moniker: {why}\n"))),
            };
            if let Some(text) = stderr {
                cmdline.printerr_literal(&text);
            }
            glib::ExitCode::from(exit_status(status))
        }
    ));
    // An activation with no command line (`gapplication launch`, a D-Bus
    // activation): a window in the home folder, the first one as the launch.
    app.connect_activate(move |app| {
        let start = Start::home(&glib::home_dir());
        if launched.replace(true) {
            window::new_window_with(app, &ctx, &start);
        } else {
            launch(app, &ctx, &start);
        }
    });
    let mut argv = vec!["moniker".to_owned()];
    argv.extend(words);
    app.run_with_args(&argv)
}

/// Write `text` to stdout or stderr. A reader that has gone (`moniker --help |
/// head -1`) is no reason to panic, so a failed write is dropped.
fn print_text(to_stdout: bool, text: &str) {
    let written = if to_stdout {
        let mut out = std::io::stdout().lock();
        out.write_all(text.as_bytes()).and_then(|()| out.flush())
    } else {
        let mut err = std::io::stderr().lock();
        err.write_all(text.as_bytes()).and_then(|()| err.flush())
    };
    drop(written);
}

/// An exit status as the byte a process exits with.
fn exit_status(status: i32) -> u8 {
    u8::try_from(status).unwrap_or(1)
}

/// The words a command line handed over, after the program's name, as text.
/// The launching process hands over only text, so a word that is not valid
/// UTF-8 is a mistake in what was handed over.
fn handed_over_words(arguments: &[OsString]) -> Result<Vec<String>, String> {
    arguments
        .iter()
        .skip(1)
        .map(|word| {
            word.to_str().map(str::to_owned).ok_or_else(|| {
                // Printed on the launching process's stderr: cleaned.
                let shown = launch_args::one_line(&word.display().to_string());
                format!("unexpected handover: {shown} (not valid UTF-8)")
            })
        })
        .collect()
}

/// Give the actions in [`SHORTCUTS`] their accelerators.
fn set_accels(app: &adw::Application) {
    for (_, items) in SHORTCUTS {
        for (_, action, accels) in *items {
            let accels: Vec<&str> = accels.split_whitespace().collect();
            app.set_accels_for_action(action, &accels);
        }
    }
}

/// Add the app actions the ☰ menu uses.
fn add_actions(app: &adw::Application, ctx: &Rc<AppContext>) {
    let new_window = gio::ActionEntry::builder("new-window")
        .activate(glib::clone!(
            #[strong]
            ctx,
            move |app: &adw::Application, _, _| {
                window::new_window(app, &ctx);
            }
        ))
        .build();
    let profiles = gio::ActionEntry::builder("profiles")
        .activate(glib::clone!(
            #[strong]
            ctx,
            move |app: &adw::Application, _, _| {
                window::open_profiles(app, &ctx);
            }
        ))
        .build();
    let recordings = gio::ActionEntry::builder("recordings")
        .activate(glib::clone!(
            #[strong]
            ctx,
            move |app: &adw::Application, _, _| {
                recordings::present(app, ctx.transcripts_dir(), &ctx.settings_path);
            }
        ))
        .build();
    let preferences = gio::ActionEntry::builder("preferences")
        .activate(glib::clone!(
            #[strong]
            ctx,
            move |app: &adw::Application, _, _| {
                if let Some(parent) = dialog_parent(app) {
                    preferences::present(&parent, app, &ctx);
                }
            }
        ))
        .build();
    let shortcuts = gio::ActionEntry::builder("shortcuts")
        .activate(|app: &adw::Application, _, _| {
            shortcuts_dialog().present(dialog_parent(app).as_ref());
        })
        .build();
    let about = gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| {
            about_dialog().present(dialog_parent(app).as_ref());
        })
        .build();
    app.add_action_entries([
        new_window,
        profiles,
        recordings,
        preferences,
        shortcuts,
        about,
    ]);
    // `app.sync-now` is not among them: it exists only while there is a sync
    // partner ([`refresh_sync_now`]).
    follow_sync_partner(app, ctx);
}

/// Show Sync Now in the ☰ menu (have `app.sync-now`, whose menu entry is
/// `hidden-when: action-missing`) only while a sync partner is recorded and
/// its profile exists, and let it be chosen only while sync is turned on; check
/// again whenever the profiles change (the switch is in the profile file) and
/// after every sync, so the first successful sync from the sync page offers it
/// at once.
fn follow_sync_partner(app: &adw::Application, ctx: &Rc<AppContext>) {
    // Weak both: the context keeps these closures, and the app the context.
    let refresh: Rc<dyn Fn()> = {
        let app = app.downgrade();
        let ctx = Rc::downgrade(ctx);
        Rc::new(move || {
            if let (Some(app), Some(ctx)) = (app.upgrade(), ctx.upgrade()) {
                refresh_sync_now(&app, &ctx);
            }
        })
    };
    refresh();
    ctx.on_profiles_changed(app, {
        let refresh = Rc::clone(&refresh);
        move || refresh()
    });
    ctx.on_sync_event(app, move |event| {
        if matches!(event, SyncEvent::Finished(_)) {
            refresh();
        }
    });
}

/// Bring `app.sync-now` in line with the partner and the sync switch:
/// * no live partner: no action, so the menu item is hidden (before sync is
///   set up, and when a sync deleted the partner);
/// * a partner and sync turned on: the action, enabled;
/// * a partner and sync turned off (or a switch that cannot be read): the
///   action, disabled, so the item shows grayed out (issue #10, the user's
///   choice: the person sees that Sync Now exists and is off, not that it is
///   gone).
fn refresh_sync_now(app: &adw::Application, ctx: &Rc<AppContext>) {
    let settings = settings::load(&ctx.settings_path);
    // Bind first so the store borrow ends here.
    let has_partner = sync::live_partner(&settings, &ctx.store.borrow()).is_some();
    // The file's word, except that a request to turn sync off counts before its
    // save has landed.
    let sync_on = ctx.sync_is_on();
    if !has_partner {
        app.remove_action("sync-now");
        return;
    }
    let action = match app
        .lookup_action("sync-now")
        .and_downcast::<gio::SimpleAction>()
    {
        Some(action) => action,
        None => {
            let action = sync_now_action(app, ctx);
            app.add_action(&action);
            action
        }
    };
    action.set_enabled(sync_on);
}

/// The `app.sync-now` action.
fn sync_now_action(app: &adw::Application, ctx: &Rc<AppContext>) -> gio::SimpleAction {
    let action = gio::SimpleAction::new("sync-now", None);
    // The app owns the action: a strong reference back would be a cycle.
    action.connect_activate(glib::clone!(
        #[weak]
        app,
        #[strong]
        ctx,
        move |_, _| sync_now(&app, &ctx)
    ));
    action
}

/// Sync Now: sync with the partner and say how it went in a notification, in
/// the main window it was chosen from.
fn sync_now(app: &adw::Application, ctx: &Rc<AppContext>) {
    sync_now_with(app, ctx, StaleSync::Refuse);
}

/// [`sync_now`], going ahead or not when a machine has been away; one that
/// stopped for that asks over that window whether to sync anyway (issue #8).
fn sync_now_with(app: &adw::Application, ctx: &Rc<AppContext>, stale: StaleSync) {
    let settings = settings::load(&ctx.settings_path);
    // Bind first so the store borrow ends here.
    let partner = sync::live_partner(&settings, &ctx.store.borrow());
    let Some(partner) = partner else {
        // The partner went away since the menu was drawn (a sync deleted it):
        // the sync page is where another one is chosen.
        profiles::present_sync(app, ctx, window::connect_fn(app, ctx));
        return;
    };
    let main = window::last_active(app);
    let over = main.as_ref().map(|main| main.window.downgrade());
    let near = main.map(|main| main.notifications.downgrade());
    let tell: Rc<dyn Fn(&str)> = {
        let notice = ctx.notice.clone();
        Rc::new(
            move |text| match near.as_ref().and_then(glib::WeakRef::upgrade) {
                Some(area) => notice.show_near(&area, text),
                None => notice.show(text),
            },
        )
    };
    let started = sync::begin_app_sync(ctx, &partner, stale, sync::runner(ctx), {
        let tell = Rc::clone(&tell);
        let (app, ctx) = (app.clone(), Rc::clone(ctx));
        move |result| {
            if let Err(failure) = result
                && stale == StaleSync::Refuse
                && sync::is_away(failure)
                && let Some(over) = over.as_ref().and_then(glib::WeakRef::upgrade)
            {
                profiles::sync_page::ask_to_sync_anyway(
                    &over,
                    &sync::failure_text(failure),
                    move || sync_now_with(&app, &ctx, StaleSync::Allow),
                );
                return;
            }
            let (text, is_error) = sync::result_text(result);
            tell(&if is_error {
                format!("Sync failed: {text}")
            } else {
                text
            });
        }
    });
    match started {
        Ok(()) => {}
        Err(message) if message == sync::ALREADY_RUNNING => tell(&message),
        Err(message) => tell(&format!("Sync failed: {message}")),
    }
}

/// A command line reached this Moniker (its own, when it is the first, or one
/// handed over by another process): `args` are the words
/// [`launch_args::encode`] made. The first one is the launch, which opens the
/// first window and runs the automatic sync; every later one opens a window and
/// nothing else. Returns the exit status of the command line and what to print
/// on the launching process's stderr, if anything. A mistake in what was handed
/// over opens no window and is no launch.
///
/// The window is opened here, before this returns, so the line is not lost with
/// a window that closed a moment ago; it is presented, so it has the focus (a
/// dialog stays with the window it is over).
pub(crate) fn handle_command_line(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    launched: &Cell<bool>,
    args: &[String],
) -> (i32, Option<String>) {
    let start = match launch_args::decode(args, &glib::home_dir()) {
        Ok(start) => start,
        Err(why) => return (2, Some(format!("moniker: {why}\n"))),
    };
    if launched.replace(true) {
        window::new_window_with(app, ctx, &start);
    } else {
        launch(app, ctx, &start);
    }
    (0, None)
}

/// The launch: the terminal font and scrollback of the Preferences, the first
/// window (a shell in the home folder, or what `start` says), and once it is
/// shown (from an idle callback, so the window and its terminal come first), the
/// automatic sync.
///
/// The application is held from here until that sync is over, or until it is
/// known there is none ([`sync_on_launch`]). A launch whose command ends at
/// once (`moniker -- true`, `x-terminal-emulator -e quick-command`) closes its
/// only window a moment later, and an application with no window and nothing
/// holding it quits: before the idle callback has even started the sync, or
/// between the secrets a sync stores and the profile file it writes after them,
/// which skips its rollback. (A keyring job holds it the same way, see
/// `keyring::change`.)
pub(crate) fn launch(app: &adw::Application, ctx: &Rc<AppContext>, start: &Start) {
    terminal_tab::use_preferences(&ctx.settings());
    let hold = app.hold();
    window::new_window_with(app, ctx, start);
    let ctx = Rc::clone(ctx);
    glib::idle_add_local_once(move || sync_on_launch(&ctx, hold));
}

/// The automatic sync: with the partner, if there is one, the automatic sync
/// switch is on and sync is not turned off (then nothing runs and nothing is
/// said: the person turned it off on purpose). Only the profile is read here;
/// the worker reads the keyring (which may ask to be unlocked) and runs the
/// exchange, so the new window and its terminal never wait for either. A
/// success is silent; a failure is one notification. If a sync is already
/// running (started by hand meanwhile), this one is skipped.
///
/// `hold` is [`launch`]'s hold on the application. It goes on to the sync's
/// callback, which drops it when the sync is over, however it ended; when there
/// is no sync, or none could start, it is dropped here.
fn sync_on_launch(ctx: &Rc<AppContext>, hold: gio::ApplicationHoldGuard) {
    let settings = settings::load(&ctx.settings_path);
    // Bind first so the store borrow ends here. A request to turn sync off that
    // has not landed in the file yet counts too.
    let target = ctx
        .sync_is_on()
        .then(|| sync::auto_sync_target(&settings, &ctx.store.borrow()))
        .flatten();
    let Some(partner) = target else {
        return;
    };
    let notice = ctx.notice.clone();
    let report = move |why: &str| notice.show(&format!("Automatic sync failed: {why}"));
    let started = sync::begin_app_sync(ctx, &partner, StaleSync::Refuse, sync::runner(ctx), {
        let report = report.clone();
        let notice = ctx.notice.clone();
        move |result| {
            // Dropped when this callback ends (or, if no sync starts, with it).
            let _hold = hold;
            match result {
                // Never asked at launch: Sync Now is where the person decides.
                Err(failure) if sync::is_away(failure) => notice.show(&format!(
                    "Did not sync automatically. {} Use Sync Now to sync anyway.",
                    sync::failure_text(failure)
                )),
                Err(failure) => report(&sync::failure_text(failure)),
                Ok(_) => {}
            }
        }
    });
    match started {
        Ok(()) => {}
        Err(message) if message == sync::ALREADY_RUNNING => {}
        Err(message) => report(&message),
    }
}

/// The window a dialog opened from the menu belongs over: the active one, else
/// the main window that was active last.
fn dialog_parent(app: &adw::Application) -> Option<gtk::Window> {
    app.active_window()
        .or_else(|| window::last_active(app).map(|main| main.window.upcast()))
}

/// The Keyboard Shortcuts dialog: [`SHORTCUTS`].
fn shortcuts_dialog() -> adw::ShortcutsDialog {
    let dialog = adw::ShortcutsDialog::new();
    for (title, items) in SHORTCUTS {
        let section = adw::ShortcutsSection::new(Some(title));
        for (name, _, accel) in *items {
            section.add(adw::ShortcutsItem::new(name, accel));
        }
        dialog.add(section);
    }
    dialog
}

/// About Moniker: name, version, the copyright line, the MIT license and the
/// repository as its website.
fn about_dialog() -> adw::AboutDialog {
    adw::AboutDialog::builder()
        .application_name("Moniker")
        .version(env!("CARGO_PKG_VERSION"))
        .copyright(ABOUT_COPYRIGHT)
        .license_type(gtk::License::MitX11)
        .website(REPOSITORY_URL)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_about_text_names_the_owner_and_license() {
        assert_eq!(ABOUT_COPYRIGHT, "Copyright 2026 Justin M. Sloan");
    }

    #[test]
    fn the_about_box_links_the_repository() {
        assert_eq!(REPOSITORY_URL, "https://github.com/justinsloan/moniker");
    }

    #[test]
    fn a_handover_is_the_words_after_the_program_name() {
        use std::os::unix::ffi::OsStringExt;
        let line = |words: &[&str]| -> Vec<OsString> { words.iter().map(OsString::from).collect() };
        assert_eq!(
            handed_over_words(&line(&["moniker", "--moniker-dir=/tmp", "--moniker-hold"])),
            Ok(vec![
                "--moniker-dir=/tmp".to_owned(),
                "--moniker-hold".to_owned()
            ])
        );
        assert_eq!(handed_over_words(&line(&["moniker"])), Ok(Vec::new()));
        assert_eq!(handed_over_words(&[]), Ok(Vec::new()));
        // Only text is handed over: anything else is a mistake that names it.
        let bad = OsString::from_vec(vec![b'a', 0xff, b'b']);
        assert_eq!(
            handed_over_words(&[OsString::from("moniker"), bad]),
            Err("unexpected handover: a\u{fffd}b (not valid UTF-8)".to_owned())
        );
        // It is printed on the launching process's stderr, often a terminal:
        // an escape sequence or a line break in the word comes out as spaces.
        let nasty = OsString::from_vec(b"\x1b]0;x\x07\n\xffb".to_vec());
        assert_eq!(
            handed_over_words(&[OsString::from("moniker"), nasty]),
            Err("unexpected handover:  ]0;x  \u{fffd}b (not valid UTF-8)".to_owned())
        );
    }

    #[test]
    fn the_exit_status_is_a_byte() {
        assert_eq!(exit_status(0), 0);
        assert_eq!(exit_status(2), 2);
        assert_eq!(exit_status(255), 255);
        // Out of range is a failure, not a different status that happens to fit.
        assert_eq!(exit_status(256), 1);
        assert_eq!(exit_status(-1), 1);
    }
}
