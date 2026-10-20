//! Short messages shown at the top centre of a window ([`NotificationArea`]).
//!
//! The area wraps a window's content in a [`gtk::Overlay`]. A message appears in
//! a dark pill at the top centre, just below the header bar, over the content
//! (over the terminal, in a main window). It is plain text that wraps onto
//! several lines within a comfortable width, stays for a few seconds
//! ([`display_time`]: longer for a long message, so there is time to read it),
//! then fades. Messages that arrive while one is up wait their turn and are shown
//! one after another.
//!
//! `AdwToastOverlay` can only show at the bottom, so this is a small widget of
//! our own in its place. The text is only ever set on a label with markup off:
//! messages can quote a profile name, a host, a file name or an error from
//! another machine. Nothing here runs per frame; the pill is an overlay child
//! that a [`gtk::Revealer`] shows and hides, and the only other work is one
//! GLib timeout at a time.
//!
//! [`NotificationArea`] is a widget of its own (a `gtk::Box` subclass holding the
//! overlay) so that it is found from the widgets around it, as the area around a
//! given widget is ([`crate::context::Notice::show_near`]), and lives and dies
//! with its window. The timers hold it weakly and it cancels the pending one
//! when it is disposed, so a window that closes with a message up leaves
//! nothing behind and nothing is called on it afterwards.
//!
//! A dialog (`AdwDialog`) shown over the window covers the area and dims it, so
//! while the window has one (`visible-dialog`) the area starts no message:
//! those that arrive wait, and one that was showing when the dialog opened is
//! taken off and shown again, for its whole time, once the dialog is gone. The
//! area follows the window it is in with one handler on the window, held so
//! that it holds the area only weakly and is disconnected when the area leaves
//! the window.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::Duration;

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, pango};
use gtk4 as gtk;
use libadwaita as adw;

/// How long a message of up to [`FREE_CHARS`] characters stays up.
const BASE_TIME: Duration = Duration::from_secs(8);

/// The longest a message stays up, however long it is.
const MAX_TIME: Duration = Duration::from_secs(15);

/// A message this long (in characters) or shorter stays up [`BASE_TIME`].
const FREE_CHARS: usize = 80;

/// Every started block of this many characters beyond [`FREE_CHARS`] adds a
/// second.
const STEP_CHARS: usize = 40;

/// How long the fade in and the fade out take. The next message waits for the
/// fade out to be over, so two never overlap.
const FADE: Duration = Duration::from_millis(250);

/// The widest a notification grows before its text wraps, in characters.
const MAX_WIDTH_CHARS: i32 = 60;

/// Space between a notification and the edge of the window or the area above.
const MARGIN: i32 = 12;

/// The height to clear when the area covers a whole window, header bar included
/// ([`NotificationArea::below_header_bar`]).
const HEADER_BAR_HEIGHT: i32 = 48;

/// The CSS class of the pill, besides libadwaita's `osd` (the dark, translucent
/// look of things drawn over content, which follows the system and high
/// contrast styles): the shape and the room around the text.
const STYLE_CLASS: &str = "moniker-notification";
const STYLE: &str = "
.moniker-notification {
    border-radius: 12px;
    padding: 10px 16px;
    box-shadow: 0 1px 3px 1px rgba(0, 0, 6, 0.07), 0 2px 6px 2px rgba(0, 0, 6, 0.03);
}
";

/// How long `text` stays up: 8 seconds for up to 80 characters, one more for
/// every further 40 (or part of 40), at most 15 seconds.
pub fn display_time(text: &str) -> Duration {
    let further = text.chars().count().saturating_sub(FREE_CHARS);
    let extra = Duration::from_secs(further.div_ceil(STEP_CHARS) as u64);
    (BASE_TIME + extra).min(MAX_TIME)
}

/// Load the pill's look for the display, once.
fn ensure_style() {
    thread_local! {
        static LOADED: Cell<bool> = const { Cell::new(false) };
    }
    if LOADED.replace(true) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(STYLE);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// The dialog shown over `window`, if it is a libadwaita window and has one.
pub(crate) fn visible_dialog(window: &gtk::Window) -> Option<adw::Dialog> {
    if let Some(window) = window.downcast_ref::<adw::ApplicationWindow>() {
        window.visible_dialog()
    } else if let Some(window) = window.downcast_ref::<adw::Window>() {
        window.visible_dialog()
    } else {
        None
    }
}

/// Whether `dialog` is shown over a window of this application right now.
pub(crate) fn is_shown(dialog: &adw::Dialog) -> bool {
    gtk::Window::list_toplevels()
        .iter()
        .filter_map(|window| window.downcast_ref::<gtk::Window>())
        .any(|window| visible_dialog(window).as_ref() == Some(dialog))
}

/// Where a notification is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing is up and nothing is waiting.
    Idle,
    /// A message is up, until its time runs out.
    Showing,
    /// It is fading out; the next message (if any) comes when that is over.
    Fading,
}

