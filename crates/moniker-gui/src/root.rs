//! Whether a session runs as root (issue #17), and the marks that say so.
//!
//! # Deciding
//!
//! What the root indicator decides is kept apart from showing it, so that it is
//! testable without a display.
//!
//! * **A local pane** is root while the process group in the foreground of its
//!   terminal runs as root, which is Ptyxis's rule ([`foreground_is_root`]). The
//!   test is the group's **effective uid**, read from the `Uid:` line of
//!   `/proc/<pid>/status` ([`effective_uid_is_root`]), not whether the folder
//!   `/proc/<pid>/` belongs to root as Ptyxis tests it: that folder is also
//!   root's for any process that made itself non-dumpable, which some ordinary
//!   programs do for safety (agents, password tools), and they would show red.
//!   `sudo` counts from the start, because it is a setuid program: its
//!   effective uid is 0 even while it asks for the password.
//! * **A profile pane** runs `ssh`, which always runs as the person, so nothing
//!   is read from `/proc` for it: the profile's own user name decides
//!   ([`logs_in_as_root`]), the one thing Moniker knows for certain about the
//!   remote session.
//! * **When to look** is Ptyxis's poll ([`Backoff`]): soon after output, then
//!   less and less often while nothing changes.
//!
//! # Showing
//!
//! What was seen is kept on the pane's holder (`PaneHolder::as_root`, set by
//! [`show_root`]), so it moves with the pane. While the switch is on
//! ([`enabled`], [`set_enabled`]) a pane that is root ([`shown`]) is marked
//! three ways, as Ptyxis marks it:
//!
//! * **the top bar** of its window is tinted red, while it is the focused pane
//!   of the tab in front ([`show_in_window`], the window's [`WINDOW_CLASS`]),
//!   and the header bar shows [`ROOT_ICON`] before its title (`crate::window`);
//! * **its header** in a split is tinted the same way, focused or not
//!   ([`PANE_CLASS`]), with the icon before its title (`crate::panes`);
//! * **its tab** has the icon, and a last line [`ROOT_TOOLTIP`] in its tooltip,
//!   while any of its panes is root (`crate::panes`).
//!
//! The icon is there as well as the color, for someone who cannot tell red and
//! for a screen reader. The tint ([`Tint`]) is fixed colors, light or dark,
//! with a text color of its own, so that the text on it keeps at least 4.5:1
//! whatever the theme's own header color is; it follows the application's
//! style ([`follow_style`]). Nothing moves or fades.
//!
//! Nothing here is per frame, and nothing here reads a secret.

use std::cell::{Cell, RefCell};
use std::os::fd::BorrowedFd;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk4 as gtk;
use libadwaita as adw;
use rustix::termios::tcgetpgrp;

use crate::panes;
use crate::terminal_tab::PaneHolder;
use crate::window::MainWindow;

/// The tooltip and accessible label of every root mark.
pub(crate) const ROOT_TOOLTIP: &str = "Running as root";

/// The icon of the root mark, before the title in the header bar and in a
/// pane's header, and as the tab's icon.
pub(crate) const ROOT_ICON: &str = "dialog-warning-symbolic";

/// The CSS class of a main window while the focused pane of its tab in front
/// is root ([`show_in_window`]): it tints the top bar.
pub(crate) const WINDOW_CLASS: &str = "moniker-root";

/// The CSS class of a pane's header (`panes::HEADER_CLASS`) while its pane is
/// root: it tints the header.
pub(crate) const PANE_CLASS: &str = "root";

/// The CSS class of a main window's toolbar view, whose top bar (the header
/// bar and the tab bar) the tint colors. libadwaita draws a flat top bar with
/// no background of its own, in the window's, so the header bar's colors have
/// to be given to the top bar itself; the class keeps the tint off the top
/// bars of dialogs shown over the window.
pub(crate) const TOP_BARS_CLASS: &str = "moniker-top-bars";

