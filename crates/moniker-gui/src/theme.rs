//! The terminal's colors: a light and a dark palette that follow the style of
//! the desktop.
//!
//! Colors are set once on a terminal ([`follow_style`]) and again only when the
//! application's light/dark style changes (`adw::StyleManager:dark`), by one
//! handler for the whole application. Nothing runs per frame, so this costs the
//! render path nothing.
//!
//! The dark palette is GNOME Console's own; the light one is close to Console's:
//! the GNOME color set on libadwaita's view colors. In both palettes the
//! neutrals run from dark to light in order (black, bright black, white, bright
//! white), because programs pair them: tmux's `fg=white,bg=black`, mc's bright
//! white on blue, powerline and starship prompts. White and bright white are
//! therefore LIGHT in the light palette too (as in GNOME Console), and text
//! drawn in them directly on the light style's white background is faint by
//! design. The tests hold the light palette's white and bright white to a 4.5:1
//! contrast on black and on blue; the dark palette keeps Console's values
//! unchanged (the user's choice), where white on blue is lower.

use std::cell::{Cell, RefCell};

use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use vte4::Terminal;
use vte4::prelude::*;

/// The colors of one style.
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

/// The palette for the dark style if `dark`, else for the light one.
pub fn palette(dark: bool) -> Palette {
    let (foreground, background, colors) = if dark {
        (
            0xffffff,
            0x1e1e1e,
            [
                0x241f31, 0xc01c28, 0x2ec27e, 0xf5c211, 0x1e78e4, 0x9841bb, 0x0ab9dc, 0xc0bfbc,
                0x5e5c64, 0xed333b, 0x57e389, 0xf8e45c, 0x51a1ff, 0xc061cb, 0x4fd2fd, 0xf6f5f4,
            ],
        )
    } else {
        (
            0x241f31,
            0xffffff,
            [
                0x241f31, 0xc01c28, 0x26a269, 0xa2734c, 0x12488b, 0xa347ba, 0x2aa1b3, 0xc0bfbc,
                0x5e5c64, 0xe01b24, 0x1c8c5a, 0x9c6b00, 0x1c71d8, 0x813d9c, 0x1d7f94, 0xdeddda,
            ],
        )
    };
    Palette {
        foreground: rgb(foreground),
        background: rgb(background),
        colors: colors.map(rgb),
    }
}

/// Give `terminal` the colors of the current style now, and again whenever the
/// style changes between light and dark for as long as the terminal lives.
pub fn follow_style(terminal: &Terminal) {
    let manager = adw::StyleManager::default();
    apply(terminal, manager.is_dark());
    TERMINALS.with_borrow_mut(|all| {
        all.retain(|weak| weak.upgrade().is_some());
        all.push(terminal.downgrade());
    });
    // One handler for all terminals, connected by the first.
    if !WATCHING.replace(true) {
        manager.connect_dark_notify(|manager| {
            let dark = manager.is_dark();
            for terminal in &live_terminals() {
                apply(terminal, dark);
            }
        });
    }
}

/// Every terminal that is still alive, in every window: each one follows the
/// style from the moment it is made (`terminal_tab::new_terminal`), so this is
/// also how a change for all terminals (the Preferences' font) reaches them.
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
    /// The terminals that follow the style. GTK runs on one thread.
    static TERMINALS: RefCell<Vec<glib::WeakRef<Terminal>>> = const { RefCell::new(Vec::new()) };
    /// Whether the style handler is connected.
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

/// Set the colors of `terminal` to the palette of the dark or light style.
fn apply(terminal: &Terminal, dark: bool) {
    let palette = palette(dark);
    let colors: Vec<&gdk::RGBA> = palette.colors.iter().collect();
    terminal.set_colors(
        Some(&palette.foreground),
        Some(&palette.background),
        &colors,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative brightness, 0 (black) to 1 (white), from the sRGB channels.
    fn luminance(color: &gdk::RGBA) -> f32 {
        0.2126 * color.red() + 0.7152 * color.green() + 0.0722 * color.blue()
    }

    #[test]
    fn dark_and_light_palettes_differ() {
        let dark = palette(true);
        let light = palette(false);
        assert_ne!(dark.background, light.background);
        assert_ne!(dark.foreground, light.foreground);
        assert!(luminance(&dark.background) < 0.2);
        assert!(luminance(&light.background) > 0.8);
        for p in [&dark, &light] {
            let gap = (luminance(&p.foreground) - luminance(&p.background)).abs();
            assert!(gap > 0.7, "the text must stand out, gap {gap}");
        }
    }

    #[test]
    fn every_color_is_opaque_and_the_ansi_colors_are_distinct() {
        for p in [palette(true), palette(false)] {
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

    /// WCAG contrast ratio of two colors, 1 (the same) to 21 (black on white).
    fn contrast(a: &gdk::RGBA, b: &gdk::RGBA) -> f32 {
        let (a, b) = (relative_luminance(a), relative_luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Terminal programs draw "white" and "bright white" text on black and on
    /// blue (mc's panels, powerline and starship prompts, tmux's
    /// `fg=white,bg=black`): in the light style that must stay readable. The
    /// dark style is GNOME Console's palette unchanged, by the user's choice.
    #[test]
    fn white_and_bright_white_read_on_black_and_blue_in_the_light_style() {
        const BLACK: usize = 0;
        const BLUE: usize = 4;
        const WHITE: usize = 7;
        const BRIGHT_WHITE: usize = 15;
        let p = palette(false);
        for text in [WHITE, BRIGHT_WHITE] {
            for background in [BLACK, BLUE] {
                let ratio = contrast(&p.colors[text], &p.colors[background]);
                assert!(
                    ratio >= 4.5,
                    "light: color {text} on color {background} is only {ratio:.2}:1"
                );
            }
        }
    }

    /// The neutrals run in order of brightness in both styles: black is dark,
    /// bright black is lighter, white lighter still and bright white the lightest.
    /// (An earlier light palette darkened white below black.)
    #[test]
    fn the_neutrals_get_lighter_from_black_to_bright_white_in_both_styles() {
        for (style, p) in [("light", palette(false)), ("dark", palette(true))] {
            let l = |index: usize| relative_luminance(&p.colors[index]);
            assert!(l(0) < 0.05, "{style}: black is dark");
            assert!(l(0) < l(8), "{style}: black is darker than bright black");
            assert!(l(8) < l(7), "{style}: bright black is darker than white");
            assert!(l(7) < l(15), "{style}: white is darker than bright white");
        }
    }

    #[test]
    fn the_ansi_colors_can_be_read_on_their_own_background() {
        // Black and bright black are for backgrounds and dim text and may sit
        // close to it; every color a program prints text in must not vanish.
        // (White and bright white are light in the light style, as in GNOME
        // Console, so text in them directly on its white background is faint
        // by design; it stays above this loose bar.)
        for p in [palette(true), palette(false)] {
            for index in (1..8).chain(9..16) {
                let gap = (luminance(&p.colors[index]) - luminance(&p.background)).abs();
                assert!(gap > 0.1, "color {index} is too close to the background");
            }
        }
    }
}