mod imp {
    use super::*;

    pub struct NotificationArea {
        /// Holds the window's content and, over it, the revealer.
        pub(super) overlay: gtk::Overlay,
        /// Shows and hides the label with a fade.
        pub(super) revealer: gtk::Revealer,
        /// The message: plain text, wrapped, never markup.
        pub(super) label: gtk::Label,
        /// The messages waiting for their turn, oldest first.
        pub(super) queue: RefCell<VecDeque<String>>,
        pub(super) phase: Cell<Phase>,
        /// The message up now (shown or fading), kept as it was told.
        pub(super) current: RefCell<Option<String>>,
        /// The window has a dialog open, so nothing is started.
        pub(super) held: Cell<bool>,
        /// The window whose `visible-dialog` is followed, and the handler on it.
        pub(super) watch: RefCell<Option<(glib::WeakRef<gtk::Window>, glib::SignalHandlerId)>>,
        /// The timeout that ends the current phase. `Some` exactly while that
        /// source is pending: its callback takes it out before doing anything
        /// else, so a source that has run is never removed (which GLib treats
        /// as an error).
        pub(super) timer: RefCell<Option<glib::SourceId>>,
        /// In tests, how long the current message was given.
        #[cfg(test)]
        pub(super) shown_for: Cell<Option<Duration>>,
        /// In tests, when the current message came up.
        #[cfg(test)]
        pub(super) shown_since: Cell<Option<std::time::Instant>>,
        /// In tests, each message that has begun to fade and how long it was up
        /// before that.
        #[cfg(test)]
        pub(super) up_log: RefCell<Vec<(String, Duration)>>,
        /// In tests, what the clock is multiplied by, so that a message that
        /// stays 8 seconds can be seen to go in a moment.
        #[cfg(test)]
        pub(super) clock_scale: Cell<f64>,
    }