thread_local! {
    /// Whether root sessions are marked at all. GTK runs on one thread.
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    /// The provider of the tint, once [`follow_style`] has installed it.
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    /// The style sheet the provider has now.
    static LOADED: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Whether root sessions are marked: true unless [`set_enabled`] turned it off.
pub(crate) fn enabled() -> bool {
    ENABLED.get()
}

/// Turn the marks on or off, and show every tab of every main window as it now
/// is. What each pane's session was seen to be (`PaneHolder::as_root`) is kept
/// either way.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn set_enabled(on: bool) {
    ENABLED.set(on);
    for window in gtk::Window::list_toplevels() {
        let Some(main) = window
            .downcast_ref::<gtk::Window>()
            .and_then(MainWindow::of)
        else {
            continue;
        };
        for index in 0..main.view.n_pages() {
            if let Some(layout) = panes::layout_of_page(&main.view.nth_page(index)) {
                panes::refresh(&layout);
            }
        }
    }
}

/// Whether the pane of `holder` is marked as root now: its session was seen to
/// run as root, and the marks are on.
pub(crate) fn shown(holder: &PaneHolder) -> bool {
    enabled() && holder.as_root().get()
}

/// Say whether the session in the pane of `holder` runs as root (`on`), and
/// show its pane, tab and window again if that changed. A holder in no pane
/// layout only keeps the flag.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn show_root(holder: &PaneHolder, on: bool) {
    if holder.as_root().replace(on) != on
        && let Some(layout) = panes::layout_of(holder)
    {
        panes::refresh(&layout);
    }
}

/// Tint the top bar of the window `view` is in while the focused pane of its
/// tab in front is marked as root ([`shown`]), and not otherwise. Nothing
/// happens for a view in no window.
pub(crate) fn show_in_window(view: &adw::TabView) {
    let Some(window) = view.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let red = view
        .selected_page()
        .and_then(|page| panes::layout_of_page(&page))
        .and_then(|layout| layout.focused_holder())
        .is_some_and(|holder| shown(&holder));
    if red {
        window.add_css_class(WINDOW_CLASS);
    } else {
        window.remove_css_class(WINDOW_CLASS);
    }
}

/// A color: sRGB channels from 0 to 255 and an opacity from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Rgba {
    /// An opaque color from its hex value, `0xrrggbb`.
    const fn hex(value: u32) -> Rgba {
        Rgba {
            r: ((value >> 16) & 0xff) as f64,
            g: ((value >> 8) & 0xff) as f64,
            b: (value & 0xff) as f64,
            a: 1.0,
        }
    }

    /// `amount` (0 to 1) of `other` mixed into this opaque color, each channel
    /// rounded to a whole value, so that what is drawn is what was measured.
    fn mix(self, other: Rgba, amount: f64) -> Rgba {
        let channel = |mine: f64, theirs: f64| (mine * (1.0 - amount) + theirs * amount).round();
        Rgba {
            r: channel(self.r, other.r),
            g: channel(self.g, other.g),
            b: channel(self.b, other.b),
            a: 1.0,
        }
    }

    /// The color in CSS.
    fn to_css(self) -> String {
        format!("rgba({}, {}, {}, {})", self.r, self.g, self.b, self.a)
    }
}

/// GNOME's red (red 3 of its palette), libadwaita's own: mixed into a header
/// color, it makes the tint.
const RED: Rgba = Rgba::hex(0xe01b24);

/// The neutral header colors the red is mixed into, light and dark: Adwaita's,
/// fixed rather than the theme's own, since a theme (Ubuntu's Yaru, with a
/// style sheet per accent color) may make its header any color.
const LIGHT_BASE: Rgba = Rgba::hex(0xebebed);
const DARK_BASE: Rgba = Rgba::hex(0x2e2e32);

/// The red of a root session's top bar and pane header, and the color of the
/// text on it, for one style of the application.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Tint {
    background: Rgba,
    text: Rgba,
}

