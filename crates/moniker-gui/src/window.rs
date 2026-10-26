//! The main window: a header bar over the terminal tabs, and nothing else.
//!
//! The header bar holds, left to right: ＋ (a new tab here showing the new-tab
//! list, [`crate::new_tab`], where a saved profile becomes a session in that
//! tab), the selected tab's title, and ☰ (the app's actions, which `main.rs`
//! defines, under a zoom row for the selected tab's text size). There is no
//! Connect button: sessions come from the new-tab list and the Connection
//! Profiles window.
//! Below it the tab bar, which hides itself while there is one tab, so a fresh
//! window looks like a plain terminal; then the tabs ([`crate::terminal_tab`])
//! inside a notification area ([`crate::notify`]) attached to the context's
//! [`crate::context::Notice`], so messages appear at the top center of the
//! terminal, below the bars.
//!
//! Each main window has the actions of its tabs: `win.new-tab` (as ＋ does),
//! `win.next-tab` and `win.previous-tab`; of the panes of a tab
//! (`crate::panes`), on the selected tab's focused pane: `win.split-right` and
//! `win.split-down` (a new pane beside it that shows the new-tab list),
//! `win.close-pane` (the tab, with one pane), `win.pane-to-tab` and
//! `win.pane-to-window`, and `win.pane-left`, `win.pane-right`, `win.pane-up`
//! and `win.pane-down` (going to another pane; the last four and the moves are
//! enabled only in a tab with several); and `win.copy`, `win.paste`,
//! `win.zoom-in`, `win.zoom-out` and `win.zoom-reset`, which act on the focused
//! pane's terminal and are enabled while it has one
//! ([`terminal_tab::sync_window_actions`]). `main.rs` gives them their
//! accelerators. While a dialog (Rename, About, Keyboard Shortcuts ...) is open
//! over the window all of these are disabled, so an accelerator's key reaches
//! the dialog and not the tab behind it.
//!
//! Every pane has its own text size. The zoom row of ☰ ([`zoom_row`]) shows the
//! focused pane's of the selected tab and follows it as the tab, the pane or
//! the size changes. When the last main window closes, the size of the pane it
//! had active (or, if its tabs were closed one by one, of the last one to go)
//! is saved for the next launch
//! ([`save_scale_when_last_window_closes`]); a window with no tab to take a size
//! from starts its first tab at that saved size.
//!
//! There can be several main windows ([`new_window`]). A tab can be dragged
//! from one into another, or out of every window, which makes a new main
//! window for it (the tab view's `create-window`); a window whose last tab
//! leaves closes. A moved tab keeps its sessions, titles and transcript
//! settings. A pane moves the same way, to another tab or window
//! (`crate::panes`).
//! Sessions started from the Connection Profiles window ([`crate::profiles`])
//! open in the one that was active last ([`last_active`], [`connect_fn`]), or in
//! a new one if none is left. When the last main window goes, the Connection
//! Profiles and Transcripts windows go with it, so the app quits
//! ([`close_side_windows_with_last_main_window`]).
//!
//! The title shows the tab's title, which may be a profile name from another
//! machine: `adw::WindowTitle` shows it as plain text.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;

use crate::context::AppContext;
use crate::notify::NotificationArea;
use crate::window_size::{self, Kind};
use crate::{new_tab, panes, profiles, recordings, settings, terminal_tab, zoom};

/// Widget name of a main window, how [`last_active`] tells it from the
/// Connection Profiles window and dialogs.
const WINDOW_NAME: &str = "moniker-main";

/// Widget name of the zoom row's percent button, the one that shows the
/// selected tab's size and resets it.
pub(crate) const ZOOM_PERCENT_NAME: &str = "zoom-percent";

/// The id the ☰ menu's zoom item has for the row that replaces it
/// (`gtk::PopoverMenu::add_child`).
const ZOOM_ITEM: &str = "zoom";

/// A main window and the parts of it others work with.
#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    /// The terminal tabs.
    pub view: adw::TabView,
    /// The notification area around the tabs, attached to the context's notice.
    pub notifications: NotificationArea,
}

