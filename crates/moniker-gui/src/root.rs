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
//! # Watching
//!
//! * **A local pane** (a shell, a command, a Local terminal sequence) is
//!   watched from the moment its session starts ([`watch_local`], a [`Watch`]
//!   its holder keeps, so it moves with the pane) until the session ends or
//!   the pane goes ([`stop`]), hidden tabs included. Its first check comes
//!   [`Backoff::FIRST`] after the terminal first prints something, and each
//!   check schedules the next, on the main loop at low priority. The handler
//!   of the terminal's `contents-changed` does constant work: it never reads
//!   `/proc` and never restyles, it only compares the delay and, at most,
//!   moves one timer. Enter, Ctrl+C and Ctrl+D typed in the terminal pull the
//!   next check in, through a key controller in the capture phase that never
//!   takes the key. A check is one `tcgetpgrp` and one read of a small `/proc`
//!   file.
//! * **A profile pane** is never watched: it is root from the moment its
//!   connection starts, the jump host's sign-in included, until its `ssh`
//!   exits, if its profile signs in as `root` (`crate::terminal_tab`).
//! * **With the switch off nothing is checked** ([`set_enabled`]), and a local
//!   pane's state is forgotten until its next check once it is on again.
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
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::{gdk, glib};
use gtk4 as gtk;
use libadwaita as adw;
use rustix::termios::tcgetpgrp;
use vte4::Terminal;
use vte4::prelude::*;

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
/// is.
///
/// Off, nothing is checked: every watch's pending check is removed, and what a
/// local pane was seen to be is forgotten, since nothing would look at it again
/// while off. A profile pane keeps what it is (its profile's user says it, and
/// nothing looks again), and [`shown`] hides it. On, every watch checks again
/// [`Backoff::FIRST`] from now, and a local pane shows its state after that
/// first check.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn set_enabled(on: bool) {
    ENABLED.set(on);
    // Collected first: a check scheduled or cancelled borrows nothing of it.
    let watches: Vec<Rc<Watching>> =
        WATCHES.with_borrow(|watches| watches.iter().filter_map(Weak::upgrade).collect());
    for watch in watches {
        if on {
            watch.backoff.set(Backoff::new());
            watch.schedule(Backoff::FIRST);
        } else {
            watch.cancel();
            if let Some(holder) = watch.holder.upgrade() {
                holder.as_root().set(false);
            }
        }
    }
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

/// What looks whether a local pane's session runs as root, on Ptyxis's
/// schedule ([`Backoff`]), from the moment the session starts
/// ([`watch_local`]) until it ends or the pane goes ([`stop`]). The pane's
/// holder keeps it (`PaneHolder::root_watch`), so it moves with the pane.
///
/// It holds the terminal and the holder weakly, so it never keeps a pane
/// alive. Dropping it removes its timer, its signal handlers and its key
/// controller.
pub(crate) struct Watch {
    inner: Rc<Watching>,
}

/// The name of a watch's key controller on its terminal ([`watch_local`]).
pub(crate) const KEYS_NAME: &str = "moniker-root-keys";

/// The state of a [`Watch`], which its timer and its handlers reach weakly.
struct Watching {
    terminal: glib::WeakRef<Terminal>,
    holder: glib::WeakRef<PaneHolder>,
    keys: glib::WeakRef<gtk::EventControllerKey>,
    /// The handlers on the terminal: `contents-changed` and `child-exited`.
    handlers: RefCell<Vec<glib::SignalHandlerId>>,
    backoff: Cell<Backoff>,
    /// The pending check: at most one at a time. Its callback takes it out
    /// before it runs, so a source that has run is never removed.
    timer: RefCell<Option<glib::SourceId>>,
    /// The watch was dropped: nothing is scheduled any more.
    stopped: Cell<bool>,
}

/// Tests only: a check of a terminal in place of the real one, whether the
/// program in its foreground runs as root ([`set_test_check`]).
#[cfg(test)]
type Check = Rc<dyn Fn(&Terminal) -> bool>;

thread_local! {
    /// Every watch, to start and stop them all with the switch
    /// ([`set_enabled`]). Dead ones are pruned as new ones come.
    static WATCHES: RefCell<Vec<Weak<Watching>>> = const { RefCell::new(Vec::new()) };
    /// The tests' check in place of the real one ([`set_test_check`]).
    #[cfg(test)]
    static TEST_CHECK: RefCell<Option<Check>> = const { RefCell::new(None) };
    /// How many checks have run ([`checks_run`]).
    #[cfg(test)]
    static CHECKS: Cell<u32> = const { Cell::new(0) };
}

