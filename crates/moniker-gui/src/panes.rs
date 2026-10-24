//! Panes: a tab holds one session or several, side by side or one above the
//! other, in any arrangement (issue #6).
//!
//! # The widgets
//!
//! The child of every tab page that `terminal_tab` makes is a [`TabLayout`]. It
//! holds a tree: a [`Pane`], or a `gtk::Paned` (a split, side by side or one
//! above the other) whose two children are panes or splits again. A pane is a
//! header over the session's [`PaneHolder`] (what a tab was before panes: the
//! new-tab list, the keyring wait or a terminal, with its title, text size and
//! askpass marker), with a drop highlight over it. A tab with one pane looks as
//! a tab always did: the header shows only while the tab has two panes or more.
//!
//! # Focus
//!
//! The pane that had the keyboard focus last is the tab's focused pane
//! ([`TabLayout::focused_holder`]): the tab and the window are titled after it,
//! the window's actions (copy, paste, text size, split, close) act on it, and
//! its header is marked while the others' titles are dimmed. Nothing dims the
//! terminals: an opacity would have them drawn off screen every frame. A change
//! of focused pane emits [`FOCUS_CHANGED`] on the layout (the ☰ menu's text
//! size follows it).
//!
//! # Splitting, moving, closing
//!
//! [`split`] halves the focused pane and puts an empty pane beside it, which the
//! caller fills with the new-tab list. [`move_beside`] moves a pane next to
//! another pane, in any tab of any window; [`join_page`] does the same with
//! every pane of a dragged tab, laid out as they were; [`move_to_new_page`]
//! makes a pane a tab of its own; [`take_out`] removes a closed pane. A split
//! whose pane leaves is replaced by its other child, so the tree never holds an
//! empty split.
//!
//! # Drag and drop
//!
//! A pane is dragged by its header (only shown with two panes or more). Every
//! pane is a drop target for a pane: it goes beside it on the side nearest the
//! pointer ([`side_at`]), and a highlight shows the half it will take. A pane
//! dropped on a tab in a tab bar becomes a tab after that tab
//! ([`setup_tab_bar`]); while a pane is dragged every tab bar shows, as
//! libadwaita shows them for a tab. A pane dropped on nothing gets a window of
//! its own ([`dropped_on_nothing`]).
//!
//! A tab (`adw::TabPage`, what libadwaita's tab drags carry) dropped on a pane
//! brings its panes there. libadwaita makes the contents of every tab
//! untargetable while a tab is dragged, so the tab view takes that drop, in the
//! capture phase, ahead of libadwaita's own target on it, and finds the pane
//! under the pointer itself ([`take_tab_drops`]). Once our drop is done the
//! drag's source sees it as a drop outside every tab view and asks its view
//! for a new window (`create-window`). The window's handler asks
//! [`claims_new_window`] first, which says yes for the page whose panes were
//! just taken: the handler then gives back the asking view itself, libadwaita
//! puts the emptied page back there, and [`page_attached`] closes it.
//!
//! The keyboard never goes to a pane out of sight: a pane given the focus in a
//! tab its window does not show sends it back to the tab that is shown
//! ([`follow_focus`]), as does a tab selected again whose remembered focus
//! moved away with a pane ([`keep_focus_in_selected`]); a pane that closes or
//! a list that shows takes it only where it may ([`in_front`]).
//!
//! Nothing here runs per frame: a change of layout is one allocation, and the
//! highlight changes only when a drag moves to another side. Titles are plain
//! text everywhere; a page's tooltip, which GTK reads as markup, is escaped.

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use vte4::Terminal;

use crate::terminal_tab::{self, PaneHolder};

/// The signal a [`TabLayout`] emits when its focused pane changes.
pub(crate) const FOCUS_CHANGED: &str = "focus-changed";

/// Title of a pane that shows the new-tab list, made by [`split`].
pub(crate) const NEW_PANE_TITLE: &str = "New Pane";

/// The action group each pane has for itself (`pane.`): what its header menu,
/// its close button and its terminal's right-click menu do to that pane.
const PANE_ACTIONS: &str = "pane";

/// Names of the main window's pane actions. `window.rs` defines them; the
/// accelerators are in `main.rs`'s `SHORTCUTS`.
pub(crate) const SPLIT_RIGHT_ACTION: &str = "split-right";
pub(crate) const SPLIT_DOWN_ACTION: &str = "split-down";
pub(crate) const CLOSE_PANE_ACTION: &str = "close-pane";
pub(crate) const TO_TAB_ACTION: &str = "pane-to-tab";
pub(crate) const TO_WINDOW_ACTION: &str = "pane-to-window";
pub(crate) const PANE_LEFT_ACTION: &str = "pane-left";
pub(crate) const PANE_RIGHT_ACTION: &str = "pane-right";
pub(crate) const PANE_UP_ACTION: &str = "pane-up";
pub(crate) const PANE_DOWN_ACTION: &str = "pane-down";

/// The CSS classes of a pane's header, of the focused pane's header, of the
/// drop highlight and of the dragged pane's icon.
pub(crate) const HEADER_CLASS: &str = "moniker-pane-header";
pub(crate) const FOCUSED_CLASS: &str = "focused";
const ZONE_CLASS: &str = "moniker-drop-zone";
const DRAG_ICON_CLASS: &str = "moniker-drag-icon";

/// The look of the header, the highlight and the drag icon, in the accent
/// color of the system style (libadwaita's `--accent-bg-color`).
const CSS: &str = "
.moniker-pane-header { padding: 0 2px 0 10px; border-bottom: 2px solid transparent; }
.moniker-pane-header.focused { border-bottom-color: var(--accent-bg-color); }
.moniker-pane-header button { min-height: 24px; min-width: 24px; padding: 0; }
.moniker-drop-zone { background-color: alpha(var(--accent-bg-color), 0.25); border: 2px solid var(--accent-bg-color); border-radius: 6px; margin: 4px; }
.moniker-drag-icon { padding: 6px 14px; border-radius: 9999px; }
";

/// The tab's widget that holds its panes: the child of its page.
mod layout {
    use std::sync::OnceLock;

    use gtk::glib;
    use gtk::glib::subclass::Signal;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk4 as gtk;

    use crate::terminal_tab::PaneHolder;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct TabLayout {
            /// The pane that had the keyboard focus last.
            pub(super) focused: glib::WeakRef<PaneHolder>,
            /// The focused pane as `FOCUS_CHANGED` last told it.
            pub(super) announced: glib::WeakRef<PaneHolder>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for TabLayout {
            const NAME: &'static str = "MonikerTabLayout";
            type Type = super::TabLayout;
            type ParentType = gtk::Box;
        }

        impl ObjectImpl for TabLayout {
            fn signals() -> &'static [Signal] {
                static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
                SIGNALS.get_or_init(|| vec![Signal::builder(super::super::FOCUS_CHANGED).build()])
            }
        }

        /// The keyboard focus given to the tab from outside (the tab view
        /// moves it in when it selects a tab it has no focus to restore for)
        /// goes to its focused pane, not to the first header button. Within
        /// the tab it moves as GTK moves it.
        impl WidgetImpl for TabLayout {
            fn grab_focus(&self) -> bool {
                match self.obj().focused_holder() {
                    Some(holder) => super::super::focus_content(&holder),
                    None => self.parent_grab_focus(),
                }
            }

            fn focus(&self, direction: gtk::DirectionType) -> bool {
                let layout = self.obj();
                let inside = layout
                    .root()
                    .and_then(|root| root.focus())
                    .is_some_and(|focus| focus.is_ancestor(&*layout));
                if !inside
                    && let Some(holder) = layout.focused_holder()
                    && super::super::focus_content(&holder)
                {
                    return true;
                }
                self.parent_focus(direction)
            }
        }
        impl BoxImpl for TabLayout {}
    }

    glib::wrapper! {
        pub struct TabLayout(ObjectSubclass<imp::TabLayout>)
            @extends gtk::Box, gtk::Widget,
            @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
    }

    impl TabLayout {
        pub(super) fn new() -> TabLayout {
            let layout: TabLayout = glib::Object::new();
            layout.set_hexpand(true);
            layout.set_vexpand(true);
            layout
        }

        pub(super) fn focused(&self) -> &glib::WeakRef<PaneHolder> {
            &self.imp().focused
        }

        pub(super) fn announced(&self) -> &glib::WeakRef<PaneHolder> {
            &self.imp().announced
        }
    }
}