impl MainWindow {
    /// The parts of `window`, if it is a main window.
    fn of(window: &gtk::Window) -> Option<MainWindow> {
        if window.widget_name() != WINDOW_NAME {
            return None;
        }
        let window = window.clone().downcast::<adw::ApplicationWindow>().ok()?;
        let toolbar = window.content().and_downcast::<adw::ToolbarView>()?;
        let notifications = toolbar.content().and_downcast::<NotificationArea>()?;
        let view = notifications
            .widget()
            .child()
            .and_downcast::<adw::TabView>()?;
        Some(MainWindow {
            window,
            view,
            notifications,
        })
    }
}

/// Build a main window with one local tab. The caller presents it. The tab
/// saves its transcript when "Save transcripts of all sessions" is on: that
/// covers the tab a window opens with, at launch and for New Window too, and is
/// decided here, before the shell starts.
pub fn build(app: &adw::Application, ctx: &Rc<AppContext>) -> MainWindow {
    let main = build_empty(app, ctx);
    let settings = ctx.settings();
    let save = terminal_tab::default_save(false, settings.save_all_transcripts);
    terminal_tab::open_local_shell(
        &main.view,
        settings.font_scale,
        terminal_tab::local_transcript_spec(ctx, save),
    );
    main
}

/// Open another main window with one local tab (☰ → New Window).
pub fn new_window(app: &adw::Application, ctx: &Rc<AppContext>) -> MainWindow {
    let main = build(app, ctx);
    main.window.present();
    main
}

/// Build a main window with no tab yet. Its first tab must come before it is
/// shown, or (a tab dropped outside every window) at once after: the window
/// closes when its last tab does, and not before it had one.
fn build_empty(app: &adw::Application, ctx: &Rc<AppContext>) -> MainWindow {
    let view = adw::TabView::builder().hexpand(true).vexpand(true).build();
    terminal_tab::setup_tab_view(&view);
    let tab_bar = adw::TabBar::builder().view(&view).autohide(true).build();
    terminal_tab::setup_tab_bar(&tab_bar);
    let notifications = NotificationArea::new(&view);
    ctx.notice.attach(&notifications);

    // A tab dropped outside every window: a new main window, presented, whose
    // view the tab view moves the tab into. The application is held weakly:
    // it keeps the windows, and so this handler.
    view.connect_create_window(glib::clone!(
        #[weak]
        app,
        #[strong]
        ctx,
        #[upgrade_or]
        None,
        move |view| {
            // Not for a tab that was dropped on a pane: its panes went there,
            // and its emptied page goes back where it came from, to be closed.
            // (No view at all would do the same, with a critical warning.)
            if panes::claims_new_window() {
                return Some(view.clone());
            }
            let main = build_empty(&app, &ctx);
            main.window.present();
            Some(main.view)
        }
    ));

    // ＋: the new-tab list in a new tab here; the same action as Ctrl+Shift+T.
    let new_tab = gtk::Button::builder()
        .icon_name("tab-new-symbolic")
        .tooltip_text("New Tab")
        .action_name("win.new-tab")
        .build();

    // Not `primary`: that would take F10, which terminal programs (htop, mc)
    // use.
    let main_menu = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(&main_menu())
        .build();
    let (zoom_row, percent) = zoom_row();
    if let Some(menu) = main_menu.popover().and_downcast::<gtk::PopoverMenu>() {
        menu.add_child(&zoom_row, ZOOM_ITEM);
    }

    let title = adw::WindowTitle::new("Moniker", "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_start(&new_tab);
    header.pack_end(&main_menu);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&tab_bar);
    toolbar.set_content(Some(&notifications));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .name(WINDOW_NAME)
        .title("Moniker")
        .content(&toolbar)
        .build();
    // At the size the main window that closed last had (issue #14).
    window_size::remember(&window, Kind::Main, &ctx.settings_path, DEFAULT_SIZE);

    // The title in the header bar, and the window's own, follow the selected
    // tab's title. The view is held weakly, so the bindings keep nothing alive.
    let selected_title = view
        .property_expression_weak("selected-page")
        .chain_property::<adw::TabPage>("title");
    selected_title.bind(&title, "title", gtk::Widget::NONE);
    selected_title.bind(&window, "title", gtk::Widget::NONE);

    // The size of the selected tab, as the zoom row shows it, and as the last
    // window to close saves it. Kept as it changes because a window whose last
    // tab closed has no selected tab left to ask.
    let scale = Rc::new(Cell::new(None));
    let show_scale = follow_selected_scale(&view, &percent, &scale);

    add_tab_actions(&window, &view, ctx);
    add_pane_actions(&window, &view, app, ctx);
    add_clipboard_actions(&window, &view);
    add_zoom_actions(&window, &view, &show_scale);
    // The actions on a terminal apply while the selected tab has one, and the
    // actions on tabs and terminals only while no dialog is open over the
    // window: GTK runs an accelerator before the focused widget, dialog fields
    // included, so with the actions enabled Ctrl+Shift+V would paste into the
    // terminal behind the Rename dialog.
    view.connect_selected_page_notify(terminal_tab::sync_window_actions);
    window.connect_visible_dialog_notify(glib::clone!(
        #[weak]
        view,
        move |_| terminal_tab::sync_window_actions(&view)
    ));
    terminal_tab::sync_window_actions(&view);
    save_scale_when_last_window_closes(&window, ctx, &scale);
    terminal_tab::end_sessions_on_close(&window, &view);
    MainWindow {
        window,
        view,
        notifications,
    }
}