/// Watch whether the local session in `terminal`, in the pane of `holder`,
/// runs as root, from now until it ends: the first check comes
/// [`Backoff::FIRST`] after the terminal first prints something, and the next
/// ones on [`Backoff`]'s schedule. Its handler of `contents-changed` does
/// constant work, on every change the terminal shows; Enter, Ctrl+C and Ctrl+D
/// typed in it (a key controller in the capture phase, which never takes the
/// key) pull the next check in to [`Backoff::HURRY`]. The session's end stops
/// it ([`stop`]).
pub(crate) fn watch_local(holder: &PaneHolder, terminal: &Terminal) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.set_name(Some(KEYS_NAME));
    let inner = Rc::new(Watching {
        terminal: terminal.downgrade(),
        holder: holder.downgrade(),
        keys: keys.downgrade(),
        handlers: RefCell::new(Vec::new()),
        backoff: Cell::new(Backoff::new()),
        timer: RefCell::new(None),
        stopped: Cell::new(false),
    });

    let weak = Rc::downgrade(&inner);
    let changed = terminal.connect_contents_changed(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.printed();
        }
    });
    let weak = Rc::downgrade(&inner);
    keys.connect_key_pressed(move |_, key, _, state| {
        if hurries(key, state)
            && let Some(inner) = weak.upgrade()
        {
            inner.hurry();
        }
        glib::Propagation::Proceed
    });
    terminal.add_controller(keys);
    let pane = holder.downgrade();
    let exited = terminal.connect_child_exited(move |_, _| {
        if let Some(holder) = pane.upgrade() {
            stop(&holder);
        }
    });
    inner.handlers.replace(vec![changed, exited]);

    WATCHES.with_borrow_mut(|watches| {
        watches.retain(|watch| watch.strong_count() > 0);
        watches.push(Rc::downgrade(&inner));
    });
    // Taken out first, so that no borrow is held while an old watch drops.
    let old = holder.root_watch().replace(Some(Watch { inner }));
    drop(old);
}

/// Stop watching the session in the pane of `holder` (it ended, or the pane
/// went), and show the pane as not root. Nothing is checked for it after this.
pub(crate) fn stop(holder: &PaneHolder) {
    let watch = holder.root_watch().take();
    drop(watch);
    show_root(holder, false);
}

/// Whether `key`, typed with `state` held, pulls the next check in: Enter
/// (Return, keypad Enter or ISO Enter) with any modifiers, or Ctrl+C or Ctrl+D
/// with Ctrl alone (Caps Lock aside): Ctrl+Shift+C is Copy, not an interrupt.
fn hurries(key: gdk::Key, state: gdk::ModifierType) -> bool {
    use gdk::ModifierType as M;
    // GTK's default modifier mask (`gtk_accelerator_get_default_mod_mask`),
    // spelled out so that this needs no display.
    let counted = M::SHIFT_MASK
        | M::CONTROL_MASK
        | M::ALT_MASK
        | M::SUPER_MASK
        | M::HYPER_MASK
        | M::META_MASK;
    match key {
        gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => true,
        _ => {
            state & counted == M::CONTROL_MASK
                && matches!(key.to_lower(), gdk::Key::c | gdk::Key::d)
        }
    }
}

/// Whether the program in the foreground of `terminal` runs as root: the
/// tests' check if they set one, else [`foreground_is_root`] on its pty.
fn is_root_now(terminal: &Terminal) -> bool {
    #[cfg(test)]
    {
        CHECKS.set(CHECKS.get() + 1);
        if let Some(check) = TEST_CHECK.with_borrow(Clone::clone) {
            return check(terminal);
        }
    }
    terminal
        .pty()
        .is_some_and(|pty| foreground_is_root(pty.fd()))
}

impl Watching {
    /// The terminal printed something. Constant work: with the switch off,
    /// nothing; with no check pending, one at the current delay; with one
    /// pending, moved to [`Backoff::FIRST`] only if the delay was longer.
    fn printed(self: &Rc<Self>) {
        if !enabled() {
            return;
        }
        if self.timer.borrow().is_none() {
            self.schedule(self.backoff.get().delay());
            return;
        }
        let mut backoff = self.backoff.get();
        if backoff.output() {
            self.backoff.set(backoff);
            self.schedule(Backoff::FIRST);
        }
    }

    /// Enter, Ctrl+C or Ctrl+D was typed: the next check in
    /// [`Backoff::HURRY`]. Nothing with the switch off.
    fn hurry(self: &Rc<Self>) {
        if !enabled() {
            return;
        }
        let mut backoff = self.backoff.get();
        let delay = backoff.hurry();
        self.backoff.set(backoff);
        self.schedule(delay);
    }