pub(crate) use layout::TabLayout;

/// One pane: a header over the session's holder, with the drop highlight.
mod pane {
    use std::cell::{Cell, OnceCell};

    use gtk::subclass::prelude::*;
    use gtk::{gio, glib};
    use gtk4 as gtk;

    use super::Side;
    use crate::terminal_tab::PaneHolder;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct Pane {
            pub(super) holder: OnceCell<PaneHolder>,
            pub(super) header: OnceCell<gtk::Box>,
            pub(super) title: OnceCell<gtk::Label>,
            pub(super) zone: OnceCell<gtk::Box>,
            pub(super) actions: OnceCell<gio::SimpleActionGroup>,
            /// The side the drop highlight shows, while something is dragged
            /// over the pane.
            pub(super) side: Cell<Option<Side>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for Pane {
            const NAME: &'static str = "MonikerPane";
            type Type = super::Pane;
            type ParentType = gtk::Box;
        }

        impl ObjectImpl for Pane {}
        impl WidgetImpl for Pane {}
        impl BoxImpl for Pane {}
    }

    glib::wrapper! {
        pub struct Pane(ObjectSubclass<imp::Pane>)
            @extends gtk::Box, gtk::Widget,
            @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
    }

    impl Pane {
        pub(super) fn new(
            holder: &PaneHolder,
            header: &gtk::Box,
            title: &gtk::Label,
            zone: &gtk::Box,
        ) -> Pane {
            let pane: Pane = glib::Object::new();
            let imp = pane.imp();
            let _ = imp.holder.set(holder.clone());
            let _ = imp.header.set(header.clone());
            let _ = imp.title.set(title.clone());
            let _ = imp.zone.set(zone.clone());
            pane
        }

        /// The session's holder.
        pub(crate) fn holder(&self) -> &PaneHolder {
            self.imp().holder.get().expect("set when made")
        }

        /// The header (title, menu, close button).
        pub(crate) fn header(&self) -> &gtk::Box {
            self.imp().header.get().expect("set when made")
        }

        /// The title in the header.
        pub(crate) fn title(&self) -> &gtk::Label {
            self.imp().title.get().expect("set when made")
        }

        pub(super) fn zone(&self) -> &gtk::Box {
            self.imp().zone.get().expect("set when made")
        }

        pub(super) fn side(&self) -> &Cell<Option<Side>> {
            &self.imp().side
        }

        /// Its own actions (`pane.`), once [`set_actions`](Pane::set_actions)
        /// gave them.
        pub(super) fn actions(&self) -> &gio::SimpleActionGroup {
            self.imp().actions.get().expect("set when made")
        }

        pub(super) fn set_actions(&self, actions: &gio::SimpleActionGroup) {
            let _ = self.imp().actions.set(actions.clone());
        }
    }
}

pub(crate) use pane::Pane;

impl TabLayout {
    /// The split or pane the layout holds (none once its panes have gone).
    fn node(&self) -> Option<gtk::Widget> {
        self.first_child()
    }

    /// Its panes, in reading order (the first child of a split before the
    /// second).
    pub(crate) fn panes(&self) -> Vec<Pane> {
        let mut panes = Vec::new();
        if let Some(node) = self.node() {
            collect_panes(&node, &mut panes);
        }
        panes
    }

    /// The holders of its panes, in reading order.
    pub(crate) fn holders(&self) -> Vec<PaneHolder> {
        self.panes()
            .iter()
            .map(|pane| pane.holder().clone())
            .collect()
    }

    /// The focused pane's holder: the pane that had the keyboard focus last,
    /// else the first.
    pub(crate) fn focused_holder(&self) -> Option<PaneHolder> {
        self.focused()
            .upgrade()
            .filter(|holder| holder.is_ancestor(self))
            .or_else(|| self.holders().into_iter().next())
    }

    /// Make `holder`'s pane the focused one (the keyboard focus is not moved:
    /// [`focus_pane`] does both).
    fn set_focused(&self, holder: &PaneHolder) {
        if self.focused().upgrade().as_ref() == Some(holder) {
            return;
        }
        self.focused().set(Some(holder));
        refresh(self);
    }
}

fn collect_panes(node: &gtk::Widget, panes: &mut Vec<Pane>) {
    if let Some(pane) = node.downcast_ref::<Pane>() {
        panes.push(pane.clone());
    } else if let Some(split) = node.downcast_ref::<gtk::Paned>() {
        for child in [split.start_child(), split.end_child()]
            .into_iter()
            .flatten()
        {
            collect_panes(&child, panes);
        }
    }
}

/// The layout of `page`, if `terminal_tab` made the page.
pub(crate) fn layout_of_page(page: &adw::TabPage) -> Option<TabLayout> {
    page.child().downcast().ok()
}

/// The layout `widget` is in.
pub(crate) fn layout_of(widget: &impl IsA<gtk::Widget>) -> Option<TabLayout> {
    widget.ancestor(TabLayout::static_type()).and_downcast()
}

/// The pane of `holder`.
pub(crate) fn pane_of(holder: &PaneHolder) -> Option<Pane> {
    holder.ancestor(Pane::static_type()).and_downcast()
}

/// The tab view `layout` is shown in and its page there, unless it is not in
/// one right now (its tab was closed, or is being dragged).
pub(crate) fn page_of(layout: &TabLayout) -> Option<(adw::TabView, adw::TabPage)> {
    let view = layout
        .ancestor(adw::TabView::static_type())
        .and_downcast::<adw::TabView>()?;
    let page = view.page(layout);
    Some((view, page))
}

/// A new layout whose one pane holds `holder`.
pub(crate) fn new_layout(holder: &PaneHolder) -> TabLayout {
    let layout = TabLayout::new();
    layout.append(&build_pane(holder));
    layout.focused().set(Some(holder));
    layout
}

/// Show what the panes of `layout` are called, which one has the focus and
/// what can be done now: the headers (only with two panes or more), the title
/// and tooltip of the tab and the window's actions; and tell those who follow
/// the focused pane ([`FOCUS_CHANGED`]).
pub(crate) fn refresh(layout: &TabLayout) {
    let panes = layout.panes();
    let several = panes.len() > 1;
    let focused = layout.focused_holder();
    for pane in &panes {
        let header = pane.header();
        header.set_visible(several);
        let is_focused = focused.as_ref() == Some(pane.holder());
        if is_focused {
            header.add_css_class(FOCUSED_CLASS);
            pane.title().remove_css_class("dim-label");
        } else {
            header.remove_css_class(FOCUSED_CLASS);
            pane.title().add_css_class("dim-label");
        }
        pane.title()
            .set_text(&terminal_tab::shown_title(pane.holder()));
        for name in [TO_TAB_ACTION, TO_WINDOW_ACTION] {
            if let Some(action) = pane
                .actions()
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(several);
            }
        }
    }
    if let Some((view, page)) = page_of(layout) {
        show_titles(&page, &panes, focused.as_ref());
        terminal_tab::sync_window_actions(&view);
    }
    if layout.announced().upgrade() != focused {
        layout.announced().set(focused.as_ref());
        layout.emit_by_name::<()>(FOCUS_CHANGED, &[]);
    }
}

/// Whether `holder`'s pane is the focused pane of its tab, and that tab the
/// one its view shows (or in no view: a tab being dragged, a test's).
pub(crate) fn is_shown_and_focused(holder: &PaneHolder) -> bool {
    let Some(layout) = layout_of(holder) else {
        return false;
    };
    if layout.focused_holder().as_ref() != Some(holder) {
        return false;
    }
    page_of(&layout).is_none_or(|(view, page)| view.selected_page().as_ref() == Some(&page))
}

