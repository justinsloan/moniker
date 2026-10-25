//! The Connection Profiles window, laid out like GNOME Settings: a sidebar listing the
//! saved profiles (with ＋ and search) and, beside it, the page of the selected
//! profile ([`page`]), where each field saves on its own. ＋ opens the New
//! Profile dialog ([`new_profile`]). A startup sequence opens as a sub-page
//! ([`sequence_page`]) pushed over the profile page in the content pane;
//! Cancel (or Escape) returns to it.
//!
//! Below the profiles, the sidebar has a Sync entry: it shows the sync page
//! ([`sync_page`]) in the content pane ([`present_sync`] opens the window on
//! it).
//!
//! One Connection Profiles window per application ([`present`]). It is not modal and
//! stays open while sessions start from it: double click or Enter on a
//! profile, or the Connect button in its header, hand the profile to the
//! [`ConnectFn`] the window was opened with.
//!
//! The window follows the profile file through
//! [`AppContext::on_profiles_changed`]: the sidebar is rebuilt (keeping the
//! selection while that profile exists), rows without an unapplied edit show
//! the new values, and a selected profile that a sync deleted leaves an empty
//! state. An open sub-page keeps its text through a refresh, and stays open
//! if its profile is deleted (saving then says so); selecting another profile
//! closes it.
//!
//! Profile data is shown with Pango markup off everywhere, and the page title
//! is a plain label, because names come from other machines.

mod new_profile;
mod page;
mod saving;
mod sequence_page;
pub(crate) mod sync_page;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;

use crate::context::AppContext;
use crate::form;
use crate::notify::NotificationArea;
use crate::terminal_tab::default_save;
use crate::window_size::{self, Kind};
use page::ProfilePage;
use sync_page::SyncPage;

/// What connecting from the Connection Profiles window does: start a session for the
/// profile with this startup sequence (`None`: just connect), recording a
/// transcript or not.
pub type ConnectFn = Rc<dyn Fn(&Profile, Option<usize>, bool)>;

/// Widget name of the Connection Profiles window, how [`present`] finds an open one.
const WINDOW_NAME: &str = "moniker-profiles";

/// Widget name of the sidebar list, how [`select`] finds it.
pub(crate) const SIDEBAR_NAME: &str = "profiles-sidebar";

/// Widget name of the sidebar's Sync entry (a list of one row), how
/// [`present_sync`] finds it.
pub(crate) const SYNC_ENTRY_NAME: &str = "profiles-sync-entry";

/// Widget name of the content pane's title label (tests look for it).
pub(crate) const TITLE_NAME: &str = "profile-title";

/// Widget name of the content pane's navigation view: the profile page and,
/// pushed over it, the startup-sequence sub-page (tests look for it).
pub(crate) const NAV_NAME: &str = "profile-nav";

/// Tag of the profile page in that navigation view.
const PROFILE_TAG: &str = "profile";

/// Tag of the startup-sequence sub-page.
pub(crate) const SEQUENCE_TAG: &str = "sequence";

/// Show the Connection Profiles window of `app`: the open one if there is one (it keeps
/// the [`ConnectFn`] it was opened with), else a new one.
pub fn present(app: &adw::Application, ctx: &Rc<AppContext>, connect: ConnectFn) -> adw::Window {
    let window = open_window(app).unwrap_or_else(|| build(app, ctx, connect));
    window.present();
    window
}

/// Show the Connection Profiles window of `app` (as [`present`] does) on its sync page.
pub fn present_sync(
    app: &adw::Application,
    ctx: &Rc<AppContext>,
    connect: ConnectFn,
) -> adw::Window {
    let window = present(app, ctx, connect);
    if let Some(list) = named::<gtk::ListBox>(&window, SYNC_ENTRY_NAME)
        && let Some(row) = list.row_at_index(0)
    {
        list.emit_by_name::<()>("row-activated", &[&row]);
    }
    window
}

