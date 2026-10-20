//! Text size: the scale a tab's terminal draws its font at, and the steps
//! Ctrl+plus and Ctrl+minus move it by.
//!
//! The scale is VTE's font scale (1.0 is the font as the system sets it), kept
//! per tab ([`crate::terminal_tab`]) and remembered across launches in the GUI
//! settings ([`crate::settings`]). This module is the arithmetic only, so it
//! needs no display: the range, the steps, and how a scale reads as a percent.

/// The smallest scale: half the normal size.
pub const MIN: f64 = 0.5;

/// The largest scale: three times the normal size.
pub const MAX: f64 = 3.0;

/// The normal size.
pub const DEFAULT: f64 = 1.0;

/// The scales Ctrl+plus and Ctrl+minus step through, ascending: tenths around
/// the normal size, where small differences matter, then wider steps, as GNOME
/// Console does. [`MIN`] and [`MAX`] are the first and the last.
const STEPS: [f64; 14] = [
    0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.5, 1.7, 2.0, 2.4, 3.0,
];

/// Two scales closer than this are the same step: a scale that went through
/// the settings file or a float calculation must not step to itself.
const EPSILON: f64 = 1e-6;

/// `scale` limited to `MIN..=MAX`; a value that is not a number (or is
/// infinite, which no sane file holds) is the normal size.
pub fn clamp(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(MIN, MAX)
    } else {
        DEFAULT
    }
}

/// The next step above `scale`; the largest stays where it is. A scale between
/// two steps goes to the one above it.
pub fn larger(scale: f64) -> f64 {
    let scale = clamp(scale);
    STEPS
        .iter()
        .copied()
        .find(|step| *step > scale + EPSILON)
        .unwrap_or(MAX)
}

/// The next step below `scale`; the smallest stays where it is. A scale
/// between two steps goes to the one below it.
pub fn smaller(scale: f64) -> f64 {
    let scale = clamp(scale);
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|step| *step < scale - EPSILON)
        .unwrap_or(MIN)
}

/// `scale` as the whole percent the menu shows: `1.0` is "100%".
pub fn percent(scale: f64) -> String {
    format!("{}%", (clamp(scale) * 100.0).round())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_go_up_and_down_and_stop_at_the_ends() {
        assert_eq!(larger(1.0), 1.1);
        assert_eq!(larger(1.1), 1.2);
        assert_eq!(larger(1.3), 1.5);
        assert_eq!(larger(2.4), 3.0);
        assert_eq!(smaller(1.0), 0.9);
        assert_eq!(smaller(1.5), 1.3);
        assert_eq!(smaller(0.6), 0.5);
        assert_eq!(larger(MAX), MAX, "the largest stays");
        assert_eq!(smaller(MIN), MIN, "the smallest stays");

        // All the way up and all the way down visits every step once.
        let mut up = vec![MIN];
        while *up.last().unwrap() < MAX {
            up.push(larger(*up.last().unwrap()));
        }
        assert_eq!(up, STEPS);
        let mut down = vec![MAX];
        while *down.last().unwrap() > MIN {
            down.push(smaller(*down.last().unwrap()));
        }
        down.reverse();
        assert_eq!(down, STEPS);
    }

    #[test]
    fn an_in_between_value_goes_to_the_neighboring_step() {
        assert_eq!(larger(1.05), 1.1);
        assert_eq!(smaller(1.05), 1.0);
        assert_eq!(larger(1.4), 1.5);
        assert_eq!(smaller(1.4), 1.3);
        // A step that went through floating point and back is still itself.
        assert_eq!(larger(1.1 + 1e-12), 1.2);
        assert_eq!(smaller(1.1 - 1e-12), 1.0);
        // Out of range counts as the end of the range.
        assert_eq!(larger(0.1), 0.6);
        assert_eq!(smaller(10.0), 2.4);
    }

    #[test]
    fn percent_rounds() {
        assert_eq!(percent(1.0), "100%");
        assert_eq!(percent(1.1), "110%");
        assert_eq!(percent(0.5), "50%");
        assert_eq!(percent(3.0), "300%");
        assert_eq!(percent(1.0 + 1e-12), "100%");
        assert_eq!(percent(1.234), "123%");
        assert_eq!(percent(f64::NAN), "100%");
    }

    #[test]
    fn clamp_limits_the_range_and_replaces_what_is_not_a_number() {
        assert_eq!(clamp(1.3), 1.3);
        assert_eq!(clamp(MIN), MIN);
        assert_eq!(clamp(MAX), MAX);
        assert_eq!(clamp(-1.0), MIN);
        assert_eq!(clamp(0.0), MIN);
        assert_eq!(clamp(100.0), MAX);
        assert_eq!(clamp(f64::NAN), DEFAULT);
        assert_eq!(clamp(f64::INFINITY), DEFAULT);
        assert_eq!(clamp(f64::NEG_INFINITY), DEFAULT);
    }
}
