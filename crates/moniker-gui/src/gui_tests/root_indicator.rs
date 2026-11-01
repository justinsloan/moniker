//! Scenarios: the root indicator (issue #17). While the focused pane of the
//! tab in front runs as root, its window's top bar is red and the header bar
//! shows a warning icon before the title; a root pane's header in a split is
//! red with the icon, focused or not; a tab with a root pane has the icon and a
//! last line in its tooltip. The marks move with the pane, sit before REC, and
//! all go while the switch is off.
//!
//! What the watch would decide is set here directly ([`root::show_root`]): the
//! watching itself is not what these check. Local tabs run `/bin/sh`.

use std::path::Path;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio};
use gtk4 as gtk;
use libadwaita as adw;

use super::main_window::{ctx_with, main_windows, test_app};
use super::save_transcripts::{configure, new_list, say};
use super::transcript_indicator::{focused, header_icon, layout_of, pane_of};
use super::windows::main_window;
use super::{all_widgets, find_widget, pane, pump_for, pump_until};
use crate::panes::RECORDING_TOOLTIP;
use crate::root::{self, PANE_CLASS, ROOT_ICON, ROOT_TOOLTIP, WINDOW_CLASS};
use crate::terminal_tab::{PaneHolder, shown_title, terminal_of};
use crate::window;

/// Run the window action `name` of `main`, as its accelerator does.
fn act(main: &window::MainWindow, name: &str) {
    WidgetExt::activate_action(&main.window, &format!("win.{name}"), None)
        .unwrap_or_else(|err| panic!("win.{name}: {err}"));
}

/// The warning icon before the header bar's title of `main`, which shows while
/// its top bar is red.
fn header_root_image(main: &window::MainWindow) -> gtk::Image {
    let header: adw::HeaderBar = find_widget(&main.window).expect("the header bar");
    all_widgets::<gtk::Image>(&header)
        .into_iter()
        .find(|image| image.widget_name() == window::ROOT_IMAGE_NAME)
        .expect("the header bar's root icon")
}

/// The name of the icon of `page` (`AdwTabPage:icon`), if it has one.
fn tab_icon(page: &adw::TabPage) -> Option<String> {
    let icon = page.icon()?.downcast::<gio::ThemedIcon>().ok()?;
    icon.names().first().map(ToString::to_string)
}

/// The last line of `page`'s tooltip.
fn last_tooltip_line(page: &adw::TabPage) -> Option<String> {
    page.tooltip()?.lines().last().map(str::to_owned)
}

/// `main`'s top bar is red and its header bar shows the icon, or neither.
#[track_caller]
fn assert_red(main: &window::MainWindow, red: bool) {
    assert_eq!(
        main.window.has_css_class(WINDOW_CLASS),
        red,
        "the window's class: {:?}",
        main.window.css_classes()
    );
    let image = header_root_image(main);
    pump_until("the header bar's root icon to follow the window", || {
        image.is_visible() == red
    });
}

/// `page` says that a pane of it runs as root: the warning icon, and the last
/// line of its tooltip; its title is its focused pane's, with nothing before
/// it.
#[track_caller]
fn assert_root_tab(page: &adw::TabPage) {
    assert_eq!(tab_icon(page).as_deref(), Some(ROOT_ICON));
    assert_eq!(
        last_tooltip_line(page).as_deref(),
        Some(ROOT_TOOLTIP),
        "{:?}",
        page.tooltip()
    );
    assert_eq!(page.title(), shown_title(&pane(page)), "no prefix");
}

/// `page` says nothing of the sort: no icon, no such line in its tooltip.
#[track_caller]
fn assert_plain_tab(page: &adw::TabPage) {
    assert_eq!(tab_icon(page), None);
    assert!(
        !page
            .tooltip()
            .unwrap_or_default()
            .lines()
            .any(|line| line == ROOT_TOOLTIP),
        "{:?}",
        page.tooltip()
    );
}

/// The header of `holder`'s pane is red with the icon (`red`), or neither.
#[track_caller]
fn assert_red_header(holder: &PaneHolder, red: bool) {
    let pane = pane_of(holder);
    assert_eq!(
        pane.header().has_css_class(PANE_CLASS),
        red,
        "{:?}",
        pane.header().css_classes()
    );
    assert_eq!(pane.root_image().is_visible(), red);
}