/// Whether `holder`'s pane may take the keyboard now: it is the focused pane
/// of the tab its window shows, and no dialog is open over the window. Keys
/// typed must never go to a session out of sight, nor behind a dialog.
pub(crate) fn in_front(holder: &PaneHolder) -> bool {
    is_shown_and_focused(holder)
        && holder
            .root()
            .and_downcast::<gtk::Window>()
            .is_none_or(|window| crate::notify::visible_dialog(&window).is_none())
}

/// Title `page` after the focused pane and list every pane's title in its
/// tooltip, which GTK reads as markup: each title is escaped.
fn show_titles(page: &adw::TabPage, panes: &[Pane], focused: Option<&PaneHolder>) {
    let titles: Vec<String> = panes
        .iter()
        .map(|pane| terminal_tab::shown_title(pane.holder()))
        .collect();
    let title = focused
        .map(terminal_tab::shown_title)
        .or_else(|| titles.first().cloned())
        .unwrap_or_default();
    page.set_title(&title);
    let tooltip: Vec<String> = titles
        .iter()
        .map(|t| glib::markup_escape_text(t).to_string())
        .collect();
    page.set_tooltip(&tooltip.join("\n"));
}

/// The pane widget for `holder`: header (title, menu, close button) over the
/// holder, with the drop highlight over it; its `pane.` actions, the drag
/// source on the header and the drop target on the pane.
fn build_pane(holder: &PaneHolder) -> Pane {
    add_css();
    let title = gtk::Label::builder()
        .hexpand(true)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .use_markup(false)
        .build();
    let menu = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Pane Menu")
        .menu_model(&header_menu())
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let close = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Close Pane")
        .action_name(format!("{PANE_ACTIONS}.close"))
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .css_classes([HEADER_CLASS])
        .visible(false)
        .build();
    header.append(&title);
    header.append(&menu);
    header.append(&close);

    let zone = gtk::Box::builder()
        .css_classes([ZONE_CLASS])
        .can_target(false)
        .visible(false)
        .build();
    let overlay = gtk::Overlay::builder()
        .child(holder)
        .hexpand(true)
        .vexpand(true)
        .build();
    overlay.add_overlay(&zone);

    let pane = Pane::new(holder, &header, &title, &zone);
    pane.set_orientation(gtk::Orientation::Vertical);
    pane.set_hexpand(true);
    pane.set_vexpand(true);
    pane.append(&header);
    pane.append(&overlay);

    add_pane_actions(&pane);
    follow_focus(&pane);
    rename_on_double_click(&pane);
    drag_by_header(&pane);
    take_drops(&pane);
    pane
}

/// The header's menu.
fn header_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(
        Some("Rename\u{2026}"),
        Some(&format!("{PANE_ACTIONS}.rename")),
    );
    let moves = gio::Menu::new();
    moves.append(
        Some("Move to New Tab"),
        Some(&format!("{PANE_ACTIONS}.{TO_TAB_ACTION}")),
    );
    moves.append(
        Some("Move to New Window"),
        Some(&format!("{PANE_ACTIONS}.{TO_WINDOW_ACTION}")),
    );
    menu.append_section(None, &moves);
    menu
}

/// The Split Right and Split Down entries of a terminal's right-click menu,
/// which split the pane the terminal is in.
pub(crate) fn split_menu_section() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(
        Some("Split Right"),
        Some(&format!("{PANE_ACTIONS}.{SPLIT_RIGHT_ACTION}")),
    );
    menu.append(
        Some("Split Down"),
        Some(&format!("{PANE_ACTIONS}.{SPLIT_DOWN_ACTION}")),
    );
    menu
}

/// The pane's own actions (`pane.`), for its header, its close button and its
/// terminal's menu: they act on this pane, not on the focused one. Splitting
/// and moving to a new window need what only the window has (the application
/// and its context), so those focus this pane and run the window's action.
/// Those of a menu run once the closing menu has handed the keyboard focus
/// back (`terminal_tab::after_the_menu_lets_go`), which would otherwise put it
/// back on the right-clicked terminal or the header button after the change.
fn add_pane_actions(pane: &Pane) {
    let actions = gio::SimpleActionGroup::new();
    let after_the_menu = |pane: &Pane, change: Box<dyn FnOnce(&Pane)>| {
        let window = pane.root().and_downcast::<gtk::Window>();
        let pane = pane.downgrade();
        terminal_tab::after_the_menu_lets_go(window.as_ref(), move || {
            if let Some(pane) = pane.upgrade() {
                change(&pane);
            }
        });
    };
    for name in [SPLIT_RIGHT_ACTION, SPLIT_DOWN_ACTION, TO_WINDOW_ACTION] {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            pane,
            move |_, _| {
                after_the_menu(
                    &pane,
                    Box::new(move |pane| {
                        show_pane(pane);
                        let _ = pane.activate_action(&format!("win.{name}"), None);
                    }),
                );
            }
        ));
        actions.add_action(&action);
    }
    let close = gio::SimpleAction::new("close", None);
    close.connect_activate(glib::clone!(
        #[weak]
        pane,
        move |_, _| terminal_tab::close_pane(pane.holder())
    ));
    actions.add_action(&close);
    let to_tab = gio::SimpleAction::new(TO_TAB_ACTION, None);
    to_tab.connect_activate(glib::clone!(
        #[weak]
        pane,
        move |_, _| {
            after_the_menu(
                &pane,
                Box::new(|pane| {
                    if let Some((view, page)) = layout_of(pane).as_ref().and_then(page_of) {
                        move_to_new_page(pane, &view, view.page_position(&page) + 1);
                    }
                }),
            );
        }
    ));
    actions.add_action(&to_tab);
    // As the tab menu's Rename does: the dialog opens once the closing menu has
    // handed the keyboard focus back.
    let rename = gio::SimpleAction::new("rename", None);
    rename.connect_activate(glib::clone!(
        #[weak]
        pane,
        move |_, _| {
            let window = pane.root().and_downcast::<gtk::Window>();
            let holder = pane.holder().clone();
            terminal_tab::after_the_menu_lets_go(window.as_ref(), move || {
                terminal_tab::ask_rename_pane(&holder);
            });
        }
    ));
    actions.add_action(&rename);
    pane.insert_action_group(PANE_ACTIONS, Some(&actions));
    pane.set_actions(&actions);
}

/// Make the pane the focused one of its tab whenever the keyboard focus goes
/// into it (a click in its terminal, a key that moves the focus there), and
/// when it is clicked anywhere (its header, a pane that waits for the keyring),
/// give it the keyboard too.
///
/// One guard for every way the keyboard could land in a pane of a tab its
/// window does not show (a widget a tab remembered that moved there, a focus
/// asked for by code that did not look): it is sent back, before any key
/// typed meanwhile is handled, to the tab that is shown
/// ([`keep_focus_in_selected`]). Keys must never reach a session out of sight.
fn follow_focus(pane: &Pane) {
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter(glib::clone!(
        #[weak]
        pane,
        move |_| {
            let Some(layout) = layout_of(&pane) else {
                return;
            };
            if let Some((view, page)) = page_of(&layout)
                && view.selected_page().as_ref() != Some(&page)
            {
                // Not from inside the focus change GTK is making: once it is
                // done, ahead of input events.
                let view = view.downgrade();
                glib::idle_add_local_full(glib::Priority::HIGH, move || {
                    if let Some(view) = view.upgrade() {
                        keep_focus_in_selected(&view);
                    }
                    glib::ControlFlow::Break
                });
                return;
            }
            layout.set_focused(pane.holder());
        }
    ));
    pane.add_controller(focus);

    let click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    click.connect_pressed(glib::clone!(
        #[weak]
        pane,
        move |_, _, x, y| {
            let within = pane
                .root()
                .and_then(|root| root.focus())
                .is_some_and(|focus| {
                    focus == *pane.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&pane)
                });
            // Not for the header's buttons: × on a pane not in use closes it
            // and leaves the keyboard where it is, the menu acts on its pane
            // anyway.
            if !within && !on_a_button(&pane, x, y) {
                focus_pane(pane.holder());
            }
        }
    ));
    pane.add_controller(click);
}

