//! Windows that open at the size they had when they last closed (issue #14).
//!
//! Each kind of window (the main window, Connection Profiles, Transcripts)
//! keeps one size in the GUI settings ([`settings::WindowSize`]). When a window
//! of that kind closes, its size and whether it was maximized are saved, so of
//! several the last to close decides (and a resize is saved once it has
//! settled, as a logout closes no window); a new one opens at that size, made to fit
//! the largest monitor, and maximized if it was. The size saved is the window's
//! default size, which GTK keeps as the size the window has when it is not
//! maximized or full screen; full screen itself is not kept. A quit that closes
//! no window (the process killed) saves nothing, as for the text size, and so
//! does a window still at its built-in size when no size was saved. One read
//! when a window opens, one write when it closes, nothing per frame.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use libadwaita as adw;

use crate::settings::{self, GuiSettings, WindowSize};

/// The kinds of window that remember their size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Main,
    Profiles,
    Transcripts,
}

impl Kind {
    /// The size saved for this kind in `settings`.
    fn saved(self, settings: &GuiSettings) -> Option<WindowSize> {
        match self {
            Kind::Main => settings.main_window,
            Kind::Profiles => settings.profiles_window,
            Kind::Transcripts => settings.transcripts_window,
        }
    }

    fn slot(self, settings: &mut GuiSettings) -> &mut Option<WindowSize> {
        match self {
            Kind::Main => &mut settings.main_window,
            Kind::Profiles => &mut settings.profiles_window,
            Kind::Transcripts => &mut settings.transcripts_window,
        }
    }
}

/// The size a window opens at, and whether it is maximized: the `saved` one,
/// else `default`, no wider than the widest of `monitors` and no taller than
/// the tallest (each a logical width and height; one with no size is ignored,
/// and with none known there is no limit), and never below
/// [`settings::WINDOW_MIN`].
pub(crate) fn opening(
    saved: Option<WindowSize>,
    default: (i32, i32),
    monitors: &[(i32, i32)],
) -> ((i32, i32), bool) {
    let (width, height, maximized) = match saved {
        Some(size) => (size.width, size.height, size.maximized),
        None => (default.0, default.1, false),
    };
    let usable = || monitors.iter().filter(|m| m.0 > 0 && m.1 > 0);
    let widest = usable().map(|m| m.0).max();
    let tallest = usable().map(|m| m.1).max();
    let fit =
        |side: i32, limit: Option<i32>, min: i32| limit.map_or(side, |l| side.min(l)).max(min);
    (
        (
            fit(width, widest, settings::WINDOW_MIN.0),
            fit(height, tallest, settings::WINDOW_MIN.1),
        ),
        maximized,
    )
}

/// The logical sizes of the default display's monitors. (GTK also bounds a
/// new window by the screen it opens on when the display says how big that
/// is, as X11 does; this fit is for a Wayland compositor that sends no
/// bounds.)
fn monitors() -> Vec<(i32, i32)> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    let list = display.monitors();
    (0..list.n_items())
        .filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>())
        .map(|m| (m.geometry().width(), m.geometry().height()))
        .collect()
}

/// How long a window must keep still after a resize before its size is saved.
const SETTLE: Duration = Duration::from_secs(1);

/// Open `window`, not yet shown, at the size saved for `kind` in the settings
/// at `settings_path` (else `default`), and save its size there once a resize
/// has settled ([`SETTLE`]: a logout or a shutdown closes no window, and would
/// otherwise lose it) and when it closes.
pub(crate) fn remember(
    window: &impl IsA<gtk::Window>,
    kind: Kind,
    settings_path: &Path,
    default: (i32, i32),
) {
    let window = window.upcast_ref::<gtk::Window>();
    let saved = kind.saved(&settings::load(settings_path));
    let (opened, maximized) = opening(saved, default, &monitors());
    window.set_default_size(opened.0, opened.1);
    if maximized {
        window.maximize();
    }
    let path: PathBuf = settings_path.to_path_buf();
    let save = Rc::new(move |window: &gtk::Window| {
        let (width, height) = window.default_size();
        let size = WindowSize {
            width,
            height,
            maximized: window.is_maximized(),
        };
        // A window not resized since it opened, with nothing saved before,
        // writes nothing, as quitting at the normal text size writes nothing.
        let untouched = (width, height) == opened && !size.maximized;
        let result = settings::update(&path, |s| {
            let slot = kind.slot(s);
            if slot.is_some() || !untouched {
                *slot = Some(size);
            }
        });
        if let Err(err) = result {
            eprintln!(
                "moniker: cannot save the window size in {}: {err}",
                path.display()
            );
        }
    });
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let settle = Rc::new({
        let (save, pending) = (Rc::clone(&save), Rc::clone(&pending));
        move |window: &gtk::Window| {
            if let Some(id) = pending.take() {
                id.remove();
            }
            let weak = window.downgrade();
            let (save, fired) = (Rc::clone(&save), Rc::clone(&pending));
            let id = glib::timeout_add_local_once(SETTLE, move || {
                fired.take();
                if let Some(window) = weak.upgrade() {
                    save(&window);
                }
            });
            pending.replace(Some(id));
        }
    });
    for property in ["default-width", "default-height", "maximized"] {
        let settle = Rc::clone(&settle);
        window.connect_notify_local(Some(property), move |window, _| settle(window));
    }
    window.connect_close_request(move |window| {
        if let Some(id) = pending.take() {
            id.remove();
        }
        save(window);
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: i32, height: i32, maximized: bool) -> Option<WindowSize> {
        Some(WindowSize {
            width,
            height,
            maximized,
        })
    }

    #[test]
    fn a_window_opens_at_its_saved_size_or_the_default() {
        let screen = [(1920, 1080)];
        assert_eq!(opening(None, (1100, 700), &screen), ((1100, 700), false));
        assert_eq!(
            opening(size(800, 500, false), (1100, 700), &screen),
            ((800, 500), false)
        );
        assert_eq!(
            opening(size(800, 500, true), (1100, 700), &screen),
            ((800, 500), true),
            "maximized again"
        );
    }

    #[test]
    fn a_size_too_big_for_every_monitor_is_made_to_fit_the_largest() {
        let monitors = [(1280, 1024), (2560, 1440)];
        assert_eq!(
            opening(size(4000, 3000, false), (1100, 700), &monitors),
            ((2560, 1440), false)
        );
        assert_eq!(
            opening(size(1000, 1200, false), (1100, 700), &monitors),
            ((1000, 1200), false),
            "each side against the largest"
        );
        assert_eq!(
            opening(None, (1100, 700), &[(1024, 600)]),
            ((1024, 600), false),
            "the default too"
        );
        assert_eq!(
            opening(size(4000, 3000, false), (1100, 700), &[]),
            ((4000, 3000), false),
            "no monitor known: as saved"
        );
    }

    #[test]
    fn a_monitor_with_no_size_is_ignored_and_a_window_never_opens_tiny() {
        assert_eq!(
            opening(size(1000, 600, false), (1100, 700), &[(0, 0), (-5, 7)]),
            ((1000, 600), false),
            "no usable monitor: as saved"
        );
        assert_eq!(
            opening(size(1000, 600, false), (1100, 700), &[(200, 100)]),
            (settings::WINDOW_MIN, false),
            "never below the smallest size kept"
        );
    }
}