/// The window's tab actions: `win.new-tab` (the new-tab list in a new tab),
/// `win.next-tab` and `win.previous-tab`. The window owns them, so the view is
/// held weakly.
fn add_tab_actions(window: &adw::ApplicationWindow, view: &adw::TabView, ctx: &Rc<AppContext>) {
    let new_tab = gio::ActionEntry::builder(terminal_tab::NEW_TAB_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            #[strong]
            ctx,
            move |_: &adw::ApplicationWindow, _, _| {
                new_tab::open(&view, &ctx);
            }
        ))
        .build();
    let next_tab = gio::ActionEntry::builder(terminal_tab::NEXT_TAB_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            move |_: &adw::ApplicationWindow, _, _| {
                view.select_next_page();
            }
        ))
        .build();
    let previous_tab = gio::ActionEntry::builder(terminal_tab::PREVIOUS_TAB_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            move |_: &adw::ApplicationWindow, _, _| {
                view.select_previous_page();
            }
        ))
        .build();
    window.add_action_entries([new_tab, next_tab, previous_tab]);
}

/// The window's pane actions, on the focused pane of the selected tab:
/// `win.split-right` and `win.split-down` (a new pane beside it, which shows
/// the new-tab list), `win.close-pane` (as its close button does; with one
/// pane, the tab's), `win.pane-to-tab`, `win.pane-to-window`, and
/// `win.pane-left`, `win.pane-right`, `win.pane-up` and `win.pane-down`. The
/// window owns them, so the view and the application are held weakly.
fn add_pane_actions(
    window: &adw::ApplicationWindow,
    view: &adw::TabView,
    app: &adw::Application,
    ctx: &Rc<AppContext>,
) {
    let split = |name: &str, side: panes::Side| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                view,
                #[strong]
                ctx,
                move |_: &adw::ApplicationWindow, _, _| {
                    if let Some(holder) = panes::split(&view, side) {
                        new_tab::fill(&holder, &ctx);
                    }
                }
            ))
            .build()
    };
    let close = gio::ActionEntry::builder(panes::CLOSE_PANE_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            move |_: &adw::ApplicationWindow, _, _| {
                if let Some(holder) = terminal_tab::selected_holder(&view) {
                    terminal_tab::close_pane(&holder);
                }
            }
        ))
        .build();
    let to_tab = gio::ActionEntry::builder(panes::TO_TAB_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            move |_: &adw::ApplicationWindow, _, _| {
                let pane = terminal_tab::selected_holder(&view).and_then(|h| panes::pane_of(&h));
                if let (Some(pane), Some(page)) = (pane, view.selected_page()) {
                    panes::move_to_new_page(&pane, &view, view.page_position(&page) + 1);
                }
            }
        ))
        .build();
    // A window of its own, presented; the pane goes into its view.
    let to_window = gio::ActionEntry::builder(panes::TO_WINDOW_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            #[weak]
            app,
            #[strong]
            ctx,
            move |_: &adw::ApplicationWindow, _, _| {
                let Some(pane) =
                    terminal_tab::selected_holder(&view).and_then(|h| panes::pane_of(&h))
                else {
                    return;
                };
                if !panes::selected_is_split(&view) {
                    return;
                }
                let main = build_empty(&app, &ctx);
                panes::move_to_new_page(&pane, &main.view, 0);
                main.window.present();
            }
        ))
        .build();
    let toward = |name: &str, direction: panes::Direction| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                view,
                move |_: &adw::ApplicationWindow, _, _| panes::focus_toward(&view, direction)
            ))
            .build()
    };
    window.add_action_entries([
        split(panes::SPLIT_RIGHT_ACTION, panes::Side::Right),
        split(panes::SPLIT_DOWN_ACTION, panes::Side::Bottom),
        close,
        to_tab,
        to_window,
        toward(panes::PANE_LEFT_ACTION, panes::Direction::Left),
        toward(panes::PANE_RIGHT_ACTION, panes::Direction::Right),
        toward(panes::PANE_UP_ACTION, panes::Direction::Up),
        toward(panes::PANE_DOWN_ACTION, panes::Direction::Down),
    ]);
}

