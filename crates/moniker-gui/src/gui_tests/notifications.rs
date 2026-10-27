//! Scenarios: the notification area ([`crate::notify`]), where the app's short
//! messages appear: at the top center of a window, below the header bar,
//! wrapped, one after another, as plain text, fading after a few seconds.
//!
//! While a dialog is open over a window its area waits
//! ([`notifications_wait_while_a_dialog_is_open`]); the Preferences dialog shows
//! its own messages in the dialog (see `preferences.rs`).
//!
//! A message stays 8 seconds or more, so these scenarios run the area's clock
//! faster (`set_clock_scale`) wherever they wait for one to go; what the time
//! given to a message is, they read from `shown_for`.

use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::pango;
use gtk4 as gtk;
use libadwaita as adw;

use super::main_window::{ctx_with, test_app};
use super::tabs::window_with_area;
use super::{MARKUP_NAME, find_widget, pump_for, pump_until};
use crate::context::Notice;
use crate::notify::NotificationArea;
use crate::{profiles, recordings, window};

/// Pump until `area` shows nothing and has nothing waiting, noting each
/// message as it comes up (and that its label never parses markup). Messages
/// are told apart by their text, so each must differ from the one before.
fn watch_until_done(area: &NotificationArea) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    pump_until("every message to have been shown", || {
        let up = area.shown_text();
        if let Some(text) = &up {
            assert!(!area.label().uses_markup(), "{text:?} is parsed as markup");
            if seen.last() != Some(text) {
                seen.push(text.clone());
            }
        }
        up.is_none()
    });
    assert_eq!(area.queued(), 0, "nothing is left waiting");
    seen
}

