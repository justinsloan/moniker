//! The new-tab list: what a tab opened with ＋ (or Ctrl+Shift+T) shows until
//! the person picks what it becomes ([`open`]), and what a new pane shows, made
//! by splitting one (Ctrl+Shift+R or D, [`fill`]). A pane of a saved session
//! whose profile has gone shows it too, with a note above the search entry
//! ([`fill_with_note`], plain text). The same list, in a small window over a
//! pane that holds a session, replaces that session ([`choose_new_session`],
//! issue #20).
//!
//! The tab, called "New Tab" (a new pane: "New Pane"), holds a search entry
//! over a list. The first row
//! is always "Local terminal"; below it come the live profiles in the store's
//! order, each with its name and `user@host:port` (and `via` the jump host it
//! goes through, [`form::list_subtitle`]; its host mark is the dimmed dash,
//! never pinged, [`crate::hosts::Hosts::mark_for`]) and, for a profile kept on
//! this machine only, a mark before its "›" ([`local_only::mark`]). "Local
//! terminal" starts out selected and with the focus, so Enter gives a local shell
//! at once.
//!
//! The tab starts at the text size of the tab that was selected when ＋ was
//! pressed ([`terminal_tab::new_holder_page`]), a new pane at the size of the
//! pane it was split from, and the session picked keeps it.
//!
//! Picking a row turns that same tab (or pane) into the session: Local terminal starts
//! the person's shell in the home folder after running its default sequence, if
//! it has one (issue #13, [`terminal_tab::start_local_in`]), a profile connects
//! and runs its default sequence, if it has one (issue #12). Local terminal's
//! sequences and default are read from their file at the pick
//! (`crate::local_terminal`), so a change made in the Connection Profiles
//! window shows without rebuilding the list; a profile's are the list's copy.
//! Whether the session saves a transcript is decided here, before the tab
//! starts: with "Save transcripts of all sessions" on, every session does;
//! without it, a profile follows its own setting and Local terminal its own
//! switch ([`terminal_tab::default_save`]). A row's "›" (or the Right arrow on
//! it) opens its choices, where that decision can be made for this one session:
//! a profile's ([`connect_menu::choices`]) are "Just connect" (which skips the
//! default sequence), each startup sequence and the "Save transcript" box, Local
//! terminal's ([`connect_menu::local_choices`]) "Just open" (which skips its
//! default) and each of its sequences, or "Open" while it has none, and the
//! box. The box shows the setting as it is when the choices open, and Local
//! terminal's choices are built again from its file every time they open, each
//! carrying the sequence it shows.
//! The search matches a profile's name,
//! host or user name; Local terminal stays the top entry whatever the query.
//! While searching, the first profile the search leaves is selected and Enter in
//! the search entry picks it (Local terminal if no profile matches); typing
//! anywhere in the list goes to the search.
//!
//! The list fills its pane once: it only ever puts a session into the fresh
//! pane it was made for, never into one that already ran one (whose unwritten
//! transcript a new session would lose). Once it has, the list is gone, and a
//! stale event from it starts nothing. Closing the tab or pane before picking
//! just closes it: there is no session, so nothing is written.
//!
//! In the New Session window the list is not in the pane it acts on, so it is
//! the window that makes it act once: the first pick closes the window, ends
//! the pane's session as closing the pane would and makes the pane fresh
//! ([`terminal_tab::clear_for_new_session`]), and only then starts the session
//! picked there. The window closes when the pane goes (its session closed it,
//! its tab or window was closed), changing nothing. A pick after the window
//! closed starts nothing anywhere; closing the window (Escape included, also in
//! its search) changes nothing, and gives the keyboard back to the pane.
//!
//! While it is still a list it follows the profiles
//! ([`AppContext::on_profiles_changed`]), keeping the search, the selected row
//! (while it exists) and the focus. Profile names, hosts and user names come
//! from other machines through sync: rows have Pango markup off.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::jump::{self, Route};
use moniker_core::profile::{Profile, StartupSequence};