/// Whether the point (`x`, `y`) of `pane` is on a button (the header's).
fn on_a_button(pane: &Pane, x: f64, y: f64) -> bool {
    let mut widget = pane.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(current) = widget {
        if current.is::<gtk::Button>() {
            return true;
        }
        if current == *pane.upcast_ref::<gtk::Widget>() {
            return false;
        }
        widget = current.parent();
    }
    false
}

/// A double click on the header's title renames the pane.
fn rename_on_double_click(pane: &Pane) {
    let double_click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_PRIMARY)
        .build();
    double_click.connect_pressed(glib::clone!(
        #[weak]
        pane,
        move |gesture, n_press, _, _| {
            if n_press == 2 {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                terminal_tab::ask_rename_pane(pane.holder());
            }
        }
    ));
    pane.title().add_controller(double_click);
}

/// Give `holder`'s pane the keyboard focus and make it its tab's focused pane:
/// its terminal, or the first thing in it that takes the focus (the new-tab
/// list).
///
/// A pane with nothing to take the keyboard yet (one that waits for the
/// keyring) is still made the focused pane, and the keyboard is taken from
/// the other panes of its tab: the focused pane and the keys must not part
/// ways, or Ctrl+Shift+W would close one pane while typing goes to another,
/// and Ctrl+Shift+C (copy, off without a terminal) would reach a terminal as
/// Ctrl+C.
pub(crate) fn focus_pane(holder: &PaneHolder) {
    let layout = layout_of(holder);
    if let Some(layout) = &layout {
        layout.set_focused(holder);
    }
    if !focus_content(holder)
        && let Some(layout) = &layout
        && let Some(root) = holder.root()
        && root.focus().is_some_and(|focus| focus.is_ancestor(layout))
    {
        root.set_focus(gtk::Widget::NONE);
    }
}

/// Give the keyboard focus to what `holder` shows: its terminal, or the first
/// thing in it that takes the focus. False if nothing took it.
fn focus_content(holder: &PaneHolder) -> bool {
    match holder.first_child().and_downcast::<Terminal>() {
        Some(terminal) => terminal.grab_focus(),
        None => holder.child_focus(gtk::DirectionType::TabForward),
    }
}

/// The widget that holds `node` (a pane or a split), and puts another in its
/// place.
fn replace(old: &gtk::Widget, new: &gtk::Widget) {
    let Some(parent) = old.parent() else {
        return;
    };
    if let Some(split) = parent.downcast_ref::<gtk::Paned>() {
        if split.start_child().as_ref() == Some(old) {
            split.set_start_child(Some(new));
        } else {
            split.set_end_child(Some(new));
        }
    } else if let Some(layout) = parent.downcast_ref::<TabLayout>() {
        layout.remove(old);
        layout.append(new);
    }
}

/// Take `node` (a pane or a split) out of its layout. A split it leaves is
/// replaced by its other child.
fn detach(node: &gtk::Widget) {
    let Some(parent) = node.parent() else {
        return;
    };
    if let Some(split) = parent.downcast_ref::<gtk::Paned>() {
        let other = if split.start_child().as_ref() == Some(node) {
            split.end_child()
        } else {
            split.start_child()
        };
        split.set_start_child(gtk::Widget::NONE);
        split.set_end_child(gtk::Widget::NONE);
        if let Some(other) = other {
            replace(split.upcast_ref(), &other);
        }
    } else if let Some(parent) = parent.downcast_ref::<TabLayout>() {
        parent.remove(node);
    }
}

/// The way a split for `side` divides its room.
fn orientation_for(side: Side) -> gtk::Orientation {
    match side {
        Side::Left | Side::Right => gtk::Orientation::Horizontal,
        Side::Top | Side::Bottom => gtk::Orientation::Vertical,
    }
}

/// How much room `target` will have, the way a split for `side` divides it,
/// once `leaving` (a pane or split about to be taken out, if any) has gone: the
/// room of the split the two share if they are its children (the target then
/// fills it), else the target's own. Measured before anything moves.
fn room_beside(target: &Pane, side: Side, leaving: Option<&gtk::Widget>) -> i32 {
    let shared = target
        .parent()
        .filter(|parent| leaving.and_then(gtk::Widget::parent).as_ref() == Some(parent));
    let room: gtk::Widget = shared.unwrap_or_else(|| target.clone().upcast());
    match orientation_for(side) {
        gtk::Orientation::Horizontal => room.width(),
        _ => room.height(),
    }
}