/// A message appears at the top center of a main window, below the header bar
/// and over the tabs, and takes no click from what is under it.
pub(super) fn notifications_appear_at_the_top_center(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(400));

    let area = &main.notifications;
    let revealer = area.revealer();
    assert_eq!(revealer.valign(), gtk::Align::Start, "at the top");
    assert_eq!(revealer.halign(), gtk::Align::Center, "in the middle");
    assert_eq!(
        revealer.parent().as_ref(),
        Some(area.widget().upcast_ref::<gtk::Widget>()),
        "over the content, in the window's overlay"
    );
    assert!(!revealer.can_target(), "clicks go through to the terminal");
    assert!(!revealer.reveals_child(), "nothing is up before a message");
    assert_eq!(area.shown_text(), None);

    ctx.notice.show("Something happened.");
    assert_eq!(area.shown_text().as_deref(), Some("Something happened."));
    pump_until("the notification to be revealed", || {
        revealer.is_child_revealed()
    });
    pump_for(Duration::from_millis(100));

    let in_window = |widget: &gtk::Widget| {
        widget
            .compute_bounds(&main.window)
            .expect("the widget has bounds in the window")
    };
    let header = in_window(
        find_widget::<adw::HeaderBar>(&main.window)
            .expect("the header bar")
            .upcast_ref(),
    );
    let pill = in_window(revealer.upcast_ref());
    let tabs = in_window(main.view.upcast_ref());
    assert!(
        pill.y() >= header.y() + header.height(),
        "below the header bar: pill at {}, header ends at {}",
        pill.y(),
        header.y() + header.height()
    );
    assert!(
        pill.y() >= tabs.y() && pill.y() < tabs.y() + 40.0,
        "at the top of the tabs: pill at {}, tabs start at {}",
        pill.y(),
        tabs.y()
    );
    let window_middle = f64::from(main.window.width()) / 2.0;
    let pill_middle = f64::from(pill.x() + pill.width() / 2.0);
    assert!(
        (pill_middle - window_middle).abs() <= 2.0,
        "centered: the pill's middle is at {pill_middle}, the window's at {window_middle}"
    );

    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The windows that cover their header bars with the area (Connection Profiles,
/// Transcripts) keep the messages below them too.
pub(super) fn the_side_windows_keep_notifications_below_their_header_bars(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let nothing: profiles::ConnectFn = Rc::new(|_, _| {});
    let profiles_window = profiles::present(&app, &ctx, nothing);
    let transcripts_window = recordings::present(&app, dir.join("transcripts"), &ctx.settings_path);
    pump_for(Duration::from_millis(400));

    for (what, window) in [
        (
            "Connection Profiles",
            profiles_window.upcast_ref::<gtk::Window>(),
        ),
        (
            "Transcripts",
            transcripts_window.upcast_ref::<gtk::Window>(),
        ),
    ] {
        let area: NotificationArea =
            find_widget(window).unwrap_or_else(|| panic!("{what}: no area"));
        area.show("Something happened.");
        pump_until("the notification to be revealed", || {
            area.revealer().is_child_revealed()
        });
        pump_for(Duration::from_millis(100));
        let pill = area
            .revealer()
            .compute_bounds(window)
            .expect("the pill has bounds");
        let lowest_header = super::all_widgets::<adw::HeaderBar>(window)
            .iter()
            .filter(|bar| bar.is_mapped())
            .map(|bar| {
                let bounds = bar.compute_bounds(window).expect("the header has bounds");
                bounds.y() + bounds.height()
            })
            .fold(0.0_f32, f32::max);
        assert!(lowest_header > 0.0, "{what}: found no header bar");
        assert!(
            pill.y() >= lowest_header,
            "{what}: the pill at {} is over a header bar that ends at {lowest_header}",
            pill.y()
        );
        assert_eq!(area.shown_text().as_deref(), Some("Something happened."));
    }
    profiles_window.close();
    transcripts_window.close();
    pump_for(Duration::from_millis(200));
}

/// A window of the given size holding an area (around a label) that is attached
/// to `notice`.
fn sized_window_with_area(
    notice: &Notice,
    width: i32,
    height: i32,
) -> (gtk::Window, NotificationArea) {
    let area = NotificationArea::new(&gtk::Label::new(Some("page")));
    let window = gtk::Window::builder()
        .default_width(width)
        .default_height(height)
        .child(&area)
        .build();
    window.present();
    notice.attach(&area);
    pump_for(Duration::from_millis(200));
    (window, area)
}

/// Show `text` and wait until the label has been laid out.
fn show_and_lay_out(notice: &Notice, area: &NotificationArea, text: &str) {
    notice.show(text);
    pump_until("the label to be laid out", || area.label().width() > 0);
    pump_for(Duration::from_millis(50));
}

/// A long message wraps within a comfortable width (it is never cut off with
/// "..." or run past the window's edge), and stays up longer, to the limit.
///
/// The layout is checked while the message is up, with the clock at its own
/// speed (15 seconds up), each time in a window of its own that is closed
/// afterwards: a slow machine can take longer to lay a long message out than a
/// sped-up clock would leave it up. Only the durations at the end run faster.
pub(super) fn long_notifications_wrap_and_stay_longer() {
    // Words, and then one word of 2,000 characters: both wrap, the second
    // anywhere (WordChar), so neither runs past the window.
    let words = "word ".repeat(400);
    let unbroken = "x".repeat(2_000);
    for text in [words.as_str(), unbroken.as_str()] {
        let notice = Notice::default();
        let (window, area) = sized_window_with_area(&notice, 900, 500);
        let label = area.label();
        assert!(label.wraps(), "wraps");
        assert_eq!(label.wrap_mode(), pango::WrapMode::WordChar);
        assert_eq!(
            label.ellipsize(),
            pango::EllipsizeMode::None,
            "never cut off"
        );
        assert_eq!(label.max_width_chars(), 60, "a comfortable width");
        assert!(!label.uses_markup());

        assert_eq!(text.chars().count(), 2_000);
        show_and_lay_out(&notice, &area, text);
        assert_eq!(label.label(), text, "all of it, as it is");
        assert_eq!(
            area.shown_for(),
            Some(Duration::from_secs(15)),
            "the longest a message stays"
        );
        let layout = label.layout();
        assert!(layout.is_wrapped(), "the text is wrapped");
        assert!(!layout.is_ellipsized(), "the text is not cut off");
        assert!(layout.line_count() > 10, "{} lines", layout.line_count());
        let width = label.width();
        assert!(
            (150..700).contains(&width),
            "about 60 characters wide, not the window's 900 or a line of 2,000: {width}"
        );
        window.destroy();
        pump_for(Duration::from_millis(100));
    }

    // In a window narrower than 60 characters, it wraps to the window.
    let notice = Notice::default();
    let (window, area) = sized_window_with_area(&notice, 300, 400);
    show_and_lay_out(&notice, &area, &words);
    assert!(
        area.label().width() <= area.width(),
        "the label ({}) fits the window ({})",
        area.label().width(),
        area.width()
    );
    assert!(area.label().layout().is_wrapped());
    window.destroy();
    pump_for(Duration::from_millis(100));

    // How long each length is given: 8 seconds up to 80 characters, then a
    // second more for every 40, at most 15; and it really is that long. The
    // time a message is given is read from `shown_for`; here the clock runs at
    // a twentieth of the speed so that the scenario does not wait 41 seconds.
    let notice = Notice::default();
    let (window, area) = sized_window_with_area(&notice, 900, 500);
    area.set_clock_scale(0.05);
    let mut expected = Vec::new();
    for (chars, secs) in [(10, 8), (80, 8), (160, 10), (2_000, 15)] {
        // Different texts, so the watcher tells them apart.
        let text = format!("{chars:04}{}", "y".repeat(chars - 4));
        notice.show(&text);
        expected.push((text, secs));
    }
    let seen = watch_until_done(&area);
    assert_eq!(
        seen,
        expected
            .iter()
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>()
    );
    let up_log = area.up_log();
    assert_eq!(up_log.len(), 4);
    for ((text, secs), (logged, up_for)) in expected.iter().zip(&up_log) {
        assert_eq!(logged, text);
        // Each was up for what it was given, at a twentieth of the speed. (A
        // timer never fires early.)
        let given = Duration::from_secs(*secs).mul_f64(0.05);
        assert!(
            *up_for >= given,
            "{} characters: up for {up_for:?}, given {given:?}",
            text.len()
        );
    }
    assert_eq!(
        area.shown_for(),
        None,
        "nothing is up, so no time is being given"
    );
    window.destroy();
    pump_for(Duration::from_millis(200));
}

/// Messages that arrive while one is up wait, and are shown one after another,
/// in order, each as it is: a message full of markup characters is shown as
/// written.
pub(super) fn notifications_queue_and_show_literally() {
    let notice = Notice::default();
    let (window, area) = window_with_area(&notice, &gtk::Label::new(Some("page")));
    area.set_clock_scale(0.01);
    pump_for(Duration::from_millis(100));

    let delivered = notice.delivered();
    for text in ["first", MARKUP_NAME, "third <i>x</i> &lt;"] {
        notice.show(text);
    }
    assert_eq!(notice.delivered(), delivered + 3);
    assert_eq!(area.shown_text().as_deref(), Some("first"), "one at a time");
    assert_eq!(area.queued(), 2, "the others wait");

    // While the second is up it is on the label as it is written, markup off.
    pump_until("the second message", || {
        area.shown_text().as_deref() == Some(MARKUP_NAME)
    });
    assert_eq!(area.label().label(), MARKUP_NAME);
    assert!(!area.label().uses_markup());
    assert_eq!(area.queued(), 1);
    let seen = watch_until_done(&area);
    assert_eq!(seen, [MARKUP_NAME, "third <i>x</i> &lt;"]);
    assert!(!area.revealer().reveals_child(), "gone when they are done");

    // Many in a row: all of them, in the order they came.
    let many: Vec<String> = (1..=6).map(|n| format!("message {n}")).collect();
    for text in &many {
        notice.show(text);
    }
    assert_eq!(area.queued(), many.len() - 1);
    assert_eq!(watch_until_done(&area), many);

    // And one that arrives after they are all gone is shown at once.
    notice.show("later");
    assert_eq!(area.shown_text().as_deref(), Some("later"));
    assert_eq!(watch_until_done(&area), ["later"]);
    window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The window closes with a message up and others waiting, in each phase a
/// message goes through: nothing panics, nothing is called on the closed
/// window, and a message told afterwards goes to stderr.
pub(super) fn closing_the_window_with_a_notification_up() {
    // While a message is up and others wait: the area is freed with its
    // window, and its pending timer with it.
    let notice = Notice::default();
    let (window, area) = window_with_area(&notice, &gtk::Label::new(Some("page")));
    for text in ["up", "waiting", "waiting too"] {
        notice.show(text);
    }
    pump_for(Duration::from_millis(100));
    assert_eq!(area.shown_text().as_deref(), Some("up"));
    assert_eq!(area.queued(), 2);
    let weak = area.downgrade();
    window.destroy();
    drop(window);
    drop(area);
    pump_until("the closed window's area to be freed", || {
        weak.upgrade().is_none()
    });
    let delivered = notice.delivered();
    notice.show("nobody is there");
    assert_eq!(notice.delivered(), delivered, "no area left to show it");
    assert_eq!(notice.printed(), ["nobody is there"], "written to stderr");
    pump_for(Duration::from_millis(200));

    // The window goes but something still holds the area (as a tab's page
    // might for a moment): the timers keep running on it and finish cleanly.
    let notice = Notice::default();
    let (window, area) = window_with_area(&notice, &gtk::Label::new(Some("page")));
    area.set_clock_scale(0.01);
    for text in ["one", "two"] {
        notice.show(text);
    }
    pump_for(Duration::from_millis(50));
    window.destroy();
    drop(window);
    assert_eq!(
        watch_until_done(&area),
        ["one", "two"],
        "carried on, unseen"
    );
    drop(area);

    // While the message is fading out, and when nothing is up at all.
    for fading in [true, false] {
        let notice = Notice::default();
        let (window, area) = window_with_area(&notice, &gtk::Label::new(Some("page")));
        area.set_clock_scale(0.01);
        notice.show("short");
        if fading {
            pump_until("the message to start fading", || {
                area.shown_text().is_some() && !area.revealer().reveals_child()
                    || area.shown_text().is_none()
            });
        } else {
            pump_until("the message to be gone", || area.shown_text().is_none());
        }
        let weak = area.downgrade();
        window.destroy();
        drop(window);
        drop(area);
        pump_until("the closed window's area to be freed", || {
            weak.upgrade().is_none()
        });
        pump_for(Duration::from_millis(400));
    }
}

/// A dialog over the window: an `AdwAlertDialog` presented over `window`, with
/// the main loop run until the window says it is the visible one.
fn open_dialog_over(window: &adw::ApplicationWindow) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some("A dialog"), None);
    dialog.add_response("ok", "OK");
    dialog.present(Some(window));
    pump_until("the dialog to open", || window.visible_dialog().is_some());
    pump_for(Duration::from_millis(100));
    dialog
}