use crate::context::AppContext;
use crate::terminal_tab::PaneHolder;
use crate::{connect_menu, form, local_only, terminal_tab};

/// Title of a tab that shows the new-tab list.
pub(crate) const TITLE: &str = "New Tab";

/// The first row, a local shell.
pub(crate) const LOCAL_LABEL: &str = "Local terminal";

/// Widget name of the Local terminal row. A profile row is named after its
/// profile's id, a UUID, so the two cannot be confused.
pub(crate) const LOCAL_ROW: &str = "new-tab-local";

/// Widget name of the list (tests look for it).
pub(crate) const LIST_NAME: &str = "new-tab-list";

/// Title of the window that replaces a pane's session ([`choose_new_session`]).
pub(crate) const NEW_SESSION_TITLE: &str = "New Session";

/// The size of the New Session window's content: a small window over the pane.
const NEW_SESSION_WIDTH: i32 = 420;
const NEW_SESSION_HEIGHT: i32 = 520;

/// Open a new tab in `view` that shows the new-tab list, select it, and return
/// its page. The list fills this page with whatever is picked.
pub fn open(view: &adw::TabView, ctx: &Rc<AppContext>) -> adw::TabPage {
    let page = terminal_tab::new_holder_page(view, TITLE);
    if let Some(holder) = terminal_tab::holder_of(&page) {
        fill(&holder, ctx);
    }
    page
}

/// Show the new-tab list in `holder`, a fresh pane (a new tab's, or one made by
/// splitting), which it fills with whatever is picked.
pub fn fill(holder: &PaneHolder, ctx: &Rc<AppContext>) {
    let (chooser, _) = build(Target::Fill, holder, ctx, None);
    terminal_tab::show_chooser(holder, &chooser);
}

/// [`fill`], with `note` above the search entry: a short line of plain text
/// (it may quote a name), wrapped if the pane is narrow. A restored pane whose
/// profile is gone says so this way (`crate::session_layout`).
pub fn fill_with_note(holder: &PaneHolder, ctx: &Rc<AppContext>, note: &str) {
    let (chooser, _) = build(Target::Fill, holder, ctx, Some(note));
    terminal_tab::show_chooser(holder, &chooser);
}

/// Offer the new-tab list in a small window titled "New Session" over
/// `holder`'s pane, which holds a session ([`terminal_tab::holds_session`]),
/// to replace that session (issue #20): the same rows, "›" choices, search and
/// following of the profiles as [`fill`]'s, with Local terminal selected and
/// the keyboard on it. The old session stays shown and runs behind the window.
///
/// A pick closes the window, ends the old session exactly as closing the pane
/// would and makes the pane fresh ([`terminal_tab::clear_for_new_session`]:
/// nothing is asked), then starts what was picked in the same pane, as a pick
/// in the new-tab list does, with that pick's transcript decision; the pane
/// keeps its place and its text size and takes the new session's own title.
/// The window closes when the pane goes (its session ended and closed it, its
/// tab or window was closed: the paths that end the pane's session close it,
/// [`PaneHolder::new_session`]), changing nothing, and a pick in it afterwards
/// starts nothing. Closing the window any other way (its close button, Escape,
/// Escape in its search) changes nothing as well, and the keyboard goes back to
/// the pane if it may have it (`crate::panes::in_front`), or to the pane in
/// front if the pane went.
pub(crate) fn choose_new_session(holder: &PaneHolder, ctx: &Rc<AppContext>) {
    let dialog = adw::Dialog::builder()
        .title(NEW_SESSION_TITLE)
        .content_width(NEW_SESSION_WIDTH)
        .content_height(NEW_SESSION_HEIGHT)
        .build();
    let over = Rc::new(Cell::new(false));
    let target = Target::Replace {
        dialog: dialog.downgrade(),
        over: Rc::clone(&over),
    };
    let (content, list) = build(target, holder, ctx, None);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&content));
    dialog.set_child(Some(&view));
    // Local terminal, selected, takes the keyboard when the window shows, so
    // Enter picks it, typing searches and Escape closes the window.
    if let Some(row) = list.selected_row() {
        dialog.set_focus(Some(&row));
    }
    // The pane knows its window, so that the paths that end the pane's session
    // (its close button, its shell exiting, its tab or window closing) can
    // close the window: with the pane gone there is nothing left to replace.
    holder.new_session().replace(Some(dialog.downgrade()));
    // Closed without a pick (or because the pane went): nothing changed, and
    // the keyboard goes back to the pane, or to the pane that took its room if
    // it went. From an idle callback, after libadwaita has given it back to
    // the widget that had it before the window opened (a header's menu
    // button, say), as the Layouts menu's questions do.
    let pane = holder.downgrade();
    let view = crate::panes::layout_of(holder)
        .as_ref()
        .and_then(crate::panes::page_of)
        .map(|(view, _)| view.downgrade());
    dialog.connect_closed(move |_| {
        if let Some(holder) = pane.upgrade() {
            holder.new_session().take();
        }
        if over.replace(true) {
            return;
        }
        let pane = pane.clone();
        let view = view.clone();
        glib::idle_add_local_once(move || {
            let heir = || {
                let view = view.as_ref()?.upgrade()?;
                terminal_tab::selected_holder(&view)
            };
            if let Some(holder) = pane.upgrade().filter(in_a_tab).or_else(heir)
                && crate::panes::in_front(&holder)
            {
                crate::panes::focus_pane(&holder);
            }
        });
    });
    dialog.present(Some(holder));
}