/// Put `node` (a pane or a split) beside `target`, on its `side`: the two share
/// `room`, the target's room measured before anything moved ([`room_beside`]),
/// half each.
fn put_beside(node: &gtk::Widget, target: &Pane, side: Side, room: i32) {
    let orientation = orientation_for(side);
    let split = gtk::Paned::builder()
        .orientation(orientation)
        .hexpand(true)
        .vexpand(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .build();
    replace(target.upcast_ref(), split.upcast_ref());
    let (first, second): (&gtk::Widget, &gtk::Widget) = match side {
        Side::Left | Side::Top => (node, target.upcast_ref()),
        Side::Right | Side::Bottom => (target.upcast_ref(), node),
    };
    split.set_start_child(Some(first));
    split.set_end_child(Some(second));
    if room > 0 {
        split.set_position(room / 2);
    }
}

/// The keyboard focus of the windows of some layouts, set aside while panes
/// and splits move in them ([`FocusAside::take`]) and given back after
/// ([`FocusAside::give_back`]). GTK's split widget warns when a widget that
/// has the keyboard focus is taken out of it, which moving a pane or a split
/// does.
struct FocusAside(Vec<gtk::Widget>);

impl FocusAside {
    /// Take the keyboard focus from every window where it is inside one of
    /// `layouts`, or on a widget no longer in the window at all (a closed
    /// pane's terminal: GTK leaves the focus on a widget taken out of the
    /// window), which is not given back.
    fn take(layouts: &[&TabLayout]) -> FocusAside {
        let mut aside = Vec::new();
        for layout in layouts {
            let Some(root) = layout.root() else {
                continue;
            };
            let Some(focus) = root.focus() else {
                continue;
            };
            let gone = focus.root().as_ref() != Some(&root);
            if gone || focus.is_ancestor(*layout) {
                root.set_focus(gtk::Widget::NONE);
                if !gone {
                    aside.push(focus);
                }
            }
        }
        FocusAside(aside)
    }

    /// Give it back to the widgets that had it, those still shown (not in a
    /// tab that was left meanwhile). What follows may give it to another pane:
    /// a moved one, for instance.
    fn give_back(self) {
        for focus in self.0 {
            if focus.is_mapped() {
                focus.grab_focus();
            }
        }
    }
}

/// After panes left `layout`: close its tab if none is left (the sessions that
/// were in it went elsewhere or were ended already), else show what is left,
/// and give the keyboard to its focused pane if its window has none left (it
/// was in a pane that went) and that pane may take it ([`in_front`]: not in a
/// tab out of sight, not behind a dialog).
fn after_leaving(layout: &TabLayout) {
    if layout.node().is_some() {
        refresh(layout);
        if layout.root().is_some_and(|root| root.focus().is_none())
            && let Some(holder) = layout.focused_holder()
            && in_front(&holder)
        {
            focus_content(&holder);
        }
    } else if let Some((view, page)) = page_of(layout) {
        view.close_page(&page);
    }
}

/// Split the focused pane of `view`'s selected tab: it keeps the first half
/// and a new, empty pane at the same text size takes the second, on its right
/// or below it (`side`). The new pane is the focused one. Returns its holder,
/// for the caller to fill (the new-tab list).
pub(crate) fn split(view: &adw::TabView, side: Side) -> Option<PaneHolder> {
    let layout = layout_of_page(&view.selected_page()?)?;
    let target = pane_of(&layout.focused_holder()?)?;
    let holder = PaneHolder::new(
        terminal_tab::TabTitle::remote(NEW_PANE_TITLE.to_owned()),
        target.holder().scale().get(),
    );
    let pane = build_pane(&holder);
    let room = room_beside(&target, side, None);
    // Not given back: the new pane takes the keyboard once it shows what it
    // is filled with.
    let _ = FocusAside::take(&[&layout]);
    put_beside(pane.upcast_ref(), &target, side, room);
    layout.focused().set(Some(&holder));
    refresh(&layout);
    Some(holder)
}

/// Take `holder`'s pane out of its tab, whose other panes share its room; the
/// one that takes its place becomes the focused pane if it was, and gets the
/// keyboard if it was in the pane that went and the tab is in front
/// ([`after_leaving`]): a pane that closes in a tab out of sight, or behind a
/// dialog, moves no keyboard focus. The session must be over
/// (`terminal_tab::close_pane`). A tab left with no pane closes.
pub(crate) fn take_out(holder: &PaneHolder) {
    let Some(pane) = pane_of(holder) else {
        return;
    };
    let was_focused = layout_of(&pane)
        .and_then(|layout| layout.focused_holder())
        .as_ref()
        == Some(holder);
    // The pane beside it, which will take its room.
    let heir = pane
        .parent()
        .and_downcast::<gtk::Paned>()
        .and_then(|split| {
            if split.start_child().as_ref() == Some(pane.upcast_ref()) {
                split.end_child()
            } else {
                split.start_child()
            }
        })
        .and_then(|node| {
            let mut panes = Vec::new();
            collect_panes(&node, &mut panes);
            panes.into_iter().next()
        });
    let Some(layout) = layout_of(&pane) else {
        return;
    };
    let aside = FocusAside::take(&[&layout]);
    detach(pane.upcast_ref());
    if was_focused && let Some(heir) = &heir {
        layout.focused().set(Some(heir.holder()));
    }
    after_leaving(&layout);
    aside.give_back();
}

/// Move `pane` beside `target` (in any tab of any window), on its `side`. The
/// moved pane gets the focus and its tab is selected. Nothing happens for a
/// pane dropped on itself.
pub(crate) fn move_beside(pane: &Pane, target: &Pane, side: Side) {
    if pane == target {
        return;
    }
    let (from, to) = (layout_of(pane), layout_of(target));
    let room = room_beside(target, side, Some(pane.upcast_ref()));
    let aside = FocusAside::take(
        &[from.as_ref(), to.as_ref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
    );
    detach(pane.upcast_ref());
    put_beside(pane.upcast_ref(), target, side, room);
    if let Some(from) = from
        && Some(&from) != to.as_ref()
    {
        after_leaving(&from);
    }
    aside.give_back();
    show_moved(pane.holder());
}

/// Select the tab `holder` is in, raise its window and give it the focus.
fn show_moved(holder: &PaneHolder) {
    let Some(layout) = layout_of(holder) else {
        return;
    };
    if let Some((view, page)) = page_of(&layout) {
        view.set_selected_page(&page);
    }
    if let Some(window) = layout.root().and_downcast::<gtk::Window>() {
        window.present();
    }
    focus_pane(holder);
    refresh(&layout);
}

/// Make `pane` a tab of its own in `view`, at `position`, selected. The tab
/// it leaves keeps its other panes (or closes, with none left).
pub(crate) fn move_to_new_page(pane: &Pane, view: &adw::TabView, position: i32) {
    let from = layout_of(pane);
    let aside = FocusAside::take(&from.iter().collect::<Vec<_>>());
    detach(pane.upcast_ref());
    let layout = TabLayout::new();
    layout.append(pane);
    layout.focused().set(Some(pane.holder()));
    let position = position.clamp(0, view.n_pages());
    let page = view.insert(&layout, position);
    if let Some(from) = from {
        after_leaving(&from);
    }
    view.set_selected_page(&page);
    aside.give_back();
    show_moved(pane.holder());
}

/// How long the tab whose panes a drop took is waited for, to be closed once
/// libadwaita puts it back ([`join_page`]); it takes milliseconds.
const JOINED_FOR: std::time::Duration = std::time::Duration::from_secs(10);

thread_local! {
    /// The tab whose panes a drop on a pane took ([`join_page`]), until it is
    /// back in a view and closed. GTK runs on one thread.
    static JOINED: glib::WeakRef<adw::TabPage> = glib::WeakRef::new();
}

/// Move every pane of the tab of `page`, laid out as they are, beside `target`
/// on its `side`; the tab, left empty, goes. `page` may be in a tab view or,
/// being dragged, in none: then libadwaita asks for a window for it after the
/// drop, which [`claims_new_window`] declines (the page goes back to its
/// view), and [`page_attached`] closes it there. False if `page` holds no
/// panes.
pub(crate) fn join_page(page: &adw::TabPage, target: &Pane, side: Side) -> bool {
    let Some(layout) = layout_of_page(page) else {
        return false;
    };
    if target.is_ancestor(&layout) {
        return false;
    }
    let Some(node) = layout.node() else {
        return false;
    };
    let focused = layout.focused_holder();
    let room = room_beside(target, side, None);
    let aside = FocusAside::take(
        &[&layout]
            .into_iter()
            .chain(layout_of(target).as_ref())
            .collect::<Vec<_>>(),
    );
    layout.remove(&node);
    put_beside(&node, target, side, room);
    match view_of_page(page) {
        Some(view) => view.close_page(page),
        None => {
            JOINED.with(|joined| joined.set(Some(page)));
            // Should libadwaita never put it back, the claim must not outlive
            // this drag: a later tab dropped outside every window gets its
            // window.
            let page = page.downgrade();
            glib::timeout_add_local_once(JOINED_FOR, move || {
                JOINED.with(|joined| {
                    if joined.upgrade().is_some_and(|j| Some(j) == page.upgrade()) {
                        joined.set(None);
                    }
                });
            });
        }
    }
    aside.give_back();
    if let Some(holder) = focused {
        show_moved(&holder);
    }
    true
}

/// The tab view that shows `page`, if it is in one now.
fn view_of_page(page: &adw::TabPage) -> Option<adw::TabView> {
    page.child()
        .ancestor(adw::TabView::static_type())
        .and_downcast()
}

/// Whether the new window a tab view asks for (`create-window`) is not wanted:
/// the tab being dragged was dropped on a pane, which took its panes
/// ([`join_page`]). The window's handler then gives back the asking view, and
/// libadwaita puts the page there.
pub(crate) fn claims_new_window() -> bool {
    JOINED.with(|joined| {
        joined
            .upgrade()
            .is_some_and(|page| view_of_page(&page).is_none())
    })
}

/// A tab came into `view` (`page-attached`): show its titles (they may have
/// changed while it was being dragged), and if it is the one whose panes a
/// drop took, close it, from an idle callback (libadwaita is still attaching
/// it).
///
/// libadwaita selects the page it puts back, and the focus that page
/// remembers has moved away with its panes, so none is left in the window.
/// The tab that was selected before it came back is selected again (with
/// several tabs, closing the page would select another), and its focused
/// pane gets the keyboard back if it may ([`in_front`]), from wherever GTK
/// put it meanwhile outside the tab. (The page is attached, and this signal
/// emitted, before libadwaita selects it.)
pub(crate) fn page_attached(view: &adw::TabView, page: &adw::TabPage) {
    if let Some(layout) = layout_of_page(page)
        && layout.node().is_some()
    {
        refresh(&layout);
    }
    let joined = JOINED.with(|joined| joined.upgrade());
    if joined.as_ref() != Some(page) {
        return;
    }
    JOINED.with(|joined| joined.set(None));
    let before = view.selected_page().map(|before| before.downgrade());
    let view = view.downgrade();
    let page = page.downgrade();
    glib::idle_add_local_once(move || {
        let (Some(view), Some(page)) = (view.upgrade(), page.upgrade()) else {
            return;
        };
        if view_of_page(&page).as_ref() != Some(&view)
            || layout_of_page(&page).is_none_or(|layout| layout.node().is_some())
        {
            return;
        }
        if let Some(before) = before.and_then(|before| before.upgrade())
            && before != page
            && view_of_page(&before).as_ref() == Some(&view)
        {
            view.set_selected_page(&before);
        }
        view.close_page(&page);
        // Left with no focus, GTK gives the window's to its first button (＋)
        // by itself, maybe before this runs: a focus outside the tab shown
        // counts as none.
        if let Some(layout) = view.selected_page().as_ref().and_then(layout_of_page)
            && let Some(holder) = layout.focused_holder()
            && in_front(&holder)
            && view
                .root()
                .is_some_and(|root| root.focus().is_none_or(|focus| !focus.is_ancestor(&layout)))
        {
            focus_content(&holder);
        }
    });
}

/// Tests only: a tab in `view` whose panes have all gone, as a tab dropped on
/// a pane is when libadwaita puts it back ([`page_attached`]).
#[cfg(test)]
pub(crate) fn empty_page_for_tests(view: &adw::TabView) -> adw::TabPage {
    view.append(&TabLayout::new())
}

/// Tests only: take `page` for the tab a drop on a pane emptied, which
/// [`page_attached`] closes once it is back in a view.
#[cfg(test)]
pub(crate) fn expect_back_for_tests(page: &adw::TabPage) {
    JOINED.with(|joined| joined.set(Some(page)));
}

/// Keep the keyboard focus out of the tabs `view` does not show
/// (`selected-page` changed). The tab view gives a tab it selects the focus
/// it had when it was left, and that widget may since have moved to another
/// tab, hidden now: keys typed then would go to a session out of sight, on
/// another host maybe. Such a focus goes to the selected tab's focused pane.
pub(crate) fn keep_focus_in_selected(view: &adw::TabView) {
    let (Some(page), Some(root)) = (view.selected_page(), view.root()) else {
        return;
    };
    let Some(focus) = root.focus() else {
        return;
    };
    let shown = page.child();
    if !focus.is_ancestor(view) || focus == shown || focus.is_ancestor(&shown) {
        return;
    }
    let focused = layout_of_page(&page).and_then(|layout| layout.focused_holder());
    let moved = focused.is_some_and(|holder| focus_content(&holder));
    if !moved && !shown.child_focus(gtk::DirectionType::TabForward) {
        // Nothing in the tab takes the keyboard: better nowhere than out of
        // sight.
        root.set_focus(gtk::Widget::NONE);
    }
}

/// Go from the focused pane of `view`'s selected tab to the pane next to it
/// `direction`, if there is one.
pub(crate) fn focus_toward(view: &adw::TabView, direction: Direction) {
    let Some(layout) = view.selected_page().as_ref().and_then(layout_of_page) else {
        return;
    };
    let panes = layout.panes();
    let Some(from) = layout
        .focused_holder()
        .and_then(|holder| panes.iter().position(|pane| pane.holder() == &holder))
    else {
        return;
    };
    let bounds: Vec<Bounds> = panes
        .iter()
        .map(|pane| {
            let rect = pane
                .compute_bounds(&layout)
                .unwrap_or_else(gtk::graphene::Rect::zero);
            Bounds {
                x: f64::from(rect.x()),
                y: f64::from(rect.y()),
                width: f64::from(rect.width()),
                height: f64::from(rect.height()),
            }
        })
        .collect();
    if let Some(to) = neighbor(bounds[from], &bounds, direction) {
        focus_pane(panes[to].holder());
    }
}

/// Whether the selected tab of `view` has two panes or more.
pub(crate) fn selected_is_split(view: &adw::TabView) -> bool {
    view.selected_page()
        .as_ref()
        .and_then(layout_of_page)
        .is_some_and(|layout| layout.panes().len() > 1)
}

/// What the dragged pane's icon shows: its title, in a pill.
fn drag_icon(pane: &Pane) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(terminal_tab::shown_title(pane.holder()))
        .use_markup(false)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .build();
    let pill = gtk::Box::builder()
        .css_classes(["osd", DRAG_ICON_CLASS])
        .build();
    pill.append(&label);
    pill.upcast()
}

/// The pane can be dragged by its header: to another pane, to a tab bar, or out
/// of every window.
fn drag_by_header(pane: &Pane) {
    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .build();
    source.connect_prepare(glib::clone!(
        #[weak]
        pane,
        #[upgrade_or]
        None,
        move |_, _, _| {
            // Only a pane among others: alone, it is its tab, which its tab
            // bar drags.
            let several = layout_of(&pane).is_some_and(|layout| layout.panes().len() > 1);
            several.then(|| gdk::ContentProvider::for_value(&pane.to_value()))
        }
    ));
    source.connect_drag_begin(glib::clone!(
        #[weak]
        pane,
        move |_, drag| {
            if let Ok(icon) = gtk::DragIcon::for_drag(drag).downcast::<gtk::DragIcon>() {
                icon.set_child(Some(&drag_icon(&pane)));
            }
            reveal_tab_bars(true);
        }
    ));
    source.connect_drag_end(|_, _, _| reveal_tab_bars(false));
    // Dropped on nothing that takes it, inside a window or outside every one:
    // a window of its own, as a tab dropped outside every window gets.
    source.connect_drag_cancel(glib::clone!(
        #[weak]
        pane,
        #[upgrade_or]
        false,
        move |_, _, reason| {
            if reason != gdk::DragCancelReason::NoTarget {
                return false;
            }
            dropped_on_nothing(&pane);
            true
        }
    ));
    pane.header().add_controller(source);
}

/// A pane dragged and dropped on nothing that takes it: a window of its own.
/// Its tab is selected first, as the drag may have hovered another tab open:
/// the window's action moves the focused pane of the selected tab.
pub(crate) fn dropped_on_nothing(pane: &Pane) {
    show_pane(pane);
    let _ = pane.activate_action(&format!("win.{TO_WINDOW_ACTION}"), None);
}

/// Select the tab of `pane` and make it the focused pane, with the keyboard:
/// what the window's actions on the focused pane then act on.
fn show_pane(pane: &Pane) {
    if let Some((view, page)) = layout_of(pane).as_ref().and_then(page_of) {
        view.set_selected_page(&page);
    }
    focus_pane(pane.holder());
}

/// The pane dragged in `value`, if it can be dropped on `onto`: another pane,
/// still in a tab (one whose session ended during the drag has gone).
fn dropped_pane(value: &glib::Value, onto: &Pane) -> Option<Pane> {
    let pane = value.get::<Pane>().ok()?;
    (&pane != onto && layout_of(&pane).is_some()).then_some(pane)
}

/// The tab dragged in `value`, if its panes can be dropped on `onto`: a tab
/// that holds panes, not the one `onto` is in.
fn dropped_tab(value: &glib::Value, onto: &Pane) -> Option<adw::TabPage> {
    let page = value.get::<adw::TabPage>().ok()?;
    let layout = layout_of_page(&page)?;
    (!onto.is_ancestor(&layout) && layout.node().is_some()).then_some(page)
}

/// The pane takes a pane dropped on it, beside it on the side nearest the
/// pointer, and shows that side while one is dragged over it. (A tab dropped on
/// a pane is taken by the tab view, [`take_tab_drops`]: libadwaita makes the
/// tabs' contents untargetable while a tab is dragged.)
fn take_drops(pane: &Pane) {
    let target = gtk::DropTarget::new(Pane::static_type(), gdk::DragAction::MOVE);
    target.set_preload(true);
    let motion = glib::clone!(
        #[weak]
        pane,
        #[upgrade_or]
        gdk::DragAction::empty(),
        move |target: &gtk::DropTarget, x: f64, y: f64| {
            let acceptable = target
                .value()
                .is_some_and(|value| dropped_pane(&value, &pane).is_some());
            if !acceptable {
                show_side(&pane, None);
                return gdk::DragAction::empty();
            }
            show_side(
                &pane,
                Some(side_at(
                    x,
                    y,
                    f64::from(pane.width()),
                    f64::from(pane.height()),
                )),
            );
            gdk::DragAction::MOVE
        }
    );
    target.connect_enter(motion.clone());
    target.connect_motion(motion);
    target.connect_leave(glib::clone!(
        #[weak]
        pane,
        move |_| show_side(&pane, None)
    ));
    target.connect_drop(glib::clone!(
        #[weak]
        pane,
        #[upgrade_or]
        false,
        move |_, value, x, y| {
            show_side(&pane, None);
            let side = side_at(x, y, f64::from(pane.width()), f64::from(pane.height()));
            match dropped_pane(value, &pane) {
                Some(dragged) => {
                    move_beside(&dragged, &pane, side);
                    true
                }
                None => false,
            }
        }
    ));
    pane.add_controller(target);
}

/// Let `view` take a tab dropped on one of its panes: the tab's panes go beside
/// that pane, on the side nearest the pointer ([`join_page`]), and the pane
/// shows that side while a tab is dragged over it.
///
/// libadwaita makes the contents of every tab untargetable while a tab is
/// dragged (`AdwTabView:is-transferring-page`), so a pane cannot take the drop
/// itself: the view does, in the capture phase, before libadwaita's own drop
/// target on it (which would move the tab into this view as a tab), and finds
/// the pane under the pointer itself. A tab dropped on a tab bar still moves as
/// a tab. One dropped where no pane is (none should be) is refused, and
/// libadwaita puts it back where it came from.
pub(crate) fn take_tab_drops(view: &adw::TabView) {
    let target = gtk::DropTarget::new(adw::TabPage::static_type(), gdk::DragAction::MOVE);
    target.set_propagation_phase(gtk::PropagationPhase::Capture);
    target.set_preload(true);
    let over: std::rc::Rc<glib::WeakRef<Pane>> = std::rc::Rc::default();
    let show = glib::clone!(
        #[strong]
        over,
        move |pane: Option<&Pane>, side: Option<Side>| {
            if let Some(old) = over.upgrade()
                && Some(&old) != pane
            {
                show_side(&old, None);
            }
            over.set(pane);
            if let Some(pane) = pane {
                show_side(pane, side);
            }
        }
    );
    let motion = glib::clone!(
        #[weak]
        view,
        #[strong]
        show,
        #[upgrade_or]
        gdk::DragAction::empty(),
        move |target: &gtk::DropTarget, x: f64, y: f64| {
            let place = pane_at(&view, x, y);
            let acceptable = place.as_ref().is_some_and(|(pane, _)| {
                target
                    .value()
                    .is_some_and(|value| dropped_tab(&value, pane).is_some())
            });
            match place.filter(|_| acceptable) {
                Some((pane, side)) => {
                    show(Some(&pane), Some(side));
                    gdk::DragAction::MOVE
                }
                None => {
                    show(None, None);
                    gdk::DragAction::empty()
                }
            }
        }
    );
    target.connect_enter(motion.clone());
    target.connect_motion(motion);
    target.connect_leave(glib::clone!(
        #[strong]
        show,
        move |_| show(None, None)
    ));
    target.connect_drop(glib::clone!(
        #[weak]
        view,
        #[strong]
        show,
        #[upgrade_or]
        false,
        move |_, value, x, y| {
            show(None, None);
            let Some((pane, side)) = pane_at(&view, x, y) else {
                return false;
            };
            match dropped_tab(value, &pane) {
                Some(page) => join_page(&page, &pane, side),
                None => false,
            }
        }
    ));
    view.add_controller(target);
}

/// The pane of `view`'s selected tab at the point (`x`, `y`) of the view, and
/// the side of it the point is nearest to. Untargetable widgets count: during a
/// tab drag they all are. On a divider between two panes (a pixel wide), the
/// pane next to it.
fn pane_at(view: &adw::TabView, x: f64, y: f64) -> Option<(Pane, Side)> {
    let pane_under = |x: f64, y: f64| {
        let picked = view.pick(x, y, gtk::PickFlags::NON_TARGETABLE)?;
        match picked.downcast::<Pane>() {
            Ok(pane) => Some(pane),
            Err(widget) => widget.ancestor(Pane::static_type()).and_downcast(),
        }
    };
    const DIVIDER: f64 = 3.0;
    let pane = [
        (0.0, 0.0),
        (-DIVIDER, 0.0),
        (DIVIDER, 0.0),
        (0.0, -DIVIDER),
        (0.0, DIVIDER),
    ]
    .into_iter()
    .find_map(|(dx, dy)| pane_under(x + dx, y + dy))?;
    let point = view.compute_point(&pane, &gtk::graphene::Point::new(x as f32, y as f32))?;
    let side = side_at(
        f64::from(point.x()),
        f64::from(point.y()),
        f64::from(pane.width()),
        f64::from(pane.height()),
    );
    Some((pane, side))
}

/// Show the drop highlight over `side` of `pane` (over its session, below the
/// header), or none. Changed only when the side changes.
fn show_side(pane: &Pane, side: Option<Side>) {
    if pane.side().replace(side) == side {
        return;
    }
    let zone = pane.zone();
    let Some(side) = side else {
        zone.set_visible(false);
        return;
    };
    let room = pane.holder();
    let (width, height) = (room.width(), room.height());
    let (halign, valign, w, h) = match side {
        Side::Left => (gtk::Align::Start, gtk::Align::Fill, width / 2, -1),
        Side::Right => (gtk::Align::End, gtk::Align::Fill, width / 2, -1),
        Side::Top => (gtk::Align::Fill, gtk::Align::Start, -1, height / 2),
        Side::Bottom => (gtk::Align::Fill, gtk::Align::End, -1, height / 2),
    };
    zone.set_halign(halign);
    zone.set_valign(valign);
    zone.set_size_request(w, h);
    zone.set_visible(true);
}

thread_local! {
    /// The tab bars of the main windows ([`setup_tab_bar`]), and those a pane
    /// drag showed, to hide again when it ends. GTK runs on one thread.
    static TAB_BARS: RefCell<Vec<glib::WeakRef<adw::TabBar>>> = const { RefCell::new(Vec::new()) };
    static REVEALED: RefCell<Vec<glib::WeakRef<adw::TabBar>>> = const { RefCell::new(Vec::new()) };
    static CSS_ADDED: Cell<bool> = const { Cell::new(false) };
}

/// Let `bar` take a pane dropped on one of its tabs, which becomes a tab of its
/// own after it, and show it while a pane is dragged.
pub(crate) fn setup_tab_bar(bar: &adw::TabBar) {
    bar.setup_extra_drop_target(gdk::DragAction::MOVE, &[Pane::static_type()]);
    bar.set_extra_drag_preload(true);
    bar.connect_extra_drag_value(|_, _, value| {
        if value.is_some_and(|value| value.get::<Pane>().is_ok()) {
            gdk::DragAction::MOVE
        } else {
            gdk::DragAction::empty()
        }
    });
    bar.connect_extra_drag_drop(|bar, page, value| {
        let (Ok(pane), Some(view)) = (value.get::<Pane>(), bar.view()) else {
            return false;
        };
        move_to_new_page(&pane, &view, view.page_position(page) + 1);
        true
    });
    TAB_BARS.with_borrow_mut(|bars| {
        bars.retain(|bar| bar.upgrade().is_some());
        bars.push(bar.downgrade());
    });
}

/// Show every tab bar while a pane is dragged (`show`), so it can be dropped on
/// a tab; hide again those that were hiding (`!show`).
fn reveal_tab_bars(show: bool) {
    if show {
        let bars: Vec<adw::TabBar> =
            TAB_BARS.with_borrow(|bars| bars.iter().filter_map(glib::WeakRef::upgrade).collect());
        for bar in bars.into_iter().filter(adw::TabBar::is_autohide) {
            bar.set_autohide(false);
            REVEALED.with_borrow_mut(|revealed| revealed.push(bar.downgrade()));
        }
    } else {
        let revealed = REVEALED.with_borrow_mut(std::mem::take);
        for bar in revealed.iter().filter_map(glib::WeakRef::upgrade) {
            bar.set_autohide(true);
        }
    }
}

/// Load the panes' CSS on the default display, once.
fn add_css() {
    if CSS_ADDED.get() {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    CSS_ADDED.set(true);
}

/// A side of a pane: where a pane dropped on it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// The side of a pane `width` wide and `height` high that the point (`x`, `y`)
/// is nearest to: the pane is cut by its diagonals into four triangles, one per
/// side, so every point has a side and there is no dead center. A pane with no
/// size has only its right side.
pub(crate) fn side_at(x: f64, y: f64, width: f64, height: f64) -> Side {
    if !(width > 0.0 && height > 0.0) {
        return Side::Right;
    }
    // How far in from each side, as a share of the pane's size; the first of
    // the nearest wins a tie.
    let across = (x / width).clamp(0.0, 1.0);
    let down = (y / height).clamp(0.0, 1.0);
    [
        (across, Side::Left),
        (1.0 - across, Side::Right),
        (down, Side::Top),
        (1.0 - down, Side::Bottom),
    ]
    .into_iter()
    .fold((f64::INFINITY, Side::Right), |nearest, (distance, side)| {
        if distance < nearest.0 {
            (distance, side)
        } else {
            nearest
        }
    })
    .1
}

/// A direction to go from the focused pane ([`neighbor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Where a pane is in its tab, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bounds {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl Bounds {
    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }
}