fn close_dialog_over(window: &adw::ApplicationWindow, dialog: &adw::AlertDialog) {
    dialog.force_close();
    pump_until("the dialog to close", || window.visible_dialog().is_none());
}

/// A dialog covers (and dims) the window's notification area, so while the
/// window has one the area starts no message: they wait, and one that was up
/// when the dialog opened is shown again, in full, when it is gone.
pub(super) fn notifications_wait_while_a_dialog_is_open(dir: &Path) {
    let ctx = ctx_with(dir, &[]);
    let app = test_app();
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(400));
    let area = main.notifications.clone();
    area.set_clock_scale(0.1);

    // Told while a dialog is open: counted as delivered, but not shown, however
    // long the dialog stays.
    let dialog = open_dialog_over(&main.window);
    let delivered = ctx.notice.delivered();
    ctx.notice.show("told over a dialog");
    assert_eq!(ctx.notice.delivered(), delivered + 1);
    assert_eq!(area.shown_text(), None, "not while the dialog is up");
    assert_eq!(area.queued(), 1, "it waits");
    pump_for(Duration::from_millis(1_200));
    assert_eq!(area.shown_text(), None, "still not");
    assert!(!area.revealer().reveals_child());
    close_dialog_over(&main.window, &dialog);
    pump_until("the message to be shown", || {
        area.shown_text().as_deref() == Some("told over a dialog")
    });
    assert!(area.revealer().reveals_child());
    assert_eq!(watch_until_done(&area), ["told over a dialog"]);

    // Up when a dialog opens: taken off at once, nothing shown while the dialog
    // is open, and shown again afterwards for its whole time. (A slower clock
    // than above, so that the message is still up when the dialog is.)
    area.set_clock_scale(0.3);
    ctx.notice.show("up when a dialog opens");
    pump_until("the notification to be revealed", || {
        area.revealer().is_child_revealed()
    });
    let logged = area.up_log().len();
    let dialog = open_dialog_over(&main.window);
    assert_eq!(area.shown_text(), None, "taken off");
    assert_eq!(area.queued(), 1, "kept, to be shown again");
    assert!(!area.revealer().reveals_child());
    ctx.notice.show("and one more");
    pump_for(Duration::from_millis(1_200));
    assert_eq!(area.shown_text(), None, "nothing while the dialog is up");
    assert_eq!(area.queued(), 2);
    assert_eq!(area.up_log().len(), logged, "it did not run out meanwhile");
    close_dialog_over(&main.window, &dialog);
    pump_until("the message to be shown again", || {
        area.shown_text().is_some()
    });
    assert_eq!(
        area.shown_text().as_deref(),
        Some("up when a dialog opens"),
        "first, as before"
    );
    assert_eq!(
        watch_until_done(&area),
        ["up when a dialog opens", "and one more"]
    );
    let again = &area.up_log()[logged];
    assert_eq!(again.0, "up when a dialog opens");
    assert!(
        again.1 >= Duration::from_secs(8).mul_f64(0.3),
        "shown for its whole time again, not what was left: {:?}",
        again.1
    );

    // A window that goes while it holds messages back leaves nothing behind.
    let dialog = open_dialog_over(&main.window);
    ctx.notice.show("never shown");
    assert_eq!(area.queued(), 1);
    let weak = area.downgrade();
    drop(dialog);
    main.window.destroy();
    drop(main);
    drop(area);
    pump_until("the closed window's area to be freed", || {
        weak.upgrade().is_none()
    });
    pump_for(Duration::from_millis(200));
}