    /// Check in `delay`, in place of a check pending.
    fn schedule(self: &Rc<Self>, delay: Duration) {
        self.cancel();
        if self.stopped.get() {
            return;
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_full(delay, glib::Priority::LOW, move || {
            if let Some(inner) = weak.upgrade() {
                // This source ends as it returns: it is not to be removed.
                inner.timer.take();
                inner.check();
            }
            glib::ControlFlow::Break
        });
        self.timer.replace(Some(source));
    }

    /// Remove the check pending, if there is one.
    fn cancel(&self) {
        if let Some(source) = self.timer.take() {
            source.remove();
        }
    }

    /// Look whether the session runs as root, show it, and schedule the next
    /// check: sooner if it changed, later if not. A pane or a terminal that is
    /// gone, or a terminal no longer in its pane, is not checked.
    fn check(self: &Rc<Self>) {
        let (Some(terminal), Some(holder)) = (self.terminal.upgrade(), self.holder.upgrade())
        else {
            return;
        };
        if self.stopped.get()
            || !enabled()
            || terminal.parent().as_ref() != Some(holder.upcast_ref::<gtk::Widget>())
        {
            return;
        }
        let before = holder.as_root().get();
        let now = is_root_now(&terminal);
        show_root(&holder, now);
        let mut backoff = self.backoff.get();
        let delay = backoff.checked(now != before);
        self.backoff.set(backoff);
        self.schedule(delay);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let inner = &self.inner;
        inner.stopped.set(true);
        inner.cancel();
        let handlers = inner.handlers.take();
        if let Some(terminal) = inner.terminal.upgrade() {
            for handler in handlers {
                terminal.disconnect(handler);
            }
            if let Some(keys) = inner.keys.upgrade()
                && keys.widget().as_ref() == Some(terminal.upcast_ref::<gtk::Widget>())
            {
                terminal.remove_controller(&keys);
            }
        }
    }
}

/// Tests only: check terminals with `check` in place of the real check (none:
/// the real one again). Every scenario that sets one takes it away at its end.
#[cfg(test)]
pub(crate) fn set_test_check(check: Option<Check>) {
    TEST_CHECK.set(check);
}

/// Tests only: how many checks have run so far, the tests' or the real one.
#[cfg(test)]
pub(crate) fn checks_run() -> u32 {
    CHECKS.get()
}

/// Tests only: the delay of the watch of `holder`'s pane before its next check
/// ([`Backoff::delay`]); none while it has no watch.
#[cfg(test)]
pub(crate) fn delay_of(holder: &PaneHolder) -> Option<Duration> {
    holder
        .root_watch()
        .borrow()
        .as_ref()
        .map(|watch| watch.inner.backoff.get().delay())
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
/// drawn in them; a pane header with [`PANE_CLASS`] is drawn in the tint. A
/// pane header's focused underline (`crate::panes`'
/// `.moniker-pane-header.focused`) is a border and stays.
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
pub(crate) fn pid_is_root(pid: i32) -> bool {
    pid > 0
        && std::fs::read_to_string(format!("/proc/{pid}/status"))
            .is_ok_and(|status| effective_uid_is_root(&status))
}

/// Whether the process group in the foreground of the terminal whose pty is
/// `pty` runs as root, by its leader's id (a group's id is its leader's pid).
/// Anything that is no terminal, or has no foreground group, is not root.
/// Neither is a group whose leader has exited while others of it still run:
/// `/proc/<pgid>` is gone with the leader, as Ptyxis finds it too.
pub(crate) fn foreground_is_root(pty: BorrowedFd<'_>) -> bool {
    tcgetpgrp(pty).is_ok_and(|group| pid_is_root(group.as_raw_pid()))
}

/// Whether a profile signs in as root: its user name is exactly `root`. An
/// account with uid 0 under another name is not seen.
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
pub(crate) struct Backoff {
    delay: Duration,
}

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
    fn enter_ctrl_c_and_ctrl_d_pull_the_next_check_in() {
        use gdk::{Key, ModifierType as M};
        let ctrl = M::CONTROL_MASK;
        for (key, state) in [
            (Key::Return, M::empty()),
            (Key::KP_Enter, M::empty()),
            (Key::ISO_Enter, M::empty()),
            (Key::Return, M::SHIFT_MASK),
            (Key::c, ctrl),
            (Key::d, ctrl),
            // Caps Lock is not a modifier that counts.
            (Key::C, ctrl | M::LOCK_MASK),
            (Key::c, ctrl | M::LOCK_MASK),
        ] {
            assert!(hurries(key, state), "{key:?} with {state:?}");
        }
        for (key, state) in [
            (Key::c, M::empty()),
            (Key::d, M::empty()),
            (Key::x, ctrl),
            (Key::z, ctrl),
            // Ctrl+Shift+C is Copy; Ctrl+Alt+D is not Ctrl+D.
            (Key::C, ctrl | M::SHIFT_MASK),
            (Key::d, ctrl | M::ALT_MASK),
            (Key::space, M::empty()),
        ] {
            assert!(!hurries(key, state), "{key:?} with {state:?}");
        }
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