/// Whether the pane of `holder` is still a pane of a tab in a tab view: not
/// one taken out of its tab (closed in a split), nor one of a tab that was
/// closed.
fn in_a_tab(holder: &PaneHolder) -> bool {
    crate::panes::layout_of(holder)
        .as_ref()
        .and_then(crate::panes::page_of)
        .is_some()
}

/// Which pane a pick in the list starts its session in.
enum Target {
    /// The fresh pane the list is shown in ([`fill`]), while it still shows
    /// the list.
    Fill,
    /// The pane, holding a session, that the New Session window `dialog` is
    /// over ([`choose_new_session`]): one pick only, and none once the window
    /// has closed (`over`).
    Replace {
        dialog: glib::WeakRef<adw::Dialog>,
        over: Rc<Cell<bool>>,
    },
}

/// What the list's handlers share. It holds no widget, so the handlers that
/// keep it (and the list keeps them) make no reference cycle.
struct Chooser {
    ctx: Rc<AppContext>,
    /// The pane to fill, or whose session to replace. Weak: the pane owns the
    /// list (or the window over it does).
    holder: glib::WeakRef<PaneHolder>,
    /// Where a pick goes.
    target: Target,
    /// The profiles listed, as the store had them at the last (re)build.
    profiles: RefCell<Vec<Profile>>,
}

impl Chooser {
    /// Claim the pane a pick in `list` starts its session in, ready for it, or
    /// none. Claiming consumes the pick and, in the New Session window, has
    /// effects: it closes the window and ends the pane's session.
    ///
    /// For the list in its own pane: that pane, while `list` is still what it
    /// shows; once a session has replaced the list (or the pane is gone) there
    /// is nothing to fill. In the New Session window: the first pick only,
    /// while the window is open (it is not, once the pane went: the paths that
    /// end the pane close the window); it closes the window and ends the pane's
    /// session, making the pane fresh
    /// ([`terminal_tab::clear_for_new_session`]). The pane is still checked to
    /// be in a tab, as a last guard.
    fn claim_pane(&self, list: &gtk::ListBox) -> Option<PaneHolder> {
        match &self.target {
            Target::Fill => self
                .holder
                .upgrade()
                .filter(|holder| list.is_ancestor(holder)),
            Target::Replace { dialog, over } => {
                if over.replace(true) {
                    return None;
                }
                let holder = self.holder.upgrade();
                // The pane's session ends next: it must not close the window
                // that is already closing.
                if let Some(holder) = &holder {
                    holder.new_session().take();
                }
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
                let holder = holder.filter(in_a_tab)?;
                terminal_tab::clear_for_new_session(&holder);
                Some(holder)
            }
        }
    }