/// The window's clipboard actions: `win.copy` and `win.paste` (Ctrl+Shift+C and
/// Ctrl+Shift+V), on the terminal of the selected tab's focused pane. They start disabled and are enabled while the
/// selected tab has a terminal, whether or not it has a selection:
/// Ctrl+Shift+C with nothing selected must be handled (and do nothing) rather
/// than reach the program ([`terminal_tab::copy_selection`]). The window owns
/// them, so the view is held weakly.
fn add_clipboard_actions(window: &adw::ApplicationWindow, view: &adw::TabView) {
    let copy = gio::ActionEntry::builder(terminal_tab::COPY_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            move |_: &adw::ApplicationWindow, _, _| {
                terminal_tab::copy_selection(&view);
            }
        ))
        .build();
    let paste = gio::ActionEntry::builder(terminal_tab::PASTE_ACTION)
        .activate(glib::clone!(
            #[weak]
            view,
            move |_: &adw::ApplicationWindow, _, _| {
                terminal_tab::paste_clipboard(&view);
            }
        ))
        .build();
    window.add_action_entries([copy, paste]);
}

/// The window's text size actions: `win.zoom-in` and `win.zoom-out` (one step
/// of `crate::zoom` up and down) and `win.zoom-reset` (the normal size), on the
/// selected tab's focused pane only. Each one then has the zoom row show the new size.
/// ([`terminal_tab::sync_window_actions`] enables them.) The window owns the
/// actions, so the view is held weakly.
fn add_zoom_actions(
    window: &adw::ApplicationWindow,
    view: &adw::TabView,
    show_scale: &Rc<dyn Fn()>,
) {
    let entry = |name: &str, change: fn(f64) -> f64| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                view,
                #[strong]
                show_scale,
                move |_: &adw::ApplicationWindow, _, _| {
                    terminal_tab::zoom_selected(&view, change);
                    show_scale();
                }
            ))
            .build()
    };
    window.add_action_entries([
        entry(terminal_tab::ZOOM_IN_ACTION, zoom::larger),
        entry(terminal_tab::ZOOM_OUT_ACTION, zoom::smaller),
        entry(terminal_tab::ZOOM_RESET_ACTION, |_| zoom::DEFAULT),
    ]);
}