impl Tint {
    /// The tint for the light or `dark` style, in high contrast or not (`hc`).
    ///
    /// The background mixes the red into the neutral header color in Ptyxis's
    /// proportions, half of it in the light style and a third in the dark one;
    /// high contrast keeps the same background. The text color is set too, so
    /// that no theme can lower the contrast: Adwaita's dark text in the light
    /// style (black in high contrast), white in the dark one.
    pub(crate) fn for_style(dark: bool, hc: bool) -> Tint {
        if dark {
            Tint {
                background: DARK_BASE.mix(RED, 0.33),
                text: Rgba::hex(0xffffff),
            }
        } else {
            Tint {
                background: LIGHT_BASE.mix(RED, 0.5),
                text: if hc {
                    Rgba::hex(0x000000)
                } else {
                    Rgba {
                        r: 0.0,
                        g: 0.0,
                        b: 6.0,
                        a: 0.8,
                    }
                },
            }
        }
    }

    /// The background in CSS.
    pub(crate) fn background_css(&self) -> String {
        self.background.to_css()
    }

    /// The background's channels, as a texture drawn with it holds them.
    #[cfg(test)]
    pub(crate) fn background_rgb(&self) -> [u8; 3] {
        let b = self.background;
        // Whole values from 0 to 255: `mix` rounds them.
        [b.r, b.g, b.b].map(|channel| channel as u8)
    }

    /// The text as it is drawn on the background: its color, at its opacity,
    /// over the background.
    #[cfg(test)]
    fn text_over_tint(&self) -> Rgba {
        let (t, b) = (self.text, self.background);
        let over = |text: f64, back: f64| text * t.a + back * (1.0 - t.a);
        Rgba {
            r: over(t.r, b.r),
            g: over(t.g, b.g),
            b: over(t.b, b.b),
            a: 1.0,
        }
    }
}

/// The contrast ratio of two opaque colors, as WCAG 2 measures it: from 1 (the
/// same) to 21 (black on white).
#[cfg(test)]
fn contrast(one: Rgba, other: Rgba) -> f64 {
    fn luminance(color: Rgba) -> f64 {
        let linear = |channel: f64| {
            let c = channel / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }
    let (a, b) = (luminance(one), luminance(other));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The style sheet of the tint for the light or `dark` style, in high contrast
/// or not (`hc`).
///
/// A window with [`WINDOW_CLASS`] gives libadwaita's header bar variables the
/// tint, the way GNOME Console does, and its top bar ([`TOP_BARS_CLASS`]) is
/// drawn in them; a pane header with [`PANE_CLASS`] is drawn in the tint. The
/// header bar's focused-pane underline is a border and stays.
pub(crate) fn css(dark: bool, hc: bool) -> String {
    let tint = Tint::for_style(dark, hc);
    let background = tint.background_css();
    let text = tint.text.to_css();
    format!(
        "window.{WINDOW_CLASS} {{ \
           --headerbar-bg-color: {background}; \
           --headerbar-backdrop-color: {background}; \
           --headerbar-fg-color: {text}; }}\n\
         window.{WINDOW_CLASS} toolbarview.{TOP_BARS_CLASS} > .top-bar {{ \
           background-color: var(--headerbar-bg-color); \
           color: var(--headerbar-fg-color); }}\n\
         window.{WINDOW_CLASS} toolbarview.{TOP_BARS_CLASS} > .top-bar:backdrop {{ \
           background-color: var(--headerbar-backdrop-color); }}\n\
         .{header}.{PANE_CLASS} {{ background-color: {background}; color: {text}; }}\n",
        header = panes::HEADER_CLASS,
    )
}

/// Load the tint on the default display, once, and load it again whenever the
/// application's style changes between light and dark, or into or out of high
/// contrast, for as long as the application runs. Nothing is drawn with it
/// until a window or a pane header has its class.
pub(crate) fn follow_style() {
    if PROVIDER.with_borrow(Option::is_some) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    PROVIDER.set(Some(provider));
    let manager = adw::StyleManager::default();
    load_tint(&manager);
    manager.connect_dark_notify(load_tint);
    manager.connect_high_contrast_notify(load_tint);
}

/// Load the tint of `manager`'s style now into the provider.
fn load_tint(manager: &adw::StyleManager) {
    let text = css(manager.is_dark(), manager.is_high_contrast());
    // Taken out first, so that no borrow is held while GTK restyles.
    if let Some(provider) = PROVIDER.with_borrow(Clone::clone) {
        provider.load_from_string(&text);
    }
    LOADED.set(text);
}

/// The style sheet the tint's provider has now (empty before
/// [`follow_style`]).
#[cfg(test)]
pub(crate) fn current_css() -> String {
    LOADED.with_borrow(Clone::clone)
}

/// Whether the text of a `/proc/<pid>/status` file says the process runs as
/// root: the second number (the effective uid) of the first `Uid:` line is 0.
///
/// The line has four numbers, the real, effective, saved and file system ids.
/// A setuid program such as `sudo` has a real id of the person and an effective
/// id of 0, and is root; a root process that dropped to a person has the
/// opposite and is not. A file without such a line, or with one that does not
/// parse, is not root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn effective_uid_is_root(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1))
        .and_then(|effective| effective.parse::<u32>().ok())
        == Some(0)
}

/// Whether process `pid` runs as root, by its `/proc/<pid>/status`. A pid of 0
/// or below (a group outside our pid namespace has none), a process that is
/// gone and a file that cannot be read are not root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn pid_is_root(pid: i32) -> bool {
    pid > 0
        && std::fs::read_to_string(format!("/proc/{pid}/status"))
            .is_ok_and(|status| effective_uid_is_root(&status))
}