    /// The listed profile `id`, cloned so no borrow is held while connecting.
    fn profile(&self, id: &str) -> Option<Profile> {
        self.profiles.borrow().iter().find(|p| p.id == id).cloned()
    }

    /// Whether `row` belongs in the list filtered by `query`.
    /// Local terminal always does: it is the list's top entry whatever the
    /// search.
    fn matches(&self, row: &gtk::ListBoxRow, query: &str) -> bool {
        let name = row.widget_name();
        if name == LOCAL_ROW {
            return true;
        }
        self.profiles
            .borrow()
            .iter()
            .find(|p| p.id == name.as_str())
            .is_none_or(|p| form::matches_query(p, query))
    }

    /// Whether "Save transcripts of all sessions" is on, as the settings file has
    /// it now: asked when a session starts or the choices open, not when the
    /// list was made, since the setting can change while the list is up.
    fn save_all(&self) -> bool {
        self.ctx.settings().save_all_transcripts
    }

    /// Turn the pane into what `row` stands for: a local shell, which runs
    /// Local terminal's default sequence first if it has one, or the profile's
    /// session, which runs the profile's default sequence if it has one (the
    /// choices behind "›" are where "Just open" and "Just connect" skip them).
    /// A profile's sequence is the one the list shows: the profile is the
    /// list's copy, so its default and its sequences are from the same moment.
    /// Local terminal's are read from its file now, with its own switch, in
    /// one read. A transcript is saved as [`terminal_tab::default_save`] says.
    fn pick(&self, list: &gtk::ListBox, row: &gtk::ListBoxRow) {
        let name = row.widget_name();
        if name == LOCAL_ROW {
            let local = self.ctx.local_terminal();
            let save = terminal_tab::default_local_save(&local, self.save_all());
            self.open_local(list, local.default_sequence(), save);
        } else if let Some(profile) = self.profile(&name) {
            let save = terminal_tab::default_save(profile.transcript, self.save_all());
            self.connect(list, &profile, profile.startup_choice(), save);
        }
    }

    /// Turn the pane into a local shell in the home folder, running
    /// `sequence` first if one is given, and saving its transcript if `save`.
    fn open_local(&self, list: &gtk::ListBox, sequence: Option<&StartupSequence>, save: bool) {
        if let Some(holder) = self.claim_pane(list) {
            let transcript = terminal_tab::local_transcript_spec(&self.ctx, save);
            terminal_tab::start_local_in(&holder, &glib::home_dir(), sequence, transcript);
        }
    }

    /// Turn the pane into a session for `profile`, saving its transcript if
    /// `record`.
    fn connect(
        &self,
        list: &gtk::ListBox,
        profile: &Profile,
        sequence: Option<usize>,
        record: bool,
    ) {
        if let Some(holder) = self.claim_pane(list) {
            terminal_tab::start_profile(&holder, &self.ctx, profile, sequence, record);
        }
    }
}

/// The live profiles, in the store's order.
fn live_profiles(ctx: &AppContext) -> Vec<Profile> {
    ctx.store.borrow().live().into_iter().cloned().collect()
}

