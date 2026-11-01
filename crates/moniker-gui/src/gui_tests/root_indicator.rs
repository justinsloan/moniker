//! Scenarios: the root indicator (issue #17). While the focused pane of the
//! tab in front runs as root, its window's top bar is red and the header bar
//! shows a warning icon before the title; a root pane's header in a split is
//! red with the icon, focused or not; a tab with a root pane has the icon and a
//! last line in its tooltip. The marks move with the pane, sit before REC, and
//! all go while the switch is off.
//!
//! The first scenarios set what the watch would decide directly
//! ([`root::show_root`]), with a check hook that keeps what is set
//! ([`keep_as_set`]): the watching itself is not what they check. The others
//! check the watching: a local pane is checked on Ptyxis's schedule
//! ([`Backoff`]) through a check hook that answers as the scenario says
//! ([`Hook`]), and once without a hook, for real; a profile pane is red while
//! its profile signs in as `root` and its connection runs, and is never
//! checked. Local tabs run `/bin/sh`, connections a fake `ssh`.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::glib::translate::IntoGlib;
use gtk::{gdk, gio};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::secrets::MemorySecretStore;
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::{ctx_with, ctx_with_secrets, main_windows, test_app};
use super::save_transcripts::{click, configure, new_list, say};
use super::transcript_indicator::{focused, header_icon, layout_of, pane_of, split_with_local};
use super::windows::main_window;
use super::{
    HeldKeyring, all_widgets, fake_ssh, find_widget, labels_in, pane, pump_for, pump_until,
    wait_for_session, wait_for_text,
};
use crate::launch_args::{Run, Start};
use crate::panes::RECORDING_TOOLTIP;
use crate::root::{self, Backoff, PANE_CLASS, ROOT_ICON, ROOT_TOOLTIP, WINDOW_CLASS};
use crate::terminal_tab::{
    self, PaneHolder, WAITING_TEXT, set_test_ssh_program, shown_title, terminal_in, terminal_of,
};
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
    // Within 2 of each channel: another renderer (GL or Vulkan, llvmpipe on
    // CI) or a color-state conversion may round differently. That still tells
    // the tint from an untinted bar by far.
    a == 255
        && [r, g, b]
            .into_iter()
            .zip(tint.background_rgb())
            .all(|(drawn, want)| drawn.abs_diff(want) <= 2)
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

/// A check hook installed for one scenario ([`root::set_test_check`]): each
/// check of a local pane's terminal is answered by the function it was made
/// with, and counted for that terminal. Dropping it takes the hook away again,
/// also on the way out of a failing assertion, so that later scenarios use the
/// real check.
struct Hook {
    checked: Rc<RefCell<Vec<Terminal>>>,
}

impl Hook {
    fn answering(answer: impl Fn(&Terminal) -> bool + 'static) -> Hook {
        let checked = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&checked);
        root::set_test_check(Some(Rc::new(move |terminal: &Terminal| {
            seen.borrow_mut().push(terminal.clone());
            answer(terminal)
        })));
        Hook { checked }
    }

    /// How many checks of `terminal` the hook has answered.
    fn checks_of(&self, terminal: &Terminal) -> usize {
        self.checked
            .borrow()
            .iter()
            .filter(|checked| *checked == terminal)
            .count()
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        root::set_test_check(None);
    }
}

/// A check hook that answers what the pane already shows, so that a check
/// changes nothing a scenario set with [`root::show_root`].
fn keep_as_set() -> Hook {
    Hook::answering(|terminal| {
        terminal
            .parent()
            .and_downcast::<PaneHolder>()
            .is_some_and(|holder| holder.as_root().get())
    })
}

/// Turns the root marks back on when dropped, also on the way out of a
/// failing assertion.
struct SwitchBackOn;

impl Drop for SwitchBackOn {
    fn drop(&mut self) {
        root::set_enabled(true);
    }
}

/// The key controller the watch put on `terminal`, which is in the capture
/// phase.
fn watch_keys(terminal: &Terminal) -> gtk::EventControllerKey {
    let controllers = terminal.observe_controllers();
    let keys = (0..controllers.n_items())
        .filter_map(|i| {
            controllers
                .item(i)
                .and_downcast::<gtk::EventControllerKey>()
        })
        .find(|keys| keys.name().as_deref() == Some(root::KEYS_NAME))
        .expect("the watch's key controller");
    assert_eq!(keys.propagation_phase(), gtk::PropagationPhase::Capture);
    keys
}