/// The zoom row of the ☰ menu: − (`win.zoom-out`), the selected tab's size as
/// a percent, which is a button that resets it (`win.zoom-reset`), and +
/// (`win.zoom-in`). Returns the row and the percent button, for
/// [`follow_selected_scale`] to keep up to date. The buttons are the actions',
/// so they are insensitive while the selected tab has no terminal.
fn zoom_row() -> (gtk::Box, gtk::Button) {
    let action = |name: &str| format!("win.{name}");
    let smaller = gtk::Button::builder()
        .icon_name("zoom-out-symbolic")
        .tooltip_text("Smaller Text")
        .action_name(action(terminal_tab::ZOOM_OUT_ACTION))
        .build();
    let percent = gtk::Button::builder()
        .name(ZOOM_PERCENT_NAME)
        .label(zoom::percent(zoom::DEFAULT))
        .tooltip_text("Normal Text Size")
        .action_name(action(terminal_tab::ZOOM_RESET_ACTION))
        .hexpand(true)
        .css_classes(["numeric"])
        .build();
    let larger = gtk::Button::builder()
        .icon_name("zoom-in-symbolic")
        .tooltip_text("Larger Text")
        .action_name(action(terminal_tab::ZOOM_IN_ACTION))
        .build();
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    row.append(&smaller);
    row.append(&percent);
    row.append(&larger);
    (row, percent)
}

/// The selected tab's layout that [`follow_selected_scale`] follows, and its
/// handler there.
type Followed = (glib::WeakRef<panes::TabLayout>, glib::SignalHandlerId);

/// Keep `percent` (the zoom row's button) and `scale` showing the text size of
/// the focused pane of `view`'s selected tab: now, and whenever the selected
/// tab or its focused pane changes. The returned function does it again; the
/// zoom actions call it after a change.
///
/// `scale` is the size of the last tab that was selected, `None` until there
/// was one. It is what [`save_scale_when_last_window_closes`] saves, and it
/// outlives the tabs: a window whose last tab has closed has no selected tab
/// left to ask. Both widgets are held weakly.
fn follow_selected_scale(
    view: &adw::TabView,
    percent: &gtk::Button,
    scale: &Rc<Cell<Option<f64>>>,
) -> Rc<dyn Fn()> {
    let show: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        view,
        #[weak]
        percent,
        #[strong]
        scale,
        move || {
            if let Some(now) = view
                .selected_page()
                .and_then(|page| terminal_tab::scale_of(&page))
            {
                percent.set_label(&zoom::percent(now));
                scale.set(Some(now));
            }
        }
    ));
    // The selected tab's layout tells when its focused pane changes; only the
    // selected one is followed.
    let followed: Rc<RefCell<Option<Followed>>> = Rc::default();
    let follow = glib::clone!(
        #[strong]
        show,
        move |view: &adw::TabView| {
            if let Some((layout, handler)) = followed.take()
                && let Some(layout) = layout.upgrade()
            {
                layout.disconnect(handler);
            }
            if let Some(layout) = view
                .selected_page()
                .as_ref()
                .and_then(panes::layout_of_page)
            {
                let show = Rc::clone(&show);
                let handler = layout.connect_local(panes::FOCUS_CHANGED, false, move |_| {
                    show();
                    None
                });
                followed.replace(Some((layout.downgrade(), handler)));
            }
            show();
        }
    );
    follow(view);
    view.connect_selected_page_notify(follow);
    show
}

/// When `window` closes and it is the last main window of its application,
/// save the text size it last had ([`follow_selected_scale`]) for the next
/// launch: the size of its active tab, or, if its tabs were closed one by one,
/// of the one that went last. A window that is not the last saves nothing, so
/// of several windows closed one after the other the last decides.
fn save_scale_when_last_window_closes(
    window: &adw::ApplicationWindow,
    ctx: &Rc<AppContext>,
    scale: &Rc<Cell<Option<f64>>>,
) {
    window.connect_close_request(glib::clone!(
        #[strong]
        ctx,
        #[strong]
        scale,
        move |window| {
            let others = window.application().is_some_and(|app| {
                app.windows()
                    .iter()
                    .any(|w| w != window && w.widget_name() == WINDOW_NAME)
            });
            if !others
                && let Some(scale) = scale.get()
                && let Err(err) = settings::save_font_scale(&ctx.settings_path, scale)
            {
                eprintln!(
                    "moniker: cannot save the text size in {}: {err}",
                    ctx.settings_path.display()
                );
            }
            glib::Propagation::Proceed
        }
    ));
}