/// The search entry over the list, in a scrolled column, for `holder`'s pane,
/// with `note` above them if there is one; and the list. A pick goes where
/// `target` says.
fn build(
    target: Target,
    holder: &PaneHolder,
    ctx: &Rc<AppContext>,
    note: Option<&str>,
) -> (gtk::Widget, gtk::ListBox) {
    let chooser = Rc::new(Chooser {
        ctx: Rc::clone(ctx),
        holder: holder.downgrade(),
        target,
        profiles: RefCell::new(live_profiles(ctx)),
    });
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search profiles")
        .build();
    let list = gtk::ListBox::builder()
        .name(LIST_NAME)
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["boxed-list"])
        .build();
    list.set_placeholder(Some(
        &gtk::Label::builder()
            .label("No matches")
            .css_classes(["dim-label"])
            .margin_top(12)
            .margin_bottom(12)
            .build(),
    ));
    let local = local_row(&chooser, &list);
    list.append(&local);
    append_profile_rows(&chooser, &list);
    list.select_row(Some(&local));

    list.set_filter_func(glib::clone!(
        #[weak]
        search,
        #[strong]
        chooser,
        #[upgrade_or]
        true,
        move |row| chooser.matches(row, &search.text())
    ));
    // The selection follows the search to the first row it leaves, which is the
    // one Enter in the search entry picks.
    search.connect_search_changed(glib::clone!(
        #[weak]
        list,
        move |search| {
            follow_search(&list, &search.text());
        }
    ));
    // The entry reports a change only after a short delay (`search-changed`),
    // but Enter arrives at once: follow the text as it is now before picking,
    // or a fast "zeta" + Enter would pick from the unfiltered list.
    search.connect_activate(glib::clone!(
        #[weak]
        list,
        #[strong]
        chooser,
        move |search| {
            if let Some(row) = follow_search(&list, &search.text()) {
                chooser.pick(&list, &row);
            }
        }
    ));
    list.connect_row_activated(glib::clone!(
        #[strong]
        chooser,
        move |list, row| chooser.pick(list, row)
    ));
    // Right arrow on a row (a profile's or Local terminal's) opens its
    // choices, as its "›" does.
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        list,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if !matches!(key, gdk::Key::Right | gdk::Key::KP_Right) {
                return glib::Propagation::Proceed;
            }
            let more = list
                .focus_child()
                .and_then(|row| descendant::<gtk::MenuButton>(&row));
            match more {
                Some(more) => {
                    more.popup();
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        }
    ));
    list.add_controller(keys);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    if let Some(note) = note {
        column.append(
            &gtk::Label::builder()
                .label(note)
                .use_markup(false)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .build(),
        );
    }
    column.append(&search);
    column.append(&list);
    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .child(&column)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .build();
    // Typing while a row has the focus searches.
    search.set_key_capture_widget(Some(&scroller));
    // Escape in the search entry stops the search, which takes the key before
    // the New Session window sees it: there it closes the window too.
    if let Target::Replace { dialog, .. } = &chooser.target {
        let dialog = dialog.clone();
        search.connect_stop_search(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        });
    }

    // The focus goes to the selected row when the list shows, unless it is
    // already somewhere in it (coming back to the tab). After the tab view has
    // moved the focus into the page, hence the idle. Only if its pane may take
    // the keyboard (`crate::panes::in_front`): the focused pane of the tab its
    // window shows, with no dialog open. A list in another pane of a split tab
    // must not take it from the pane in use, nor one shown again as panes
    // move from a tab that is no longer in sight. (In the New Session window
    // the window gives the keyboard to the selected row: `choose_new_session`.)
    if matches!(chooser.target, Target::Fill) {
        scroller.connect_map(glib::clone!(
            #[weak]
            list,
            #[strong]
            chooser,
            move |scroller| {
                let scroller = scroller.downgrade();
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    list,
                    #[strong]
                    chooser,
                    move || {
                        let Some(scroller) = scroller.upgrade() else {
                            return;
                        };
                        let in_front = chooser
                            .holder
                            .upgrade()
                            .is_some_and(|holder| crate::panes::in_front(&holder));
                        if scroller.is_mapped()
                            && in_front
                            && !focus_within(&scroller)
                            && let Some(row) = list.selected_row()
                        {
                            row.grab_focus();
                        }
                    }
                ));
            }
        ));
    }

    // A change to the profiles shows while this is a list. Weak: the context
    // outlives the list, and this closure lives in the context's list of
    // listeners until the scroller is gone.
    let weak = Rc::downgrade(&chooser);
    let weak_list = list.downgrade();
    ctx.on_profiles_changed(&scroller, move || {
        if let (Some(chooser), Some(list)) = (weak.upgrade(), weak_list.upgrade()) {
            refresh(&chooser, &list);
        }
    });
    (scroller.upcast(), list)
}