/// Close the Connection Profiles window of `app`, if it is open, as its close button
/// does (a typed password is cleared, a settling port saved).
pub fn close(app: &adw::Application) {
    if let Some(window) = open_window(app) {
        window.close();
    }
}

/// The open Connection Profiles window of `app`, if there is one.
fn open_window(app: &adw::Application) -> Option<adw::Window> {
    app.windows()
        .into_iter()
        .find(|window| window.widget_name() == WINDOW_NAME)
        .and_downcast::<adw::Window>()
}

/// Select the profile `id` in the sidebar of the Profiles `window`, which
/// shows its page. Nothing happens if it is not listed.
pub(crate) fn select(window: &adw::Window, id: &str) {
    let Some(list) = named::<gtk::ListBox>(window, SIDEBAR_NAME) else {
        return;
    };
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if row.widget_name() == id {
            list.select_row(Some(&row));
            return;
        }
        index += 1;
    }
}

/// The widget of type `T` called `name` in `window`.
fn named<T: IsA<gtk::Widget>>(window: &adw::Window, name: &str) -> Option<T> {
    let mut stack = vec![window.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == name {
            return widget.downcast().ok();
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    None
}

/// The window's state. Owned by the window (see [`build`]); every handler
/// holds it weakly.
struct State {
    ctx: Rc<AppContext>,
    connect: ConnectFn,
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    /// The sidebar's Sync entry.
    sync_list: gtk::ListBox,
    content: adw::NavigationPage,
    /// The content pane's pages: the profile page, and over it the sub-page.
    nav: adw::NavigationView,
    body: adw::ToolbarView,
    title: gtk::Label,
    connect_button: gtk::Button,
    /// The listed profiles by id, for the search filter.
    listed: RefCell<HashMap<String, Profile>>,
    /// The profile whose page is shown.
    selected: RefCell<Option<String>>,
    /// Shared so a handler can use it without keeping the cell borrowed: what
    /// it calls may refresh the window, which replaces the page.
    page: RefCell<Option<Rc<ProfilePage>>>,
    /// The sync page, while it is shown (instead of a profile's page).
    sync_page: RefCell<Option<Rc<SyncPage>>>,
    /// The sidebar is being rebuilt: its selection signals are not the person's.
    rebuilding: Cell<bool>,
}

fn build(app: &adw::Application, ctx: &Rc<AppContext>, connect: ConnectFn) -> adw::Window {
    // Sidebar: header with ＋ and search, then the list.
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add Profile")
        .build();
    let search_toggle = gtk::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text("Search")
        .build();
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.pack_start(&add);
    sidebar_header.pack_end(&search_toggle);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search profiles")
        .build();
    let search_bar = gtk::SearchBar::builder().child(&search).build();
    search_bar.connect_entry(&search);
    search_toggle
        .bind_property("active", &search_bar, "search-mode-enabled")
        .bidirectional()
        .build();
    // Single click selects (and shows the page); double click or Enter connects.
    let list = gtk::ListBox::builder()
        .name(SIDEBAR_NAME)
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(false)
        .css_classes(["navigation-sidebar"])
        .build();
    list.set_placeholder(Some(
        &gtk::Label::builder()
            .label("No profiles")
            .css_classes(["dim-label"])
            .margin_top(24)
            .margin_bottom(24)
            .build(),
    ));
    // The Sync entry, at the bottom of the sidebar, below the scrolling list.
    let sync_list = gtk::ListBox::builder()
        .name(SYNC_ENTRY_NAME)
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["navigation-sidebar"])
        .build();
    let sync_row = adw::ActionRow::builder()
        .title("Sync")
        .use_markup(false)
        .activatable(true)
        .build();
    sync_row.add_prefix(&gtk::Image::from_icon_name("emblem-synchronizing-symbolic"));
    sync_list.append(&sync_row);
    let sidebar_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_column.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build(),
    );
    sidebar_column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_column.append(&sync_list);
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.add_top_bar(&search_bar);
    sidebar_view.set_content(Some(&sidebar_column));
    let sidebar = adw::NavigationPage::builder()
        .title("Connection Profiles")
        .child(&sidebar_view)
        .build();

    // Content: header with the profile's name and Connect, then the page.
    let title = gtk::Label::builder()
        .use_markup(false)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["title"])
        .build();
    title.set_widget_name(TITLE_NAME);
    let connect_button = gtk::Button::builder()
        .label("Connect")
        .css_classes(["suggested-action"])
        .build();
    let content_header = adw::HeaderBar::builder().title_widget(&title).build();
    content_header.pack_end(&connect_button);
    let body = adw::ToolbarView::new();
    body.add_top_bar(&content_header);
    // The split view's content is one page; a navigation view inside it lets
    // the startup-sequence sub-page be pushed over the profile page.
    let nav = adw::NavigationView::builder().name(NAV_NAME).build();
    nav.add(
        &adw::NavigationPage::builder()
            .title("Profile")
            .tag(PROFILE_TAG)
            .child(&body)
            .build(),
    );
    let content = adw::NavigationPage::builder()
        .title("Profile")
        .child(&nav)
        .build();

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(220.0)
        .build();
    // Over the whole window, header bars included: the messages go below them.
    let notifications = NotificationArea::new(&split);
    notifications.below_header_bar();
    ctx.notice.attach(&notifications);

    let window = adw::Window::builder()
        .application(app)
        .name(WINDOW_NAME)
        .title("Connection Profiles")
        .width_request(360)
        .height_request(400)
        .content(&notifications)
        .build();
    // At the size it had when it last closed (issue #14).
    window_size::remember(&window, Kind::Profiles, &ctx.settings_path, (900, 640));
    // Narrow: one pane at a time with back navigation, as Settings does.
    let narrow = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 600sp").expect("a valid breakpoint condition"),
    );
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);
    search_bar.set_key_capture_widget(Some(&sidebar_view));

    let state = Rc::new(State {
        ctx: Rc::clone(ctx),
        connect,
        split,
        list,
        search,
        sync_list,
        content,
        nav,
        body,
        title,
        connect_button,
        listed: RefCell::new(HashMap::new()),
        selected: RefCell::new(None),
        page: RefCell::new(None),
        sync_page: RefCell::new(None),
        rebuilding: Cell::new(false),
    });
    state.wire(&window, &add);
    state.rebuild_sidebar();
    state.show_empty();
    window
}