/// Press `key` with `state` held, as `keys` sees it: true if it took the key.
fn press(keys: &gtk::EventControllerKey, key: gdk::Key, state: gdk::ModifierType) -> bool {
    keys.emit_by_name::<bool>("key-pressed", &[&key.into_glib(), &0u32, &state])
}

/// Whether `main`'s top bar is red now.
fn is_red(main: &window::MainWindow) -> bool {
    main.window.has_css_class(WINDOW_CLASS)
}

/// A new tab in `main` running `/bin/sh` as a local command, as `moniker --
/// /bin/sh` opens one: unlike a shell tab it keeps its pane, with a note, when
/// the shell exits with a failure.
fn command_tab(main: &window::MainWindow, dir: &Path) -> (adw::TabPage, Terminal) {
    let start = Start {
        dir: dir.to_path_buf(),
        title: None,
        run: Run::Command {
            argv: vec!["/bin/sh".to_owned()],
            hold: false,
        },
        dir_refused: None,
    };
    let terminal = terminal_tab::open_local(&main.view, 1.0, &start, None);
    let page = main.view.selected_page().expect("the command tab");
    assert_eq!(terminal_of(&page).as_ref(), Some(&terminal));
    (page, terminal)
}

/// The top bar is red while the focused pane of the tab in front is root: not
/// while another tab is in front, nor while another pane of the tab is
/// focused. The header bar's icon (with its tooltip) follows the window; the
/// tab keeps its icon and the last line of its tooltip while any pane of it is
/// root, and its title has nothing added. In a split, the root pane's header is
/// red with the icon whether it is focused or not, and is never dimmed.
pub(super) fn the_top_bar_follows_the_focused_pane(dir: &Path) {
    let _hook = keep_as_set();
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
    let _hook = keep_as_set();
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
    let _hook = keep_as_set();
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
/// shows a mark, in any window, and what the local panes were seen to be is
/// forgotten: their watches look again once it is on. On again, every mark is
/// back after the panes' first checks (the hook answers that they are root).
pub(super) fn turning_the_mark_off_hides_everything(dir: &Path) {
    let _hook = Hook::answering(|_| true);
    let _on = SwitchBackOn;
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
        !a.as_root().get() && !c.as_root().get(),
        "what a local pane was seen to be is forgotten"
    );

    root::set_enabled(true);
    assert!(root::enabled());
    pump_until("both top bars red again, after the first checks", || {
        is_red(&one) && is_red(&two)
    });
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
    let _hook = keep_as_set();
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

/// A local pane is checked once it has printed something, and is red while
/// its check says it runs as root: the top bar, the header bar's icon and the
/// tab's marks come and go with the answer. Moved to a window of its own, it
/// takes its watch along, and that window follows the check from then on.
pub(super) fn a_local_pane_follows_its_check(dir: &Path) {
    let answer = Rc::new(Cell::new(false));
    let hook = Hook::answering({
        let answer = Rc::clone(&answer);
        move |_| answer.get()
    });
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let holder = pane(&page);
    let terminal = terminal_of(&page).expect("a local shell");

    let before = root::checks_run();
    say(&terminal, "first");
    pump_until("a check of the shell", || hook.checks_of(&terminal) > 0);
    assert!(root::checks_run() > before, "every check is counted");
    assert!(!holder.as_root().get());
    assert_red(&main, false);
    assert_plain_tab(&page);

    answer.set(true);
    say(&terminal, "second");
    pump_until("the top bar red", || is_red(&main));
    assert!(holder.as_root().get());
    assert_red(&main, true);
    assert_root_tab(&page);

    answer.set(false);
    say(&terminal, "third");
    pump_until("the top bar back", || !is_red(&main));
    assert!(!holder.as_root().get());
    assert_red(&main, false);
    assert_plain_tab(&page);

    // To a window of its own (from a split: a pane alone in its tab has none
    // to move from), where it is still checked.
    act(&main, crate::panes::SPLIT_RIGHT_ACTION);
    let windows = main_windows(&app);
    WidgetExt::activate_action(pane_of(&holder).header(), "pane.pane-to-window", None)
        .expect("pane.pane-to-window");
    pump_until("a new window", || main_windows(&app) == windows + 1);
    let own = window_showing(&app, &holder);
    answer.set(true);
    say(&terminal, "moved");
    pump_until("the new window's top bar red", || is_red(&own));
    assert_red(&own, true);
    assert_red(&main, false);

    own.window.close();
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// The checks of a local pane back off while nothing changes (500 ms, 1 s,
/// 2 s, and on), and output brings them back to 500 ms. Enter (Return, keypad
/// Enter, ISO Enter), Ctrl+C and Ctrl+D pull the next one in to 100 ms; other
/// keys, C and D without Ctrl, and Ctrl+Shift+C change nothing; and the
/// watch's key controller never takes a key from the terminal.
pub(super) fn the_schedule_backs_off_and_comes_back(dir: &Path) {
    let _hook = Hook::answering(|_| false);
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let holder = pane(&page);
    let terminal = terminal_of(&page).expect("a local shell");
    let delay = || root::delay_of(&holder);
    let grown = |to: Duration| {
        pump_until(&format!("the delay to grow to {to:?}"), || {
            delay() >= Some(to)
        });
    };

    say(&terminal, "quiet");
    grown(Duration::from_secs(2));
    say(&terminal, "again");
    pump_until("the delay back at 500 ms", || {
        delay() == Some(Backoff::FIRST)
    });

    let keys = watch_keys(&terminal);
    let none = gdk::ModifierType::empty();
    let ctrl = gdk::ModifierType::CONTROL_MASK;
    for (key, state) in [
        (gdk::Key::Return, none),
        (gdk::Key::KP_Enter, none),
        (gdk::Key::ISO_Enter, none),
        (gdk::Key::c, ctrl),
        (gdk::Key::d, ctrl),
    ] {
        grown(Duration::from_secs(1));
        assert!(!press(&keys, key, state), "{key:?} goes on to the terminal");
        assert_eq!(delay(), Some(Backoff::HURRY), "{key:?} with {state:?}");
    }
    grown(Duration::from_secs(1));
    let before = delay();
    for (key, state) in [
        (gdk::Key::x, none),
        (gdk::Key::c, none),
        (gdk::Key::d, none),
        (gdk::Key::x, ctrl),
        // Ctrl+Shift+C is Copy, not an interrupt.
        (gdk::Key::C, ctrl | gdk::ModifierType::SHIFT_MASK),
    ] {
        assert!(!press(&keys, key, state), "{key:?} goes on to the terminal");
        assert_eq!(delay(), before, "{key:?} with {state:?} changes nothing");
    }

    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// Without a hook the check is the real one: the launch tab's `/bin/sh` is
/// checked through its pty's foreground process group and `/proc`, and is not
/// root (its delay grows, which only a check of its own does). Skipped when the
/// tests run as root.
pub(super) fn a_real_local_pane_is_not_root(dir: &Path) {
    if rustix::process::geteuid().is_root() {
        return; // the shell would be root, and red
    }
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let holder = pane(&page);
    let terminal = terminal_of(&page).expect("a local shell");

    let before = root::checks_run();
    say(&terminal, "real");
    pump_until("a real check of the shell", || {
        root::checks_run() > before && root::delay_of(&holder) >= Some(Duration::from_secs(1))
    });
    assert!(!holder.as_root().get());
    assert_red(&main, false);
    assert_plain_tab(&page);

    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A local session that ends is not red, and is not checked any more: a
/// command tab's shell, red while it runs (the hook says it is root), exits
/// with status 3, and its pane stays with the note, neither red nor checked.
pub(super) fn an_ended_session_is_not_red(dir: &Path) {
    let hook = Hook::answering(|_| true);
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let launch = main.view.selected_page().expect("the launch tab");
    let (page, terminal) = command_tab(&main, dir);
    let holder = pane(&page);
    // The command tab alone, so that its checks are the only ones.
    main.view.close_page(&launch);
    pump_until("the launch tab closed", || main.view.n_pages() == 1);

    say(&terminal, "ended");
    pump_until("the top bar red", || is_red(&main));
    assert_root_tab(&page);

    terminal.feed_child(b"exit 3\n");
    wait_for_text(&terminal, "[session ended: status 3]");
    pump_until("the top bar back", || !is_red(&main));
    assert_eq!(
        terminal_of(&page).as_ref(),
        Some(&terminal),
        "the pane stays, with its note"
    );
    assert!(!holder.as_root().get());
    assert_red(&main, false);
    assert_plain_tab(&page);
    assert_eq!(root::delay_of(&holder), None, "its watch is gone");
    let checks = root::checks_run();
    let own = hook.checks_of(&terminal);
    pump_for(Duration::from_millis(1500));
    assert_eq!(hook.checks_of(&terminal), own, "not checked after its end");
    assert_eq!(root::checks_run(), checks, "no check at all");
    assert!(!is_red(&main));

    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// Closing a pane stops its watch at once, even while its shell lives on (it
/// ignores the hang-up): nothing checks it any more, and nothing goes wrong.
/// It was the focused pane and root, so the top bar loses its red as the
/// other pane, which is not, takes the focus.
pub(super) fn a_closed_pane_stops_its_watch(dir: &Path) {
    let rooted: Rc<RefCell<Option<Terminal>>> = Rc::default();
    let hook = Hook::answering({
        let rooted = Rc::clone(&rooted);
        move |terminal| rooted.borrow().as_ref() == Some(terminal)
    });
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let a = pane(&page);
    let root_shell = terminal_of(&page).expect("a local shell");
    rooted.replace(Some(root_shell.clone()));
    let b = split_with_local(&main, &page);
    let plain_shell = terminal_in(&b).expect("the second pane's shell");
    say(&plain_shell, "plain");
    pump_until("a check of the second pane", || {
        hook.checks_of(&plain_shell) > 0
    });
    crate::panes::focus_pane(&a);
    assert_eq!(focused(&page), a);
    say(&root_shell, "root");
    pump_until("the top bar red", || is_red(&main));
    assert_red_header(&a, true);
    assert_red_header(&b, false);

    // From now on its shell ignores the hang-up, so that only the closing can
    // stop its watch.
    root_shell.feed_child(b"trap '' HUP\n");
    say(&root_shell, "trapped");
    WidgetExt::activate_action(pane_of(&a).header(), "pane.close", None).expect("pane.close");
    let checked = hook.checks_of(&root_shell);
    pump_until("the top bar no longer red", || !is_red(&main));
    assert_eq!(layout_of(&page).holders(), std::slice::from_ref(&b));
    assert_eq!(focused(&page), b);
    assert_red(&main, false);
    assert_plain_tab(&page);
    assert_eq!(root::delay_of(&a), None, "its watch is gone");
    assert!(!a.as_root().get());
    pump_for(Duration::from_millis(1500));
    assert_eq!(
        hook.checks_of(&root_shell),
        checked,
        "the closed pane is not checked"
    );
    assert!(!is_red(&main));

    // Its shell, still running, ends now.
    root_shell.feed_child(b"exit\n");
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A profile that signs in as `root` is red while its connection runs: not
/// while it waits for the keyring, red once its `ssh` (a fake here) has
/// started, and not once the connection has ended. One that signs in as
/// another user is never red. Neither is ever checked (a connection's terminal
/// runs `ssh`, which runs as the person): the hook would say root to every
/// check. With the switch off, a root profile's pane hides its red and keeps
/// what it is; on again, it is red again at once.
pub(super) fn a_root_profile_is_red_while_connected(bin: &Path, dir: &Path) {
    let fake = fake_ssh(bin, "root-profile", "echo FAKE-END\nread line\nexit 3");
    let admin = Profile::new("admin", "admin.example.test", "root");
    let alice = Profile::new("alice", "alice.example.test", "alice");
    let keyring = Arc::new(HeldKeyring::new(Arc::new(MemorySecretStore::default())));
    let ctx = ctx_with_secrets(
        dir,
        &[admin.clone(), alice.clone()],
        Arc::clone(&keyring) as _,
    );
    let hook = Hook::answering(|_| true);
    let _on = SwitchBackOn;
    let app = test_app();
    let main = main_window(&app, &ctx);
    let launch = main.view.selected_page().expect("the launch tab");
    set_test_ssh_program(Some(fake));

    // alice: never red, never checked.
    let (as_alice, list) = new_list(&main, &ctx);
    click(&list, &alice.id);
    let alice_terminal = wait_for_session(&as_alice);
    wait_for_text(&alice_terminal, "FAKE-END");
    // Only connections from now on: no local pane to check.
    main.view.close_page(&launch);
    pump_until("the launch tab closed", || main.view.n_pages() == 1);
    let checks = root::checks_run();
    pump_for(Duration::from_millis(1500));
    assert!(!pane(&as_alice).as_root().get());
    assert_red(&main, false);
    assert_plain_tab(&as_alice);
    assert_eq!(
        hook.checks_of(&alice_terminal),
        0,
        "a connection is not checked"
    );
    assert_eq!(root::checks_run(), checks, "no check at all");

    // root: not while it waits for the keyring.
    keyring.hold();
    let (as_root, list) = new_list(&main, &ctx);
    click(&list, &admin.id);
    let holder = pane(&as_root);
    pump_until("the pane to say it waits for the keyring", || {
        labels_in(&holder)
            .iter()
            .any(|label| label.label() == WAITING_TEXT && label.is_visible())
    });
    assert!(terminal_of(&as_root).is_none(), "it still waits");
    assert!(!holder.as_root().get());
    assert_red(&main, false);
    assert_plain_tab(&as_root);
    keyring.release();
    let root_terminal = wait_for_session(&as_root);
    pump_until("the top bar red", || is_red(&main));
    assert_red(&main, true);
    assert_root_tab(&as_root);
    wait_for_text(&root_terminal, "FAKE-END");

    // The switch off hides it and keeps it: nothing would tell it again.
    root::set_enabled(false);
    assert_red(&main, false);
    assert_plain_tab(&as_root);
    assert!(holder.as_root().get(), "a profile pane keeps what it is");
    root::set_enabled(true);
    assert_red(&main, true);
    assert_root_tab(&as_root);

    // The connection ends (status 3 keeps the pane, with its note).
    root_terminal.feed_child(b"\n");
    wait_for_text(&root_terminal, "[session ended: status 3]");
    pump_until("the top bar back", || !is_red(&main));
    assert!(!holder.as_root().get());
    assert_plain_tab(&as_root);
    assert_eq!(
        hook.checks_of(&root_terminal),
        0,
        "a connection is not checked"
    );
    assert_eq!(hook.checks_of(&alice_terminal), 0);

    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// With the switch off a local pane is not checked at all, whatever it prints
/// or is typed into it, and is not red; on again, it is checked from 500 ms
/// on, and red again (the hook says it is root).
pub(super) fn the_switch_stops_the_checks(dir: &Path) {
    let hook = Hook::answering(|_| true);
    let _on = SwitchBackOn;
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = main.view.selected_page().expect("the launch tab");
    let holder = pane(&page);
    let terminal = terminal_of(&page).expect("a local shell");
    say(&terminal, "red");
    pump_until("the top bar red", || is_red(&main));

    root::set_enabled(false);
    assert_red(&main, false);
    assert_plain_tab(&page);
    assert!(!holder.as_root().get(), "a local pane's state is forgotten");
    let checks = root::checks_run();
    let own = hook.checks_of(&terminal);
    say(&terminal, "off");
    assert!(!press(
        &watch_keys(&terminal),
        gdk::Key::Return,
        gdk::ModifierType::empty()
    ));
    pump_for(Duration::from_millis(1500));
    assert_eq!(hook.checks_of(&terminal), own, "not checked while off");
    assert_eq!(root::checks_run(), checks, "nothing is");
    assert_red(&main, false);

    root::set_enabled(true);
    assert_eq!(
        root::delay_of(&holder),
        Some(Backoff::FIRST),
        "checked again from 500 ms"
    );
    pump_until("the top bar red again", || is_red(&main));
    assert_root_tab(&page);

    main.window.close();
    pump_for(Duration::from_millis(200));
}