/// Whether the process group in the foreground of the terminal whose pty is
/// `pty` runs as root, by its leader's id (a group's id is its leader's pid).
/// Anything that is no terminal, or has no foreground group, is not root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn foreground_is_root(pty: BorrowedFd<'_>) -> bool {
    tcgetpgrp(pty).is_ok_and(|group| pid_is_root(group.as_raw_pid()))
}

/// Whether a profile signs in as root: its user name is exactly `root`. An
/// account with uid 0 under another name is not seen.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn logs_in_as_root(user: &str) -> bool {
    user == "root"
}

/// How long to wait before the next check of a local pane: the schedule of
/// Ptyxis's tab monitor (`ptyxis-tab-monitor.c`), kept per pane.
///
/// A check comes [`Backoff::FIRST`] after the terminal's first output. A check
/// that found no change doubles the delay, up to [`Backoff::MAX`]: 500 ms, 1 s,
/// 2 s, 4 s, 8 s, 10 s. A check that found a change sets it back to
/// [`Backoff::FIRST`], and so does output while the delay is longer
/// ([`Backoff::output`]). Enter, Ctrl+C and Ctrl+D pull the next check in to
/// [`Backoff::HURRY`] ([`Backoff::hurry`]).
///
/// It only counts delays; the caller owns the timer.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Backoff {
    delay: Duration,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Backoff {
    /// The first delay, and the shortest one after a change or output.
    pub(crate) const FIRST: Duration = Duration::from_millis(500);
    /// The longest delay.
    pub(crate) const MAX: Duration = Duration::from_secs(10);
    /// The delay after Enter, Ctrl+C or Ctrl+D.
    pub(crate) const HURRY: Duration = Duration::from_millis(100);

    /// A schedule that waits [`Backoff::FIRST`].
    pub(crate) fn new() -> Backoff {
        Backoff { delay: Self::FIRST }
    }

    /// The current delay.
    pub(crate) fn delay(&self) -> Duration {
        self.delay
    }

    /// A check has run: `changed` says whether it found the root state
    /// different. Returns the delay before the next one.
    pub(crate) fn checked(&mut self, changed: bool) -> Duration {
        self.delay = if changed {
            Self::FIRST
        } else {
            (self.delay * 2).clamp(Self::FIRST, Self::MAX)
        };
        self.delay
    }

    /// The terminal printed something. When the delay is longer than
    /// [`Backoff::FIRST`] it goes back to it and this returns true: the caller
    /// moves its timer. At [`Backoff::FIRST`] (or shorter, after a hurry)
    /// nothing changes and this returns false.
    pub(crate) fn output(&mut self) -> bool {
        if self.delay > Self::FIRST {
            self.delay = Self::FIRST;
            true
        } else {
            false
        }
    }

    /// Enter, Ctrl+C or Ctrl+D was typed: the next check comes in
    /// [`Backoff::HURRY`]. Returns that delay.
    pub(crate) fn hurry(&mut self) -> Duration {
        self.delay = Self::HURRY;
        self.delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_says_what_the_design_says() {
        assert_eq!(ROOT_TOOLTIP, "Running as root");
        assert_eq!(ROOT_ICON, "dialog-warning-symbolic");
    }

    #[test]
    fn the_effective_uid_decides() {
        assert!(effective_uid_is_root(
            "Name:\tbash\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n"
        ));
        // setuid: real 1000, effective 0 (sudo asking for the password)
        assert!(effective_uid_is_root("Uid:\t1000\t0\t1000\t1000\n"));
        // real 0, effective 1000: not root
        assert!(!effective_uid_is_root("Uid:\t0\t1000\t0\t0\n"));
        assert!(effective_uid_is_root("Uid:   0   0 0 0\n"));
        assert!(!effective_uid_is_root("Name:\tbash\n"));
        assert!(!effective_uid_is_root("Uid:\n"));
        assert!(!effective_uid_is_root("Uid:\tx\ty\n"));
        assert!(!effective_uid_is_root(""));
        // only the first Uid: line counts
        assert!(!effective_uid_is_root(
            "Uid:\t1000\t1000\t1000\t1000\nUid:\t0\t0\t0\t0\n"
        ));
    }

    #[test]
    fn this_process_is_read_as_it_is() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        assert_eq!(
            effective_uid_is_root(&status),
            rustix::process::geteuid().is_root()
        );
        assert_eq!(
            pid_is_root(rustix::process::getpid().as_raw_pid()),
            rustix::process::geteuid().is_root()
        );
    }

    #[test]
    fn a_pid_with_no_proc_entry_is_not_root() {
        assert!(!pid_is_root(0));
        assert!(!pid_is_root(-1));
        assert!(!pid_is_root(0x3fff_ffff)); // above Linux's pid_max ceiling
    }

    #[test]
    fn something_that_is_no_terminal_is_not_root() {
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!foreground_is_root(std::os::fd::AsFd::as_fd(&null)));
    }

    #[test]
    fn only_the_user_root_logs_in_as_root() {
        assert!(logs_in_as_root("root"));
        for user in ["Root", "ROOT", "rootx", " root", "root ", "", "toor"] {
            assert!(!logs_in_as_root(user), "{user:?}");
        }
    }

    #[test]
    fn the_tint_keeps_its_text_readable() {
        for (dark, hc) in [(false, false), (false, true), (true, false), (true, true)] {
            let t = Tint::for_style(dark, hc);
            assert!(
                contrast(t.text_over_tint(), t.background) >= 4.5,
                "dark={dark} hc={hc}: {t:?}"
            );
        }
    }

    #[test]
    fn the_css_tints_the_window_and_the_pane_header() {
        for (dark, hc) in [(false, false), (true, false), (false, true), (true, true)] {
            let css = css(dark, hc);
            let t = Tint::for_style(dark, hc);
            assert!(css.contains("window.moniker-root"));
            assert!(css.contains("--headerbar-bg-color"));
            assert!(css.contains("--headerbar-backdrop-color"));
            assert!(css.contains("--headerbar-fg-color"));
            assert!(css.contains(".moniker-pane-header.root"));
            assert!(css.contains(&t.background_css()));
        }
        assert_ne!(css(false, false), css(true, false));
    }

    #[test]
    fn the_schedule_is_ptyxis_s() {
        let ms = Duration::from_millis;
        let mut b = Backoff::new();
        assert_eq!(b.delay(), ms(500));
        let grown: Vec<_> = (0..6).map(|_| b.checked(false)).collect();
        assert_eq!(
            grown,
            [
                ms(1000),
                ms(2000),
                ms(4000),
                ms(8000),
                ms(10_000),
                ms(10_000)
            ]
        );
        assert_eq!(b.checked(true), ms(500));
        assert!(!b.output(), "at 500 ms output changes nothing");
        b.checked(false); // 1000
        assert!(b.output());
        assert_eq!(b.delay(), ms(500));
        assert_eq!(b.hurry(), ms(100));
        assert_eq!(
            b.checked(false),
            ms(500),
            "doubling never goes below 500 ms"
        );
        b.hurry();
        assert_eq!(b.checked(true), ms(500));
    }
}
