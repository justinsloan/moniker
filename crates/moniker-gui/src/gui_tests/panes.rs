//! Panes (issue #6): splitting a tab, the focused pane that the tab, the window
//! and the actions follow, closing a pane, and moving panes to other panes,
//! tabs and windows, a whole tab dropped on a pane included.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::secrets::{MemorySecretStore, SecretStore, password_key};
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{ctx_with, ctx_with_secrets, test_app};
use super::windows::{main_window, row, zoom_buttons};
use super::{
    HeldKeyring, all_widgets, fake_shell, fake_ssh, files_in, find_widget, pane, profile, pump_for,
    pump_until, wait_for_text,
};
use crate::panes::{self, Pane, Side, TabLayout};
use crate::terminal_tab::{
    self, PaneHolder, TranscriptSpec, rename_pane, start_shell_in, terminal_in,
};
use crate::{keyring, new_tab, window, zoom};

/// Run the window action `name` of `main`, as its accelerator does.
fn act(main: &window::MainWindow, name: &str) {
    WidgetExt::activate_action(&main.window, &format!("win.{name}"), None)
        .unwrap_or_else(|err| panic!("win.{name}: {err}"));
}

/// Whether the window action `name` of `main` is enabled.
fn enabled(main: &window::MainWindow, name: &str) -> bool {
    main.window
        .lookup_action(name)
        .unwrap_or_else(|| panic!("no action {name}"))
        .is_enabled()
}

fn layout_of(page: &adw::TabPage) -> TabLayout {
    panes::layout_of_page(page).expect("a tab made by terminal_tab has a layout")
}

fn pane_of(holder: &PaneHolder) -> Pane {
    panes::pane_of(holder).expect("a holder is in a pane")
}

/// The focused pane of `page`.
fn focused(page: &adw::TabPage) -> PaneHolder {
    layout_of(page).focused_holder().expect("a focused pane")
}

/// The new-tab list in `holder`, if it still shows one.
fn list_in_pane(holder: &PaneHolder) -> Option<gtk::ListBox> {
    find_widget::<gtk::ListBox>(holder).filter(|l| l.widget_name() == new_tab::LIST_NAME)
}

/// Start a fake local shell running `body` in `holder`'s pane (which shows the
/// new-tab list or nothing yet).
fn shell_in(holder: &PaneHolder, bin: &Path, dir: &Path, name: &str, body: &str) -> Terminal {
    start_shell_in(holder, &fake_shell(bin, name, body), dir, None)
}

/// The split `layout` holds at the top, and its two children.
fn top_split(layout: &TabLayout) -> (gtk::Paned, gtk::Widget, gtk::Widget) {
    let split = layout
        .first_child()
        .and_downcast::<gtk::Paned>()
        .expect("a split at the top");
    let start = split.start_child().expect("a first child");
    let end = split.end_child().expect("a second child");
    (split, start, end)
}

/// Every split below `root` has both its children: none is left half empty.
fn assert_no_empty_split(root: &impl IsA<gtk::Widget>) {
    for split in all_widgets::<gtk::Paned>(root) {
        assert!(
            split.start_child().is_some() && split.end_child().is_some(),
            "a split with a child missing"
        );
    }
}