impl State {
    /// Connect the window's signals. Handlers hold the state weakly; the one
    /// strong reference lives in the window's destroy handler, so the state
    /// goes when the window does.
    fn wire(self: &Rc<Self>, window: &adw::Window, add: &gtk::Button) {
        let weak = Rc::downgrade(self);

        self.list.set_filter_func(glib::clone!(
            #[strong]
            weak,
            move |row| {
                let Some(state) = weak.upgrade() else {
                    return true;
                };
                let query = state.search.text();
                state
                    .listed
                    .borrow()
                    .get(row.widget_name().as_str())
                    .is_none_or(|profile| form::matches_query(profile, &query))
            }
        ));
        self.search.connect_search_changed(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(state) = weak.upgrade() {
                    state.list.invalidate_filter();
                }
            }
        ));

        self.list.connect_row_selected(glib::clone!(
            #[strong]
            weak,
            move |_, row| {
                let (Some(state), Some(row)) = (weak.upgrade(), row) else {
                    return;
                };
                if !state.rebuilding.get() {
                    state.show_profile(row.widget_name().as_str());
                }
            }
        ));
        self.sync_list.connect_row_activated(glib::clone!(
            #[strong]
            weak,
            move |_, _| {
                if let Some(state) = weak.upgrade() {
                    state.show_sync();
                }
            }
        ));
        self.list.connect_row_activated(glib::clone!(
            #[strong]
            weak,
            move |_, row| {
                if let Some(state) = weak.upgrade() {
                    state.connect_to(row.widget_name().as_str());
                }
            }
        ));
        self.connect_button.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let selected = state.selected.borrow().clone();
                if let Some(id) = selected {
                    state.connect_to(&id);
                }
            }
        ));

        add.connect_clicked(glib::clone!(
            #[strong]
            weak,
            #[weak]
            window,
            move |_| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let window_for_select = window.downgrade();
                new_profile::show(&window, &state.ctx, move |id| {
                    if let Some(window) = window_for_select.upgrade() {
                        select(&window, id);
                    }
                });
            }
        ));

        // Leaving a row with an unapplied edit discards it. The window's focus
        // widget, not the rows' own focus-out, decides: switching to another
        // application does not move it, so it does not discard anything.
        window.connect_focus_widget_notify(glib::clone!(
            #[strong]
            weak,
            move |window| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                if let Some(page) = state.current_page() {
                    page.focus_moved(RootExt::focus(window).as_ref());
                }
            }
        ));
        // Never leave a typed password or sync key in a closed window; a port
        // change that is still settling is saved, not lost.
        window.connect_close_request(glib::clone!(
            #[strong]
            weak,
            move |_| {
                let Some(state) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if let Some(page) = state.current_page() {
                    page.flush();
                    page.clear_secret();
                }
                if let Some(sync_page) = state.sync_page.borrow().as_ref() {
                    sync_page.clear_secret();
                }
                glib::Propagation::Proceed
            }
        ));

        self.ctx.on_profiles_changed(window, move || {
            if let Some(state) = weak.upgrade() {
                state.refresh();
            }
        });

        let keep = Rc::clone(self);
        window.connect_destroy(move |_| {
            // Dropping the pages here empties their password and key rows.
            keep.page.replace(None);
            keep.sync_page.replace(None);
        });
    }

    fn current_page(&self) -> Option<Rc<ProfilePage>> {
        self.page.borrow().clone()
    }

    /// Connect to the live profile `id` the way the window was told to.
    fn connect_to(&self, id: &str) {
        // Bind first so the store borrow ends before connecting.
        let profile = self.ctx.store.borrow().get(id).cloned();
        if let Some(profile) = profile {
            // "Save transcripts of all sessions" covers this way in too.
            let save = default_save(profile.transcript, self.ctx.settings().save_all_transcripts);
            (self.connect)(&profile, None, save);
        }
    }

    /// Show the page of profile `id` (a new page, so any unapplied edit on the
    /// previous one is dropped).
    fn show_profile(&self, id: &str) {
        if self.selected.borrow().as_deref() == Some(id) {
            self.split.set_show_content(true);
            return;
        }
        // Bind first so the store borrow ends here.
        let profile = self.ctx.store.borrow().get(id).cloned();
        let Some(profile) = profile else {
            self.show_empty();
            return;
        };
        self.leave_sync();
        // A sub-page belongs to the profile it was opened on: it goes, with
        // whatever was typed in it, like any unapplied edit.
        self.nav.pop_to_tag(PROFILE_TAG);
        let page = ProfilePage::new(&self.ctx, &profile);
        self.body.set_content(Some(&page.widget));
        // Replacing the old page drops it, which saves a port change still
        // settling there and empties its password row.
        self.page.replace(Some(Rc::new(page)));
        self.selected.replace(Some(profile.id.clone()));
        self.show_title(&profile.name);
        self.connect_button.set_visible(true);
        self.split.set_show_content(true);
    }

    /// Show the sync page (the sidebar's Sync entry), in place of a profile's
    /// page. An unapplied edit on that page is dropped, as when another
    /// profile is chosen.
    fn show_sync(&self) {
        if self.sync_page.borrow().is_some() {
            self.split.set_show_content(true);
            return;
        }
        self.nav.pop_to_tag(PROFILE_TAG);
        let sync_page = SyncPage::new(&self.ctx);
        self.body.set_content(Some(&sync_page.widget));
        // Replacing the profile page drops it, which saves a port change
        // still settling there and empties its password row.
        self.page.replace(None);
        self.selected.replace(None);
        self.sync_page.replace(Some(sync_page));
        self.show_title("Sync");
        self.connect_button.set_visible(false);
        self.rebuilding.set(true);
        self.list.unselect_all();
        self.rebuilding.set(false);
        if let Some(row) = self.sync_list.row_at_index(0) {
            self.sync_list.select_row(Some(&row));
        }
        self.split.set_show_content(true);
    }

    /// The sync page goes (with a key typed into it), if it is shown.
    fn leave_sync(&self) {
        self.sync_page.replace(None);
        self.sync_list.unselect_all();
    }

    /// Nothing selected: a status page instead of a profile.
    fn show_empty(&self) {
        self.leave_sync();
        self.page.replace(None);
        self.selected.replace(None);
        self.body.set_content(Some(
            &adw::StatusPage::builder()
                .icon_name("network-server-symbolic")
                .title("No profile selected")
                .description("Choose a profile in the list, or add one with +.")
                .build(),
        ));
        self.show_title("");
        self.connect_button.set_visible(false);
        self.rebuilding.set(true);
        self.list.unselect_all();
        self.rebuilding.set(false);
        self.split.set_show_content(false);
    }

    fn show_title(&self, name: &str) {
        self.title.set_label(name);
        self.content
            .set_title(if name.is_empty() { "Profile" } else { name });
    }

    /// The profile file changed (the store is already reloaded): rebuild the
    /// sidebar and update the page, or leave an empty state if the selected
    /// profile is gone.
    fn refresh(&self) {
        self.rebuild_sidebar();
        let selected = self.selected.borrow().clone();
        let Some(id) = selected else {
            return;
        };
        // Bind first so the store borrow ends here.
        let profile = self.ctx.store.borrow().get(&id).cloned();
        match profile {
            Some(profile) => {
                if let Some(page) = self.current_page() {
                    page.refresh(&profile);
                }
                self.show_title(&profile.name);
            }
            None => self.show_empty(),
        }
    }

    /// One row per live profile, in the store's order, keeping the selection
    /// while that profile is listed.
    fn rebuild_sidebar(&self) {
        let profiles: Vec<Profile> = self
            .ctx
            .store
            .borrow()
            .live()
            .into_iter()
            .cloned()
            .collect();
        let selected = self.selected.borrow().clone();
        // GTK moves focus on from a focused row that is removed, and the list
        // selects whatever row gets focus: a deleted selection would silently
        // become its neighbor. So focus leaves the list while it is rebuilt
        // and comes back only to the row that stays selected.
        let root = self.list.root();
        let focus_in_list = root.as_ref().and_then(RootExt::focus).is_some_and(|f| {
            f == *self.list.upcast_ref::<gtk::Widget>() || f.is_ancestor(&self.list)
        });
        if focus_in_list && let Some(root) = &root {
            root.set_focus(None::<&gtk::Widget>);
        }
        self.rebuilding.set(true);
        // Not `remove_all`: that would take the placeholder with it.
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let mut listed = HashMap::new();
        let mut reselected = None;
        for profile in profiles {
            let row = adw::ActionRow::builder()
                .name(profile.id.as_str())
                .title(profile.name.as_str())
                .subtitle(form::subtitle(&profile))
                .use_markup(false)
                .title_lines(1)
                .subtitle_lines(1)
                .activatable(true)
                .build();
            // Whether the host answers a ping (`crate::hosts`), before the name.
            row.add_prefix(&self.ctx.hosts.mark(&profile));
            self.list.append(&row);
            if selected.as_deref() == Some(profile.id.as_str()) {
                self.list.select_row(Some(&row));
                reselected = Some(row);
            }
            listed.insert(profile.id.clone(), profile);
        }
        self.listed.replace(listed);
        if focus_in_list && let Some(row) = reselected {
            row.grab_focus();
        }
        self.rebuilding.set(false);
        self.list.invalidate_filter();
    }
}
