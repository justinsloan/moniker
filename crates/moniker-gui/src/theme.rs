//! The terminal's colors: one dark palette, whatever the style of the desktop
//! or of the application.
//!
//! The terminal area is always dark (issue #19, the user's word "always");
//! everything around it, the header bars, menus, dialogs and lists, follows the
//! application's style, which the Preferences' Style setting can force
//! ([`use_style`]). Colors are set once on a terminal ([`paint`]) and never
//! change, so there is no style handler and nothing runs per frame: this costs
//! the render path nothing.
//!
//! The palette is GNOME Console's own dark one, unchanged (the user's choice).
//! The neutrals run from dark to light in order (black, bright black, white,
//! bright white), because programs pair them: tmux's `fg=white,bg=black`, mc's
//! bright white on blue, powerline and starship prompts. White on blue is
//! lower in contrast than it could be; that is Console's own value.

use std::cell::RefCell;

use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use vte4::Terminal;
use vte4::prelude::*;

use crate::settings::Style;

/// The terminal's colors.
pub struct Palette {
    pub foreground: gdk::RGBA,
    pub background: gdk::RGBA,
    /// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan,
    /// white, and the bright version of each.
    pub colors: [gdk::RGBA; 16],
}

/// An opaque color from `0xRRGGBB`.
fn rgb(hex: u32) -> gdk::RGBA {
    let channel = |shift: u32| ((hex >> shift) & 0xff) as f32 / 255.0;
    gdk::RGBA::new(channel(16), channel(8), channel(0), 1.0)
}

/// The palette every terminal has: GNOME Console's dark one.
pub fn palette() -> Palette {
    Palette {
        foreground: rgb(0xffffff),
        background: rgb(0x1e1e1e),
        colors: [
            0x241f31, 0xc01c28, 0x2ec27e, 0xf5c211, 0x1e78e4, 0x9841bb, 0x0ab9dc, 0xc0bfbc,
            0x5e5c64, 0xed333b, 0x57e389, 0xf8e45c, 0x51a1ff, 0xc061cb, 0x4fd2fd, 0xf6f5f4,
        ]
        .map(rgb),
    }
}

/// The color scheme libadwaita uses for `style`: Follow System leaves the
/// choice to the desktop, Light and Dark force it.
pub fn scheme_of(style: Style) -> adw::ColorScheme {
    match style {
        Style::Follow => adw::ColorScheme::Default,
        Style::Light => adw::ColorScheme::ForceLight,
        Style::Dark => adw::ColorScheme::ForceDark,
    }
}

/// Make `style` the style of the whole application, in every window at once
/// (libadwaita's style manager is the application's, not a window's). A
/// terminal is not touched: its palette is set once and is dark under every
/// style. The same style again changes nothing.
pub fn use_style(style: Style) {
    adw::StyleManager::default().set_color_scheme(scheme_of(style));
}

/// Give `terminal` the palette now, and remember it so that [`live_terminals`]
/// finds it for as long as it lives.
pub fn paint(terminal: &Terminal) {
    let palette = palette();
    let colors: Vec<&gdk::RGBA> = palette.colors.iter().collect();
    terminal.set_colors(
        Some(&palette.foreground),
        Some(&palette.background),
        &colors,
    );
    TERMINALS.with_borrow_mut(|all| {
        all.retain(|weak| weak.upgrade().is_some());
        all.push(terminal.downgrade());
    });
}

/// Every terminal that is still alive, in every window: each one is registered
/// by [`paint`] from the moment it is made (`terminal_tab::new_terminal`), so
/// this is how a change for all terminals (the Preferences' font) reaches them.
///
/// Collected into a list rather than visited under the registry's borrow:
/// changing a terminal runs no code of ours, but a borrow held across foreign
/// calls is an invitation.
pub fn live_terminals() -> Vec<Terminal> {
    TERMINALS.with_borrow_mut(|all| {
        all.retain(|weak| weak.upgrade().is_some());
        all.iter().filter_map(glib::WeakRef::upgrade).collect()
    })
}

thread_local! {
    /// The terminals that have been painted. GTK runs on one thread.
    static TERMINALS: RefCell<Vec<glib::WeakRef<Terminal>>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative brightness, 0 (black) to 1 (white), from the sRGB channels.
    fn luminance(color: &gdk::RGBA) -> f32 {
        0.2126 * color.red() + 0.7152 * color.green() + 0.0722 * color.blue()
    }

    /// Follow System leaves the choice to the desktop, Light and Dark force it.
    #[test]
    fn each_style_maps_to_its_color_scheme() {
        assert_eq!(scheme_of(Style::Follow), adw::ColorScheme::Default);
        assert_eq!(scheme_of(Style::Light), adw::ColorScheme::ForceLight);
        assert_eq!(scheme_of(Style::Dark), adw::ColorScheme::ForceDark);
    }

    /// The terminal is always dark (issue #19): a dark background, with text
    /// that stands out from it.
    #[test]
    fn the_background_is_dark_and_the_text_stands_out() {
        let p = palette();
        assert!(luminance(&p.background) < 0.2);
        let gap = (luminance(&p.foreground) - luminance(&p.background)).abs();
        assert!(gap > 0.7, "the text must stand out, gap {gap}");
    }

    #[test]
    fn every_color_is_opaque_and_the_ansi_colors_are_distinct() {
        let p = palette();
        assert_eq!(p.colors.len(), 16);
        for color in p.colors.iter().chain([&p.foreground, &p.background]) {
            assert_eq!(color.alpha(), 1.0);
        }
        for (i, a) in p.colors.iter().enumerate() {
            for b in &p.colors[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// WCAG relative luminance: the sRGB channels made linear, then weighted.
    fn relative_luminance(color: &gdk::RGBA) -> f32 {
        let linear = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.red())
            + 0.7152 * linear(color.green())
            + 0.0722 * linear(color.blue())
    }

    /// The neutrals run in order of brightness: black is dark, bright black is
    /// lighter, white lighter still and bright white the lightest.
    #[test]
    fn the_neutrals_get_lighter_from_black_to_bright_white() {
        let p = palette();
        let l = |index: usize| relative_luminance(&p.colors[index]);
        assert!(l(0) < 0.05, "black is dark");
        assert!(l(0) < l(8), "black is darker than bright black");
        assert!(l(8) < l(7), "bright black is darker than white");
        assert!(l(7) < l(15), "white is darker than bright white");
    }

    #[test]
    fn the_ansi_colors_can_be_read_on_their_own_background() {
        // Black and bright black are for backgrounds and dim text and may sit
        // close to it; every color a program prints text in must not vanish.
        let p = palette();
        for index in (1..8).chain(9..16) {
            let gap = (luminance(&p.colors[index]) - luminance(&p.background)).abs();
            assert!(gap > 0.1, "color {index} is too close to the background");
        }
    }
}