/// Splitting: Ctrl+Shift+R puts a new pane on the right of the focused one and
/// Ctrl+Shift+D one below it, each showing the new-tab list, with the focus,
/// at the text size of the pane it came from and half its room; the headers
/// show once there are two panes; what is picked in a pane's list starts there.
pub(super) fn splitting_puts_a_new_pane_beside_the_focused_one(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    crate::set_accels(&app);
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let first = pane(&page);

    // One pane: no header, nothing to go to, but splitting works.
    assert!(!pane_of(&first).header().is_visible(), "no header alone");
    assert!(enabled(&main, panes::SPLIT_RIGHT_ACTION));
    assert!(enabled(&main, panes::SPLIT_DOWN_ACTION));
    for name in [
        panes::PANE_LEFT_ACTION,
        panes::PANE_RIGHT_ACTION,
        panes::PANE_UP_ACTION,
        panes::PANE_DOWN_ACTION,
        panes::TO_TAB_ACTION,
        panes::TO_WINDOW_ACTION,
    ] {
        assert!(
            !enabled(&main, name),
            "{name} is off in a tab with one pane (its keys stay VTE's)"
        );
    }
    let width = layout_of(&page).width();

    // Zoomed first, so the new pane's size is seen to come from it.
    act(&main, "zoom-in");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let layout = layout_of(&page);
    assert_eq!(layout.panes().len(), 2);
    let second = focused(&page);
    assert_ne!(second, first, "the new pane has the focus");
    assert!(list_in_pane(&second).is_some(), "it shows the new-tab list");
    assert_eq!(
        second.scale().get(),
        first.scale().get(),
        "the same text size"
    );
    let (split, start, end) = top_split(&layout);
    assert_eq!(split.orientation(), gtk::Orientation::Horizontal);
    assert_eq!(
        start,
        pane_of(&first).upcast::<gtk::Widget>(),
        "the old pane on the left"
    );
    assert_eq!(
        end,
        pane_of(&second).upcast::<gtk::Widget>(),
        "the new one on the right"
    );
    assert!(
        (split.position() - width / 2).abs() <= 2,
        "half the room each: {} of {width}",
        split.position()
    );
    for holder in [&first, &second] {
        assert!(
            pane_of(holder).header().is_visible(),
            "headers with two panes"
        );
    }
    assert!(
        pane_of(&second)
            .header()
            .has_css_class(panes::FOCUSED_CLASS)
    );
    assert!(!pane_of(&first).header().has_css_class(panes::FOCUSED_CLASS));
    assert_eq!(
        page.title(),
        panes::NEW_PANE_TITLE,
        "the tab is titled after the focused pane"
    );
    assert!(enabled(&main, panes::PANE_LEFT_ACTION));
    pump_until("the new pane's list to take the keyboard", || {
        list_in_pane(&second).is_some_and(|list| {
            list.root()
                .and_then(|root| root.focus())
                .is_some_and(|focus| focus.is_ancestor(&list))
        })
    });

    // Down from the second pane: a split one above the other in its place.
    let two = shell_in(
        &second,
        bin,
        dir,
        "pane-two",
        "echo PANE-TWO\nexec sleep 600",
    );
    wait_for_text(&two, "PANE-TWO");
    act(&main, panes::SPLIT_DOWN_ACTION);
    let third = focused(&page);
    assert_eq!(
        layout.holders(),
        [first.clone(), second.clone(), third.clone()]
    );
    let (_, start, end) = top_split(&layout);
    assert_eq!(start, pane_of(&first).upcast::<gtk::Widget>());
    let column = end.downcast::<gtk::Paned>().expect("a split on the right");
    assert_eq!(column.orientation(), gtk::Orientation::Vertical);
    assert_eq!(
        column.start_child(),
        Some(pane_of(&second).upcast()),
        "the old pane above"
    );
    assert_eq!(
        column.end_child(),
        Some(pane_of(&third).upcast()),
        "the new one below"
    );
    assert_eq!(main.view.n_pages(), 1, "still one tab");

    // Picking Local terminal in the third pane's list starts it there.
    let list = list_in_pane(&third).expect("the third pane's list");
    list.emit_by_name::<()>("row-activated", &[&row(&list, new_tab::LOCAL_ROW)]);
    pump_until("a shell in the third pane", || {
        terminal_in(&third).is_some()
    });
    assert_eq!(main.view.n_pages(), 1, "in the pane, not a new tab");

    // The text size is the focused pane's own.
    panes::focus_pane(&first);
    act(&main, "zoom-reset");
    assert_eq!(first.scale().get(), zoom::DEFAULT);
    assert_eq!(
        second.scale().get(),
        zoom::larger(zoom::DEFAULT),
        "the others keep theirs"
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The focused pane: the tab, the window title and the ☰ menu's text size
/// follow it; Ctrl+Shift+arrows go to the pane that way (and nowhere at an
/// edge); a click in a pane's terminal makes it the focused one; the header of
/// the focused pane is marked; the tab's tooltip lists every pane.
pub(super) fn the_tab_follows_its_focused_pane(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let first = pane(&page);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let second = focused(&page);
    let two = shell_in(&second, bin, dir, "focus-two", "echo TWO\nexec sleep 600");
    act(&main, panes::SPLIT_DOWN_ACTION);
    let third = focused(&page);
    let three = shell_in(
        &third,
        bin,
        dir,
        "focus-three",
        "echo THREE\nexec sleep 600",
    );
    wait_for_text(&two, "TWO");
    wait_for_text(&three, "THREE");
    rename_pane(&first, "one");
    rename_pane(&second, "two");
    rename_pane(&third, "three");
    assert_eq!(page.tooltip().as_deref(), Some("one\ntwo\nthree"));

    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    panes::focus_pane(&first);
    assert_eq!(page.title(), "one");
    assert_eq!(title.title(), "one");
    assert_eq!(pane_of(&first).title().text(), "one");

    // From the tall pane on the left, right goes to the upper of the two.
    act(&main, panes::PANE_RIGHT_ACTION);
    assert_eq!(focused(&page), second);
    pump_until("the second pane's terminal to have the keyboard", || {
        two.has_focus()
    });
    assert_eq!(page.title(), "two");
    assert_eq!(title.title(), "two");
    assert!(
        pane_of(&second)
            .header()
            .has_css_class(panes::FOCUSED_CLASS)
    );
    assert!(pane_of(&first).title().has_css_class("dim-label"));
    act(&main, panes::PANE_DOWN_ACTION);
    assert_eq!(focused(&page), third);
    act(&main, panes::PANE_DOWN_ACTION);
    assert_eq!(focused(&page), third, "nothing below: it stays");
    act(&main, panes::PANE_UP_ACTION);
    assert_eq!(focused(&page), second);
    act(&main, panes::PANE_LEFT_ACTION);
    assert_eq!(focused(&page), first);
    act(&main, panes::PANE_LEFT_ACTION);
    assert_eq!(focused(&page), first, "nothing on the left: it stays");

    // A click in a terminal (VTE takes the focus) makes its pane the focused one.
    three.grab_focus();
    pump_until("the third pane to be the focused one", || {
        focused(&page) == third
    });
    assert_eq!(page.title(), "three");

    // The ☰ menu's text size is the focused pane's.
    act(&main, "zoom-in");
    let (_, percent, _) = zoom_buttons(&main);
    assert_eq!(
        percent.label().as_deref(),
        Some(zoom::percent(zoom::larger(zoom::DEFAULT)).as_str())
    );
    panes::focus_pane(&second);
    assert_eq!(
        percent.label().as_deref(),
        Some(zoom::percent(zoom::DEFAULT).as_str())
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Closing a pane: its transcript is written and its shell hung up, the pane
/// beside it takes its room (no split is left half empty) and the focus; a
/// shell that exits closes only its pane; the last pane closes the tab.
pub(super) fn closing_a_pane_gives_its_room_to_the_others(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let first = pane(&page);
    let transcripts = dir.join("transcripts");
    let hup = dir.join("hung-up");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let second = focused(&page);
    let two = start_shell_in(
        &second,
        &fake_shell(
            bin,
            "close-two",
            &format!(
                "trap 'touch {}; exit 0' HUP\necho CLOSE-TWO\nwhile :; do sleep 0.1; done",
                hup.display()
            ),
        ),
        dir,
        Some(TranscriptSpec {
            host: "two.example.test".to_owned(),
            dir: transcripts.clone(),
            notice: ctx.notice.clone(),
        }),
    );
    act(&main, panes::SPLIT_DOWN_ACTION);
    let third = focused(&page);
    let three = shell_in(
        &third,
        bin,
        dir,
        "close-three",
        "echo CLOSE-THREE\nread x\nexit 0",
    );
    wait_for_text(&two, "CLOSE-TWO");
    wait_for_text(&three, "CLOSE-THREE");

    // Closing the second (above the third): the third takes the right column.
    panes::focus_pane(&second);
    act(&main, panes::CLOSE_PANE_ACTION);
    let layout = layout_of(&page);
    assert_eq!(layout.holders(), [first.clone(), third.clone()]);
    assert_no_empty_split(&layout);
    let (split, _, end) = top_split(&layout);
    assert_eq!(split.orientation(), gtk::Orientation::Horizontal);
    assert_eq!(
        end,
        pane_of(&third).upcast::<gtk::Widget>(),
        "the third took its place"
    );
    assert_eq!(focused(&page), third, "and the focus");
    pump_until("the third pane's terminal to have the keyboard", || {
        three.has_focus()
    });
    pump_until("the closed pane's shell to be hung up", || hup.exists());
    assert_eq!(files_in(&transcripts).len(), 1, "its transcript, once");
    assert!(two.parent().is_none(), "its terminal is out of the window");

    // The third's shell exits: only its pane closes; the tab stays, alone again,
    // and the first pane, which takes its room, has the keyboard. (Not the
    // widget GTK would move the focus to on its own, the next one after the
    // terminal that went.)
    three.feed_child(b"\n");
    pump_until("the third pane to close", || layout.panes().len() == 1);
    let one = terminal_in(&first).expect("the launch shell");
    pump_until("the first pane's terminal to have the keyboard", || {
        one.has_focus()
    });
    assert_eq!(main.view.n_pages(), 1);
    assert_eq!(layout.holders(), std::slice::from_ref(&first));
    assert!(
        !pane_of(&first).header().is_visible(),
        "alone again: no header"
    );
    assert!(!enabled(&main, panes::PANE_LEFT_ACTION));
    assert_no_empty_split(&layout);

    // The last pane is the tab: closing it closes the tab (and the window).
    act(&main, panes::CLOSE_PANE_ACTION);
    pump_until("the window to close with its last tab", || {
        !main.window.is_visible()
    });
    assert_eq!(files_in(&transcripts).len(), 1, "nothing more written");
    pump_for(Duration::from_millis(200));
}

/// Moving panes: beside another pane of the same tab, to a tab of its own
/// (the header menu's Move to New Tab, and a drop on a tab of the tab bar),
/// beside a pane in another window, and to a window of its own. The session
/// goes along, running; a tab left with no pane goes; and a whole tab dropped
/// on a pane brings its panes as they were laid out.
pub(super) fn panes_move_between_tabs_and_windows(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let one = main_window(&app, &ctx);
    let a = one.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    act(&one, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    let echo = "echo READY\nwhile read line; do echo \"GOT:$line\"; done";
    let t2 = shell_in(&a2, bin, dir, "move-two", echo);
    act(&one, panes::SPLIT_RIGHT_ACTION);
    let a3 = focused(&a);
    let t3 = shell_in(&a3, bin, dir, "move-three", echo);
    wait_for_text(&t2, "READY");
    wait_for_text(&t3, "READY");

    // Beside a pane of the same tab: the third goes to the left of the first.
    panes::move_beside(&pane_of(&a3), &pane_of(&a1), Side::Left);
    let layout = layout_of(&a);
    assert_eq!(layout.holders(), [a3.clone(), a1.clone(), a2.clone()]);
    assert_no_empty_split(&layout);
    assert_eq!(focused(&a), a3, "the moved pane has the focus");
    assert!(
        terminal_in(&a3).as_ref() == Some(&t3),
        "with its own terminal"
    );
    t3.feed_child(b"moved\n");
    wait_for_text(&t3, "GOT:moved");

    // Move to New Tab: a tab of its own right after this one, selected.
    act(&one, panes::TO_TAB_ACTION);
    assert_eq!(one.view.n_pages(), 2);
    let b = one.view.selected_page().expect("the new tab");
    assert_eq!(one.view.page_position(&b), 1, "right after the tab it left");
    assert_eq!(layout_of(&b).holders(), std::slice::from_ref(&a3));
    assert!(
        !pane_of(&a3).header().is_visible(),
        "alone in its tab: no header"
    );
    assert_eq!(layout.holders(), [a1.clone(), a2.clone()]);
    assert_eq!(b.title(), terminal_tab::shown_title(&a3));

    // A pane dropped on a tab of the tab bar does the same, after that tab (as
    // the bar's extra drop calls it).
    panes::move_to_new_page(&pane_of(&a2), &one.view, one.view.page_position(&b) + 1);
    assert_eq!(one.view.n_pages(), 3);
    let c = one.view.selected_page().expect("the tab made by the drop");
    assert_eq!(one.view.page_position(&c), 2);
    assert_eq!(layout_of(&c).holders(), std::slice::from_ref(&a2));
    assert_eq!(layout.holders(), std::slice::from_ref(&a1));
    t2.feed_child(b"still here\n");
    wait_for_text(&t2, "GOT:still here");

    // Beside a pane in another window: the tab it leaves, now empty, goes.
    let two = main_window(&app, &ctx);
    let d = two.view.selected_page().expect("the other window's tab");
    let d1 = pane(&d);
    panes::move_beside(&pane_of(&a2), &pane_of(&d1), Side::Bottom);
    pump_until("the emptied tab to close", || one.view.n_pages() == 2);
    assert_eq!(layout_of(&d).holders(), [d1.clone(), a2.clone()]);
    assert_eq!(
        top_split(&layout_of(&d)).0.orientation(),
        gtk::Orientation::Vertical
    );
    assert_eq!(two.view.selected_page().as_ref(), Some(&d));
    t2.feed_child(b"other window\n");
    wait_for_text(&t2, "GOT:other window");

    // To a window of its own.
    let windows = app.windows().len();
    panes::focus_pane(&a2);
    act(&two, panes::TO_WINDOW_ACTION);
    pump_until("a new window", || app.windows().len() == windows + 1);
    assert_eq!(layout_of(&d).holders(), std::slice::from_ref(&d1));
    let in_a2 = |view: &adw::TabView| {
        view.selected_page()
            .is_some_and(|p| layout_of(&p).holders() == std::slice::from_ref(&a2))
    };
    let three = app
        .windows()
        .into_iter()
        .find(|w| find_widget::<adw::TabView>(w).is_some_and(|view| in_a2(&view)))
        .expect("the window the pane went to");
    let three_view: adw::TabView = find_widget(&three).expect("its tabs");
    assert_eq!(three_view.n_pages(), 1);
    t2.feed_child(b"own window\n");
    wait_for_text(&t2, "GOT:own window");

    // A tab dropped on a pane: its two panes, side by side as they were, go
    // below the pane; the tab goes.
    let e = new_tab::open(&one.view, &ctx);
    let e1 = pane(&e);
    let te1 = shell_in(&e1, bin, dir, "join-one", echo);
    act(&one, panes::SPLIT_RIGHT_ACTION);
    let e2 = focused(&e);
    let te2 = shell_in(&e2, bin, dir, "join-two", echo);
    wait_for_text(&te1, "READY");
    wait_for_text(&te2, "READY");
    let tabs = one.view.n_pages();
    assert!(panes::join_page(&e, &pane_of(&d1), Side::Bottom));
    pump_until("the joined tab to close", || one.view.n_pages() == tabs - 1);
    let joined = layout_of(&d);
    assert_eq!(joined.holders(), [d1.clone(), e1.clone(), e2.clone()]);
    let (split, _, below) = top_split(&joined);
    assert_eq!(split.orientation(), gtk::Orientation::Vertical);
    let row_of_two = below.downcast::<gtk::Paned>().expect("the tab's own split");
    assert_eq!(
        row_of_two.orientation(),
        gtk::Orientation::Horizontal,
        "as it was"
    );
    assert_eq!(focused(&d), e2, "the tab's focused pane has the focus");
    te1.feed_child(b"joined\n");
    wait_for_text(&te1, "GOT:joined");
    assert!(
        !panes::claims_new_window(),
        "the tab was in a view: no window to decline"
    );

    for window in [one.window.upcast(), two.window.upcast(), three] {
        window.destroy();
    }
    pump_for(Duration::from_millis(200));
}

/// A tab that is selected again gives the keyboard to its own focused pane,
/// not to the widget it had the focus on when it was left: that pane may
/// have moved to another tab meanwhile, where keys typed would reach a
/// session out of sight.
pub(super) fn a_tab_never_gives_the_focus_to_a_pane_that_left_it(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let x = pane(&a);
    let tx = terminal_in(&x).expect("the launch shell");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let y = focused(&a);
    let ty = shell_in(&y, bin, dir, "left-y", "echo LEFT-Y\nexec sleep 600");
    wait_for_text(&ty, "LEFT-Y");
    panes::focus_pane(&y);
    pump_until("the second pane to have the keyboard", || ty.has_focus());

    // Another tab: the first remembers the second pane's terminal.
    let b = new_tab::open(&main.view, &ctx);
    let b1 = pane(&b);
    let tb = shell_in(&b1, bin, dir, "left-b", "echo LEFT-B\nexec sleep 600");
    wait_for_text(&tb, "LEFT-B");
    // The second pane moves into this tab.
    panes::move_beside(&pane_of(&y), &pane_of(&b1), Side::Right);
    assert_eq!(layout_of(&b).holders(), [b1.clone(), y.clone()]);
    pump_until("the moved pane to have the keyboard", || ty.has_focus());

    // Back to the first tab: the keyboard goes to its own pane.
    main.view.set_selected_page(&a);
    pump_until("the first tab's pane to have the keyboard", || {
        tx.has_focus()
    });
    assert!(
        !ty.has_focus(),
        "the keyboard went to a pane in a hidden tab"
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The terminal's right-click menu acts on its own pane, which a right click
/// does not focus: Paste pastes there, not into the focused pane, and Split
/// Right splits it.
pub(super) fn the_terminal_menu_acts_on_its_own_pane(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let first = pane(&page);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let second = focused(&page);
    let cat = "echo READY\nexec cat";
    let t2 = shell_in(&second, bin, dir, "menu-two", cat);
    wait_for_text(&t2, "READY");
    let t1 = terminal_in(&first).expect("the launch shell");
    panes::focus_pane(&first);

    gdk::Display::default()
        .expect("a display")
        .clipboard()
        .set_text("PASTED-IN-TWO");
    WidgetExt::activate_action(&t2, "term.paste", None).expect("term.paste");
    wait_for_text(&t2, "PASTED-IN-TWO");
    pump_for(Duration::from_millis(300));
    assert!(
        !terminal_tab::terminal_text(&t1).contains("PASTED-IN-TWO"),
        "the paste went to the focused pane"
    );

    WidgetExt::activate_action(&t2, "pane.split-right", None).expect("pane.split-right");
    let layout = layout_of(&page);
    // Once the menu would have handed the keyboard back.
    pump_until("the split", || layout.panes().len() == 3);
    let new = focused(&page);
    assert_eq!(
        layout.holders(),
        [first.clone(), second.clone(), new.clone()]
    );
    let (_, _, end) = top_split(&layout);
    let split = end.downcast::<gtk::Paned>().expect("the second pane split");
    assert_eq!(split.start_child(), Some(pane_of(&second).upcast()));
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A session that waited for the keyring starts in its pane without taking
/// the keyboard from the pane in use (whose keys would then go to this host);
/// it has the focus once its pane is the focused one.
pub(super) fn a_late_session_leaves_the_focus_in_the_pane_in_use(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "pane-keyring", "echo PANE-END\nexec sleep 600");
    let web = profile(&[]);
    let seeded = Arc::new(MemorySecretStore::default());
    seeded
        .set(&password_key(&web.id), "pw")
        .expect("seed the password");
    let keyring = Arc::new(HeldKeyring::new(seeded));
    let ctx = ctx_with_secrets(dir, std::slice::from_ref(&web), Arc::clone(&keyring) as _);
    let app = test_app();
    let main = main_window(&app, &ctx);
    terminal_tab::set_test_ssh_program(Some(fake));
    let page = main.view.selected_page().expect("the launch tab");
    let first = pane(&page);
    let t1 = terminal_in(&first).expect("the launch shell");

    keyring.hold();
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let second = focused(&page);
    let list = list_in_pane(&second).expect("the new pane's list");
    list.emit_by_name::<()>("row-activated", &[&row(&list, &web.id)]);
    pump_for(Duration::from_millis(100));
    assert!(terminal_in(&second).is_none(), "it waits for the keyring");
    // Going to the waiting pane takes the keyboard from the other: the keys
    // and the pane in use never part ways.
    panes::focus_pane(&first);
    pump_until("the first pane's shell to have the focus", || {
        t1.has_focus()
    });
    panes::focus_pane(&second);
    assert_eq!(focused(&page), second);
    assert!(!t1.has_focus(), "the keyboard stayed in another pane");
    panes::focus_pane(&first);
    pump_until("the first pane's shell to have the focus", || {
        t1.has_focus()
    });
    keyring.release();
    pump_until("the session in the second pane", || {
        terminal_in(&second).is_some()
    });
    pump_until("the keyring calls to come back", || keyring::pending() == 0);
    let t2 = terminal_in(&second).expect("the session");
    wait_for_text(&t2, "PANE-END");
    assert!(
        t1.has_focus(),
        "the late session took the focus from the pane in use"
    );
    assert_eq!(focused(&page), first);
    panes::focus_pane(&second);
    pump_until("the session to have the focus", || t2.has_focus());
    terminal_tab::set_test_ssh_program(None);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The header's menu and close button act on their own pane; the header's
/// title is plain text and a double click on it renames the pane ("Rename
/// Pane", as the tab's Rename becomes with several panes).
pub(super) fn the_header_acts_on_its_own_pane(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let first = pane(&page);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let second = focused(&page);
    let t2 = shell_in(
        &second,
        bin,
        dir,
        "header-two",
        "echo HEADER\nexec sleep 600",
    );
    wait_for_text(&t2, "HEADER");
    panes::focus_pane(&second);

    // The first pane's menu: its entries are the pane's own actions.
    let menu = all_widgets::<gtk::MenuButton>(pane_of(&first).header())
        .into_iter()
        .next()
        .expect("the header's menu button");
    let model = menu.menu_model().expect("a menu");
    let items = super::main_window::menu_items(&model);
    assert_eq!(
        items,
        [
            ("Rename\u{2026}".to_owned(), "pane.rename".to_owned()),
            ("Move to New Tab".to_owned(), "pane.pane-to-tab".to_owned()),
            (
                "Move to New Window".to_owned(),
                "pane.pane-to-window".to_owned()
            ),
        ]
    );
    WidgetExt::activate_action(pane_of(&first).header(), "pane.pane-to-tab", None)
        .expect("pane.pane-to-tab");
    pump_until("the pane's own tab", || main.view.n_pages() == 2);
    assert_eq!(
        main.view.n_pages(),
        2,
        "the first pane went to a tab of its own"
    );
    assert_eq!(layout_of(&page).holders(), std::slice::from_ref(&second));
    let moved = main.view.selected_page().expect("its tab");
    assert_eq!(layout_of(&moved).holders(), std::slice::from_ref(&first));

    // The close button closes its pane, not the focused one.
    main.view.set_selected_page(&page);
    act(&main, panes::SPLIT_DOWN_ACTION);
    let third = focused(&page);
    panes::focus_pane(&third);
    let close = all_widgets::<gtk::Button>(pane_of(&second).header())
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Close Pane"))
        .expect("the close button");
    close.emit_clicked();
    pump_until("the second pane to close", || {
        layout_of(&page).holders() == std::slice::from_ref(&third)
    });

    // Rename Pane with several panes, Rename Tab with one.
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let fourth = focused(&page);
    terminal_tab::ask_rename_pane(&fourth);
    let dialog: adw::AlertDialog = pump_dialog(&main);
    assert_eq!(dialog.heading().as_deref(), Some("Rename Pane"));
    dialog.close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
    main.view.set_selected_page(&moved);
    terminal_tab::ask_rename(&moved);
    let dialog: adw::AlertDialog = pump_dialog(&main);
    assert_eq!(dialog.heading().as_deref(), Some("Rename Tab"));
    dialog.close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The alert dialog shown over `main`, once it shows.
fn pump_dialog(main: &window::MainWindow) -> adw::AlertDialog {
    pump_until("the dialog", || main.window.visible_dialog().is_some());
    main.window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("an alert dialog")
}

/// The drop target of `pane` (a pane dropped on it), as GTK runs it.
fn pane_drop_target(pane: &Pane) -> gtk::DropTarget {
    controllers::<gtk::DropTarget>(pane)
        .into_iter()
        .next()
        .expect("a pane takes drops")
}

/// The event controllers of type `T` on `widget`.
fn controllers<T: IsA<gtk::EventController> + IsA<gtk::glib::Object>>(
    widget: &impl IsA<gtk::Widget>,
) -> Vec<T> {
    let all = widget.observe_controllers();
    (0..all.n_items())
        .filter_map(|i| all.item(i).and_downcast::<T>())
        .collect()
}

/// The point at (`x`, `y`) of `pane`, in the coordinates of `view`.
fn point_in(view: &adw::TabView, pane: &Pane, x: f32, y: f32) -> (f64, f64) {
    let point = pane
        .compute_point(view, &gtk::graphene::Point::new(x, y))
        .expect("the pane is in the view");
    (f64::from(point.x()), f64::from(point.y()))
}

/// A pane that closes in a tab out of sight, or behind a dialog, moves no
/// keyboard focus: the keys typed next still go to the pane in use, not to the
/// session beside the one that went (on another host maybe).
pub(super) fn a_pane_that_closes_out_of_sight_moves_no_keyboard(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    let ends = "echo ENDS\nread x\nexit 0";
    let t2 = shell_in(&a2, bin, dir, "sight-two", ends);
    wait_for_text(&t2, "ENDS");
    panes::focus_pane(&a2);
    pump_until("the second pane to have the keyboard", || t2.has_focus());

    // Another tab, in use; the second pane's shell ends in the hidden one.
    let b = new_tab::open(&main.view, &ctx);
    let tb = shell_in(&pane(&b), bin, dir, "sight-b", "echo IN-USE\nexec cat");
    wait_for_text(&tb, "IN-USE");
    pump_until("the tab in use to have the keyboard", || tb.has_focus());
    t2.feed_child(b"\n");
    pump_until("the pane to close", || layout_of(&a).panes().len() == 1);
    pump_for(Duration::from_millis(300));
    assert!(tb.has_focus(), "the keyboard left the tab in use");
    assert_eq!(main.view.selected_page().as_ref(), Some(&b));
    assert_eq!(focused(&a), a1, "the first pane is the focused one there");

    // Behind a dialog: a pane of the tab in use ends while About is open.
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let b2 = focused(&b);
    let tb2 = shell_in(&b2, bin, dir, "sight-dialog", ends);
    wait_for_text(&tb2, "ENDS");
    panes::focus_pane(&b2);
    pump_until("the new pane to have the keyboard", || tb2.has_focus());
    let about = crate::about_dialog();
    about.present(Some(&main.window));
    pump_until("the About dialog", || {
        main.window.visible_dialog().is_some()
    });
    tb2.feed_child(b"\n");
    pump_until("the pane to close", || layout_of(&b).panes().len() == 1);
    pump_for(Duration::from_millis(300));
    assert!(
        !tb.has_focus(),
        "the keyboard went to the pane behind the dialog"
    );
    about.close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The drops, as GTK hands them over: a pane dropped on a pane goes beside it
/// on that side (onto itself: refused); a tab dropped on a pane of a shown tab
/// (taken by the tab view, whose tabs' contents a tab drag makes untargetable)
/// brings its panes and goes; a pane dropped on a tab of the tab bar becomes a
/// tab after it.
pub(super) fn drops_reach_panes_tabs_and_the_tab_bar(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    shell_in(&a2, bin, dir, "drop-two", "echo DROP-TWO\nexec sleep 600");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a3 = focused(&a);
    shell_in(
        &a3,
        bin,
        dir,
        "drop-three",
        "echo DROP-THREE\nexec sleep 600",
    );
    pump_for(Duration::from_millis(300));

    // The third pane dropped near the left edge of the first.
    let onto = pane_of(&a1);
    let height = f64::from(onto.height());
    let took: bool = pane_drop_target(&onto).emit_by_name(
        "drop",
        &[
            &gtk::glib::BoxedValue(pane_of(&a3).to_value()),
            &2.0f64,
            &(height / 2.0),
        ],
    );
    assert!(took, "the drop is taken");
    assert_eq!(
        layout_of(&a).holders(),
        [a3.clone(), a1.clone(), a2.clone()]
    );
    let took: bool = pane_drop_target(&onto).emit_by_name(
        "drop",
        &[&gtk::glib::BoxedValue(onto.to_value()), &2.0f64, &2.0f64],
    );
    assert!(!took, "a pane dropped on itself is refused");

    // A tab dropped near the right edge of the second pane.
    let e = new_tab::open(&main.view, &ctx);
    let e1 = pane(&e);
    shell_in(&e1, bin, dir, "drop-tab", "echo DROP-TAB\nexec sleep 600");
    main.view.set_selected_page(&a);
    pump_for(Duration::from_millis(300));
    let view_target = controllers::<gtk::DropTarget>(&main.view)
        .into_iter()
        .find(|t| t.propagation_phase() == gtk::PropagationPhase::Capture)
        .expect("the tab view takes tabs dropped on its panes");
    let beside = pane_of(&a2);
    let (x, y) = point_in(
        &main.view,
        &beside,
        beside.width() as f32 - 2.0,
        beside.height() as f32 / 2.0,
    );
    let tabs = main.view.n_pages();
    let took: bool =
        view_target.emit_by_name("drop", &[&gtk::glib::BoxedValue(e.to_value()), &x, &y]);
    assert!(took, "the tab view took the tab");
    pump_until("the tab to go", || main.view.n_pages() == tabs - 1);
    assert_eq!(
        layout_of(&a).holders(),
        [a3.clone(), a1.clone(), a2.clone(), e1.clone()],
        "its pane on the right of the second"
    );

    // A pane dropped on the tab of the tab bar: a tab after it.
    let bar: adw::TabBar = find_widget(&main.window).expect("the tab bar");
    let took: bool = bar.emit_by_name(
        "extra-drag-drop",
        &[&a, &gtk::glib::BoxedValue(pane_of(&a1).to_value())],
    );
    assert!(took, "the tab bar took the pane");
    let made = main.view.selected_page().expect("the pane's own tab");
    assert_eq!(
        main.view.page_position(&made),
        main.view.page_position(&a) + 1
    );
    assert_eq!(layout_of(&made).holders(), std::slice::from_ref(&a1));
    assert_eq!(
        layout_of(&a).holders(),
        [a3.clone(), a2.clone(), e1.clone()]
    );
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A pane dropped on nothing gets a window of its own: that very pane, even
/// when the drag hovered another tab open meanwhile.
pub(super) fn a_pane_dropped_on_nothing_gets_its_own_window(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    let t2 = shell_in(&a2, bin, dir, "nothing-two", "echo NOTHING\nexec sleep 600");
    wait_for_text(&t2, "NOTHING");
    // The drag hovered another tab open: a split one, whose focused pane is not
    // the dragged one.
    let b = new_tab::open(&main.view, &ctx);
    shell_in(&pane(&b), bin, dir, "nothing-b", "echo B\nexec sleep 600");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let b_panes = layout_of(&b).holders();
    let windows = app.windows().len();
    panes::dropped_on_nothing(&pane_of(&a2));
    pump_until("a new window", || app.windows().len() == windows + 1);
    assert_eq!(layout_of(&a).holders(), std::slice::from_ref(&a1));
    assert_eq!(
        layout_of(&b).holders(),
        b_panes,
        "the other tab keeps its panes"
    );
    let own = app
        .windows()
        .into_iter()
        .find(|w| {
            find_widget::<adw::TabView>(w).is_some_and(|view| {
                view.selected_page()
                    .is_some_and(|p| layout_of(&p).holders() == std::slice::from_ref(&a2))
            })
        })
        .expect("the dragged pane's window");
    own.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A pane dropped outside every window on GNOME's Wayland: the drag offers the
/// type the compositor takes such a drop with, so the drag finishes instead of
/// being cancelled, and the pane gets a window of its own unless a pane or a
/// tab bar of Moniker took it.
pub(super) fn a_pane_dropped_outside_every_window_gets_its_own_window(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    let t2 = shell_in(&a2, bin, dir, "outside-two", "echo OUTSIDE\nexec sleep 600");
    wait_for_text(&t2, "OUTSIDE");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a3 = focused(&a);
    let t3 = shell_in(&a3, bin, dir, "outside-three", "echo AWAY\nexec sleep 600");
    wait_for_text(&t3, "AWAY");

    let source = controllers::<gtk::DragSource>(pane_of(&a3).header())
        .into_iter()
        .next()
        .expect("a pane is dragged by its header");
    let content: Option<gdk::ContentProvider> = source.emit_by_name("prepare", &[&5.0f64, &5.0f64]);
    let formats = content.expect("a pane among others drags").formats();
    assert!(
        formats.contain_mime_type(panes::ROOT_DROP_MIME),
        "the drag offers what the compositor takes a drop outside every window with"
    );
    assert!(formats.contains_type(Pane::static_type()), "and the pane");

    // Taken by nothing of Moniker: a window of its own.
    let windows = app.windows().len();
    panes::pane_drag_began();
    panes::pane_drag_finished(&pane_of(&a3));
    pump_until("a new window", || app.windows().len() == windows + 1);
    assert_eq!(layout_of(&a).holders(), [a1.clone(), a2.clone()]);
    let own = app
        .windows()
        .into_iter()
        .find(|w| {
            find_widget::<adw::TabView>(w).is_some_and(|view| {
                view.selected_page()
                    .is_some_and(|p| layout_of(&p).holders() == std::slice::from_ref(&a3))
            })
        })
        .expect("the dragged pane's window");
    own.destroy();
    pump_for(Duration::from_millis(200));
    let windows = app.windows().len();

    // Taken by a pane: no window.
    panes::pane_drag_began();
    let onto = pane_of(&a1);
    let height = f64::from(onto.height());
    let took: bool = pane_drop_target(&onto).emit_by_name(
        "drop",
        &[
            &gtk::glib::BoxedValue(pane_of(&a2).to_value()),
            &2.0f64,
            &(height / 2.0),
        ],
    );
    assert!(took, "the pane took it");
    panes::pane_drag_finished(&pane_of(&a2));
    pump_for(Duration::from_millis(300));
    assert_eq!(
        app.windows().len(),
        windows,
        "no window for a pane a pane took"
    );
    assert_eq!(layout_of(&a).holders(), [a2.clone(), a1.clone()]);

    // Taken by a tab bar: no window either. (The pane is then alone in its
    // tab, which the window's action would refuse to move anyway, so this
    // part passes without the mark too; it stays for a drop that might not.)
    panes::pane_drag_began();
    let bar: adw::TabBar = find_widget(&main.window).expect("the tab bar");
    let took: bool = bar.emit_by_name(
        "extra-drag-drop",
        &[&a, &gtk::glib::BoxedValue(pane_of(&a1).to_value())],
    );
    assert!(took, "the tab bar took it");
    panes::pane_drag_finished(&pane_of(&a1));
    pump_for(Duration::from_millis(300));
    assert_eq!(
        app.windows().len(),
        windows,
        "no window for a pane a tab bar took"
    );
    assert_eq!(main.view.n_pages(), 2, "a tab of its own");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The new-tab list shown in a pane of a split tab takes the keyboard only as
/// the pane in use: not each time its tab is shown again while another pane is
/// the one in use, nor when panes move and it is left alone in a tab out of
/// sight.
pub(super) fn a_list_in_a_split_tab_leaves_the_keyboard_alone(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    let t1 = terminal_in(&a1).expect("the launch shell");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    assert!(list_in_pane(&a2).is_some());
    pump_for(Duration::from_millis(300));
    panes::focus_pane(&a1);
    pump_until("the first pane's shell to have the keyboard", || {
        t1.has_focus()
    });

    // Away and back: the shell keeps the keyboard.
    let b = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(300));
    main.view.set_selected_page(&a);
    pump_until("the shell to have the keyboard again", || t1.has_focus());
    pump_for(Duration::from_millis(300));
    assert!(t1.has_focus(), "the list took the keyboard");
    assert_eq!(focused(&a), a1);

    // The shell moves to a tab of its own; the list, alone in a tab out of
    // sight now, does not take the keyboard.
    act(&main, panes::TO_TAB_ACTION);
    pump_until("the shell to have the keyboard in its tab", || {
        t1.has_focus()
    });
    pump_for(Duration::from_millis(300));
    assert!(
        t1.has_focus(),
        "the list in the tab out of sight took the keyboard"
    );
    let _ = b;

    // Clicking a pane (its header, say) makes it the one in use.
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let c = main.view.selected_page().expect("the shell's tab");
    let c2 = focused(&c);
    panes::focus_pane(&a1);
    pump_until("the shell to have the keyboard", || t1.has_focus());
    let click = controllers::<gtk::GestureClick>(&pane_of(&c2))
        .into_iter()
        .find(|g| g.propagation_phase() == gtk::PropagationPhase::Capture)
        .expect("a pane follows clicks");
    click.emit_by_name::<()>("pressed", &[&1i32, &5.0f64, &5.0f64]);
    assert_eq!(focused(&c), c2, "the clicked pane is the one in use");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A tab whose panes a drop on a pane took comes back, empty, as libadwaita
/// puts it back after the drop: at the first place, selected (it is attached
/// here the way libadwaita does it, by a transfer from another window). It
/// goes, the tab that was in use is shown again, not the one beside the place
/// it came back to, and its pane has the keyboard.
pub(super) fn a_tab_whose_panes_were_taken_goes_quietly(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let t1 = terminal_in(&pane(&a)).expect("the launch shell");
    new_tab::open(&main.view, &ctx);
    new_tab::open(&main.view, &ctx);
    // [B, A, C], A in use.
    main.view.reorder_page(&a, 1);
    main.view.set_selected_page(&a);
    panes::focus_pane(&pane(&a));
    pump_until("the shell to have the keyboard", || t1.has_focus());

    let other = main_window(&app, &ctx);
    let emptied = panes::empty_page_for_tests(&other.view);
    panes::expect_back_for_tests(&emptied);
    other.view.transfer_page(&emptied, &main.view, 0);
    pump_until("the emptied tab to go", || main.view.n_pages() == 3);
    assert_eq!(
        main.view.selected_page().as_ref(),
        Some(&a),
        "the tab in use is shown again"
    );
    // The other window is the active one now: the focus inside this one.
    wait_for_focus(&main.window, &t1);
    other.window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The × of a pane not in use closes it and leaves the keyboard in the pane in
/// use (the press on the button does not make its pane the one in use first).
/// A keyboard focus put in a pane of a tab out of sight is sent back to the tab
/// shown.
pub(super) fn closing_another_pane_and_a_stray_focus_leave_the_keyboard(bin: &Path, dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let a = main.view.selected_page().expect("the launch tab");
    let a1 = pane(&a);
    let t1 = terminal_in(&a1).expect("the launch shell");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a2 = focused(&a);
    let t2 = shell_in(&a2, bin, dir, "stray-two", "echo STRAY-TWO\nexec sleep 600");
    act(&main, panes::SPLIT_RIGHT_ACTION);
    let a3 = focused(&a);
    shell_in(
        &a3,
        bin,
        dir,
        "stray-three",
        "echo STRAY-THREE\nexec sleep 600",
    );
    wait_for_text(&t2, "STRAY-TWO");
    panes::focus_pane(&a1);
    pump_until("the first pane to have the keyboard", || t1.has_focus());

    // A press on the third pane's ×, then its click.
    let third = pane_of(&a3);
    let close = all_widgets::<gtk::Button>(third.header())
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Close Pane"))
        .expect("the close button");
    let at = close
        .compute_point(
            &third,
            &gtk::graphene::Point::new(close.width() as f32 / 2.0, close.height() as f32 / 2.0),
        )
        .expect("the button is in its pane");
    let press = controllers::<gtk::GestureClick>(&third)
        .into_iter()
        .find(|g| g.propagation_phase() == gtk::PropagationPhase::Capture)
        .expect("a pane follows clicks");
    press.emit_by_name::<()>("pressed", &[&1i32, &f64::from(at.x()), &f64::from(at.y())]);
    close.emit_clicked();
    pump_until("the third pane to close", || {
        layout_of(&a).panes().len() == 2
    });
    pump_for(Duration::from_millis(300));
    assert!(t1.has_focus(), "the keyboard left the pane in use");
    assert_eq!(focused(&a), a1);

    // A focus given to a pane of a tab out of sight goes back.
    let b = new_tab::open(&main.view, &ctx);
    let tb = shell_in(
        &pane(&b),
        bin,
        dir,
        "stray-b",
        "echo STRAY-B\nexec sleep 600",
    );
    wait_for_text(&tb, "STRAY-B");
    pump_until("the new tab to have the keyboard", || tb.has_focus());
    t2.grab_focus();
    pump_until("the keyboard back in the tab shown", || tb.has_focus());
    assert!(!t2.has_focus());
    assert_eq!(focused(&a), a1, "the hidden tab's pane in use is unchanged");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Run the main loop until `widget` is the focus widget of `window` (whether
/// or not that window is the active one); on a timeout, say where the focus is.
#[track_caller]
fn wait_for_focus(window: &impl IsA<gtk::Window>, widget: &impl IsA<gtk::Widget>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let context = gtk::glib::MainContext::default();
    loop {
        while context.iteration(false) {}
        let focus = gtk::prelude::RootExt::focus(window.upcast_ref::<gtk::Window>());
        if focus.as_ref() == Some(widget.upcast_ref()) {
            return;
        }
        if std::time::Instant::now() > deadline {
            let mut chain = Vec::new();
            let mut current = focus;
            while let Some(w) = current {
                chain.push(w.type_().name().to_owned());
                current = w.parent();
            }
            panic!("timed out waiting for the focus; it is on {chain:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
