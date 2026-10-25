//! Moniker GUI: terminal tabs under a header bar, with saved connection
//! profiles one click away (the new-tab list) and managed in their own window.
//!
//! This file starts the application and defines its actions, the entries of
//! the main window's ☰ menu: New Window, Connection Profiles, Transcripts, Sync
//! Now, Preferences, Keyboard Shortcuts and About; and the keyboard shortcuts
//! ([`SHORTCUTS`]), which are the accelerators of those and of the main
//! window's tab, clipboard and text size actions.
//!
//! It also starts the automatic sync on launch ([`launch`]). Sync Now and the
//! sync on launch both sync with the partner recorded in the GUI settings
//! ([`settings`]) by the first successful sync from the sync page; until then
//! neither exists for the person (Sync Now is hidden, nothing syncs on
//! launch).

mod connect_menu;
mod context;
mod form;
#[cfg(test)]
mod gui_tests;
mod hosts;
mod keyring;
mod new_tab;
mod notify;
mod panes;
mod preferences;
mod profiles;
mod recordings;
mod settings;
mod sync;
mod terminal_tab;
mod theme;
mod window;
mod zoom;

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use crate::context::AppContext;
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
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(glib::clone!(
        #[strong]
        ctx,
        move |app| {
            add_actions(app, &ctx);
            set_accels(app);
            window::close_side_windows_with_last_main_window(app);
        }
    ));
    // The first activation is the launch; a later one (`moniker` started
    // again while this one runs) only opens another window.
    let launched = Cell::new(false);
    app.connect_activate(move |app| {
        if launched.replace(true) {
            window::new_window(app, &ctx);
        } else {
            launch(app, &ctx);
        }
    });
    app.run()
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
                recordings::present(app, ctx.transcripts_dir());
            }
        ))
        .build();
    let sync_now = gio::ActionEntry::builder("sync-now")
        .activate(glib::clone!(
            #[strong]
            ctx,
            move |app: &adw::Application, _, _| {
                sync_now(app, &ctx);
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
        sync_now,
        preferences,
        shortcuts,
        about,
    ]);
    follow_sync_partner(app, ctx);
}

/// Offer Sync Now (enable `app.sync-now`, which shows its menu entry) only
/// while a sync partner is recorded and its profile exists; check again
/// whenever the profiles change and after every sync, so the first successful
/// sync from the sync page offers it at once.
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

fn refresh_sync_now(app: &adw::Application, ctx: &AppContext) {
    let settings = settings::load(&ctx.settings_path);
    let enabled = sync::live_partner(&settings, &ctx.store.borrow()).is_some();
    if let Some(action) = app
        .lookup_action("sync-now")
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(enabled);
    }
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

/// The launch: the terminal font and scrollback of the Preferences, the first
/// window, and once it is shown (from an idle callback, so the window and its
/// terminal come first), the automatic sync.
pub(crate) fn launch(app: &adw::Application, ctx: &Rc<AppContext>) {
    terminal_tab::use_preferences(&ctx.settings());
    window::new_window(app, ctx);
    let ctx = Rc::clone(ctx);
    glib::idle_add_local_once(move || sync_on_launch(&ctx));
}

/// The automatic sync: with the partner, if there is one and the switch is
/// on. Only the profile is read here; the worker reads the keyring (which may
/// ask to be unlocked) and runs the exchange, so the new window and its
/// terminal never wait for either. A success is silent; a failure is one
/// notification. If a sync is already running (started by hand meanwhile),
/// this one is skipped.
fn sync_on_launch(ctx: &Rc<AppContext>) {
    let settings = settings::load(&ctx.settings_path);
    // Bind first so the store borrow ends here.
    let target = sync::auto_sync_target(&settings, &ctx.store.borrow());
    let Some(partner) = target else {
        return;
    };
    let notice = ctx.notice.clone();
    let report = move |why: &str| notice.show(&format!("Automatic sync failed: {why}"));
    let started = sync::begin_app_sync(ctx, &partner, StaleSync::Refuse, sync::runner(ctx), {
        let report = report.clone();
        let notice = ctx.notice.clone();
        move |result| match result {
            // Never asked at launch: Sync Now is where the person decides.
            Err(failure) if sync::is_away(failure) => notice.show(&format!(
                "Did not sync automatically. {} Use Sync Now to sync anyway.",
                sync::failure_text(failure)
            )),
            Err(failure) => report(&sync::failure_text(failure)),
            Ok(_) => {}
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
}