/// The color `widget` draws at `x`, `y` of itself (background, border, and
/// whatever its children draw there), as red, green, blue and opacity. It is
/// drawn off screen with the window's own renderer.
fn drawn_at(widget: &impl IsA<gtk::Widget>, x: i32, y: i32) -> [u8; 4] {
    let widget = widget.upcast_ref::<gtk::Widget>();
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(
        &snapshot,
        f64::from(widget.width()),
        f64::from(widget.height()),
    );
    let node = snapshot.to_node().expect("the widget draws something");
    let renderer = widget
        .native()
        .and_then(|native| native.renderer())
        .expect("a renderer");
    let mut download = gdk::TextureDownloader::new(&renderer.render_texture(&node, None));
    download.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = download.download_bytes();
    let at = usize::try_from(y).expect("y") * stride + usize::try_from(x).expect("x") * 4;
    [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
}

/// Whether `widget` draws the tint of the style now (`manager`) at a spot of
/// its own background, near its top left corner.
fn drawn_tinted(widget: &impl IsA<gtk::Widget>, manager: &adw::StyleManager) -> bool {
    let tint = root::Tint::for_style(manager.is_dark(), manager.is_high_contrast());
    let [r, g, b, a] = drawn_at(widget, 3, 2);
    a == 255 && [r, g, b] == tint.background_rgb()
}

/// The top bar of `main`: the box of its toolbar view that holds the header
/// bar and the tab bar.
fn top_bar(main: &window::MainWindow) -> gtk::Widget {
    let toolbar: adw::ToolbarView = find_widget(&main.window).expect("the toolbar view");
    all_widgets::<gtk::Widget>(&toolbar)
        .into_iter()
        .find(|widget| widget.has_css_class("top-bar"))
        .expect("its top bar")
}

/// The main window of `app` whose tab in front holds only `holder`.
fn window_showing(app: &adw::Application, holder: &PaneHolder) -> window::MainWindow {
    app.windows()
        .iter()
        .filter_map(window::MainWindow::of)
        .find(|main| {
            main.view
                .selected_page()
                .is_some_and(|page| layout_of(&page).holders() == std::slice::from_ref(holder))
        })
        .expect("the window the pane went to")
}

/// The top bar is red while the focused pane of the tab in front is root: not
/// while another tab is in front, nor while another pane of the tab is
/// focused. The header bar's icon (with its tooltip) follows the window; the
/// tab keeps its icon and the last line of its tooltip while any pane of it is
/// root, and its title has nothing added. In a split, the root pane's header is
/// red with the icon whether it is focused or not, and is never dimmed.
pub(super) fn the_top_bar_follows_the_focused_pane(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let image = header_root_image(&main);
    assert_eq!(image.icon_name().as_deref(), Some(ROOT_ICON));
    assert_eq!(image.tooltip_text().as_deref(), Some(ROOT_TOOLTIP));
    assert!(!image.is_focusable(), "not a button");
    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    let tab_a = main.view.selected_page().expect("the launch tab");
    let a = pane(&tab_a);
    assert_red(&main, false);
    assert_plain_tab(&tab_a);

    root::show_root(&a, true);
    assert!(a.as_root().get());
    assert_red(&main, true);
    assert_root_tab(&tab_a);
    assert_eq!(title.title(), tab_a.title(), "the header bar's title");

    // Another tab in front: not red; the root tab keeps its icon.
    let (tab_b, _list) = new_list(&main, &ctx);
    assert_eq!(main.view.selected_page().as_ref(), Some(&tab_b));
    assert_red(&main, false);
    assert_plain_tab(&tab_b);
    assert_root_tab(&tab_a);
    main.view.set_selected_page(&tab_a);
    assert_red(&main, true);

    // A split: the new pane is focused and not root, so the top bar is not
    // red, but the root pane's header is, and the tab keeps its marks.
    act(&main, crate::panes::SPLIT_RIGHT_ACTION);
    let other = focused(&tab_a);
    assert_ne!(other, a);
    assert_red(&main, false);
    assert!(pane_of(&a).header().is_visible());
    assert_red_header(&a, true);
    assert_red_header(&other, false);
    assert_eq!(
        pane_of(&a).root_image().tooltip_text().as_deref(),
        Some(ROOT_TOOLTIP)
    );
    assert_eq!(
        pane_of(&a).root_image().icon_name().as_deref(),
        Some(ROOT_ICON)
    );
    assert!(!pane_of(&a).root_image().is_focusable(), "not a button");
    assert_eq!(
        pane_of(&a).root_image().next_sibling().as_ref(),
        Some(pane_of(&a).badge().upcast_ref::<gtk::Widget>()),
        "the root icon comes first"
    );
    assert!(
        !pane_of(&a).title().has_css_class("dim-label"),
        "a red header is not dimmed, though not focused"
    );
    assert_root_tab(&tab_a);

    // The root pane focused again: red again; now the other pane's title is
    // the dimmed one.
    crate::panes::focus_pane(&a);
    assert_eq!(focused(&tab_a), a);
    assert_red(&main, true);
    assert_red_header(&a, true);
    assert!(pane_of(&other).title().has_css_class("dim-label"));

    // No longer root: nothing is left of it.
    root::show_root(&a, false);
    assert_red(&main, false);
    assert_red_header(&a, false);
    assert_plain_tab(&tab_a);
    assert_eq!(tab_a.title(), shown_title(&a));

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A root pane moved to a tab of its own (its header's Move to New Tab) or to a
/// window of its own (Move to New Window) takes its marks along: the new tab,
/// and the new window's top bar, show it. The tab it left loses them, unless
/// another of its panes is root.
pub(super) fn a_root_pane_moves_with_its_state(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let first = main.view.selected_page().expect("the launch tab");
    let a = pane(&first);
    root::show_root(&a, true);
    act(&main, crate::panes::SPLIT_RIGHT_ACTION);
    let b = focused(&first);
    assert_root_tab(&first);

    // To a tab of its own.
    WidgetExt::activate_action(pane_of(&a).header(), "pane.pane-to-tab", None)
        .expect("pane.pane-to-tab");
    pump_until("the pane's own tab", || main.view.n_pages() == 2);
    let moved = main.view.selected_page().expect("its tab");
    assert_eq!(layout_of(&moved).holders(), std::slice::from_ref(&a));
    assert_root_tab(&moved);
    assert_red(&main, true);
    assert_eq!(layout_of(&first).holders(), std::slice::from_ref(&b));
    assert_plain_tab(&first);

    // To a window of its own, from a split whose other pane is root too: the
    // tab it leaves keeps its marks.
    act(&main, crate::panes::SPLIT_RIGHT_ACTION);
    let c = focused(&moved);
    assert_ne!(c, a);
    root::show_root(&c, true);
    let windows = main_windows(&app);
    WidgetExt::activate_action(pane_of(&a).header(), "pane.pane-to-window", None)
        .expect("pane.pane-to-window");
    pump_until("a new window", || main_windows(&app) == windows + 1);
    let own = window_showing(&app, &a);
    let in_own = own.view.selected_page().expect("its tab");
    assert_root_tab(&in_own);
    assert_red(&own, true);
    assert_eq!(layout_of(&moved).holders(), std::slice::from_ref(&c));
    assert_root_tab(&moved);
    assert_red(&main, true);

    own.window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A tab whose pane both saves a transcript and runs as root: its tooltip ends
/// with REC's line and then root's; the header bar shows the root icon, then
/// the record icon, then the title; a pane's header in a split shows the root
/// icon, then the REC badge, then the pane's title.
pub(super) fn rec_and_root_together(dir: &Path) {
    let transcripts = dir.join("transcripts");
    let ctx = ctx_with(dir, &[]);
    configure(&ctx, &transcripts, true);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let holder = pane(&page);
    say(&terminal_of(&page).expect("a local shell"), "both");
    assert!(holder.recording().get(), "it saves a transcript");
    root::show_root(&holder, true);

    let tooltip = page.tooltip().unwrap_or_default();
    assert!(
        tooltip.ends_with(&format!("\n{RECORDING_TOOLTIP}\n{ROOT_TOOLTIP}")),
        "{tooltip:?}"
    );
    let root_image = header_root_image(&main);
    let rec_image = header_icon(&main);
    let title: adw::WindowTitle = find_widget(&main.window).expect("the window title");
    pump_until("both icons in the header bar", || {
        root_image.is_visible() && rec_image.is_visible()
    });
    assert_eq!(
        root_image.next_sibling().as_ref(),
        Some(rec_image.upcast_ref::<gtk::Widget>()),
        "the root icon, then the record icon"
    );
    assert_eq!(
        rec_image
            .next_sibling()
            .and_downcast::<adw::WindowTitle>()
            .as_ref(),
        Some(&title),
        "then the title"
    );

    // In a split, the pane's header: the root icon, the REC badge, the title.
    act(&main, crate::panes::SPLIT_RIGHT_ACTION);
    let shown = pane_of(&holder);
    assert!(shown.header().is_visible());
    assert!(shown.root_image().is_visible());
    assert!(shown.badge().is_visible());
    assert_eq!(
        shown.root_image().next_sibling().as_ref(),
        Some(shown.badge().upcast_ref::<gtk::Widget>()),
        "the root icon, then the REC badge"
    );
    assert_eq!(
        shown.badge().next_sibling().as_ref(),
        Some(shown.title().upcast_ref::<gtk::Widget>()),
        "then the pane's title"
    );

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// With the switch off, no window is red and no header bar, pane header or tab
/// shows a mark, in any window, while what each pane was seen to be is kept;
/// on again, every mark is back.
pub(super) fn turning_the_mark_off_hides_everything(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let one = main_window(&app, &ctx);
    let two = main_window(&app, &ctx);
    let tab_one = one.view.selected_page().expect("the first window's tab");
    let a = pane(&tab_one);
    root::show_root(&a, true);
    act(&one, crate::panes::SPLIT_RIGHT_ACTION);
    crate::panes::focus_pane(&a);
    let tab_two = two.view.selected_page().expect("the second window's tab");
    let c = pane(&tab_two);
    root::show_root(&c, true);
    let all_shown = || {
        assert_red(&one, true);
        assert_red(&two, true);
        assert_red_header(&a, true);
        assert_root_tab(&tab_one);
        assert_root_tab(&tab_two);
    };
    all_shown();

    root::set_enabled(false);
    assert!(!root::enabled());
    assert_red(&one, false);
    assert_red(&two, false);
    assert_red_header(&a, false);
    assert_plain_tab(&tab_one);
    assert_plain_tab(&tab_two);
    assert!(
        a.as_root().get() && c.as_root().get(),
        "what was seen is kept"
    );

    root::set_enabled(true);
    assert!(root::enabled());
    all_shown();

    one.window.destroy();
    two.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The tint is loaded again when the application's style changes between light
/// and dark, and a red window stays red. It is drawn, too: on the top bar of a
/// red window (libadwaita draws a flat top bar with no background of its own,
/// so the header bar's variables alone would color nothing), and on a root
/// pane's header.
pub(super) fn the_tint_follows_light_and_dark(dir: &Path) {
    let manager = adw::StyleManager::default();
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let bar = top_bar(&main);
    let page = main.view.selected_page().expect("the launch tab");
    let holder = pane(&page);
    root::show_root(&holder, true);
    assert_red(&main, true);

    manager.set_color_scheme(adw::ColorScheme::ForceDark);
    assert!(manager.is_dark());
    assert_eq!(
        root::current_css(),
        root::css(true, manager.is_high_contrast())
    );
    assert_red(&main, true);
    pump_until("the top bar drawn in the tint", || {
        drawn_tinted(&bar, &manager)
    });

    manager.set_color_scheme(adw::ColorScheme::ForceLight);
    assert!(!manager.is_dark());
    assert_eq!(
        root::current_css(),
        root::css(false, manager.is_high_contrast())
    );
    assert_red(&main, true);
    pump_until("the top bar drawn in the tint", || {
        drawn_tinted(&bar, &manager)
    });

    // A split whose other pane is focused: the top bar is not red, the root
    // pane's header is, and the other one's is not.
    act(&main, crate::panes::SPLIT_RIGHT_ACTION);
    let other = focused(&page);
    assert_red(&main, false);
    pump_until("the top bar drawn without the tint", || {
        !drawn_tinted(&bar, &manager)
    });
    pump_until("the root pane's header drawn in the tint", || {
        drawn_tinted(pane_of(&holder).header(), &manager)
    });
    assert!(
        !drawn_tinted(pane_of(&other).header(), &manager),
        "{:?}",
        drawn_at(pane_of(&other).header(), 3, 2)
    );

    manager.set_color_scheme(adw::ColorScheme::Default);
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}