    impl Default for NotificationArea {
        fn default() -> Self {
            NotificationArea {
                overlay: gtk::Overlay::builder().hexpand(true).vexpand(true).build(),
                revealer: gtk::Revealer::builder()
                    .halign(gtk::Align::Center)
                    .valign(gtk::Align::Start)
                    .margin_top(MARGIN)
                    .margin_bottom(MARGIN)
                    .margin_start(MARGIN)
                    .margin_end(MARGIN)
                    .transition_type(gtk::RevealerTransitionType::Crossfade)
                    .transition_duration(FADE.as_millis() as u32)
                    // A notification never takes a click or a key from what is
                    // under it.
                    .can_target(false)
                    .can_focus(false)
                    .build(),
                label: gtk::Label::builder()
                    .css_classes(["osd", STYLE_CLASS])
                    .wrap(true)
                    .wrap_mode(pango::WrapMode::WordChar)
                    .ellipsize(pango::EllipsizeMode::None)
                    .max_width_chars(MAX_WIDTH_CHARS)
                    .use_markup(false)
                    .selectable(false)
                    .build(),
                queue: RefCell::new(VecDeque::new()),
                phase: Cell::new(Phase::Idle),
                current: RefCell::new(None),
                held: Cell::new(false),
                watch: RefCell::new(None),
                timer: RefCell::new(None),
                #[cfg(test)]
                shown_for: Cell::new(None),
                #[cfg(test)]
                shown_since: Cell::new(None),
                #[cfg(test)]
                up_log: RefCell::new(Vec::new()),
                #[cfg(test)]
                clock_scale: Cell::new(1.0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NotificationArea {
        const NAME: &'static str = "MonikerNotificationArea";
        type Type = super::NotificationArea;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for NotificationArea {
        fn constructed(&self) {
            self.parent_constructed();
            let area = self.obj();
            area.set_orientation(gtk::Orientation::Vertical);
            area.set_hexpand(true);
            area.set_vexpand(true);
            self.revealer.set_child(Some(&self.label));
            // The pill floats over the content and adds nothing to its size.
            self.overlay.add_overlay(&self.revealer);
            self.overlay.set_measure_overlay(&self.revealer, false);
            area.append(&self.overlay);
        }

        fn dispose(&self) {
            self.unwatch_dialogs();
            // A message up when the window goes: its timer goes with it. The
            // callbacks hold the area weakly, so one that did run late would
            // find nothing, but there is no reason to leave it pending.
            if let Some(timer) = self.timer.take() {
                timer.remove();
            }
            self.queue.borrow_mut().clear();
        }
    }

    impl WidgetImpl for NotificationArea {
        fn root(&self) {
            self.parent_root();
            self.watch_dialogs();
        }

        fn unroot(&self) {
            self.unwatch_dialogs();
            self.parent_unroot();
        }
    }
    impl BoxImpl for NotificationArea {}

    impl NotificationArea {
        /// Queue `text`, and show it now if nothing is up.
        pub(super) fn show(&self, text: &str) {
            self.queue.borrow_mut().push_back(text.to_owned());
            if self.phase.get() == Phase::Idle {
                self.next();
            }
        }

        /// Follow the dialogs of the window the area is in, from now on.
        fn watch_dialogs(&self) {
            self.unwatch_dialogs();
            let area = self.obj();
            if let Some(window) = area.root().and_downcast::<gtk::Window>()
                && (window.is::<adw::ApplicationWindow>() || window.is::<adw::Window>())
            {
                let weak = area.downgrade();
                let handler = window.connect_notify_local(Some("visible-dialog"), move |_, _| {
                    if let Some(area) = weak.upgrade() {
                        area.imp().sync_held();
                    }
                });
                self.watch.replace(Some((window.downgrade(), handler)));
            }
            self.sync_held();
        }

        /// Stop following the window's dialogs. Nothing is held for them any
        /// more, but nothing is started either: the area is leaving the window.
        fn unwatch_dialogs(&self) {
            if let Some((window, handler)) = self.watch.take()
                && let Some(window) = window.upgrade()
            {
                window.disconnect(handler);
            }
            self.held.set(false);
        }

        /// The window opened or closed a dialog (or the area was put in a
        /// window that has one): hold or go on.
        fn sync_held(&self) {
            let open = self
                .obj()
                .root()
                .and_downcast::<gtk::Window>()
                .is_some_and(|window| visible_dialog(&window).is_some());
            if open == self.held.replace(open) {
                return;
            }
            if open {
                self.hold();
            } else if self.phase.get() == Phase::Idle {
                self.next();
            }
        }

        /// A dialog opened over the window. A message that is showing is taken
        /// off, to be shown again in full when the dialog is gone; one that is
        /// already fading finishes fading (the next one is then held back by
        /// [`next`](Self::next)).
        fn hold(&self) {
            if self.phase.get() != Phase::Showing {
                return;
            }
            if let Some(timer) = self.timer.take() {
                timer.remove();
            }
            if let Some(text) = self.current.take() {
                self.queue.borrow_mut().push_front(text);
            }
            self.revealer.set_reveal_child(false);
            self.phase.set(Phase::Idle);
            #[cfg(test)]
            {
                self.shown_for.set(None);
                self.shown_since.set(None);
            }
        }

        /// Show the oldest waiting message, or become idle if there is none.
        fn next(&self) {
            if self.held.get() {
                self.phase.set(Phase::Idle);
                return;
            }
            let next = self.queue.borrow_mut().pop_front();
            let Some(text) = next else {
                self.phase.set(Phase::Idle);
                return;
            };
            let time = display_time(&text);
            self.label.set_label(&text);
            self.revealer.set_reveal_child(true);
            // Screen readers: a toast announced itself, so this must too.
            self.label
                .announce(&text, gtk::AccessibleAnnouncementPriority::Medium);
            self.current.replace(Some(text));
            self.phase.set(Phase::Showing);
            #[cfg(test)]
            {
                self.shown_for.set(Some(time));
                self.shown_since.set(Some(std::time::Instant::now()));
            }
            self.schedule(self.scaled(time), Self::fade);
        }

        /// The time is up: fade the message out.
        fn fade(&self) {
            #[cfg(test)]
            if let Some(since) = self.shown_since.take() {
                self.up_log
                    .borrow_mut()
                    .push((self.label.label().to_string(), since.elapsed()));
            }
            self.phase.set(Phase::Fading);
            self.revealer.set_reveal_child(false);
            self.schedule(FADE, Self::finish);
        }

        /// The fade out is over: clear the label and go on to the next message.
        fn finish(&self) {
            self.label.set_label("");
            self.current.take();
            #[cfg(test)]
            self.shown_for.set(None);
            self.next();
        }

        /// Call `then` on this area after `after`, unless it is gone by then.
        fn schedule(&self, after: Duration, then: fn(&Self)) {
            let area = self.obj().downgrade();
            let timer = glib::timeout_add_local_once(after, move || {
                let Some(area) = area.upgrade() else {
                    return;
                };
                let this = area.imp();
                // This source is spent: forget it before anything else.
                this.timer.take();
                then(this);
            });
            if let Some(stale) = self.timer.replace(Some(timer)) {
                // Not reachable (a phase ends before the next begins), but a
                // second pending timer would show messages twice as fast.
                stale.remove();
            }
        }

        /// `time` as the clock runs: as it is, except in tests.
        #[cfg(not(test))]
        fn scaled(&self, time: Duration) -> Duration {
            time
        }

        #[cfg(test)]
        fn scaled(&self, time: Duration) -> Duration {
            time.mul_f64(self.clock_scale.get())
        }
    }
}

glib::wrapper! {
    /// A window's content with notifications over it: see the [module
    /// documentation](self).
    pub struct NotificationArea(ObjectSubclass<imp::NotificationArea>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl NotificationArea {
    /// An area around `child`, to be put in a window in its place.
    pub fn new(child: &impl IsA<gtk::Widget>) -> NotificationArea {
        ensure_style();
        let area: NotificationArea = glib::Object::new();
        area.imp().overlay.set_child(Some(child));
        area
    }

    /// Show `text` at the top centre: now, if nothing is up, else after the
    /// messages before it. It is plain text, never markup. Each message stays
    /// for [`display_time`] and then fades.
    pub fn show(&self, text: &str) {
        self.imp().show(text);
    }

    /// The overlay that holds the content and, over it, the notification.
    pub fn widget(&self) -> &gtk::Overlay {
        &self.imp().overlay
    }

    /// For an area that covers a whole window, header bars included (the
    /// Connection Profiles and Transcripts windows, whose header bars are in
    /// their panes): keep the notifications below the header bar. In a main
    /// window the area already starts below it.
    pub fn below_header_bar(&self) {
        self.imp()
            .revealer
            .set_margin_top(MARGIN + HEADER_BAR_HEIGHT);
    }
}

#[cfg(test)]
impl NotificationArea {
    /// The notification, hidden unless a message is up.
    pub(crate) fn revealer(&self) -> &gtk::Revealer {
        &self.imp().revealer
    }

    /// The label that draws the message.
    pub(crate) fn label(&self) -> &gtk::Label {
        &self.imp().label
    }

    /// The message that is up (fading out or not is not told), if any.
    pub(crate) fn shown_text(&self) -> Option<String> {
        let imp = self.imp();
        (imp.phase.get() != Phase::Idle).then(|| imp.label.label().to_string())
    }

    /// How many messages wait behind the one that is up.
    pub(crate) fn queued(&self) -> usize {
        self.imp().queue.borrow().len()
    }

    /// How long the message that is up was given.
    pub(crate) fn shown_for(&self) -> Option<Duration> {
        self.imp().shown_for.get()
    }

    /// Each message that has begun to fade, and how long it was up before that.
    pub(crate) fn up_log(&self) -> Vec<(String, Duration)> {
        self.imp().up_log.borrow().clone()
    }

    /// Run the clock `scale` times as fast as it is (the fade stays as long as
    /// it is): a test that would wait 8 seconds a message sets 0.01.
    pub(crate) fn set_clock_scale(&self, scale: f64) {
        self.imp().clock_scale.set(scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(text: &str) -> u64 {
        display_time(text).as_secs()
    }

    #[test]
    fn display_time_grows_with_length_and_caps() {
        assert_eq!(display_time(""), Duration::from_secs(8));
        assert_eq!(display_time(&"x".repeat(80)), Duration::from_secs(8));
        // Every further 40 characters, or part of 40, adds a second.
        assert_eq!(secs(&"x".repeat(81)), 9);
        assert_eq!(secs(&"x".repeat(120)), 9);
        assert_eq!(secs(&"x".repeat(121)), 10);
        assert_eq!(display_time(&"x".repeat(160)), Duration::from_secs(10));
        // At most 15 seconds, however long.
        assert_eq!(secs(&"x".repeat(80 + 7 * 40)), 15);
        assert_eq!(secs(&"x".repeat(80 + 7 * 40 + 1)), 15);
        assert_eq!(display_time(&"x".repeat(2_000)), Duration::from_secs(15));
    }

    #[test]
    fn display_time_counts_characters_not_bytes() {
        // 80 characters of 4 bytes each are still 80 characters.
        assert_eq!(secs(&"\u{1F600}".repeat(80)), 8);
        assert_eq!(secs(&"\u{1F600}".repeat(160)), 10);
    }
}