/// How far apart two panes may be and still touch: the divider between them,
/// and rounding.
const TOUCHING: f64 = 2.0;

/// Which of `others` is the next pane from `from` going `direction`, if any
/// (`others` may hold `from` itself, which is never the answer).
///
/// Only panes entirely beyond `from`'s edge that way count. Of those, a pane
/// beside it (overlapping it across the direction) beats one that is not; then
/// the nearest wins, then the one that overlaps it most, then the first.
pub(crate) fn neighbor(from: Bounds, others: &[Bounds], direction: Direction) -> Option<usize> {
    // (index, beside, gap, overlap, off-center) for each pane that way.
    let candidates = others.iter().enumerate().filter_map(|(index, other)| {
        if *other == from {
            return None;
        }
        let (gap, low, high, other_low, other_high) = match direction {
            Direction::Left => (
                from.x - other.right(),
                from.y,
                from.bottom(),
                other.y,
                other.bottom(),
            ),
            Direction::Right => (
                other.x - from.right(),
                from.y,
                from.bottom(),
                other.y,
                other.bottom(),
            ),
            Direction::Up => (
                from.y - other.bottom(),
                from.x,
                from.right(),
                other.x,
                other.right(),
            ),
            Direction::Down => (
                other.y - from.bottom(),
                from.x,
                from.right(),
                other.x,
                other.right(),
            ),
        };
        if gap < -TOUCHING {
            return None;
        }
        let overlap = high.min(other_high) - low.max(other_low);
        let off_center = ((low + high) / 2.0 - (other_low + other_high) / 2.0).abs();
        Some((index, overlap > 0.0, gap.max(0.0), overlap, off_center))
    });
    candidates
        .min_by(|a, b| {
            let (_, a_beside, a_gap, a_overlap, a_off) = *a;
            let (_, b_beside, b_gap, b_overlap, b_off) = *b;
            // Beside first; then nearer; then more overlap; then, for panes
            // not beside, nearer across the direction. `min_by` keeps the first
            // of equals.
            b_beside
                .cmp(&a_beside)
                .then(a_gap.total_cmp(&b_gap))
                .then(b_overlap.total_cmp(&a_overlap))
                .then(a_off.total_cmp(&b_off))
        })
        .map(|(index, ..)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f64, y: f64, width: f64, height: f64) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_point_takes_the_side_it_is_nearest_to() {
        // A wide pane: nearness is measured against its size, not in pixels.
        let (w, h) = (800.0, 200.0);
        assert_eq!(side_at(10.0, 100.0, w, h), Side::Left);
        assert_eq!(side_at(790.0, 100.0, w, h), Side::Right);
        assert_eq!(side_at(400.0, 10.0, w, h), Side::Top);
        assert_eq!(side_at(400.0, 190.0, w, h), Side::Bottom);
        // 150 px in from the left is a fifth of the width, but the middle of
        // the height: left, not top or bottom.
        assert_eq!(side_at(150.0, 100.0, w, h), Side::Left);
        // The same point a little higher is nearer the top than the left.
        assert_eq!(side_at(150.0, 30.0, w, h), Side::Top);
    }

    #[test]
    fn the_middle_has_a_side_too() {
        // No dead center: the exact middle and the diagonals still give one.
        assert_eq!(side_at(400.0, 100.0, 800.0, 200.0), Side::Left);
        assert_eq!(side_at(50.0, 50.0, 100.0, 100.0), Side::Left);
        assert_eq!(side_at(90.0, 10.0, 100.0, 100.0), Side::Right);
    }

    #[test]
    fn a_pane_with_no_size_has_only_its_right_side() {
        assert_eq!(side_at(0.0, 0.0, 0.0, 0.0), Side::Right);
        assert_eq!(side_at(5.0, 5.0, 0.0, 300.0), Side::Right);
    }

    /// Three columns, 300 wide, with 1 px dividers.
    fn columns() -> [Bounds; 3] {
        [
            bounds(0.0, 0.0, 300.0, 600.0),
            bounds(301.0, 0.0, 300.0, 600.0),
            bounds(602.0, 0.0, 300.0, 600.0),
        ]
    }

    #[test]
    fn left_and_right_go_to_the_next_column() {
        let c = columns();
        assert_eq!(neighbor(c[1], &c, Direction::Left), Some(0));
        assert_eq!(neighbor(c[1], &c, Direction::Right), Some(2));
        // The next one, not the one beyond it.
        assert_eq!(neighbor(c[0], &c, Direction::Right), Some(1));
        assert_eq!(neighbor(c[2], &c, Direction::Left), Some(1));
    }

    #[test]
    fn there_is_nothing_beyond_the_edge() {
        let c = columns();
        assert_eq!(neighbor(c[0], &c, Direction::Left), None);
        assert_eq!(neighbor(c[2], &c, Direction::Right), None);
        assert_eq!(neighbor(c[1], &c, Direction::Up), None);
        assert_eq!(neighbor(c[1], &c, Direction::Down), None);
    }

    /// Two columns, the right one split top and bottom.
    fn column_and_stack() -> [Bounds; 3] {
        [
            bounds(0.0, 0.0, 400.0, 600.0),
            bounds(401.0, 0.0, 400.0, 300.0),
            bounds(401.0, 301.0, 400.0, 299.0),
        ]
    }

    #[test]
    fn up_and_down_stay_in_the_column() {
        let p = column_and_stack();
        assert_eq!(neighbor(p[1], &p, Direction::Down), Some(2));
        assert_eq!(neighbor(p[2], &p, Direction::Up), Some(1));
        assert_eq!(neighbor(p[0], &p, Direction::Up), None);
        assert_eq!(neighbor(p[0], &p, Direction::Down), None);
    }

    #[test]
    fn going_sideways_takes_the_pane_beside() {
        let p = column_and_stack();
        // Both panes on the right are beside the tall one: the upper one, the
        // first in reading order, wins the tie.
        assert_eq!(neighbor(p[0], &p, Direction::Right), Some(1));
        // From either of them, the tall one.
        assert_eq!(neighbor(p[1], &p, Direction::Left), Some(0));
        assert_eq!(neighbor(p[2], &p, Direction::Left), Some(0));
    }

    #[test]
    fn a_pane_beside_wins_over_one_that_is_not() {
        // A short pane at the bottom left, and on the right a short pane at the
        // top and a tall one below it: going right from the bottom left pane
        // reaches the tall one, which is beside it.
        let p = [
            bounds(0.0, 400.0, 400.0, 200.0),
            bounds(401.0, 0.0, 400.0, 100.0),
            bounds(401.0, 101.0, 400.0, 499.0),
        ];
        assert_eq!(neighbor(p[0], &p, Direction::Right), Some(2));
    }

    #[test]
    fn with_nothing_beside_the_nearest_pane_that_way_is_taken() {
        // Nothing on the right overlaps the pane vertically: the nearest pane
        // on the right still is the next one.
        let p = [
            bounds(0.0, 0.0, 400.0, 100.0),
            bounds(401.0, 300.0, 400.0, 100.0),
            bounds(401.0, 500.0, 400.0, 100.0),
        ];
        assert_eq!(neighbor(p[0], &p, Direction::Right), Some(1));
    }

    #[test]
    fn a_pane_alone_has_no_neighbor() {
        let one = [bounds(0.0, 0.0, 800.0, 600.0)];
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(neighbor(one[0], &one, direction), None);
        }
    }
}