/// Rebuild the profile rows from the store, keeping the selected row (Local
/// terminal if its profile went) and the focus if it was in the list.
fn refresh(chooser: &Rc<Chooser>, list: &gtk::ListBox) {
    let selected = list.selected_row().map(|row| row.widget_name());
    let had_focus = focus_within(list);
    chooser.profiles.replace(live_profiles(&chooser.ctx));
    while let Some(row) = list.row_at_index(1) {
        list.remove(&row);
    }
    append_profile_rows(chooser, list);
    list.invalidate_filter();
    let row = selected
        .and_then(|name| row_named(list, &name))
        .or_else(|| list.row_at_index(0));
    list.select_row(row.as_ref());
    if had_focus && let Some(row) = row {
        row.grab_focus();
    }
}

/// A row for each listed profile, after Local terminal.
fn append_profile_rows(chooser: &Rc<Chooser>, list: &gtk::ListBox) {
    let profiles = chooser.profiles.borrow().clone();
    for profile in &profiles {
        // Where its connection goes (issue #9): the line under the name and
        // the mark say so.
        let route = jump::route(profile, |id| profiles.iter().find(|p| p.id == id));
        list.append(&profile_row(chooser, list, profile, &route));
    }
}

/// The "›" button of a row: it opens `popover`, which holds the row's choices.
/// (The Layouts menu's rows have one too.)
pub(crate) fn more_button(tooltip: &str, popover: &gtk::Popover) -> gtk::MenuButton {
    gtk::MenuButton::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .popover(popover)
        .build()
}

/// Check or uncheck the "Save transcript" box of `popover`'s choices to `save()`
/// each time the popover opens, so it shows the setting as it is then and not
/// as it was when the list was made.
fn refresh_box_on_show(popover: &gtk::Popover, save: impl Fn() -> bool + 'static) {
    popover.connect_show(move |popover| {
        if let Some(record) = popover
            .child()
            .and_then(|choices| descendant::<gtk::CheckButton>(&choices))
        {
            record.set_active(save());
        }
    });
}

/// The Local terminal row, with a "›" whose popover holds its choices
/// ([`connect_menu::local_choices`]). They are built again from the settings
/// file every time the popover opens ([`local_choices_now`]), so they offer
/// Local terminal's sequences and its "Save transcript" box as the file has
/// them then, not as when the list was made.
fn local_row(chooser: &Rc<Chooser>, list: &gtk::ListBox) -> adw::ActionRow {
    let popover = gtk::Popover::new();
    popover.connect_show(glib::clone!(
        #[weak]
        list,
        #[strong]
        chooser,
        move |popover| popover.set_child(Some(&local_choices_now(&chooser, &list, popover)))
    ));
    let row = adw::ActionRow::builder()
        .name(LOCAL_ROW)
        .title(LOCAL_LABEL)
        .use_markup(false)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("utilities-terminal-symbolic"));
    row.add_suffix(&more_button("Terminal Options", &popover));
    row
}

/// Local terminal's choices as its file has them now, for `popover`: its
/// sequences, and the "Save transcript" box checked when its own switch or
/// "Save transcripts of all sessions" is on. A choice closes the popover and
/// turns the list's pane into a local shell running the sequence that choice
/// showed (none for "Open" and "Just open").
fn local_choices_now(
    chooser: &Rc<Chooser>,
    list: &gtk::ListBox,
    popover: &gtk::Popover,
) -> gtk::Widget {
    let local = chooser.ctx.local_terminal();
    connect_menu::local_choices(
        &local.sequences,
        terminal_tab::default_local_save(&local, chooser.save_all()),
        glib::clone!(
            #[weak]
            popover,
            #[weak]
            list,
            #[strong]
            chooser,
            move |sequence, save| {
                popover.popdown();
                chooser.open_local(&list, sequence.as_ref(), save);
            }
        ),
    )
}