/// The size of a main window when none has closed yet.
const DEFAULT_SIZE: (i32, i32) = (1100, 700);

/// The ☰ menu: the zoom row, then entries that are app actions (`main.rs`),
/// Preferences among them.
/// Sync Now is hidden while its action is missing, that is while no sync
/// partner is recorded, and shown grayed out while sync is turned off (its
/// action is then disabled; `main.rs` keeps both).
fn main_menu() -> gio::Menu {
    // A custom item: the zoom row takes its place ([`zoom_row`]).
    let zoom_item = gio::MenuItem::new(None, None);
    zoom_item.set_attribute_value("custom", Some(&ZOOM_ITEM.to_variant()));
    let zoom_section = gio::Menu::new();
    zoom_section.append_item(&zoom_item);
    let window_section = gio::Menu::new();
    window_section.append(Some("New Window"), Some("app.new-window"));
    let app_section = gio::Menu::new();
    app_section.append(Some("Connection Profiles"), Some("app.profiles"));
    app_section.append(Some("Transcripts"), Some("app.recordings"));
    let sync_now = gio::MenuItem::new(Some("Sync Now"), Some("app.sync-now"));
    sync_now.set_attribute_value("hidden-when", Some(&"action-missing".to_variant()));
    app_section.append_item(&sync_now);
    let about_section = gio::Menu::new();
    about_section.append(Some("Preferences"), Some("app.preferences"));
    about_section.append(Some("Keyboard Shortcuts"), Some("app.shortcuts"));
    about_section.append(Some("About Moniker"), Some("app.about"));
    let menu = gio::Menu::new();
    menu.append_section(None, &zoom_section);
    menu.append_section(None, &window_section);
    menu.append_section(None, &app_section);
    menu.append_section(None, &about_section);
    menu
}

/// The main window of `app` that was active last, if any is open. (GTK keeps
/// an application's windows most recently focused first.)
pub fn last_active(app: &adw::Application) -> Option<MainWindow> {
    app.windows().iter().find_map(MainWindow::of)
}

/// When the last main window of `app` goes, close the Connection Profiles and
/// Transcripts windows too, so the app quits. (A dialog goes with the window it
/// belongs to.)
pub fn close_side_windows_with_last_main_window(app: &adw::Application) {
    app.connect_window_removed(|app, removed| {
        let was_main = removed.widget_name() == WINDOW_NAME;
        let main_left = app
            .windows()
            .iter()
            .any(|w| w != removed && w.widget_name() == WINDOW_NAME);
        if was_main && !main_left {
            profiles::close(app);
            recordings::close(app);
        }
    });
}

/// Show the Connection Profiles window of `app`; sessions started from it open as
/// [`connect_fn`] says.
pub fn open_profiles(app: &adw::Application, ctx: &Rc<AppContext>) -> adw::Window {
    profiles::present(app, ctx, connect_fn(app, ctx))
}

/// What connecting from the Connection Profiles window does: a new tab in the main window
/// that was active last (a new window if none is left), which is raised.
pub fn connect_fn(app: &adw::Application, ctx: &Rc<AppContext>) -> profiles::ConnectFn {
    connect_with(app, ctx, terminal_tab::open_profile)
}

/// [`connect_fn`] with the session opened by `open` (tests swap in one that
/// starts no `ssh`). Holds the application weakly: the Connection Profiles window keeps
/// this, and the application keeps that window.
pub(crate) fn connect_with(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    open: impl Fn(&adw::TabView, &Rc<AppContext>, &Profile, Option<usize>, bool) + 'static,
) -> profiles::ConnectFn {
    let app = app.downgrade();
    let ctx = Rc::clone(ctx);
    Rc::new(move |profile, sequence, record| {
        let Some(app) = app.upgrade() else {
            return;
        };
        let main = last_active(&app).unwrap_or_else(|| build_empty(&app, &ctx));
        open(&main.view, &ctx, profile, sequence, record);
        main.window.present();
    })
}
