//! Windows that open at the size they had when they last closed (issue #14).
//!
//! Each kind of window (the main window, Connection Profiles, Transcripts)
//! keeps one size in the GUI settings ([`settings::WindowSize`]). When a window
//! of that kind closes, its size and whether it was maximized are saved, so of
//! several the last to close decides; a new one opens at that size, made to fit
//! the largest monitor, and maximized if it was. The size saved is the window's
//! default size, which GTK keeps as the size the window has when it is not
//! maximized or full screen; full screen itself is not kept. A quit that closes
//! no window (the process killed) saves nothing, as for the text size, and so
//! does a window still at its built-in size when no size was saved. One read
//! when a window opens, one write when it closes, nothing per frame.

use std::path::{Path, PathBuf};

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
/// the tallest (each a logical width and height; none known, no limit).
pub(crate) fn opening(
    saved: Option<WindowSize>,
    default: (i32, i32),
    monitors: &[(i32, i32)],
) -> ((i32, i32), bool) {
    let (width, height, maximized) = match saved {
        Some(size) => (size.width, size.height, size.maximized),
        None => (default.0, default.1, false),
    };
    let widest = monitors.iter().map(|m| m.0).max();
    let tallest = monitors.iter().map(|m| m.1).max();
    (
        (
            widest.map_or(width, |w| width.min(w)),
            tallest.map_or(height, |h| height.min(h)),
        ),
        maximized,
    )
}

/// The logical sizes of the default display's monitors.
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

/// Open `window`, not yet shown, at the size saved for `kind` in the settings
/// at `settings_path` (else `default`), and save its size there when it
/// closes.
pub(crate) fn remember(
    window: &impl IsA<gtk::Window>,
    kind: Kind,
    settings_path: &Path,
    default: (i32, i32),
) {
    let window = window.upcast_ref::<gtk::Window>();
    let saved = kind.saved(&settings::load(settings_path));
    let ((width, height), maximized) = opening(saved, default, &monitors());
    window.set_default_size(width, height);
    if maximized {
        window.maximize();
    }
    let path: PathBuf = settings_path.to_path_buf();
    window.connect_close_request(move |window| {
        let (width, height) = window.default_size();
        let size = WindowSize {
            width,
            height,
            maximized: window.is_maximized(),
        };
        // A window never resized, with nothing saved before, writes nothing,
        // as quitting at the normal text size writes nothing.
        let untouched = (width, height) == default && !size.maximized;
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
}