/// One profile: its name and `user@host:port` (and the jump host it goes
/// through, if any, [`form::list_subtitle`]), plain text, and a "›" whose
/// popover holds its connect choices.
fn profile_row(
    chooser: &Rc<Chooser>,
    list: &gtk::ListBox,
    profile: &Profile,
    route: &Route,
) -> adw::ActionRow {
    let popover = gtk::Popover::new();
    let chosen = profile.clone();
    let choices = connect_menu::choices(
        profile,
        terminal_tab::default_save(profile.transcript, chooser.save_all()),
        glib::clone!(
            #[weak]
            popover,
            #[weak]
            list,
            #[strong]
            chooser,
            move |sequence, record| {
                popover.popdown();
                chooser.connect(&list, &chosen, sequence, record);
            }
        ),
    );
    popover.set_child(Some(&choices));
    let own = profile.transcript;
    refresh_box_on_show(
        &popover,
        glib::clone!(
            #[strong]
            chooser,
            move || terminal_tab::default_save(own, chooser.save_all())
        ),
    );
    let more = more_button("Connect Options", &popover);
    let row = adw::ActionRow::builder()
        .name(profile.id.as_str())
        .title(profile.name.as_str())
        .subtitle(form::list_subtitle(profile, route))
        .use_markup(false)
        .title_lines(1)
        .subtitle_lines(1)
        .activatable(true)
        .build();
    // Whether the host answers a ping (`crate::hosts`), before the name.
    row.add_prefix(&chooser.ctx.hosts.mark_for(profile, route));
    // That it stays on this machine, before the › button.
    if let Some(kept) = local_only::mark(profile) {
        row.add_suffix(&kept);
    }
    row.add_suffix(&more);
    row
}

/// The row of `list` whose widget name is `name`.
fn row_named(list: &gtk::ListBox, name: &str) -> Option<gtk::ListBoxRow> {
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if row.widget_name() == name {
            return Some(row);
        }
        index += 1;
    }
    None
}

/// Filter `list` by the search `query` now and select the row [`default_row`]
/// gives; returns that row. The filter function reads the entry's live text, so
/// invalidating it is all a refilter takes.
fn follow_search(list: &gtk::ListBox, query: &str) -> Option<gtk::ListBoxRow> {
    list.invalidate_filter();
    let row = default_row(list, query);
    if let Some(row) = &row {
        list.select_row(Some(row));
    }
    row
}

/// The row Enter in the search entry picks, and the one selected while
/// searching: with a query, the first profile the search leaves, else Local
/// terminal (the first row, which every search leaves).
fn default_row(list: &gtk::ListBox, query: &str) -> Option<gtk::ListBoxRow> {
    let first_profile = || {
        let mut index = 1;
        while let Some(row) = list.row_at_index(index) {
            if row.is_child_visible() {
                return Some(row);
            }
            index += 1;
        }
        None
    };
    let searching = !query.trim().is_empty();
    searching
        .then(first_profile)
        .flatten()
        .or_else(|| list.row_at_index(0))
}

/// Whether the keyboard focus of `widget`'s window is on `widget` or inside it.
fn focus_within(widget: &impl IsA<gtk::Widget>) -> bool {
    widget
        .root()
        .and_then(|root| root.focus())
        .is_some_and(|focus| &focus == widget.upcast_ref() || focus.is_ancestor(widget))
}

/// The first widget of type `T` below `root`.
fn descendant<T: IsA<gtk::Widget>>(root: &gtk::Widget) -> Option<T> {
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Ok(found) = widget.clone().downcast::<T>() {
            return Some(found);
        }
        if let Some(found) = descendant(&widget) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}
